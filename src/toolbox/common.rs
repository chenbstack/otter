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
