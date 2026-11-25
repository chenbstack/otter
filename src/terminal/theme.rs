//! 终端外观偏好:主题(色板)、字体、字号。全局状态 + 存进 settings 表,
//! `start_session` 和容器抽屉终端都从这里取配置;设置页改动后对已打开的终端即时生效(`Home::apply_term_prefs`)。
//! 只做深色预设(决策见计划 2026-11-24-settings-page)。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::ui::{MONO, TERM_FONT};

pub const KEY_THEME: &str = "term_theme";
pub const KEY_FONT: &str = "term_font";
pub const KEY_SIZE: &str = "term_font_size";

pub const SIZE_MIN: f32 = 10.0;
pub const SIZE_MAX: f32 = 24.0;

pub type Rgb = (u8, u8, u8);

pub struct Theme {
    pub id: &'static str,
    pub name: &'static str,
    pub fg: Rgb,
    /// 主题的原背景色。终端和设置页预览都不画它(背景透明,让窗口底光晕透出来),只交给色板
    pub bg: Rgb,
    pub cursor: Rgb,
    /// 0–7 普通,8–15 明亮
    pub ansi: [Rgb; 16],
}

const fn h(v: u32) -> Rgb {
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// 默认主题(没有保存过选择时用)
pub const DEFAULT_ID: &str = "tokyo-night";

pub static THEMES: [Theme; 9] = [
    Theme {
        id: "tokyo-night",
        name: "Tokyo Night",
        fg: h(0xC0CAF5),
        bg: h(0x1A1B26),
        cursor: h(0xC0CAF5),
        ansi: [
            h(0x15161E), h(0xF7768E), h(0x9ECE6A), h(0xE0AF68), h(0x7AA2F7), h(0xBB9AF7), h(0x7DCFFF), h(0xA9B1D6),
            h(0x414868), h(0xF7768E), h(0x9ECE6A), h(0xE0AF68), h(0x7AA2F7), h(0xBB9AF7), h(0x7DCFFF), h(0xC0CAF5),
        ],
    },
    Theme {
        id: "one-dark",
        name: "One Dark",
        fg: h(0xABB2BF),
        bg: h(0x282C34),
        cursor: h(0x528BFF),
        ansi: [
            h(0x282C34), h(0xE06C75), h(0x98C379), h(0xE5C07B), h(0x61AFEF), h(0xC678DD), h(0x56B6C2), h(0xABB2BF),
            h(0x5C6370), h(0xE06C75), h(0x98C379), h(0xE5C07B), h(0x61AFEF), h(0xC678DD), h(0x56B6C2), h(0xFFFFFF),
        ],
    },
    Theme {
        id: "dracula",
        name: "Dracula",
        fg: h(0xF8F8F2),
        bg: h(0x282A36),
        cursor: h(0xF8F8F2),
        ansi: [
            h(0x21222C), h(0xFF5555), h(0x50FA7B), h(0xF1FA8C), h(0xBD93F9), h(0xFF79C6), h(0x8BE9FD), h(0xF8F8F2),
            h(0x6272A4), h(0xFF6E6E), h(0x69FF94), h(0xFFFFA5), h(0xD6ACFF), h(0xFF92DF), h(0xA4FFFF), h(0xFFFFFF),
        ],
    },
    Theme {
        id: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        fg: h(0xCDD6F4),
        bg: h(0x1E1E2E),
        cursor: h(0xF5E0DC),
        ansi: [
            h(0x45475A), h(0xF38BA8), h(0xA6E3A1), h(0xF9E2AF), h(0x89B4FA), h(0xF5C2E7), h(0x94E2D5), h(0xBAC2DE),
            h(0x585B70), h(0xF38BA8), h(0xA6E3A1), h(0xF9E2AF), h(0x89B4FA), h(0xF5C2E7), h(0x94E2D5), h(0xA6ADC8),
        ],
    },
    Theme {
        id: "gruvbox-dark",
        name: "Gruvbox Dark",
        fg: h(0xEBDBB2),
        bg: h(0x282828),
        cursor: h(0xEBDBB2),
        ansi: [
            h(0x282828), h(0xCC241D), h(0x98971A), h(0xD79921), h(0x458588), h(0xB16286), h(0x689D6A), h(0xA89984),
            h(0x928374), h(0xFB4934), h(0xB8BB26), h(0xFABD2F), h(0x83A598), h(0xD3869B), h(0x8EC07C), h(0xEBDBB2),
        ],
    },
    Theme {
        id: "nord",
        name: "Nord",
        fg: h(0xD8DEE9),
        bg: h(0x2E3440),
        cursor: h(0xD8DEE9),
        ansi: [
            h(0x3B4252), h(0xBF616A), h(0xA3BE8C), h(0xEBCB8B), h(0x81A1C1), h(0xB48EAD), h(0x88C0D0), h(0xE5E9F0),
            h(0x4C566A), h(0xBF616A), h(0xA3BE8C), h(0xEBCB8B), h(0x81A1C1), h(0xB48EAD), h(0x8FBCBB), h(0xECEFF4),
        ],
    },
    Theme {
        id: "monokai",
        name: "Monokai",
        fg: h(0xF8F8F2),
        bg: h(0x272822),
        cursor: h(0xF8F8F0),
        ansi: [
            h(0x272822), h(0xF92672), h(0xA6E22E), h(0xF4BF75), h(0x66D9EF), h(0xAE81FF), h(0xA1EFE4), h(0xF8F8F2),
            h(0x75715E), h(0xF92672), h(0xA6E22E), h(0xF4BF75), h(0x66D9EF), h(0xAE81FF), h(0xA1EFE4), h(0xF9F8F5),
        ],
    },
    Theme {
        id: "github-dark",
        name: "GitHub Dark",
        fg: h(0xC9D1D9),
        bg: h(0x0D1117),
        cursor: h(0x58A6FF),
        ansi: [
            h(0x484F58), h(0xFF7B72), h(0x3FB950), h(0xD29922), h(0x58A6FF), h(0xBC8CFF), h(0x39C5CF), h(0xB1BAC4),
            h(0x6E7681), h(0xFFA198), h(0x56D364), h(0xE3B341), h(0x79C0FF), h(0xD2A8FF), h(0x56D4DD), h(0xF0F6FC),
        ],
    },
    Theme {
        id: "solarized-dark",
        name: "Solarized Dark",
        fg: h(0x839496),
        bg: h(0x002B36),
        cursor: h(0x93A1A1),
        ansi: [
            h(0x073642), h(0xDC322F), h(0x859900), h(0xB58900), h(0x268BD2), h(0xD33682), h(0x2AA198), h(0xEEE8D5),
            h(0x586E75), h(0xCB4B16), h(0x7C9A1A), h(0xC9A227), h(0x6C9FD4), h(0x6C71C4), h(0x93A1A1), h(0xFDF6E3),
        ],
    },
];

/// `usize::MAX` = 还没选过,用默认主题
static THEME: AtomicUsize = AtomicUsize::new(usize::MAX);
static SIZE: AtomicU32 = AtomicU32::new(0);
static FONT: Mutex<String> = Mutex::new(String::new());

pub fn current() -> &'static Theme {
    match THEME.load(Ordering::Relaxed) {
        i if i < THEMES.len() => &THEMES[i],
        _ => THEMES.iter().find(|t| t.id == DEFAULT_ID).unwrap_or(&THEMES[0]),
    }
}

