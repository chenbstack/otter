//! 本机用户与 sudo:列出可登录用户(root 和 UID ≥ 1000)、新建、改密码、锁定/解锁、加入/移出 sudo 组、删除。
//!
//! 密码和公钥都经 stdin 交给脚本,不进命令行。不允许对 root 做锁定、删除、移出 sudo;
//! 当前连接用的用户也不允许锁定、删除、移出 sudo(会把自己关在外面)。

use super::common::{first, sections, valid_user};
use super::PATH_FIX;
use crate::sysops::quote;

pub const USERS_SCRIPT: &str = r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH
echo '@@GROUP'; for g in sudo wheel admin; do getent group $g >/dev/null 2>&1 && { echo $g; break; }; done
today=$(( $(date +%s) / 86400 ))
echo '@@USERS'
{ getent passwd 2>/dev/null || cat /etc/passwd; } | while IFS=: read -r n x u g gecos h s; do
  if [ "$u" = 0 ] || { [ "$u" -ge 1000 ] 2>/dev/null && [ "$u" -lt 60000 ]; }; then
    sh=$(awk -F: -v n="$n" '$1==n{print $2 ":" $8}' /etc/shadow 2>/dev/null)
    hash=${sh%%:*}; exp=${sh#*:}
    case "$hash" in '') st=empty;; '!'|'!!'|'*') st=nopw;; '!'*|'*'*) st=locked;; *) st=pw;; esac
    ex=0; [ -n "$exp" ] && [ "$exp" -le "$today" ] 2>/dev/null && ex=1
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$n" "$u" "$h" "$s" "$st" "$ex" "$(id -nG "$n" 2>/dev/null | tr ' ' ',')" "$gecos"
  fi
