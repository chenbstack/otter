//! 服务器运维页:进程、服务、存储、计划任务。
//!
//! 数据来自 SSH 命令的输出(脚本和解析在 `crate::sysops`),和 Docker 互不依赖:没装 Docker 的服务器也能用。
//! 读取类命令不需要 root;结束进程、启停服务这类操作走 sudo(免密,或用节点保存的密码)。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};

use super::daemonpage::field;

use super::super::*;
use super::dockerpages::{DockAsk, head, icon_btn, icon_btn_busy, row, small, table_flow};
use crate::docker::Action;
use crate::sysops::{self, CronInfo, Job, Kind, Mount, Proc, Service};
use super::spacelens::LensStore;

/// 进程列表的刷新间隔
fn proc_every() -> Duration {
    Duration::from_secs(crate::prefs::proc_secs())
}
/// 发出结束后等多久,进程还在就提供强制结束
const FORCE_AFTER: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, PartialEq, Default)]
pub(crate) enum ProcSort {
    #[default]
    Cpu,
    Mem,
    Pid,
}

#[derive(Clone, Copy, PartialEq, Default)]
pub(crate) enum SvcFilter {
    #[default]
    All,
    Running,
    Stopped,
    Failed,
}

#[derive(Default)]
pub(crate) struct SysUi {
    search: Option<Entity<InputState>>,
    /// 上次同步时在哪一页(换页时清掉搜索、重新读取)
    shown: Option<Page>,
    procs: Option<Result<Vec<Proc>, String>>,
    services: Option<Result<Vec<Service>, String>>,
    pub(super) storage: Option<Result<Vec<Mount>, String>>,
    cron: Option<Result<CronInfo, String>>,
    /// 存储页的空间透镜(点开的挂载点和扫描结果)
    pub(super) lens: Option<LensStore>,
    /// 透镜扫描的编号,每次扫描加一
    pub(super) lens_seq: u64,
    pub(super) asked: HashMap<Kind, Instant>,
    pub(super) inflight: HashSet<Kind>,
    /// 网站页的状态和表单
    pub(super) web: super::websites::WebUi,
    sort: ProcSort,
    filter: SvcFilter,
    /// 计划任务的新增/编辑表单:None 是没打开,Some(None) 是新增,Some(Some(行号)) 是编辑 crontab 里的那一行
    cron_form: Option<Option<usize>>,
    /// 计划、命令两个输入框
    cron_inputs: Option<Vec<Entity<InputState>>>,
    demo_filled: bool,
    /// 窗口当前是否在前台、是否失去过焦点(只有前台才自动读;重新获得焦点时读一次)
    active: bool,
    blurred: bool,
    /// 已经发出「结束」的进程和发出的时间;过 1 秒还在,就把「结束」换成「强制结束」
    killed: HashMap<u32, Instant>,
    /// 刚点删除的计划任务所在行(crontab 行号),让转圈出现在这一行
    cron_row: Option<usize>,
    /// 空间透镜里悬停的子目录下标(圆环高亮对应的一段)
    pub(super) lens_hover: Option<String>,
    /// 透镜里已经展开的块(完整路径)
    pub(super) lens_open: HashSet<String>,
    /// 双击放大的目录栈,最后一个当顶层;空是整个挂载点
    pub(super) lens_zoom: Vec<String>,
    /// 扫描前等确认的大存储:(挂载点, 已用 KiB, 是不是网络存储)
    pub(super) lens_ask: Option<(String, u64, bool)>,
    /// 正在做访问检查的挂载点:(路径, 已用 KiB, 是不是网络存储, 这次是不是用 root 检查)
    pub(super) lens_probe: Option<(String, u64, bool, bool)>,
    /// 透镜画布的实际大小(像素,canvas 量出来的),色块按它排版
    pub(super) lens_size: std::rc::Rc<std::cell::Cell<(f32, f32)>>,
    /// 右键菜单:(块的完整路径, 点击位置)
    pub(super) lens_menu: Option<(String, Point<Pixels>)>,
    /// 正在走删除流程的块
    pub(super) lens_del: Option<super::spacelens::LensDel>,
}

fn spec(p: Page) -> Option<(Kind, &'static str)> {
    match p {
        Page::Processes => Some((Kind::Procs, sysops::PROCS_SCRIPT)),
        Page::Services => Some((Kind::Services, sysops::SERVICES_SCRIPT)),
        Page::Storage => Some((Kind::Storage, sysops::STORAGE_SCRIPT)),
        Page::Cron => Some((Kind::Cron, sysops::CRON_SCRIPT)),
        // 网站页的脚本是动态生成的(见 `sys_sync`)
        Page::Websites => Some((Kind::Web, "")),
        _ => None,
    }
}

/// 新结果替换旧的;新结果是错误而旧的是好数据时保留旧的(重读失败不清掉已经显示的内容)
fn keep_old<T>(slot: &mut Option<Result<T, String>>, new: Result<T, String>) {
    if new.is_err() && matches!(slot, Some(Ok(_))) {
        return;
    }
    *slot = Some(new);
}

/// 页面停留时自动重读的间隔(进程页另有自己的间隔;网站页见 `web_reload_due`)
fn reload_every(k: Kind) -> Option<Duration> {
    match k {
        Kind::Services | Kind::Cron => Some(Duration::from_secs(crate::prefs::page_secs())),
        Kind::Storage => Some(Duration::from_secs(crate::prefs::page_secs())),
        _ => None,
    }
}

