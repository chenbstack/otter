//! otter 首页外观原型(mock 数据)。
//!
//! 标题栏(含标签)+ 内容区:节点页 / 节点标签(二级导航 + 页面)等。只为看外观和交互手感,没有任何真实连接。
#![recursion_limit = "512"]
// release 版在 Windows 上不弹控制台窗口(CLI 是另一个二进制 otter.exe)
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod confirm;
mod ctl;
mod ipc;
mod ipc_server;
mod platform;
mod takeover;
mod daemon_conf;
mod sysops;
mod ctfiles;
mod docker;
mod json_hl;
mod group_form;
mod import;
mod files;
mod keychain;
mod known_hosts;
mod logs;
mod secrets;
mod sftp;
mod store;
mod mock;
mod monitor;
mod node_form;
mod nodes;
mod nodes_side;
mod assets;
mod osinfo;
mod pages;
mod terminal;
mod titlebar;
mod ui;
mod update;
mod update_ui;
mod workspace;

use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{VirtualListScrollHandle, v_virtual_list};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ui::*;
use workspace::Workspace;
use mock::{Group, Node, NodeId, Running, SPARK_N, Service, Stack};

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Overview,
    Containers,
    Stacks,
    Images,
    Volumes,
    Registry,
    Config,
    Processes,
    Services,
    Storage,
    Cron,
}

impl Page {
    const ALL: [Page; 11] = [
        Page::Overview,
        Page::Containers,
        Page::Stacks,
        Page::Images,
        Page::Volumes,
        Page::Registry,
        Page::Config,
        Page::Processes,
        Page::Services,
        Page::Storage,
        Page::Cron,
    ];
    /// 容器相关的页(侧栏的「容器」项下面用页签切换)
    fn is_docker(self) -> bool {
        matches!(self, Page::Containers | Page::Stacks | Page::Images | Page::Volumes | Page::Registry | Page::Config)
    }
    /// 服务器运维页:读 SSH 命令的输出,不依赖 Docker
    fn is_sys(self) -> bool {
        matches!(self, Page::Processes | Page::Services | Page::Storage | Page::Cron)
    }
    fn label(self) -> &'static str {
        match self {
            Page::Overview => "概览",
            Page::Containers => "容器",
            Page::Stacks => "堆栈",
            Page::Images => "镜像",
            Page::Volumes => "存储卷",
            Page::Registry => "仓库",
            Page::Config => "配置",
            Page::Processes => "进程",
            Page::Services => "服务",
            Page::Storage => "存储",
            Page::Cron => "计划任务",
        }
    }
    fn icon(self) -> IconName {
        match self {
            Page::Overview => IconName::Activity,
            Page::Containers => IconName::Box,
            Page::Stacks => IconName::Layers,
            Page::Images => IconName::Image,
            Page::Volumes => IconName::HardDrive,
            Page::Registry => IconName::KeyRound,
            Page::Config => IconName::FileCog,
            Page::Processes => IconName::Cpu,
            Page::Services => IconName::Settings2,
            Page::Storage => IconName::HardDrive,
            Page::Cron => IconName::CalendarClock,
        }
    }
}

