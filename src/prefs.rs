//! 设置页里的偏好(内存里的全局值 + `settings` 表的键)。启动时 `restore`,设置页改值时 `set_*` 并保存。
//! 终端主题/字体、窗口记忆、日志保留、更新、SSH 连接、AI 接管的偏好在各自模块里(见 docs/modules/settings.md)。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

macro_rules! num_pref {
    ($get:ident, $set:ident, $static:ident, $key:ident, $keystr:literal, $def:expr, $min:expr, $max:expr) => {
        static $static: AtomicU64 = AtomicU64::new($def);
        pub const $key: &str = $keystr;
        pub fn $get() -> u64 {
            $static.load(Ordering::Relaxed)
        }
        pub fn $set(v: u64) {
            $static.store(v.clamp($min, $max), Ordering::Relaxed);
        }
    };
}

macro_rules! bool_pref {
    ($get:ident, $set:ident, $static:ident, $key:ident, $keystr:literal, $def:expr) => {
        static $static: AtomicBool = AtomicBool::new($def);
        pub const $key: &str = $keystr;
        pub fn $get() -> bool {
            $static.load(Ordering::Relaxed)
        }
        pub fn $set(on: bool) {
            $static.store(on, Ordering::Relaxed);
        }
    };
}

// 终端
num_pref!(scrollback, set_scrollback, SCROLLBACK, KEY_SCROLLBACK, "term_scrollback", 10000, 1000, 100000);
// 光标形状:0 块 / 1 竖线 / 2 下划线
num_pref!(cursor_style, set_cursor_style, CURSOR_STYLE, KEY_CURSOR_STYLE, "term_cursor_style", 0, 0, 2);
bool_pref!(cursor_blink, set_cursor_blink, CURSOR_BLINK, KEY_CURSOR_BLINK, "term_cursor_blink", false);
bool_pref!(copy_on_select, set_copy_on_select, COPY_ON_SELECT, KEY_COPY_ON_SELECT, "term_copy_on_select", true);
bool_pref!(right_click_paste, set_right_click_paste, RIGHT_PASTE, KEY_RIGHT_PASTE, "term_right_click_paste", false);
bool_pref!(confirm_close_tab, set_confirm_close_tab, CONFIRM_TAB, KEY_CONFIRM_TAB, "confirm_close_tab", false);
// 响铃:0 无 / 1 视觉闪烁 / 2 系统提示音
num_pref!(bell, set_bell, BELL, KEY_BELL, "term_bell", 0, 0, 2);

// 通用
bool_pref!(confirm_quit, set_confirm_quit, CONFIRM_QUIT, KEY_CONFIRM_QUIT, "confirm_quit", false);
bool_pref!(restore_tabs, set_restore_tabs, RESTORE_TABS, KEY_RESTORE_TABS, "restore_tabs", false);
/// 上次退出时打开的节点标签(节点 id,逗号分隔)和活动的那个
pub const KEY_LAST_NODES: &str = "last_nodes";
pub const KEY_LAST_ACTIVE: &str = "last_active_node";

// 刷新频率(秒)
num_pref!(proc_secs, set_proc_secs, PROC_SECS, KEY_PROC_SECS, "refresh_proc_secs", 3, 1, 60);
/// 其余页面(容器、镜像、存储卷、服务、存储、计划任务、网站、数据库)自动重读的间隔
num_pref!(page_secs, set_page_secs, PAGE_SECS, KEY_PAGE_SECS, "refresh_page_secs", 10, 5, 600);
num_pref!(ctlog_secs, set_ctlog_secs, CTLOG_SECS, KEY_CTLOG_SECS, "refresh_ctlog_secs", 2, 1, 60);

// AI 提权:是否允许在确认框里选「10/30 分钟内免确认」
bool_pref!(sudo_allow_window, set_sudo_allow_window, SUDO_WINDOW, KEY_SUDO_WINDOW, "sudo_allow_window", true);

fn flag(st: &crate::store::Store, key: &str, def: bool) -> bool {
    match st.get_setting(key).as_deref() {
        Some("on") => true,
        Some("off") => false,
        _ => def,
    }
}

pub fn restore(st: &crate::store::Store) {
    let num = |k: &str| st.get_setting(k).and_then(|v| v.parse::<u64>().ok());
    if let Some(v) = num(KEY_SCROLLBACK) { set_scrollback(v) }
    if let Some(v) = num(KEY_CURSOR_STYLE) { set_cursor_style(v) }
    if let Some(v) = num(KEY_DEFAULT_MODE) { set_default_mode(v) }
    if let Some(v) = num(KEY_BELL) { set_bell(v) }
    if let Some(v) = num(KEY_PROC_SECS) { set_proc_secs(v) }
    if let Some(v) = num(KEY_CTLOG_SECS) { set_ctlog_secs(v) }
    if let Some(v) = num(KEY_PAGE_SECS) { set_page_secs(v) }
    set_cursor_blink(flag(st, KEY_CURSOR_BLINK, false));
    set_copy_on_select(flag(st, KEY_COPY_ON_SELECT, true));
    set_right_click_paste(flag(st, KEY_RIGHT_PASTE, false));
    set_confirm_close_tab(flag(st, KEY_CONFIRM_TAB, false));
    set_confirm_quit(flag(st, KEY_CONFIRM_QUIT, false));
    set_restore_tabs(flag(st, KEY_RESTORE_TABS, false));
    set_sudo_allow_window(flag(st, KEY_SUDO_WINDOW, true));
}

// 新开节点标签的默认模式:0 终端 / 1 SFTP / 2 服务器(顺序同标题栏右上角的模式切换)
num_pref!(default_mode, set_default_mode, DEFAULT_MODE, KEY_DEFAULT_MODE, "default_node_mode", 0, 0, 2);

pub fn default_node_mode() -> crate::workspace::NodeMode {
    match default_mode() {
        1 => crate::workspace::NodeMode::Files,
        2 => crate::workspace::NodeMode::Containers,
        _ => crate::workspace::NodeMode::Terminal,
    }
}
