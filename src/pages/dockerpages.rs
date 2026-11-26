//! 容器模式的真实数据页:概览、容器、镜像、存储卷,以及它们和堆栈页共用的状态同步、连接/错误页、确认框。
//!
//! 数据来自 `crate::docker`(挂在节点标签上的 `Tab.docker`);`--demo` / `--shot` 没有真实连接,
//! 用 `docker::demo_snapshot` 把 mock 堆栈拼成同样的快照,界面只读 `Snapshot`。

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};

use super::super::*;
use crate::docker::{self, Action, Container, Docker, Problem, Snapshot};
use crate::terminal::connecting::ConnTarget;

pub(crate) const DOCKER_STEP_LABELS: [&str; 3] = ["连接服务器", "身份认证", "检测 Docker"];
const NOTICE_SECS: u64 = 4;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SortKey {
    Name,
    State,
    Cpu,
    Mem,
}

/// 等用户确认的危险操作(弹窗用删除确认框 `Home::delete_confirm` 渲染)
pub(crate) struct DockAsk {
    pub(super) title: &'static str,
    pub(super) tile: IconName,
    pub(super) name: String,
    pub(super) detail: &'static str,
    pub(super) body: &'static str,
    pub(super) label: &'static str,
    pub(super) action: Action,
    /// 会被影响的对象清单(清理镜像时列出未使用的镜像),为空就不列
    pub(super) list: Vec<String>,
}

pub(crate) struct DockUi {
    /// 当前页读的快照(真实或 demo)
    pub snap: Arc<Snapshot>,
    /// 调试:截图时等数据库实例出现后再打开的弹层(见 `debug_dbm`)
    pub dbm_dbg: Option<String>,
    search: Option<Entity<InputState>>,
    pull: Option<Entity<InputState>>,
    pub(super) pull_open: bool,
    /// 一键安装 Docker 时手动选的安装源(None 是自动);下拉是否展开
    inst_src: Option<crate::docker_install::Source>,
    inst_src_open: bool,
    /// 安装确认框里「配置镜像加速」的勾选;打开确认框时按 Docker Hub 实测速度给默认值
    inst_accel: bool,
    /// 安装页是否展开日志(默认收起)
    inst_log_open: bool,
    /// 拉取时选的仓库地址(None 是不选,走默认的 Docker Hub)
    pull_reg: Option<String>,
    pull_reg_open: bool,
    pub conf: super::daemonpage::ConfUi,
    pub reg: super::registrypage::RegUi,
    pub sys: super::syspages::SysUi,
    pub ct: super::ctpanel::CtUi,
    sort: (SortKey, bool),
    confirm: Option<DockAsk>,
    pub(super) comp: super::composeedit::CompUi,
    pub(super) api: super::installui::AppUi,
    pub(super) dbm: super::dbmanage::DbMgr,
    pub(super) notice: Option<(SharedString, Instant)>,
    /// 刚点下的容器操作(容器名 → 动词, 点击时刻):后台标记「进行中」前先让按钮转起来
    pub(super) pending: Vec<(String, String, Instant)>,
    polling: bool,
    /// 上次读到的 `Docker::version`
    seen: u64,
    /// 当前同步的标签,变了就清空快照
    key: Option<(usize, NodeId)>,
    /// 概览页的整机监控读数、不能监控的原因、进程是否按内存排
    pub mon: Option<Arc<crate::monitor::MonSnapshot>>,
    pub mon_err: Option<SharedString>,
    pub proc_by_mem: bool,
    /// 内容区宽度(进程表窄了就收掉次要的列)
    pub view_w: f32,
}

const PENDING_MIN_MS: u64 = 600;
const PENDING_MAX_SECS: u64 = 60;

impl DockUi {
    pub fn new() -> Self {
        Self { snap: Arc::new(Snapshot::default()), dbm_dbg: None, search: None, pull: None, pull_open: false, pull_reg: None, pull_reg_open: false, inst_src: None, inst_src_open: false, inst_accel: false, inst_log_open: false, conf: Default::default(), reg: Default::default(), sys: Default::default(), ct: Default::default(), comp: Default::default(), api: Default::default(), dbm: Default::default(), sort: (SortKey::Name, true), confirm: None, notice: None, pending: Vec::new(), polling: false, seen: 0, key: None, mon: None, mon_err: None, proc_by_mem: false, view_w: 1200.0 }
    }
}

pub(super) fn small(s: impl Into<SharedString>, color: Rgba) -> Div {
    div().text_size(px(t::TYPE_CAPTION)).text_color(color).child(s.into())
}

pub(crate) fn head(label: &'static str) -> Div {
    div().text_size(px(t::TYPE_CAPTION)).font_weight(FontWeight::SEMIBOLD).text_color(c::ghost()).child(label)
}

/// 表格容器:表头 + 可滚动的行
pub(crate) fn table(id: &'static str, header: Div, rows: Vec<Stateful<Div>>) -> Div {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .rounded(px(t::RADIUS_XL))
        .border_1()
        .border_color(c::dialog_border())
        .overflow_hidden()
        .child(header.h(px(34.0)).flex_shrink_0().flex().flex_row().items_center().gap(px(12.0)).px(px(16.0)).border_b_1().border_color(c::border()))
        .child(div().id(id).flex_1().min_h_0().overflow_y_scroll().children(rows))
}

/// 整页一起滚动时的表格:自己不滚,有多少行就多高(外层页面负责滚动)
pub(crate) fn table_flow(header: Div, rows: Vec<Stateful<Div>>) -> Div {
    div()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .rounded(px(t::RADIUS_XL))
        .border_1()
        .border_color(c::dialog_border())
        .overflow_hidden()
        .child(header.h(px(34.0)).flex_shrink_0().flex().flex_row().items_center().gap(px(12.0)).px(px(16.0)).border_b_1().border_color(c::border()))
        .children(rows)
}