struct Home {
    groups: Vec<Group>,
    /// 标签页
    workspace: Workspace,
    /// 当前选中的节点(跟随当前标签)
    sel: NodeId,
    /// 下一个新节点的 id
    next_id: u32,
    /// 节点页的搜索框
    node_search: Option<Entity<gpui_kit::component::input::InputState>>,
    /// 节点页当前进入的分组(None = 全部节点)
    node_group: Option<usize>,
    /// 节点页卡片右键菜单:(目标, 鼠标位置)
    node_menu: Option<(nodes::NodeMenuTarget, Point<Pixels>)>,
    /// 待确认删除的分组 id(非空分组)
    group_delete: Option<i64>,
    /// 待确认删除的节点 id
    node_delete: Option<NodeId>,
    /// 新建分组弹窗
    group_form: Option<group_form::GroupForm>,
    /// 标签右键菜单:(标签下标, 鼠标位置)
    tab_menu: Option<(usize, Point<Pixels>)>,
    /// 节点页侧栏当前视图
    nodes_view: nodes::NodesView,
    /// 已知主机视图的缓存(切到该视图或删除后重新读文件)
    known_hosts: Vec<known_hosts::KnownHost>,
    /// 「信任」页的 Agent 授权列表
    grants: Vec<store::AgentGrant>,
    /// 钥匙串里的密钥(含被几个节点使用)
    keys: Vec<store::KeyRow>,
    /// 钥匙串页的弹窗:添加 / 重命名 / 删除
    key_modal: Option<keychain::KeyModal>,
    /// 节点导入预览弹窗
    import: Option<import::ImportDialog>,
    /// 节点页「新建节点」旁的下拉菜单是否展开
    add_menu: bool,
    /// 刚复制过公钥的密钥 id(图标短暂变成对勾)
    key_copied: Option<i64>,
    page: Page,
    hide_charts: bool,
    cols: usize,
    /// 当前每张服务卡片的宽度,用来估算端口能显示几个
    svc_w: f32,
    scroll: VirtualListScrollHandle,
    /// 离屏截图:(输出路径, 已渲染帧数)。仅用于 `--shot`。
    shot: Option<(String, u32)>,
    /// 添加 / 编辑节点弹窗
    form: Option<node_form::NodeForm>,
    /// 本地配置库;`--demo` 或库打不开时为 None
    store: Option<store::Store>,
    /// 秘密加密器;第一次用到时才向系统凭据库取主密钥(避免启动就弹授权)
    vault: Option<secrets::Vault>,
    /// 当前标签的文件视图状态(节点标签里是抽屉,文件标签里是主体),随标签切换
    files: files::Files,
    /// 演示模式(mock 数据,不连服务器)
    demo: bool,
    /// 容器模式各页的界面状态和当前快照
    dock: pages::dockerpages::DockUi,
    /// 已经(或正在)探测过系统的节点,同一次运行里不重复探测
    os_probed: std::collections::HashSet<NodeId>,
    /// 自动更新状态(标题栏右侧的下载图标)
    update: update::State,
    /// 各文件视图共享的 mock 上传/新建/删除状态
    fstore: files::Store,
    /// 等用户确认的 agent 接管申请(队首正在弹窗)
    takeover_prompts: std::collections::VecDeque<takeover::Prompt>,
    /// 等用户确认的提权(sudo 代填)请求
    sudo_prompt: Option<takeover::SudoPrompt>,
    /// 被结束但 agent 还没得知的接管:(连接 id, 会话 id) → 原因
    takeover_revoked: std::collections::HashMap<(u64, u64), &'static str>,
    /// 持久接管(lease)被结束的原因,按令牌记,下一个带着令牌来的连接能知道
    lease_revoked: std::collections::HashMap<String, &'static str>,
    /// 接管遮罩的焦点句柄:接管期间焦点放在它上面,键盘到不了终端
    takeover_focus: Option<FocusHandle>,
    /// 节点页的排序方式(存在设置里,重启后保留)
    node_sort: nodes::NodeSort,
    /// 排序下拉菜单是否展开
    sort_menu: bool,
    /// 已经记过「最近连接」的 (标签 id, 连接代数),避免每帧写库
    connect_noted: std::collections::HashSet<(u64, u32)>,
}

