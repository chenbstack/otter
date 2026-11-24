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

/// 等用户确认的危险操作
pub(crate) struct Confirm {
    title: String,
    body: String,
    label: &'static str,
    action: Action,
}

pub(crate) struct DockUi {
    /// 当前页读的快照(真实或 demo)
    pub snap: Arc<Snapshot>,
    search: Option<Entity<InputState>>,
    pull: Option<Entity<InputState>>,
    pull_open: bool,
    sort: (SortKey, bool),
    confirm: Option<Confirm>,
    notice: Option<(SharedString, Instant)>,
    polling: bool,
    /// 上次读到的 `Docker::version`
    seen: u64,
    /// 当前同步的标签,变了就清空快照
    key: Option<(usize, NodeId)>,
}

impl DockUi {
    pub fn new() -> Self {
        Self { snap: Arc::new(Snapshot::default()), search: None, pull: None, pull_open: false, sort: (SortKey::Name, true), confirm: None, notice: None, polling: false, seen: 0, key: None }
    }
}

fn stat_tile(ic: IconName, label: &'static str, value: String, sub: Div, chart: Option<Div>) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_row()
        .items_end()
        .gap(px(12.0))
        .p(px(14.0))
        .glass(t::RADIUS_XL, GlassStyle::CARD)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(div().flex().flex_row().items_center().gap(px(6.0)).text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(icon(ic, 12.0, c::ghost())).child(label))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap(px(6.0))
                        .child(div().text_size(px(t::TYPE_DISPLAY)).font_family(MONO).font_weight(FontWeight::SEMIBOLD).child(value))
                        .child(sub),
                ),
        )
        .children(chart.map(|c| div().w(px(84.0)).flex_shrink_0().child(c)))
}

fn small(s: impl Into<SharedString>, color: Rgba) -> Div {
    div().text_size(px(t::TYPE_CAPTION)).text_color(color).child(s.into())
}

fn head(label: &'static str) -> Div {
    div().text_size(px(t::TYPE_CAPTION)).font_weight(FontWeight::SEMIBOLD).text_color(c::ghost()).child(label)
}

/// 表格容器:表头 + 可滚动的行
fn table(id: &'static str, header: Div, rows: Vec<Stateful<Div>>) -> Div {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .glass(t::RADIUS_XL, GlassStyle::CARD)
        .overflow_hidden()
        .child(header.h(px(34.0)).flex_shrink_0().flex().flex_row().items_center().gap(px(12.0)).px(px(16.0)).border_b_1().border_color(c::border()))
        .child(div().id(id).flex_1().min_h_0().overflow_y_scroll().children(rows))
}

