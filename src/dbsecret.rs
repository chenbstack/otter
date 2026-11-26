//! 数据库用户密码的服务器端保存(用户在「新建数据库」「新建用户」里打开「保存密码」时才写)。
//! 位置 `/opt/otter/databases/secrets/<容器名>/<用户>[@主机].json`,目录 0700、文件 0600,只有 root 能读;
//! 一个用户一个文件(MySQL 的用户是 用户@主机),所以库的所有者、「用户」表里的用户都按同一个键查。
//! 密码走 stdin 传给脚本,不进命令行和日志;读回来只放内存。设计见 docs/modules/srvdata.md。

use crate::srvdata::DB;
use crate::sysops::quote;

/// 文件名里的键:`用户` 或 `用户@主机`。只允许 `[A-Za-z0-9_.%-]`(主机还可以有 `@` 前的分隔),
/// 不以 `.`/`-` 开头,不含 `..`;不合规返回 None(这样的用户不能保存密码)
pub fn key(user: &str, host: &str) -> Option<String> {
    let ok = |s: &str| !s.is_empty() && s.len() <= 80 && !s.starts_with(['.', '-']) && !s.contains("..") && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '%' | '-'));
    if !ok(user) {
        return None;
    }
    if host.is_empty() {
        return Some(user.to_string());
    }
    ok(host).then(|| format!("{user}@{host}"))
}

/// 容器名(docker 的命名规则)
fn container_ok(c: &str) -> bool {
    !c.is_empty() && c.len() <= 128 && !c.starts_with(['.', '-', '_']) && c.chars().all(|x| x.is_ascii_alphanumeric() || matches!(x, '_' | '.' | '-'))
}

fn dir(container: &str) -> Result<String, String> {
    if container_ok(container) { Ok(format!("{DB}/secrets/{container}")) } else { Err("容器名不合法".into()) }
}

fn file(container: &str, key: &str) -> Result<String, String> {
    if key.is_empty() || key.contains('/') || key.starts_with('.') {
        return Err("用户名不合法".into());
    }
    Ok(format!("{}/{key}.json", dir(container)?))
}

/// 文件内容(stdin 传给保存脚本):`{"user":…,"host":…,"password":…}`(PostgreSQL 没有 host)
pub fn content(user: &str, host: &str, password: &str) -> String {
    let mut m = serde_json::Map::new();
    m.insert("user".into(), user.into());
    if !host.is_empty() {
        m.insert("host".into(), host.into());
    }
    m.insert("password".into(), password.into());
    serde_json::to_string_pretty(&serde_json::Value::Object(m)).unwrap_or_default() + "\n"
}

/// 读回来的内容 → (用户, 主机, 密码);没有用户或密码就是 None
pub fn parse(text: &str) -> Option<(String, String, String)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let g = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    Some((g("user")?, g("host").unwrap_or_default(), g("password")?))
}

/// 保存:内容从 stdin 读(用 [`content`] 生成),原子写入。`only_if_exists` 为真时文件不存在就什么都不做
/// (重置密码时同步已保存的那份用)。输出 `OK`
pub fn save_script(container: &str, key: &str, only_if_exists: bool) -> Result<String, String> {
    let d = quote(&dir(container)?);
    let f = quote(&file(container, key)?);
    let guard = if only_if_exists { format!("[ -f {f} ] || {{ cat >/dev/null; echo OK; exit 0; }}; ") } else { String::new() };
    Ok(format!(
        "set -e; umask 077; {guard}mkdir -p {d}; chmod 700 {db}/secrets {d} 2>/dev/null || true; cat > {f}.tmp; chmod 600 {f}.tmp; mv {f}.tmp {f}; echo OK",
        db = DB
    ))
}

/// 列出容器下已保存的键,一行一个
pub fn list_script(container: &str) -> Result<String, String> {
    let d = quote(&dir(container)?);
    Ok(format!("for f in {d}/*.json; do [ -f \"$f\" ] || continue; b=${{f##*/}}; echo \"${{b%.json}}\"; done; true"))
}

/// 读出一个用户保存的文件(要 root)
pub fn read_script(container: &str, key: &str) -> Result<String, String> {
    Ok(format!("cat {}", quote(&file(container, key)?)))
}

