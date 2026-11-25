//! 窗口位置和大小的记忆:改动后稍等一会儿写进设置,下次启动照着恢复。
//!
//! 存的是 `模式,x,y,宽,高`(模式 `w` 普通、`m` 最大化,最大化时的宽高是还原后的大小)。
//! 全屏不记,按普通窗口存还原大小。保存的位置不在任何屏幕上(拔了外接显示器)时,回到默认大小并居中。记下的大小比最小还小时抬到最小。

use gpui_kit::*;

use crate::ui::{WINDOW_DEFAULT_H, WINDOW_DEFAULT_W, WINDOW_MIN_H, WINDOW_MIN_W};

/// 设置表里的键
pub const KEY: &str = "window_bounds";
/// 「记住窗口位置和大小」开关的键,值 `on` / `off`,缺省为开
pub const KEY_REMEMBER: &str = "window_remember";

static REMEMBER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn remember() -> bool {
    REMEMBER.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_remember(on: bool) {
    REMEMBER.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn encode(b: &WindowBounds) -> String {
    let (m, r) = match b {
        WindowBounds::Windowed(r) | WindowBounds::Fullscreen(r) => ('w', r),
        WindowBounds::Maximized(r) => ('m', r),
    };
    format!("{m},{},{},{},{}", f32::from(r.origin.x), f32::from(r.origin.y), f32::from(r.size.width), f32::from(r.size.height))
}

pub fn decode(s: &str) -> Option<WindowBounds> {
    let mut it = s.split(',');
    let mode = it.next()?;
    let mut n = || it.next()?.trim().parse::<f32>().ok().filter(|v| v.is_finite());
    let (x, y, w, h) = (n()?, n()?, n()?, n()?);
    let r = Bounds::new(point(px(x), px(y)), size(px(w.max(WINDOW_MIN_W)), px(h.max(WINDOW_MIN_H))));
    Some(if mode == "m" { WindowBounds::Maximized(r) } else { WindowBounds::Windowed(r) })
}

/// 启动时用的窗口位置:有记忆且还落在某块屏幕上就用它,否则默认大小居中
pub fn initial(saved: Option<&str>, cx: &App) -> WindowBounds {
    let default = || WindowBounds::Windowed(Bounds::centered(None, size(px(WINDOW_DEFAULT_W), px(WINDOW_DEFAULT_H)), cx));
    let Some(b) = saved.and_then(decode) else { return default() };
    let r = match &b {
        WindowBounds::Windowed(r) | WindowBounds::Maximized(r) | WindowBounds::Fullscreen(r) => *r,
    };
    // 窗口至少有 200×100 在某块屏幕里才算可见
    let visible = cx.displays().iter().any(|d| {
        let i = d.bounds().intersect(&r);
        f32::from(i.size.width) >= 200.0 && f32::from(i.size.height) >= 100.0
    });
    if visible { b } else { default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn roundtrip_and_min_size() {
        let b = WindowBounds::Windowed(Bounds::new(point(px(10.0), px(20.0)), size(px(1400.0), px(900.0))));
        assert_eq!(decode(&encode(&b)), Some(b));
        let m = WindowBounds::Maximized(Bounds::new(point(px(0.0), px(0.0)), size(px(1400.0), px(900.0))));
        assert_eq!(decode(&encode(&m)), Some(m));
        // 记下的大小比最小还小(旧版本、手改)时抬到最小
        let Some(WindowBounds::Windowed(r)) = decode("w,0,0,100,100") else { panic!() };
        assert_eq!((f32::from(r.size.width), f32::from(r.size.height)), (WINDOW_MIN_W, WINDOW_MIN_H));
        assert_eq!(decode("garbage"), None);
        assert_eq!(decode("w,1,2,x,4"), None);
    }
}