pub(super) fn chip(id: (&'static str, usize), label: impl Into<SharedString>, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(30.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .cursor_pointer()
        .text_size(px(t::TYPE_CONTROL))
        .when(active, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::SEMIBOLD))
        .when(!active, |d| d.rounded(px(t::RADIUS_MD)).border_1().border_color(gpui_kit::transparent_black()).text_color(c::muted()).hover(|s| s.bg(c::field_hover()).text_color(c::secondary())))
        .child(label.into())
}

pub(super) fn tip(b: Stateful<Div>, text: &'static str) -> Stateful<Div> {
    b.tooltip(move |_, cx| cx.new(|_| crate::files::Tip(text.into())).into())
}

fn cell(w: f32, s: impl Into<SharedString>) -> Div {
    div().w(px(w)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(s.into())
}

fn mono_cell(w: f32, s: impl Into<SharedString>, color: Rgba) -> Div {
    div().w(px(w)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(color).child(s.into())
}

fn message(icon_name: IconName, title: &'static str, detail: String) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .child(empty_state(icon_name, title, "处理后点右上角「刷新」重试"))
        .child(div().pb(px(60.0)).px(px(24.0)).flex().justify_center().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child(SharedString::from(detail)))
        .into_any_element()
}

fn reading(what: &'static str) -> AnyElement {
    crate::ui::loading(format!("正在{what}…")).into_any_element()
}

fn job(label: &str, target: &str, script: String, reload: (Kind, &str)) -> Job {
    Job { label: label.into(), target: target.into(), script, root: true, input: None, reload: Some((reload.0, reload.1.to_string(), false)) }
}

fn svc_ask(title: &'static str, body: &'static str, verb: &'static str, cmd: &str, ic: IconName, name: String) -> DockAsk {
    let j = job(verb, &name, format!("systemctl {cmd} {name}.service"), (Kind::Services, sysops::SERVICES_SCRIPT));
    DockAsk { title, tile: ic, name, detail: "systemd 服务", body, label: verb, action: Action::Sys(j), list: Vec::new() }
}

fn usage_color(pct: u8) -> Rgba {
    if pct >= 90 {
        c::danger()
    } else if pct >= 80 {
        c::warn()
    } else {
        c::online()
    }
}

fn bar(frac: f32, w: f32, color: Rgba) -> Div {
    div().w(px(w)).h(px(6.0)).flex_shrink_0().rounded_full().bg(c::idle_soft()).overflow_hidden().child(div().h_full().w(px((w * frac.clamp(0.0, 1.0)).max(if frac > 0.0 { 2.0 } else { 0.0 }))).rounded_full().bg(color))
}

impl Home {
    pub(super) fn sys_docker(&self) -> Option<&crate::docker::Docker> {
        self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref())
    }

    /// 刷新按钮:当前页的数据重新读
    /// 换了标签/节点:整个运维页状态回到初始(数据和表单都是上一台服务器的)
    pub(crate) fn sys_clear(&mut self) {
        self.dock.sys = SysUi::default();
    }

    pub(crate) fn sys_reset(&mut self) {
        self.dock.sys.asked.clear();
        self.dock.sys.inflight.clear();
        // 透镜重新扫一遍挂载点
        let root = self.dock.sys.lens.as_ref().filter(|l| !l.is_demo() && !l.probing).map(|l| (l.root.clone(), l.total, l.as_root));
        if let Some((path, total, as_root)) = root {
            self.lens_begin_as(&path, total, as_root);
        }
    }

    /// 进程页在前台、上一次读取已经返回且过了刷新间隔:需要重绘来触发下一次读取
    pub(crate) fn sys_due(&self) -> bool {
        // 刚过了 1 秒的「结束」:重绘一次,把按钮换成强制结束
        let force_now = self.page == Page::Processes && self.dock.sys.killed.values().any(|t| t.elapsed() >= FORCE_AFTER && t.elapsed() < FORCE_AFTER + Duration::from_millis(900));
        force_now || self.web_due() || self.api_due() || (self.page == Page::Processes && self.dock.sys.active && !self.demo && !self.dock.sys.inflight.contains(&Kind::Procs) && self.dock.sys.asked.get(&Kind::Procs).is_some_and(|t| t.elapsed() >= proc_every())) || spec(self.page).is_some_and(|(k, _)| self.sys_reload_due(k))
    }

    /// 服务、存储、计划任务页:窗口在前台、没有在编辑、上次读取已返回且过了间隔,该重读了
    fn sys_reload_due(&self, kind: Kind) -> bool {
        let Some(every) = reload_every(kind) else { return false };
        let s = &self.dock.sys;
        s.active && !self.demo && spec(self.page).is_some_and(|(k, _)| k == kind) && s.cron_form.is_none() && !s.inflight.contains(&kind) && s.asked.get(&kind).is_some_and(|t| t.elapsed() >= every)
    }

    /// 记下刚发出的结束操作(`dock_act` 调用)
    pub(crate) fn sys_note(&mut self, j: &Job) {
        if j.label.ends_with("结束进程") {
            if let Ok(pid) = j.target.parse::<u32>() {
                self.dock.sys.killed.insert(pid, Instant::now());
            }
        }
    }

    pub(super) fn sys_ask(&mut self, kind: Kind, script: &str, root: bool) {
        self.dock.sys.asked.insert(kind, Instant::now());
        self.dock.sys.inflight.insert(kind);
        if let Some(d) = self.sys_docker() {
            d.sys_load(kind, script, root);
        }
    }

    fn sys_run(&mut self, job: Job, cx: &mut Context<Self>) {
        self.dock_act(Action::Sys(job), cx);
    }

    fn sys_store(&mut self, kind: Kind, res: Result<String, String>, window: &mut Window, cx: &mut Context<Self>) {
        self.dock.sys.inflight.remove(&kind);
        let s = &mut self.dock.sys;
        match kind {
            Kind::Procs => {
                keep_old(&mut s.procs, res.and_then(|t| sysops::parse_procs(&t)));
                // 已经结束的进程不用再记
                if let Some(Ok(l)) = &s.procs {
                    s.killed.retain(|pid, _| l.iter().any(|p| p.pid == *pid));
                }
            }
            Kind::Services => keep_old(&mut s.services, res.and_then(|t| sysops::parse_services(&t))),
            Kind::Storage => keep_old(&mut s.storage, res.and_then(|t| sysops::parse_storage(&t))),
            Kind::Cron => keep_old(&mut s.cron, res.map(|t| sysops::parse_cron(&t))),
            Kind::Web => self.web_loaded(res, cx),
            Kind::WebOp => self.web_op_done(res, cx),
            Kind::WebPoll => self.web_polled(res, cx),
            Kind::ComposeFile | Kind::ComposeEnv => {}
            Kind::Probe => self.lens_probed(res, cx),
            Kind::Remove => self.lens_removed(res, cx),
        }
        let _ = (window, cx);
    }

    /// 每次渲染前调用(运维页在前台时):建搜索框、收新数据、按需要发起读取
    pub(crate) fn sys_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.page;
        if page != Page::Websites {
            self.web_page_left();
        }
        let Some((kind, script)) = spec(page) else { return };
        if self.dock.sys.search.is_none() {
            let s = cx.new(|cx| InputState::new(window, cx).placeholder("搜索"));
            cx.observe(&s, |_, _, cx| cx.notify()).detach();
            self.dock.sys.search = Some(s);
        }
        if self.dock.sys.shown != Some(page) {
            self.dock.sys.shown = Some(page);
            self.dock.sys.asked.remove(&kind);
            self.dock.sys.cron_form = None;
            self.lens_close();
            if let Some(s) = self.dock.sys.search.clone() {
                s.update(cx, |s, cx| s.set_value("", window, cx));
            }
        }
        if self.demo {
            self.sys_demo();
            if page == Page::Websites {
                self.web_demo();
            }
            // 调试:截图时直接打开编辑表单
            if page == Page::Cron && self.dock.sys.cron_form.is_none() && self.dock.sys.cron_inputs.is_none() && std::env::var_os("OTTER_CRON_FORM").is_some() {
                self.cron_open(Some(1), "0 3 * * *", "/usr/local/bin/backup.sh --full", window, cx);
            }
            return;
        }
        let got: Vec<(Kind, Result<String, String>)> = match self.sys_docker() {
            Some(d) => [kind, Kind::Probe, Kind::Remove, Kind::WebOp, Kind::WebPoll].into_iter().filter_map(|k| d.sys_take(k).map(|r| (k, r))).collect(),
            None => return,
        };
        self.lens_pump();
        self.web_pump();
        for (k, r) in got {
            self.sys_store(k, r, window, cx);
        }
        if kind == Kind::Web {
            self.web_entered(window.is_window_active());
        }
        // 窗口在后台不读;重新获得焦点时重读一次(类似 SWR 的 revalidate on focus)
        let active = window.is_window_active();
        let refocus = active && self.dock.sys.blurred;
        self.dock.sys.active = active;
        self.dock.sys.blurred = !active;
        if refocus && kind != Kind::Web {
            self.dock.sys.asked.remove(&kind);
        }
        let idle = !self.dock.sys.inflight.contains(&kind);
        let due = match self.dock.sys.asked.get(&kind) {
            None => true,
            Some(t) => active && ((kind == Kind::Procs && t.elapsed() >= proc_every()) || (kind == Kind::Web && self.web_reload_due()) || self.sys_reload_due(kind)),
        };
        if idle && due {
            if kind == Kind::Web {
                self.sys_ask(kind, &crate::web::state_script(), true);
            } else {
                self.sys_ask(kind, script, false);
            }
        }
    }

    fn sys_demo(&mut self) {
        if self.dock.sys.demo_filled {
            return;
        }
        self.dock.sys.demo_filled = true;
        let s = &mut self.dock.sys;
        s.procs = Some(sysops::parse_procs(DEMO_PROCS));
        s.services = Some(sysops::parse_services(DEMO_SERVICES));
        s.storage = Some(sysops::parse_storage(DEMO_STORAGE));
        s.cron = Some(Ok(sysops::parse_cron(DEMO_CRON)));
        if std::env::var_os("OTTER_SYS_TOP").is_some() {
            s.lens = Some(LensStore::mock("/"));
            // 截图用:OTTER_LENS_HOVER=/var/lib  OTTER_LENS_OPEN="/var/lib;/usr"
            s.lens_hover = std::env::var("OTTER_LENS_HOVER").ok();
            s.lens_open = std::env::var("OTTER_LENS_OPEN").map(|v| v.split(';').filter(|x| !x.is_empty()).map(str::to_string).collect()).unwrap_or_default();
        }
    }

    fn sys_query(&self, cx: &App) -> String {
        self.dock.sys.search.as_ref().map(|s| s.read(cx).value().trim().to_lowercase()).unwrap_or_default()
    }

    // ---- 页头 ----

    pub(crate) fn sys_header_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v: Vec<AnyElement> = Vec::new();
        let search = |s: Entity<InputState>| {
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
                .into_any_element()
        };
        match self.page {
            Page::Processes => {
                let mut row = div().flex().flex_row().items_center().gap(px(2.0));
                for (i, (label, k)) in [("CPU", ProcSort::Cpu), ("内存", ProcSort::Mem), ("PID", ProcSort::Pid)].into_iter().enumerate() {
                    row = row.child(chip(("ps-sort", i), label, self.dock.sys.sort == k).on_click(cx.listener(move |this, _, _, cx| {
                        this.dock.sys.sort = k;
                        cx.notify();
                    })));
                }
                v.push(small("排序", c::ghost()).into_any_element());
                v.push(row.into_any_element());
                v.extend(self.dock.sys.search.clone().map(search));
            }
            Page::Services => {
                let mut row = div().flex().flex_row().items_center().gap(px(2.0));
                for (i, (label, k)) in [("全部", SvcFilter::All), ("运行中", SvcFilter::Running), ("已停止", SvcFilter::Stopped), ("失败", SvcFilter::Failed)].into_iter().enumerate() {
                    row = row.child(chip(("sv-filter", i), label, self.dock.sys.filter == k).on_click(cx.listener(move |this, _, _, cx| {
                        this.dock.sys.filter = k;
                        cx.notify();
                    })));
                }
                v.push(row.into_any_element());
                v.extend(self.dock.sys.search.clone().map(search));
            }
            Page::Websites => v.extend(self.web_header_actions(cx)),
            Page::Cron => {
                if matches!(self.dock.sys.cron, Some(Ok(_))) {
                    v.push(button("cron-add", IconName::Plus, "新增任务").on_click(cx.listener(|this, _, window, cx| this.cron_open(None, "", "", window, cx))).into_any_element());
                }
            }
            _ => {}
        }
        v
    }

    pub(crate) fn sys_subtitle(&self) -> Option<String> {
        let n = &self.node().name;
        match self.page {
            Page::Processes => Some(format!("{n} 上正在运行的进程,每 {} 秒刷新", crate::prefs::proc_secs())),
            Page::Services => Some(format!("{n} 上的 systemd 服务")),
            Page::Storage => Some(format!("{n} 的磁盘和挂载点,点一行看里面最大的目录")),
            Page::Websites => Some(self.web_subtitle()),
            Page::Cron => Some(format!("{n} 上的 crontab 和 systemd 定时器;只能改自己的 crontab")),
            _ => None,
        }
    }

    pub(crate) fn sys_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.page {
            Page::Processes => self.procs_page(cx),
            Page::Services => self.services_page(cx),
            Page::Storage => self.storage_page(cx),
            Page::Websites => self.web_page(cx),
            _ => self.cron_page(cx),
        }
    }

    // ---- 进程 ----

    fn procs_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let list = match &self.dock.sys.procs {
            None => return reading("读取服务器上的进程"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了进程列表", e.clone()),
            Some(Ok(l)) => l,
        };
        let q = self.sys_query(cx);
        let mut v: Vec<&Proc> = list.iter().filter(|p| q.is_empty() || p.cmd.to_lowercase().contains(&q) || p.user.to_lowercase().contains(&q) || p.pid.to_string().contains(&q)).collect();
        match self.dock.sys.sort {
            ProcSort::Cpu => v.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(a.pid.cmp(&b.pid))),
            ProcSort::Mem => v.sort_by(|a, b| b.rss_kb.cmp(&a.rss_kb).then(a.pid.cmp(&b.pid))),
            ProcSort::Pid => v.sort_by_key(|p| p.pid),
        }
        let total = v.len();
        v.truncate(200);
        let narrow = self.dock.view_w < 900.0;
        let mut header = div().child(head("PID").w(px(64.0))).child(head("用户").w(px(96.0))).child(head("CPU").w(px(56.0))).child(head("内存").w(px(56.0))).child(head("占用").w(px(76.0)));
        if !narrow {
            header = header.child(head("运行时长").w(px(88.0)));
        }
        header = header.child(head("命令").flex_1().min_w_0()).child(div().w(px(60.0)));
        let mut rows = Vec::new();
        for (i, p) in v.iter().enumerate() {
            let name = p.cmd.split_whitespace().next().map(|s| s.rsplit('/').next().unwrap_or(s)).unwrap_or("进程").to_string();
            let target = p.pid.to_string();
            let mut act = div().w(px(60.0)).flex_shrink_0().flex().flex_row().justify_end().gap(px(2.0));
            let pending = self.dock.sys.killed.get(&p.pid).map(|t| t.elapsed());
            if self.op_busy(&target).is_some() || pending.is_some_and(|e| e < FORCE_AFTER) {
                act = act.child(tip(icon_btn_busy(("pk", i), IconName::Square, c::danger(), true), "结束中"));
            } else if pending.is_some() {
                // 发出结束 1 秒后进程还在:换成强制结束
                let (pid, n2) = (p.pid, name.clone());
                act = act.child(tip(icon_btn(("pf", i), IconName::Zap, c::warn()), "强制结束").on_click(cx.listener(move |this, _, _, cx| {
                    let j = job("强制结束进程", &pid.to_string(), format!("kill -KILL {pid}"), (Kind::Procs, sysops::PROCS_SCRIPT));
                    this.ask(DockAsk { title: "强制结束进程", tile: IconName::Zap, name: format!("{n2} · PID {pid}"), detail: "进程", body: "发送 SIGKILL,进程会被立刻杀死,来不及保存数据", label: "强制结束", action: Action::Sys(j), list: Vec::new() }, cx);
                })));
            } else {
                let (pid, n1) = (p.pid, name.clone());
                act = act.child(tip(icon_btn(("pk", i), IconName::Square, c::danger()), "结束").on_click(cx.listener(move |this, _, _, cx| {
                    let j = job("结束进程", &pid.to_string(), format!("kill -TERM {pid}"), (Kind::Procs, sysops::PROCS_SCRIPT));
                    this.ask(DockAsk { title: "结束进程", tile: IconName::Cpu, name: format!("{n1} · PID {pid}"), detail: "进程", body: "向进程发送 SIGTERM,让它正常退出", label: "结束进程", action: Action::Sys(j), list: Vec::new() }, cx);
                })));
            }
            let cpu_color = if p.cpu >= 80.0 { c::danger() } else if p.cpu >= 40.0 { c::warn() } else { c::secondary() };
            let mut r = row(("pr", i))
                .child(mono_cell(64.0, target, c::muted()))
                .child(cell(96.0, p.user.clone()))
                .child(mono_cell(56.0, format!("{:.1}", p.cpu), cpu_color))
                .child(mono_cell(56.0, format!("{:.1}", p.mem), c::secondary()))
                .child(mono_cell(76.0, sysops::fmt_kb(p.rss_kb), c::secondary()));
            if !narrow {
                r = r.child(mono_cell(88.0, p.etime.clone(), c::muted()));
            }
            rows.push(r.child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::text()).child(p.cmd.clone())).child(act));
        }
        if rows.is_empty() {
            return empty_state(IconName::Search, "没有匹配的进程", "换个关键字试试").into_any_element();
        }
        let _ = total;
        div().flex_shrink_0().pb(px(14.0)).flex().flex_col().child(table_flow(header, rows)).into_any_element()
    }

    // ---- 服务 ----

    fn services_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let list = match &self.dock.sys.services {
            None => return reading("读取服务器上的 systemd 服务"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了服务列表", e.clone()),
            Some(Ok(l)) => l,
        };
        let q = self.sys_query(cx);
        let filter = self.dock.sys.filter;
        let v: Vec<&Service> = list
            .iter()
            .filter(|s| match filter {
                SvcFilter::All => true,
                SvcFilter::Running => s.running(),
                SvcFilter::Stopped => !s.running() && !s.failed(),
                SvcFilter::Failed => s.failed(),
            })
            .filter(|s| q.is_empty() || s.name.to_lowercase().contains(&q) || s.desc.to_lowercase().contains(&q))
            .take(400)
            .collect();
        if v.is_empty() {
            return empty_state(IconName::Search, "没有匹配的服务", "换个关键字或筛选条件试试").into_any_element();
        }
        let narrow = self.dock.view_w < 900.0;
        let mut header = div().child(head("名称").w(px(230.0))).child(head("状态").w(px(80.0))).child(head("开机自启").w(px(72.0)));
        if !narrow {
            header = header.child(head("描述").flex_1().min_w_0());
        } else {
            header = header.child(div().flex_1());
        }
        header = header.child(div().w(px(92.0)));
        let reload = (Kind::Services, sysops::SERVICES_SCRIPT);
        let mut rows = Vec::new();
        for (i, s) in v.iter().enumerate() {
            let unit = format!("{}.service", s.name);
            let (label, fg, bg) = if s.running() {
                ("运行中", c::online(), c::online_soft())
            } else if s.failed() {
                ("失败", c::danger(), c::danger().opacity(0.17))
            } else if s.active == "activating" {
                ("启动中", c::warn(), c::warn_soft())
            } else if s.active == "active" {
                ("已完成", c::muted(), c::idle_soft())
            } else {
                ("已停止", c::muted(), c::idle_soft())
            };
            let (en_text, en_color) = match s.enabled.as_str() {
                "enabled" | "enabled-runtime" => ("已启用", c::online()),
                "disabled" => ("未启用", c::muted()),
                "static" => ("静态", c::ghost()),
                "masked" | "masked-runtime" => ("已屏蔽", c::warn()),
                "" => ("—", c::ghost()),
                _ => ("其他", c::ghost()),
            };
            let mut act = div().w(px(92.0)).flex_shrink_0().flex().flex_row().justify_end().gap(px(2.0));
            if sysops::safe_unit(&unit) {
                let name = s.name.clone();
                let busy = self.op_busy(&name).unwrap_or_default();
                match s.enabled.as_str() {
                    "enabled" => {
                        let n = name.clone();
                        let spin = busy == "关闭自启";
                        let b = tip(icon_btn_busy(("sv-en", i), IconName::Power, c::online(), spin), "关闭开机自启");
                        act = act.child(if spin { b } else { b.on_click(cx.listener(move |this, _, _, cx| {
                            this.ask(svc_ask("关闭开机自启", "服务当前的运行状态不变,下次开机时不再自动启动", "关闭自启", "disable", IconName::Power, n.clone()), cx);
                        })) });
                    }
                    "disabled" => {
                        let n = name.clone();
                        let spin = busy == "启用自启";
                        let b = tip(icon_btn_busy(("sv-en", i), IconName::Power, c::ghost(), spin), "设为开机自启");
                        act = act.child(if spin { b } else { b.on_click(cx.listener(move |this, _, _, cx| {
                            let j = job("启用自启", &n, format!("systemctl enable {n}.service"), reload);
                            this.sys_run(j, cx);
                        })) });
                    }
                    _ => {}
                }
                if s.running() || s.active == "activating" {
                    let n = name.clone();
                    let spin = busy == "重启服务";
                    let b = tip(icon_btn_busy(("sv-rs", i), IconName::RotateCw, c::secondary(), spin), "重启");
                    act = act.child(if spin { b } else { b.on_click(cx.listener(move |this, _, _, cx| {
                        this.ask(svc_ask("重启服务", "服务会短暂中断,正在处理的请求可能失败", "重启服务", "restart", IconName::RotateCw, n.clone()), cx);
                    })) });
                    let n = name.clone();
                    let spin = busy == "停止服务";
                    let b = tip(icon_btn_busy(("sv-st", i), IconName::Square, c::danger(), spin), "停止");
                    act = act.child(if spin { b } else { b.on_click(cx.listener(move |this, _, _, cx| {
                        this.ask(svc_ask("停止服务", "服务会停止运行,依赖它的程序可能受影响", "停止服务", "stop", IconName::Square, n.clone()), cx);
                    })) });
                } else {
                    let n = name.clone();
                    let spin = busy == "启动服务";
                    let b = tip(icon_btn_busy(("sv-go", i), IconName::Play, c::online(), spin), "启动");
                    act = act.child(if spin { b } else { b.on_click(cx.listener(move |this, _, _, cx| {
                        let j = job("启动服务", &n, format!("systemctl start {n}.service"), reload);
                        this.sys_run(j, cx);
                    })) });
                }
            }
            let mut r = row(("sv", i)).child(div().w(px(230.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(s.name.clone())).child(div().w(px(80.0)).flex_shrink_0().flex().flex_row().child(pill(label, fg, bg))).child(cell(72.0, en_text).text_color(en_color));
            r = if narrow { r.child(div().flex_1()) } else { r.child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(s.desc.clone())) };
            rows.push(r.child(act));
        }
        div().flex_shrink_0().pb(px(14.0)).flex().flex_col().child(table_flow(header, rows)).into_any_element()
    }

    // ---- 存储 ----

    fn storage_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let list = match &self.dock.sys.storage {
            None => return reading("读取服务器的磁盘信息"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了磁盘信息", e.clone()),
            Some(Ok(l)) => l,
        };
        let picked = self.dock.sys.lens.as_ref().map(|l| l.root.clone());
        let header = div().child(head("挂载点").flex_1().min_w_0()).child(head("设备").w(px(260.0))).child(head("类型").w(px(64.0))).child(head("用量").w(px(190.0))).child(head("已用 / 总量").w(px(150.0))).child(head("使用率").w(px(56.0))).child(head("inode").w(px(52.0)));
        let mut rows = Vec::new();
        for (i, m) in list.iter().enumerate() {
            let sel = picked.as_deref() == Some(m.mount.as_str());
            let path = m.mount.clone();
            let used_kb = m.used_kb;
            let inode = m.inode_pct.map(|p| format!("{p}%")).unwrap_or_else(|| "—".into());
            let inode_color = m.inode_pct.map_or(c::ghost(), |p| if p >= 90 { c::danger() } else { c::muted() });
            rows.push(
                row(("st", i))
                    .cursor_pointer()
                    .when(sel, |d| d.bg(c::field_hover()))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(m.mount.clone()))
                    .child(mono_cell(260.0, m.dev.clone(), c::muted()))
                    .child(cell(64.0, m.fstype.clone()))
                    .child(div().w(px(190.0)).flex_shrink_0().child(bar(m.pct as f32 / 100.0, 170.0, usage_color(m.pct))))
                    .child(mono_cell(150.0, format!("{} / {}", sysops::fmt_kb(m.used_kb), sysops::fmt_kb(m.total_kb)), c::secondary()))
                    .child(mono_cell(56.0, format!("{}%", m.pct), usage_color(m.pct)))
                    .child(mono_cell(52.0, inode, inode_color))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.lens_request(&path, used_kb, cx);
                    })),
            );
        }
        let mut col = div().flex_grow(1.0).flex_shrink_0().pb(px(14.0)).flex().flex_col().gap(px(12.0)).child(table_flow(header, rows));
        if self.dock.sys.lens.is_some() {
            col = col.child(self.lens_panel(cx));
        }
        col.into_any_element()
    }

    // ---- 计划任务 ----

    /// 打开新增/编辑表单(`line` 是要改的那一行)
    fn cron_open(&mut self, line: Option<usize>, sched: &str, cmd: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.sys.cron_inputs.is_none() {
            let v = ["*/5 * * * *", "/usr/local/bin/backup.sh"]
                .iter()
                .map(|ph| {
                    let e = cx.new(|cx| InputState::new(window, cx).placeholder(*ph));
                    cx.observe(&e, |_, _, cx| cx.notify()).detach();
                    e
                })
                .collect();
            self.dock.sys.cron_inputs = Some(v);
        }
        if let Some(ins) = &self.dock.sys.cron_inputs {
            ins[0].update(cx, |s, cx| s.set_value(sched, window, cx));
            ins[1].update(cx, |s, cx| s.set_value(cmd, window, cx));
        }
        self.dock.sys.cron_form = Some(line);
        if let Some(ins) = &self.dock.sys.cron_inputs {
            let f = ins[0].focus_handle(cx);
            window.focus(&f, cx);
        }
        cx.notify();
    }

    fn cron_notice(&mut self, msg: &'static str, cx: &mut Context<Self>) {
        self.dock.notice = Some((msg.into(), Instant::now()));
        cx.notify();
    }

    /// 保存表单:改写「我的 crontab」原文再整体写回(crontab 命令自己还会检查一遍)
    fn cron_submit(&mut self, cx: &mut Context<Self>) {
        let (Some(line), Some(ins)) = (self.dock.sys.cron_form, self.dock.sys.cron_inputs.clone()) else { return };
        let sched = ins[0].read(cx).value().split_whitespace().collect::<Vec<_>>().join(" ");
        let cmd = ins[1].read(cx).value().trim().to_string();
        if !sysops::valid_schedule(&sched) {
            return self.cron_notice("计划要写成 5 段(分 时 日 月 周),或 @daily、@reboot 这样的简写", cx);
        }
        if cmd.is_empty() || cmd.contains(['\n', '\r']) {
            return self.cron_notice("命令不能为空,也不能换行", cx);
        }
        let Some(Ok(info)) = &self.dock.sys.cron else { return };
        let text = sysops::crontab_replace(&info.user_text, line, Some(&format!("{sched} {cmd}")));
        self.dock.sys.cron_form = None;
        self.cron_write(if line.is_some() { "修改任务" } else { "添加任务" }, text, cx);
    }

    fn cron_write(&mut self, label: &str, text: String, cx: &mut Context<Self>) {
        self.dock.sys.cron_row = None;
        let j = Job { label: label.into(), target: "crontab".into(), script: "crontab -".into(), root: false, input: Some(text), reload: Some((Kind::Cron, sysops::CRON_SCRIPT.to_string(), false)) };
        self.sys_run(j, cx);
    }

    /// 新增/编辑计划任务的窗口(没打开时是 None)
    pub(crate) fn cron_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if self.page != Page::Cron {
            return None;
        }
        let line = self.dock.sys.cron_form?;
        let ins = self.dock.sys.cron_inputs.clone()?;
        let sched = ins[0].read(cx).value().split_whitespace().collect::<Vec<_>>().join(" ");
        let ok = sched.is_empty() || sysops::valid_schedule(&sched);
        let meaning = if sched.is_empty() {
            "例:*/5 * * * * 每 5 分钟;0 3 * * * 每天 03:00;@reboot 开机时".to_string()
        } else if ok {
            format!("含义:{}", sysops::describe(&sched))
        } else {
            "还不是合法的计划:要 5 段(分 时 日 月 周),或 @daily 这样的简写".to_string()
        };
        let can = !sched.is_empty() && ok && !ins[1].read(cx).value().trim().is_empty();
        let editing = line.is_some();
        let title = div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .px(px(24.0))
            .pt(px(24.0))
            .pb(px(14.0))
            .child(div().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).child(if editing { "编辑计划任务" } else { "新增计划任务" }))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("写进当前登录用户的 crontab,注释和其他任务保持不变"));
        let body = div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .px(px(24.0))
            .pb(px(20.0))
            .child(field("计划", "分 时 日 月 周,或 @daily、@reboot 这样的简写", &ins[0]))
            .child(field("命令", "用登录用户的身份执行,路径写绝对路径更稳妥", &ins[1]));
        let cancel = crate::node_form::ctl_btn("cron-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
            this.dock.sys.cron_form = None;
            cx.notify();
        }));
        let submit = div()
            .id("cron-ok")
            .flex()
            .flex_row()
            .items_center()
            .h(px(34.0))
            .px(px(16.0))
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .when(can, |d| d.bg(c::accent()).text_color(c::on_accent()).cursor_pointer().hover(|s| s.opacity(0.88)))
            .when(!can, |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()))
            .child("保存")
            .on_click(cx.listener(|this, _, _, cx| this.cron_submit(cx)));
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(24.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(c::line())
            .child(div().flex_1().min_w_0().text_size(px(t::TYPE_MICRO)).text_color(if ok { c::ghost() } else { c::warn() }).child(SharedString::from(meaning)))
            .child(cancel)
            .child(submit);
        Some(crate::ui::modal(
            "cron-dialog",
            520.0,
            cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                "escape" => {
                    this.dock.sys.cron_form = None;
                    cx.notify();
                    cx.stop_propagation();
                }
                "enter" => {
                    this.cron_submit(cx);
                    cx.stop_propagation();
                }
                _ => {}
            }),
            [title.into_any_element(), body.into_any_element(), footer.into_any_element()],
        ))
    }

    fn cron_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let info = match &self.dock.sys.cron {
            None => return reading("读取服务器上的计划任务"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了计划任务", e.clone()),
            Some(Ok(i)) => i,
        };
        let card = |id: &'static str, title: &'static str, n: usize, header: Div, rows: Vec<Stateful<Div>>, empty: &'static str| {
            let body: AnyElement = if rows.is_empty() { div().h(px(56.0)).flex().items_center().justify_center().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(empty).into_any_element() } else { div().id(id).children(rows).into_any_element() };
            div()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(div().flex().flex_row().items_baseline().gap(px(8.0)).child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child(title)).child(small(n.to_string(), c::ghost())))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .rounded(px(t::RADIUS_XL))
                        .border_1()
                        .border_color(c::dialog_border())
                        .overflow_hidden()
                        .child(header.h(px(34.0)).flex_shrink_0().flex().flex_row().items_center().gap(px(12.0)).px(px(16.0)).border_b_1().border_color(c::border()))
                        .child(body),
                )
        };
        let jobs_header = div().child(head("来源").w(px(170.0))).child(head("计划").w(px(190.0))).child(head("用户").w(px(72.0))).child(head("命令").flex_1().min_w_0()).child(div().w(px(60.0)));
        let busy = self.op_busy("crontab").is_some();
        let busy_row = self.dock.sys.cron_row;
        let job_rows: Vec<Stateful<Div>> = info
            .jobs
            .iter()
            .enumerate()
            .map(|(i, j)| {
                let mut act = div().w(px(60.0)).flex_shrink_0().flex().flex_row().justify_end().gap(px(2.0));
                if let Some(line) = j.line {
                    if busy {
                        if busy_row == Some(line) {
                            act = act.child(icon_btn_busy(("cj-del", i), IconName::Trash, c::danger(), true));
                        }
                    } else {
                        let (sc, cm) = (j.schedule.clone(), j.cmd.clone());
                        act = act.child(tip(icon_btn(("cj-edit", i), IconName::Pencil, c::secondary()), "编辑").on_click(cx.listener(move |this, _, window, cx| this.cron_open(Some(line), &sc, &cm, window, cx))));
                        let cm = j.cmd.clone();
                        act = act.child(tip(icon_btn(("cj-del", i), IconName::Trash, c::danger()), "删除").on_click(cx.listener(move |this, _, _, cx| {
                            let Some(Ok(info)) = &this.dock.sys.cron else { return };
                            this.dock.sys.cron_row = Some(line);
                            let text = sysops::crontab_replace(&info.user_text, Some(line), None);
                            let j = Job { label: "删除任务".into(), target: "crontab".into(), script: "crontab -".into(), root: false, input: Some(text), reload: Some((Kind::Cron, sysops::CRON_SCRIPT.to_string(), false)) };
                            this.ask(DockAsk { title: "删除计划任务", tile: IconName::CalendarClock, name: cm.chars().take(40).collect(), detail: "我的 crontab", body: "从 crontab 里删除这一行,之后不会再自动执行", label: "删除任务", action: Action::Sys(j), list: Vec::new() }, cx);
                        })));
                    }
                }
                row(("cj", i))
                    .child(div().w(px(170.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(j.source.clone()))
                    .child(div().w(px(190.0)).flex_shrink_0().flex().flex_col().child(div().text_size(px(t::TYPE_CONTROL)).child(sysops::describe(&j.schedule))).child(div().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::ghost()).child(j.schedule.clone())))
                    .child(cell(72.0, j.user.clone()))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).child(j.cmd.clone()))
                    .child(act)
            })
            .collect();
        let timer_header = div().child(head("定时器").w(px(280.0))).child(head("触发").flex_1().min_w_0()).child(head("下次运行").w(px(220.0)));
        let timer_rows: Vec<Stateful<Div>> = info
            .timers
            .iter()
            .enumerate()
            .map(|(i, tm)| {
                row(("tm", i))
                    .child(mono_cell(280.0, tm.unit.clone(), c::text()))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(tm.activates.clone()))
                    .child(mono_cell(220.0, if tm.next.is_empty() { "—".to_string() } else { tm.next.clone() }, c::secondary()))
            })
            .collect();
        div()
            .flex_shrink_0()
            .pb(px(14.0))
            .flex()
            .flex_col()
            .gap(px(18.0))
            .child(card("cj-list", "计划任务", info.jobs.len(), jobs_header, job_rows, "没有找到 cron 任务"))
            .child(card("tm-list", "systemd 定时器", info.timers.len(), timer_header, timer_rows, "没有 systemd 定时器"))
            .into_any_element()
    }
}

