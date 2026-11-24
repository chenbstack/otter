//! 平台差异集中在这里:主目录、快捷键修饰键。其余代码不直接写 `cfg(target_os)`。

use std::path::PathBuf;

use gpui_kit::*;

/// 用户主目录;取不到时退回环境变量,再取不到是空路径。
pub fn home_dir() -> PathBuf {
    directories::UserDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from))
        .unwrap_or_default()
}

/// 主目录的字符串形式,取不到时是根目录。
pub fn home_string() -> String {
    let h = home_dir();
    if h.as_os_str().is_empty() { std::path::MAIN_SEPARATOR.to_string() } else { h.to_string_lossy().into_owned() }
}

/// 应用级加速键:macOS 是 ⌘;Windows / Linux 上 Ctrl 要留给终端(Ctrl+C 是中断),所以用 Ctrl+Shift。
pub fn accel(m: &Modifiers) -> bool {
    if cfg!(target_os = "macos") { m.platform && !m.control && !m.alt } else { m.control && m.shift && !m.alt && !m.platform }
}

/// 「加速键 + Shift」:macOS 是 ⌘⇧,其它平台是 Ctrl+Alt+Shift(Ctrl+Shift 已被 `accel` 占用)。
pub fn accel_shift(m: &Modifiers) -> bool {
    if cfg!(target_os = "macos") { m.platform && m.shift && !m.control && !m.alt } else { m.control && m.shift && m.alt && !m.platform }
}

/// 界面提示里加速键的写法。
pub fn accel_name() -> &'static str {
    if cfg!(target_os = "macos") { "⌘" } else { "Ctrl+Shift+" }
}

/// `ensure_link` 的结果
#[derive(Debug, PartialEq)]
pub enum Link {
    Created,
    Updated,
    /// 已经指向当前程序
    Same,
    /// 那个位置有别的东西,不动它
    Occupied,
}

/// 让 `link` 成为指向 `exe` 的软链接:不存在就建;是指向另一个 Otter.app 的旧链接就改指;其它情况(普通文件、
/// 别的程序的链接)一律不碰。
#[cfg(unix)]
pub fn ensure_link(exe: &std::path::Path, link: &std::path::Path) -> std::io::Result<Link> {
    use std::os::unix::fs::symlink;
    match std::fs::symlink_metadata(link) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = link.parent() {
                std::fs::create_dir_all(dir)?;
            }
            symlink(exe, link)?;
            Ok(Link::Created)
        }
        Err(e) => Err(e),
        Ok(md) if md.file_type().is_symlink() => {
            let target = std::fs::read_link(link)?;
            if target == exe {
                Ok(Link::Same)
            } else if target.to_string_lossy().contains(".app/Contents/MacOS/otter") {
                std::fs::remove_file(link)?;
                symlink(exe, link)?;
                Ok(Link::Updated)
            } else {
                Ok(Link::Occupied)
            }
        }
        Ok(_) => Ok(Link::Occupied),
    }
}

/// 启动时调用:从「应用程序」里的 Otter.app 运行时,在 `~/.local/bin/otter` 放一个软链接,
/// 这样终端和 MCP 配置里可以直接用 `otter`。开发版(target/ 下)不会动它。返回要写进日志的一句话。
#[cfg(target_os = "macos")]
pub fn install_cli_link() -> Option<String> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let s = exe.to_string_lossy();
    if !(s.contains("/Applications/") && s.contains(".app/Contents/MacOS/")) {
        return None;
    }
    let link = home_dir().join(".local/bin/otter");
    match ensure_link(&exe, &link) {
        Ok(Link::Created) => Some(format!("已创建命令行入口 {}(要在终端里直接用 otter,请确认 ~/.local/bin 在 PATH 里)", link.display())),
        Ok(Link::Updated) => Some(format!("已更新命令行入口 {}", link.display())),
        Ok(Link::Occupied) => Some(format!("{} 已被占用,没有创建命令行入口", link.display())),
        Ok(Link::Same) => None,
        Err(e) => Some(format!("创建命令行入口失败:{e}")),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn install_cli_link() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::home_string;

    #[test]
    fn home_is_never_empty_string() {
        assert!(!home_string().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_link_cases() {
        use super::{Link, ensure_link};
        let dir = std::env::temp_dir().join(format!("otter-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let exe = std::path::PathBuf::from("/Applications/Otter.app/Contents/MacOS/otter");
        let link = dir.join("bin/otter");
        assert_eq!(ensure_link(&exe, &link).unwrap(), Link::Created);
        assert_eq!(ensure_link(&exe, &link).unwrap(), Link::Same);
        // 旧的、指向别处 Otter.app 的链接会改指
        let other = std::path::PathBuf::from("/x/Otter.app/Contents/MacOS/otter");
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert_eq!(ensure_link(&exe, &link).unwrap(), Link::Updated);
        // 别的程序的链接、普通文件都不碰
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("/usr/bin/true", &link).unwrap();
        assert_eq!(ensure_link(&exe, &link).unwrap(), Link::Occupied);
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "x").unwrap();
        assert_eq!(ensure_link(&exe, &link).unwrap(), Link::Occupied);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
