//! SSH 登录安全设置与公钥管理。
//!
//! 设置:一次改一项,写进 `/etc/ssh/sshd_config.d/00-otter.conf`(sshd 对同一项取**第一个**值,所以文件名要排在
//! `50-cloud-init.conf` 这类前面;主配置没有 Include 的老系统改为在主文件开头加带 `# otter` 标记的行)。
//! 流程:备份 → 写 → `sshd -t` → 重载 → 用 `sshd -T` 读回验证 → 在服务器上排一个 120 秒的定时回滚;
//! 本机能新建连接后由执行器取消回滚(`Guard`),连不上就让服务器自己还原。
//! 公钥:增删 `~/.ssh/authorized_keys` 里的行,按原文整行匹配。

use super::common::{first, sections};
use super::PATH_FIX;
use crate::sysops::quote;

pub const SSH_SCRIPT: &str = r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH
echo '@@SSHD'
{ sshd -T 2>/dev/null || sshd -T -C user=root,host=localhost,addr=127.0.0.1 2>/dev/null; } | grep -iE '^(port|permitrootlogin|passwordauthentication|pubkeyauthentication|maxauthtries|clientaliveinterval) '
echo '@@DROPBEAR'; if ! command -v sshd >/dev/null 2>&1 && command -v dropbear >/dev/null 2>&1; then echo yes; fi
echo '@@UNIT'; if systemctl cat ssh.service >/dev/null 2>&1; then echo ssh; else echo sshd; fi
echo '@@SOCKET'; if systemctl is-active ssh.socket >/dev/null 2>&1; then echo yes; else echo no; fi
echo '@@INCLUDE'; if grep -qiE '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\.d/' /etc/ssh/sshd_config 2>/dev/null; then echo yes; else echo no; fi
echo '@@OTTER'
cat /etc/ssh/sshd_config.d/00-otter.conf 2>/dev/null
grep -E '# otter$' /etc/ssh/sshd_config 2>/dev/null | sed 's/ *# otter$//'
echo '@@KEYS'
getent passwd | awk -F: '($3==0||($3>=1000&&$3<60000)) && $7!~/(nologin|false)$/ {print $1 " " $6}' | while read -r u h; do
  printf 'U\t%s\t%s\n' "$u" "$h"
  f="$h/.ssh/authorized_keys"
  [ -f "$f" ] || continue
  T=$(mktemp 2>/dev/null || echo /tmp/.otter-k.$$)
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in ''|'#'*) continue;; esac
    k=$(printf '%s\n' "$line" | awk '{for(i=1;i<=NF;i++) if ($i ~ /^(ssh-|ecdsa-|sk-)/){ s=$i; for(j=i+1;j<=NF;j++) s=s " " $j; print s; exit }}')
    fp=
    if [ -n "$k" ] && command -v ssh-keygen >/dev/null 2>&1; then printf '%s\n' "$k" > "$T"; fp=$(ssh-keygen -lf "$T" 2>/dev/null | awk '{print $2 "\t" $NF}'); fi
    [ -n "$fp" ] || fp=$(printf '?\t?')
    printf 'K\t%s\t%s\n' "$fp" "$line"
  done < "$f"
  rm -f "$T"