pub(crate) fn row(id: (&'static str, usize)) -> Stateful<Div> {
    div().id(id).h(px(44.0)).flex_shrink_0().flex().flex_row().items_center().gap(px(12.0)).px(px(16.0)).border_b_1().border_color(c::border()).hover(|s| s.bg(c::field_hover()))
}

/// 列表操作列用的窄按钮(24px 正方形,和卡片上的按钮一致)
pub(crate) fn icon_btn_c(id: (&'static str, usize), ic: IconName, color: Rgba) -> Stateful<Div> {
    icon_btn(id, ic, color).size(px(24.0))
}

/// `busy` 时把图标换成加载动画(这时不要再挂点击)
pub(crate) fn icon_btn_busy(id: (&'static str, usize), ic: IconName, color: Rgba, busy: bool) -> Stateful<Div> {
    if busy {
        div().id(id).size(px(26.0)).flex().items_center().justify_center().child(dot_spinner(id, color))
    } else {
        icon_btn(id, ic, color)
    }
}

pub(crate) fn icon_btn(id: (&'static str, usize), ic: IconName, color: Rgba) -> Stateful<Div> {
    div().id(id).size(px(26.0)).flex().items_center().justify_center().rounded(px(t::RADIUS_XS)).cursor_pointer().hover(|s| s.bg(c::raised())).child(icon(ic, 14.0, color))
}

pub(super) fn state_color(c0: &Container) -> (Rgba, Rgba) {
    match c0.state.as_str() {
        "running" if c0.status.contains("(unhealthy)") => (c::warn(), c::warn_soft()),
        "running" => (c::online(), c::online_soft()),
        "restarting" | "paused" => (c::warn(), c::warn_soft()),
        "dead" => (c::danger(), c::danger().opacity(0.17)),
        "exited" if !c0.status.starts_with("Exited (0)") => (c::danger(), c::danger().opacity(0.17)),
        _ => (c::muted(), c::idle_soft()),
    }
}

impl Home {
    pub(crate) fn dock_visible(&self) -> bool {
        self.workspace.active.and_then(|a| self.workspace.tabs.get(a)).is_some_and(|t| matches!(t.kind, workspace::TabKind::Node(_)) && t.mode == workspace::NodeMode::Containers)
    }

    /// 每次渲染前调用:建立 / 复用连接、收新数据、起定时刷新、创建输入框。
    pub(crate) fn sync_docker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dock_visible() {
            return;
        }
        let Some(a) = self.workspace.active else { return };
        if self.dock.search.is_none() {
            let s = cx.new(|cx| InputState::new(window, cx).placeholder("搜索容器"));
            cx.observe(&s, |_, _, cx| cx.notify()).detach();
            self.dock.search = Some(s);
            let p = cx.new(|cx| InputState::new(window, cx).placeholder("镜像名,如 nginx:alpine"));
            cx.observe(&p, |_, _, cx| cx.notify()).detach();
            self.dock.pull = Some(p);
        }
        if !self.dock.polling {
            self.dock.polling = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_millis(400)).await;
                    if this.update(cx, |this, cx| this.dock_tick(cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        let Some(id) = self.workspace.tabs[a].node() else { return };
        if self.dock.key != Some((a, id)) {
            self.dock.key = Some((a, id));
            self.dock.snap = Arc::new(Snapshot::default());
            self.dock.mon = None;
            self.dock.mon_err = None;
            self.dock.seen = 0;
            // 运维页(进程、服务、存储、计划任务、网站)的数据属于某一台服务器,换标签要清掉,否则会显示上一台的
            self.sys_clear();
        }
        if self.demo {
            self.dock.snap = Arc::new(docker::demo_snapshot(&self.node().stacks));
            self.dock.mon = Some(Arc::new(crate::monitor::demo_snapshot()));
            self.conf_sync(window, cx);
            self.reg_sync(window, cx);
            self.sys_sync(window, cx);
            self.ct_sync(window, cx);
            self.comp_sync(window, cx);
            if self.dock.dbm_dbg.is_some() && self.dbm_current().is_some() {
                if let Some(k) = self.dock.dbm_dbg.take() {
                    self.debug_dbm(&k, window, cx);
                }
            }
            return;
        }
        // 这个标签的终端已连上就复用它的连接;没连上等它(连接页显示终端的进度);没有终端才自己连。
        let term = self.workspace.tabs[a].term.as_ref().map(|t| (t.handle.ever_connected(), t.handle.session(), t.handle.generation()));
        let stale = self.workspace.tabs[a].docker.as_ref().and_then(|d| d.epoch()).zip(term.as_ref().map(|t| t.2)).is_some_and(|(e, g)| e != g);
        if stale {
            self.workspace.tabs[a].docker = None;
        }
        if self.workspace.tabs[a].docker.is_none() {
            let shared = match term {
                Some((false, _, _)) | Some((true, None, _)) => return,
                Some((true, Some(sess), epoch)) => Some((sess, epoch)),
                None => None,
            };
            let conn = self.conn_for(id);
            self.workspace.tabs[a].docker = Some(Docker::connect(conn, shared));
        }
        let want_sizes = self.page == Page::Volumes;
        let Some(d) = self.workspace.tabs[a].docker.as_ref() else { return };
        d.touch();
        d.want_volume_sizes(want_sizes);
        self.dock.mon = d.monitor();
        self.dock.mon_err = d.monitor_error().map(Into::into);
        let notice = d.take_notice();
        let ver = d.version();
        let snap = (ver != self.dock.seen).then(|| d.snapshot()).flatten();
        self.dock.seen = ver;
        if let Some(n) = notice {
            self.comp_applied(&n, cx);
            self.dock.notice = Some((n.into(), Instant::now()));
        }
        if let Some(s) = snap {
            let ex = self.node().stacks.iter().map(|s| (s.name.clone(), s.expanded)).collect();
            let stacks = docker::to_stacks(&s, &ex);
            self.node_mut().stacks = stacks;
            self.dock.snap = s;
        }
        self.conf_sync(window, cx);
        self.reg_sync(window, cx);
        self.sys_sync(window, cx);
        self.ct_sync(window, cx);
        self.comp_sync(window, cx);
    }

    /// 定时器:后台有新数据、连接状态在变、提示过期时重绘
    fn dock_tick(&mut self, cx: &mut Context<Self>) {
        if !self.dock_visible() {
            return;
        }
        let mut redraw = self.sys_due() || self.ct_due() || (self.page == Page::Database && self.dbm_retry_due());
        // 点击后至少转 PENDING_MIN_MS,之后后台不再「进行中」就撤掉;兜底 PENDING_MAX_SECS
        if !self.dock.pending.is_empty() {
            let d = self.workspace.active.and_then(|a| self.workspace.tabs.get(a)).and_then(|t| t.docker.as_ref());
            self.dock.pending.retain(|(n, v, t)| t.elapsed() < Duration::from_secs(PENDING_MAX_SECS) && (t.elapsed() < Duration::from_millis(PENDING_MIN_MS) || d.is_some_and(|d| d.is_busy_verb(v, n))));
            redraw = true;
        }
        if let Some(a) = self.workspace.active {
            if let Some(d) = self.workspace.tabs[a].docker.as_ref() {
                d.touch();
                redraw |= d.version() != self.dock.seen || !matches!(d.status(), docker::Status::Ready | docker::Status::Failed(_));
            }
        }
        if self.dock.notice.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(NOTICE_SECS)) {
            self.dock.notice = None;
            redraw = true;
        }
        // Docker 装好了:丢掉旧连接重新检测,直接进入 Docker 页面
        if let Some(t) = self.workspace.active.and_then(|a| self.workspace.tabs.get_mut(a)) {
            // 连上之后 Docker 被卸载了:刷新读到「没有 docker」,旧快照还在。重新连接一次,走和首次连接相同的检测和安装流程
            let gone = t.docker.as_ref().is_some_and(|d| d.snapshot().is_some() && d.problem() == Some(Problem::NoDocker));
            if gone || t.docker.as_ref().is_some_and(|d| d.install_view().phase == docker::InstallPhase::Done) {
                t.docker = None;
                self.dock.seen = 0;
                redraw = true;
            }
        }
        if redraw {
            cx.notify();
        }
    }

    pub(crate) fn dock_refresh(&mut self, cx: &mut Context<Self>) {
        if self.page == Page::Config {
            self.conf_reload();
        }
        if self.page == Page::Registry {
            self.reg_reload();
        }
        self.sys_reset();
        if let Some(d) = self.workspace.active.and_then(|a| self.workspace.tabs.get(a)).and_then(|t| t.docker.as_ref()) {
            d.refresh();
        }
        cx.notify();
    }

    pub(crate) fn dock_act(&mut self, a: Action, cx: &mut Context<Self>) {
        if let Action::Sys(j) = &a {
            self.sys_note(j);
        }
        if self.demo {
            self.dock.notice = Some(("演示数据,不会真的执行".into(), Instant::now()));
        } else if let Some(d) = self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref()) {
            if matches!(a, Action::ComposeUp { .. }) {
                self.dock.comp.applying = true;
            }
            if matches!(a, Action::Start(_) | Action::Stop(_) | Action::Restart(_) | Action::Remove(_) | Action::RemoveImage(_) | Action::RemoveVolume(_) | Action::Logout(_) | Action::Pull(_) | Action::PruneImages | Action::PruneVolumes) {
                self.dock.pending.retain(|p| p.0 != a.target());
                self.dock.pending.push((a.target().to_string(), a.verb().to_string(), Instant::now()));
            }
            if let Action::Sys(j) = &a {
                self.dock.pending.retain(|p| p.0 != j.target);
                self.dock.pending.push((j.target.clone(), j.label.clone(), Instant::now()));
            }
            d.act(a);
        }
        cx.notify();
    }

    /// 连接中 / 失败 / 没装 Docker / 权限不足 / 读取中:返回整页的状态页;可以正常显示数据时返回 None。
    pub(crate) fn docker_gate(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.demo {
            return None;
        }
        let a = self.workspace.active?;
        let tab = &self.workspace.tabs[a];
        if let Some(t) = tab.term.as_ref().filter(|t| !t.handle.ever_connected()) {
            return Some(self.connecting_page(ConnTarget::Terminal(a), t.handle.steps(), t.handle.host_prompt(), crate::terminal::ssh::STEP_LABELS, cx).into_any_element());
        }
        let Some(d) = tab.docker.as_ref() else { return Some(loading("正在连接…").into_any_element()) };
        match d.status() {
            docker::Status::HostPrompt(p) => Some(self.connecting_page(ConnTarget::Docker, d.steps(), Some(p), DOCKER_STEP_LABELS, cx).into_any_element()),
            docker::Status::Failed(_) => Some(self.connecting_page(ConnTarget::Docker, d.steps(), None, DOCKER_STEP_LABELS, cx).into_any_element()),
            docker::Status::Connecting if d.epoch().is_none() => Some(self.connecting_page(ConnTarget::Docker, d.steps(), None, DOCKER_STEP_LABELS, cx).into_any_element()),
            docker::Status::Connecting => Some(loading("正在检测 Docker…").into_any_element()),
            docker::Status::Ready => {
                // 运维页读的是 SSH 命令的输出,Docker 有没有装都能用
                if self.page.is_sys() {
                    return None;
                }
                // 概览页是整机监控,不依赖 Docker:读到系统数据(或确定读不了)就能显示
                if self.page == Page::Overview {
                    return (d.monitor().is_none() && d.monitor_error().is_none()).then(|| loading("正在读取系统信息…").into_any_element());
                }
                // 有旧数据时继续显示(偶发的读取失败不该清空页面);Docker 停了、没权限了则要告诉用户
                if d.snapshot().is_some() && !matches!(d.problem(), Some(Problem::DaemonDown(_) | Problem::NoPermission(_))) {
                    return None;
                }
                match d.problem() {
                    Some(p) if self.page == Page::Database => Some(self.db_problem_page(p, cx).into_any_element()),
                    Some(p) => {
                        if p == Problem::NoDocker {
                            d.install_probe();
                        }
                        Some(self.problem_page(p, cx).into_any_element())
                    }
                    None => Some(loading("正在读取容器…").into_any_element()),
                }
            }
        }
    }

    /// 「没有安装 Docker」页上的一键安装:探测结果、安装源选择、按钮、安装日志
    fn install_section(&self, cx: &mut Context<Self>) -> Div {
        use crate::docker_install::Source;
        let Some(d) = self.workspace.active.and_then(|a| self.workspace.tabs.get(a)).and_then(|t| t.docker.as_ref()) else { return div() };
        let v = d.install_view();
        let note = |text: String, color: Rgba| div().max_w(px(520.0)).text_size(px(t::TYPE_CONTROL)).text_color(color).child(text);
        // 日志框固定高度(不随行数跳动);不满一屏时从上往下排,满了贴底滚动。"==>" 开头的是步骤标题,报错行标橙
        const LOG_ROWS: usize = 10;
        let log_box = |lines: &[String]| {
            let full = lines.len() >= LOG_ROWS;
            let tail = lines.iter().rev().take(LOG_ROWS).rev().cloned().collect::<Vec<_>>();
            let bad = |l: &str| {
                let low = l.to_lowercase();
                l.starts_with("E:") || l.contains("失败") || low.contains("timed out") || low.contains("error") || low.contains("could not")
            };
            div()
                .w(px(520.0))
                .h(px(LOG_ROWS as f32 * 20.0 + 24.0))
                .px(px(14.0))
                .py(px(12.0))
                .rounded(px(t::RADIUS_MD))
                .bg(c::sunken())
                .border_1()
                .border_color(c::border())
                .overflow_hidden()
                .flex()
                .flex_col()
                .when(full, |d| d.justify_end())
                .text_size(px(t::TYPE_CAPTION))
                .font_family(MONO)
                .children(tail.into_iter().map(|l| {
                    let (color, weight) = if l.starts_with("==>") {
                        (c::secondary(), FontWeight::SEMIBOLD)
                    } else if bad(&l) {
                        (c::warn(), FontWeight::NORMAL)
                    } else {
                        (c::muted(), FontWeight::NORMAL)
                    };
                    div().h(px(20.0)).flex_shrink_0().truncate().text_color(color).font_weight(weight).child(l)
                }))
        };
        // 日志默认收起,用户点「查看日志」再展开;有日志才给入口
        let log_open = self.dock.inst_log_open;
        let log_part = |cx: &mut Context<Self>| -> Div {
            if v.log.is_empty() {
                return div();
            }
            div().flex().flex_col().items_center().gap(px(10.0)).child(
                div()
                    .id("inst-log-toggle")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .cursor_pointer()
                    .text_size(px(t::TYPE_CAPTION))
                    .text_color(c::ghost())
                    .hover(|s| s.text_color(c::secondary()))
                    .child(icon(if log_open { IconName::ChevronUp } else { IconName::ChevronDown }, 12.0, c::ghost()))
                    .child(if log_open { "收起日志" } else { "查看日志" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.dock.inst_log_open = !this.dock.inst_log_open;
                        cx.notify();
                    })),
            ).children(log_open.then(|| log_box(&v.log)))
        };
        let col = div().mt(px(8.0)).flex().flex_col().items_center().gap(px(14.0));
        match v.phase {
            docker::InstallPhase::Idle | docker::InstallPhase::Probing => col.child(
                div().flex().flex_row().items_center().gap(px(8.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(dot_spinner(("inst-probe", 0), c::muted())).child("正在检测系统和网络…"),
            ),
            docker::InstallPhase::Done => col.child(note("Docker 安装完成,正在重新检测…".into(), c::online())),
            docker::InstallPhase::Running if v.cancelling => col
                .child(div().flex().flex_row().items_center().gap(px(8.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(dot_spinner(("inst-run", 0), c::muted())).child("正在取消安装…"))
                .child(note("正在结束服务器上的安装进程。apt 正在配置软件包时会等它处理完当前这个包,避免损坏系统,可能要几十秒".into(), c::ghost()))
                .child(log_part(cx)),
            docker::InstallPhase::Cancelled => col
                .child(div().flex().flex_row().items_center().gap(px(8.0)).child(icon(IconName::CircleX, 18.0, c::muted())).child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child("安装已取消")))
                .child(note("服务器上的安装进程已停止,临时文件已清理。可以随时重新开始。".into(), c::ghost()))
                .child(log_part(cx))
                .child(button("inst-restart", IconName::RefreshCw, "重新开始").mt(px(2.0)).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(t) = this.workspace.active.and_then(|a| this.workspace.tabs.get_mut(a)) {
                        t.docker = None;
                    }
                    this.dock.seen = 0;
                    cx.notify();
                }))),
            docker::InstallPhase::Running => {
                let stalled = v.last_output.is_some_and(|t| t.elapsed().as_secs() >= 180);
                let (frac, step) = crate::docker_install::progress(&v.log);
                let mut col = col
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(4.0))
                            .child(div().flex().flex_row().items_center().gap(px(8.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(dot_spinner(("inst-run", 0), c::muted())).child(if v.cancelling { "正在取消…" } else { "正在安装 Docker" }))
                            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(if v.cancelling { "正在取消".to_string() } else { format!("当前:{step}") })),
                    )
                    .child(
                        div()
                            .w(px(520.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(12.0))
                            .child(div().flex_1().h(px(4.0)).rounded(px(2.0)).bg(c::sunken()).overflow_hidden().child(div().h_full().w(relative(frac)).rounded(px(2.0)).bg(c::accent())))
                            .child(div().w(px(56.0)).text_right().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::ghost()).child(format!("约 {}%", (frac * 100.0).round() as u32))),
                    )
                    .child(log_part(cx))
                    .child(note("安装在服务器上独立运行,断网、休眠或关掉 Otter 都不会中断".into(), c::ghost()));
                if stalled && !v.cancelling {
                    col = col.child(note("已经 3 分钟没有新输出,可能网络卡住了。可以继续等,也可以取消后重试。".into(), c::warn()));
                }
                if !v.cancelling {
                    col = col.child(button("inst-cancel", IconName::X, "取消安装").mt(px(2.0)).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(d) = this.workspace.active.and_then(|a| this.workspace.tabs.get(a)).and_then(|t| t.docker.as_ref()) {
                            d.install_cancel();
                        }
                        cx.notify();
                    })));
                }
                col
            }
            docker::InstallPhase::Lost => col
                .child(log_part(cx))
                .child(note("与服务器的连接断了。安装仍在服务器上继续,重新连接后可以接着看进度。".into(), c::warn()))
                .child(button("inst-reconnect", IconName::RefreshCw, "重新检测").on_click(cx.listener(|this, _, _, cx| {
                    // 重建连接:新连接探测到进行中的安装会自动接上
                    if let Some(t) = this.workspace.active.and_then(|a| this.workspace.tabs.get_mut(a)) {
                        t.docker = None;
                    }
                    this.dock.seen = 0;
                    cx.notify();
                }))),
            docker::InstallPhase::Failed(e) => {
                col.child(note(e, c::danger())).child(log_part(cx)).child(button("inst-again", IconName::RefreshCw, "重新检测").on_click(cx.listener(|this, _, _, cx| {
                    // 重建连接再检测:失败常因为连接已断,复用旧连接会一直失败
                    if let Some(t) = this.workspace.active.and_then(|a| this.workspace.tabs.get_mut(a)) {
                        t.docker = None;
                    }
                    this.dock.seen = 0;
                    cx.notify();
                })))
            }
            docker::InstallPhase::Ready => {
                let Some(probe) = v.probe else { return col };
                if let Some(reason) = probe.blocked() {
                    return col.child(note(format!("不能一键安装:{reason}"), c::ghost()));
                }
                let auto = probe.pick();
                let spd = |s: Source| probe.speed_of(s).map_or("不通".to_string(), crate::docker_install::fmt_speed);
                let mut items = vec![crate::ui::MenuItem::new(format!("自动(推荐):{}", auto.map_or("无".into(), |s| format!("{} · {}", s.label(), spd(s)))))];
                items.extend(Source::ALL.iter().map(|s| crate::ui::MenuItem::new(format!("{} · {}", s.label(), spd(*s)))));
                let selected = self.dock.inst_src.and_then(|s| Source::ALL.iter().position(|x| *x == s)).map_or(0, |i| i + 1);
                let (t1, t2, t3) = (cx.entity(), cx.entity(), cx.entity());
                let chosen = self.dock.inst_src.or(auto);
                let accel = probe.hub_slow();
                let name = probe.os_name.clone();
                let select = div().w(px(240.0)).child(crate::ui::select_sized(
                        30.0,
                        "inst-src",
                        IconName::Package,
                        items,
                        selected,
                        self.dock.inst_src_open,
                        move |_, cx| t1.update(cx, |h, cx| {
                            h.dock.inst_src_open = !h.dock.inst_src_open;
                            cx.notify();
                        }),
                        move |i, _, cx| t2.update(cx, |h, cx| {
                            h.dock.inst_src = i.checked_sub(1).and_then(|k| Source::ALL.get(k).copied());
                            h.dock.inst_src_open = false;
                            cx.notify();
                        }),
                        move |_, cx| t3.update(cx, |h, cx| {
                            h.dock.inst_src_open = false;
                            cx.notify();
                        }),
                        None,
                    ));
                let go = button("inst-go", IconName::Download, "一键安装 Docker").on_click(cx.listener(move |this, _, _, cx| {
                    let Some(source) = chosen else { return };
                    this.dock.inst_accel = accel;
                    this.ask(DockAsk { title: "安装 Docker", tile: IconName::Container, name: name.clone(), detail: "这台服务器", body: "", label: "开始安装", action: Action::InstallDocker { source, accel }, list: Vec::new() }, cx);
                }));
                // 系统信息 / 安装源 + 安装按钮同一行
                col.child(note(format!("{} · {}", probe.os_name, probe.arch), c::ghost())).child(div().flex().flex_row().items_center().gap(px(8.0)).child(select).child(go))
            }
        }
    }

    fn problem_page(&self, p: Problem, cx: &mut Context<Self>) -> Div {
        let (ic, title, sub) = problem_text(&p);
        // 没装 Docker 时有自己的「重新检测」和安装流程,「重试」只在不能安装、用户去处理后才需要
        let show_retry = p != Problem::NoDocker
            || self.workspace.active.and_then(|a| self.workspace.tabs.get(a)).and_then(|t| t.docker.as_ref()).is_some_and(|d| {
                let v = d.install_view();
                v.phase == docker::InstallPhase::Ready && v.probe.is_some_and(|p| p.blocked().is_some())
            });
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .pb(px(72.0)) // 视觉重心上提:内容整体偏上一点才不显得沉
            .gap(px(10.0))
            .child(icon(ic, 34.0, c::ghost()))
            .child(div().mt(px(4.0)).text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child(title))
            .children((!sub.is_empty()).then(|| div().max_w(px(520.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(sub)))
            .children((p == Problem::NoDocker).then(|| self.install_section(cx)))
            .children(show_retry.then(|| button("dock-retry", IconName::RefreshCw, "重试").mt(px(6.0)).on_click(cx.listener(|this, _, _, cx| {
                if let Some(t) = this.workspace.active.and_then(|a| this.workspace.tabs.get_mut(a)) {
                    t.docker = None;
                }
                this.dock.seen = 0;
                cx.notify();
            }))))
    }

    /// 页头右侧的按钮(各页不同)
    pub(crate) fn dock_header_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v: Vec<AnyElement> = self.sys_header_actions(cx);
        if self.page == Page::Containers {
            if let Some(s) = self.dock.search.clone() {
                v.push(
                    div()
                        .w(px(220.0))
                        .h(px(30.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .pl(px(10.0))
                        .rounded(px(t::RADIUS_MD))
                        .bg(c::field())
                        .border_1()
                        .border_color(c::field_border())
                        .text_size(px(t::TYPE_CONTROL))
                        .child(icon(IconName::Search, 13.0, c::ghost()))
                        .child(div().flex_1().min_w_0().ml(px(-4.0)).child(Input::new(&s).appearance(false).with_size(px(12.0 / 0.875))))
                        .into_any_element(),
                );
            }
        }
        if self.page == Page::Database {
            match self.dbm_current() {
                // 已经装过:换成卸载(面板管理的只读,不给卸载)
                Some(inst) => {
                    if inst.container.panel.is_none() {
                        let name = inst.container.name.clone();
                        v.push(
                            div()
                                .id("db-uninstall")
                                .h(px(30.0))
                                .px(px(12.0))
                                .flex()
                                .items_center()
                                .rounded(px(t::RADIUS_MD))
                                .cursor_pointer()
                                .text_size(px(t::TYPE_CONTROL))
                                .text_color(c::danger())
                                .hover(|s| s.bg(c::field_hover()))
                                .child("卸载")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.ask(DockAsk { title: "卸载数据库", tile: IconName::Database, name: name.clone(), detail: "数据库容器", body: "会停止并删除这个容器。数据存储卷和 compose 文件、.env 会保留在服务器上,需要时可以到「存储卷」页或服务器上手动删除", label: "卸载", action: Action::Remove(name.clone()), list: Vec::new() }, cx);
                                }))
                                .into_any_element(),
                        );
                    }
                }
                // 没有实例时,「安装数据库」放在页面中间的空状态里(见 `database_page`)
                None => {}
            }
        }
        if self.page == Page::Stacks {
            v.push(
                button("stack-create", IconName::Plus, "创建堆栈")
                    .on_click(cx.listener(|this, _, window, cx| this.create_open(window, cx)))
                    .into_any_element(),
            );
        }
        if self.page == Page::Images {
            v.push(
                button_busy("dock-pull", IconName::Download, "拉取镜像", self.verb_busy("拉取"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.dock.pull_open = !this.dock.pull_open;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            v.push(
                button_busy("dock-prune-images", IconName::Trash, "清理镜像", self.verb_busy("清理镜像"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        // 没被任何容器用到的镜像(和 docker image prune -a 的范围一致)
                        let snap = this.dock.snap.clone();
                        let unused: Vec<&docker::Image> = snap.images.iter().filter(|i| !snap.containers.iter().any(|ct| i.matches(&ct.image))).collect();
                        if unused.is_empty() {
                            this.dock.notice = Some(("没有未使用的镜像可清理".into(), Instant::now()));
                            cx.notify();
                            return;
                        }
                        let list = unused.iter().map(|i| format!("{}:{}  ·  {}", i.repo, i.tag, i.size)).collect();
                        this.ask(DockAsk { title: "清理镜像", tile: IconName::Image, name: format!("{} 个未使用的镜像", unused.len()), detail: "Docker 镜像", body: "删除下面这些没有被任何容器使用的镜像,需要时可以重新拉取", label: "清理镜像", action: Action::PruneImages, list }, cx);
                    }))
                    .into_any_element(),
            );
        }
        if self.page == Page::Registry {
            v.push(
                button("reg-add", IconName::Plus, "添加仓库")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.dock.reg.open = !this.dock.reg.open;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        if self.page == Page::Volumes {
            v.push(
                button_busy("dock-prune-volumes", IconName::Trash, "清理存储卷", self.verb_busy("清理存储卷"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        // 没被任何容器挂载的存储卷(含已停止的容器,和 docker volume prune 一致)
                        let snap = this.dock.snap.clone();
                        let unused: Vec<&docker::Volume> = snap.volumes.iter().filter(|v| snap.volume_users(v).is_empty()).collect();
                        if unused.is_empty() {
                            this.dock.notice = Some(("没有未使用的存储卷可清理".into(), Instant::now()));
                            cx.notify();
                            return;
                        }
                        let list = unused.iter().map(|v| match &v.size { Some(sz) => format!("{}  ·  {}", v.name, sz), None => v.name.clone() }).collect();
                        this.ask(DockAsk { title: "清理存储卷", tile: IconName::HardDrive, name: format!("{} 个未使用的存储卷", unused.len()), detail: "Docker 存储卷", body: "删除下面这些没有被任何容器使用的存储卷,里面的数据无法恢复", label: "清理存储卷", action: Action::PruneVolumes, list }, cx);
                    }))
                    .into_any_element(),
            );
        }
        v.push(div().id("dock-refresh").size(px(30.0)).flex().items_center().justify_center().cursor_pointer().child(icon(IconName::RefreshCw, 15.0, c::muted())).hover(|s| s.opacity(0.7)).on_click(cx.listener(|this, _, _, cx| {
            if this.page == Page::Database {
                this.dbm_invalidate();
            }
            if this.page == Page::Store {
                this.store_refresh(cx);
            }
            this.dock_refresh(cx)
        })).into_any_element());
        v
    }

    pub(crate) fn dock_subtitle(&self) -> String {
        let n = &self.node().name;
        match self.page {
            Page::Overview => format!("{n} 的系统资源"),
            Page::Containers => format!("{n} 上的全部容器"),
            Page::Database => format!("{n} 上的数据库容器"),
            Page::Store => "一键安装常用应用;列表来自 GitHub 上的应用目录".to_string(),
            Page::Stacks => format!("{n} 上的 Compose 项目"),
            Page::Images => format!("{n} 上的本地镜像"),
            Page::Volumes => format!("{n} 上的存储卷"),
            Page::Registry => format!("{n} 上登录的镜像仓库"),
            Page::Config => format!("{n} 上的 Docker 守护进程配置"),
            Page::Processes | Page::Services | Page::Storage | Page::Cron | Page::Websites => self.sys_subtitle().unwrap_or_default(),
        }
    }

    pub(crate) fn docker_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.page {
            Page::Overview => self.overview_page(cx),
            Page::Containers => self.containers_page(cx),
            Page::Database => self.database_page(cx),
            Page::Store => self.store_page(cx),
            Page::Images => self.images_page(cx),
            Page::Volumes => self.volumes_page(cx),
            Page::Registry => self.registry_page(cx),
            Page::Config => self.config_page(cx),
            Page::Stacks => div().into_any_element(),
            Page::Processes | Page::Services | Page::Storage | Page::Cron | Page::Websites => self.sys_page(cx),
        }
    }

    /// 页面最上层的覆盖物:确认框、操作结果提示
    pub(crate) fn dock_overlays(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v: Vec<AnyElement> = Vec::new();
        v.extend(self.ct_drawer(window, cx));
        if let Some(m) = self.cron_dialog(cx) {
            v.push(m.into_any_element());
        }
        if let Some(m) = self.lens_confirm_dialog(cx) {
            v.push(m.into_any_element());
        }
        if let Some(m) = self.lens_delete_dialog(cx) {
            v.push(m.into_any_element());
        }
        v.extend(self.lens_menu_view(cx));
        v.extend(self.web_overlays(window, cx));
        if let Some(m) = self.create_drawer(window, cx) {
            v.push(m.into_any_element());
        }
        v.extend(self.api_overlays(window, cx));
        if let Some(m) = self.comp_dialog(window, cx) {
            v.push(m.into_any_element());
        }
        if let Some(cf) = &self.dock.confirm {
            let mut action = cf.action.clone();
            let install = matches!(action, Action::InstallDocker { .. });
            if let Action::InstallDocker { accel, .. } = &mut action {
                *accel = self.dock.inst_accel;
            }
            let spec = crate::confirm::Confirm {
                ids: ["dock-cancel", "dock-ok"],
                title: cf.title,
                tile: Self::icon_tile(cf.tile, c::accent(), c::accent_soft()).into_any_element(),
                name: cf.name.clone().into(),
                detail: cf.detail.into(),
                points: if let Action::InstallDocker { source, .. } = &cf.action {
                    let spd = self.workspace.active.and_then(|a| self.workspace.tabs.get(a)).and_then(|t| t.docker.as_ref()).and_then(|d| d.install_view().probe).and_then(|p| p.speed_of(*source));
                    vec![
                        (IconName::Download, c::accent(), "经 SSH 在服务器上以 root 执行 Docker 官方安装脚本,装完设为开机自启".into()),
                        (IconName::Globe, c::muted(), format!("安装源:{}{},失败会自动换用其他源", source.label(), spd.map_or(String::new(), |x| format!("(测速 {})", crate::docker_install::fmt_speed(x)))).into()),
                    ]
                } else {
                    std::iter::once((IconName::Trash, c::danger(), cf.body.into()))
                    .chain(cf.list.iter().take(8).map(|x| (cf.tile, c::muted(), x.clone().into())))
                    .chain((cf.list.len() > 8).then(|| (cf.tile, c::ghost(), format!("…还有 {} 个", cf.list.len() - 8).into())))
                    .collect()
                },
                ok_label: cf.label,
                kind: if install { crate::confirm::ConfirmKind::Action } else { crate::confirm::ConfirmKind::Danger },
                extra: install.then(|| self.accel_check(cx).into_any_element()),
            };
            v.push(self.delete_confirm("dock-confirm", spec, |this| {
                // 取消 HTTP 仓库的确认时,后台暂存的登录凭据一并丢掉
                if matches!(this.dock.confirm.as_ref().map(|c| &c.action), Some(Action::ConfirmInsecure(_))) {
                    this.reg_drop_pending();
                }
                this.dock.confirm = None;
            }, move |this, cx| {
                this.dock.confirm = None;
                this.dock_act(action.clone(), cx);
            }, cx).into_any_element());
        }
        if let Some((msg, _)) = &self.dock.notice {
            v.push(
                div()
                    .absolute()
                    .bottom(px(20.0))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(div().max_w(px(560.0)).px(px(14.0)).py(px(8.0)).rounded(px(t::RADIUS_MD)).bg(c::dialog()).border_1().border_color(c::dialog_border()).text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(msg.clone()))
                    .into_any_element(),
            );
        }
        v
    }

    /// 安装确认框里的「配置镜像加速」勾选项:默认值取决于 Docker Hub 实测是否慢
    fn accel_check(&self, cx: &mut Context<Self>) -> Div {
        let on = self.dock.inst_accel;
        let slow = self.workspace.active.and_then(|a| self.workspace.tabs.get(a)).and_then(|t| t.docker.as_ref()).and_then(|d| d.install_view().probe).is_some_and(|p| p.hub_slow());
        let hint = if slow { "检测到 Docker Hub 访问较慢,建议开启" } else { "Docker Hub 访问正常,一般不需要" };
        div().w_full().flex().flex_row().items_start().gap(px(10.0)).px(px(2.0)).child(
            div()
                .id("inst-accel")
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(10.0))
                .cursor_pointer()
                .child(
                    div()
                        .mt(px(1.0))
                        .size(px(16.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .border_1()
                        .when(on, |d| d.bg(c::accent()).border_color(c::accent()))
                        .when(!on, |d| d.border_color(c::field_border()))
                        .children(on.then(|| icon(IconName::Check, 12.0, c::on_accent()))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child("配置 Docker Hub 镜像加速"))
                        .child(div().text_size(px(t::TYPE_MICRO + 1.0)).text_color(c::ghost()).child(hint)),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.dock.inst_accel = !this.dock.inst_accel;
                    cx.notify();
                })),
        )
    }

    pub(super) fn ask(&mut self, ask: DockAsk, cx: &mut Context<Self>) {
        self.dock.confirm = Some(ask);
        cx.notify();
    }

    pub(super) fn is_busy(&self, target: &str) -> bool {
        self.workspace.active.and_then(|a| self.workspace.tabs.get(a)).and_then(|t| t.docker.as_ref()).is_some_and(|d| d.is_busy(target))
    }

    // ---- 容器 ----

    fn containers_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let snap = self.dock.snap.clone();
        if snap.containers.is_empty() {
            return empty_state(IconName::Container, "还没有容器", "这台服务器上没有任何容器").into_any_element();
        }
        let q = self.dock.search.as_ref().map(|s| s.read(cx).value().trim().to_lowercase()).unwrap_or_default();
        let mut list: Vec<&Container> = snap.containers.iter().filter(|c| q.is_empty() || c.name.to_lowercase().contains(&q) || c.image.to_lowercase().contains(&q) || c.project.as_deref().is_some_and(|p| p.to_lowercase().contains(&q))).collect();
        let (key, asc) = self.dock.sort;
        let stat = |c: &Container| snap.stats.get(&c.id).copied().unwrap_or_default();
        list.sort_by(|a, b| {
            let o = match key {
                SortKey::Name => a.name.cmp(&b.name),
                SortKey::State => b.running().cmp(&a.running()).then(a.name.cmp(&b.name)),
                SortKey::Cpu => stat(a).cpu.partial_cmp(&stat(b).cpu).unwrap_or(std::cmp::Ordering::Equal),
                SortKey::Mem => stat(a).mem.cmp(&stat(b).mem),
            };
            if asc { o } else { o.reverse() }
        });

        let sorter = |id: &'static str, label: &'static str, k: SortKey, cx: &mut Context<Self>| {
            let on = self.dock.sort.0 == k;
            let arrow = if !on { None } else if self.dock.sort.1 { Some(IconName::ArrowUp) } else { Some(IconName::ArrowDown) };
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(3.0))
                .cursor_pointer()
                .text_color(if on { c::secondary() } else { c::ghost() })
                .child(head(label).text_color(if on { c::secondary() } else { c::ghost() }))
                .children(arrow.map(|a| icon(a, 11.0, c::muted())))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.dock.sort = if this.dock.sort.0 == k { (k, !this.dock.sort.1) } else { (k, k == SortKey::Name || k == SortKey::State) };
                    cx.notify();
                }))
        };
        let header = div()
            .child(sorter("h-name", "名称", SortKey::Name, cx).flex_1().min_w_0())
            .child(head("堆栈").w(px(110.0)))
            .child(head("镜像").flex_1().min_w_0())
            .child(sorter("h-state", "状态", SortKey::State, cx).w(px(104.0)))
            .child(sorter("h-cpu", "CPU", SortKey::Cpu, cx).w(px(58.0)))
            .child(sorter("h-mem", "内存", SortKey::Mem, cx).w(px(74.0)))
            .child(head("端口").w(px(150.0)))
            .child(div().w(px(172.0)));

        let mut rows: Vec<Stateful<Div>> = Vec::new();
        for (i, ct) in list.iter().enumerate() {
            let st = stat(ct);
            let up = ct.running();
            let (fg, bg) = state_color(ct);
            let busy = self.ct_busy_name(&ct.name);
            let name = ct.name.clone();
            let mut acts = div().w(px(172.0)).flex().flex_row().justify_end().items_center().gap(px(2.0));
            if let Some(p) = ct.panel {
                acts = acts.child(tag(format!("{p} · 只读")));
            } else {
                let (n, run) = (name.clone(), up);
                acts = acts.child(icon_btn_c(("ct-logs", i), IconName::ScrollText, c::muted()).on_click(cx.listener(move |this, _, window, cx| this.ct_open(n.clone(), run, super::ctpanel::CtTab::Logs, window, cx))));
                let n = name.clone();
                acts = acts.child(icon_btn_c(("ct-term", i), IconName::SquareTerminal, c::muted()).on_click(cx.listener(move |this, _, window, cx| this.ct_open(n.clone(), run, super::ctpanel::CtTab::Term, window, cx))));
                let n = name.clone();
                acts = acts.child(icon_btn_c(("ct-files", i), IconName::FolderOpen, c::muted()).on_click(cx.listener(move |this, _, window, cx| this.ct_open(n.clone(), run, super::ctpanel::CtTab::Files, window, cx))));
                // 正在执行的那个按钮换成加载动画,其余照常
                let slot = |id: &'static str, ic: IconName, color: Rgba, verb: &'static str| {
                    if busy == Some(verb) {
                        div().id((id, i)).size(px(24.0)).flex().items_center().justify_center().child(dot_spinner((id, i), color))
                    } else {
                        icon_btn_c((id, i), ic, color)
                    }
                };
                if up {
                    let n = name.clone();
                    let b = slot("stop", IconName::Square, c::muted(), "停止");
                    acts = acts.child(if busy == Some("停止") { b } else { b.on_click(cx.listener(move |this, _, _, cx| this.dock_act(Action::Stop(n.clone()), cx))) });
                    let n = name.clone();
                    let b = slot("restart", IconName::RotateCw, c::muted(), "重启");
                    acts = acts.child(if busy == Some("重启") { b } else { b.on_click(cx.listener(move |this, _, _, cx| this.dock_act(Action::Restart(n.clone()), cx))) });
                } else {
                    let n = name.clone();
                    let b = slot("start", IconName::Play, c::online(), "启动");
                    acts = acts.child(if busy == Some("启动") { b } else { b.on_click(cx.listener(move |this, _, _, cx| this.dock_act(Action::Start(n.clone()), cx))) });
                }
                let n = name.clone();
                let b = slot("rm", IconName::Trash, c::danger(), "删除");
                acts = acts.child(if busy == Some("删除") {
                    b
                } else {
                    b.on_click(cx.listener(move |this, _, _, cx| {
                        this.ask(DockAsk { title: "删除容器", tile: IconName::Box, name: n.clone(), detail: "Docker 容器", body: "强制删除,容器内未保存到存储卷的数据会丢失", label: "删除容器", action: Action::Remove(n.clone()), list: Vec::new() }, cx);
                    }))
                });
            }
            let ports = ct.ports.iter().take(2).map(|p| tag(p.clone())).collect::<Vec<_>>();
            let more = ct.ports.len().saturating_sub(2);
            rows.push(
                row(("ct", i))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(div().size(px(7.0)).flex_shrink_0().rounded_full().bg(fg))
                            .child(div().min_w_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(ct.name.clone())),
                    )
                    .child(div().w(px(110.0)).truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(ct.project.clone().unwrap_or_else(|| "—".into())))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(ct.image.clone()))
                    .child(div().w(px(104.0)).flex().flex_row().child(pill(docker::uptime_text(ct), fg, bg)))
                    .child(div().w(px(58.0)).text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::secondary()).child(if up { format!("{:.1}%", st.cpu) } else { "—".into() }))
                    .child(div().w(px(74.0)).text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::secondary()).child(if up { docker::fmt_bytes(st.mem) } else { "—".into() }))
                    .child(div().w(px(150.0)).flex().flex_row().gap(px(4.0)).overflow_hidden().children(ports).children((more > 0).then(|| small(format!("+{more}"), c::ghost()))))
                    .child(acts),
            );
        }
        if rows.is_empty() {
            return empty_state(IconName::Search, "没有匹配的容器", "换个关键字试试").into_any_element();
        }
        div().flex_shrink_0().pb(px(14.0)).flex().flex_col().child(table_flow(header, rows)).into_any_element()
    }

    // ---- 镜像 ----

    fn images_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let snap = self.dock.snap.clone();
        let mut col = div().flex_shrink_0().pb(px(14.0)).flex().flex_col().gap(px(10.0));
        let hosts = self.reg_hosts();
        if self.dock.pull_open {
            if let Some(p) = self.dock.pull.clone() {
                col = col.child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .children((!hosts.is_empty()).then(|| {
                            let (t1, t2, t3) = (cx.entity(), cx.entity(), cx.entity());
                            let mut items = vec![crate::ui::MenuItem::new("不选仓库")];
                            items.extend(hosts.iter().map(|h| crate::ui::MenuItem::new(h.clone())));
                            let selected = self.dock.pull_reg.as_ref().and_then(|r| hosts.iter().position(|h| h == r)).map_or(0, |i| i + 1);
                            let hs = hosts.clone();
                            div().w(px(220.0)).flex_shrink_0().child(crate::ui::select_sized(
                                34.0,
                                "pull-reg",
                                IconName::Package,
                                items,
                                selected,
                                self.dock.pull_reg_open,
                                move |_, cx| t1.update(cx, |h, cx| {
                                    h.dock.pull_reg_open = !h.dock.pull_reg_open;
                                    cx.notify();
                                }),
                                move |i, _, cx| t2.update(cx, |h, cx| {
                                    h.dock.pull_reg = i.checked_sub(1).and_then(|k| hs.get(k).cloned());
                                    h.dock.pull_reg_open = false;
                                    cx.notify();
                                }),
                                move |_, cx| t3.update(cx, |h, cx| {
                                    h.dock.pull_reg_open = false;
                                    cx.notify();
                                }),
                                None,
                            ))
                        }))
                        .child(
                            div()
                                .flex_1()
                                .h(px(34.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .pl(px(12.0))
                                .rounded(px(t::RADIUS_MD))
                                .bg(c::field())
                                .border_1()
                                .border_color(c::field_border())
                                .text_size(px(t::TYPE_CONTROL))
                                .child(icon(IconName::Download, 13.0, c::ghost()))
                                .child(div().flex_1().min_w_0().ml(px(-4.0)).child(Input::new(&p).appearance(false).with_size(px(12.0 / 0.875)))),
                        )
                        .child(button("do-pull", IconName::Download, "拉取").on_click(cx.listener(|this, _, window, cx| {
                            let Some(p) = this.dock.pull.clone() else { return };
                            let mut name = p.read(cx).value().trim().to_string();
                            if let Some(reg) = this.dock.pull_reg.clone().filter(|r| !name.is_empty() && !name.starts_with(&format!("{r}/"))) {
                                name = format!("{reg}/{name}");
                            }
                            if !docker::safe_name(&name) {
                                this.dock.notice = Some(("镜像名只能包含字母、数字和 _ . : / @ - +".into(), Instant::now()));
                            } else {
                                p.update(cx, |s, cx| s.set_value("", window, cx));
                                this.dock.pull_open = false;
                                this.dock.notice = Some((format!("正在拉取 {name}…").into(), Instant::now()));
                                this.dock_act(Action::Pull(name), cx);
                            }
                            cx.notify();
                        }))),
                );
            }
        }
        if snap.images.is_empty() {
            return col.child(empty_state(IconName::Package, "还没有镜像", "点右上角「拉取镜像」")).into_any_element();
        }
        let header = div()
            .child(head("镜像").flex_1().min_w_0())
            .child(head("ID").w(px(110.0)))
            .child(head("大小").w(px(90.0)))
            .child(head("创建").w(px(110.0)))
            .child(head("使用").w(px(80.0)))
            .child(div().w(px(40.0)));
        let mut rows = Vec::new();
        for (i, im) in snap.images.iter().enumerate() {
            let users = snap.image_users(im);
            let name = if im.repo == "<none>" { "<none>".to_string() } else { format!("{}:{}", im.repo, im.tag) };
            let target = if im.repo == "<none>" || im.tag == "<none>" { im.id.clone() } else { name.clone() };
            let busy = self.op_busy(&target).is_some();
            let mut act = div().w(px(40.0)).flex().justify_end();
            if busy {
                act = act.child(icon_btn_busy(("rmi", i), IconName::Trash, c::danger(), true));
            } else if users == 0 {
                let (tg, nm) = (target.clone(), name.clone());
                act = act.child(icon_btn(("rmi", i), IconName::Trash, c::danger()).on_click(cx.listener(move |this, _, _, cx| {
                    this.ask(DockAsk { title: "删除镜像", tile: IconName::Image, name: nm.clone(), detail: "Docker 镜像", body: "只删除本地副本,需要时可以重新拉取", label: "删除镜像", action: Action::RemoveImage(tg.clone()), list: Vec::new() }, cx);
                })));
            }
            rows.push(
                row(("im", i))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(name))
                    .child(div().w(px(110.0)).text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(im.id.clone()))
                    .child(div().w(px(90.0)).text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::secondary()).child(im.size.clone()))
                    .child(div().w(px(110.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(im.created.clone()))
                    .child(div().w(px(80.0)).child(if users > 0 { pill(format!("{users} 个容器"), c::online(), c::online_soft()) } else { div().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child("未使用") }))
                    .child(act),
            );
        }
        col.child(table_flow(header, rows)).into_any_element()
    }

    // ---- 存储卷 ----

    fn volumes_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let snap = self.dock.snap.clone();
        if snap.volumes.is_empty() {
            return empty_state(IconName::HardDrive, "没有存储卷", "这台服务器上没有 Docker 命名卷").into_any_element();
        }
        let header = div().child(head("名称").flex_1().min_w_0()).child(head("驱动").w(px(80.0))).child(head("大小").w(px(90.0))).child(head("挂载到").w(px(260.0))).child(div().w(px(40.0)));
        let mut rows = Vec::new();
        for (i, v) in snap.volumes.iter().enumerate() {
            let users = snap.volume_users(v);
            let busy = self.op_busy(&v.name).is_some();
            let mut act = div().w(px(40.0)).flex().justify_end();
            if busy {
                act = act.child(icon_btn_busy(("rmv", i), IconName::Trash, c::danger(), true));
            } else if users.is_empty() {
                let n = v.name.clone();
                act = act.child(icon_btn(("rmv", i), IconName::Trash, c::danger()).on_click(cx.listener(move |this, _, _, cx| {
                    this.ask(DockAsk { title: "删除存储卷", tile: IconName::HardDrive, name: n.clone(), detail: "Docker 存储卷", body: "卷里的数据会永久丢失", label: "删除存储卷", action: Action::RemoveVolume(n.clone()), list: Vec::new() }, cx);
                })));
            }
            let mounted: Vec<crate::ui::Tag> = users.iter().take(3).map(|c| tag(c.name.clone())).collect();
            rows.push(
                row(("vl", i))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(v.name.clone()))
                    .child(div().w(px(80.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(v.driver.clone()))
                    .child(div().w(px(90.0)).text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::secondary()).child(v.size.clone().unwrap_or_else(|| "…".into())))
                    .child(
                        div()
                            .w(px(260.0))
                            .flex()
                            .flex_row()
                            .gap(px(4.0))
                            .overflow_hidden()
                            .children(mounted)
                            .children((users.len() > 3).then(|| small(format!("+{}", users.len() - 3), c::ghost())))
                            .children(users.is_empty().then(|| small("未使用", c::ghost()))),
                    )
                    .child(act),
            );
        }
        div().flex_shrink_0().pb(px(14.0)).flex().flex_col().child(table_flow(header, rows)).into_any_element()
    }

    // ---- 概览 ----
}

/// 各种 Docker 问题的图标、标题和说明(容器页和数据库页共用)
pub(super) fn problem_text(p: &Problem) -> (IconName, &'static str, String) {
    match p {
        Problem::NoDocker => (IconName::Container, "这台服务器没有安装 Docker", String::new()),
        Problem::DaemonDown(m) => (IconName::CircleAlert, "Docker 没有在运行", m.clone()),
        Problem::NoPermission(m) => (IconName::Lock, "没有权限运行 docker", m.clone()),
        Problem::Other(m) => (IconName::TriangleAlert, "读取 Docker 失败", m.clone()),
    }
}

pub(super) fn loading(msg: &'static str) -> Div {
    crate::ui::loading(msg)
}
