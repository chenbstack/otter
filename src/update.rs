//! 自动更新:从 GitHub Release 检查新版本,下载对应平台的包并替换当前安装。
//!
//! HTTP 用 `ureq`(同步、rustls,不依赖系统的 curl 或 OpenSSL);都在后台线程里调用。
//! 只读公开仓库的最新正式版(`/releases/latest` 不含草稿和预发布)。
//!
//! - macOS:下载 dmg,退出后由一段 shell 脚本挂载、替换 `Otter.app`、重新打开。
//! - Windows:下载 `-setup.exe`,以 `--silent` 静默安装(安装程序会结束旧进程、覆盖文件、再启动新版)。
//! - Linux:没有安装器,点击后打开发布页。
//! - 没找到当前平台的安装包、或不是从安装位置运行(如 `cargo run`)时,同样退回到打开发布页。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

pub const REPO: &str = "chenbstack/otter";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    /// 不带 v 前缀的版本号
    pub version: String,
    /// 发布页,装不了时打开它
    pub page: String,
    pub asset: Option<Asset>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    Available(Update),
    Downloading(Update),
    Failed(Update, String),
}

// ---- 版本比较 ---------------------------------------------------------------------------------

/// `v1.2.3-rc1` → ([1,2,3], true);无法解析返回 None。
fn parse_version(s: &str) -> Option<(Vec<u64>, bool)> {
    let s = s.trim().trim_start_matches('v');
    let (core, pre) = match s.find(['-', '+']) {
        Some(i) => (&s[..i], s[i..].starts_with('-')),
        None => (s, false),
    };
    let nums: Option<Vec<u64>> = core.split('.').map(|p| p.parse().ok()).collect();
    let nums = nums?;
    if nums.is_empty() { None } else { Some((nums, pre)) }
}

/// `latest` 是否比 `current` 新。数字部分相同时,预发布版(带 `-xxx`)比正式版旧。
pub fn is_newer(latest: &str, current: &str) -> bool {
    let (Some((mut a, a_pre)), Some((mut b, b_pre))) = (parse_version(latest), parse_version(current)) else { return false };
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    if a != b {
        return a > b;
    }
    b_pre && !a_pre
}

// ---- 解析 Release -----------------------------------------------------------------------------

/// 从 Release 的 assets 里挑当前平台的安装包。
fn pick_asset(assets: &[Value], os: &str, arch: &str) -> Option<Asset> {
    let want = |name: &str| -> bool {
        let n = name.to_ascii_lowercase();
        match os {
            "macos" => n.ends_with(".dmg") && (if arch == "aarch64" { n.contains("arm64") || n.contains("aarch64") } else { n.contains("x86_64") || n.contains("x64") || n.contains("intel") }),
            "windows" => n.ends_with("-setup.exe"),
            _ => false,
        }
    };
    assets.iter().find_map(|a| {
        let name = a.get("name")?.as_str()?;
        if !want(name) {
            return None;
        }
        Some(Asset { name: name.to_string(), url: a.get("browser_download_url")?.as_str()?.to_string() })
    })
}

/// 解析 `/releases/latest` 的响应;草稿、预发布或版本号不比 `current` 新时返回 None。
pub fn parse_release(body: &str, current: &str, os: &str, arch: &str) -> Option<Update> {
    let v: Value = serde_json::from_str(body).ok()?;
    if v.get("draft").and_then(Value::as_bool) == Some(true) || v.get("prerelease").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let tag = v.get("tag_name")?.as_str()?;
    if !is_newer(tag, current) {
        return None;
    }
    let assets = v.get("assets").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    Some(Update {
        version: tag.trim_start_matches('v').to_string(),
        page: v.get("html_url").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("https://github.com/{REPO}/releases")),
        asset: pick_asset(assets, os, arch),
    })
}

// ---- 进程辅助 ---------------------------------------------------------------------------------