impl Home {
    /// `demo`:载入 mock 示例数据且不碰本地库(`--demo`;截图也用它,保证画面可复现)。
    /// 否则打开本地库;库打不开时给出日志并退回空列表(此时新增的节点只在内存里)。
    fn new(demo: bool) -> Self {
        let mut store = None;
        let groups = if demo {
            mock::groups()
        } else {
            match store::Store::open_default().and_then(|s| s.load_groups().map(|g| (s, g))) {
                Ok((s, g)) => {
                    files::restore_prefs(&s);
                    terminal::highlight::restore(&s);
                    store = Some(s);
                    g
                }
                Err(e) => {
                    logs::push(logs::Level::Error, "本地库", &format!("{e};本次运行的节点不会保存"));
                    vec![Group { id: 0, name: "未分组".into(), color: 0x7d8ea6, nodes: vec![] }]
                }
            }
        };
        // 没有节点时 sel 为 0(「无」)
        let sel = groups.iter().flat_map(|g| g.nodes.iter()).map(|n| n.id).next().unwrap_or(NodeId(0));
        let next_id = groups.iter().flat_map(|g| g.nodes.iter()).map(|n| n.id.0).max().unwrap_or(0) + 1;
        let keys = match &store {
            Some(s) => s.list_keys().unwrap_or_default(),
            None if demo => mock::keys(),
            None => vec![],
        };
        let node_sort = store.as_ref().and_then(|s| s.get_setting("node_sort")).map_or(nodes::NodeSort::Manual, |v| nodes::NodeSort::from_key(&v));
        Self {
            store,
            vault: None,
            groups,
            workspace: Workspace::default(),
            sel,
            next_id,
            node_search: None,
            node_group: None,
            node_menu: None,
            group_delete: None,
            node_delete: None,
            group_form: None,
            tab_menu: None,
            nodes_view: nodes::NodesView::Hosts,
            known_hosts: Vec::new(),
            grants: Vec::new(),
            keys,
            key_modal: None,
            import: None,
            add_menu: false,
            key_copied: None,
            page: Page::Overview,
            hide_charts: false,
            cols: 2,
            svc_w: SVC_MIN_W,
            scroll: VirtualListScrollHandle::new(),
            shot: None,
            form: None,
            files: files::Files::new(),
            demo,
            dock: pages::dockerpages::DockUi::new(),
            os_probed: Default::default(),
            update: update::State::Idle,
            fstore: files::Store::default(),
            takeover_prompts: Default::default(),
            sudo_prompt: None,
            takeover_revoked: Default::default(),
            lease_revoked: Default::default(),
            takeover_focus: None,
            node_sort,
            sort_menu: false,
            connect_noted: Default::default(),
        }
    }

    /// 节点所在的 (分组下标, 节点下标)。id 不存在时退回第一个节点。
    fn locate(&self, id: NodeId) -> (usize, usize) {
        for (gi, g) in self.groups.iter().enumerate() {
            if let Some(ni) = g.nodes.iter().position(|n| n.id == id) {
                return (gi, ni);
            }
        }
        (0, 0)
    }

    fn node(&self) -> &Node {
        let (g, n) = self.locate(self.sel);
        &self.groups[g].nodes[n]
    }

    fn node_mut(&mut self) -> &mut Node {
        let (g, n) = self.locate(self.sel);
        &mut self.groups[g].nodes[n]
    }

    fn all_nodes(&self) -> impl Iterator<Item = &Node> {
        self.groups.iter().flat_map(|g| g.nodes.iter())
    }

    fn stack_height(&self, s: &Stack) -> f32 {
        let mut h = STACK_HEADER_H;
        if s.expanded {
            let rows = s.services.len().div_ceil(self.cols) as f32;
            let svc_h = if self.hide_charts { SVC_H_PLAIN } else { SVC_H_CHART };
            h += STACK_BODY_PAD * 2.0 + rows * svc_h + (rows - 1.0).max(0.0) * SVC_GAP;
        }
        h + STACK_GAP
    }
}

