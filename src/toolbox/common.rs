//! 新增几个工具(主机名与 hosts/DNS、内核参数、SSH、用户、fail2ban)共用的小函数。

use std::collections::HashMap;
use std::net::IpAddr;

/// 按 `@@名字` 行把输出切成段;`@@名字 其余内容` 时其余内容算该段第一行
pub fn sections(out: &str) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    let mut cur: Option<String> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            let (name, tail) = match rest.split_once(char::is_whitespace) {
                Some((n, t)) => (n.to_string(), Some(t.to_string())),
                None => (rest.trim().to_string(), None),
            };
            let e = m.entry(name.clone()).or_default();
            if let Some(t) = tail {
                e.push(t);
            }
            cur = Some(name);
        } else if let Some(c) = &cur {
            m.entry(c.clone()).or_default().push(line.to_string());
        }
    }
    m
}

/// 段的第一行(去首尾空白);没有这一段是空串
pub fn first(m: &HashMap<String, Vec<String>>, name: &str) -> String {
    m.get(name).and_then(|v| v.first()).map(|s| s.trim().to_string()).unwrap_or_default()
}

pub fn valid_ip(s: &str) -> bool {
    s.parse::<IpAddr>().is_ok()
}

/// IP 或带掩码的网段(`10.0.0.0/8`)
pub fn valid_ip_or_cidr(s: &str) -> bool {
    match s.split_once('/') {
        None => valid_ip(s),
        Some((ip, bits)) => match (ip.parse::<IpAddr>(), bits.parse::<u8>()) {
            (Ok(IpAddr::V4(_)), Ok(b)) => b <= 32,
            (Ok(IpAddr::V6(_)), Ok(b)) => b <= 128,
            _ => false,
        },
    }
}

/// 一个主机名标签或完整域名(RFC 1123):字母数字和 `-`,每段 1-63,总长 ≤ 253,不以 `-` 开头结尾
pub fn valid_hostname(s: &str) -> bool {
    !s.is_empty() && s.len() <= 253 && s.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// 本机用户名:小写字母或下划线开头,后面小写字母、数字、`_-`,最长 32
pub fn valid_user(s: &str) -> bool {
    let mut cs = s.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_lowercase() || c == '_') && s.len() <= 32 && cs.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_sections() {
        let m = sections("junk\n@@A\nx\ny\n@@B hello\n@@C\n");
        assert_eq!(m["A"], vec!["x", "y"]);
        assert_eq!(m["B"], vec!["hello"]);
        assert!(m["C"].is_empty());
        assert_eq!(first(&m, "A"), "x");
        assert_eq!(first(&m, "Z"), "");
    }

    #[test]
    fn validators() {
        assert!(valid_ip("10.0.0.1") && valid_ip("::1") && !valid_ip("10.0.0") && !valid_ip("a b"));
        assert!(valid_ip_or_cidr("10.0.0.0/8") && valid_ip_or_cidr("fd00::/64") && !valid_ip_or_cidr("10.0.0.0/33") && !valid_ip_or_cidr("x/8"));
        assert!(valid_hostname("web-01") && valid_hostname("a.b.example.com") && !valid_hostname("-a") && !valid_hostname("a_b") && !valid_hostname("a..b") && !valid_hostname(""));
        assert!(valid_user("deploy") && valid_user("_svc-1") && !valid_user("Root") && !valid_user("1abc") && !valid_user("a b") && !valid_user(&"a".repeat(33)));
    }
}

/// 把 apt / dnf / yum 的常见安装失败翻成能直接照着处理的话;认不出来返回 None(界面照旧显示原始最后一行)。
/// 输入是命令的完整输出(stderr 在前)。
pub fn pkg_error_hint(out: &str) -> Option<&'static str> {
    let has = |s: &str| out.contains(s);
    if has("Could not get lock") || has("dpkg frontend lock") || has("Waiting for cache lock") || has("Another app is currently holding the yum lock") || has("Waiting for process with pid") {
        return Some("服务器上有别的包管理进程在运行(常见于开机后的自动更新),等几分钟再试");
    }
    if has("No space left on device") {
        return Some("服务器磁盘空间不够,清理后再试");
    }
    if has("NO_PUBKEY") || has("GPG error") || has("signatures couldn't be verified") || has("GPG check FAILED") || has("public key is not installed") {
        return Some("软件源的签名校验没通过,多半是源的密钥过期或没导入,先修好软件源再试");
    }
    if has("Cannot find a valid baseurl") || has("Cannot prepare internal mirrorlist") || has("Failed to download metadata for repo") {
        return Some("软件源已经失效(CentOS 7/8 等停止维护的系统,官方镜像已下架),需要先把软件源换成可用的镜像,再重试");
    }
    if has("404  Not Found") || has("404 Not Found") || has("Unable to fetch some archives") {
        return Some("软件源上的包已经下架或索引过期,多半是系统版本已结束支持(如 Debian 11、Ubuntu 18.04);先 apt-get update,仍不行就把软件源换成归档源或升级系统");
    }
    if has("Could not resolve host") || has("Temporary failure resolving") || has("Could not resolve '") || has("Name or service not known") || has("Temporary failure in name resolution") {
        return Some("服务器解析不了软件源的域名,检查服务器的 DNS(工具箱「主机名与 DNS」)和网络");
    }
    if has("Connection timed out") || has("Network is unreachable") || has("Unable to connect to") || has("Could not connect to") || has("Connection refused") || has("Operation timed out") {
        return Some("服务器连不上软件源,检查服务器能不能访问外网(防火墙、代理、安全组)");
    }
    if has("Unable to locate package") || has("No match for argument") || has("Unable to find a match") || has("has no installation candidate") {
        return Some("软件源里没有这个包:先更新软件源;RHEL 系可能要先启用 EPEL 等额外仓库");
    }
    None
}

#[cfg(test)]
mod pkg_hint_tests {
    use super::pkg_error_hint;

    #[test]
    fn hints() {
        let apt404 = "E: Failed to fetch http://deb.debian.org/debian-security/pool/x.deb  404  Not Found [IP: 1.2.3.4 80]\nE: Unable to fetch some archives, maybe run apt-get update or try with --fix-missing?";
        assert!(pkg_error_hint(apt404).unwrap().contains("结束支持"));
        assert!(pkg_error_hint("E: Could not get lock /var/lib/dpkg/lock-frontend. It is held by process 123").unwrap().contains("别的包管理"));
        assert!(pkg_error_hint("Errors during downloading metadata for repository 'base':\nError: Cannot find a valid baseurl for repo: base").unwrap().contains("已经失效"));
        assert!(pkg_error_hint("Err:1 http://x InRelease\n  Temporary failure resolving 'deb.debian.org'").unwrap().contains("DNS"));
        assert!(pkg_error_hint("E: Unable to locate package fail2ban").unwrap().contains("没有这个包"));
        assert!(pkg_error_hint("sshd 配置校验失败,已还原").is_none());
    }
}