/// 删除一个用户保存的文件
pub fn delete_script(container: &str, key: &str) -> Result<String, String> {
    Ok(format!("rm -f {}; echo OK", quote(&file(container, key)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(key("app", "").as_deref(), Some("app"));
        assert_eq!(key("app", "%").as_deref(), Some("app@%"));
        assert_eq!(key("a_b-1.x", "10.0.0.%").as_deref(), Some("a_b-1.x@10.0.0.%"));
        for bad in ["", ".x", "-x", "a/b", "a b", "a..b", "a;b", "a'b", "用户"] {
            assert_eq!(key(bad, ""), None, "{bad}");
        }
        assert_eq!(key("app", "a/b"), None);
    }

    #[test]
    fn content_round_trips() {
        for pw in ["plain", "it's \"quoted\" $x `y` \\ ;", "a'\\''b", "p@ss=word"] {
            let t = content("app", "%", pw);
            assert!(!t.contains("managed"), "json 里不放注释");
            assert_eq!(parse(&t), Some(("app".into(), "%".into(), pw.into())), "{pw}");
        }
        assert_eq!(parse(&content("pg", "", "x")), Some(("pg".into(), String::new(), "x".into())));
        assert_eq!(parse("{\"user\":\"a\"}"), None);
        assert_eq!(parse("not json"), None);
    }

    #[test]
    fn scripts_are_valid_shell_and_safe() {
        for s in [save_script("pg-main", "app", false).unwrap(), save_script("pg-main", "app@%", true).unwrap(), list_script("pg-main").unwrap(), read_script("pg-main", "app").unwrap(), delete_script("pg-main", "app").unwrap()] {
            let o = std::process::Command::new("bash").arg("-n").arg("-c").arg(&s).output().unwrap();
            assert!(o.status.success(), "{s}: {}", String::from_utf8_lossy(&o.stderr));
        }
        // 密码不在脚本里;路径用引号包着
        let s = save_script("pg-main", "app", false).unwrap();
        assert!(s.contains("umask 077") && s.contains("chmod 600") && s.contains("/opt/otter/databases/secrets/pg-main"));
        assert!(save_script("../x", "app", false).is_err());
        assert!(read_script("pg", "../etc").is_err());
        assert!(read_script("pg", ".hidden").is_err());
    }

    /// 在临时目录里真跑一遍:保存(权限)、只在存在时覆盖、列出、读回、删除
    #[test]
    fn save_list_read_delete_on_disk() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let root = std::env::temp_dir().join(format!("otter-dbsecret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let fix = |s: String| s.replace(DB, root.to_str().unwrap());
        let run = |script: String, input: Option<&str>| -> String {
            let mut c = std::process::Command::new("bash").arg("-c").arg(fix(script)).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
            if let Some(i) = input {
                c.stdin.take().unwrap().write_all(i.as_bytes()).unwrap();
            } else {
                drop(c.stdin.take());
            }
            let o = c.wait_with_output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).to_string()
        };
        // 文件不存在时 only_if_exists 什么都不写
        assert_eq!(run(save_script("c1", "u1", true).unwrap(), Some("x")).trim(), "OK");
        assert!(!root.join("secrets/c1/u1.json").exists());
        let c = content("u1", "", "p'w");
        assert_eq!(run(save_script("c1", "u1", false).unwrap(), Some(&c)).trim(), "OK");
        let f = root.join("secrets/c1/u1.json");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(root.join("secrets/c1")).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(root.join("secrets")).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(run(list_script("c1").unwrap(), None).trim(), "u1");
        assert_eq!(parse(&run(read_script("c1", "u1").unwrap(), None)), Some(("u1".into(), String::new(), "p'w".into())));
        // 重置:已存在就覆盖
        assert_eq!(run(save_script("c1", "u1", true).unwrap(), Some(&content("u1", "", "new"))).trim(), "OK");
        assert_eq!(parse(&run(read_script("c1", "u1").unwrap(), None)).unwrap().2, "new");
        assert_eq!(run(delete_script("c1", "u1").unwrap(), None).trim(), "OK");
        assert!(!f.exists());
        assert_eq!(run(list_script("c1").unwrap(), None).trim(), "");
        let _ = std::fs::remove_dir_all(&root);
    }
}