fn row(id: (&'static str, usize)) -> Stateful<Div> {
    div().id(id).h(px(44.0)).flex_shrink_0().flex().flex_row().items_center().gap(px(12.0)).px(px(16.0)).border_b_1().border_color(c::border()).hover(|s| s.bg(c::hover()))
}

fn icon_btn(id: (&'static str, usize), ic: IconName, color: Rgba) -> Stateful<Div> {
    div().id(id).size(px(26.0)).flex().items_center().justify_center().rounded(px(t::RADIUS_XS)).cursor_pointer().hover(|s| s.bg(c::raised())).child(icon(ic, 14.0, color))
}

fn state_color(c0: &Container) -> (Rgba, Rgba) {
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
    fn dock_visible(&self) -> bool {
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
            self.dock.seen = 0;
        }
        if self.demo {
            self.dock.snap = Arc::new(docker::demo_snapshot(&self.node().stacks));
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
        let notice = d.take_notice();
        let ver = d.version();
        let snap = (ver != self.dock.seen).then(|| d.snapshot()).flatten();
        self.dock.seen = ver;
        if let Some(n) = notice {
            self.dock.notice = Some((n.into(), Instant::now()));
        }
        if let Some(s) = snap {
            let ex = self.node().stacks.iter().map(|s| (s.name.clone(), s.expanded)).collect();
            let stacks = docker::to_stacks(&s, &ex);
            self.node_mut().stacks = stacks;
            self.dock.snap = s;
        }
    }

    /// 定时器:后台有新数据、连接状态在变、提示过期时重绘
    fn dock_tick(&mut self, cx: &mut Context<Self>) {
        if !self.dock_visible() {
            return;
        }
        let mut redraw = false;
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
        if redraw {
            cx.notify();
        }
    }

    pub(crate) fn dock_refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(d) = self.workspace.active.and_then(|a| self.workspace.tabs.get(a)).and_then(|t| t.docker.as_ref()) {
            d.refresh();
        }
        cx.notify();
    }

    pub(crate) fn dock_act(&mut self, a: Action, cx: &mut Context<Self>) {
        if self.demo {
            self.dock.notice = Some(("演示数据,不会真的执行".into(), Instant::now()));
        } else if let Some(d) = self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref()) {
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
                if d.snapshot().is_some() {
                    return None;
                }
                match d.problem() {
                    Some(p) => Some(self.problem_page(p, cx).into_any_element()),
                    None => Some(loading("正在读取容器…").into_any_element()),
                }
            }
        }
    }

    fn problem_page(&self, p: Problem, cx: &mut Context<Self>) -> Div {
        let (ic, title, sub): (IconName, &'static str, String) = match &p {
            Problem::NoDocker => (IconName::Container, "这台服务器没有安装 Docker", "终端和文件功能仍然可用".into()),
            Problem::DaemonDown(m) => (IconName::CircleAlert, "Docker 没有在运行", m.clone()),
            Problem::NoPermission(m) => (IconName::Lock, "没有权限运行 docker", m.clone()),
            Problem::Other(m) => (IconName::TriangleAlert, "读取 Docker 失败", m.clone()),
        };
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .child(icon(ic, 34.0, c::ghost()))
            .child(div().mt(px(4.0)).text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child(title))
            .child(div().max_w(px(520.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(sub))
            .child(button("dock-retry", IconName::RefreshCw, "重试").mt(px(6.0)).on_click(cx.listener(|this, _, _, cx| {
                if let Some(t) = this.workspace.active.and_then(|a| this.workspace.tabs.get_mut(a)) {
                    t.docker = None;
                }
                this.dock.seen = 0;
                cx.notify();
            })))
    }

    /// 页头右侧的按钮(各页不同)
    pub(crate) fn dock_header_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v: Vec<AnyElement> = Vec::new();
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
        if self.page == Page::Images {
            v.push(
                button("dock-pull", IconName::Download, "拉取镜像")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.dock.pull_open = !this.dock.pull_open;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        v.push(button("dock-refresh", IconName::RefreshCw, "刷新").on_click(cx.listener(|this, _, _, cx| this.dock_refresh(cx))).into_any_element());
        v
    }

    pub(crate) fn dock_subtitle(&self) -> String {
        let n = &self.node().name;
        match self.page {
            Page::Overview => format!("{n} 上的 Docker 概况"),
            Page::Containers => format!("{n} 上的全部容器"),
            Page::Stacks => format!("{n} 上的 Compose 项目"),
            Page::Images => format!("{n} 上的本地镜像"),
            Page::Volumes => format!("{n} 上的存储卷"),
        }
    }

    pub(crate) fn docker_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.page {
            Page::Overview => self.overview_page(),
            Page::Containers => self.containers_page(cx),
            Page::Images => self.images_page(cx),
            Page::Volumes => self.volumes_page(cx),
            Page::Stacks => div().into_any_element(),
        }
    }

    /// 页面最上层的覆盖物:确认框、操作结果提示
    pub(crate) fn dock_overlays(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v: Vec<AnyElement> = Vec::new();
        if let Some(cf) = &self.dock.confirm {
            let action = cf.action.clone();
            v.push(
                div()
                    .id("dock-confirm")
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(c::scrim_dialog())
                    .occlude()
                    .child(
                        div()
                            .w(px(380.0))
                            .p(px(20.0))
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .rounded(px(t::RADIUS_XL))
                            .bg(c::dialog())
                            .border_1()
                            .border_color(c::dialog_border())
                            .child(div().text_size(px(t::TYPE_TITLE)).font_weight(FontWeight::SEMIBOLD).child(cf.title.clone()))
                            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(cf.body.clone()))
                            .child(
                                div()
                                    .mt(px(10.0))
                                    .flex()
                                    .flex_row()
                                    .justify_end()
                                    .gap(px(8.0))
                                    .child(button("dock-cancel", IconName::X, "取消").on_click(cx.listener(|this, _, _, cx| {
                                        this.dock.confirm = None;
                                        cx.notify();
                                    })))
                                    .child(
                                        div()
                                            .id("dock-ok")
                                            .h(px(30.0))
                                            .px(px(14.0))
                                            .flex()
                                            .items_center()
                                            .rounded(px(t::RADIUS_MD))
                                            .bg(c::danger())
                                            .text_color(c::on_accent())
                                            .text_size(px(t::TYPE_CONTROL))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .cursor_pointer()
                                            .child(cf.label)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.dock.confirm = None;
                                                this.dock_act(action.clone(), cx);
                                            })),
                                    ),
                            ),
                    )
                    .into_any_element(),
            );
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

    fn ask(&mut self, title: String, body: String, label: &'static str, action: Action, cx: &mut Context<Self>) {
        self.dock.confirm = Some(Confirm { title, body, label, action });
        cx.notify();
    }

    fn is_busy(&self, target: &str) -> bool {
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
            .child(div().w(px(112.0)));

        let mut rows: Vec<Stateful<Div>> = Vec::new();
        for (i, ct) in list.iter().enumerate() {
            let st = stat(ct);
            let up = ct.running();
            let (fg, bg) = state_color(ct);
            let busy = self.is_busy(&ct.name);
            let name = ct.name.clone();
            let mut acts = div().w(px(112.0)).flex().flex_row().justify_end().items_center().gap(px(2.0));
            if let Some(p) = ct.panel {
                acts = acts.child(tag(format!("{p} · 只读")));
            } else if busy {
                acts = acts.child(small("处理中…", c::muted()));
            } else {
                if up {
                    let n = name.clone();
                    acts = acts.child(icon_btn(("stop", i), IconName::Square, c::muted()).on_click(cx.listener(move |this, _, _, cx| this.dock_act(Action::Stop(n.clone()), cx))));
                    let n = name.clone();
                    acts = acts.child(icon_btn(("restart", i), IconName::RotateCw, c::muted()).on_click(cx.listener(move |this, _, _, cx| this.dock_act(Action::Restart(n.clone()), cx))));
                } else {
                    let n = name.clone();
                    acts = acts.child(icon_btn(("start", i), IconName::Play, c::online()).on_click(cx.listener(move |this, _, _, cx| this.dock_act(Action::Start(n.clone()), cx))));
                }
                let n = name.clone();
                acts = acts.child(icon_btn(("rm", i), IconName::Trash, c::danger()).on_click(cx.listener(move |this, _, _, cx| {
                    this.ask(format!("删除容器 {n}?"), "容器会被强制删除(运行中的先停止),容器内未保存到存储卷的数据会丢失。".into(), "删除", Action::Remove(n.clone()), cx);
                })));
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
                    .child(div().w(px(104.0)).child(pill(docker::uptime_text(ct), fg, bg)))
                    .child(div().w(px(58.0)).text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::secondary()).child(if up { format!("{:.1}%", st.cpu) } else { "—".into() }))
                    .child(div().w(px(74.0)).text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::secondary()).child(if up { docker::fmt_bytes(st.mem) } else { "—".into() }))
                    .child(div().w(px(150.0)).flex().flex_row().gap(px(4.0)).overflow_hidden().children(ports).children((more > 0).then(|| small(format!("+{more}"), c::ghost()))))
                    .child(acts),
            );
        }
        if rows.is_empty() {
            return empty_state(IconName::Search, "没有匹配的容器", "换个关键字试试").into_any_element();
        }
        div().flex_1().min_h_0().pb(px(14.0)).flex().flex_col().child(table("ct-list", header, rows)).into_any_element()
    }

    // ---- 镜像 ----

    fn images_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let snap = self.dock.snap.clone();
        let mut col = div().flex_1().min_h_0().pb(px(14.0)).flex().flex_col().gap(px(10.0));
        if self.dock.pull_open {
            if let Some(p) = self.dock.pull.clone() {
                col = col.child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
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
                            let name = p.read(cx).value().trim().to_string();
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
            let busy = self.is_busy(&target);
            let mut act = div().w(px(40.0)).flex().justify_end();
            if busy {
                act = act.child(small("…", c::muted()));
            } else if users == 0 {
                let (tg, nm) = (target.clone(), name.clone());
                act = act.child(icon_btn(("rmi", i), IconName::Trash, c::danger()).on_click(cx.listener(move |this, _, _, cx| {
                    this.ask(format!("删除镜像 {nm}?"), "只删除本地副本,需要时可以重新拉取。".into(), "删除", Action::RemoveImage(tg.clone()), cx);
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
        col.child(table("im-list", header, rows)).into_any_element()
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
            let busy = self.is_busy(&v.name);
            let mut act = div().w(px(40.0)).flex().justify_end();
            if busy {
                act = act.child(small("…", c::muted()));
            } else if users.is_empty() {
                let n = v.name.clone();
                act = act.child(icon_btn(("rmv", i), IconName::Trash, c::danger()).on_click(cx.listener(move |this, _, _, cx| {
                    this.ask(format!("删除存储卷 {n}?"), "卷里的数据会永久丢失,无法恢复。".into(), "删除", Action::RemoveVolume(n.clone()), cx);
                })));
            }
            let mounted: Vec<Div> = users.iter().take(3).map(|c| tag(c.name.clone())).collect();
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
        div().flex_1().min_h_0().pb(px(14.0)).flex().flex_col().child(table("vl-list", header, rows)).into_any_element()
    }

    // ---- 概览 ----

    fn overview_page(&self) -> AnyElement {
        let snap = &self.dock.snap;
        let total = snap.containers.len();
        let up = snap.running();
        let stacks = &self.node().stacks;
        let agg = super::stacks::aggregate(stacks.iter().flat_map(|s| s.services.iter()));
        let attention = snap.attention();
        let panel_n = snap.containers.iter().filter(|c| c.panel.is_some()).count();
        let mem = mock::fmt_mem(agg.mem_mb());
        let (mem_v, mem_u) = mem.split_once(' ').unwrap_or((&mem, ""));
        let hide = self.hide_charts;

        let tiles1 = div()
            .flex()
            .flex_row()
            .gap(px(10.0))
            .pb(px(10.0))
            .flex_shrink_0()
            .child(stat_tile(IconName::Container, "容器", format!("{up}/{total}"), if up == total { small("全部运行", c::online()) } else { small(format!("{} 个未运行", total - up), c::warn()) }, None))
            .child(stat_tile(IconName::Layers, "堆栈", stacks.iter().filter(|s| s.name != "独立容器").count().to_string(), small("个 Compose 项目", c::muted()), None))
            .child(stat_tile(IconName::Package, "镜像", snap.images.len().to_string(), small(format!("{} 个未使用", snap.images.iter().filter(|i| snap.image_users(i) == 0).count()), c::muted()), None))
            .child(stat_tile(IconName::HardDrive, "存储卷", snap.volumes.len().to_string(), small(format!("{} 个未使用", snap.volumes.iter().filter(|v| snap.volume_users(v).is_empty()).count()), c::muted()), None));
        let tiles2 = div()
            .flex()
            .flex_row()
            .gap(px(10.0))
            .pb(px(14.0))
            .flex_shrink_0()
            .child(stat_tile(IconName::Cpu, "CPU 合计", format!("{:.1}", agg.cpu()), small("%", c::muted()), (!hide).then(|| spark(&agg.cpu_s(), c::cpu(), 26.0, 2.0))))
            .child(stat_tile(IconName::MemoryStick, "内存合计", mem_v.to_string(), small(mem_u.to_string(), c::muted()), (!hide).then(|| spark(&agg.mem_s(), c::mem(), 26.0, 2.0))));

        let mut list = div().flex().flex_col();
        if attention.is_empty() {
            list = list.child(div().h(px(48.0)).px(px(16.0)).flex().items_center().gap(px(8.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(icon(IconName::Check, 14.0, c::online())).child("一切正常,没有需要关注的容器"));
        }
        for (i, ct) in attention.iter().enumerate() {
            let (fg, bg) = state_color(ct);
            list = list.child(
                row(("att", i))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(ct.name.clone()))
                    .child(div().w(px(110.0)).truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(ct.project.clone().unwrap_or_else(|| "—".into())))
                    .child(div().w(px(200.0)).truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(ct.status.clone()))
                    .child(pill(docker::uptime_text(ct), fg, bg)),
            );
        }
        let attention_card = div()
            .w_full()
            .flex()
            .flex_col()
            .glass(t::RADIUS_XL, GlassStyle::CARD)
            .overflow_hidden()
            .child(div().h(px(38.0)).px(px(16.0)).flex().flex_row().items_center().gap(px(8.0)).border_b_1().border_color(c::border()).text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child(icon(IconName::TriangleAlert, 14.0, if attention.is_empty() { c::ghost() } else { c::warn() })).child(format!("需要关注({})", attention.len())))
            .child(list);

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(tiles1)
            .child(tiles2)
            .child(attention_card)
            .children((panel_n > 0).then(|| div().mt(px(12.0)).text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(format!("{panel_n} 个容器由 1Panel / 宝塔 管理,本应用只读,不会修改。"))))
            .into_any_element()
    }
}

fn loading(msg: &'static str) -> Div {
    div().flex_1().flex().items_center().justify_center().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(msg)
}
