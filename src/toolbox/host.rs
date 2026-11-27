//! 主机名、/etc/hosts、DNS 服务器。
//!
//! - 主机名:`hostnamectl set-hostname`,并同步 /etc/hosts 里的 `127.0.1.1` 行(Debian/Ubuntu 惯例)。
//! - hosts:整个文件的每一条记录都能改(按行号 + 原文核对,文件在你打开之后被改过就拒绝)。
//! - DNS:systemd-resolved 在跑时写 drop-in `99-otter.conf`;resolv.conf 是普通文件时直接改 nameserver 行;
//!   NetworkManager 在跑(且没有 systemd-resolved)时改默认路由所在连接的 ipv4.dns / ipv6.dns 并 reapply;
//!   符号链接或其它网络管理器生成时只读(改了会被覆盖)。

use super::common::{first, sections, valid_hostname, valid_ip};
use super::PATH_FIX;
use crate::sysops::quote;

pub const HOST_SCRIPT: &str = r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH
echo '@@HOSTNAME'; hostname 2>/dev/null
echo '@@STATIC'; { hostnamectl --static 2>/dev/null || cat /etc/hostname 2>/dev/null; } | head -1
echo '@@HOSTS'; cat /etc/hosts 2>/dev/null
echo '@@MODE'
NMU=
if systemctl is-active systemd-resolved >/dev/null 2>&1; then echo resolved
elif systemctl is-active NetworkManager >/dev/null 2>&1 && command -v nmcli >/dev/null 2>&1; then
  NMDEV=$(ip route show default 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="dev"){print $(i+1);exit}}')
  NMU=$(nmcli -g GENERAL.CON-UUID device show "$NMDEV" 2>/dev/null)
  if [ -n "$NMU" ]; then echo "nm $NMU $(nmcli -g GENERAL.CONNECTION device show "$NMDEV" 2>/dev/null)"; else echo "readonly NetworkManager 管理着 DNS,但没找到默认路由所在的连接"; fi
elif [ -L /etc/resolv.conf ]; then echo "readonly /etc/resolv.conf 是符号链接(指向 $(readlink -f /etc/resolv.conf)),由其它服务管理"
elif grep -qiE 'NetworkManager|resolvconf|systemd-resolved|dhclient|netconfig' /etc/resolv.conf 2>/dev/null; then echo "readonly /etc/resolv.conf 由网络管理器自动生成,直接改会被覆盖"
else echo file; fi
echo '@@DNS'; resolvectl dns 2>/dev/null
echo '@@RESOLV'; grep '^nameserver' /etc/resolv.conf 2>/dev/null
echo '@@NMDNS'; [ -n "$NMU" ] && nmcli --escape no -g ipv4.dns,ipv6.dns con show "$NMU" 2>/dev/null
echo '@@CONF'; cat /etc/systemd/resolved.conf.d/99-otter.conf 2>/dev/null
true
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DnsMode {
    /// systemd-resolved 在跑:写 drop-in
    Resolved,
    /// resolv.conf 是普通文件:直接改
    File,
    /// NetworkManager 管理:改默认路由所在连接(UUID、显示名)的 DNS
    NetworkManager { uuid: String, name: String },
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
    } else if let Some(rest) = mode_line.strip_prefix("nm ") {
        let (uuid, name) = rest.split_once(' ').unwrap_or((rest, ""));
        DnsMode::NetworkManager { uuid: uuid.to_string(), name: name.to_string() }
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
    let dns_otter = if matches!(mode, DnsMode::NetworkManager { .. }) {
        // 连接上手动设置的 DNS(nmcli -g 多字段时每个字段一行,地址之间用逗号)
        m.get("NMDNS").into_iter().flatten().flat_map(|l| l.split(|c: char| c == ',' || c.is_whitespace())).filter(|s| valid_ip(s)).map(str::to_string).collect()
    } else {
        m.get("CONF").into_iter().flatten().find_map(|l| l.trim().strip_prefix("DNS=").map(|v| v.split_whitespace().map(str::to_string).collect::<Vec<_>>())).unwrap_or_default()
    };
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
        DnsMode::NetworkManager { uuid, .. } => {
            let (v4, v6): (Vec<&str>, Vec<&str>) = servers.iter().map(String::as_str).partition(|s| !s.contains(':'));
            Ok(nm_script(uuid, &v4.join(" "), &v6.join(" "), servers[0].as_str()))
        }
        DnsMode::Readonly(why) => Err(why.clone()),
    }
}