fn command(program: &str) -> Command {
    let c = Command::new(program);
    #[cfg(windows)]
    let c = {
        use std::os::windows::process::CommandExt;
        let mut c = c;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    };
    c
}

fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(timeout_secs)))
        .user_agent("otter-updater")
        .build()
        .into()
}

/// 检查是否有新版本(阻塞,放在后台线程里调用)。
pub fn check() -> Result<Option<Update>, String> {
    let url = std::env::var("OTTER_UPDATE_API").unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest"));
    let mut resp = agent(20).get(&url).header("Accept", "application/vnd.github+json").call().map_err(|e| format!("请求 GitHub 失败: {e}"))?;
    let body = resp.body_mut().read_to_string().map_err(|e| format!("读取响应失败: {e}"))?;
    Ok(parse_release(&body, CURRENT, std::env::consts::OS, std::env::consts::ARCH))
}

fn work_dir() -> PathBuf {
    std::env::temp_dir().join("otter-update")
}

/// 下载安装包到临时目录(阻塞)。
pub fn download(asset: &Asset) -> Result<PathBuf, String> {
    let dir = work_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    // 文件名来自网络,只取最后一段,防止路径穿越
    let name = Path::new(&asset.name).file_name().map(|n| n.to_owned()).ok_or("安装包名称无效")?;
    let path = dir.join(name);
    let resp = agent(900).get(&asset.url).call().map_err(|e| format!("下载失败: {e}"))?;
    let mut file = std::fs::File::create(&path).map_err(|e| format!("创建文件失败: {e}"))?;
    std::io::copy(&mut resp.into_body().into_reader(), &mut file).map_err(|e| format!("下载中断: {e}"))?;
    match std::fs::metadata(&path) {
        Ok(m) if m.len() > 0 => Ok(path),
        _ => Err("下载的文件是空的".into()),
    }
}

/// 打开发布页(浏览器)。
pub fn open_page(url: &str) {
    if !url.starts_with("https://") {
        return;
    }
    #[cfg(target_os = "macos")]
    let _ = command("open").arg(url).spawn();
    #[cfg(windows)]
    let _ = command("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = command("xdg-open").arg(url).spawn();
}

/// 当前 `.app` 包的路径(只有从应用包里运行才有)。
#[cfg(target_os = "macos")]
fn current_app() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let app = exe.parent()?.parent()?.parent()?.to_path_buf();
    (app.extension().is_some_and(|e| e == "app")).then_some(app)
}

#[cfg(target_os = "macos")]
const APPLY_SH: &str = r#"#!/bin/sh
# 等 Otter 退出 → 挂载 dmg → 替换 .app → 重新打开。失败时把 dmg 打开,让用户手动拖。
PID="$1"; APP="$2"; DMG="$3"
while kill -0 "$PID" 2>/dev/null; do sleep 0.2; done
MNT="$(mktemp -d /tmp/otter-mnt.XXXXXX)"
if hdiutil attach -nobrowse -readonly -mountpoint "$MNT" "$DMG" >/dev/null 2>&1 && [ -d "$MNT/Otter.app" ]; then
  rm -rf "$APP.new"
  if ditto "$MNT/Otter.app" "$APP.new" && rm -rf "$APP" && mv "$APP.new" "$APP"; then
    hdiutil detach "$MNT" -quiet >/dev/null 2>&1
    rm -f "$DMG"
    xattr -cr "$APP" 2>/dev/null
    open "$APP"
    exit 0
  fi
  rm -rf "$APP.new"
fi
hdiutil detach "$MNT" -quiet >/dev/null 2>&1
open "$DMG"
"#;

