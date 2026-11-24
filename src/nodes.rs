//! 节点页:分组文件夹卡片 + 节点卡片,带搜索和新建节点入口。点节点卡片打开该节点的标签。

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};

use super::*;
use crate::node_form::NodeForm;
use crate::workspace::NodeMode;

/// 删除确认框的内容(节点、分组、密钥共用,见 `confirm_parts`)
pub(crate) struct Confirm {
    /// 取消、确认两个按钮的元素 id
    pub ids: [&'static str; 2],
    pub title: &'static str,
    /// 对象卡片左边的图标块
    pub tile: AnyElement,
    pub name: SharedString,
    pub detail: SharedString,
    /// 后果清单,一条一行:(图标, 图标颜色, 文字)。会丢的东西用红色,不受影响的用绿色
    pub points: Vec<(IconName, Rgba, SharedString)>,
    pub ok_label: &'static str,
}

/// 删除确认框宽度(比表单弹窗窄:内容少,居中排版)
pub(crate) const CONFIRM_W: f32 = 400.0;

/// 右键菜单的目标:节点卡片或分组卡片(分组下标)
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum NodeMenuTarget {
    Node(NodeId),
    Group(usize),
    /// 节点菜单里展开的「移动到分组」子列表
    MoveNode(NodeId),
}

/// 拖拽中的节点卡片
#[derive(Clone)]
pub(crate) struct DragNode {
    id: NodeId,
    name: String,
    w: f32,
}

struct NodeGhost(DragNode);

impl Render for NodeGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Home::card_shell("node-ghost".into(), self.0.w)
            .bg(c::dialog())
            .border_1()
            .border_color(c::accent().opacity(0.6))
            .shadow_lg()
            .child(Home::icon_tile(IconName::Server, c::accent(), c::accent_soft()))
            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(self.0.name.clone()))
    }
}

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
    /// 终端连上了就记下「最近连接」时间:每个 (标签, 连接代数) 只记一次,重连也算。
    pub(crate) fn sync_last_connected(&mut self) {
        if self.demo {
            return;
        }
        let now = crate::store::now_secs();
        let mut done: Vec<NodeId> = vec![];
        for tab in &self.workspace.tabs {
            let (Some(id), Some(t)) = (tab.node(), tab.term.as_ref()) else { continue };
            if t.handle.status() == crate::terminal::ssh::Status::Connected && self.connect_noted.insert((tab.id, t.handle.generation())) {
                done.push(id);
            }
        }
        for id in done {
            if let Some(Err(e)) = self.store.as_mut().map(|s| s.set_node_connected(id, now)) {
                crate::logs::push(crate::logs::Level::Error, "本地库", format!("记录最近连接时间失败: {e}"));
            }
            if let Some(n) = self.groups.iter_mut().flat_map(|g| g.nodes.iter_mut()).find(|n| n.id == id) {
                n.last_connected = now;
            }
        }
    }

    fn set_node_sort(&mut self, sort: NodeSort, cx: &mut Context<Self>) {
        self.node_sort = sort;
        self.sort_menu = false;
        if let Some(s) = &self.store {
            let _ = s.set_setting("node_sort", sort.key());
        }
        cx.notify();
    }

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
        let split_h = NODE_BAR_H;
        let add_main = div()
            .id("add-node")
            .h(px(split_h))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(14.0))
            .rounded_l(px(t::RADIUS_MD))
            .bg(c::accent())
            .text_color(c::on_accent())
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .cursor_pointer()
            .hover(|s| s.bg(c::accent().opacity(0.88)))
            .child(icon(IconName::Plus, 13.0, c::on_accent()))
            .child("新建节点")
            .on_click(cx.listener(|this, _, window, cx| this.open_add_node(window, cx)));
        let add_more = div()
            .id("add-node-more")
            .h(px(split_h))
            .w(px(split_h))
            .flex()
            .items_center()
            .justify_center()
            .rounded_r(px(t::RADIUS_MD))
            .bg(c::accent())
            .border_l(px(2.0))
            .border_color(c::on_accent().opacity(0.25))
            .cursor_pointer()
            .hover(|s| s.bg(c::accent().opacity(0.88)))
            .child(icon(if self.add_menu { IconName::ChevronUp } else { IconName::ChevronDown }, 14.0, c::on_accent()))
            .on_click(cx.listener(|this, _, _, cx| {
                this.add_menu = !this.add_menu;
                cx.notify();
            }));
        let (t1, t2) = (cx.entity(), cx.entity());
        let add = div()
            .relative()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .child(add_main)
            .child(add_more)
            .when(self.add_menu, |d| {
                d.child(div().absolute().top(px(split_h + 4.0)).right_0().w(px(crate::ui::DROPDOWN_COMPACT_W)).h(px(0.0)).child(crate::ui::dropdown_menu_compact(
                    "add-node-menu",
                    vec![crate::ui::MenuItem::new("新建分组").icon(IconName::FolderPlus), crate::ui::MenuItem::new("导入").icon(IconName::FileUp)],
                    None,
                    crate::ui::MenuSide::Below,
                    0.0,
                    move |i, window, cx| {
                        t1.update(cx, |h, cx| {
                            h.add_menu = false;
                            if i == 0 {
                                h.open_add_group(window, cx);
                            } else {
                                h.open_import(window, cx);
                            }
                        })
                    },
                    move |_, cx| {
                        t2.update(cx, |h, cx| {
                            h.add_menu = false;
                            cx.notify();
                        })
                    },
                )))
            });
        let (s1, s2) = (cx.entity(), cx.entity());
        let sort_btn = div()
            .relative()
            .flex_shrink_0()
            .child(
                div()
                    .id("node-sort")
                    .h(px(NODE_BAR_H))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(7.0))
                    .px(px(12.0))
                    .rounded(px(t::RADIUS_MD))
                    .bg(c::field())
                    .border_1()
                    .border_color(c::field_border())
                    .hover(|s| s.bg(c::field_hover()))
                    .text_size(px(t::TYPE_CONTROL))
                    .text_color(if self.node_sort == NodeSort::Manual { c::muted() } else { c::text() })
                    .cursor_pointer()
                    .child(icon(IconName::ArrowUpDown, 14.0, c::ghost()))
                    .child(self.node_sort.label())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sort_menu = !this.sort_menu;
                        cx.notify();
                    })),
            )
            .when(self.sort_menu, |d| {
                d.child(div().absolute().top(px(NODE_BAR_H + 4.0)).right_0().w(px(crate::ui::DROPDOWN_COMPACT_W)).h(px(0.0)).child(crate::ui::dropdown_menu_compact(
                    "node-sort-menu",
                    NodeSort::ALL.iter().map(|s| crate::ui::MenuItem::new(s.label()).icon(s.icon())).collect(),
                    NodeSort::ALL.iter().position(|s| *s == self.node_sort),
                    crate::ui::MenuSide::Below,
                    0.0,
                    move |i, _, cx| s1.update(cx, |h, cx| h.set_node_sort(NodeSort::ALL[i], cx)),
                    move |_, cx| {
                        s2.update(cx, |h, cx| {
                            h.sort_menu = false;
                            cx.notify();
                        })
                    },
                )))
            });
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
            .child(sort_btn)
            .child(add);

        // 面包屑
        let crumb_root = div()
            .id("crumb-root")
            .h(px(26.0))
            // 左右留白让悬停底色不贴着文字;负外边距抵消,文字仍和下面的内容左对齐
            .px(px(8.0))
            .ml(px(-8.0))
            .flex()
            .items_center()
            .rounded(px(t::RADIUS_MD))
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
        let avail = f32::from(window.viewport_size().width) - NODES_SIDE_W - CONTENT_PAD * 2.0;
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

        let mut nodes: Vec<&Node> = self
            .groups
            .iter()
            .enumerate()
            .filter(|(gi, _)| self.node_group.is_none_or(|g| g == *gi))
            .flat_map(|(_, g)| g.nodes.iter())
            .filter(|n| query.is_empty() || n.name.to_lowercase().contains(&query) || n.user_host.to_lowercase().contains(&query))
            .collect();
        // 稳定排序:相同的保持默认顺序;没连过的(0)排最后
        match self.node_sort {
            NodeSort::Manual => {}
            NodeSort::Added => nodes.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.0.cmp(&a.id.0))),
            NodeSort::Connected => nodes.sort_by(|a, b| b.last_connected.cmp(&a.last_connected)),
        }
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
    pub(crate) fn icon_tile(ic: IconName, fg: Rgba, bg: Rgba) -> Div {
        Self::icon_tile_with(fg, bg, icon(ic, 16.0, fg))
    }

    fn icon_tile_with(fg: Rgba, bg: Rgba, content: impl IntoElement) -> Div {
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
            .child(content)
    }

    /// 节点卡片的图标块:认出系统就用系统自己的图标和品牌色(macOS 是苹果图标,发行版是内置的 logo),否则是通用服务器图标
    pub(crate) fn os_tile(os: crate::osinfo::OsKind) -> Div {
        use crate::osinfo::OsKind;
        let (fg, bg) = match os.hue() {
            Some(h) => (rgb(h), rgb(h).opacity(0.18)),
            None => (c::accent(), c::accent_soft()),
        };
        match (os, os.logo()) {
            (OsKind::MacOs, _) => Self::icon_tile(IconName::Apple, fg, bg),
            (_, Some(path)) => Self::icon_tile_with(fg, bg, gpui::svg().path(path).size(px(18.0)).text_color(fg)),
            _ => Self::icon_tile(IconName::Server, fg, bg),
        }
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
            .drag_over::<DragNode>(|s, _, _, _| s.bg(c::accent_soft()).border_color(c::accent()))
            .on_drop(cx.listener(move |this, d: &DragNode, _, cx| this.move_node_to(d.id, gi, cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                this.node_menu = Some((NodeMenuTarget::Group(gi), e.position));
                cx.notify();
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.node_group = Some(gi);
                cx.notify();
            }))
    }

    /// 节点卡片/分组卡片的右键菜单浮层
    pub(crate) fn node_menu_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (target, pos) = self.node_menu?;
        let item = |id: ElementId, ic: IconName, label: SharedString| {
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
                .cursor_pointer()
                .text_color(c::secondary())
                .hover(|s| s.bg(c::hover()).text_color(c::text()))
                .child(icon(ic, 14.0, c::ghost()))
                .child(label)
        };
        let danger = |d: Stateful<Div>| d.text_color(c::danger()).hover(|s| s.bg(c::hover()));
        let mut panel = div()
            .id("node-menu")
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
            .occlude();
        match target {
            NodeMenuTarget::Node(nid) | NodeMenuTarget::MoveNode(nid) => {
                // 悬停到别的菜单项时收起二级菜单
                let collapse = move |this: &mut Home, hovered: &bool, cx: &mut Context<Home>, nid: NodeId| {
                    if *hovered && matches!(this.node_menu, Some((NodeMenuTarget::MoveNode(_), _))) {
                        this.node_menu = Some((NodeMenuTarget::Node(nid), pos));
                        cx.notify();
                    }
                };
                for (id, ic, label, mode) in [
                    ("nm-term", NodeMode::Terminal.icon(), "打开终端", NodeMode::Terminal),
                    ("nm-sftp", NodeMode::Files.icon(), "打开 SFTP", NodeMode::Files),
                    ("nm-docker", NodeMode::Containers.icon(), "打开容器", NodeMode::Containers),
                ] {
                    panel = panel.child(item(id.into(), ic, label.into()).on_hover(cx.listener(move |this, h: &bool, _, cx| collapse(this, h, cx, nid))).on_click(cx.listener(move |this, _, window, cx| {
                        this.node_menu = None;
                        this.open_node(nid, Some(mode), window, cx);
                    })));
                }
                panel = panel
                    .child(div().h(px(1.0)).mx(px(6.0)).my(px(4.0)).bg(c::border()))
                    .child(item("nm-edit".into(), IconName::Pencil, "编辑".into()).on_hover(cx.listener(move |this, h: &bool, _, cx| collapse(this, h, cx, nid))).on_click(cx.listener(move |this, _, window, cx| {
                        this.node_menu = None;
                        this.open_edit_node(nid, window, cx);
                    })));
                if self.groups.len() > 1 {
                    panel = panel.child(item("nm-move".into(), IconName::Folder, "移动到分组".into()).child(div().flex_1()).child(icon(IconName::ChevronRight, 12.0, c::ghost())).when(target == NodeMenuTarget::MoveNode(nid), |d| d.bg(c::hover()).text_color(c::text())).on_hover(cx.listener(move |this, h: &bool, _, cx| {
                        if *h {
                            this.node_menu = Some((NodeMenuTarget::MoveNode(nid), pos));
                            cx.notify();
                        }
                    })).on_click(cx.listener(move |this, _, _, cx| {
                        this.node_menu = Some((NodeMenuTarget::MoveNode(nid), pos));
                        cx.notify();
                    })));
                }
                panel = panel
                    .child(div().h(px(1.0)).mx(px(6.0)).my(px(4.0)).bg(c::border()))
                    .child(danger(item("nm-del".into(), IconName::Trash, "删除".into())).on_hover(cx.listener(move |this, h: &bool, _, cx| collapse(this, h, cx, nid))).on_click(cx.listener(move |this, _, _, cx| {
                        this.node_menu = None;
                        this.node_delete = Some(nid);
                        cx.notify();
                    })));
            }
            NodeMenuTarget::Group(gi) => {
                panel = panel
                    .child(item("nm-open".into(), IconName::FolderOpen, "打开".into()).on_click(cx.listener(move |this, _, _, cx| {
                        this.node_menu = None;
                        this.node_group = Some(gi);
                        cx.notify();
                    })))
                    .child(item("nm-new".into(), IconName::Plus, "在此分组新建节点".into()).on_click(cx.listener(move |this, _, window, cx| {
                        this.node_menu = None;
                        this.form = Some(NodeForm::new(gi, window, cx));
                        cx.notify();
                    })))
                    .child(item("nm-edit-group".into(), IconName::Pencil, "编辑".into()).on_click(cx.listener(move |this, _, window, cx| {
                        this.node_menu = None;
                        this.open_edit_group(gi, window, cx);
                    })));
                // 至少保留一个分组:新建节点等入口都假定分组非空
                if self.groups.len() > 1 {
                    panel = panel
                        .child(div().h(px(1.0)).mx(px(6.0)).my(px(4.0)).bg(c::border()))
                        .child(danger(item("nm-del-group".into(), IconName::Trash, "删除分组".into())).on_click(cx.listener(move |this, _, _, cx| {
                            this.node_menu = None;
                            this.request_delete_group(gi, cx);
                        })));
                }
            }
        }
        // 二级菜单:在主菜单右侧留 6 的间隔,对齐「移动到分组」那一行
        let mut sub = None;
        if let NodeMenuTarget::MoveNode(nid) = target {
            let cur = self.locate(nid).0;
            let mut sp = div()
                .id("node-menu-sub")
                .absolute()
                .left(pos.x + px(168.0 + 6.0))
                .top(pos.y + px(125.0))
                .w(px(168.0))
                .max_h(px(320.0))
                .overflow_y_scroll()
                .p(px(4.0))
                .flex()
                .flex_col()
                .rounded(px(t::RADIUS_MD))
                .bg(c::dialog())
                .border_1()
                .border_color(c::border())
                .occlude();
            for (gi, g) in self.groups.iter().enumerate().filter(|(gi, _)| *gi != cur) {
                sp = sp.child(item(("nm-mv", gi).into(), IconName::Folder, g.name.clone().into()).on_click(cx.listener(move |this, _, _, cx| {
                    this.node_menu = None;
                    this.move_node_to(nid, gi, cx);
                })));
            }
            sub = Some(sp);
        }
        let close = cx.listener(|this: &mut Home, _: &MouseDownEvent, _, cx| {
            this.node_menu = None;
            cx.notify();
        });
        let close2 = cx.listener(|this: &mut Home, _: &MouseDownEvent, _, cx| {
            this.node_menu = None;
            cx.notify();
        });
        Some(
            div()
                .id("node-menu-mask")
                .absolute()
                .size_full()
                .top_0()
                .left_0()
                .occlude()
                .on_mouse_down(MouseButton::Left, close)
                .on_mouse_down(MouseButton::Right, close2)
                .child(panel)
                .children(sub)
                .into_any_element(),
        )
    }

    /// 删除节点:先写库,再关掉它的标签、从列表移除。
    fn delete_node_now(&mut self, id: NodeId, cx: &mut Context<Self>) {
        if let Some(store) = &self.store {
            if let Err(e) = store.delete_node(id) {
                crate::logs::push(crate::logs::Level::Error, "本地库", &format!("删除节点失败: {e}"));
                return;
            }
        }
        for g in &mut self.groups {
            g.nodes.retain(|n| n.id != id);
        }
        self.after_nodes_removed(&[id], cx);
    }

    /// 删除分组:空分组直接删,有节点先弹确认。
    fn request_delete_group(&mut self, gi: usize, cx: &mut Context<Self>) {
        let Some(g) = self.groups.get(gi) else { return };
        if g.nodes.is_empty() {
            self.delete_group_now(g.id, cx);
        } else {
            self.group_delete = Some(g.id);
            cx.notify();
        }
    }

    pub(crate) fn delete_group_now(&mut self, gid: i64, cx: &mut Context<Self>) {
        self.group_delete = None;
        let Some(gi) = self.groups.iter().position(|g| g.id == gid) else { return };
        if self.groups.len() <= 1 {
            return;
        }
        if let Some(store) = &self.store {
            if let Err(e) = store.delete_group(gid) {
                crate::logs::push(crate::logs::Level::Error, "本地库", &format!("删除分组失败: {e}"));
                return;
            }
        }
        let removed: Vec<NodeId> = self.groups.remove(gi).nodes.iter().map(|n| n.id).collect();
        self.node_group = None;
        self.after_nodes_removed(&removed, cx);
    }

    /// 节点已从列表移除后的收尾:关掉它们的标签,修正选中节点。
    fn after_nodes_removed(&mut self, ids: &[NodeId], cx: &mut Context<Self>) {
        let ixs: Vec<usize> = self.workspace.tabs.iter().enumerate().filter(|(_, t)| t.node().is_some_and(|n| ids.contains(&n))).map(|(i, _)| i).collect();
        for ix in ixs.into_iter().rev() {
            self.close_tab(ix, cx);
        }
        if ids.contains(&self.sel) || !self.groups.iter().any(|g| g.nodes.iter().any(|n| n.id == self.sel)) {
            self.sel = self.groups.iter().flat_map(|g| g.nodes.iter()).map(|n| n.id).next().unwrap_or(NodeId(0));
        }
        cx.notify();
    }

    /// 删除非空分组的确认框
    pub(crate) fn group_delete_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let gid = self.group_delete?;
        let g = self.groups.iter().find(|g| g.id == gid)?;
        let fg = rgb(g.color);
        let spec = Confirm {
            ids: ["gd-cancel", "gd-ok"],
            title: "删除分组",
            tile: Self::icon_tile(IconName::Folder, fg, fg.opacity(0.18)).into_any_element(),
            name: g.name.clone().into(),
            detail: format!("{} 个节点", g.nodes.len()).into(),
            points: vec![
                (IconName::Server, c::danger(), format!("分组里的 {} 个节点会一起删除", g.nodes.len()).into()),
                (IconName::KeyRound, c::danger(), "包括它们保存的密码和私钥".into()),
                (IconName::ShieldCheck, c::online(), "服务器上的内容不受影响".into()),
            ],
            ok_label: "删除分组",
        };
        Some(self.delete_confirm("group-del-dialog", spec, |this| this.group_delete = None, move |this, cx| this.delete_group_now(gid, cx), cx))
    }

    /// 删除节点的确认框(删除会连带清掉它保存的密码和私钥,所以要确认)
    pub(crate) fn node_delete_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let id = self.node_delete?;
        let n = self.groups.iter().flat_map(|g| &g.nodes).find(|n| n.id == id)?;
        let spec = Confirm {
            ids: ["nd-cancel", "nd-ok"],
            title: "删除节点",
            tile: Self::os_tile(n.os).into_any_element(),
            name: n.name.clone().into(),
            detail: n.user_host.clone().into(),
            points: vec![
                (IconName::KeyRound, c::danger(), "删除保存的密码和私钥".into()),
                (IconName::PanelTop, c::danger(), "关闭它已打开的标签".into()),
                (IconName::ShieldCheck, c::online(), "服务器上的内容不受影响".into()),
            ],
            ok_label: "删除节点",
        };
        Some(self.delete_confirm(
            "node-del-dialog",
            spec,
            |this| this.node_delete = None,
            move |this, cx| {
                this.node_delete = None;
                this.delete_node_now(id, cx);
            },
            cx,
        ))
    }

    /// 删除确认框:包一层弹窗外壳,Esc 取消。内容见 `confirm_parts`。
    pub(crate) fn delete_confirm(
        &self,
        dialog_id: &'static str,
        spec: Confirm,
        cancel_fn: impl Fn(&mut Self) + Copy + 'static,
        ok_fn: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let parts = self.confirm_parts(spec, cancel_fn, ok_fn, cx);
        modal_glass(
            dialog_id,
            CONFIRM_W,
            cx.listener(move |this, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    cancel_fn(this);
                    cx.notify();
                    cx.stop_propagation();
                }
            }),
            parts,
        )
    }

    /// 删除确认框的内容(节点、分组、密钥共用),居中排版,自上而下:
    /// 红色图标(带一圈淡光晕) → 标题 + “此操作无法撤销” → 对象卡片(和节点页的节点卡片同款) → 后果清单(一行一项,图标区分“会丢的”和“不受影响的”) → 取消 / 删除两个等宽按钮。
    /// 不再单独做底栏:按钮直接放在内容里,整体更紧凑。
    pub(crate) fn confirm_parts(
        &self,
        spec: Confirm,
        cancel_fn: impl Fn(&mut Self) + Copy + 'static,
        ok_fn: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let hero = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4.0))
            .child(
                // 外圈淡光晕 + 内层图标块
                div()
                    .size(px(68.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(t::RADIUS_2XL))
                    .bg(c::danger().opacity(0.06))
                    .border_1()
                    .border_color(c::danger().opacity(0.12))
                    .child(
                        div()
                            .size(px(48.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(t::RADIUS_XL))
                            .bg(c::danger().opacity(0.14))
                            .border_1()
                            .border_color(c::danger().opacity(0.32))
                            .child(icon(IconName::Trash, 22.0, c::danger())),
                    ),
            )
            .child(div().mt(px(10.0)).text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).text_color(c::text()).child(spec.title))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("此操作无法撤销"));

        // 对象卡片:和节点页的节点卡片一模一样(`card_shell` 同款尺寸、玻璃样式、间距、字号)
        let target = div()
            .w_full()
            .h(px(NODE_CARD_H))
            .glass(t::RADIUS_LG, GlassStyle::CARD)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .child(spec.tile)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(spec.name))
                    .child(div().truncate().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::ghost()).child(spec.detail)),
            );

        // 后果清单:一个整块的圆角容器,行与行之间一根细线
        let n = spec.points.len();
        let points = div().w_full().flex().flex_col().rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).bg(c::field()).children(spec.points.into_iter().enumerate().map(|(i, (ic, color, text))| {
            div()
                .h(px(38.0))
                .px(px(12.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .when(i + 1 < n, |d| d.border_b_1().border_color(c::line()))
                .child(icon(ic, 14.0, color))
                .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(text))
        }));

        let cancel = crate::node_form::ctl_btn(spec.ids[0], None, "取消", 38.0).flex_1().justify_center().on_click(cx.listener(move |this, _, _, cx| {
            cancel_fn(this);
            cx.notify();
        }));
        let ok = div()
            .id(spec.ids[1])
            .flex_1()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .h(px(38.0))
            .rounded(px(t::RADIUS_MD))
            .bg(c::danger())
            .text_color(c::on_accent())
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .cursor_pointer()
            .hover(|s| s.opacity(0.88))
            .child(icon(IconName::Trash, 13.0, c::on_accent()))
            .child(spec.ok_label)
            .on_click(cx.listener(move |this, _, _, cx| ok_fn(this, cx)));
        let buttons = div().w_full().flex().flex_row().gap(px(10.0)).child(cancel).child(ok);

        vec![div().flex().flex_col().items_center().gap(px(18.0)).px(px(28.0)).pt(px(28.0)).pb(px(24.0)).child(hero).child(target).child(points).child(buttons).into_any_element()]
    }

    /// 拖拽把节点移到分组 `gi`:先写库,成功后再改内存列表。
    fn move_node_to(&mut self, id: NodeId, gi: usize, cx: &mut Context<Self>) {
        let (old, ni) = self.locate(id);
        if old == gi || self.groups.get(old).and_then(|g| g.nodes.get(ni)).is_none_or(|n| n.id != id) {
            return;
        }
        if let Some(store) = &self.store {
            if let Err(e) = store.move_node(id, self.groups[gi].id) {
                crate::logs::push(crate::logs::Level::Error, "本地库", &format!("移动节点失败: {e}"));
                return;
            }
        }
        let node = self.groups[old].nodes.remove(ni);
        self.groups[gi].nodes.push(node);
        cx.notify();
    }

    fn node_card(&self, n: &Node, w: f32, cx: &mut Context<Self>) -> Stateful<Div> {
        let id = n.id;
        // 所属分组:名称右上角的小字;进了某个分组之后都在同一组,就不再显示;「未分组」也不显示
        let group = if self.node_group.is_none() { self.groups.get(self.locate(id).0).filter(|g| g.name != "未分组").map(|g| g.name.clone()) } else { None };
        Self::card_shell(("node", id.0 as usize).into(), w)
            .group(format!("node-card-{}", id.0))
            .child(Self::os_tile(n.os))
            .when_some(group.clone(), |d, g| d.child(div().absolute().top(px(7.0)).right(px(12.0)).max_w(px(88.0)).truncate().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(g)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    // 给右上角的分组名让出位置,长名称不会盖住它
                                    .when(group.is_some(), |d| d.pr(px(56.0)))
                                    .text_size(px(t::TYPE_BODY))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(c::text())
                                    .child(n.name.clone()),
                            )
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
            .on_mouse_down(MouseButton::Right, cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                this.node_menu = Some((NodeMenuTarget::Node(id), e.position));
                cx.notify();
            }))
            .on_drag(DragNode { id, name: n.name.clone(), w }, |d, _, _, cx| cx.new(|_| NodeGhost(d.clone())))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_node(id, None, window, cx);
            }))
    }
}