const DEMO_PROCS: &str = "\
 2210 www-data              42.5  3.4 204800       12:30 nginx: worker process\n\
  911 postgres              18.2  8.1 665544    3-04:12:09 postgres: main: checkpointer\n\
 3377 chen                  12.0  2.2 143360       05:41 node /srv/app/server.js --port 3000\n\
 1822 root                   6.3  1.0  66560    9-12:03:44 /usr/bin/dockerd -H fd://\n\
 4120 redis                  1.2  0.6  40960    9-12:03:40 redis-server *:6379\n\
    1 root                   0.0  0.1  11852   12-05:12:01 /sbin/init splash\n\
  622 root                   0.0  0.2  23552   12-05:11:58 /usr/sbin/sshd -D\n\
 5011 chen                   0.0  0.1   9216       00:03 -zsh\n";

const DEMO_SERVICES: &str = "\
  nginx.service loaded active running A high performance web server\n\
  docker.service loaded active running Docker Application Container Engine\n\
  ssh.service loaded active running OpenBSD Secure Shell server\n\
  postgresql@16-main.service loaded active running PostgreSQL Cluster 16-main\n\
  cron.service loaded active running Regular background program processing daemon\n\
● backup.service loaded failed failed Nightly backup job\n\
  ufw.service loaded active exited Uncomplicated firewall\n\
  redis-server.service loaded inactive dead Advanced key-value store\n\
@@FILES\n\
nginx.service enabled enabled\n\
docker.service enabled enabled\n\
ssh.service enabled enabled\n\
postgresql@16-main.service enabled enabled\n\
cron.service enabled enabled\n\
backup.service static -\n\
ufw.service enabled enabled\n\
redis-server.service disabled enabled\n";

