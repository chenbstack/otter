//! fail2ban:读状态、解封、改封禁参数和白名单、启停、(用户确认后)用发行版包管理器安装。
//!
//! fail2ban 是发行版软件包提供的服务,由 systemd 托管,不依赖 Otter 常驻(见决策 0002);Otter 不自动安装。
//! 配置只写 `/etc/fail2ban/jail.d/otter.local`,不碰 jail.conf 和用户自己的 jail。
//! 白名单默认带上本次 SSH 连接的来源 IP,避免输错密码把自己封了。

use super::common::{first, sections, valid_ip_or_cidr};
use super::PATH_FIX;

pub const CONF_FILE: &str = "/etc/fail2ban/jail.d/otter.local";

pub const F2B_SCRIPT: &str = r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH
echo '@@BIN'; command -v fail2ban-client || echo none
echo '@@PKG'; if command -v apt-get >/dev/null 2>&1; then echo apt; elif command -v dnf >/dev/null 2>&1; then echo dnf; elif command -v yum >/dev/null 2>&1; then echo yum; else echo none; fi
echo '@@ACTIVE'; systemctl is-active fail2ban 2>/dev/null
echo '@@CLIENT'; echo "${SSH_CONNECTION%% *}"
echo '@@CONF'; cat /etc/fail2ban/jail.d/otter.local 2>/dev/null
echo '@@JAILS'
if command -v fail2ban-client >/dev/null 2>&1 && systemctl is-active fail2ban >/dev/null 2>&1; then
  for j in $(fail2ban-client status 2>/dev/null | sed -n 's/.*Jail list:[[:space:]]*//p' | tr -d ','); do
    printf 'J\t%s\n' "$j"
    fail2ban-client status "$j" 2>/dev/null | sed 's/^[ |`-]*//'
    for k in bantime findtime maxretry; do printf 'S\t%s\t%s\n' "$k" "$(fail2ban-client get "$j" "$k" 2>/dev/null)"; done
  done
fi
true
"#;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Jail {
    pub name: String,
    pub failed_now: u64,
    pub failed_total: u64,
    pub banned_now: u64,
    pub banned_total: u64,
    pub banned: Vec<String>,
    /// 秒;-1 是永久
    pub bantime: Option<i64>,
    pub findtime: Option<i64>,
    pub maxretry: Option<i64>,
}

/// otter.local 的内容:`[DEFAULT]` 和 `[sshd]` 两段的键值,其余原样保留
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conf {
    pub default: Vec<(String, String)>,
    pub sshd: Vec<(String, String)>,
    pub extra: String,
}

