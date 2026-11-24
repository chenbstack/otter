//! 终端:`gpui_xterm` 负责模拟与渲染,这里负责把它接到 SSH 通道上,并画终端标签。

pub(crate) mod connecting;
pub mod highlight;
pub mod ssh;

use ::gpui::{Animation, AnimationExt as _, Transformation, percentage};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui_xterm::{ColorPalette, TerminalConfig, TerminalView};

use super::*;
use ssh::{Conn, Status};

const OVERLAY_SPIN_MS: u64 = 900;
const BADGE_H: f32 = 32.0;

/// 一个服务器终端标签持有的东西。丢弃即断开连接。
pub struct TermSession {
    pub view: Entity<TerminalView>,
    pub handle: ssh::Handle,
    pub conn: Conn,
    /// 服务器是否开了括号粘贴模式(`ESC[?2004h`),粘贴时据此决定要不要包标记
    pub bracketed_paste: Arc<AtomicBool>,
    /// 终端输出活跃度,标签栏据此显示「正在执行」
    pub activity: Arc<Activity>,
    /// 用户关掉了「连接已断开」浮层(之后只剩右上角的「重新连接」)
    pub overlay_dismissed: bool,
}

/// 终端的「正在执行」状态,标签栏据此转圈。
/// - 用户(或 agent)按下回车提交命令 → 立刻开始;**起步转得快、之后逐渐放慢**(`lap_phase`),
///   所以 `ls` 这种瞬间结束的命令一眨眼转完一圈,长任务则是从容的慢速;
/// - 忙的期间每来一块输出就顺延 `BUSY_GAP_MS`,输出停了才结束;
/// - 至少转满第一圈(快速的那一圈),再短的命令也看得见;
/// - 没提交过命令但冒出一阵持续输出(累计 ≥ `BUSY_BYTES`,如后台任务、`tail -f`)也算。
///
/// 敲键盘的回显只有几个字节,且不在忙的时候不会触发。没有输出的长命令(`sleep 100`)
/// 只会转一圈就停,检测不到它还在跑。
#[derive(Default)]
pub struct Activity {
    run_until_ms: std::sync::atomic::AtomicU64,
    busy_since_ms: std::sync::atomic::AtomicU64,
    last_ms: std::sync::atomic::AtomicU64,
    burst_start_ms: std::sync::atomic::AtomicU64,
    burst_bytes: std::sync::atomic::AtomicU64,
    /// 提交命令后的一小段时间里,第一块输出(哪怕很少)就算命令开始出结果,重新开始忙;用掉一次就清零
    armed_until_ms: std::sync::atomic::AtomicU64,
}

/// 提交命令后多久内,够大的一块输出可以重新触发忙碌(应对先静默一阵才出结果、或输出稀疏的命令);每次触发都续期
pub const ARM_MS: u64 = 10_000;
/// 重新触发要求的最小输出块:敲键盘的回显只有 1~3 字节,命令输出一般更大
pub const REARM_MIN_BYTES: usize = 16;

/// 忙的期间输出停多久就算结束
pub const BUSY_GAP_MS: u64 = 600;
/// 判断「一阵持续输出」时,两块输出间隔超过它就重新计字节
const BURST_GAP_MS: u64 = 1200;
/// 没提交命令时,一阵输出累计多少字节才算在执行
pub const BUSY_BYTES: u64 = 512;

// 转速(圈/秒):起步 `V_FAST`,按时间常数 `TAU` 指数衰减到 `V_SLOW`
const V_SLOW: f64 = 1.0 / 1.8;
const V_FAST: f64 = 2.2;
const TAU: f64 = 0.55;

/// 开始后 `secs` 秒时转过的圈数(速度从快到慢的积分)。
pub fn lap_phase(secs: f64) -> f64 {
    V_SLOW * secs + (V_FAST - V_SLOW) * TAU * (1.0 - (-secs / TAU).exp())
}

/// 转到 `phase` 圈需要的秒数(`lap_phase` 的反函数,二分求解)。
fn lap_time(phase: f64) -> f64 {
    let (mut lo, mut hi) = (0.0, phase / V_SLOW + 1.0);
    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        if lap_phase(mid) < phase { lo = mid } else { hi = mid }
    }
    hi
}