done
true
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthKey {
    pub user: String,
    /// SHA256:xxxx;读不出来是 `?`
    pub fp: String,
    /// (ED25519) (RSA) …
    pub alg: String,
    pub comment: String,
    /// authorized_keys 里的原文整行
    pub raw: String,
    /// 行首带 command=、from= 之类的选项
    pub options: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshUser {
    pub name: String,
    pub home: String,
    pub keys: Vec<AuthKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshInfo {
    pub ports: Vec<u16>,
    /// yes / prohibit-password / no / forced-commands-only
    pub root_login: String,
    pub password: bool,
    pub pubkey: bool,
    pub max_tries: Option<u32>,
    pub alive_interval: Option<u32>,
    pub unit: String,
    /// Ubuntu 22.10+ 用 ssh.socket 接管监听
    pub socket: bool,
    pub include: bool,
    /// Otter 写过的项
    pub otter: Vec<(String, String)>,
    pub users: Vec<SshUser>,
}

fn yes(v: &str) -> bool {
    v.eq_ignore_ascii_case("yes")
}

pub fn parse_ssh(out: &str) -> Result<SshInfo, String> {
    let m = sections(out);
    let sshd = m.get("SSHD").cloned().unwrap_or_default();
    if sshd.is_empty() && first(&m, "DROPBEAR") == "yes" {
        return Err("这台服务器用 dropbear(OpenWrt 等精简系统),暂不支持 SSH 设置和公钥管理".into());
    }
    if sshd.is_empty() {
        return Err("读不到 sshd 的有效配置(服务器上没有 sshd,或没有 root 权限)".into());
    }
    let (mut ports, mut root_login, mut password, mut pubkey, mut max_tries, mut alive) = (Vec::new(), String::new(), true, true, None, None);
    for l in &sshd {
        let Some((k, v)) = l.trim().split_once(' ') else { continue };
        let v = v.trim();
        match k.to_ascii_lowercase().as_str() {
            "port" => ports.extend(v.parse::<u16>()),
            "permitrootlogin" => root_login = v.to_ascii_lowercase().replace("without-password", "prohibit-password"),
            "passwordauthentication" => password = yes(v),
            "pubkeyauthentication" => pubkey = yes(v),
            "maxauthtries" => max_tries = v.parse().ok(),
            "clientaliveinterval" => alive = v.parse().ok(),
            _ => {}
        }
    }
    let otter = m
        .get("OTTER")
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            l.split_once(' ').map(|(k, v)| (k.to_string(), v.trim().to_string()))
        })
        .collect();
    let mut users: Vec<SshUser> = Vec::new();
    for l in m.get("KEYS").into_iter().flatten() {
        let p: Vec<&str> = l.splitn(5, '\t').collect();
        match p.as_slice() {
            ["U", name, home] => users.push(SshUser { name: name.to_string(), home: home.to_string(), keys: Vec::new() }),
            ["K", fp, alg, raw] | ["K", fp, alg, raw, _] if users.last().is_some() => {
                let raw = if p.len() == 5 { format!("{raw}\t{}", p[4]) } else { raw.to_string() };
                let u = users.last_mut().unwrap();
                let toks: Vec<&str> = raw.split_whitespace().collect();
                let at = toks.iter().position(|t| t.starts_with("ssh-") || t.starts_with("ecdsa-") || t.starts_with("sk-")).unwrap_or(0);
                let comment = toks.get(at + 2..).map(|c| c.join(" ")).unwrap_or_default();
                let name = u.name.clone();
                u.keys.push(AuthKey { user: name, fp: fp.to_string(), alg: alg.to_string(), comment, options: at > 0, raw });
            }
            _ => {}
        }
    }
    Ok(SshInfo { ports, root_login, password, pubkey, max_tries, alive_interval: alive, unit: first(&m, "UNIT"), socket: first(&m, "SOCKET") == "yes", include: first(&m, "INCLUDE") == "yes", otter, users })
}

impl SshInfo {
    pub fn user(&self, name: &str) -> Option<&SshUser> {
        self.users.iter().find(|u| u.name == name)
    }

    pub fn otter_value(&self, key: &str) -> Option<&str> {
        self.otter.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }
}

// ---- 设置 ----

pub const GUARD_SECS: u32 = 120;
const GUARD_UNIT: &str = "otter-ssh-rollback";
const RB: &str = "/run/otter-ssh-rollback.sh";

/// 取消定时回滚(本机能新建连接之后执行)
pub fn guard_cancel() -> String {
    format!("systemctl stop {GUARD_UNIT}.timer {GUARD_UNIT}.service >/dev/null 2>&1; systemctl reset-failed {GUARD_UNIT}.service >/dev/null 2>&1; true")
}

const ROLLBACK_BODY: &str = r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH
cp -p /run/otter-ssh.main.bak /etc/ssh/sshd_config
if [ -e /run/otter-ssh.drop.none ]; then rm -f /etc/ssh/sshd_config.d/00-otter.conf; else cp -p /run/otter-ssh.drop.bak /etc/ssh/sshd_config.d/00-otter.conf; fi
SD=/etc/systemd/system/ssh.socket.d
if [ -e /run/otter-ssh.sock.none ]; then
  if [ -e $SD/otter.conf ]; then rm -f $SD/otter.conf; systemctl daemon-reload; systemctl restart ssh.socket >/dev/null 2>&1; fi
