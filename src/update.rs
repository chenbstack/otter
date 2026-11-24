//! 自动更新:从 GitHub Release 检查新版本,下载对应平台的包并替换当前安装。
//!
//! HTTP 用 `ureq`(同步、rustls,不依赖系统的 curl 或 OpenSSL);都在后台线程里调用。
//! 只读公开仓库的最新正式版(`/releases/latest` 不含草稿和预发布)。
//!
//! 复制、替换、重启都在进程内完成,不再用 `sh`/`ditto`/`xattr`/`open`;唯一的系统命令是 macOS 自带的 `hdiutil`(挂载 dmg):
//! - macOS:下载 `.dmg`,用 `hdiutil` 挂载后把 `Otter.app` 复制到旁边的 `Otter.app.new`,换掉旧包后直接启动新版。
//! - Linux:下载 `.tar.gz`,取出 `otter-gui`/`otter` 覆盖当前所在目录里的同名文件,再启动新版。
//! - Windows:下载 `-setup.exe`,以 `--silent` 静默安装(安装程序会结束旧进程、覆盖文件、再启动新版)。
//! - 没找到当前平台的安装包、或不是从安装位置运行(如 `cargo run`)时,退回到打开发布页(浏览器只能交给系统打开)。
//!
//! 新进程带 `--after-update <旧pid>` 启动,先等旧进程退出再继续,避免抢控制 socket。

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
            "linux" => n.ends_with(".tar.gz") && n.contains("linux") && (arch != "aarch64" || n.contains("aarch64") || n.contains("arm64")) && (arch != "x86_64" || n.contains("x86_64")),
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

/// 新版启动参数:`--after-update <旧pid>`。main 一开始调用,等旧进程退出。
pub fn wait_for_old_process() {
    let mut args = std::env::args().skip_while(|a| a != "--after-update");
    let Some(pid) = args.nth(1).and_then(|p| p.parse::<u32>().ok()) else { return };
    #[cfg(unix)]
    for _ in 0..100 {
        // kill(pid, 0):只探测进程是否还在
        if unsafe { libc::kill(pid as i32, 0) } != 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(unix)]
fn relaunch(exe: &Path) -> Result<(), String> {
    Command::new(exe)
        .arg("--after-update")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动新版失败: {e}"))
}

/// 递归复制目录,保留符号链接和权限(`.app` 里的签名封印依赖它们)。
#[cfg(target_os = "macos")]
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (from, to) = (e.path(), dst.join(e.file_name()));
        let ty = e.file_type()?;
        if ty.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
        } else if ty.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?; // 连同权限位一起复制
        }
    }
    std::fs::set_permissions(dst, std::fs::metadata(src)?.permissions())
}

/// macOS:挂载 dmg → 复制 `.app` → 替换 → 启动新版。
#[cfg(target_os = "macos")]
fn install_macos(pkg: &Path) -> Result<(), String> {
    let app = current_app().ok_or("当前不是从应用包里运行,请手动安装下载的 dmg")?;
    let parent = app.parent().ok_or("找不到应用所在目录")?;
    let name = app.file_name().ok_or("应用名无效")?.to_string_lossy().into_owned();
    // 复制到应用旁边,保证和旧包在同一个卷上,后面的 rename 才是原子的
    let new = parent.join(format!("{name}.new"));
    let old = parent.join(format!("{name}.old"));
    let _ = std::fs::remove_dir_all(&new);
    let _ = std::fs::remove_dir_all(&old);
    let mnt = work_dir().join("mnt");
    let _ = std::fs::create_dir_all(&mnt);
    let attach = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-noverify", "-mountpoint"])
        .arg(&mnt)
        .arg(pkg)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("挂载 dmg 失败: {e}"))?;
    if !attach.status.success() {
        return Err(format!("挂载 dmg 失败: {}", String::from_utf8_lossy(&attach.stderr).trim()));
    }
    let src = mnt.join("Otter.app");
    let copied = if src.join("Contents/MacOS/otter-gui").is_file() {
        copy_tree(&src, &new).map_err(|e| format!("复制失败(应用目录不可写?): {e}"))
    } else {
        Err("dmg 里没有 Otter.app".into())
    };
    let _ = Command::new("hdiutil").args(["detach", "-quiet", "-force"]).arg(&mnt).output();
    if let Err(e) = copied {
        let _ = std::fs::remove_dir_all(&new);
        return Err(e);
    }
    std::fs::rename(&app, &old).map_err(|e| format!("替换旧版失败: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &app) {
        let _ = std::fs::rename(&old, &app); // 回滚
        return Err(format!("替换旧版失败: {e}"));
    }
    let _ = std::fs::remove_dir_all(&old);
    let _ = std::fs::remove_file(pkg);
    relaunch(&app.join("Contents/MacOS/otter-gui"))
}

/// Linux:从 tar.gz 里取出 `otter-gui` / `otter`,覆盖当前目录里的同名文件 → 启动新版。
#[cfg(all(unix, not(target_os = "macos")))]
fn install_linux(pkg: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let exe = std::env::current_exe().map_err(|e| format!("找不到当前程序: {e}"))?;
    let dir = exe.parent().ok_or("找不到安装目录")?.to_path_buf();
    let file = std::fs::File::open(pkg).map_err(|e| format!("打开安装包失败: {e}"))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut staged = Vec::new();
    for entry in tar.entries().map_err(|e| format!("安装包损坏: {e}"))? {
        let mut entry = entry.map_err(|e| format!("安装包损坏: {e}"))?;
        // 只认这两个文件名,取 basename,不信任包内路径
        let Some(base) = entry.path().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())) else { continue };
        if !matches!(base.as_str(), "otter-gui" | "otter") || !entry.header().entry_type().is_file() {
            continue;
        }
        let tmp = dir.join(format!(".{base}.new"));
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("安装目录不可写: {e}"))?;
        std::io::copy(&mut entry, &mut f).map_err(|e| format!("解压失败: {e}"))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| format!("设置权限失败: {e}"))?;
        staged.push((tmp, dir.join(&base)));
    }
    if !staged.iter().any(|(_, dst)| dst.file_name().is_some_and(|n| n == "otter-gui")) {
        for (tmp, _) in &staged {
            let _ = std::fs::remove_file(tmp);
        }
        return Err("安装包里没有 otter-gui".into());
    }
    for (tmp, dst) in &staged {
        std::fs::rename(tmp, dst).map_err(|e| format!("替换 {} 失败: {e}", dst.display()))?; // 覆盖运行中的可执行文件在 Linux 上是允许的
    }
    let _ = std::fs::remove_file(pkg);
    relaunch(&exe)
}

/// 把下载好的安装包交给平台的安装流程。成功返回 Ok 后调用方应立即退出程序。
pub fn install(pkg: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        install_macos(pkg)
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
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        install_linux(pkg)
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
        assert_eq!(parse_release(BODY, "0.1.0", "linux", "x86_64").unwrap().asset.unwrap().url, "https://example.com/c.tgz");
        // 没有对应架构的包:有更新但没有 asset,界面退回打开发布页
        assert!(parse_release(BODY, "0.1.0", "linux", "aarch64").unwrap().asset.is_none());
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