fn now_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + BUSY_GAP_MS + 1
}

impl Activity {
    pub fn note(&self, n: usize) {
        self.note_at(now_ms(), n);
    }
    /// 写进终端的字节:含回车就算提交了一条命令
    pub fn on_input(&self, bytes: &[u8]) {
        if bytes.iter().any(|b| *b == b'\r' || *b == b'\n') {
            let now = now_ms();
            self.start_at(now);
            self.armed_until_ms.store(now + ARM_MS, Ordering::Relaxed);
        }
    }
    pub fn busy(&self) -> bool {
        self.busy_at(now_ms())
    }
    /// 本次忙碌已经持续了多久(毫秒);不忙时为 `None`。动画用它保证每次都从头转起。
    pub fn elapsed_ms(&self) -> Option<u64> {
        let now = now_ms();
        self.busy_at(now).then(|| now.saturating_sub(self.busy_since_ms.load(Ordering::Relaxed)))
    }
    /// 忙到什么时候:最后一块输出之后再等 `BUSY_GAP_MS`,但至少转满第一圈(快速的那一圈)
    fn end_ms(&self) -> u64 {
        let since = self.busy_since_ms.load(Ordering::Relaxed);
        let until = self.run_until_ms.load(Ordering::Relaxed);
        until.max(since + (lap_time(1.0) * 1000.0).ceil() as u64)
    }
    fn busy_at(&self, now: u64) -> bool {
        now < self.end_ms()
    }
    /// 开始忙:已经在忙就不动(不重新起转);否则重新计时
    fn start_at(&self, now: u64) {
        if !self.busy_at(now) {
            self.busy_since_ms.store(now, Ordering::Relaxed);
            self.run_until_ms.store(now, Ordering::Relaxed);
        }
    }
    fn extend_at(&self, now: u64) {
        self.run_until_ms.fetch_max(now + BUSY_GAP_MS, Ordering::Relaxed);
    }
    fn note_at(&self, now: u64, n: usize) {
        let last = self.last_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) > BURST_GAP_MS {
            self.burst_start_ms.store(now, Ordering::Relaxed);
            self.burst_bytes.store(0, Ordering::Relaxed);
        }
        let bytes = self.burst_bytes.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        self.last_ms.store(now, Ordering::Relaxed);
        let armed = self.armed_until_ms.load(Ordering::Relaxed) > now;
        if self.busy_at(now) {
            self.extend_at(now);
            if armed {
                self.armed_until_ms.store(now + ARM_MS, Ordering::Relaxed);
            }
        } else if (armed && n >= REARM_MIN_BYTES) || bytes >= BUSY_BYTES {
            self.start_at(now);
            self.extend_at(now);
            self.armed_until_ms.store(now + ARM_MS, Ordering::Relaxed);
        }
    }
}

/// 透传 SSH 输出给终端模拟器,同时盯着 `ESC[?2004h/l`,记下括号粘贴模式的开关。
struct PasteModeScan<R> {
    inner: R,
    flag: Arc<AtomicBool>,
    activity: Arc<Activity>,
    /// 上一块的尾部,防止转义序列被拆在两次读取之间
    tail: Vec<u8>,
}