elif [ -e /run/otter-ssh.sock.bak ]; then cp -p /run/otter-ssh.sock.bak $SD/otter.conf; systemctl daemon-reload; systemctl restart ssh.socket >/dev/null 2>&1; fi
SVC=sshd; systemctl cat ssh.service >/dev/null 2>&1 && SVC=ssh
systemctl is-active ssh.socket >/dev/null 2>&1 || { systemctl reload $SVC >/dev/null 2>&1 || systemctl restart $SVC >/dev/null 2>&1; }
true
"#;

/// 这几项能设什么值
pub fn setting_ok(key: &str, val: &str) -> Result<(), String> {
    let bad = || Err(format!("{key} 不能设成「{val}」"));
    match key {
        "Port" => match val.parse::<u32>() {
            Ok(p) if (1..=65535).contains(&p) && val == p.to_string() => Ok(()),
            _ => Err("端口要填 1 到 65535 的数字".into()),
        },
        "PermitRootLogin" if matches!(val, "yes" | "prohibit-password" | "no") => Ok(()),
        "PasswordAuthentication" | "PubkeyAuthentication" if matches!(val, "yes" | "no") => Ok(()),
        "MaxAuthTries" if val.parse::<u32>().is_ok_and(|n| (1..=20).contains(&n)) => Ok(()),
        "ClientAliveInterval" if val.parse::<u32>().is_ok_and(|n| n <= 3600) => Ok(()),
        _ => bad(),
    }
}

/// 改之前的防锁死检查:`login` 是当前连接用的用户名
pub fn check_change(info: &SshInfo, login: &str, key: &str, val: &str) -> Result<(), String> {
    setting_ok(key, val)?;
    let keys_of = |u: &str| info.user(u).map(|u| u.keys.len()).unwrap_or(0);
    match (key, val) {
        ("PasswordAuthentication", "no") => {
            if keys_of(login) == 0 {
                return Err(format!("{login} 的 authorized_keys 里没有任何公钥,关闭密码登录后就再也登录不了了。请先在「SSH 公钥」里添加公钥"));
            }
            if !info.pubkey {
                return Err("公钥登录现在是关着的,不能同时关闭密码登录".into());
            }
        }
        ("PubkeyAuthentication", "no") if !info.password => return Err("密码登录现在是关着的,不能同时关闭公钥登录,否则谁都登录不了".into()),
        ("PermitRootLogin", "no") if login == "root" => return Err("你现在就是用 root 连接的,禁止 root 登录后断开就连不回来了。请先用其它用户登录(「用户与 sudo」里可以新建)".into()),
        ("PermitRootLogin", "prohibit-password") if login == "root" && keys_of("root") == 0 => return Err("root 还没有公钥,改成「仅密钥」后就登录不了了。请先给 root 添加公钥".into()),
        _ => {}
    }
    Ok(())
}

