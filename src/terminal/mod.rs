//! 终端:`gpui_xterm` 负责模拟与渲染,这里负责把它接到 SSH 通道上,并画终端标签。

pub(crate) mod connecting;
pub mod highlight;
pub mod ssh;

use ::gpui::{Animation, AnimationExt as _, Transformation, ease_out_quint, percentage};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui_xterm::{ColorPalette, TerminalConfig, TerminalView};

use super::*;
use ssh::{Conn, Status};

const OVERLAY_SPIN_MS: u64 = 900;
const OVERLAY_FADE_MS: u64 = 220;
const OVERLAY_SCRIM: f32 = 0.62;
const OVERLAY_PILL_H: f32 = 44.0;
const BADGE_H: f32 = 32.0;

/// 一个服务器终端标签持有的东西。丢弃即断开连接。
pub struct TermSession {
    pub view: Entity<TerminalView>,
    pub handle: ssh::Handle,
    pub conn: Conn,
    /// 服务器是否开了括号粘贴模式(`ESC[?2004h`),粘贴时据此决定要不要包标记
    pub bracketed_paste: Arc<AtomicBool>,
}

/// 透传 SSH 输出给终端模拟器,同时盯着 `ESC[?2004h/l`,记下括号粘贴模式的开关。
struct PasteModeScan<R> {
    inner: R,
    flag: Arc<AtomicBool>,
    /// 上一块的尾部,防止转义序列被拆在两次读取之间
    tail: Vec<u8>,
}

impl<R: std::io::Read> std::io::Read for PasteModeScan<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let mut hay = std::mem::take(&mut self.tail);
        hay.extend_from_slice(&buf[..n]);
        let find = |pat: &[u8]| hay.windows(pat.len()).rposition(|w| w == pat);
        match (find(b"\x1b[?2004h"), find(b"\x1b[?2004l")) {
            (Some(h), Some(l)) => self.flag.store(h > l, Ordering::Relaxed),
            (Some(_), None) => self.flag.store(true, Ordering::Relaxed),
            (None, Some(_)) => self.flag.store(false, Ordering::Relaxed),
            _ => {}
        }
        let keep = hay.len().saturating_sub(7);
        self.tail = hay.split_off(keep);
        Ok(n)
    }
}

/// 把剪贴板文字整理成可以写进 PTY 的字节:换行统一成回车;服务器开了括号粘贴就包上标记,并去掉文字里自带的结束标记。
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if !bracketed {
        return text.into_bytes();
    }
    // 去掉所有控制字符(含 ESC 和 C1),只留回车和 Tab:只删结束标记会被 `ESC[2ESC[201~01~` 这种拼接绕过
    let text: String = text.chars().filter(|&c| c == '\r' || c == '\t' || !c.is_control()).collect();
    let mut out = b"\x1b[200~".to_vec();
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

