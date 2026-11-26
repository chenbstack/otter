//! 服务器端数据目录 `/opt/otter/`:路径常量、初始化脚本、命名规则和几个脚本小零件。
//! 目录表、原则见 `docs/modules/srvdata.md`、决策 0012。**所有功能不得自己拼 `/opt/otter/...` 路径,一律从这里取。**

use crate::sysops::quote;

/// 数据目录根
pub const ROOT: &str = "/opt/otter";
/// 布局版本(`otter.json` 里的 `layout`)。服务器上的比这个大,只读不改
pub const LAYOUT: u32 = 1;
pub const WEB: &str = "/opt/otter/websites";
pub const DB: &str = "/opt/otter/databases";
pub const JOBS: &str = "/opt/otter/jobs";
pub const TMP: &str = "/opt/otter/tmp";
pub const LOCK: &str = "/opt/otter/.lock";
/// 所有 compose 文件的统一位置:`docker/compose/<名>/`
pub const COMPOSE: &str = "/opt/otter/docker/compose";

pub fn compose_dir(name: &str) -> String {
    format!("{COMPOSE}/{name}")
}

/// 名字(账号 id、证书名、站点名)只允许 `[a-z0-9._-]`,不以 `.`、`-` 开头,最长 64
pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && !s.starts_with(['.', '-']) && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// 域名变成名字:转小写,`*` 写成 `_wildcard_`,其余不合规的字符换成 `-`
pub fn name_of_domain(d: &str) -> String {
    let d = d.trim().to_lowercase().replace('*', "_wildcard_");
    let s: String = d.chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
    s.trim_start_matches(['.', '-']).chars().take(64).collect()
}

/// 初始化脚本(要 root,可重复执行):建目录、设权限、没有 `otter.json` 就写一个
pub fn init_script() -> String {
    format!(
        "set -e; mkdir -p {r}/docker/compose {r}/jobs {r}/tmp {w}/conf/conf.d {w}/conf/backup {w}/sites {w}/acme-webroot {w}/certs {w}/accounts {w}/secrets/acme {w}/secrets/dns {w}/bin {w}/logs; \
         chmod 755 {r} {w}; chmod 700 {w}/secrets; \
         [ -f {r}/otter.json ] || printf '{{\"layout\":{l},\"created\":\"%s\",\"by\":\"otter\"}}\\n' \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\" > {r}/otter.json",
        r = ROOT,
        w = WEB,
        l = LAYOUT
    )
}

/// 多步操作的开头:拿 `.lock` 上的锁(最多等 30 秒);没有 `flock` 就跳过。拿不到输出 `ERR:…` 并退出
pub fn lock_prelude() -> String {
    format!("mkdir -p {ROOT}; exec 9>{LOCK} 2>/dev/null; if command -v flock >/dev/null 2>&1; then flock -w 30 9 || {{ echo 'ERR:另一个操作正在进行,请稍后再试'; exit 0; }}; fi")
}

/// 把一段文本写到服务器上的路径(原子:先写同目录临时文件再 `mv`),内容用 base64 带进脚本,免去转义
pub fn write_file_cmd(path: &str, content: &str, mode: &str) -> String {
    let q = quote(path);
    format!("printf %s {} | base64 -d > {q}.tmp && chmod {mode} {q}.tmp && mv {q}.tmp {q}", quote(&b64(content.as_bytes())))
}

/// 标准 base64(带 `=` 补位)
pub fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for ch in data.chunks(3) {
        let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if ch.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if ch.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// 读 `otter.json` 里的布局版本
pub fn parse_layout(text: &str) -> Option<u32> {
    serde_json::from_str::<serde_json::Value>(text.trim()).ok()?.get("layout")?.as_u64().map(|v| v as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_validated_and_derived() {
        assert!(valid_name("a.com") && valid_name("my-site_1") && valid_name("_wildcard_.a.com"));
        assert!(!valid_name("") && !valid_name("../x") && !valid_name("-x") && !valid_name(".x") && !valid_name("A.com") && !valid_name("a b") && !valid_name("a/b"));
        assert_eq!(name_of_domain("*.Example.com"), "_wildcard_.example.com");
        assert_eq!(name_of_domain(" a b.com"), "a-b.com");
        assert!(valid_name(&name_of_domain("*.a.com")));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64("你好\n".as_bytes()), "5L2g5aW9Cg==");
    }

    #[test]
    fn layout_is_read() {
        assert_eq!(parse_layout(r#"{"layout":1,"by":"otter"}"#), Some(1));
        assert_eq!(parse_layout("x"), None);
    }

    #[test]
    fn init_script_is_valid_shell_and_idempotent() {
        let d = std::env::temp_dir().join(format!("otter-srvdata-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let s = init_script().replace(ROOT, d.to_str().unwrap());
        for _ in 0..2 {
            let o = std::process::Command::new("sh").arg("-c").arg(&s).output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        }
        assert!(d.join("websites/conf/conf.d").is_dir() && d.join("websites/secrets/dns").is_dir() && d.join("docker/compose").is_dir());
        assert_eq!(parse_layout(&std::fs::read_to_string(d.join("otter.json")).unwrap()), Some(1));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(d.join("websites/secrets")).unwrap().permissions().mode() & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn write_file_roundtrips_any_content() {
        let d = std::env::temp_dir().join(format!("otter-wf-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("it's a file");
        let text = "line1\n'quoted' \"x\" $HOME `id`\n中文\n";
        let o = std::process::Command::new("sh").arg("-c").arg(write_file_cmd(p.to_str().unwrap(), text, "600")).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), text);
        let _ = std::fs::remove_dir_all(&d);
    }
}