pub fn current_id() -> &'static str {
    current().id
}

pub fn set_theme(id: &str) {
    if let Some(i) = THEMES.iter().position(|t| t.id == id) {
        THEME.store(i, Ordering::Relaxed);
    }
}

/// 当前字号(未设置时取默认)
pub fn size() -> f32 {
    let b = SIZE.load(Ordering::Relaxed);
    if b == 0 { TERM_FONT } else { f32::from_bits(b) }
}

pub fn set_size(v: f32) {
    SIZE.store(v.clamp(SIZE_MIN, SIZE_MAX).round().to_bits(), Ordering::Relaxed);
}

/// 用户选的字体名;空串 = 各平台默认等宽字体
pub fn font_choice() -> String {
    FONT.lock().map(|f| f.clone()).unwrap_or_default()
}

/// 实际使用的字体名
pub fn font() -> String {
    let f = font_choice();
    if f.is_empty() { MONO.to_string() } else { f }
}

pub fn set_font(name: &str) {
    if let Ok(mut f) = FONT.lock() {
        *f = name.to_string();
    }
}

/// 启动时按记住的设置恢复
pub fn restore(st: &crate::store::Store) {
    if let Some(id) = st.get_setting(KEY_THEME) {
        set_theme(&id);
    }
    if let Some(v) = st.get_setting(KEY_SIZE).and_then(|v| v.parse::<f32>().ok()) {
        set_size(v);
    }
    if let Some(f) = st.get_setting(KEY_FONT) {
        set_font(&f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_unique_and_default_is_tokyo_night() {
        for (i, a) in THEMES.iter().enumerate() {
            assert!(THEMES.iter().skip(i + 1).all(|b| b.id != a.id));
        }
        assert_eq!(THEMES[0].id, DEFAULT_ID);
        assert_eq!(current_id(), DEFAULT_ID);
    }

    #[test]
    fn size_is_clamped_and_unknown_theme_ignored() {
        set_size(99.0);
        assert_eq!(size(), SIZE_MAX);
        set_size(1.0);
        assert_eq!(size(), SIZE_MIN);
        let before = current_id();
        set_theme("nope");
        assert_eq!(current_id(), before);
        set_size(TERM_FONT);
    }
}