impl Render for Home {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((path, frames)) = &mut self.shot {
            *frames += 1;
            let target: u32 = std::env::var("OTTER_SHOT_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
            if *frames == target {
                match window.render_to_image() {
                    Ok(img) => match img.save(&*path) {
                        Ok(()) => println!("截图已保存: {path}"),
                        Err(e) => println!("保存失败: {e}"),
                    },
                    Err(e) => println!("离屏渲染失败: {e}"),
                }
                cx.quit();
            } else {
                window.request_animation_frame();
            }
        }
        self.sync_docker(window, cx);
        self.sync_os(cx);
        self.sync_last_connected();
        let titlebar = self.titlebar(window, cx);
        let active_kind = self.workspace.active.map(|a| &self.workspace.tabs[a].kind);
        let mode = self.workspace.active.map(|a| self.workspace.tabs[a].mode);
        let on_node_tab = matches!(active_kind, Some(workspace::TabKind::Node(_)));
        let is_files_tab = on_node_tab && mode == Some(workspace::NodeMode::Files);
        let body = match active_kind {
            Some(workspace::TabKind::Terminal { container, .. }) => {
                let node = self.node();
                div().flex_1().min_h_0().flex().flex_col().child(self.terminal_view(&node.name, &node.user_host, Some(container)))
            }
            Some(workspace::TabKind::Logs { stack, svc, .. }) => {
                let a = self.workspace.active.unwrap_or(0);
                div().flex_1().min_h_0().flex().flex_col().child(self.logs_view(a, stack, svc, cx))
            }
            Some(workspace::TabKind::Node(_)) => match mode {
                Some(workspace::NodeMode::Terminal) if self.active_term().is_some() => {
                    div().flex_1().min_h_0().flex().flex_col().child(self.ssh_terminal_view(cx))
                }
                Some(workspace::NodeMode::Files) => div().flex_1().min_h_0().flex().flex_col().child(self.files_tab(window, cx)),
                _ => {
                    let menu = self.menu(cx);
                    let content = self.content(window, cx);
                    div().flex_1().min_h_0().flex().flex_row().child(menu).child(content)
                }
            },
            Some(workspace::TabKind::Nodes) | None => div().flex_1().min_h_0().flex().flex_col().child(self.nodes_page(window, cx)),
        };
        // 终端连接中断、自动重连时,整个标签内容区盖一层加载状态(透过它能看到后面的内容)
        let overlay = self.reconnect_overlay(cx);
        let takeover = self.takeover_overlay(cx);
        let body = div().relative().flex_1().min_h_0().flex().flex_col().child(body).children(overlay).children(takeover);
        let drawer = if is_files_tab { None } else { self.files_drawer(window, cx) };
        let dialog = self.sudo_prompt_dialog(window, cx).or_else(|| self.takeover_prompt_dialog(cx)).or_else(|| self.node_dialog(window, cx)).or_else(|| self.key_modal_view(window, cx)).or_else(|| self.import_dialog(cx)).or_else(|| self.group_delete_dialog(cx)).or_else(|| self.node_delete_dialog(cx)).or_else(|| self.group_form_dialog(window, cx));
        // 容器页的抽屉、确认框、提示盖在整个窗口上(含标题栏)
        let dock_layer = if self.dock_visible() { self.dock_overlays(window, cx) } else { Vec::new() };
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(c::bg())
            .text_color(c::text())
            .child(backdrop())
            .child(titlebar)
            .child(body)
            .children(drawer)
            .children(dock_layer)
            .children(dialog)
            // 接管确认框弹出时,标题栏盖在遮罩上面,让被申请的标签保持醒目
            .children(if self.takeover_prompts.is_empty() && self.sudo_prompt.is_none() { None } else { Some(self.titlebar_spotlight(window, cx)) })
            .children(self.tab_menu_overlay(cx))
            .children(self.node_menu_overlay(cx))
    }
}

