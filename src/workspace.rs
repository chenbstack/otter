//! 标签页工作区:标签模型、打开/激活/关闭规则。标签的外观在 `titlebar.rs`。

use std::mem;

use super::*;
use crate::files::{Files, Target};

/// 节点标签的三种模式,用标题栏右侧的分段切换。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeMode {
    Terminal,
    Files,
    Containers,
}

impl NodeMode {
    pub const ALL: [NodeMode; 3] = [NodeMode::Terminal, NodeMode::Files, NodeMode::Containers];

    pub fn label(self) -> &'static str {
        match self {
            NodeMode::Terminal => "终端",
            NodeMode::Files => "SFTP",
            NodeMode::Containers => "容器",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            NodeMode::Terminal => IconName::SquareTerminal,
            NodeMode::Files => IconName::FolderOpen,
            NodeMode::Containers => IconName::Server,
        }
    }
}

/// 标签类型。
pub enum TabKind {
    /// 节点页:固定在最左,全局唯一,不可关闭。
    Nodes,
    /// 节点标签:一个标签一条会话,在终端 / SFTP / 容器管理三种模式间切换(`Tab.mode`)。
    /// 同一节点要多个就复制标签。
    Node(NodeId),
    /// 容器终端标签:可以同一容器开多个。P1 只放占位内容。
    Terminal { node: NodeId, container: (String, String), n: usize },
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
    /// 节点标签的 SSH 会话;挂在标签上而不是终端模式上,切到别的模式不断开。关闭标签即丢弃,连接随之断开
    pub term: Option<crate::terminal::TermSession>,
    /// 节点标签的当前模式
    pub mode: NodeMode,
    /// 节点标签里「另一种模式」的文件视图状态:非 SFTP 模式时存 SFTP 的,SFTP 模式时存抽屉的。
    /// 进出 SFTP 模式时与 `Home.files` 对调(见 `set_mode`)。
    pub stash: Files,
    /// 容器模式的 Docker 连接(真实数据);关闭标签即丢弃
    pub docker: Option<crate::docker::Docker>,
}

impl Tab {
    pub(crate) fn new(kind: TabKind, files: Files) -> Self {
        Self {
            kind,
            page: Page::Stacks,
            scroll: VirtualListScrollHandle::new(),
            files,
            follow: true,
            log_scroll: ScrollHandle::new(),
            term: None,
            mode: NodeMode::Containers,
            stash: Files::new(),
            docker: None,
        }
    }

    /// 新节点标签:先当作容器模式,`set_mode` 再切到目标模式。
    pub(crate) fn node_tab(id: NodeId) -> Self {
        let mut t = Self::new(TabKind::Node(id), Files::new());
        t.stash = Files::for_tab(id, Target::Host);
        t
    }

    /// 标签所属的节点;节点页不属于任何节点。
    pub fn node(&self) -> Option<NodeId> {
        match &self.kind {
            TabKind::Nodes => None,
            TabKind::Node(id) => Some(*id),
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
    /// 打开节点标签:已有就激活(`mode` 为空时保持它上次的模式),没有就插在当前标签右边新开。
    /// 新开时 `mode` 为空按终端模式(点卡片的默认入口)。
    pub(crate) fn open_node(&mut self, id: NodeId, mode: Option<NodeMode>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.workspace.tabs.iter().position(|t| matches!(t.kind, TabKind::Node(n) if n == id)) {
            self.activate(ix);
        } else {
            let ix = self.workspace.active.map_or(self.workspace.tabs.len(), |a| a + 1);
            self.workspace.tabs.insert(ix, Tab::node_tab(id));
            self.activate(ix);
            self.set_mode(mode.unwrap_or(NodeMode::Terminal), window, cx);
            return;
        }
        if let Some(m) = mode {
            self.set_mode(m, window, cx);
        }
        self.focus_terminal_if_active(window, cx);
        cx.notify();
    }

    fn focus_terminal_if_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(a) = self.workspace.active {
            let t = &self.workspace.tabs[a];
            if t.mode == NodeMode::Terminal && matches!(t.kind, TabKind::Node(_)) {
                if let Some(term) = &t.term {
                    let fh = term.view.read(cx).focus_handle().clone();
                    window.focus(&fh, cx);
                }
            }
        }
    }

    /// 切换活动节点标签的模式。SFTP 状态与抽屉状态在 `Home.files` 和 `Tab.stash` 间对调;
    /// 第一次进终端模式时才建立 SSH 连接。
    pub(crate) fn set_mode(&mut self, mode: NodeMode, window: &mut Window, cx: &mut Context<Self>) {
        let Some(a) = self.workspace.active else { return };
        if !matches!(self.workspace.tabs[a].kind, TabKind::Node(_)) {
            return;
        }
        let old = self.workspace.tabs[a].mode;
        if old != mode {
            if old == NodeMode::Files || mode == NodeMode::Files {
                mem::swap(&mut self.files, &mut self.workspace.tabs[a].stash);
            }
            self.workspace.tabs[a].mode = mode;
        }
        match mode {
            NodeMode::Files => {
                self.files.init_search(window, cx);
                self.files.open = true;
            }
            NodeMode::Terminal => {
                if self.workspace.tabs[a].term.is_none() {
                    let conn = self.conn_for(self.node().id);
                    let session = self.start_session(conn, window, cx);
                    self.workspace.tabs[a].term = Some(session);
                }
                self.focus_terminal_if_active(window, cx);
            }
            NodeMode::Containers => {}
        }
        cx.notify();
    }

    /// 复制标签:同节点同模式的新标签,插在原标签右边;二级页复制,会话重新建立。
    pub(crate) fn duplicate_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tabs.get(ix) else { return };
        let TabKind::Node(id) = tab.kind else { return };
        let (mode, page) = (tab.mode, if self.workspace.active == Some(ix) { self.page } else { tab.page });
        self.workspace.tabs.insert(ix + 1, Tab::node_tab(id));
        self.workspace.tabs[ix + 1].page = page;
        self.activate(ix + 1);
        self.set_mode(mode, window, cx);
    }

    /// 打开容器终端标签:总是新开一个,编号按同一容器已有的终端数递增。
    pub(crate) fn open_terminal(&mut self, container: (String, String), cx: &mut Context<Self>) {
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