done
true
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pw {
    /// 有密码
    Set,
    /// 没设密码,只能用公钥登录
    None,
    /// 密码被锁(usermod -L)
    Locked,
    /// 空密码(危险)
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserRow {
    pub name: String,
    pub uid: u32,
    pub home: String,
    pub shell: String,
    pub pw: Pw,
    /// 账号已过期(`usermod -e 1` 之类)
    pub expired: bool,
    pub groups: Vec<String>,
    pub gecos: String,
}

impl UserRow {
    pub fn locked(&self) -> bool {
        self.pw == Pw::Locked || self.expired
    }

    /// 能不能登录 shell
    pub fn has_shell(&self) -> bool {
        !(self.shell.ends_with("nologin") || self.shell.ends_with("/false"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsersInfo {
    /// 管理员组:sudo、wheel 或 admin;都没有是 None
    pub admin_group: Option<String>,
    pub users: Vec<UserRow>,
}

impl UsersInfo {
    pub fn is_admin(&self, u: &UserRow) -> bool {
        u.uid == 0 || self.admin_group.as_ref().is_some_and(|g| u.groups.contains(g))
    }
}

pub fn parse_users(out: &str) -> Result<UsersInfo, String> {
    let m = sections(out);
    let Some(list) = m.get("USERS") else { return Err("读不到用户列表".into()) };
    let users: Vec<UserRow> = list
        .iter()
        .filter_map(|l| {
            let p: Vec<&str> = l.splitn(8, '\t').collect();
            if p.len() < 7 {
                return None;
            }
            Some(UserRow {
                name: p[0].to_string(),
                uid: p[1].parse().ok()?,
                home: p[2].to_string(),
                shell: p[3].to_string(),
                pw: match p[4] {
                    "pw" => Pw::Set,
                    "locked" => Pw::Locked,
                    "empty" => Pw::Empty,
                    _ => Pw::None,
                },
                expired: p[5] == "1",
                groups: p[6].split(',').filter(|s| !s.is_empty()).map(str::to_string).collect(),
                gecos: p.get(7).map(|s| s.split(',').next().unwrap_or("").to_string()).unwrap_or_default(),
            })
        })
        .collect();
    if users.is_empty() {
        return Err("没有读到任何用户".into());
    }
    let g = first(&m, "GROUP");
    Ok(UsersInfo { admin_group: (!g.is_empty()).then_some(g), users })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Password,
    Lock,
    Unlock,
    SudoOn,
    SudoOff,
    Delete,
}

/// 能不能对这个用户做这件事;`login` 是当前连接用的用户名
pub fn check_op(info: &UsersInfo, login: &str, name: &str, op: Op) -> Result<(), String> {
    let Some(u) = info.users.iter().find(|u| u.name == name) else { return Err(format!("没有用户 {name}")) };
    let dangerous = matches!(op, Op::Lock | Op::Delete | Op::SudoOff);
    if dangerous && u.uid == 0 {
        return Err("不能对 root 做这个操作".into());
    }
    if dangerous && name == login {
        return Err(format!("{name} 是你现在连接用的用户,这样做会把自己关在外面。请换一个用户登录后再操作"));
    }
    if matches!(op, Op::SudoOn | Op::SudoOff) && info.admin_group.is_none() {
        return Err("这台服务器上没有 sudo、wheel 或 admin 组".into());
    }
    if op == Op::SudoOn && info.is_admin(u) {
        return Err(format!("{name} 已经是管理员"));
    }
    Ok(())
}

/// 新建用户。`stdin` 的第一行是密码(可空),第二行是公钥(可空);两者至少要有一个
pub fn create_script(name: &str, sudo_group: Option<&str>, password: &str, pubkey: &str) -> Result<(String, String), String> {
    if !valid_user(name) {
        return Err("用户名只能用小写字母、数字、_ 和 -,以字母或 _ 开头,最长 32 个字符".into());
    }
    if password.is_empty() && pubkey.is_empty() {
        return Err("密码和公钥至少要填一个,否则这个用户没法登录".into());
    }
    if password.contains('\n') || password.contains('\r') {
        return Err("密码里不能有换行".into());
    }
    let key = if pubkey.is_empty() { String::new() } else { super::ssh::check_pubkey(pubkey)? };
    if let Some(g) = sudo_group {
        if !valid_user(g) {
            return Err("管理员组名不合法".into());
        }
    }
    let mut s = format!(
        "{PATH_FIX}set -e\nU={}\nid \"$U\" >/dev/null 2>&1 && {{ echo \"用户 $U 已经存在\" >&2; exit 1; }}\nIFS= read -r PW || PW=\nIFS= read -r KEY || KEY=\n\
         SH=/bin/bash; [ -x /bin/bash ] || SH=/bin/sh\nOK=0; trap '[ \"$OK\" = 1 ] || userdel -rf \"$U\" >/dev/null 2>&1' EXIT\nuseradd -m -s \"$SH\" \"$U\"\n\
         [ -z \"$PW\" ] || printf '%s:%s\\n' \"$U\" \"$PW\" | chpasswd\n",
        quote(name)
    );
    if let Some(g) = sudo_group {
        s.push_str(&format!("usermod -aG {} \"$U\"\n", quote(g)));
    }
    s.push_str("if [ -n \"$KEY\" ]; then\n  H=$(getent passwd \"$U\" | cut -d: -f6); G=$(id -gn \"$U\")\n  install -d -m 700 -o \"$U\" -g \"$G\" \"$H/.ssh\"\n  printf '%s\\n' \"$KEY\" > \"$H/.ssh/authorized_keys\"; chown \"$U:$G\" \"$H/.ssh/authorized_keys\"; chmod 600 \"$H/.ssh/authorized_keys\"\n  command -v restorecon >/dev/null 2>&1 && restorecon -R \"$H/.ssh\" >/dev/null 2>&1 || true\nfi\nOK=1\n");
    Ok((s, format!("{password}\n{key}\n")))
}

/// 改密码;密码经 stdin
pub fn password_script(name: &str, password: &str) -> Result<(String, String), String> {
    if !valid_user(name) {
        return Err("用户名不合法".into());
    }
    if password.is_empty() {
        return Err("密码不能为空".into());
    }
    if password.contains('\n') || password.contains('\r') {
        return Err("密码里不能有换行".into());
    }
    Ok((format!("{PATH_FIX}set -e\nU={}\nIFS= read -r PW\nprintf '%s:%s\\n' \"$U\" \"$PW\" | chpasswd\n", quote(name)), format!("{password}\n")))
}

/// 锁定:密码锁住并让账号过期,密码和公钥都登录不了
pub fn lock_script(name: &str) -> Result<String, String> {
    if !valid_user(name) {
        return Err("用户名不合法".into());
    }
    Ok(format!("{PATH_FIX}set -e\nU={}\nusermod -L -e 1 \"$U\"\nloginctl terminate-user \"$U\" >/dev/null 2>&1 || pkill -KILL -u \"$U\" >/dev/null 2>&1 || true\n", quote(name)))
}

pub fn unlock_script(name: &str) -> Result<String, String> {
    if !valid_user(name) {
        return Err("用户名不合法".into());
    }
    // 没设过密码的用户 usermod -U 会拒绝(会变成空密码),忽略它的报错
    Ok(format!("{PATH_FIX}set -e\nU={}\nusermod -e '' \"$U\"\nusermod -U \"$U\" >/dev/null 2>&1 || true\n", quote(name)))
}

pub fn sudo_script(name: &str, group: &str, on: bool) -> Result<String, String> {
    if !valid_user(name) || !valid_user(group) {
        return Err("名称不合法".into());
    }
    Ok(if on {
        format!("{PATH_FIX}set -e\nusermod -aG {} {}\n", quote(group), quote(name))
    } else {
        format!("{PATH_FIX}set -e\ngpasswd -d {} {} >/dev/null\n", quote(name), quote(group))
    })
}

pub fn delete_script(name: &str, remove_home: bool) -> Result<String, String> {
    if !valid_user(name) {
        return Err("用户名不合法".into());
    }
    let r = if remove_home { " -r" } else { "" };
    Ok(format!(
        "{PATH_FIX}U={}\nid \"$U\" >/dev/null 2>&1 || {{ echo \"用户 $U 已经不存在\" >&2; exit 1; }}\nloginctl terminate-user \"$U\" >/dev/null 2>&1 || true\npkill -KILL -u \"$U\" >/dev/null 2>&1 || true\nsleep 1\n\
         userdel{r} \"$U\" >/dev/null 2>&1 || true\nif id \"$U\" >/dev/null 2>&1; then echo \"删除失败,用户 $U 还在(可能还有进程在用)\" >&2; exit 1; fi\n",
        quote(name)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "@@GROUP\nsudo\n@@USERS\nroot\t0\t/root\t/bin/bash\tpw\t0\troot\troot\ndeploy\t1000\t/home/deploy\t/bin/bash\tnopw\t0\tdeploy,sudo\tDeploy,,,\nbob\t1001\t/home/bob\t/bin/sh\tlocked\t0\tbob\t\nold\t1002\t/home/old\t/bin/bash\tpw\t1\told\told\nsvc\t1003\t/home/svc\t/usr/sbin/nologin\tnopw\t0\tsvc\t\n";

    #[test]
    fn parse() {
        let i = parse_users(OUT).unwrap();
        assert_eq!(i.admin_group.as_deref(), Some("sudo"));
        assert_eq!(i.users.len(), 5);
        assert!(i.is_admin(&i.users[0]) && i.is_admin(&i.users[1]) && !i.is_admin(&i.users[2]));
        assert_eq!(i.users[1].pw, Pw::None);
        assert_eq!(i.users[1].gecos, "Deploy");
        assert!(i.users[2].locked() && i.users[3].locked() && !i.users[1].locked());
        assert!(!i.users[4].has_shell() && i.users[0].has_shell());
        assert!(parse_users("@@USERS\n").is_err());
    }

    #[test]
    fn op_checks() {
        let i = parse_users(OUT).unwrap();
        assert!(check_op(&i, "deploy", "root", Op::Delete).is_err());
        assert!(check_op(&i, "deploy", "root", Op::Password).is_ok());
        assert!(check_op(&i, "deploy", "deploy", Op::Lock).is_err());
        assert!(check_op(&i, "deploy", "deploy", Op::Password).is_ok());
        assert!(check_op(&i, "deploy", "bob", Op::Delete).is_ok());
        assert!(check_op(&i, "deploy", "bob", Op::SudoOn).is_ok());
        assert!(check_op(&i, "deploy", "deploy", Op::SudoOn).is_err());
        let mut n = i.clone();
        n.admin_group = None;
        assert!(check_op(&n, "x", "bob", Op::SudoOn).is_err());
        assert!(check_op(&i, "x", "nobody", Op::Lock).is_err());
    }

    #[test]
    fn scripts() {
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBqXq3VtKzv1yrmZ6l1p0xPq0q8oQwYb5T1x2oR6mYcA me";
        let (s, input) = create_script("alice", Some("sudo"), "pw", key).unwrap();
        assert!(s.contains("useradd -m") && s.contains("usermod -aG 'sudo'") && s.contains("trap"));
        assert_eq!(input, format!("pw\n{key}\n"));
        assert!(create_script("alice", None, "", "").is_err());
        assert!(create_script("Alice", None, "x", "").is_err());
        assert!(create_script("alice", None, "a\nb", "").is_err());
        assert!(create_script("alice", None, "", "not a key").is_err());
        assert!(!create_script("alice", None, "p", "").unwrap().0.contains("usermod -aG"));
        assert!(password_script("alice", "").is_err());
        assert!(!password_script("alice", "s3cret").unwrap().0.contains("s3cret"));
        assert!(delete_script("bob", true).unwrap().contains("userdel -r"));
        assert!(!delete_script("bob", false).unwrap().contains("userdel -r"));
        assert!(sudo_script("bob", "wheel", false).unwrap().contains("gpasswd -d"));
        assert!(lock_script("a;b").is_err());
    }
}
