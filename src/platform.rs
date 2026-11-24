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

#[cfg(test)]
mod tests {
    use super::home_string;

    #[test]
    fn home_is_never_empty_string() {
        assert!(!home_string().is_empty());
    }
}