/// 节点页的排序方式。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum NodeSort {
    /// 默认顺序:分组内按添加顺序、手动移动后的位置
    #[default]
    Manual,
    /// 最近添加的在前
    Added,
    /// 最近连接的在前,没连过的排最后
    Connected,
}

impl NodeSort {
    pub(crate) const ALL: [NodeSort; 3] = [NodeSort::Manual, NodeSort::Added, NodeSort::Connected];
    pub(crate) fn label(self) -> &'static str {
        match self {
            NodeSort::Manual => "默认顺序",
            NodeSort::Added => "最近添加",
            NodeSort::Connected => "最近连接",
        }
    }
    pub(crate) fn icon(self) -> IconName {
        match self {
            NodeSort::Manual => IconName::ListOrdered,
            NodeSort::Added => IconName::CalendarPlus,
            NodeSort::Connected => IconName::Clock,
        }
    }
    pub(crate) fn key(self) -> &'static str {
        match self {
            NodeSort::Manual => "manual",
            NodeSort::Added => "added",
            NodeSort::Connected => "connected",
        }
    }
    pub(crate) fn from_key(k: &str) -> Self {
        Self::ALL.into_iter().find(|s| s.key() == k).unwrap_or_default()
    }
}

#[cfg(test)]
mod sort_tests {
    use super::NodeSort;

    #[test]
    fn sort_key_round_trips_and_unknown_falls_back() {
        for s in NodeSort::ALL {
            assert_eq!(NodeSort::from_key(s.key()), s);
        }
        assert_eq!(NodeSort::from_key("???"), NodeSort::Manual);
    }
}
