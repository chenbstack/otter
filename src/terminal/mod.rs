//! 终端:`gpui_xterm` 负责模拟与渲染,这里负责把它接到 SSH 通道上,并画终端标签。

mod connecting;
pub mod ssh;

use gpui_xterm::{ColorPalette, TerminalConfig, TerminalView};

use super::*;
use crate::workspace::{Tab, TabKind};
use ssh::{Conn, Status};

/// 一个服务器终端标签持有的东西。丢弃即断开连接。
pub struct TermSession {
    pub view: Entity<TerminalView>,
    pub handle: ssh::Handle,
    pub conn: Conn,
}

impl Home {
    pub(crate) fn active_term(&self) -> Option<&TermSession> {
        self.workspace.active.and_then(|a| self.workspace.tabs[a].term.as_ref())
    }

    fn start_session(&mut self, conn: Conn, window: &mut Window, cx: &mut Context<Self>) -> TermSession {
        let (inp, out, handle) = ssh::connect(conn.clone(), 80, 24);
        let resizer = handle.resizer();
        let inputter = handle.inputter();
        let cfg = TerminalConfig {
            font_family: MONO.into(),
            font_size: px(TERM_FONT),
            padding: Edges::all(px(TERM_PAD)),
            colors: ColorPalette::builder().background(0x0B, 0x0A, 0x0F).foreground(0xF5, 0xF4, 0xF7).cursor(0x8B, 0x80, 0xFF).build(),
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
        TermSession { view, handle, conn }
    }

    /// 点节点卡片:该节点已有服务器终端就切过去,没有就新开并开始连接。
    pub(crate) fn open_server_terminal(&mut self, node: NodeId, window: &mut Window, cx: &mut Context<Self>) {
        let found = self
            .workspace
            .tabs
            .iter()
            .position(|t| matches!(&t.kind, TabKind::Terminal { node: n, container: None, .. } if *n == node) && t.term.is_some());
        if let Some(ix) = found {
            self.activate(ix);
            let fh = self.workspace.tabs[ix].term.as_ref().map(|t| t.view.read(cx).focus_handle().clone());
            if let Some(fh) = fh {
                window.focus(&fh, cx);
            }
            cx.notify();
            return;
        }
        self.sel = node;
        let conn = Conn::parse(&self.node().user_host);
        let session = self.start_session(conn, window, cx);
        let ix = self.workspace.active.map_or(self.workspace.tabs.len(), |a| a + 1);
        let mut tab = Tab::new(TabKind::Terminal { node, container: None, n: 1 }, crate::files::Files::new());
        tab.term = Some(session);
        self.workspace.tabs.insert(ix, tab);
        self.activate(ix);
        cx.notify();
    }

    /// 重新连接:丢弃旧会话,用同样的连接信息开新会话。
    fn reconnect_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(a) = self.workspace.active else { return };
        let Some(conn) = self.workspace.tabs[a].term.as_ref().map(|t| t.conn.clone()) else { return };
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
            .bg(c::bg())
            .child(div().size_full().child(t.view.clone()))
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