/// 把下载好的安装包交给平台的安装流程。成功返回 Ok 后调用方应立即退出程序。
pub fn install(pkg: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let app = current_app().ok_or("当前不是从应用包里运行,请手动安装下载的 dmg")?;
        let script = work_dir().join("apply.sh");
        std::fs::write(&script, APPLY_SH).map_err(|e| format!("写更新脚本失败: {e}"))?;
        Command::new("sh")
            .arg(&script)
            .arg(std::process::id().to_string())
            .arg(&app)
            .arg(pkg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("启动更新脚本失败: {e}"))?;
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).ok_or("找不到安装目录")?;
        Command::new(pkg)
            .arg("--silent")
            .arg("--dir")
            .arg(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0000_0008 | 0x0800_0000) // DETACHED_PROCESS | CREATE_NO_WINDOW
            .spawn()
            .map_err(|e| format!("启动安装程序失败: {e}"))?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (pkg, Stdio::null());
        Err("这个平台暂不支持自动安装".into())
    }
}

#[cfg(test)]
mod tests {
    use crate::update::{is_newer, parse_release, parse_version};

    #[test]
    fn version_parse_and_compare() {
        assert_eq!(parse_version("v1.2.3"), Some((vec![1, 2, 3], false)));
        assert_eq!(parse_version("0.1.0-test2"), Some((vec![0, 1, 0], true)));
        assert_eq!(parse_version("abc"), None);
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        // 预发布比同号正式版旧
        assert!(!is_newer("0.1.0-test2", "0.1.0"));
        assert!(is_newer("0.1.0", "0.1.0-rc1"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    const BODY: &str = r#"{"tag_name":"v0.3.0","html_url":"https://github.com/x/y/releases/tag/v0.3.0","draft":false,"prerelease":false,"assets":[
        {"name":"Otter-0.3.0-arm64.dmg","browser_download_url":"https://example.com/a.dmg"},
        {"name":"Otter-0.3.0-windows-x86_64-setup.exe","browser_download_url":"https://example.com/b.exe"},
        {"name":"Otter-0.3.0-linux-x86_64.tar.gz","browser_download_url":"https://example.com/c.tgz"}]}"#;

    #[test]
    fn release_picks_platform_asset() {
        let u = parse_release(BODY, "0.1.0", "macos", "aarch64").unwrap();
        assert_eq!(u.version, "0.3.0");
        assert_eq!(u.asset.unwrap().url, "https://example.com/a.dmg");
        let u = parse_release(BODY, "0.1.0", "windows", "x86_64").unwrap();
        assert!(u.asset.unwrap().name.ends_with("-setup.exe"));
        // Linux 没有安装器:有更新但没有 asset,界面退回打开发布页
        assert!(parse_release(BODY, "0.1.0", "linux", "x86_64").unwrap().asset.is_none());
        // Intel Mac 没有对应的 dmg
        assert!(parse_release(BODY, "0.1.0", "macos", "x86_64").unwrap().asset.is_none());
    }

    #[test]
    fn release_ignores_old_draft_and_prerelease() {
        assert!(parse_release(BODY, "0.3.0", "macos", "aarch64").is_none());
        assert!(parse_release(BODY, "0.9.0", "macos", "aarch64").is_none());
        let draft = BODY.replace("\"draft\":false", "\"draft\":true");
        assert!(parse_release(&draft, "0.1.0", "macos", "aarch64").is_none());
        let pre = BODY.replace("\"prerelease\":false", "\"prerelease\":true");
        assert!(parse_release(&pre, "0.1.0", "macos", "aarch64").is_none());
        assert!(parse_release("not json", "0.1.0", "macos", "aarch64").is_none());
    }

    /// 端到端(需要网络/本地服务器,默认不跑):`OTTER_UPDATE_API=http://127.0.0.1:8000/release.json cargo test --release -- --ignored e2e`
    #[test]
    #[ignore]
    fn e2e_check_and_download() {
        let u = crate::update::check().expect("check").expect("应该发现新版本");
        println!("发现 {}", u.version);
        let asset = u.asset.expect("应该有当前平台的安装包");
        let p = crate::update::download(&asset).expect("download");
        println!("已下载 {} ({} 字节)", p.display(), std::fs::metadata(&p).unwrap().len());
    }
}
