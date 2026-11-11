//! 标签页工作区:标签模型、打开/激活/关闭规则、标签栏。

use std::mem;

use super::*;
use crate::files::{Files, Target};

/// 标签类型。P1 逐步增加终端、文件、日志。
pub enum TabKind {
    /// 节点标签:二级导航 + 页面。每个节点最多一个。
    Node(NodeId),
    /// 文件标签:全尺寸文件管理。可以同一节点开多个。
    Files(NodeId),
}

pub struct Tab {
    pub kind: TabKind,
    /// 该标签的二级页。激活期间的实时值在 `Home.page`,切走时存回这里。
    pub page: Page,
    /// 该标签的列表滚动句柄,存取方式同 `page`。
    pub scroll: VirtualListScrollHandle,
    /// 该标签的文件视图状态:节点标签里是抽屉的,文件标签里是主体的。存取方式同 `page`。
    pub files: Files,
}

impl Tab {
    pub fn node(&self) -> NodeId {
        match self.kind {
            TabKind::Node(id) | TabKind::Files(id) => id,
        }
    }
}

#[derive(Default)]
pub struct Workspace {
    pub tabs: Vec<Tab>,
    pub active: Option<usize>,
}

impl Home {
    /// 打开节点标签:已有就激活,没有就插在当前标签右边再激活。
    pub(crate) fn open_node(&mut self, id: NodeId) {
        if let Some(ix) = self.workspace.tabs.iter().position(|t| matches!(t.kind, TabKind::Node(n) if n == id)) {
            self.activate(ix);
            return;
        }
        let ix = self.workspace.active.map_or(self.workspace.tabs.len(), |a| a + 1);
        self.workspace.tabs.insert(
            ix,
            Tab { kind: TabKind::Node(id), page: Page::Stacks, scroll: VirtualListScrollHandle::new(), files: Files::new() },
        );
        self.activate(ix);
    }

    /// 打开文件标签:总是新开一个,插在当前标签右边。
    pub(crate) fn open_files_tab(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.sel;
        let mut files = Files::for_tab(id, target);
        files.init_search(window, cx);
        let ix = self.workspace.active.map_or(self.workspace.tabs.len(), |a| a + 1);
        self.workspace.tabs.insert(
            ix,
            Tab { kind: TabKind::Files(id), page: Page::Stacks, scroll: VirtualListScrollHandle::new(), files },
        );
        self.activate(ix);
        cx.notify();
    }

    /// 激活标签:把当前标签的页面状态存回去,载入目标标签的,并让节点选中跟随。
    pub(crate) fn activate(&mut self, ix: usize) {
        let ws = &mut self.workspace;
        if ws.active == Some(ix) {
            return;
        }
        if let Some(a) = ws.active {
            ws.tabs[a].page = self.page;
            mem::swap(&mut self.scroll, &mut ws.tabs[a].scroll);
            mem::swap(&mut self.files, &mut ws.tabs[a].files);
        }
        self.page = ws.tabs[ix].page;
        mem::swap(&mut self.scroll, &mut ws.tabs[ix].scroll);
        mem::swap(&mut self.files, &mut ws.tabs[ix].files);
        ws.active = Some(ix);
        self.sel = ws.tabs[ix].node();
    }

    /// 关闭标签:关的是当前标签时激活右侧相邻,没有就左侧;全关后显示空状态。
    pub(crate) fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        let ws = &mut self.workspace;
        if ix >= ws.tabs.len() {
            return;
        }
        let was_active = ws.active == Some(ix);
        ws.tabs.remove(ix);
        if was_active {
            ws.active = None;
            if ws.tabs.is_empty() {
                self.files.open = false;
            } else {
                // 当前标签的实时状态随它一起丢弃,不必存回
                let next = if ix < ws.tabs.len() { ix } else { ix - 1 };
                self.activate(next);
            }
        } else if let Some(a) = ws.active {
            if ix < a {
                ws.active = Some(a - 1);
            }
        }
        cx.notify();
    }

    pub(crate) fn tab_bar(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let active = self.workspace.active;
        let tabs = self.workspace.tabs.iter().enumerate().map(|(ix, tab)| {
            let on = active == Some(ix);
            let (title, online, kind_icon) = {
                let (g, n) = self.locate(tab.node());
                let node = &self.groups[g].nodes[n];
                match tab.kind {
                    TabKind::Node(_) => (node.name.clone(), node.online(), None),
                    TabKind::Files(_) => (format!("{} · 文件", node.name), node.online(), Some(IconName::FolderOpen)),
                }
            };
            let group: SharedString = format!("tab-{ix}").into();
            div()
                .id(("tab", ix))
                .group(group.clone())
                .h_full()
                .flex_shrink_0()
                .min_w(px(120.0))
                .max_w(px(220.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .pl(px(12.0))
                .pr(px(8.0))
                .border_r_1()
                .border_color(c::border())
                .text_size(px(t::TYPE_CONTROL))
                .cursor_pointer()
                .when(on, |d| d.bg(c::bg()).text_color(c::text()).font_weight(FontWeight::MEDIUM))
                .when(!on, |d| d.text_color(c::muted()).hover(|s| s.bg(c::hover()).text_color(c::secondary())))
                .child(match kind_icon {
                    Some(ic) => div().size(px(14.0)).flex_shrink_0().flex().items_center().justify_center().child(icon(ic, 13.0, if on { c::online() } else { c::ghost() })),
                    None => status_dot(if online { c::online() } else { c::ghost() }, online),
                })
                .child(div().flex_1().min_w_0().truncate().child(title))
                .child(
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
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.activate(ix);
                    cx.notify();
                }))
        });
        div()
            .id("tabbar")
            .h(px(TABBAR_H))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .bg(c::chrome())
            .border_b_1()
            .border_color(c::border())
            .overflow_x_scroll()
            .children(tabs)
    }
}
