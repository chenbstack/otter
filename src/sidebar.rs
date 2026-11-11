//! 标题栏与节点树。

use super::*;

impl Home {
    pub(crate) fn titlebar(&self) -> Div {
        let online = self.count(Filter::Online);
        let total = self.count(Filter::All);
        let (g, _) = self.locate(self.sel);
        let node = self.node();
        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(TITLEBAR_H))
            .flex_shrink_0()
            .bg(c::chrome())
            .border_b_1()
            .border_color(c::border())
            // 品牌:紧跟红绿灯
            .child(
                div()
                    .w(px(SIDEBAR_W))
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .pl(px(84.0))
                    .child(
                        div()
                            .size(px(20.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(t::RADIUS_SM))
                            .bg(c::online_soft())
                            .child(icon(IconName::Server, 12.0, c::online())),
                    )
                    .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child("DeskPanel")),
            )
            // 面包屑:分组 / 节点 · 地址
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(16.0))
                    .text_size(px(t::TYPE_CONTROL))
                    .child(div().size(px(6.0)).rounded_full().bg(rgb(self.groups[g].color)))
                    .child(div().text_color(c::muted()).child(self.groups[g].name.clone()))
                    .child(icon(IconName::ChevronRight, 12.0, c::ghost()))
                    .child(div().text_color(c::text()).font_weight(FontWeight::SEMIBOLD).child(node.name.clone()))
                    .child(
                        div()
                            .ml(px(4.0))
                            .text_size(px(t::TYPE_CAPTION))
                            .font_family(MONO)
                            .text_color(c::ghost())
                            .child(node.user_host.clone()),
                    ),
            )
            .child(
                div()
                    .mr(px(CONTENT_PAD))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(7.0))
                    .h(px(24.0))
                    .px(px(10.0))
                    .rounded_full()
                    .border_1()
                    .border_color(c::border())
                    .text_size(px(t::TYPE_CAPTION))
                    .text_color(c::muted())
                    .child(div().size(px(6.0)).rounded_full().bg(c::online()))
                    .child(
                        div()
                            .font_family(MONO)
                            .text_color(c::secondary())
                            .child(format!("{online}/{total}")),
                    )
                    .child("节点在线"),
            )
    }

    pub(crate) fn sidebar(&self, cx: &mut Context<Self>) -> Div {
        let search = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .h(px(32.0))
            .px(px(10.0))
            .rounded(px(t::RADIUS_MD))
            .border_1()
            .border_color(c::border())
            .bg(c::sunken())
            .text_size(px(t::TYPE_CONTROL))
            .text_color(c::ghost())
            .child(icon(IconName::Search, 14.0, c::ghost()))
            .child(div().flex_1().child("搜索节点"))
            .child(
                div()
                    .px(px(5.0))
                    .rounded(px(t::RADIUS_XS))
                    .border_1()
                    .border_color(c::border_strong())
                    .text_size(px(t::TYPE_MICRO))
                    .font_family(MONO)
                    .text_color(c::ghost())
                    .child("⌘K"),
            );

        let tab = |id: &'static str, label: &'static str, f: Filter, cx: &mut Context<Self>| {
            let active = self.filter == f;
            div()
                .id(id)
                .flex_1()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(5.0))
                .h(px(26.0))
                .rounded(px(t::RADIUS_SM))
                .text_size(px(t::TYPE_CONTROL))
                .cursor_pointer()
                .when(active, |d| {
                    d.bg(c::selected())
                        .text_color(c::text())
                        .font_weight(FontWeight::SEMIBOLD)
                        .shadow_xs()
                })
                .when(!active, |d| d.text_color(c::muted()).hover(|s| s.text_color(c::secondary())))
                .child(label)
                .child(
                    div()
                        .text_size(px(t::TYPE_CAPTION))
                        .font_family(MONO)
                        .text_color(if active { c::muted() } else { c::ghost() })
                        .child(self.count(f).to_string()),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.filter = f;
                    cx.notify();
                }))
        };

        let tabs = div()
            .flex()
            .flex_row()
            .p(px(3.0))
            .rounded(px(t::RADIUS_MD))
            .bg(c::sunken())
            .border_1()
            .border_color(c::border())
            .child(tab("f-all", "全部", Filter::All, cx))
            .child(tab("f-on", "在线", Filter::Online, cx))
            .child(tab("f-off", "离线", Filter::Offline, cx));

        let mut tree = div().flex().flex_col().gap(px(2.0)).flex_1().min_h_0().id("tree").overflow_y_scroll();
        for (gi, g) in self.groups.iter().enumerate() {
            let nodes: Vec<(usize, &Node)> = g
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| match self.filter {
                    Filter::All => true,
                    Filter::Online => n.online(),
                    Filter::Offline => !n.online(),
                })
                .collect();
            if nodes.is_empty() && self.filter != Filter::All {
                continue;
            }
            let collapsed = self.collapsed.contains(&gi);
            tree = tree.child(
                div()
                    .id(("g", gi))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(28.0))
                    .px(px(8.0))
                    .mt(px(if gi == 0 { 2.0 } else { 10.0 }))
                    .rounded(px(t::RADIUS_SM))
                    .cursor_pointer()
                    .hover(|s| s.bg(c::hover()))
                    .child(icon(
                        if collapsed { IconName::ChevronRight } else { IconName::ChevronDown },
                        12.0,
                        c::ghost(),
                    ))
                    .child(div().size(px(7.0)).rounded(px(2.0)).bg(rgb(g.color)))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(t::TYPE_CAPTION))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(c::muted())
                            .child(g.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(t::TYPE_MICRO))
                            .font_family(MONO)
                            .text_color(c::ghost())
                            .child(g.nodes.len().to_string()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed.remove(&gi) {
                            this.collapsed.insert(gi);
                        }
                        cx.notify();
                    })),
            );
            if collapsed {
                continue;
            }
            for (ni, n) in nodes {
                let id = n.id;
                let selected = self.sel == id;
                tree = tree.child(
                    div()
                        .id(("n", gi * 100 + ni))
                        .relative()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.0))
                        .px(px(10.0))
                        .py(px(4.0))
                        .ml(px(6.0))
                        .rounded(px(t::RADIUS_MD))
                        .cursor_pointer()
                        .border_1()
                        .when(selected, |d| d.bg(c::raised_hi()).border_color(c::border_strong()))
                        .when(!selected, |d| d.border_color(gpui_kit::transparent_black()).hover(|s| s.bg(c::hover())))
                        .child(status_dot(if n.online() { c::online() } else { c::ghost() }, n.online()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_size(px(t::TYPE_BODY))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(if n.online() { c::text() } else { c::muted() })
                                        .child(n.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(t::TYPE_MICRO))
                                        .font_family(MONO)
                                        .text_color(c::ghost())
                                        .truncate()
                                        .child(n.user_host.clone()),
                                ),
                        )
                        .child(match n.latency_ms {
                            Some(ms) => div()
                                .text_size(px(t::TYPE_MICRO))
                                .font_family(MONO)
                                .text_color(latency_color(ms))
                                .child(format!("{ms}ms")),
                            None => div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child("离线"),
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.sel = id;
                            this.scroll = VirtualListScrollHandle::new();
                            cx.notify();
                        })),
                );
            }
        }

        let add = div()
            .id("add-node")
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .h(px(34.0))
            .rounded(px(t::RADIUS_MD))
            .border_1()
            .border_dashed()
            .border_color(c::border_strong())
            .text_size(px(t::TYPE_BODY))
            .text_color(c::muted())
            .cursor_pointer()
            .hover(|s| s.bg(c::hover()).text_color(c::text()))
            .child(icon(IconName::Plus, 14.0, c::muted()))
            .child("添加节点")
            .on_click(cx.listener(|this, _, window, cx| this.open_add_node(window, cx)));

        div()
            .w(px(SIDEBAR_W))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(12.0))
            .bg(c::chrome())
            .child(search)
            .child(tabs)
            .child(tree)
            .child(add)
    }
}
