//! 标题栏:红绿灯 + 标签(固定的“节点”页标签在最左)。标签是垂直居中的胶囊。

use super::*;
use workspace::TabKind;

impl Home {
    pub(crate) fn titlebar(&self, cx: &mut Context<Self>) -> Div {
        let active = self.workspace.active;

        let tab = |ix: usize, cx: &mut Context<Self>| {
            let tab = &self.workspace.tabs[ix];
            let on = active == Some(ix);
            let (title, kind_icon): (String, IconName) = match tab.node() {
                None => ("节点".into(), IconName::LayoutGrid),
                Some(id) => {
                    let (g, n) = self.locate(id);
                    let node = &self.groups[g].nodes[n];
                    match &tab.kind {
                        TabKind::Files(_) => (format!("{} · 文件", node.name), IconName::FolderOpen),
                        TabKind::Terminal { container, n, .. } => {
                            let who = container.as_ref().map_or(node.name.clone(), |(_, svc)| svc.clone());
                            let title = if *n > 1 { format!("{who} · 终端 {n}") } else { format!("{who} · 终端") };
                            (title, IconName::SquareTerminal)
                        }
                        TabKind::Logs { svc, .. } => (format!("{svc} · 日志"), IconName::ScrollText),
                        _ => (node.name.clone(), IconName::Server),
                    }
                }
            };
            let pinned = matches!(tab.kind, TabKind::Nodes);
            // 服务器终端标签的图标颜色就是连接状态
            let icon_color = match tab.term.as_ref().map(|t| t.handle.status()) {
                Some(crate::terminal::ssh::Status::Connected) => c::online(),
                Some(crate::terminal::ssh::Status::Closed(_)) => c::danger(),
                Some(_) => c::warn(),
                None => if on { c::accent() } else { c::ghost() },
            };
            let group: SharedString = format!("tab-{ix}").into();
            div()
                .id(("tab", ix))
                .group(group.clone())
                .h(px(TAB_H))
                .flex_shrink_0()
                .when(!pinned, |d| d.min_w(px(110.0)).max_w(px(200.0)))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(7.0))
                .pl(px(10.0))
                .pr(px(if pinned { 12.0 } else { 6.0 }))
                .rounded(px(t::RADIUS_MD))
                .border_1()
                .text_size(px(t::TYPE_CONTROL))
                .cursor_pointer()
                .when(on, |d| d.bg(c::raised_hi()).border_color(c::border_strong()).text_color(c::text()).font_weight(FontWeight::MEDIUM))
                .when(!on, |d| {
                    d.border_color(gpui_kit::transparent_black())
                        .text_color(c::muted())
                        .hover(|s| s.bg(c::hover()).text_color(c::secondary()))
                })
                .child(icon(kind_icon, 14.0, icon_color))
                .child(div().flex_1().min_w_0().truncate().child(title))
                .when(!pinned, |d| {
                    d.child(
                        div()
                            .id(("tab-close", ix))
                            .size(px(18.0))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(t::RADIUS_XS))
                            .when(!on, |d| d.opacity(0.0).group_hover(group.clone(), |s| s.opacity(1.0)))
                            .hover(|s| s.bg(c::selected()))
                            .child(icon(IconName::X, 12.0, c::muted()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, cx);
                            })),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.activate(ix);
                    cx.notify();
                }))
        };

        // 节点页固定在最左,其余标签在可横向滚动的区域里
        let pinned = tab(0, cx);
        let rest: Vec<_> = (1..self.workspace.tabs.len()).map(|ix| tab(ix, cx)).collect();

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .h(px(TITLEBAR_H))
            .flex_shrink_0()
            .pl(px(TITLEBAR_PAD_L))
            .pr(px(CONTENT_PAD))
            .bg(c::chrome())
            .border_b_1()
            .border_color(c::border())
            .child(pinned)
            .child(div().w(px(1.0)).h(px(14.0)).mx(px(4.0)).flex_shrink_0().bg(c::border_strong()))
            .child(div().id("tabs").flex().flex_row().items_center().gap(px(4.0)).min_w_0().overflow_x_scroll().children(rest))
    }
}