/// 改一项 sshd 设置。`Port` 另外处理 SELinux、ssh.socket 和端口占用
pub fn set_script(key: &str, val: &str) -> Result<String, String> {
    setting_ok(key, val)?;
    let lower = key.to_ascii_lowercase();
    let mut s = format!("{PATH_FIX}set -e\n");
    s.push_str(&format!(
        "M=/etc/ssh/sshd_config; D=/etc/ssh/sshd_config.d; F=$D/00-otter.conf\nK={}; V={}\nSD=/etc/systemd/system/ssh.socket.d; SF=$SD/otter.conf\n\
         command -v sshd >/dev/null 2>&1 || {{ echo '服务器上没有 sshd' >&2; exit 1; }}\n\
         SVC=sshd; systemctl cat ssh.service >/dev/null 2>&1 && SVC=ssh\nSOCK=0; systemctl is-active ssh.socket >/dev/null 2>&1 && SOCK=1\n\
         INC=0; grep -qiE '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\\.d/' $M && INC=1\n\
         systemctl stop {GUARD_UNIT}.timer {GUARD_UNIT}.service >/dev/null 2>&1 || true\n",
        quote(key),
        quote(val)
    ));
    if key == "Port" {
        s.push_str(
            "CUR=$(sshd -T 2>/dev/null | awk '$1==\"port\"{print $2}')\n\
             if ! echo \"$CUR\" | grep -qx \"$V\" && ss -ltn 2>/dev/null | awk '{print $4}' | grep -qE \"[:.]$V\\$\"; then echo \"端口 $V 已经被别的程序占用\" >&2; exit 1; fi\n\
             if command -v getenforce >/dev/null 2>&1 && [ \"$(getenforce 2>/dev/null)\" = Enforcing ]; then\n  if command -v semanage >/dev/null 2>&1; then semanage port -a -t ssh_port_t -p tcp \"$V\" 2>/dev/null || semanage port -m -t ssh_port_t -p tcp \"$V\" 2>/dev/null || true\n  else echo 'SELinux 开着但没有 semanage(policycoreutils-python-utils),sshd 不能监听新端口' >&2; exit 1; fi\nfi\n",
        );
    }
    // 备份和回滚脚本
    s.push_str(&format!(
        "rm -f /run/otter-ssh.main.bak /run/otter-ssh.drop.bak /run/otter-ssh.drop.none /run/otter-ssh.sock.bak /run/otter-ssh.sock.none\ncp -p $M /run/otter-ssh.main.bak\n\
         if [ -e $F ]; then cp -p $F /run/otter-ssh.drop.bak; else : > /run/otter-ssh.drop.none; fi\n\
         if [ -e $SF ]; then cp -p $SF /run/otter-ssh.sock.bak; else : > /run/otter-ssh.sock.none; fi\n\
         cat > {RB} <<'OTTER_RB'\n{ROLLBACK_BODY}OTTER_RB\n\
         undo() {{ sh {RB} >/dev/null 2>&1; }}\n"
    ));
    // 写入
    s.push_str(
        "if [ $INC = 1 ]; then\n  mkdir -p $D; touch $F\n  grep -vi \"^$K[[:space:]]\" $F > $F.tmp || true; echo \"$K $V\" >> $F.tmp; cat $F.tmp > $F; rm -f $F.tmp; chmod 644 $F\n\
         else\n  { echo \"$K $V # otter\"; grep -vE \"^$K[[:space:]].*# otter\\$\" $M || true; } > $M.otter-new; cat $M.otter-new > $M; rm -f $M.otter-new\nfi\n\
         err=$(sshd -t 2>&1) || { undo; echo \"sshd 配置校验失败,已还原:$(echo \"$err\" | tail -1)\" >&2; exit 1; }\n",
    );
    // 生效
    if key == "Port" {
        s.push_str("if [ $SOCK = 1 ]; then\n  mkdir -p $SD; printf '[Socket]\\nListenStream=\\nListenStream=%s\\n' \"$V\" > $SF\n  systemctl daemon-reload; systemctl restart ssh.socket || { undo; echo 'ssh.socket 重启失败,已还原' >&2; exit 1; }\nelse\n  systemctl reload $SVC >/dev/null 2>&1 || systemctl restart $SVC || { undo; echo 'sshd 重载失败,已还原' >&2; exit 1; }\nfi\nsleep 1\n");
    } else {
        s.push_str("if [ $SOCK = 0 ]; then systemctl reload $SVC >/dev/null 2>&1 || systemctl restart $SVC || { undo; echo 'sshd 重载失败,已还原' >&2; exit 1; }; fi\nsleep 1\n");
    }
    // 读回验证
    s.push_str(&format!("eff=$(sshd -T 2>/dev/null | awk '$1==\"{lower}\"{{print $2}}' | sed 's/^without-password$/prohibit-password/')\nwant=$(echo \"$V\" | tr 'A-Z' 'a-z')\n[ \"$eff\" = \"$want\" ] || {{ undo; echo \"设置没有生效(sshd 实际是「$eff」,可能被 Match 块或别的配置覆盖),已还原\" >&2; exit 1; }}\n"));
    if key == "Port" {
        s.push_str("ss -ltn 2>/dev/null | awk '{print $4}' | grep -qE \"[:.]$V\\$\" || { undo; echo \"sshd 没有在端口 $V 上监听,已还原\" >&2; exit 1; }\n");
    }
    // 排定时回滚
    s.push_str(&format!("command -v systemd-run >/dev/null 2>&1 && systemd-run --quiet --unit={GUARD_UNIT} --on-active={GUARD_SECS} --timer-property=AccuracySec=1s /bin/sh {RB} || true\n"));
    Ok(s)
}

// ---- 公钥 ----