impl Home {
    /// 连接中断时盖在整个标签内容区上的状态层:能透过它隐约看到后面的内容。
    /// - 自动重连中(所有模式):转圈 + 提示;
    /// - 重连放弃了(非终端模式;终端模式自己有「重新连接」按钮):提示 + 重新连接。
    /// GPUI 没有背景模糊,这里用半透明遮罩压暗后面的内容代替。
    pub(crate) fn reconnect_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let a = self.workspace.active?;
        let tab = &self.workspace.tabs[a];
        if !matches!(tab.kind, workspace::TabKind::Node(_)) {
            return None;
        }
        // 调试:DOCKYARD_FAKE_RECONNECT=1 / =lost 在没有真实终端时也画出遮罩(截图用)
        let fake = std::env::var("DOCKYARD_FAKE_RECONNECT").ok();
        let (status, ever) = match (tab.term.as_ref(), fake.as_deref()) {
            (Some(t), _) => (t.handle.status(), t.handle.ever_connected()),
            (None, Some("lost")) => (Status::Closed(Some("连接中断".into())), true),
            (None, Some(_)) => (Status::Reconnecting(2), true),
            (None, None) => return None,
        };
        let (title, sub, lost) = match status {
            Status::Reconnecting(n) => ("连接中断,正在重连".to_string(), format!("第 {n} 次尝试"), false),
            Status::Closed(Some(e)) if ever && tab.mode != workspace::NodeMode::Terminal => ("连接已断开".to_string(), e, true),
            _ => return None,
        };
        let glyph: AnyElement = if lost {
            icon(IconName::WifiOff, 16.0, c::warn()).into_any_element()
        } else {
            icon(IconName::LoaderCircle, 16.0, c::accent())
                .with_animation("recon-spin", Animation::new(std::time::Duration::from_millis(OVERLAY_SPIN_MS)).repeat(), |i, t| i.transform(Transformation::rotate(percentage(t))))
                .into_any_element()
        };
        // 自动重连中:不遮挡内容,只在右下角显示一个小状态,重连完自动消失
        if !lost {
            return Some(
                div()
                    .id("reconnect-badge")
                    .absolute()
                    .right(px(16.0))
                    .bottom(px(16.0))
                    .child(
                        div().rounded(px(BADGE_H / 2.0)).bg(c::bg()).child(
                            div()
                                .h(px(BADGE_H))
                                .px(px(12.0))
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .glass(BADGE_H / 2.0, GlassStyle::CARD)
                                .child(glyph)
                                .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::text()).child(format!("{title} · {sub}"))),
                        ),
                    )
                    .into_any_element(),
            );
        }
        // 一条紧凑的胶囊:图标 + 一行文字 + 重新连接,居中压在压暗的内容上
        Some(
            div()
                .id("reconnect-overlay")
                .absolute()
                .inset_0()
                .occlude()
                .bg(c::bg().opacity(OVERLAY_SCRIM))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    // 节点卡片的底是半透明的,先垫一层不透明底色,免得后面的文字透上来
                    div().rounded(px(OVERLAY_PILL_H / 2.0)).bg(c::bg()).child(
                    div()
                        .h(px(OVERLAY_PILL_H))
                        .pl(px(16.0))
                        .pr(if lost { px(7.0) } else { px(18.0) })
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .glass(OVERLAY_PILL_H / 2.0, GlassStyle::CARD)
                        .child(glyph)
                        .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).text_color(c::text()).child(title))
                        .child(div().max_w(px(320.0)).truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(sub))
                        .when(lost, |d| {
                            d.child(
                                div()
                                    .id("overlay-reconnect")
                                    .ml(px(4.0))
                                    .h(px(30.0))
                                    .px(px(12.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .rounded_full()
                                    .bg(c::accent())
                                    .text_color(c::on_accent())
                                    .text_size(px(t::TYPE_CONTROL))
                                    .font_weight(FontWeight::MEDIUM)
                                    .cursor_pointer()
                                    .hover(|s| s.opacity(0.88))
                                    .child(icon(IconName::RotateCw, 13.0, c::on_accent()))
                                    .child("重新连接")
                                    .on_click(cx.listener(|this, _, window, cx| this.reconnect_terminal(window, cx))),
                            )
                        }),
                    ),
                )
                .with_animation("recon-fade", Animation::new(std::time::Duration::from_millis(OVERLAY_FADE_MS)).with_easing(ease_out_quint()), |d, t| d.opacity(t))
                .into_any_element(),
        )
    }

    pub(crate) fn active_term(&self) -> Option<&TermSession> {
        self.workspace.active.and_then(|a| self.workspace.tabs[a].term.as_ref())
    }

    pub(crate) fn start_session(&mut self, conn: Conn, window: &mut Window, cx: &mut Context<Self>) -> TermSession {
        let (inp, out, handle) = ssh::connect(conn.clone(), 80, 24);
        let bracketed_paste = Arc::new(AtomicBool::new(false));
        let out = PasteModeScan { inner: out, flag: bracketed_paste.clone(), tail: Vec::new() };
        let resizer = handle.resizer();
        let inputter = handle.inputter();
        let cfg = TerminalConfig {
            font_family: MONO.into(),
            font_size: px(TERM_FONT),
            padding: Edges::all(px(TERM_PAD)),
            colors: term_palette(),
            highlighter: Some(highlight::highlighter()),
            ..Default::default()
        };
        let view = cx.new(|cx| {
            TerminalView::new(inp, out, cfg, cx)
                .with_resize_callback(move |c, r| {
                    let _ = resizer.send((c as u16, r as u16));
                })
                // gpui_xterm 把“带字符的按键”都先交给输入法,而回车、Tab 不会经输入法写回来,这里直接发
                .with_key_handler(move |ev| {
                    let m = &ev.keystroke.modifiers;
                    if m.platform || m.control {
                        return false;
                    }
                    let bytes: &[u8] = match (ev.keystroke.key.as_str(), m.shift, m.alt) {
                        ("enter", _, false) => b"\r",
                        ("enter", _, true) => b"\x1b\r",
                        ("tab", false, false) => b"\t",
                        ("tab", true, false) => b"\x1b[Z",
                        _ => return false,
                    };
                    let _ = inputter.send(bytes.to_vec());
                    true
                })
        });
        let fh = view.read(cx).focus_handle().clone();
        window.focus(&fh, cx);
        // 连接状态在后台线程里变化,界面这边轮询一下,变了就重绘工具栏
        let status = handle.status.clone();
        let steps = handle.steps.clone();
        cx.spawn(async move |this, cx| {
            let mut last: Option<(Status, Vec<ssh::Step>)> = None;
            let mut log_ver = crate::logs::version();
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(300)).await;
                let now = status.lock().map(|s| s.clone()).unwrap_or(Status::Closed(None));
                let ver = crate::logs::version();
                let snap = (now.clone(), steps.lock().map(|s| s.clone()).unwrap_or_default());
                if last.as_ref() != Some(&snap) || ver != log_ver {
                    last = Some(snap);
                    log_ver = ver;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
                if matches!(now, Status::Closed(_)) {
                    break;
                }
            }
        })
        .detach();
        TermSession { view, handle, conn, bracketed_paste }
    }

    /// 重新连接:丢弃旧会话,用同样的连接信息开新会话。
    fn reconnect_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(a) = self.workspace.active else { return };
        let Some(old) = self.workspace.tabs[a].term.as_ref().map(|t| t.conn.clone()) else { return };
        // 节点可能刚被编辑过,重新从库里读认证信息
        let conn = match self.workspace.tabs[a].node() {
            Some(id) => self.conn_for(id),
            None => old,
        };
        self.workspace.tabs[a].term = None;
        let session = self.start_session(conn, window, cx);
        self.workspace.tabs[a].term = Some(session);
        cx.notify();
    }

    /// 终端标签的内容:只有终端画布。连接状态看标签上的图标颜色;断开后右上角浮出「重连」。
    pub(crate) fn ssh_terminal_view(&self, cx: &mut Context<Self>) -> Div {
        let Some(t) = self.active_term() else { return div() };
        let status = t.handle.status();
        // 没连上之前显示「连接中」页(失败也留在这页);要在终端里输入时(指纹确认、密码)切到终端画布
        if !t.handle.ever_connected() && status != Status::Waiting {
            return self.connecting_view(t.handle.steps(), cx);
        }
        let closed = matches!(status, Status::Closed(_));
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .relative()
            .child({
                // gpui_xterm 只认 Ctrl+C/V,macOS 上的 Cmd+C/V 在这里接管;粘贴自己写,才能支持括号粘贴
                let view = t.view.clone();
                let inputter = t.handle.inputter();
                let bracketed = t.bracketed_paste.clone();
                div().size_full().child(t.view.clone()).capture_key_down(move |ev, _, cx| {
                    let m = &ev.keystroke.modifiers;
                    if !m.platform || m.control || m.alt {
                        return;
                    }
                    match ev.keystroke.key.as_str() {
                        "v" => {
                            if let Ok(text) = gpui_xterm::Clipboard::new().and_then(|mut c| c.paste()) {
                                let _ = inputter.send(paste_bytes(&text, bracketed.load(Ordering::Relaxed)));
                            }
                            cx.stop_propagation();
                        }
                        "c" => {
                            if view.read(cx).copy_selection() {
                                cx.stop_propagation();
                            }
                        }
                        _ => {}
                    }
                })
                // Cmd+Shift+H:开关关键字高亮并记住。目前还没有设置页,先用快捷键
                .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                    let m = &ev.keystroke.modifiers;
                    if !(m.platform && m.shift) || m.control || m.alt || ev.keystroke.key != "h" {
                        return;
                    }
                    let on = !highlight::enabled();
                    highlight::set_enabled(on);
                    if let Some(st) = this.store.as_ref() {
                        let _ = st.set_setting(highlight::SETTING_KEY, if on { "on" } else { "off" });
                    }
                    if let Some(view) = this.active_term().map(|t| t.view.clone()) {
                        view.update(cx, |_, cx| cx.notify());
                    }
                    cx.stop_propagation();
                }))
            })
            .when(closed, |d| {
                d.child(
                    div()
                        .id("term-reconnect")
                        .absolute()
                        .top(px(TERM_PAD))
                        .right(px(CONTENT_PAD))
                        .h(px(28.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(t::RADIUS_MD))
                        .bg(c::accent())
                        .text_color(c::on_accent())
                        .text_size(px(t::TYPE_CONTROL))
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.88))
                        .child(icon(IconName::RotateCw, 13.0, c::on_accent()))
                        .child("重新连接")
                        .on_click(cx.listener(|this, _, window, cx| this.reconnect_terminal(window, cx))),
                )
            })
    }
}

