//! 已知主机:读写终端首次确认时记下的 known_hosts 文件(OpenSSH 格式)。

use russh::keys::{HashAlg, PublicKey};

use crate::terminal::ssh::known_hosts_file;

pub struct KnownHost {
    /// 展示用:`host` 或 `host:port`;被哈希的主机显示为「已哈希」
    pub host: String,
    pub algo: String,
    pub fingerprint: String,
    /// 文件里的整行,删除时按它匹配
    line: String,
}

pub fn load() -> Vec<KnownHost> {
    let Ok(text) = std::fs::read_to_string(known_hosts_file()) else { return Vec::new() };
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<KnownHost> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let mut it = t.split_whitespace();
    let pat = it.next()?;
    let algo = it.next()?;
    let b64 = it.next()?;
    let fingerprint = PublicKey::from_openssh(&format!("{algo} {b64}")).map(|k| k.fingerprint(HashAlg::Sha256).to_string()).unwrap_or_else(|_| "无法解析".into());
    let host = if pat.starts_with("|1|") {
        "已哈希".to_string()
    } else if let Some(rest) = pat.strip_prefix('[') {
        match rest.split_once("]:") {
            Some((h, p)) => format!("{h}:{p}"),
            None => pat.to_string(),
        }
    } else {
        pat.to_string()
    };
    Some(KnownHost { host, algo: algo.to_string(), fingerprint, line: line.to_string() })
}

/// 删除一条记录(整行匹配)。成功返回 true。
pub fn remove(entry: &KnownHost) -> bool {
    let path = known_hosts_file();
    let Ok(text) = std::fs::read_to_string(&path) else { return false };
    let mut removed = false;
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| {
            if !removed && *l == entry.line {
                removed = true;
                false
            } else {
                true
            }
        })
        .collect();
    if !removed {
        return false;
    }
    let mut out = kept.join("\n");
    out.push('\n');
    std::fs::write(&path, out).is_ok()
}