const KEY_TYPES: [&str; 6] = ["ssh-ed25519", "ssh-rsa", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521", "sk-ssh-ed25519@openssh.com"];

/// 检查一行公钥:单行、`类型 base64 [备注]`
pub fn check_pubkey(line: &str) -> Result<String, String> {
    let l = line.trim();
    if l.is_empty() {
        return Err("请粘贴公钥".into());
    }
    if l.contains('\n') || l.contains('\r') {
        return Err("一次只能添加一把公钥(只能有一行)".into());
    }
    let toks: Vec<&str> = l.split_whitespace().collect();
    if toks.len() < 2 || !KEY_TYPES.contains(&toks[0]) {
        return Err("格式不对:公钥应该是「ssh-ed25519 AAAA… 备注」这样的一行(通常是 id_ed25519.pub 的内容)".into());
    }
    if toks[1].len() < 20 || !toks[1].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')) {
        return Err("公钥的内容部分不是有效的 base64,可能没复制完整".into());
    }
    if toks[0] == "ssh-rsa" && toks[1].len() < 300 {
        return Err("这把 RSA 公钥太短(不足 2048 位)".into());
    }
    Ok(l.to_string())
}

/// 添加公钥;公钥经 stdin 交给脚本
pub fn add_key_script(user: &str) -> Result<String, String> {
    if !super::common::valid_user(user) {
        return Err("用户名不合法".into());
    }
    Ok(format!(
        "{PATH_FIX}set -e\nU={}\nH=$(getent passwd \"$U\" | cut -d: -f6)\n[ -n \"$H\" ] && [ -d \"$H\" ] || {{ echo \"找不到 $U 的家目录\" >&2; exit 1; }}\nG=$(id -gn \"$U\")\nK=$(cat)\n\
         install -d -m 700 -o \"$U\" -g \"$G\" \"$H/.ssh\"\nF=$H/.ssh/authorized_keys\n[ -e \"$F\" ] || : > \"$F\"\n\
         if grep -qxF -e \"$K\" \"$F\"; then echo '这把公钥已经在里面了' >&2; exit 1; fi\n\
         [ -s \"$F\" ] && [ -n \"$(tail -c1 \"$F\")\" ] && echo >> \"$F\"\nprintf '%s\\n' \"$K\" >> \"$F\"\nchown \"$U:$G\" \"$F\"; chmod 600 \"$F\"\n\
         command -v restorecon >/dev/null 2>&1 && restorecon -R \"$H/.ssh\" >/dev/null 2>&1 || true\n",
        quote(user)
    ))
}

/// 删除一行公钥(整行匹配)
pub fn delete_key_script(user: &str, raw: &str) -> Result<String, String> {
    if !super::common::valid_user(user) {
        return Err("用户名不合法".into());
    }
    if raw.contains('\n') {
        return Err("公钥内容不对".into());
    }
    Ok(format!(
        "{PATH_FIX}set -e\nU={}\nH=$(getent passwd \"$U\" | cut -d: -f6)\nF=$H/.ssh/authorized_keys\nL={}\n[ -f \"$F\" ] || {{ echo '没有 authorized_keys 文件' >&2; exit 1; }}\n\
         grep -qxF -e \"$L\" \"$F\" || {{ echo '这把公钥已经不在文件里了,请刷新' >&2; exit 1; }}\ncp -p \"$F\" \"$F.otter-bak\"\n\
         grep -vxF -e \"$L\" \"$F.otter-bak\" > \"$F.otter-new\" || true\ncat \"$F.otter-new\" > \"$F\"; rm -f \"$F.otter-new\"\n",
        quote(user),
        quote(raw)
    ))
}