const DEMO_STORAGE: &str = "\
Filesystem     Type 1024-blocks      Used Available Capacity Mounted on\n\
/dev/vda1      ext4    103081248  84651200  18430048      83% /\n\
/dev/vdb1      xfs     515396075  98304000 417092075      20% /data\n\
/dev/vdc1      ext4     20511312  19700000    811312      97% /backup\n\
@@INODES\n\
Filesystem      Inodes   IUsed    IFree IUse% Mounted on\n\
/dev/vda1      6553600  845678  5707922   13% /\n\
/dev/vdb1     51539607   12011 51527596    1% /data\n\
/dev/vdc1      1310720 1250000    60720   96% /backup\n";

const DEMO_CRON: &str = "\
@@USER\n\
MAILTO=ops@example.com\n\
*/5 * * * * /usr/local/bin/health-check.sh\n\
0 3 * * * /usr/local/bin/backup.sh --full\n\
@reboot /opt/app/start.sh\n\
@@FILE /etc/crontab\n\
17 * * * * root cd / && run-parts --report /etc/cron.hourly\n\
25 6 * * * root test -x /usr/sbin/anacron || run-parts --report /etc/cron.daily\n\
@@FILE /etc/cron.d/certbot\n\
0 */12 * * * root certbot -q renew\n\
@@TIMERS\n\
Tue 2026-11-24 10:00:00 CST 12min left Tue 2026-11-24 09:00:01 CST 47min ago apt-daily.timer apt-daily.service\n\
Wed 2026-11-25 00:00:00 CST 14h left Tue 2026-11-24 00:00:00 CST 9h ago logrotate.timer logrotate.service\n";