fn main() {
    update::wait_for_old_process();
    // 调试参数:--shot <png> --node <g>,<n> --tabs n:<节点名>(容器模式),t:<节点名>(终端模式),f:<节点名>[@堆栈/服务](SFTP 模式),... --active <k> --expand <k> --plain --page <0..10> --add(打开添加节点弹窗) --files <host|堆栈/服务> --fcd <目录> --fsel <名字>
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let shot = arg("--shot");
    let node = arg("--node");
    let expand: usize = arg("--expand").and_then(|v| v.parse().ok()).unwrap_or(0);
    let plain = args.iter().any(|a| a == "--plain");
    let page: Option<usize> = arg("--page").and_then(|v| v.parse().ok());
    let add = args.iter().any(|a| a == "--add");
    // --empty:截图用,空节点列表(不读写本地库)
    let empty = args.iter().any(|a| a == "--empty");
    // --demo 或 --shot:用 mock 示例数据,不读写本地库;
    // 调试:OTTER_SHOT_REAL=1 让 --shot 也读本地库(配合临时 HOME 验证真实 SFTP 画面)
    let demo = args.iter().any(|a| a == "--demo") || (shot.is_some() && std::env::var_os("OTTER_SHOT_REAL").is_none());
    // --tabs n:节点名,f:节点名[@堆栈/服务]  清空后依次打开标签;--active k 激活第 k 个(从 0 起)
    let tabs_arg = arg("--tabs");
    let active_arg: Option<usize> = arg("--active").and_then(|v| v.parse().ok());
    // --files host | <堆栈>/<服务>  打开文件抽屉;--fsel <名字> 选中一项;--fcd <路径> 进入目录
    let files_arg = arg("--files");
    let fsel = arg("--fsel");
    let fcd = arg("--fcd");
    // --mode terminal|files|containers  切换活动节点标签的模式
    let mode_arg = arg("--mode");
    // --ssh user@host[:port]  把第一个节点的连接地址改成它,并直接打开它的终端
    let ssh_arg = arg("--ssh");
    // --nview 0..3  节点页侧栏视图:主机、钥匙串、已知主机、日志(配合 --active 0)
    let nview: Option<usize> = arg("--nview").and_then(|v| v.parse().ok());

    gpui_kit::application().with_assets(assets::AppAssets).run(move |cx| {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1360.0), px(860.0)), cx))),
            titlebar: Some(TitlebarOptions {
                title: Some("Otter".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.0), px(16.0))),
            }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| {
                let mut h = Home::new(demo);
                if empty {
                    h.groups = vec![Group { id: 0, name: "未分组".into(), color: 0x7d8ea6, nodes: vec![] }];
                    h.sel = NodeId(0);
                }
                h.init_node_search(window, cx);
                if let Some(n) = &node {
                    let mut it = n.split(',').filter_map(|x| x.parse::<usize>().ok());
                    if let (Some(g), Some(n)) = (it.next(), it.next()) {
                        if let Some(node) = h.groups.get(g).and_then(|g| g.nodes.get(n)) {
                            let id = node.id;
                            h.workspace = Workspace::default();
                            h.open_node(id, Some(workspace::NodeMode::Containers), window, cx);
                        }
                    }
                }
                if let Some(spec) = &tabs_arg {
                    h.workspace = Workspace::default();
                    for item in spec.split(',') {
                        let Some((kind, rest)) = item.split_once(':') else { continue };
                        let (name, ct) = match rest.split_once('@') {
                            Some((n, ct)) => (n, Some(ct)),
                            None => (rest, None),
                        };
                        let Some(id) = h.all_nodes().find(|n| n.name == name).map(|n| n.id) else { continue };
                        match kind {
                            "n" => h.open_node(id, Some(workspace::NodeMode::Containers), window, cx),
                            "t" => {
                                // t:节点名 = 节点标签的终端模式;t:节点名@堆栈/服务 = 容器终端标签
                                h.sel = id;
                                match ct.and_then(|c| c.split_once('/')) {
                                    Some((a, b)) => h.open_terminal((a.to_string(), b.to_string()), cx),
                                    None => h.open_node(id, Some(workspace::NodeMode::Terminal), window, cx),
                                }
                            }
                            "l" => {
                                h.sel = id;
                                if let Some((a, b)) = ct.and_then(|c| c.split_once('/')) {
                                    h.open_logs(a.into(), b.into(), cx);
                                }
                            }
                            "f" => {
                                // f:节点名 = 节点标签的 SFTP 模式(服务器文件);f:节点名@堆栈/服务 = 同上但指向容器
                                h.sel = id;
                                h.open_node(id, Some(workspace::NodeMode::Files), window, cx);
                                if let Some((stack, svc)) = ct.and_then(|c| c.split_once('/')) {
                                    h.files.retarget(files::Target::Container { stack: stack.into(), svc: svc.into() });
                                }
                                h.files.debug_jump(fcd.clone(), fsel.clone());
                            }
                            _ => {}
                        }
                    }
                    if let Some(k) = active_arg.filter(|k| *k < h.workspace.tabs.len()) {
                        h.activate(k);
                    }
                }
                if let Some(addr) = &ssh_arg {
                    let id = h.groups[0].nodes[0].id;
                    h.groups[0].nodes[0].user_host = addr.clone();
                    h.open_node(id, Some(workspace::NodeMode::Terminal), window, cx);
                }
                if let Some(m) = mode_arg.as_deref() {
                    let m = match m {
                        "files" => workspace::NodeMode::Files,
                        "containers" => workspace::NodeMode::Containers,
                        _ => workspace::NodeMode::Terminal,
                    };
                    h.set_mode(m, window, cx);
                }
                if let Some(k) = nview {
                    h.activate(0);
                    h.set_nodes_view(nodes::NodesView::ALL[k.min(3)], cx);
                    if let Ok(kind) = std::env::var("OTTER_KEY_MODAL") {
                        h.debug_key_modal(&kind, window, cx);
                    }
                }
                h.add_menu = std::env::var("OTTER_ADD_MENU").is_ok();
                h.sort_menu = std::env::var("OTTER_SORT_MENU").is_ok();
                // 调试:OTTER_GROUP_FORM=1 截图时直接打开新建分组弹窗
                if std::env::var("OTTER_GROUP_FORM").is_ok() {
                    h.open_add_group(window, cx);
                }
                // 调试:OTTER_NODE_DELETE=1 截图时弹出第一个节点的删除确认框
                if std::env::var("OTTER_NODE_DELETE").is_ok() {
                    h.node_delete = h.groups.iter().flat_map(|g| g.nodes.iter()).map(|n| n.id).next();
                }
                if std::env::var("OTTER_IMPORT_OPEN").is_ok() {
                    h.open_import(window, cx);
                }
                if std::env::var("OTTER_IMPORT_SSH").is_ok() {
                    h.load_ssh_config(cx);
                }
                if let Ok(f) = std::env::var("OTTER_IMPORT") {
                    h.load_import_file(std::path::Path::new(&f), cx);
                }
                if h.all_nodes().next().is_some() {
                    for st in h.node_mut().stacks.iter_mut().take(expand) {
                        st.expanded = true;
                    }
                }
                h.hide_charts = plain;
                if let Some(p) = page {
                    h.page = Page::ALL[p.min(Page::ALL.len() - 1)];
                }
                h.shot = shot.clone().map(|p| (p, 0));
                // 截图/调试:假装发现了新版本(OTTER_FAKE_UPDATE=<版本号>)
                if let Ok(v) = std::env::var("OTTER_FAKE_UPDATE") {
                    h.update = update::State::Available(update::Update { version: v, page: "https://github.com/".into(), asset: None });
                }
                if let Ok(m) = std::env::var("OTTER_TAKEOVER") {
                    h.debug_takeover(&m, window, cx);
                }
                if !demo && shot.is_none() {
                    h.start_activity_watch(window, cx);
                    h.start_update_check(cx);
                    h.start_ctl(window, cx);
                    if let Some(msg) = platform::install_cli_link() {
                        logs::push(logs::Level::Info, "Otter", msg);
                    }
                }
                if add {
                    h.open_add_node(window, cx);
                    if std::env::var_os("OTTER_FORM_KEY").is_some() {
                        h.debug_form_key();
                    }
                    if std::env::var_os("OTTER_ADD_MENU").is_some() {
                        h.debug_open_group_menu();
                    }
                }
                if let Some(t) = &files_arg {
                    let target = match t.split_once('/') {
                        Some((stack, svc)) => files::Target::Container { stack: stack.into(), svc: svc.into() },
                        None => files::Target::Host,
                    };
                    h.open_files(target, window, cx);
                    h.files.debug_jump(fcd.clone(), fsel.clone());
                }
                h
            })
        })
        .expect("failed to open window");
        cx.activate(true);
    });
}
