//! 应用商店「可升级」判断(纯函数):
//! - 实例用的是具体版本(`8.4.13`、`18.7`):在版本列表里找同一条线上更新的 tag(小版本:`8.4.x`、`18.x`),不跨大版本;
//! - 实例用的是浮动标签(`alpine`、`latest`、`8.4` 这类会随新版本移动的):比较本机镜像的 digest 和 Docker Hub 上该 tag 现在指向的 digest,不同就是有新镜像。
//! 设计见 docs/plans/active/2026-11-26-store-tabs-upgrade.md。

use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// 已是最新
    Latest,
    /// 同一条线上有更新的具体版本
    NewTag(String),
    /// 浮动标签,需要比较镜像 digest(`Check::Digest`)
    NeedDigest,
    /// 浮动标签,镜像 digest 不同:有新镜像,升级 = 重新拉取同一个 tag
    NewImage,
    /// 判断不了(版本不是数字、列表里没有等)
    Unknown,
}

/// `8.4.13` / `1.27.1.2-alpine` → (数字段, 后缀)。必须以数字开头
fn split(tag: &str) -> Option<(Vec<u64>, String)> {
    let end = tag.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(tag.len());
    let (num, suffix) = tag.split_at(end);
    let parts: Option<Vec<u64>> = num.trim_end_matches('.').split('.').map(|p| p.parse().ok()).collect();
    let parts = parts.filter(|p| !p.is_empty())?;
    Some((parts, suffix.to_string()))
}

/// 这个 tag 是不是浮动标签:不是数字开头,或版本列表里还有别的 tag 以它加「.」开头(`8.4` 之于 `8.4.13`)
pub fn is_floating(tag: &str, list: &[String]) -> bool {
    if split(tag).is_none() {
        return true;
    }
    let (parts, suffix) = split(tag).unwrap_or_default();
    let dot = format!("{}.", tag.strip_suffix(suffix.as_str()).unwrap_or(tag));
    list.iter().any(|t| t != tag && t.starts_with(&dot) && split(t).is_some_and(|(p, s)| s == suffix && p.len() > parts.len()))
}

/// 版本判断:`installed` 是实例镜像的 tag,`list` 是 Docker Hub 的版本列表
pub fn check_tag(installed: &str, list: &[String]) -> Status {
    if is_floating(installed, list) {
        return Status::NeedDigest;
    }
    let Some((parts, suffix)) = split(installed) else { return Status::Unknown };
    if parts.len() < 2 {
        return Status::Latest;
    }
    // 同一条线 = 前两段相同、只有后面的段变(8.4.13 → 8.4.x;1.27.1.2 → 1.27.x.y);两段版本只看第一段(18.7 → 18.x);后缀也要相同
    let n = 2.min(parts.len() - 1);
    let line = &parts[..n];
    let best = list
        .iter()
        .filter_map(|t| split(t).map(|(p, s)| (p, s, t)))
        .filter(|(p, s, _)| *s == suffix && p.len() == parts.len() && p[..n] == *line && *p > parts)
        .max_by(|a, b| a.0.cmp(&b.0));
    match best {
        Some((_, _, t)) => Status::NewTag(t.clone()),
        None => Status::Latest,
    }
}

/// 检查更新时只需要同一条线上的 tag:返回查询用的前缀(`8.12.0` → `8.12.`,`18.7` → `18.`),
/// 浮动标签(不以数字开头)不需要版本列表,返回 None
pub fn line_prefix(tag: &str) -> Option<String> {
    let (parts, _) = split(tag)?;
    let n = if parts.len() >= 3 { 2 } else { 1 };
    Some(parts[..n].iter().map(|p| format!("{p}.")).collect())
}

/// 本机请求这条线上的 tag 列表(只取 2 页并行,比完整版本列表小得多);失败返回 None
pub fn fetch_line_tags(repo: &str, prefix: &str) -> Option<Vec<String>> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(15))).user_agent("otter").build().into();
    let res: Vec<Option<String>> = std::thread::scope(|sc| {
        let hs: Vec<_> = (1..=2)
            .map(|p| {
                let (agent, url) = (&agent, crate::appstore::hub_url(repo, Some(prefix), p));
                sc.spawn(move || agent.get(&url).call().ok().and_then(|mut r| r.body_mut().read_to_string().ok()))
            })
            .collect();
        hs.into_iter().map(|h| h.join().ok().flatten()).collect()
    });
    let raw: Vec<String> = res.into_iter().flatten().collect();
    (!raw.is_empty()).then(|| crate::appstore::parse_tag_names(&raw.join("\n")))
}

