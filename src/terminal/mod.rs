//! 终端:`gpui_xterm` 负责模拟与渲染,这里负责把它接到 SSH 通道上,并画终端标签。

pub mod ssh;

use gpui_xterm::{TerminalConfig, TerminalView};

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
        let cfg = TerminalConfig {
            font_family: MONO.into(),
            font_size: px(TERM_FONT),
            padding: Edges::all(px(TERM_PAD)),
            ..Default::default()
        };
        let view = cx.new(|cx| {
            TerminalView::new(inp, out, cfg, cx).with_resize_callback(move |c, r| {
                let _ = resizer.send((c as u16, r as u16));
            })
        });
        let fh = view.read(cx).focus_handle().clone();
        window.focus(&fh, cx);
        // 连接状态在后台线程里变化,界面这边轮询一下,变了就重绘工具栏
        let status = handle.status.clone();
        cx.spawn(async move |this, cx| {
            let mut last = None;
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(300)).await;
                let now = status.lock().map(|s| s.clone()).unwrap_or(Status::Closed(None));
                if last.as_ref() != Some(&now) {
                    last = Some(now.clone());
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

    pub(crate) fn ssh_terminal_view(&self, cx: &mut Context<Self>) -> Div {
        let Some(t) = self.active_term() else { return div() };
        let status = t.handle.status();
        let (dot, label) = match &status {
            Status::Connecting => (c::warn(), "连接中".to_string()),
            Status::Waiting => (c::warn(), "等待输入".to_string()),
            Status::Connected => (c::online(), "已连接".to_string()),
            Status::Closed(Some(why)) => (c::danger(), format!("已断开:{why}")),
            Status::Closed(None) => (c::ghost(), "已断开".to_string()),
        };
        let closed = matches!(status, Status::Closed(_));
        let node = self.node();
        let id = node.id;
        let btn = |id: &'static str, text: &'static str, ic: IconName| {
            div()
                .id(id)
                .h(px(24.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(5.0))
                .rounded(px(t::RADIUS_SM))
                .text_size(px(t::TYPE_CAPTION))
                .text_color(c::muted())
                .cursor_pointer()
                .hover(|s| s.bg(c::hover()).text_color(c::text()))
                .child(icon(ic, 13.0, c::muted()))
                .child(text)
        };
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(TERM_BAR_H))
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(CONTENT_PAD))
                    .border_b_1()
                    .border_color(c::border())
                    .child(div().w(px(7.0)).h(px(7.0)).rounded_full().bg(dot))
                    .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(node.name.clone()))
                    .child(div().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::ghost()).child(node.user_host.clone()))
                    .child(div().truncate().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(label))
                    .child(div().flex_1())
                    .when(closed, |d| {
                        d.child(btn("term-reconnect", "重连", IconName::RotateCw).on_click(cx.listener(|this, _, window, cx| this.reconnect_terminal(window, cx))))
                    })
                    .child(btn("term-manage", "管理", IconName::LayoutGrid).on_click(cx.listener(move |this, _, _, cx| {
                        this.open_node(id);
                        cx.notify();
                    }))),
            )
            .child(div().flex_1().min_h_0().bg(c::sunken()).child(t.view.clone()))
    }
}
