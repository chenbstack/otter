//! dockyard 首页外观原型(mock 数据)。
//!
//! 三栏:节点树 / 节点菜单 / 内容区(堆栈页)。只为看外观和交互手感,没有任何真实连接。
//! 色板沿用旧版 DeskPanel 的 theme.ts。

mod files;
mod mock;
mod node_form;
mod pages;
mod sidebar;
mod ui;

use std::collections::HashSet;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{VirtualListScrollHandle, v_virtual_list};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ui::*;
use mock::{Group, Node, NodeId, Running, SPARK_N, Service, Stack};

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    Online,
    Offline,
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Overview,
    Containers,
    Stacks,
    Images,
    Volumes,
    Databases,
    Websites,
}

impl Page {
    const ALL: [Page; 7] = [
        Page::Overview,
        Page::Containers,
        Page::Stacks,
        Page::Images,
        Page::Volumes,
        Page::Databases,
        Page::Websites,
    ];
    fn label(self) -> &'static str {
        match self {
            Page::Overview => "概览",
            Page::Containers => "容器",
            Page::Stacks => "堆栈",
            Page::Images => "镜像",
            Page::Volumes => "存储卷",
            Page::Databases => "数据库",
            Page::Websites => "网站",
        }
    }
    fn icon(self) -> IconName {
        match self {
            Page::Overview => IconName::Activity,
            Page::Containers => IconName::Box,
            Page::Stacks => IconName::Layers,
            Page::Images => IconName::Image,
            Page::Volumes => IconName::HardDrive,
            Page::Databases => IconName::Database,
            Page::Websites => IconName::Globe,
        }
    }
}

struct Home {
    groups: Vec<Group>,
    /// 当前选中的节点
    sel: NodeId,
    /// 下一个新节点的 id
    next_id: u32,
    filter: Filter,
    collapsed: HashSet<usize>,
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
    /// 右侧文件管理抽屉
    files: files::Files,
}

impl Home {
    fn new() -> Self {
        let groups = mock::groups();
        let sel = groups[1].nodes[0].id;
        let next_id = groups.iter().flat_map(|g| g.nodes.iter()).map(|n| n.id.0).max().unwrap_or(0) + 1;
        Self {
            groups,
            sel,
            next_id,
            filter: Filter::All,
            collapsed: HashSet::new(),
            page: Page::Stacks,
            hide_charts: false,
            cols: 2,
            svc_w: SVC_MIN_W,
            scroll: VirtualListScrollHandle::new(),
            shot: None,
            form: None,
            files: files::Files::new(),
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

    fn count(&self, f: Filter) -> usize {
        self.all_nodes()
            .filter(|n| match f {
                Filter::All => true,
                Filter::Online => n.online(),
                Filter::Offline => !n.online(),
            })
            .count()
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
            if *frames == 10 {
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
        let titlebar = self.titlebar();
        let sidebar = self.sidebar(cx);
        let menu = self.menu(cx);
        let content = self.content(window, cx);
        let drawer = self.files_drawer(window, cx);
        let dialog = self.node_dialog(window, cx);
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(c::bg())
            .text_color(c::text())
            .child(titlebar)
            .child(div().flex_1().min_h_0().flex().flex_row().child(sidebar).child(menu).child(content))
            .children(drawer)
            .children(dialog)
    }
}

fn main() {
    // 调试参数:--shot <png> --node <g>,<n> --expand <k> --plain --page <0..6> --add(打开添加节点弹窗) --files <host|堆栈/服务> --fcd <目录> --fsel <名字>
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let shot = arg("--shot");
    let node = arg("--node");
    let expand: usize = arg("--expand").and_then(|v| v.parse().ok()).unwrap_or(0);
    let plain = args.iter().any(|a| a == "--plain");
    let page: Option<usize> = arg("--page").and_then(|v| v.parse().ok());
    let add = args.iter().any(|a| a == "--add");
    // --files host | <堆栈>/<服务>  打开文件抽屉;--fsel <名字> 选中一项;--fcd <路径> 进入目录
    let files_arg = arg("--files");
    let fsel = arg("--fsel");
    let fcd = arg("--fcd");

    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1360.0), px(860.0)), cx))),
            titlebar: Some(TitlebarOptions {
                title: Some("DeskPanel".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.0), px(14.0))),
            }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| {
                let mut h = Home::new();
                if let Some(n) = &node {
                    let mut it = n.split(',').filter_map(|x| x.parse::<usize>().ok());
                    if let (Some(g), Some(n)) = (it.next(), it.next()) {
                        if let Some(node) = h.groups.get(g).and_then(|g| g.nodes.get(n)) {
                            h.sel = node.id;
                        }
                    }
                }
                for st in h.node_mut().stacks.iter_mut().take(expand) {
                    st.expanded = true;
                }
                h.hide_charts = plain;
                if let Some(p) = page {
                    h.page = Page::ALL[p.min(6)];
                }
                h.shot = shot.clone().map(|p| (p, 0));
                if add {
                    h.open_add_node(window, cx);
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