fn term_palette() -> ColorPalette {
    let a = crate::ui::TERM_ANSI;
    let b = ColorPalette::builder().background(0x0B, 0x0A, 0x0F).background_alpha(0.0).foreground(crate::ui::TERM_FG.0, crate::ui::TERM_FG.1, crate::ui::TERM_FG.2).cursor(0x8B, 0x80, 0xFF);
    let b = b.black(a[0].0, a[0].1, a[0].2).red(a[1].0, a[1].1, a[1].2).green(a[2].0, a[2].1, a[2].2).yellow(a[3].0, a[3].1, a[3].2);
    let b = b.blue(a[4].0, a[4].1, a[4].2).magenta(a[5].0, a[5].1, a[5].2).cyan(a[6].0, a[6].1, a[6].2).white(a[7].0, a[7].1, a[7].2);
    let b = b.bright_black(a[8].0, a[8].1, a[8].2).bright_red(a[9].0, a[9].1, a[9].2).bright_green(a[10].0, a[10].1, a[10].2).bright_yellow(a[11].0, a[11].1, a[11].2);
    b.bright_blue(a[12].0, a[12].1, a[12].2).bright_magenta(a[13].0, a[13].1, a[13].2).bright_cyan(a[14].0, a[14].1, a[14].2).bright_white(a[15].0, a[15].1, a[15].2).build()
}