/// NetworkManager:改连接的 ipv4.dns / ipv6.dns(不动 ignore-auto-dns,和 DHCP 下发的并存),`reapply` 不断网;
/// 读回 resolv.conf 确认第一个地址出现,没生效就还原。`first` 为空表示清空(恢复默认),不校验。
fn nm_script(uuid: &str, v4: &str, v6: &str, first: &str) -> String {
    let verify = if first.is_empty() {
        String::new()
    } else {
        format!("ok=0; for i in 1 2 3 4 5 6; do grep -qF {} /etc/resolv.conf && ok=1 && break; sleep 1; done\n[ $ok = 1 ] || {{ back; echo 'DNS 没有生效,已还原' >&2; exit 1; }}\n", quote(first))
    };
    format!(
        "{PATH_FIX}set -e\nU={u}\nold4=$(nmcli --escape no -g ipv4.dns con show \"$U\"); old6=$(nmcli --escape no -g ipv6.dns con show \"$U\")\n\
         dev=$(nmcli -g GENERAL.DEVICES con show \"$U\" | head -1)\n\
         apply() {{ if [ -n \"$dev\" ]; then nmcli device reapply \"$dev\" >/dev/null 2>&1 || nmcli con up \"$U\" >/dev/null 2>&1 || true; fi; }}\n\
         back() {{ nmcli con mod \"$U\" ipv4.dns \"$(echo $old4 | tr , ' ')\" ipv6.dns \"$(echo $old6 | tr , ' ')\" >/dev/null 2>&1 || true; apply; }}\n\
         nmcli con mod \"$U\" ipv4.dns {v4q}\n\
         if [ -n {v6q} ] || [ -n \"$old6\" ]; then nmcli con mod \"$U\" ipv6.dns {v6q} || {{ back; echo 'IPv6 DNS 设置失败,已还原' >&2; exit 1; }}; fi\n\
         apply\n{verify}",
        u = quote(uuid),
        v4q = quote(v4),
        v6q = quote(v6),
    )
}

/// 去掉 Otter 设置的 DNS(systemd-resolved:删 drop-in;NetworkManager:清空连接上的手动 DNS)
pub fn dns_reset_script(mode: &DnsMode) -> String {
    match mode {
        DnsMode::NetworkManager { uuid, .. } => nm_script(uuid, "", "", ""),
        _ => format!("{PATH_FIX}set -e\nrm -f {RESOLVED_DIR}/99-otter.conf\nsystemctl restart systemd-resolved\n"),
    }
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
    fn parse_nm() {
        let h = parse_host("@@HOSTNAME\nx\n@@MODE\nnm 1b2c-uuid Wired connection 1\n@@RESOLV\nnameserver 10.0.0.2\n@@NMDNS\n9.9.9.9,149.112.112.112\n2001:4860:4860::8888\n").unwrap();
        assert_eq!(h.mode, DnsMode::NetworkManager { uuid: "1b2c-uuid".into(), name: "Wired connection 1".into() });
        assert_eq!(h.dns, vec!["10.0.0.2"]);
        assert_eq!(h.dns_otter, vec!["9.9.9.9", "149.112.112.112", "2001:4860:4860::8888"]);
    }

    #[test]
    fn nm_scripts() {
        let m = DnsMode::NetworkManager { uuid: "u-1".into(), name: "eth".into() };
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let sc = dns_set_script(&m, &s(&["1.1.1.1", "2606:4700::1111"])).unwrap();
        assert!(sc.contains("ipv4.dns '1.1.1.1'") && sc.contains("ipv6.dns '2606:4700::1111'"), "{sc}");
        assert!(dns_set_script(&m, &s(&["1.1.1.1; reboot"])).is_err());
        let r = dns_reset_script(&m);
        assert!(r.contains("ipv4.dns ''") && !r.contains("grep -qF"), "{r}");
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
