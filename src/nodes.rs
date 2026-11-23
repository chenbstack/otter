//! 节点页:分组文件夹卡片 + 节点卡片,带搜索和新建节点入口。点节点卡片打开该节点的标签。

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};

use super::*;

/// 节点页侧栏的四个视图
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum NodesView {
    Hosts,
    Keychain,
    KnownHosts,
    Logs,
}

impl NodesView {
    pub(crate) const ALL: [NodesView; 4] = [NodesView::Hosts, NodesView::Keychain, NodesView::KnownHosts, NodesView::Logs];

    pub(crate) fn label(self) -> &'static str {
        match self {
            NodesView::Hosts => "主机",
            NodesView::Keychain => "钥匙串",
            NodesView::KnownHosts => "已知主机",
            NodesView::Logs => "日志",
        }
    }

    pub(crate) fn icon(self) -> IconName {
        match self {
            NodesView::Hosts => IconName::Server,
            NodesView::Keychain => IconName::KeyRound,
            NodesView::KnownHosts => IconName::ShieldCheck,
            NodesView::Logs => IconName::ScrollText,
        }
    }
}

impl Home {
    /// 创建搜索输入框(需要 window 和 cx,所以不放在 `new` 里)
    pub(crate) fn init_node_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.node_search.is_none() {
            let s = cx.new(|cx| InputState::new(window, cx).placeholder("搜索节点"));
            cx.observe(&s, |_, _, cx| cx.notify()).detach();
            self.node_search = Some(s);
        }
    }

    /// 「主机」视图:搜索条、分组、节点卡片网格
    pub(super) fn hosts_view(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let query = self.node_search.as_ref().map(|s| s.read(cx).value().trim().to_lowercase()).unwrap_or_default();
        let searching = !query.is_empty();

        // 通栏搜索条 + 主按钮
        let search = self.node_search.clone().map(|s| {
            div()
                .flex_1()
                .min_w_0()
                .h(px(NODE_BAR_H))
                .flex()
                .flex_row()
                .items_center()
                .pl(px(12.0))
                .rounded(px(t::RADIUS_MD))
                .bg(c::field())
                .border_1()
                .border_color(c::field_border())
                .hover(|s| s.bg(c::field_hover()))
                .text_size(px(t::TYPE_CONTROL))
                .child(icon(IconName::Search, 14.0, c::ghost()))
                .child(div().flex_1().min_w_0().ml(px(-4.0)).child(Input::new(&s).appearance(false).with_size(px(12.0 / 0.875))))
        });
        let add = div()
            .id("add-node")
            .flex_shrink_0()
            .h(px(NODE_BAR_H))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(14.0))
            .rounded(px(t::RADIUS_MD))
            .bg(c::accent())
            .text_color(c::on_accent())
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .cursor_pointer()
            .hover(|s| s.bg(c::accent().opacity(0.88)))
            .child(icon(IconName::Plus, 13.0, c::on_accent()))
            .child("新建节点")
            .on_click(cx.listener(|this, _, window, cx| this.open_add_node(window, cx)));
        let top = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .flex_shrink_0()
            .px(px(CONTENT_PAD))
            .pt(px(16.0))
            .pb(px(10.0))
            .children(search)
            .child(add);

        // 面包屑
        let crumb_root = div()
            .id("crumb-root")
            .h(px(24.0))
            .flex()
            .items_center()
            .rounded(px(t::RADIUS_SM))
            .text_size(px(t::TYPE_BODY))
            .font_weight(FontWeight::SEMIBOLD)
            .when(self.node_group.is_some(), |d| {
                d.text_color(c::muted()).cursor_pointer().hover(|s| s.bg(c::hover()).text_color(c::text()))
            })
            .when(self.node_group.is_none(), |d| d.text_color(c::text()))
            .child("全部节点")
            .on_click(cx.listener(|this, _, _, cx| {
                this.node_group = None;
                cx.notify();
            }));
        let crumbs = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .child(crumb_root)
            .children(self.node_group.and_then(|gi| self.groups.get(gi)).map(|g| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(icon(IconName::ChevronRight, 12.0, c::ghost()))
                    .child(div().size(px(7.0)).rounded(px(2.0)).bg(rgb(g.color)))
                    .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(g.name.clone()))
            }));
        let sub = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .px(px(CONTENT_PAD))
            .pb(px(10.0))
            .child(crumbs);

        // 卡片网格:放得下几张就分几列,卡片均分剩余宽度
        let avail = f32::from(window.viewport_size().width) - CONTENT_PAD * 2.0;
        let cols = (((avail + NODE_CARD_GAP) / (NODE_CARD_MIN_W + NODE_CARD_GAP)).floor() as usize).max(1);
        let card_w = (avail - NODE_CARD_GAP * (cols - 1) as f32) / cols as f32;
        let grid = |cards: Vec<Stateful<Div>>| div().flex().flex_row().flex_wrap().gap(px(NODE_CARD_GAP)).children(cards);
        let section = |label: &'static str, first: bool| {
            div()
                .mt(px(if first { 0.0 } else { 20.0 }))
                .mb(px(8.0))
                .text_size(px(t::TYPE_CAPTION))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(c::muted())
                .child(label)
        };

        let mut list = div()
            .id("nodes-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .px(px(CONTENT_PAD))
            .pb(px(CONTENT_PAD));

        // 根目录且没在搜索/筛选时,先列分组文件夹
        let mut first = true;
        if self.node_group.is_none() && !searching {
            let folders: Vec<_> = self.groups.iter().enumerate().map(|(gi, g)| self.folder_card(gi, g, card_w, cx)).collect();
            list = list.child(section("分组", true)).child(grid(folders));
            first = false;
        }

        let nodes: Vec<&Node> = self
            .groups
            .iter()
            .enumerate()
            .filter(|(gi, _)| self.node_group.is_none_or(|g| g == *gi))
            .flat_map(|(_, g)| g.nodes.iter())
            .filter(|n| query.is_empty() || n.name.to_lowercase().contains(&query) || n.user_host.to_lowercase().contains(&query))
            .collect();
        if nodes.is_empty() {
            if self.all_nodes().next().is_none() {
                list = list.child(empty_state(IconName::Server, "还没有节点", "点右上角「新建节点」添加第一台服务器"));
            } else {
                list = list.child(empty_state(IconName::Search, "没有匹配的节点", "换个关键词或筛选条件"));
            }
        } else {
            let label = if searching { "搜索结果" } else { "节点" };
            let cards: Vec<_> = nodes.into_iter().map(|n| self.node_card(n, card_w, cx)).collect();
            list = list.child(section(label, first)).child(grid(cards));
        }

        div().flex_1().min_w_0().h_full().flex().flex_col().child(top).child(sub).child(list)
    }

    /// 图标块:圆角方块里放图标,Termius 风格
    fn icon_tile(ic: IconName, fg: Rgba, bg: Rgba) -> Div {
        div()
            .size(px(NODE_ICON))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(t::RADIUS_MD))
            .border_1()
            .border_color(fg.opacity(0.28))
            .bg(bg)
            .child(icon(ic, 16.0, fg))
    }

    fn card_shell(id: ElementId, w: f32) -> Stateful<Div> {
        div()
            .id(id)
            .glass(t::RADIUS_LG, GlassStyle::CARD)
            .glass_hover(GlassStyle::CARD)
            .w(px(w))
            .h(px(NODE_CARD_H))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
    }

    fn folder_card(&self, gi: usize, g: &Group, w: f32, cx: &mut Context<Self>) -> Stateful<Div> {
        Self::card_shell(("folder", gi).into(), w)
            .child(Self::icon_tile(IconName::Folder, rgb(g.color).into(), rgba((g.color << 8) | 0x24)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(g.name.clone()))
                    .child(
                        div()
                            .text_size(px(t::TYPE_CAPTION))
                            .text_color(c::muted())
                            .child(format!("{} 个节点", g.nodes.len())),
                    ),
            )
            .child(icon(IconName::ChevronRight, 14.0, c::ghost()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.node_group = Some(gi);
                cx.notify();
            }))
    }

    fn node_card(&self, n: &Node, w: f32, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = n.id;
        Self::card_shell(("node", id.0 as usize).into(), w)
            .group(format!("node-card-{}", id.0))
            .child(Self::icon_tile(IconName::Server, c::accent(), c::accent_soft()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(t::TYPE_BODY))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(c::text())
                            .child(n.name.clone()),
                    )
                    .child(div().truncate().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::ghost()).child(n.user_host.clone())),
            )
            .child(
                div()
                    .id(("node-edit", id.0 as usize))
                    .size(px(24.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(t::RADIUS_SM))
                    .opacity(0.0)
                    .group_hover(format!("node-card-{}", id.0), |s| s.opacity(1.0))
                    .hover(|s| s.bg(c::hover()))
                    .child(icon(IconName::Pencil, 14.0, c::muted()))
                    .tooltip(|_, cx| cx.new(|_| crate::files::Tip("编辑".into())).into())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.open_edit_node(id, window, cx);
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_node(id, None, window, cx);
            }))
    }
}