#[cfg(test)]
mod paste_tests {
    // 不能 `use super::*`:它会带进 gpui 的 `test` 属性宏,盖掉标准的 `#[test]`
    use super::{PasteModeScan, paste_bytes};
    use std::io::Read;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn paste_normalizes_newlines_and_brackets() {
        assert_eq!(paste_bytes("a\nb\r\nc", false), b"a\rb\rc");
        assert_eq!(paste_bytes("a\nb", true), b"\x1b[200~a\rb\x1b[201~");
        assert_eq!(paste_bytes("x\x1b[201~rm", true), b"\x1b[200~x[201~rm\x1b[201~");
        // 拼接绕过:去掉 ESC 后不会再出现结束标记
        let out = paste_bytes("a\x1b[2\x1b[201~01~b\u{9b}c\x07", true);
        assert_eq!(out, b"\x1b[200~a[2[201~01~bc\x1b[201~");
        assert_eq!(out.windows(6).filter(|w| *w == b"\x1b[201~").count(), 1);
    }

    #[test]
    fn scan_detects_mode_split_across_reads() {
        let flag = Arc::new(AtomicBool::new(false));
        let chunks: Vec<&[u8]> = vec![b"hi \x1b[?20", b"04h$ ", b"\x1b[?2004l"];
        struct Seq(std::vec::IntoIter<&'static [u8]>);
        impl Read for Seq {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let c = self.0.next().unwrap_or(b"");
                buf[..c.len()].copy_from_slice(c);
                Ok(c.len())
            }
        }
        let leaked: Vec<&'static [u8]> = chunks.into_iter().map(|c| &*Box::leak(c.to_vec().into_boxed_slice())).collect();
        let mut s = PasteModeScan { inner: Seq(leaked.into_iter()), flag: flag.clone(), tail: Vec::new() };
        let mut buf = [0u8; 64];
        s.read(&mut buf).unwrap();
        assert!(!flag.load(Ordering::Relaxed));
        s.read(&mut buf).unwrap();
        assert!(flag.load(Ordering::Relaxed));
        s.read(&mut buf).unwrap();
        assert!(!flag.load(Ordering::Relaxed));
    }
}