/// Docker Hub 上某个 tag 的地址
pub fn hub_tag_url(repo: &str, tag: &str) -> String {
    format!("https://hub.docker.com/v2/repositories/{repo}/tags/{tag}")
}

/// Hub 返回里该 tag 现在指向的 digest(清单列表的和各架构的都收,本机拉到哪个都能对上)
pub fn parse_hub_digests(json: &str) -> Option<HashSet<String>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let mut set = HashSet::new();
    if let Some(d) = v.get("digest").and_then(|d| d.as_str()) {
        set.insert(d.to_string());
    }
    for i in v.get("images").and_then(|i| i.as_array()).into_iter().flatten() {
        if let Some(d) = i.get("digest").and_then(|d| d.as_str()) {
            set.insert(d.to_string());
        }
    }
    (!set.is_empty()).then_some(set)
}

/// 本机请求 Hub 上某 tag 的 digest(阻塞,放后台线程调用);失败返回 None
pub fn fetch_hub_digests(repo: &str, tag: &str) -> Option<HashSet<String>> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(15))).user_agent("otter").build().into();
    let mut resp = agent.get(&hub_tag_url(repo, tag)).call().ok()?;
    parse_hub_digests(&resp.body_mut().read_to_string().ok()?)
}

/// 服务器上取本机镜像 digest 的脚本:每行一个 `sha256:…`(`RepoDigests` 里 `@` 后面的部分)
pub fn local_digest_script(image: &str) -> String {
    format!("docker image inspect --format '{{{{range .RepoDigests}}}}{{{{println .}}}}{{{{end}}}}' {} 2>/dev/null | sed 's/.*@//' | grep '^sha256:'; true", crate::sysops::quote(image))
}

/// 本机 digest 和 Hub 的比较:本机任何一个出现在 Hub 里就是最新;本机没有 digest(不是从仓库拉的)判断不了
pub fn check_digest(local: &str, hub: &HashSet<String>) -> Status {
    let l: Vec<&str> = local.lines().map(str::trim).filter(|x| x.starts_with("sha256:")).collect();
    if l.is_empty() {
        Status::Unknown
    } else if l.iter().any(|d| hub.contains(*d)) {
        Status::Latest
    } else {
        Status::NewImage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn floating_detection() {
        let list = l(&["8.4.13", "8.4.11", "8.4", "8", "9.7.4", "18.7", "18", "alpine"]);
        assert!(is_floating("8.4", &list) && is_floating("8", &list) && is_floating("alpine", &list) && is_floating("latest", &list));
        assert!(!is_floating("8.4.13", &list) && !is_floating("18.7", &list));
    }

    #[test]
    fn minor_line_upgrade() {
        let list = l(&["9.7.4", "8.4.14", "8.4.13", "8.4.9", "8.4", "8.0.40", "8"]);
        assert_eq!(check_tag("8.4.13", &list), Status::NewTag("8.4.14".into()));
        assert_eq!(check_tag("8.4.14", &list), Status::Latest);
        // 不跨 8.4 → 8.5 / 9.x
        assert_eq!(check_tag("8.0.40", &list), Status::Latest);
        // PostgreSQL 两段版本:18.7 → 18.9,不到 19
        let pg = l(&["19.1", "18.9", "18.8", "18.7", "18", "17.6"]);
        assert_eq!(check_tag("18.7", &pg), Status::NewTag("18.9".into()));
        assert_eq!(check_tag("17.6", &pg), Status::Latest);
        // 后缀要相同
        let ng = l(&["1.27.2.1-alpine", "1.27.1.2-alpine", "1.27.2.1"]);
        assert_eq!(check_tag("1.27.1.2-alpine", &ng), Status::NewTag("1.27.2.1-alpine".into()));
    }

    #[test]
    fn digests() {
        let hub = parse_hub_digests(r#"{"digest":"sha256:aaa","images":[{"digest":"sha256:bbb"},{"digest":"sha256:ccc"}]}"#).unwrap();
        assert_eq!(check_digest("sha256:aaa\n", &hub), Status::Latest);
        assert_eq!(check_digest("sha256:ccc\nsha256:zzz\n", &hub), Status::Latest);
        assert_eq!(check_digest("sha256:old\n", &hub), Status::NewImage);
        assert_eq!(check_digest("", &hub), Status::Unknown);
        assert!(parse_hub_digests("{}").is_none() && parse_hub_digests("x").is_none());
        let s = local_digest_script("redis:8.4");
        assert!(s.contains("RepoDigests") && s.contains("'redis:8.4'"));
    }
}
