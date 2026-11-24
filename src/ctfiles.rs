//! 容器的日志和文件(右侧抽屉):命令输出的解析,以及 `docker cp` 用的 tar 打包/解包。
//!
//! 日志来自 `docker logs`,文件来自 `docker exec ls/cat/...` 和 `docker cp`(tar 流),
//! 容器里不需要装任何额外的东西,但要有 `ls`、`cat`、`head`、`sh` 这类基础命令(distroless 镜像没有)。

use std::path::{Path, PathBuf};

/// 抽屉里读取的数据种类,同时也是界面缓存结果的键
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CtKind {
    Logs,
    /// 堆栈的 compose 文件位置(`docker inspect` 读容器标签)
    Compose,
}

/// 单个传输的大小上限(整个文件在内存里中转)
pub const XFER_MAX: u64 = 256 * 1024 * 1024;

// ---- 日志 ----

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    /// 本地时间的 `月-日 时:分:秒`;没有时间戳是空串
    pub ts: String,
    pub level: Level,
    pub msg: String,
}

/// 去掉终端颜色之类的转义序列(`\x1b[31m`)
fn strip_ansi(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(ch) = it.next() {
        if ch == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for c in it.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn level_of(msg: &str) -> Level {
    let up = msg.to_uppercase();
    if ["ERROR", "FATAL", "PANIC", "CRIT", "EXCEPTION", "TRACEBACK"].iter().any(|k| up.contains(k)) {
        Level::Error
    } else if up.contains("WARN") {
        Level::Warn
    } else {
        Level::Info
    }
}

/// 解析 `docker logs --timestamps` 的输出
pub fn parse_logs(text: &str) -> Vec<LogLine> {
    text.lines()
        .map(|l| {
            let l = l.trim_end_matches('\r');
            let (ts, rest) = match l.split_once(' ') {
                Some((a, b)) if a.len() >= 20 && a.contains('T') => match chrono::DateTime::parse_from_rfc3339(a) {
                    Ok(d) => (d.with_timezone(&chrono::Local).format("%m-%d %H:%M:%S").to_string(), b),
                    Err(_) => (String::new(), l),
                },
                _ => (String::new(), l),
            };
            let msg = strip_ansi(rest);
            LogLine { ts, level: level_of(&msg), msg }
        })
        .collect()
}

// ---- 文件列表 ----

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EKind {
    Dir,
    File,
    Link,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub kind: EKind,
    pub size: u64,
    pub mode: String,
    pub owner: String,
    pub mtime: String,
    /// 符号链接指向的位置
    pub link: Option<String>,
}

/// 取前 n 个以空白分隔的词,返回它们和剩下的部分(保留名字里的空格)
fn take_tokens(s: &str, n: usize) -> Option<(Vec<&str>, &str)> {
    let mut toks = Vec::with_capacity(n);
    let mut rest = s.trim_start();
    for _ in 0..n {
        let end = rest.find(char::is_whitespace)?;
        toks.push(&rest[..end]);
        rest = rest[end..].trim_start();
    }
    Some((toks, rest))
}

fn is_iso_date(s: &str) -> bool {
    s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-'
}

/// 解析 `ls -la`(GNU 和 busybox 的输出都行)。不含 `.` 和 `..`。
pub fn parse_ls(text: &str) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    let mut saw_header = false;
    for line in text.lines() {
        if line.starts_with("total ") {
            saw_header = true;
            continue;
        }
        let Some((t, mut rest)) = take_tokens(line, 5) else { continue };
        let perms = t[0];
        let Some(k) = perms.chars().next() else { continue };
        if perms.len() < 10 || !"-dlcbps".contains(k) {
            continue;
        }
        // 设备文件的「大小」是 `主设备号, 次设备号`
        let mut size = t[4].parse::<u64>().unwrap_or(0);
        if t[4].ends_with(',') {
            let Some((m, r)) = take_tokens(rest, 1) else { continue };
            let _ = m;
            rest = r;
            size = 0;
        }
        let first = rest.split_whitespace().next().unwrap_or("");
        let nd = if is_iso_date(first) { 2 } else { 3 };
        let Some((d, name)) = take_tokens(rest, nd) else { continue };
        let kind = match k {
            'd' => EKind::Dir,
            '-' => EKind::File,
            'l' => EKind::Link,
            _ => EKind::Other,
        };
        let (name, link) = match (kind, name.split_once(" -> ")) {
            (EKind::Link, Some((n, l))) => (n.to_string(), Some(l.to_string())),
            _ => (name.to_string(), None),
        };
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        out.push(Entry { name, kind, size, mode: perms.to_string(), owner: format!("{}:{}", t[2], t[3]), mtime: d.join(" "), link });
    }
    if out.is_empty() && !saw_header {
        let msg = text.trim().lines().next().unwrap_or("");
        if !msg.is_empty() {
            return Err(msg.to_string());
        }
    }
    out.sort_by(|a, b| (b.kind == EKind::Dir).cmp(&(a.kind == EKind::Dir)).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

/// `ls -la` 的修改时间转 Unix 秒:`2026-11-20 08:12`,busybox 的 `Nov 20 08:12` / `Nov 20  2025`
pub fn mtime_secs(s: &str) -> Option<i64> {
    use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, TimeZone};
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let naive: Option<NaiveDateTime> = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S"))
        .ok()
        .or_else(|| {
            let year = Local::now().year();
            NaiveDateTime::parse_from_str(&format!("{year} {s}"), "%Y %b %d %H:%M").ok()
        })
        .or_else(|| NaiveDate::parse_from_str(&s, "%b %d %Y").ok().and_then(|d| d.and_hms_opt(0, 0, 0)));
    naive.and_then(|n| Local.from_local_datetime(&n).earliest()).map(|d| d.timestamp())
}

/// 容器里的路径是否可以拿来拼命令(绝对路径,没有换行)
pub fn safe_path(p: &str) -> bool {
    p.starts_with('/') && !p.contains(['\n', '\r', '\0']) && p.len() <= 4096
}

/// 文件名是否可用(不含 `/`、不能是 `.` `..`、没有换行)
pub fn safe_filename(n: &str) -> bool {
    !n.is_empty() && n != "." && n != ".." && !n.contains(['/', '\n', '\r', '\0']) && n.len() <= 255
}

// ---- tar ----

/// 把本地文件/目录打成 tar(`docker cp - 容器:目录` 的输入)。属主统一成 root,不带本机的 uid。
pub fn build_tar(paths: &[PathBuf]) -> Result<Vec<u8>, String> {
    let mut b = tar::Builder::new(Vec::new());
    b.mode(tar::HeaderMode::Deterministic);
    b.follow_symlinks(false);
    let mut total = 0u64;
    for p in paths {
        let name = p.file_name().ok_or_else(|| format!("路径不对:{}", p.display()))?;
        let md = std::fs::metadata(p).map_err(|e| format!("读不了 {}:{e}", p.display()))?;
        if md.is_dir() {
            b.append_dir_all(name, p).map_err(|e| format!("打包 {} 失败:{e}", p.display()))?;
        } else {
            total += md.len();
            if total > XFER_MAX {
                return Err(format!("文件太大(上限 {} MB)", XFER_MAX / 1024 / 1024));
            }
            b.append_path_with_name(p, name).map_err(|e| format!("打包 {} 失败:{e}", p.display()))?;
        }
    }
    let data = b.into_inner().map_err(|e| e.to_string())?;
    if data.len() as u64 > XFER_MAX + 1024 * 1024 {
        return Err(format!("内容太大(上限 {} MB)", XFER_MAX / 1024 / 1024));
    }
    Ok(data)
}

/// 从 `docker cp 容器:文件 -` 的 tar 流里取出这个文件,存到 `dest`
pub fn extract_file(data: &[u8], dest: &Path) -> Result<(), String> {
    let mut a = tar::Archive::new(data);
    for e in a.entries().map_err(|e| e.to_string())? {
        let mut e = e.map_err(|e| e.to_string())?;
        if e.header().entry_type().is_file() {
            let mut f = std::fs::File::create(dest).map_err(|e| format!("写不了 {}:{e}", dest.display()))?;
            std::io::copy(&mut e, &mut f).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    Err("没有读到文件内容".into())
}

/// 把目录的 tar 流解到 `parent` 下(里面第一层就是目录名)
pub fn extract_dir(data: &[u8], parent: &Path) -> Result<(), String> {
    tar::Archive::new(data).unpack(parent).map_err(|e| format!("解包失败:{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gnu_ls() {
        let gnu = "total 12\ndrwxr-xr-x 1 root root 4096 2026-11-20 10:01 etc\n-rw-r--r-- 1 root root  220 2026-11-20 10:01 my file.txt\nlrwxrwxrwx 1 root root    7 2026-11-20 10:01 bin -> usr/bin\ncrw-rw-rw- 1 root root 1, 3 2026-11-20 10:01 null\n";
        let e = parse_ls(gnu).unwrap();
        let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
        // 目录在前,其余按名字
        assert_eq!(names, ["etc", "bin", "my file.txt", "null"]);
        assert_eq!(e[2].size, 220);
        assert_eq!(e[1].link.as_deref(), Some("usr/bin"));
        assert_eq!(e[3].kind, EKind::Other);
        assert_eq!(e[0].mtime, "2026-11-20 10:01");
    }

    #[test]
    fn parses_busybox_ls_and_errors() {
        let bb = "total 8\ndrwxr-xr-x    2 root     root          4096 Nov 24 10:00 .\ndrwxr-xr-x    1 root     root          4096 Nov 24 10:00 ..\n-rw-r--r--    1 app      app             12 Nov 24 10:00 a b.conf\ndrwxr-xr-x    2 app      app           4096 Nov  1  2025 data\n";
        let e = parse_ls(bb).unwrap();
        assert_eq!(e.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["data", "a b.conf"]);
        assert_eq!(e[0].mtime, "Nov 1 2025");
        assert_eq!(e[1].owner, "app:app");
        assert!(parse_ls("ls: /nope: No such file or directory\n").is_err());
        // 空目录:只有 total
        assert!(parse_ls("total 0\n").unwrap().is_empty());
    }

    #[test]
    fn parses_logs_with_levels_and_ansi() {
        let t = "2026-11-24T10:00:01.123456789Z server started\n2026-11-24T10:00:02.5Z \x1b[33mWARN\x1b[0m slow request\nno timestamp ERROR boom\n";
        let l = parse_logs(t);
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].level, Level::Info);
        assert!(!l[0].ts.is_empty() && l[0].msg == "server started");
        assert_eq!((l[1].level, l[1].msg.as_str()), (Level::Warn, "WARN slow request"));
        assert_eq!((l[2].ts.as_str(), l[2].level), ("", Level::Error));
    }

    #[test]
    fn mtime_formats() {
        assert!(mtime_secs("2026-11-20 08:12").is_some());
        assert!(mtime_secs("Nov 20 08:12").is_some());
        assert!(mtime_secs("Nov 20 2025").is_some());
        assert!(mtime_secs("garbage").is_none());
    }

    #[test]
    fn path_helpers() {
        assert!(safe_path("/a b/c") && !safe_path("a") && !safe_path("/a\nb"));
        assert!(safe_filename("a b.txt") && !safe_filename("a/b") && !safe_filename("..") && !safe_filename(""));
    }

    #[test]
    fn tar_roundtrip() {
        let dir = std::env::temp_dir().join(format!("otter-tar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/sub")).unwrap();
        std::fs::write(dir.join("src/sub/a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("one.bin"), [0u8, 1, 2, 255]).unwrap();
        // 单个文件
        let data = build_tar(&[dir.join("one.bin")]).unwrap();
        extract_file(&data, &dir.join("out.bin")).unwrap();
        assert_eq!(std::fs::read(dir.join("out.bin")).unwrap(), [0u8, 1, 2, 255]);
        // 目录
        let data = build_tar(&[dir.join("src")]).unwrap();
        std::fs::create_dir_all(dir.join("dst")).unwrap();
        extract_dir(&data, &dir.join("dst")).unwrap();
        assert_eq!(std::fs::read(dir.join("dst/src/sub/a.txt")).unwrap(), b"hello");
        assert!(build_tar(&[dir.join("missing")]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
