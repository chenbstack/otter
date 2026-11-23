//! 标签页工作区:标签模型、打开/激活/关闭规则。标签的外观在 `titlebar.rs`。

use std::mem;

use super::*;
use crate::files::{Files, Target};

/// 标签类型。P1 逐步增加终端、文件、日志。
pub enum TabKind {
    /// 节点页:固定在最左,全局唯一,不可关闭。
    Nodes,
    /// 节点标签:二级导航 + 页面。每个节点最多一个。
    Node(NodeId),
    /// 文件标签:全尺寸文件管理。可以同一节点开多个。
    Files(NodeId),
    /// 终端标签:可以同一目标开多个。`container` 为空表示服务器本机。P1 只放占位内容。
    Terminal { node: NodeId, container: Option<(String, String)>, n: usize },
    /// 日志标签:每个容器一个。
    Logs { node: NodeId, stack: String, svc: String },
}

pub struct Tab {
    pub kind: TabKind,
    /// 该标签的二级页。激活期间的实时值在 `Home.page`,切走时存回这里。
    pub page: Page,
    /// 该标签的列表滚动句柄,存取方式同 `page`。
    pub scroll: VirtualListScrollHandle,
    /// 该标签的文件视图状态:节点标签里是抽屉的,文件标签里是主体的。存取方式同 `page`。
    pub files: Files,
    /// 日志标签:是否跟随最新,以及日志列表的滚动句柄(只在日志标签里用,不随标签切换交换)
    pub follow: bool,
    pub log_scroll: ScrollHandle,
    /// 服务器终端标签的 SSH 会话;关闭标签即丢弃,连接随之断开
    pub term: Option<crate::terminal::TermSession>,
}

impl Tab {
    pub(crate) fn new(kind: TabKind, files: Files) -> Self {
        Self { kind, page: Page::Stacks, scroll: VirtualListScrollHandle::new(), files, follow: true, log_scroll: ScrollHandle::new(), term: None }
    }

    /// 标签所属的节点;节点页不属于任何节点。
    pub fn node(&self) -> Option<NodeId> {
        match &self.kind {
            TabKind::Nodes => None,
            TabKind::Node(id) | TabKind::Files(id) => Some(*id),
            TabKind::Terminal { node, .. } | TabKind::Logs { node, .. } => Some(*node),
        }
    }
}

pub struct Workspace {
    pub tabs: Vec<Tab>,
    pub active: Option<usize>,
}

impl Default for Workspace {
    /// 只有固定的节点页标签,并停在它上面。
    fn default() -> Self {
        Self { tabs: vec![Tab::new(TabKind::Nodes, Files::new())], active: Some(0) }
    }
}

impl Home {
    /// 打开节点标签:已有就激活,没有就插在当前标签右边再激活。
    pub(crate) fn open_node(&mut self, id: NodeId) {
        if let Some(ix) = self.workspace.tabs.iter().position(|t| matches!(t.kind, TabKind::Node(n) if n == id)) {
            self.activate(ix);
            return;
        }
        let ix = self.workspace.active.map_or(self.workspace.tabs.len(), |a| a + 1);
        self.workspace.tabs.insert(ix, Tab::new(TabKind::Node(id), Files::new()));
        self.activate(ix);
    }

    /// 打开文件标签:总是新开一个,插在当前标签右边。
    pub(crate) fn open_files_tab(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.sel;
        let mut files = Files::for_tab(id, target);
        files.init_search(window, cx);
        let ix = self.workspace.active.map_or(self.workspace.tabs.len(), |a| a + 1);
        self.workspace.tabs.insert(ix, Tab::new(TabKind::Files(id), files));
        self.activate(ix);
        cx.notify();
    }

    /// 打开终端标签:总是新开一个,编号按同一目标已有的终端数递增。
    pub(crate) fn open_terminal(&mut self, container: Option<(String, String)>, cx: &mut Context<Self>) {
        let node = self.sel;
        let n = 1 + self
            .workspace
            .tabs
            .iter()
            .filter(|t| matches!(&t.kind, TabKind::Terminal { node: nn, container: cc, .. } if *nn == node && *cc == container))
            .count();
        let ix = self.workspace.active.map_or(self.workspace.tabs.len(), |a| a + 1);
        self.workspace.tabs.insert(ix, Tab::new(TabKind::Terminal { node, container, n }, Files::new()));
        self.activate(ix);
        cx.notify();
    }

    /// 打开日志标签:每个容器一个,已有就激活。
    pub(crate) fn open_logs(&mut self, stack: String, svc: String, cx: &mut Context<Self>) {
        let node = self.sel;
        let found = self
            .workspace
            .tabs
            .iter()
            .position(|t| matches!(&t.kind, TabKind::Logs { node: nn, stack: ss, svc: vv } if *nn == node && *ss == stack && *vv == svc));
        match found {
            Some(ix) => self.activate(ix),
            None => {
                let ix = self.workspace.active.map_or(self.workspace.tabs.len(), |a| a + 1);
                self.workspace.tabs.insert(ix, Tab::new(TabKind::Logs { node, stack, svc }, Files::new()));
                self.activate(ix);
            }
        }
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
        if let Some(n) = ws.tabs[ix].node() {
            self.sel = n;
        }
    }

    /// 关闭标签:关的是当前标签时激活右侧相邻,没有就左侧(最左永远是节点页)。节点页不可关闭。
    pub(crate) fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        let ws = &mut self.workspace;
        if ix >= ws.tabs.len() || matches!(ws.tabs[ix].kind, TabKind::Nodes) {
            return;
        }
        let was_active = ws.active == Some(ix);
        ws.tabs.remove(ix);
        if was_active {
            ws.active = None;
            // 当前标签的实时状态随它一起丢弃,不必存回
            let next = if ix < ws.tabs.len() { ix } else { ix - 1 };
            self.activate(next);
        } else if let Some(a) = ws.active {
            if ix < a {
                ws.active = Some(a - 1);
            }
        }
        cx.notify();
    }
}
