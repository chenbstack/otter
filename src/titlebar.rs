//! 标题栏:红绿灯 + 标签(固定的“节点”页标签在最左)。标签是垂直居中的胶囊。

use super::*;
use workspace::{NodeMode, TabKind};

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
                        TabKind::Terminal { container, n, .. } => {
                            let who = &container.1;
                            let title = if *n > 1 { format!("{who} · 终端 {n}") } else { format!("{who} · 终端") };
                            (title, IconName::SquareTerminal)
                        }
                        TabKind::Logs { svc, .. } => (format!("{svc} · 日志"), IconName::ScrollText),
                        _ => {
                            // 同一节点开了多个标签(复制出来的)时加序号区分
                            let same: Vec<usize> = (0..self.workspace.tabs.len())
                                .filter(|&i| matches!(self.workspace.tabs[i].kind, TabKind::Node(n) if n == id))
                                .collect();
                            let title = match same.iter().position(|&i| i == ix) {
                                Some(k) if same.len() > 1 => format!("{} · {}", node.name, k + 1),
                                _ => node.name.clone(),
                            };
                            (title, tab.mode.icon())
                        }
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
                .relative()
                .when(on, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::MEDIUM))
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
                .when(!pinned, |d| {
                    d.on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                            this.tab_menu = Some((ix, e.position));
                            cx.notify();
                        }),
                    )
                })
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
            .child(pinned)
            .child(div().w(px(8.0)).flex_shrink_0())
            .child(div().id("tabs").flex().flex_row().items_center().gap(px(4.0)).min_w_0().overflow_x_scroll().children(rest))
            .child(div().flex_1().min_w(px(12.0)))
            .children(self.mode_switch(cx))
    }

    /// 标题栏右侧的模式切换:只在活动标签是节点标签时出现。
    fn mode_switch(&self, cx: &mut Context<Self>) -> Option<Div> {
        let a = self.workspace.active?;
        let tab = &self.workspace.tabs[a];
        if !matches!(tab.kind, TabKind::Node(_)) {
            return None;
        }
        let cur = tab.mode;
        let items = NodeMode::ALL.iter().enumerate().map(|(i, &m)| {
            let on = m == cur;
            div()
                .id(("node-mode", i))
                .h(px(TAB_H - 4.0))
                .px(px(10.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .rounded(px(t::RADIUS_SM))
                .text_size(px(t::TYPE_CONTROL))
                .cursor_pointer()
                .when(on, |d| d.glass(t::RADIUS_SM, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::MEDIUM))
                .when(!on, |d| {
                    d.border_1().border_color(gpui_kit::transparent_black()).text_color(c::muted()).hover(|s| s.bg(c::hover()).text_color(c::secondary()))
                })
                .child(icon(m.icon(), 14.0, if on { c::accent() } else { c::ghost() }))
                .child(m.label())
                .on_click(cx.listener(move |this, _, window, cx| this.set_mode(m, window, cx)))
        });
        Some(
            div()
                .flex_shrink_0()
                .h(px(TAB_H))
                .p(px(2.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .rounded(px(t::RADIUS_MD))
                .border_1()
                .border_color(c::border())
                .children(items),
        )
    }

    /// 标签右键菜单:铺满窗口的透明遮罩 + 菜单面板。
    pub(crate) fn tab_menu_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (ix, pos) = self.tab_menu?;
        let tab = self.workspace.tabs.get(ix)?;
        let is_node = matches!(tab.kind, TabKind::Node(_));
        let item = |id: &'static str, ic: IconName, label: &'static str, enabled: bool| {
            div()
                .id(id)
                .h(px(28.0))
                .px(px(10.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .rounded(px(t::RADIUS_SM))
                .text_size(px(t::TYPE_CONTROL))
                .when(enabled, |d| d.cursor_pointer().text_color(c::secondary()).hover(|s| s.bg(c::hover()).text_color(c::text())))
                .when(!enabled, |d| d.text_color(c::ghost()))
                .child(icon(ic, 14.0, c::ghost()))
                .child(label)
        };
        let panel = div()
            .id("tab-menu")
            .absolute()
            .left(pos.x)
            .top(pos.y)
            .w(px(168.0))
            .p(px(4.0))
            .flex()
            .flex_col()
            .rounded(px(t::RADIUS_MD))
            .bg(c::dialog())
            .border_1()
            .border_color(c::border())
            .occlude()
            .child(item("tab-dup", IconName::Copy, "复制标签", is_node).when(is_node, |d| {
                d.on_click(cx.listener(move |this, _, window, cx| {
                    this.tab_menu = None;
                    this.duplicate_tab(ix, window, cx);
                }))
            }))
            .child(item("tab-close-menu", IconName::X, "关闭标签", true).on_click(cx.listener(move |this, _, _, cx| {
                this.tab_menu = None;
                this.close_tab(ix, cx);
            })));
        Some(
            div()
                .id("tab-menu-mask")
                .absolute()
                .size_full()
                .top_0()
                .left_0()
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.tab_menu = None;
                    cx.notify();
                }))
                .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| {
                    this.tab_menu = None;
                    cx.notify();
                }))
                .child(panel)
                .into_any_element(),
        )
    }
}