impl Conf {
    pub fn parse(text: &str) -> Conf {
        let mut c = Conf::default();
        let mut sec = String::new();
        for line in text.lines() {
            let t = line.trim();
            if t.starts_with("# 由 Otter 管理") {
                continue;
            }
            if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                sec = name.to_string();
                if sec != "DEFAULT" && sec != "sshd" {
                    c.extra.push_str(line);
                    c.extra.push('\n');
                }
                continue;
            }
            match sec.as_str() {
                "DEFAULT" | "sshd" => {
                    if t.is_empty() || t.starts_with('#') {
                        continue;
                    }
                    if let Some((k, v)) = t.split_once('=') {
                        let list = if sec == "DEFAULT" { &mut c.default } else { &mut c.sshd };
                        list.retain(|(kk, _)| kk != k.trim());
                        list.push((k.trim().to_string(), v.trim().to_string()));
                    }
                }
                "" => {}
                _ => {
                    c.extra.push_str(line);
                    c.extra.push('\n');
                }
            }
        }
        c
    }

    pub fn render(&self) -> String {
        let mut s = String::from("# 由 Otter 管理(工具箱 → fail2ban),其它配置请写在别的文件里\n");
        if !self.default.is_empty() {
            s.push_str("[DEFAULT]\n");
            for (k, v) in &self.default {
                s.push_str(&format!("{k} = {v}\n"));
            }
            s.push('\n');
        }
        if !self.sshd.is_empty() {
            s.push_str("[sshd]\n");
            for (k, v) in &self.sshd {
                s.push_str(&format!("{k} = {v}\n"));
            }
            s.push('\n');
        }
        s.push_str(&self.extra);
        s
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.default.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn set(&mut self, key: &str, val: &str) {
        self.default.retain(|(k, _)| k != key);
        self.default.push((key.to_string(), val.to_string()));
    }

    pub fn sshd_enabled(&self) -> bool {
        self.sshd.iter().any(|(k, v)| k == "enabled" && v == "true")
    }

    pub fn set_sshd_enabled(&mut self, on: bool) {
        self.sshd.retain(|(k, _)| k != "enabled");
        self.sshd.insert(0, ("enabled".into(), if on { "true" } else { "false" }.into()));
    }

    pub fn ignore(&self) -> Vec<String> {
        self.get("ignoreip").map(|v| v.split_whitespace().map(str::to_string).collect()).unwrap_or_default()
    }

    pub fn set_ignore(&mut self, list: &[String]) {
        self.set("ignoreip", &list.join(" "));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct F2bInfo {
    /// fail2ban-client 的路径;None 是没装
    pub bin: Option<String>,
    /// apt / dnf / yum / none
    pub pkg: String,
    pub active: bool,
    /// 本次 SSH 连接的来源 IP(服务器看到的)
    pub client: String,
    pub conf: Conf,
    pub jails: Vec<Jail>,
}

fn num(s: &str) -> u64 {
    s.trim().parse().unwrap_or(0)
}

pub fn parse_f2b(out: &str) -> Result<F2bInfo, String> {
    let m = sections(out);
    if !m.contains_key("BIN") {
        return Err("读不到 fail2ban 的状态".into());
    }
    let bin = Some(first(&m, "BIN")).filter(|b| !b.is_empty() && b != "none");
    let mut jails: Vec<Jail> = Vec::new();
    for l in m.get("JAILS").into_iter().flatten() {
        if let Some(n) = l.strip_prefix("J\t") {
            jails.push(Jail { name: n.trim().to_string(), ..Default::default() });
            continue;
        }
        let Some(j) = jails.last_mut() else { continue };
        if let Some(r) = l.strip_prefix("S\t") {
            let (k, v) = r.split_once('\t').unwrap_or((r, ""));
            let v = v.trim().parse::<i64>().ok();
            match k {
                "bantime" => j.bantime = v,
                "findtime" => j.findtime = v,
                "maxretry" => j.maxretry = v,
                _ => {}
            }
        } else if let Some((k, v)) = l.split_once(':') {
            match k.trim() {
                "Currently failed" => j.failed_now = num(v),
                "Total failed" => j.failed_total = num(v),
                "Currently banned" => j.banned_now = num(v),
                "Total banned" => j.banned_total = num(v),
                "Banned IP list" => j.banned = v.split_whitespace().map(str::to_string).collect(),
                _ => {}
            }
        }
    }
    Ok(F2bInfo { bin, pkg: first(&m, "PKG"), active: first(&m, "ACTIVE") == "active", client: first(&m, "CLIENT"), conf: Conf::parse(&m.get("CONF").map(|v| v.join("\n")).unwrap_or_default()), jails })
}

/// 常用封禁时长(秒,-1 永久)
pub const BANTIMES: [(i64, &str); 5] = [(600, "10 分钟"), (3600, "1 小时"), (86400, "1 天"), (604_800, "7 天"), (-1, "永久")];
pub const FINDTIMES: [(i64, &str); 3] = [(600, "10 分钟"), (3600, "1 小时"), (86400, "1 天")];
pub const MAXRETRIES: [i64; 3] = [3, 5, 10];

/// 秒数写成 fail2ban 配置里的值(整秒数即可)
pub fn time_value(secs: i64) -> String {
    secs.to_string()
}

pub fn fmt_secs(s: i64) -> String {
    match s {
        -1 => "永久".into(),
        s if s % 86400 == 0 && s >= 86400 => format!("{} 天", s / 86400),
        s if s % 3600 == 0 && s >= 3600 => format!("{} 小时", s / 3600),
        s if s % 60 == 0 && s >= 60 => format!("{} 分钟", s / 60),
        s => format!("{s} 秒"),
    }
}

/// 第一次启用时的配置:白名单(回环 + 本次连接的来源 IP)、sshd jail 打开
pub fn initial_conf(client: &str) -> Conf {
    let mut c = Conf::default();
    let mut ig = vec!["127.0.0.1/8".to_string(), "::1".to_string()];
    if valid_ip_or_cidr(client) && !ig.iter().any(|i| i == client) {
        ig.push(client.to_string());
    }
    c.set_ignore(&ig);
    c.set_sshd_enabled(true);
    c
}

/// 配置里的值要合法,不然写进去 fail2ban 起不来
pub fn check_conf(c: &Conf) -> Result<(), String> {
    for (k, v) in c.default.iter().chain(c.sshd.iter()) {
        if v.contains('\n') || k.contains('\n') {
            return Err("配置里不能有换行".into());
        }
    }
    for ip in c.ignore() {
        if !valid_ip_or_cidr(&ip) {
            return Err(format!("白名单里的「{ip}」不是有效的 IP 或网段"));
        }
    }
    for k in ["bantime", "findtime", "maxretry"] {
        if let Some(v) = c.get(k) {
            if v.parse::<i64>().is_err() {
                return Err(format!("{k} 要是整数"));
            }
        }
    }
    Ok(())
}

/// 写 otter.local 的片段(新内容从 stdin 来):备份、写入、`fail2ban-client -t` 检查、服务在跑就重载,失败还原
const WRITE_CONF: &str = r#"F=/etc/fail2ban/jail.d/otter.local
mkdir -p /etc/fail2ban/jail.d
rm -f /run/otter-f2b.bak
[ -f "$F" ] && cp -p "$F" /run/otter-f2b.bak
cat > "$F"
back() { if [ -f /run/otter-f2b.bak ]; then cp -p /run/otter-f2b.bak "$F"; else rm -f "$F"; fi; }
if grep -q '^\[sshd\]' "$F" && [ ! -e /var/log/auth.log ] && [ ! -e /var/log/secure ] && ! sed -n '/^\[sshd\]/,/^\[/p' "$F" | grep -q '^backend'; then sed -i '/^\[sshd\]/a backend = systemd' "$F"; fi
out=$(fail2ban-client -t 2>&1) || { back; echo "配置检查没通过,已还原:$(echo "$out" | tail -1)" >&2; exit 1; }
if systemctl is-active fail2ban >/dev/null 2>&1; then
  systemctl restart fail2ban || { back; systemctl restart fail2ban >/dev/null 2>&1; echo 'fail2ban 重启失败,已还原' >&2; exit 1; }
  sleep 2
  fail2ban-client ping >/dev/null 2>&1 || { back; systemctl restart fail2ban >/dev/null 2>&1; echo 'fail2ban 重启后没有响应,已还原' >&2; exit 1; }
fi
"#;

/// 保存配置(stdin 是 `Conf::render()` 的内容)
pub fn save_conf_script(c: &Conf) -> Result<(String, String), String> {
    check_conf(c)?;
    Ok((format!("{PATH_FIX}set -e\n{WRITE_CONF}"), c.render()))
}

/// 安装 fail2ban 并写入初始配置(stdin 是配置内容)
pub fn install_script(pkg: &str, c: &Conf) -> Result<(String, String), String> {
    check_conf(c)?;
    let install = match pkg {
        "apt" => "export DEBIAN_FRONTEND=noninteractive\napt-get update -qq\napt-get install -y -qq fail2ban\ncommand -v nft >/dev/null 2>&1 || command -v iptables >/dev/null 2>&1 || apt-get install -y -qq nftables\n",
        "dnf" => "dnf install -y -q fail2ban || { dnf install -y -q epel-release && dnf install -y -q fail2ban; }\n",
        "yum" => "yum install -y -q fail2ban || { yum install -y -q epel-release && yum install -y -q fail2ban; }\n",
        _ => return Err("没找到包管理器(apt、dnf、yum),请手动安装 fail2ban".into()),
    };
    // 没有 firewalld 但有 nft 时,封禁走 nftables;其它情况用发行版默认
    let ban = "if ! systemctl is-active firewalld >/dev/null 2>&1 && command -v nft >/dev/null 2>&1 && ! grep -q '^banaction' \"$F\"; then sed -i '/^\\[DEFAULT\\]/a banaction = nftables\\nbanaction_allports = nftables[type=allports]' \"$F\"; fi\n";
    // 安装时服务还没起来,WRITE_CONF 里的重启分支不会走;写完配置后启动并确认
    let start = "systemctl enable --now fail2ban >/dev/null 2>&1 || systemctl start fail2ban || { back; echo 'fail2ban 启动失败' >&2; exit 1; }\nsleep 2\nfail2ban-client ping >/dev/null 2>&1 || { back; systemctl restart fail2ban >/dev/null 2>&1; echo 'fail2ban 启动后没有响应' >&2; exit 1; }\n";
    let mut conf_part = WRITE_CONF.to_string();
    conf_part = conf_part.replacen("out=$(fail2ban-client -t", &format!("{ban}out=$(fail2ban-client -t"), 1);
    Ok((format!("{PATH_FIX}set -e\n{install}{conf_part}{start}"), c.render()))
}

fn jail_ok(j: &str) -> bool {
    !j.is_empty() && j.len() <= 64 && j.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

pub fn unban_script(jail: &str, ip: &str) -> Result<String, String> {
    if !jail_ok(jail) {
        return Err("jail 名字不合法".into());
    }
    if !super::common::valid_ip(ip) {
        return Err("IP 不合法".into());
    }
    Ok(format!("{PATH_FIX}set -e\nfail2ban-client set {jail} unbanip {ip} >/dev/null\n"))
}

pub fn service_script(on: bool) -> String {
    if on {
        format!("{PATH_FIX}set -e\nsystemctl enable --now fail2ban\nsleep 1\nsystemctl is-active fail2ban >/dev/null\n")
    } else {
        format!("{PATH_FIX}set -e\nsystemctl disable --now fail2ban\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "@@BIN\n/usr/bin/fail2ban-client\n@@PKG\napt\n@@ACTIVE\nactive\n@@CLIENT\n203.0.113.9\n@@CONF\n# 由 Otter 管理\n[DEFAULT]\nbantime = 3600\nignoreip = 127.0.0.1/8 ::1 203.0.113.9\n\n[sshd]\nenabled = true\n@@JAILS\nJ\tsshd\nCurrently failed:\t2\nTotal failed:\t17\nFile list:\t/var/log/auth.log\nCurrently banned:\t1\nTotal banned:\t4\nBanned IP list:\t198.51.100.7 198.51.100.8\nS\tbantime\t3600\nS\tfindtime\t600\nS\tmaxretry\t5\n";

    #[test]
    fn parse() {
        let i = parse_f2b(OUT).unwrap();
        assert_eq!(i.bin.as_deref(), Some("/usr/bin/fail2ban-client"));
        assert!(i.active);
        assert_eq!(i.client, "203.0.113.9");
        assert_eq!(i.jails.len(), 1);
        let j = &i.jails[0];
        assert_eq!((j.failed_now, j.failed_total, j.banned_now, j.banned_total), (2, 17, 1, 4));
        assert_eq!(j.banned, vec!["198.51.100.7", "198.51.100.8"]);
        assert_eq!((j.bantime, j.findtime, j.maxretry), (Some(3600), Some(600), Some(5)));
        assert_eq!(i.conf.get("bantime"), Some("3600"));
        assert!(i.conf.sshd_enabled());
        assert_eq!(i.conf.ignore().len(), 3);
        let none = parse_f2b("@@BIN\nnone\n@@PKG\ndnf\n@@ACTIVE\ninactive\n").unwrap();
        assert!(none.bin.is_none() && !none.active && none.jails.is_empty());
        assert!(parse_f2b("").is_err());
    }

    #[test]
    fn conf_roundtrip() {
        let mut c = Conf::parse("[DEFAULT]\nbantime = 600\n[sshd]\nenabled = true\n[nginx-http-auth]\nenabled = true\nport = http\n");
        assert_eq!(c.extra, "[nginx-http-auth]\nenabled = true\nport = http\n");
        c.set("bantime", "86400");
        c.set_ignore(&["10.0.0.0/8".into()]);
        let r = c.render();
        assert!(r.contains("bantime = 86400") && r.contains("ignoreip = 10.0.0.0/8") && r.contains("[nginx-http-auth]"));
        assert_eq!(Conf::parse(&r), c);
        c.set_sshd_enabled(false);
        assert!(!c.sshd_enabled());
    }

    #[test]
    fn initial_and_check() {
        let c = initial_conf("203.0.113.9");
        assert_eq!(c.ignore(), vec!["127.0.0.1/8", "::1", "203.0.113.9"]);
        assert!(c.sshd_enabled());
        assert_eq!(initial_conf("").ignore().len(), 2);
        let mut bad = c.clone();
        bad.set_ignore(&["not-an-ip".into()]);
        assert!(check_conf(&bad).is_err());
        let mut bad2 = c.clone();
        bad2.set("bantime", "1h; reboot");
        assert!(check_conf(&bad2).is_err());
    }

    #[test]
    fn scripts() {
        let c = initial_conf("1.2.3.4");
        let (s, input) = install_script("apt", &c).unwrap();
        assert!(s.contains("apt-get install") && s.contains("fail2ban-client -t") && s.contains("banaction = nftables") && input.contains("enabled = true"));
        assert!(install_script("dnf", &c).unwrap().0.contains("epel-release"));
        assert!(install_script("none", &c).is_err());
        assert!(unban_script("sshd", "1.2.3.4").is_ok());
        assert!(unban_script("sshd; x", "1.2.3.4").is_err());
        assert!(unban_script("sshd", "1.2.3.4; x").is_err());
        assert_eq!(fmt_secs(-1), "永久");
        assert_eq!(fmt_secs(86400), "1 天");
        assert_eq!(fmt_secs(600), "10 分钟");
        assert_eq!(fmt_secs(90), "90 秒");
    }
}
