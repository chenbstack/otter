//! 主机名、/etc/hosts、DNS 服务器。
//!
//! - 主机名:`hostnamectl set-hostname`,并同步 /etc/hosts 里的 `127.0.1.1` 行(Debian/Ubuntu 惯例)。
//! - hosts:整个文件的每一条记录都能改(按行号 + 原文核对,文件在你打开之后被改过就拒绝)。
//! - DNS:systemd-resolved 在跑时写 drop-in `99-otter.conf`;resolv.conf 是普通文件时直接改 nameserver 行;
//!   符号链接或网络管理器生成时只读(改了会被覆盖)。

use super::common::{first, sections, valid_hostname, valid_ip};
use super::PATH_FIX;
use crate::sysops::quote;

pub const HOST_SCRIPT: &str = r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH
echo '@@HOSTNAME'; hostname 2>/dev/null
echo '@@STATIC'; { hostnamectl --static 2>/dev/null || cat /etc/hostname 2>/dev/null; } | head -1
echo '@@HOSTS'; cat /etc/hosts 2>/dev/null
echo '@@MODE'
if systemctl is-active systemd-resolved >/dev/null 2>&1; then echo resolved
elif [ -L /etc/resolv.conf ]; then echo "readonly /etc/resolv.conf 是符号链接(指向 $(readlink -f /etc/resolv.conf)),由其它服务管理"
elif grep -qiE 'NetworkManager|resolvconf|systemd-resolved|dhclient|netconfig' /etc/resolv.conf 2>/dev/null; then echo "readonly /etc/resolv.conf 由网络管理器自动生成,直接改会被覆盖"
else echo file; fi
echo '@@DNS'; resolvectl dns 2>/dev/null
echo '@@RESOLV'; grep '^nameserver' /etc/resolv.conf 2>/dev/null
echo '@@CONF'; cat /etc/systemd/resolved.conf.d/99-otter.conf 2>/dev/null
true
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DnsMode {
    /// systemd-resolved 在跑:写 drop-in
    Resolved,
    /// resolv.conf 是普通文件:直接改
    File,
    /// 改不了,附原因
    Readonly(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostEntry {
    /// 在 /etc/hosts 里的行号(从 1 数)
    pub line: usize,
    /// 原文整行(核对用)
    pub raw: String,
    pub ip: String,
    pub names: Vec<String>,
    pub comment: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostInfo {
    pub hostname: String,
    pub static_name: String,
    pub entries: Vec<HostEntry>,
    pub mode: DnsMode,
    /// 当前实际生效的 DNS 服务器
    pub dns: Vec<String>,
    /// 其中由 Otter 设置的(drop-in 里的 DNS=)
    pub dns_otter: Vec<String>,
}

pub fn parse_hosts(text: &str) -> Vec<HostEntry> {
    let mut v = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let t = raw.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let (body, comment) = match t.split_once('#') {
            Some((b, c)) => (b, c.trim().to_string()),
            None => (t, String::new()),
        };
        let mut it = body.split_whitespace();
        let Some(ip) = it.next() else { continue };
        let names: Vec<String> = it.map(str::to_string).collect();
        v.push(HostEntry { line: i + 1, raw: raw.to_string(), ip: ip.to_string(), names, comment });
    }
    v
}

pub fn parse_host(out: &str) -> Result<HostInfo, String> {
    let m = sections(out);
    let hostname = first(&m, "HOSTNAME");
    if hostname.is_empty() && !m.contains_key("HOSTS") {
        return Err("读不到主机名信息".into());
    }
    let mode_line = first(&m, "MODE");
    let mode = if mode_line == "resolved" {
        DnsMode::Resolved
    } else if mode_line == "file" {
        DnsMode::File
    } else if let Some(why) = mode_line.strip_prefix("readonly ") {
        DnsMode::Readonly(why.to_string())
    } else {
        DnsMode::Readonly("读不到 DNS 配置方式".into())
    };
    let mut dns: Vec<String> = Vec::new();
    if mode == DnsMode::Resolved {
        for l in m.get("DNS").into_iter().flatten() {
            if let Some((_, list)) = l.split_once(": ") {
                for s in list.split_whitespace() {
                    let ip = s.split('#').next().unwrap_or(s);
                    if valid_ip(ip) && !dns.iter().any(|d| d == ip) {
                        dns.push(ip.to_string());
                    }
                }
            }
        }
    } else {
        for l in m.get("RESOLV").into_iter().flatten() {
            if let Some(ip) = l.strip_prefix("nameserver").map(str::trim) {
                if valid_ip(ip) && !dns.iter().any(|d| d == ip) {
                    dns.push(ip.to_string());
                }
            }
        }
    }
    let dns_otter = m
        .get("CONF")
        .into_iter()
        .flatten()
        .find_map(|l| l.trim().strip_prefix("DNS=").map(|v| v.split_whitespace().map(str::to_string).collect::<Vec<_>>()))
        .unwrap_or_default();
    Ok(HostInfo { hostname, static_name: first(&m, "STATIC"), entries: parse_hosts(&m.get("HOSTS").map(|v| v.join("\n")).unwrap_or_default()), mode, dns, dns_otter })
}

// ---- 主机名 ----

pub fn set_hostname_script(name: &str) -> Result<String, String> {
    if !valid_hostname(name) || name.chars().any(|c| c.is_ascii_uppercase()) {
        return Err("主机名只能用小写字母、数字和 -(可用 . 分隔,每段不超过 63 个字符),不能以 - 开头或结尾".into());
    }
    let n = quote(name);
    Ok(format!(
        "{PATH_FIX}set -e\nNEW={n}\nif command -v hostnamectl >/dev/null 2>&1; then hostnamectl set-hostname \"$NEW\"; else echo \"$NEW\" > /etc/hostname; hostname \"$NEW\"; fi\n\
         if grep -qE '^127\\.0\\.1\\.1[[:space:]]' /etc/hosts 2>/dev/null; then\n  cp -p /etc/hosts /etc/hosts.otter-bak\n  awk -v n=\"$NEW\" '/^127\\.0\\.1\\.1[ \\t]/{{print \"127.0.1.1\\t\" n; next}}{{print}}' /etc/hosts.otter-bak > /etc/hosts.otter-new && cat /etc/hosts.otter-new > /etc/hosts; rm -f /etc/hosts.otter-new\nfi\n\
         [ \"$(hostname)\" = \"$NEW\" ] || {{ echo \"主机名没改成功,现在是 $(hostname)\" >&2; exit 1; }}\n"
    ))
}

// ---- hosts ----

/// 拼成 hosts 的一行:`ip<TAB>名字 名字 # 备注`
pub fn hosts_line(ip: &str, names: &str, comment: &str) -> Result<String, String> {
    let ip = ip.trim();
    if !valid_ip(ip) {
        return Err(format!("「{ip}」不是有效的 IP 地址"));
    }
    let list: Vec<&str> = names.split_whitespace().collect();
    if list.is_empty() {
        return Err("至少填一个域名".into());
    }
    for n in &list {
        if !n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) || n.starts_with('-') || n.len() > 253 {
            return Err(format!("域名「{n}」不合法:只能用字母、数字和 . - _,多个域名用空格分开"));
        }
    }
    let comment = comment.trim();
    if comment.chars().any(|c| c.is_control()) {
        return Err("备注里不能有换行等控制字符".into());
    }
    Ok(if comment.is_empty() { format!("{ip}\t{}", list.join(" ")) } else { format!("{ip}\t{} # {comment}", list.join(" ")) })
}

const HOSTS_BAK: &str = "[ -f /etc/hosts ] || : > /etc/hosts\ncp -p /etc/hosts /etc/hosts.otter-bak\n";
const HOSTS_APPLY: &str = "cat /etc/hosts.otter-new > /etc/hosts; rm -f /etc/hosts.otter-new\n";

pub fn hosts_add_script(line: &str) -> String {
    format!("{PATH_FIX}set -e\n{HOSTS_BAK}NEW={}\n{{ cat /etc/hosts.otter-bak; [ -z \"$(tail -c1 /etc/hosts.otter-bak)\" ] || echo; printf '%s\\n' \"$NEW\"; }} > /etc/hosts.otter-new\n{HOSTS_APPLY}", quote(line))
}

fn hosts_check(n: usize, old: &str) -> String {
    format!("N={n}\nOLD={}\ncur=$(sed -n \"${{N}}p\" /etc/hosts)\n[ \"$cur\" = \"$OLD\" ] || {{ echo '/etc/hosts 在你打开之后被改过,请刷新后再试' >&2; exit 1; }}\n", quote(old))
}

pub fn hosts_edit_script(n: usize, old: &str, new: &str) -> String {
    format!("{PATH_FIX}set -e\n{}{HOSTS_BAK}NEW={}\nNEW=\"$NEW\" awk -v n=\"$N\" 'NR==n{{print ENVIRON[\"NEW\"]; next}}{{print}}' /etc/hosts.otter-bak > /etc/hosts.otter-new\n{HOSTS_APPLY}", hosts_check(n, old), quote(new))
}

pub fn hosts_delete_script(n: usize, old: &str) -> String {
    format!("{PATH_FIX}set -e\n{}{HOSTS_BAK}awk -v n=\"$N\" 'NR!=n' /etc/hosts.otter-bak > /etc/hosts.otter-new\n{HOSTS_APPLY}", hosts_check(n, old))
}

// ---- DNS ----

const RESOLVED_DIR: &str = "/etc/systemd/resolved.conf.d";

pub fn dns_set_script(mode: &DnsMode, servers: &[String]) -> Result<String, String> {
    if servers.is_empty() || servers.len() > 3 {
        return Err("DNS 服务器填 1 到 3 个".into());
    }
    if let Some(bad) = servers.iter().find(|s| !valid_ip(s)) {
        return Err(format!("「{bad}」不是有效的 IP 地址"));
    }
    let list = servers.join(" ");
    match mode {
        DnsMode::Resolved => Ok(format!(
            "{PATH_FIX}set -e\nD={RESOLVED_DIR}; F=$D/99-otter.conf\nmkdir -p \"$D\"\nrm -f /run/otter-dns.bak\n[ -f \"$F\" ] && cp -p \"$F\" /run/otter-dns.bak\n\
             back() {{ if [ -f /run/otter-dns.bak ]; then cp -p /run/otter-dns.bak \"$F\"; else rm -f \"$F\"; fi; systemctl restart systemd-resolved >/dev/null 2>&1 || true; }}\n\
             printf '[Resolve]\\nDNS=%s\\n' {} > \"$F\"\n\
             systemctl restart systemd-resolved || {{ back; echo 'systemd-resolved 重启失败,已还原' >&2; exit 1; }}\nsleep 1\n\
             resolvectl dns 2>/dev/null | grep -qF {} || {{ back; echo 'DNS 没有生效,已还原' >&2; exit 1; }}\n",
            quote(&list),
            quote(&servers[0])
        )),
        DnsMode::File => Ok(format!(
            "{PATH_FIX}set -e\ncp -p /etc/resolv.conf /etc/resolv.conf.otter-bak\n{{ for s in {list}; do echo \"nameserver $s\"; done; grep -v '^nameserver' /etc/resolv.conf.otter-bak || true; }} > /etc/resolv.conf.otter-new\ncat /etc/resolv.conf.otter-new > /etc/resolv.conf; rm -f /etc/resolv.conf.otter-new\ngrep -q '^nameserver' /etc/resolv.conf\n"
        )),
        DnsMode::Readonly(why) => Err(why.clone()),
    }
}

/// 去掉 Otter 设置的 DNS(只对 systemd-resolved 模式有意义)
pub fn dns_reset_script() -> String {
    format!("{PATH_FIX}set -e\nrm -f {RESOLVED_DIR}/99-otter.conf\nsystemctl restart systemd-resolved\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "@@HOSTNAME\nweb-1\n@@STATIC\nweb-1\n@@HOSTS\n127.0.0.1\tlocalhost\n# comment\n\n127.0.1.1 web-1 web  # self\n::1 ip6-localhost\n@@MODE\nresolved\n@@DNS\nGlobal: 1.1.1.1 8.8.8.8\nLink 2 (eth0): 10.0.0.2 fd00::2\nLink 3 (docker0):\n@@RESOLV\nnameserver 127.0.0.53\n@@CONF\n[Resolve]\nDNS=1.1.1.1 8.8.8.8\n";

    #[test]
    fn parse_all() {
        let h = parse_host(OUT).unwrap();
        assert_eq!(h.hostname, "web-1");
        assert_eq!(h.entries.len(), 3);
        assert_eq!((h.entries[1].line, h.entries[1].ip.as_str(), h.entries[1].comment.as_str()), (4, "127.0.1.1", "self"));
        assert_eq!(h.entries[1].names, vec!["web-1", "web"]);
        assert_eq!(h.mode, DnsMode::Resolved);
        assert_eq!(h.dns, vec!["1.1.1.1", "8.8.8.8", "10.0.0.2", "fd00::2"]);
        assert_eq!(h.dns_otter, vec!["1.1.1.1", "8.8.8.8"]);
    }

    #[test]
    fn parse_modes() {
        let h = parse_host("@@HOSTNAME\nx\n@@MODE\nreadonly /etc/resolv.conf 是符号链接\n@@RESOLV\nnameserver 9.9.9.9\nnameserver bad\n").unwrap();
        assert!(matches!(h.mode, DnsMode::Readonly(ref s) if s.contains("符号链接")));
        assert_eq!(h.dns, vec!["9.9.9.9"]);
        let f = parse_host("@@HOSTNAME\nx\n@@MODE\nfile\n@@RESOLV\nnameserver 8.8.4.4\n").unwrap();
        assert_eq!(f.mode, DnsMode::File);
        assert!(parse_host("").is_err());
    }

    #[test]
    fn hostname_rules() {
        assert!(set_hostname_script("web-01.example.com").is_ok());
        assert!(set_hostname_script("Web").is_err());
        assert!(set_hostname_script("a_b").is_err());
        assert!(set_hostname_script("-a").is_err());
        assert!(set_hostname_script("a'; rm -rf /; '").is_err());
    }

    #[test]
    fn hosts_lines() {
        assert_eq!(hosts_line("10.0.0.5", "db.local  db", "").unwrap(), "10.0.0.5\tdb.local db");
        assert_eq!(hosts_line("::1", "x", " note ").unwrap(), "::1\tx # note");
        assert!(hosts_line("nope", "x", "").is_err());
        assert!(hosts_line("1.1.1.1", "", "").is_err());
        assert!(hosts_line("1.1.1.1", "a;b", "").is_err());
        assert!(hosts_line("1.1.1.1", "a", "x\ny").is_err());
    }

    #[test]
    fn dns_scripts() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(dns_set_script(&DnsMode::Resolved, &s(&["1.1.1.1"])).unwrap().contains("99-otter.conf"));
        assert!(dns_set_script(&DnsMode::File, &s(&["1.1.1.1", "8.8.8.8"])).unwrap().contains("for s in 1.1.1.1 8.8.8.8"));
        assert!(dns_set_script(&DnsMode::File, &s(&["1.1.1.1; reboot"])).is_err());
        assert!(dns_set_script(&DnsMode::File, &[]).is_err());
        assert!(dns_set_script(&DnsMode::Readonly("x".into()), &s(&["1.1.1.1"])).is_err());
    }
}