impl<R: std::io::Read> std::io::Read for PasteModeScan<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.activity.note(n);
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
    /// - 重连放弃了:一张卡片,写明原因,「重试」和(终端模式)「关闭」。
    /// GPUI 没有背景模糊,这里用半透明遮罩压暗后面的内容代替。
    pub(crate) fn reconnect_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let a = self.workspace.active?;
        let tab = &self.workspace.tabs[a];
        if !matches!(tab.kind, workspace::TabKind::Node(_)) {
            return None;
        }
        // 调试:OTTER_FAKE_RECONNECT=1 / =lost 在没有真实终端时也画出遮罩(截图用)
        let fake = std::env::var("OTTER_FAKE_RECONNECT").ok();
        let (status, ever) = match (tab.term.as_ref(), fake.as_deref()) {
            (_, Some("lost")) => (Status::Closed(Some("已自动重试 5 次,仍然连不上:连接超时".into())), true),
            (_, Some(_)) => (Status::Reconnecting(2), true),
            (Some(t), None) => (t.handle.status(), t.handle.ever_connected()),
            (None, None) => return None,
        };
        let (title, sub, lost) = match status {
            Status::Reconnecting(n) => ("连接中断,正在重连".to_string(), format!("第 {n} 次尝试"), false),
            Status::Closed(Some(e)) if ever && !tab.term.as_ref().is_some_and(|t| t.overlay_dismissed) => ("连接已断开".to_string(), e, true),
            _ => return None,
        };
        // 终端模式里关掉浮层后还能看历史输出,右上角仍有「重新连接」;其它模式没有别的入口,所以不给关闭
        let can_dismiss = tab.mode == workspace::NodeMode::Terminal;
        let glyph: AnyElement = if lost {
            icon(IconName::WifiOff, 24.0, c::warn()).into_any_element()
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
        // 样式和接管确认框共用 ui::alert_* 部件
        let hero = crate::ui::alert_hero(IconName::WifiOff, c::warn(), title, "网络可能不稳定,或服务器暂时不可达");
        let target = tab.node().and_then(|id| self.all_nodes().find(|n| n.id == id)).map(Self::node_target_card);
        let mut rows: Vec<(IconName, Rgba, String)> = vec![(IconName::WifiOff, c::warn(), sub)];
        if can_dismiss {
            rows.push((IconName::ScanEye, c::muted(), "关闭后仍可查看终端里已有的内容".into()));
        }
        let btn_w = crate::ui::alert_btn_w(if can_dismiss { 2 } else { 1 });
        let retry = crate::ui::alert_primary_btn("overlay-reconnect", Some(IconName::RotateCw), "重试", can_dismiss.then_some(btn_w)).on_click(cx.listener(|this, _, window, cx| this.reconnect_terminal(window, cx)));
        let close = crate::node_form::ctl_btn("overlay-dismiss", None, "关闭", 38.0).w(px(btn_w)).flex_none().justify_center().on_click(cx.listener(|this, _, _, cx| this.dismiss_lost_overlay(cx)));
        let buttons = div().w_full().flex().flex_row().gap(px(10.0)).when(can_dismiss, |d| d.child(close)).child(retry);
        let body = crate::ui::alert_body(hero, target, crate::ui::alert_rows(rows), buttons);
        Some(
            crate::ui::modal_glass(
                "reconnect-overlay",
                crate::confirm::CONFIRM_W,
                cx.listener(move |this, e: &KeyDownEvent, window, cx| {
                    match e.keystroke.key.as_str() {
                        "escape" if can_dismiss => this.dismiss_lost_overlay(cx),
                        "enter" => this.reconnect_terminal(window, cx),
                        _ => return,
                    }
                    cx.stop_propagation();
                }),
                [body.into_any_element()],
            )
            .into_any_element(),
        )
    }

    /// 关掉「连接已断开」浮层(终端里的内容和右上角的「重新连接」还在)。
    fn dismiss_lost_overlay(&mut self, cx: &mut Context<Self>) {
        if let Some(t) = self.workspace.active.and_then(|a| self.workspace.tabs[a].term.as_mut()) {
            t.overlay_dismissed = true;
        }
        cx.notify();
    }

    pub(crate) fn active_term(&self) -> Option<&TermSession> {
        self.workspace.active.and_then(|a| self.workspace.tabs[a].term.as_ref())
    }

    pub(crate) fn start_session(&mut self, conn: Conn, window: &mut Window, cx: &mut Context<Self>) -> TermSession {
        let activity = Arc::new(Activity::default());
        let (inp, out, handle) = ssh::connect(conn.clone(), 80, 24, activity.clone());
        let bracketed_paste = Arc::new(AtomicBool::new(false));
        let out = PasteModeScan { inner: out, flag: bracketed_paste.clone(), activity: activity.clone(), tail: Vec::new() };
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
        TermSession { view, handle, conn, bracketed_paste, activity, overlay_dismissed: false }
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
                // gpui_xterm 只认 Ctrl+C/V,macOS 上的 Cmd+C/V、其它平台的 Ctrl+Shift+C/V 在这里接管;粘贴自己写,才能支持括号粘贴
                let view = t.view.clone();
                let inputter = t.handle.inputter();
                let bracketed = t.bracketed_paste.clone();
                div().size_full().child(t.view.clone()).capture_key_down(move |ev, _, cx| {
                    let m = &ev.keystroke.modifiers;
                    if !crate::platform::accel(m) {
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
                    if !crate::platform::accel_shift(m) || ev.keystroke.key != "h" {
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
        let mut s = PasteModeScan { inner: Seq(leaked.into_iter()), flag: flag.clone(), activity: Default::default(), tail: Vec::new() };
        let mut buf = [0u8; 64];
        s.read(&mut buf).unwrap();
        assert!(!flag.load(Ordering::Relaxed));
        s.read(&mut buf).unwrap();
        assert!(flag.load(Ordering::Relaxed));
        s.read(&mut buf).unwrap();
        assert!(!flag.load(Ordering::Relaxed));
    }
}

#[cfg(test)]
mod activity_tests {
    use super::{Activity, lap_phase, lap_time};
    use std::sync::atomic::Ordering;

    /// 第一圈转完的时刻(毫秒)
    fn first_lap_ms() -> u64 {
        (lap_time(1.0) * 1000.0) as u64
    }

    #[test]
    fn speed_starts_fast_and_settles() {
        assert!(lap_time(1.0) < 0.9, "第一圈很快: {}", lap_time(1.0));
        let late = lap_time(11.0) - lap_time(10.0);
        assert!((late - 1.8).abs() < 0.05, "稳态一圈约 1.8 秒: {late}");
        assert!((lap_phase(lap_time(3.0)) - 3.0).abs() < 1e-6, "反函数");
    }

    #[test]
    fn typing_echo_alone_is_not_busy() {
        let a = Activity::default();
        for i in 0..20 {
            a.note_at(10_000 + i * 100, 3);
        }
        assert!(!a.busy_at(12_000));
    }

    #[test]
    fn a_silent_command_spins_exactly_one_fast_lap() {
        let a = Activity::default();
        a.start_at(10_000);
        assert!(a.busy_at(10_000 + first_lap_ms() - 30));
        assert!(!a.busy_at(10_000 + first_lap_ms() + 30));
    }

    #[test]
    fn a_short_command_with_output_stops_soon_after_its_output() {
        let a = Activity::default();
        a.start_at(10_000); // ls
        a.armed_until_ms.store(10_000 + super::ARM_MS, Ordering::Relaxed);
        a.note_at(10_030, 40);
        assert!(a.busy_at(10_000 + first_lap_ms() - 30), "至少转满第一圈");
        assert!(!a.busy_at(10_000 + first_lap_ms() + 30), "ls 这种瞬间出完结果的命令,恰好转一圈就停");
    }

    #[test]
    fn sparse_output_pulses_once_per_chunk_and_fast_output_stays_on() {
        let a = Activity::default();
        a.start_at(10_000); // ping,每秒一行
        a.armed_until_ms.store(10_000 + super::ARM_MS, Ordering::Relaxed);
        for i in 1..=10 {
            a.note_at(10_000 + i * 1000, 64);
        }
        assert!(a.busy_at(20_000 + 300), "每来一行就转一圈");
        assert!(!a.busy_at(20_000 + 1000 + first_lap_ms()), "输出停了就结束");
        let b = Activity::default();
        b.start_at(10_000);
        b.armed_until_ms.store(10_000 + super::ARM_MS, Ordering::Relaxed);
        for i in 0..30 {
            b.note_at(10_000 + i * 300, 64); // 输出密集:一直在忙
        }
        assert!(b.busy_at(10_000 + 29 * 300 + 500));
    }

    #[test]
    fn typing_echo_after_a_command_does_not_retrigger() {
        let a = Activity::default();
        a.start_at(10_000);
        a.armed_until_ms.store(10_000 + super::ARM_MS, Ordering::Relaxed);
        a.note_at(10_030, 40); // 命令输出
        for i in 0..10 {
            a.note_at(13_000 + i * 120, 2); // 下一条命令的键入回显
        }
        assert!(!a.busy_at(14_500));
    }

    #[test]
    fn a_big_burst_without_enter_counts_and_restarts_the_lap() {
        let a = Activity::default();
        a.note_at(10_000, 400);
        assert!(!a.busy_at(10_100));
        a.note_at(10_300, 400);
        assert!(a.busy_at(10_400));
        assert_eq!(a.busy_since_ms.load(Ordering::Relaxed), 10_300);
    }
}