/// 删公钥前的防锁死检查:密码登录关着时,不能删掉当前登录用户的最后一把
pub fn check_delete_key(info: &SshInfo, login: &str, key: &AuthKey) -> Result<(), String> {
    if !info.password && key.user == login && info.user(login).map(|u| u.keys.len()).unwrap_or(0) <= 1 {
        return Err("密码登录已经关闭,这是你当前登录用户的最后一把公钥,删掉后就再也登录不了了。请先添加别的公钥,或者先开启密码登录".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBqXq3VtKzv1yrmZ6l1p0xPq0q8oQwYb5T1x2oR6mYcA me@laptop";

    const OUT: &str = "@@SSHD\nport 22\npermitrootlogin without-password\npasswordauthentication yes\npubkeyauthentication yes\nmaxauthtries 6\nclientaliveinterval 0\n@@UNIT\nssh\n@@SOCKET\nyes\n@@INCLUDE\nyes\n@@OTTER\nPasswordAuthentication yes\n@@KEYS\nU\troot\t/root\nK\tSHA256:abc\t(ED25519)\tssh-ed25519 AAAA me@laptop\nK\tSHA256:def\t(RSA)\tcommand=\"x\",no-pty ssh-rsa BBBB build box\nU\tdeploy\t/home/deploy\n";

    #[test]
    fn parse() {
        let i = parse_ssh(OUT).unwrap();
        assert_eq!(i.ports, vec![22]);
        assert_eq!(i.root_login, "prohibit-password");
        assert!(i.password && i.pubkey && i.socket && i.include);
        assert_eq!(i.max_tries, Some(6));
        assert_eq!(i.unit, "ssh");
        assert_eq!(i.otter_value("passwordauthentication"), Some("yes"));
        assert_eq!(i.users.len(), 2);
        let r = i.user("root").unwrap();
        assert_eq!(r.keys.len(), 2);
        assert_eq!((r.keys[0].fp.as_str(), r.keys[0].alg.as_str(), r.keys[0].comment.as_str(), r.keys[0].options), ("SHA256:abc", "(ED25519)", "me@laptop", false));
        assert!(r.keys[1].options);
        assert_eq!(r.keys[1].comment, "build box");
        assert!(i.user("deploy").unwrap().keys.is_empty());
        assert!(parse_ssh("@@SSHD\n").is_err());
    }

    #[test]
    fn settings_validation() {
        assert!(setting_ok("Port", "2222").is_ok());
        assert!(setting_ok("Port", "0").is_err() && setting_ok("Port", "70000").is_err() && setting_ok("Port", "22; reboot").is_err() && setting_ok("Port", "022").is_err());
        assert!(setting_ok("PermitRootLogin", "no").is_ok() && setting_ok("PermitRootLogin", "maybe").is_err());
        assert!(setting_ok("MaxAuthTries", "21").is_err() && setting_ok("ClientAliveInterval", "60").is_ok());
        assert!(setting_ok("UsePAM", "no").is_err());
        assert!(set_script("Port", "2222").unwrap().contains("ssh.socket"));
        let s = set_script("PasswordAuthentication", "no").unwrap();
        assert!(s.contains("sshd -t") && s.contains("otter-ssh-rollback") && s.contains("sshd -T"));
        assert!(!s.contains("semanage"));
    }

    #[test]
    fn lockout_checks() {
        let mut i = parse_ssh(OUT).unwrap();
        assert!(check_change(&i, "deploy", "PasswordAuthentication", "no").is_err());
        assert!(check_change(&i, "root", "PasswordAuthentication", "no").is_ok());
        assert!(check_change(&i, "root", "PermitRootLogin", "no").is_err());
        assert!(check_change(&i, "deploy", "PermitRootLogin", "no").is_ok());
        i.password = false;
        assert!(check_change(&i, "root", "PubkeyAuthentication", "no").is_err());
        let k = i.user("root").unwrap().keys[0].clone();
        assert!(check_delete_key(&i, "root", &k).is_ok());
        let mut one = i.clone();
        one.users[0].keys.truncate(1);
        assert!(check_delete_key(&one, "root", &k).is_err());
        assert!(check_delete_key(&one, "deploy", &k).is_ok());
    }

    #[test]
    fn pubkey() {
        assert!(check_pubkey(KEY).is_ok());
        assert!(check_pubkey("ssh-ed25519").is_err());
        assert!(check_pubkey("hello world").is_err());
        assert!(check_pubkey(&format!("{KEY}\n{KEY}")).is_err());
        assert!(check_pubkey("ssh-ed25519 AAAA$$$$$$$$$$$$$$$$$$$$$$$$ x").is_err());
        assert!(add_key_script("deploy").unwrap().contains("authorized_keys"));
        assert!(add_key_script("De ploy").is_err());
        assert!(delete_key_script("deploy", KEY).unwrap().contains("grep -vxF"));
        assert!(delete_key_script("deploy", "a\nb").is_err());
    }
}
