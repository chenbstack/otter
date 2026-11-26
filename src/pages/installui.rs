//! 统一的应用安装抽屉:数据库、OpenResty 和商店里的其他应用共用。表单由目录里的参数定义生成(`crate::appstore`)。
//! 提交后在抽屉里显示安装进度(服务器上的后台任务,关掉 Otter 也不会中断),装完展示一次性信息(密码等)。

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Editor, EditorState, Input, InputState};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use super::super::*;
use super::composeedit::{field_col, field_col_with};
use crate::appstore::{self, App as AppDef, PType, Param, Plan, Record, Values};
use crate::web;

const POLL_EVERY: Duration = Duration::from_millis(1500);
const LOG_LINES: usize = 14;
const T_START: &str = "appjob-start";
const T_POLL: &str = "appjob-poll";
const T_CTL: &str = "appjob-ctl";
const FLAG_IDS: [&str; 8] = ["api-flag-0", "api-flag-1", "api-flag-2", "api-flag-3", "api-flag-4", "api-flag-5", "api-flag-6", "api-flag-7"];

/// 一次安装 / 升级 / 卸载任务的界面状态
pub(crate) struct AppJob {
    title: String,
    app_name: String,
    container: String,
    kind: Option<String>,
    /// 正在等「启动任务」的结果
    starting: bool,
    inflight: bool,
    offset: usize,
    log: String,
    started: Instant,
    last: Instant,
    done: Option<i32>,
    /// 装完展示的一次性信息 (名称, 值)
    secrets: Vec<(String, String)>,
    notes: String,
    url: String,
    /// 任务成功后要不要把表单收起(卸载)
    close_on_ok: bool,
}

#[derive(Default)]
pub(crate) struct AppUi {
    pub(super) open: bool,
    /// 任务页的「详细输出」是否展开
    log_open: bool,
    app: String,
    version: usize,
    ver_open: bool,
    ver_custom: Option<Entity<InputState>>,
    ver_search: Option<Entity<InputState>>,
    project: Option<Entity<InputState>>,
    container: Option<Entity<InputState>>,
    cpus: Option<Entity<InputState>>,
    memory: Option<Entity<InputState>>,
    mem_gb: bool,
    inputs: BTreeMap<String, Entity<InputState>>,
    flags: BTreeMap<String, bool>,
    picks: BTreeMap<String, usize>,
    show_pwd: bool,
    auto_container: String,
    last_project: String,
    show_compose: bool,
    /// compose 预览的编辑器;`compose_seed` 是上次按表单渲染出的文本,编辑器内容和它不同就是用户手动改过
    compose_ed: Option<Entity<EditorState>>,
    compose_seed: String,
    error: Option<String>,
    /// 各应用的版本列表(应用 id → (版本, 来源))
    tags: HashMap<String, (Vec<String>, &'static str)>,
    tag_req: Vec<String>,
    tag_srv_done: Vec<String>,
    tag_errs: HashMap<String, (u8, u8)>,
    tag_fail: Vec<String>,
    pub(super) job: Option<AppJob>,
    pub(super) st: super::storepage::StoreUi,
    /// 装完要跳到并选中的数据库实例名;容器出现在列表里后由数据库页取走
    pub(super) jump: Option<String>,
}

impl AppUi {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// 正在跑的安装/升级/卸载任务里,属于这个容器的任务标题
    pub(super) fn job_of(&self, container: &str) -> Option<&str> {
        self.job.as_ref().filter(|j| j.container == container && j.done.is_none()).map(|j| j.title.as_str())
    }

    /// 丢掉某应用缓存的版本列表,下次用到时重新取
    pub(super) fn forget_tags(&mut self, id: &str) {
        self.tags.remove(id);
        self.tag_req.retain(|x| x != id);
        self.tag_srv_done.retain(|x| x != id);
        self.tag_errs.remove(id);
    }

    pub(super) fn tags_failed(&self, id: &str) -> bool {
        self.tag_fail.iter().any(|x| x == id) && !self.tags.contains_key(id)
    }

    pub(super) fn tags_of(&self, id: &str) -> Option<Vec<String>> {
        self.tags.get(id).map(|t| t.0.clone())
    }
}

fn mk_input(ph: &str, masked: bool, window: &mut Window, cx: &mut Context<Home>) -> Entity<InputState> {
    let s = cx.new(|cx| {
        let s = InputState::new(window, cx).placeholder(ph.to_string());
        if masked { s.masked(true) } else { s }
    });
    cx.observe(&s, |_, _, cx| cx.notify()).detach();
    s
}

fn set(i: &Option<Entity<InputState>>, v: &str, window: &mut Window, cx: &mut Context<Home>) {
    if let Some(i) = i {
        i.update(cx, |s, cx| s.set_value(v, window, cx));
    }
}

impl Home {
    pub(super) fn api_app(&self) -> Option<AppDef> {
        appstore::current().apps.iter().find(|a| a.id == self.dock.api.app).cloned()
    }

    pub(super) fn api_docker(&self) -> Option<&crate::docker::Docker> {
        self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref())
    }

    pub(super) fn taken_names(&self) -> Vec<String> {
        let snap = &self.dock.snap;
        snap.containers.iter().map(|c| c.name.clone()).chain(self.node().stacks.iter().map(|s| s.name.clone())).chain(snap.containers.iter().filter_map(|c| c.project.clone())).collect()
    }

    pub(super) fn used_ports(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self.dock.snap.containers.iter().flat_map(|c| crate::dbinstall::host_ports(&c.ports)).collect();
        if let Some(st) = self.web_state_ref() {
            v.extend(st.ports.iter().map(|p| p.0));
        }
        v
    }

    /// 打开某个应用的安装抽屉
    pub(crate) fn api_open(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.api.project.is_none() {
            let (a, b, c1, c2, c3, c4) = (mk_input("如 17.5-alpine", false, window, cx), mk_input("搜索版本,如 8.4", false, window, cx), mk_input("小写字母、数字、- 和 _", false, window, cx), mk_input("小写字母、数字、- 和 _", false, window, cx), mk_input("不限制", false, window, cx), mk_input("不限制", false, window, cx));
            let u = &mut self.dock.api;
            (u.ver_custom, u.ver_search, u.project, u.container, u.cpus, u.memory) = (Some(a), Some(b), Some(c1), Some(c2), Some(c3), Some(c4));
        }
        // 上一次的任务已经结束就清掉;还在跑的保留(抽屉直接显示进度)
        if self.dock.api.job.as_ref().is_some_and(|j| j.done.is_some()) {
            self.dock.api.job = None;
        }
        self.dock.api.open = true;
        if self.dock.api.job.is_none() {
            self.api_load(id, window, cx);
        }
        cx.notify();
    }

    /// 换成某个应用:名字、端口、密码等全部按目录重新生成
    fn api_load(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(app) = appstore::current().apps.iter().find(|a| a.id == id).cloned() else { return };
        let v = appstore::default_values(&app, &self.taken_names(), &self.used_ports());
        let mut inputs = BTreeMap::new();
        let (mut flags, mut picks) = (BTreeMap::new(), BTreeMap::new());
        for p in &app.params {
            let val = v.params.get(&p.id).cloned().unwrap_or_default();
            match p.ty {
                PType::Bool => {
                    flags.insert(p.id.clone(), val == "true");
                }
                PType::Select => {
                    picks.insert(p.id.clone(), p.options.iter().position(|o| o.value == val).unwrap_or(0));
                }
                _ => {
                    let e = mk_input(&p.label, p.ty == PType::Password, window, cx);
                    e.update(cx, |s, cx| s.set_value(&val, window, cx));
                    inputs.insert(p.id.clone(), e);
                }
            }
        }
        let u = &mut self.dock.api;
        u.app = app.id.clone();
        (u.inputs, u.flags, u.picks) = (inputs, flags, picks);
        u.show_compose = false;
        u.compose_ed = None;
        u.compose_seed.clear();
        u.show_pwd = false;
        u.error = None;
        u.ver_open = false;
        u.mem_gb = true;
        u.auto_container = v.container.clone();
        u.last_project = v.project.clone();
        u.version = 0;
        let (pj, ct, cp, me, vc) = (u.project.clone(), u.container.clone(), u.cpus.clone(), u.memory.clone(), u.ver_custom.clone());
        set(&pj, &v.project, window, cx);
        set(&ct, &v.container, window, cx);
        set(&cp, "", window, cx);
        set(&me, "", window, cx);
        set(&vc, "", window, cx);
        if let Some(t) = self.dock.api.tags.get(&app.id) {
            self.dock.api.version = appstore::default_index(&app, &t.0);
        }
        self.api_fetch_tags(&app, cx);
    }

    /// 截图用:OTTER_SHOT_JOB=run|ok|fail 直接显示任务页
    pub(super) fn api_demo_job(&mut self, mode: &str) {
        let now = Instant::now();
        let (log, done): (&str, Option<i32>) = match mode {
            "run" => ("==> 检查环境\n==> 拉取镜像\n Image openresty/openresty:1.31.9.1-alpine Pulling\n", None),
            "fail" => ("==> 检查环境\n==> 拉取镜像\nError response from daemon: pull access denied for minio/minio, repository does not exist\n", Some(1)),
            _ => ("==> 检查环境\n==> 拉取镜像\n Image openresty/openresty:1.31.9.1-alpine Pulled\n==> 启动容器\n Container otter-openresty Started\n==> OpenResty 已启动\n", Some(0)),
        };
        self.dock.api.app = "openresty".into();
        self.dock.api.open = true;
        self.dock.api.job = Some(AppJob { title: "安装 OpenResty".into(), app_name: "OpenResty".into(), container: "otter-openresty".into(), kind: Some("website".into()), starting: false, inflight: false, offset: 0, log: log.into(), started: now, last: now, done, secrets: vec![], notes: String::new(), url: "http://192.168.64.5:80".into(), close_on_ok: false });
    }

    pub(super) fn api_close(&mut self, cx: &mut Context<Self>) {
        let u = &mut self.dock.api;
        u.open = false;
        u.log_open = false;
        if u.job.as_ref().is_some_and(|j| j.done.is_some()) {
            u.job = None;
        }
        cx.notify();
    }

    fn api_versions(&self) -> Vec<String> {
        self.dock.api.tags.get(&self.dock.api.app).map(|t| t.0.clone()).unwrap_or_default()
    }

    /// 取版本列表:服务器和本机同时请求 Docker Hub,谁先拿到就用谁;静态列表直接用
    pub(super) fn api_fetch_tags(&mut self, app: &AppDef, cx: &mut Context<Self>) {
        let id = app.id.clone();
        if self.dock.api.tags.contains_key(&id) || self.dock.api.tag_req.contains(&id) {
            return;
        }
        // 统一入口:24 小时内更新过的本地版本列表直接用,不请求 Hub;过期了才往下走,用 Hub 补最新的
        if let Some(l) = appstore::fresh_versions(app) {
            if self.dock.api.app == id {
                self.dock.api.version = appstore::default_index(app, &l);
            }
            self.dock.api.tags.insert(id, (l, "本地"));
            return;
        }
        let Some((repo, search, mut pages)) = appstore::hub_pages(app) else {
            if let Some(l) = appstore::select_versions(app, &[]) {
                self.dock.api.version = appstore::default_index(app, &l);
                self.dock.api.tags.insert(id, (l, "目录"));
            }
            return;
        };
        // 目录里有版本列表时,Hub 只补比列表更新的:按更新时间排序,取第一页就够
        if appstore::fallback_versions(app).is_some() {
            pages = 1;
        }
        self.dock.api.tag_req.push(id.clone());
        let key = format!("apptags-{id}");
        let routes = match if self.demo { None } else { self.api_docker() } {
            Some(d) => {
                d.shell_run(&key, &appstore::hub_script(&repo, search.as_deref(), pages));
                2
            }
            None => 1,
        };
        self.dock.api.tag_errs.insert(id.clone(), (0, routes));
        let app = app.clone();
        cx.spawn(async move |this, cx| {
            let r = cx.background_executor().spawn(async move { appstore::fetch_hub_local(&repo, search.as_deref(), pages) }).await;
            let _ = this.update(cx, |h, cx| match r.ok().and_then(|raw| appstore::select_versions(&app, &appstore::with_catalog_names(&app, appstore::parse_tag_names(&raw)))) {
                Some(l) => h.api_tags_arrived(&app, l, "本机", cx),
                None => h.api_tag_error(&app.id, cx),
            });
        })
        .detach();
    }

    fn api_tags_arrived(&mut self, app: &AppDef, list: Vec<String>, from: &'static str, cx: &mut Context<Self>) {
        let u = &mut self.dock.api;
        if !u.tags.contains_key(&app.id) {
            appstore::save_versions(&app.id, &list);
            if u.app == app.id {
                u.version = appstore::default_index(app, &list);
            }
            u.tags.insert(app.id.clone(), (list, from));
            cx.notify();
        }
    }

    fn api_tag_error(&mut self, id: &str, cx: &mut Context<Self>) {
        let u = &mut self.dock.api;
        let e = u.tag_errs.entry(id.to_string()).or_insert((0, 1));
        e.0 += 1;
        if e.0 >= e.1 && !u.tags.contains_key(id) {
            // 在线都没取到:有目录里的备份就用备份,没有再报失败
            let fb = appstore::current().apps.iter().find(|a| a.id == id).and_then(|a| appstore::fallback_versions(a).and_then(|_| appstore::select_versions(a, &appstore::with_catalog_names(a, Vec::new()))).map(|l| (appstore::default_index(a, &l), l)));
            match fb {
                Some((idx, l)) => {
                    if u.app == id {
                        u.version = idx;
                    }
                    u.tags.insert(id.to_string(), (l, "内置备份"));
                }
                None => u.tag_fail.push(id.to_string()),
            }
        }
        cx.notify();
    }

    fn api_poll_tags(&mut self, cx: &mut Context<Self>) {
        let Some(app) = self.api_app() else { return };
        let id = app.id.clone();
        let u = &self.dock.api;
        if !u.tag_req.contains(&id) || u.tags.contains_key(&id) || u.tag_srv_done.contains(&id) || u.tag_errs.get(&id).is_none_or(|e| e.1 != 2) {
            return;
        }
        let Some(r) = self.api_docker().and_then(|d| d.db_take(&format!("apptags-{id}"))) else { return };
        self.dock.api.tag_srv_done.push(id.clone());
        match r.ok().and_then(|raw| appstore::select_versions(&app, &appstore::with_catalog_names(&app, appstore::parse_tag_names(&raw)))) {
            Some(l) => self.api_tags_arrived(&app, l, "服务器", cx),
            None => self.api_tag_error(&id, cx),
        }
    }

    /// 每次渲染前:堆栈名变了,没手改过的容器名跟着变;收版本列表;推进后台任务
    pub(crate) fn api_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.api_pump(cx);
        self.store_sync(window, cx);
        if !self.dock.api.open || self.dock.api.job.is_some() {
            return;
        }
        self.api_poll_tags(cx);
        let u = &self.dock.api;
        let (Some(a), Some(b)) = (u.project.clone(), u.container.clone()) else { return };
        let project = a.read(cx).value().trim().to_string();
        if project != u.last_project {
            let old = u.auto_container.clone();
            let u = &mut self.dock.api;
            u.last_project = project.clone();
            if b.read(cx).value() == old {
                u.auto_container = project.clone();
                b.update(cx, |s, cx| s.set_value(&project, window, cx));
            }
        }
    }

    /// 读表单。版本还没取到又没填时返回空版本,由校验报错
    fn api_form(&self, app: &AppDef, cx: &App) -> Values {
        let u = &self.dock.api;
        let v = |i: &Option<Entity<InputState>>| i.as_ref().map(|i| i.read(cx).value().trim().to_string()).unwrap_or_default();
        let vers = self.api_versions();
        let version = if matches!(app.versions, appstore::Versions::None { .. }) {
            vers.first().cloned().unwrap_or_else(|| appstore::fallback_version(app))
        } else {
            vers.get(u.version).cloned().unwrap_or_else(|| v(&u.ver_custom))
        };
        let mem = v(&u.memory);
        let memory = if mem.is_empty() { mem } else { format!("{mem}{}", if u.mem_gb { "g" } else { "m" }) };
        let mut params = BTreeMap::new();
        for p in &app.params {
            let val = match p.ty {
                PType::Bool => u.flags.get(&p.id).copied().unwrap_or(false).to_string(),
                PType::Select => p.options.get(u.picks.get(&p.id).copied().unwrap_or(0)).map(|o| o.value.clone()).unwrap_or_default(),
                _ => u.inputs.get(&p.id).map(|i| i.read(cx).value().trim().to_string()).unwrap_or_default(),
            };
            params.insert(p.id.clone(), val);
        }
        Values { version, project: v(&u.project), container: v(&u.container), cpus: v(&u.cpus), memory, params }
    }

    fn api_submit(&mut self, cx: &mut Context<Self>) {
        self.api_submit_with(false, cx);
    }

    /// `purge`:清掉上次安装留下的目录和数据卷再装
    fn api_submit_with(&mut self, purge: bool, cx: &mut Context<Self>) {
        let Some(app) = self.api_app() else { return };
        if self.dock.api.job.is_some() {
            return;
        }
        let id = app.id.clone();
        if !self.dock.api.tags.contains_key(&id) && !self.dock.api.tag_fail.contains(&id) {
            self.dock.api.error = Some("版本列表还在获取,请稍候".into());
            cx.notify();
            return;
        }
        let v = self.api_form(&app, cx);
        let mut plan = match appstore::render(&app, &v) {
            Ok(p) => p,
            Err(e) => {
                self.dock.api.error = Some(e);
                cx.notify();
                return;
            }
        };
        // 用户在预览里手动改过 compose,就按改后的装
        if let Some(ed) = self.dock.api.compose_ed.as_ref() {
            let text = ed.read(cx).value().to_string();
            if text != self.dock.api.compose_seed {
                if !text.contains("services:") {
                    self.dock.api.error = Some("compose 文件里要有 services:".into());
                    cx.notify();
                    return;
                }
                plan.compose = if text.ends_with('\n') { text } else { text + "\n" };
            }
        }
        let taken = self.taken_names();
        if taken.contains(&v.project) {
            self.dock.api.error = Some("已经有同名的堆栈".into());
        } else if taken.contains(&v.container) {
            self.dock.api.error = Some("已经有同名的容器".into());
        } else if let Some(p) = plan.ports.iter().find(|p| self.used_ports().contains(p)) {
            self.dock.api.error = Some(format!("服务器上 {p} 端口已被占用,换一个"));
        } else {
            self.dock.api.error = None;
        }
        if self.dock.api.error.is_some() {
            cx.notify();
            return;
        }
        let rec = Record::new(&plan, &v);
        let script = appstore::install_script(&app, &plan, &rec, purge);
        let secrets = self.api_secrets(&app, &v);
        self.api_job_start(&app, &plan, format!("安装 {}", app.name), script, secrets, false, cx);
    }

    /// 装完要一次性展示的参数 (名称, 值)
    fn api_secrets(&self, app: &AppDef, v: &Values) -> Vec<(String, String)> {
        let show = app.post.as_ref().map(|p| p.show.clone()).unwrap_or_default();
        app.params.iter().filter(|p| show.contains(&p.id)).map(|p| (p.label.clone(), v.params.get(&p.id).cloned().unwrap_or_default())).collect()
    }

    fn api_job_start(&mut self, app: &AppDef, plan: &Plan, title: String, script: String, secrets: Vec<(String, String)>, close_on_ok: bool, cx: &mut Context<Self>) {
        let post = app.post.clone().unwrap_or_default();
        let url = post.url.replace("<服务器>", &self.api_host()).replace("${port}", plan.public_values.get("port").map(String::as_str).unwrap_or("")).replace("${console_port}", plan.public_values.get("console_port").map(String::as_str).unwrap_or(""));
        self.api_job_raw(app, &plan.container, url, title, script, secrets, close_on_ok, cx);
    }

    /// 安装、升级、卸载共用:在服务器上起后台任务,抽屉切到进度视图
    pub(super) fn api_job_raw(&mut self, app: &AppDef, container: &str, url: String, title: String, script: String, secrets: Vec<(String, String)>, close_on_ok: bool, cx: &mut Context<Self>) {
        let post = app.post.clone().unwrap_or_default();
        self.dock.api.open = true;
        let now = Instant::now();
        let demo = self.demo;
        self.dock.api.job = Some(AppJob {
            title,
            app_name: app.name.clone(),
            container: container.to_string(),
            kind: app.kind.clone(),
            starting: !demo,
            inflight: false,
            offset: 0,
            log: if demo { "==> 检查\n==> 拉取镜像\n==> 启动容器\n==> 已启动\n".into() } else { String::new() },
            started: now,
            last: now - POLL_EVERY,
            done: demo.then_some(0),
            secrets,
            notes: post.notes,
            url,
            close_on_ok,
        });
        if !demo {
            if let Some(d) = self.api_docker() {
                d.host_run(T_START, appstore::job_start_script(&script), 60);
            }
        }
        cx.notify();
    }

    /// 服务器地址(给「访问地址」用);取不到就留占位
    pub(super) fn api_host(&self) -> String {
        let uh = &self.node().user_host;
        let h = uh.rsplit('@').next().unwrap_or(uh).split(':').next().unwrap_or("");
        if h.is_empty() { "服务器地址".into() } else { h.to_string() }
    }

    /// 页面前台时要不要重绘:后台任务到了该轮询的时间
    pub(crate) fn api_due(&self) -> bool {
        !self.demo && self.dock.api.job.as_ref().is_some_and(|j| j.done.is_none() && (j.starting || j.last.elapsed() >= POLL_EVERY) && !j.inflight)
    }

    /// 推进后台任务:收启动结果、收轮询结果、到时间发下一次轮询
    fn api_pump(&mut self, cx: &mut Context<Self>) {
        if self.demo || self.dock.api.job.as_ref().is_none_or(|j| j.done.is_some()) {
            return;
        }
        let Some(d) = self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref()) else { return };
        let Some(j) = self.dock.api.job.as_mut() else { return };
        let mut finished = false;
        if j.starting {
            match d.db_take(T_START) {
                Some(Ok(out)) if out.contains("ERR:") => {
                    j.log = out.lines().find_map(|l| l.strip_prefix("ERR:")).unwrap_or("启动失败").to_string();
                    j.done = Some(1);
                    j.starting = false;
                }
                Some(Ok(_)) => {
                    j.starting = false;
                    j.last = Instant::now() - POLL_EVERY;
                }
                Some(Err(e)) => {
                    j.log = e;
                    j.done = Some(1);
                    j.starting = false;
                }
                None => {}
            }
        } else {
            if let Some(r) = d.db_take(T_POLL) {
                j.inflight = false;
                j.last = Instant::now();
                if let Ok(raw) = r {
                    let p = web::parse_job_poll(&raw);
                    j.offset += p.log.len();
                    j.log.push_str(&p.log);
                    if let Some(code) = p.status {
                        j.done = Some(code);
                    } else if !p.alive && j.started.elapsed() > Duration::from_secs(10) {
                        j.done = Some(-1);
                    }
                }
                finished = j.done.is_some();
            }
            if j.done.is_none() && !j.inflight && j.last.elapsed() >= POLL_EVERY {
                j.inflight = true;
                d.host_run(T_POLL, appstore::job_poll_script(j.offset), 30);
            }
        }
        if finished || self.dock.api.job.as_ref().is_some_and(|j| j.done.is_some()) {
            self.api_finished(cx);
        }
        cx.notify();
    }

    /// 任务结束:刷新容器列表;数据库装好后让数据库页跳到它;清掉服务器上的任务目录
    fn api_finished(&mut self, _cx: &mut Context<Self>) {
        let Some(j) = self.dock.api.job.as_ref() else { return };
        let ok = j.done == Some(0);
        let (kind, name) = (j.kind.clone(), j.container.clone());
        // 装/升/卸之后旧的升级检查结果作废,回到商店页重新检查
        self.store_forget_check(&name);
        if let Some(d) = self.api_docker() {
            d.refresh();
            d.host_run(T_CTL, appstore::job_clear_script(), 30);
        }
        if ok {
            if kind.as_deref() == Some("database") {
                self.dock.api.jump = Some(name);
            }
            if kind.as_deref() == Some("website") {
                self.dock.sys.asked.remove(&crate::sysops::Kind::Web);
            }
        }
    }

    fn api_cancel(&mut self) {
        if let Some(d) = self.api_docker() {
            d.host_run(T_CTL, appstore::job_cancel_script(), 30);
        }
    }

    pub(crate) fn api_overlays(&mut self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v: Vec<AnyElement> = self.api_drawer(window, cx).map(|d| d.into_any_element()).into_iter().collect();
        v.extend(self.store_dialogs(cx));
        if self.page == Page::Database {
            v.extend(self.dbm_overlays(window, cx));
        }
        v
    }

    // ---------------------------------------------------------------- 界面

    /// 渲染前准备 compose 编辑器(需要窗口):展开时建好,表单变了且用户没改过就跟着重新渲染
    pub(crate) fn api_prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dock.api.open || !self.dock.api.show_compose {
            return;
        }
        let Some(app) = self.api_app() else { return };
        let Ok(plan) = appstore::render(&app, &self.api_form(&app, cx)) else { return };
        let text = plan.compose;
        match self.dock.api.compose_ed.clone() {
            None => {
                let ed = cx.new(|cx| EditorState::new(window, cx).language("yaml").line_number(true));
                ed.update(cx, |s, cx| s.set_value(text.as_str(), window, cx));
                cx.observe(&ed, |_, _, cx| cx.notify()).detach();
                self.dock.api.compose_ed = Some(ed);
                self.dock.api.compose_seed = text;
                cx.notify();
            }
            Some(ed) => {
                if ed.read(cx).value() == self.dock.api.compose_seed.as_str() && text != self.dock.api.compose_seed {
                    ed.update(cx, |s, cx| s.set_value(text.as_str(), window, cx));
                    self.dock.api.compose_seed = text;
                    cx.notify();
                }
            }
        }
    }

    fn api_drawer(&self, window: &Window, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let u = &self.dock.api;
        if !u.open {
            return None;
        }
        let app = self.api_app()?;
        let w = (f32::from(window.viewport_size().width) * 0.5).clamp(480.0, 720.0);
        let ic = match app.icon.as_str() {
            "database" => IconName::Database,
            "globe" => IconName::Globe,
            "activity" => IconName::Activity,
            "hard-drive" => IconName::HardDrive,
            "notebook-pen" => IconName::NotebookPen,
            _ => IconName::Package,
        };
        let title = u.job.as_ref().map(|j| j.title.clone()).unwrap_or_else(|| format!("安装 {}", app.name));
        let head = div()
            .h(px(52.0))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(16.0))
            .border_b_1()
            .border_color(c::border())
            .child(icon(ic, 16.0, c::secondary()))
            .child(div().flex_1().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(SharedString::from(title)))
            .child(super::syspages::tip(super::dockerpages::icon_btn(("api-x", 0), IconName::X, c::muted()).on_click(cx.listener(|this, _, _, cx| this.api_close(cx))), "关闭"));
        let (body, foot) = if u.job.is_some() { (self.api_job_body(cx), self.api_job_foot(cx)) } else { (self.api_form_body(&app, cx), self.api_form_foot(cx)) };
        Some(crate::ui::drawer("api-drawer", w, cx.listener(|this, _, _, cx| this.api_close(cx)), [head.into_any_element(), body.into_any_element(), foot.into_any_element()]).on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
            if e.keystroke.key == "escape" {
                this.api_close(cx);
                cx.stop_propagation();
            }
        })))
    }

    fn api_form_foot(&self, cx: &mut Context<Self>) -> Div {
        let u = &self.dock.api;
        let ok = div()
            .id("api-ok")
            .h(px(34.0))
            .px(px(18.0))
            .flex()
            .items_center()
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .bg(c::accent())
            .text_color(c::on_accent())
            .cursor_pointer()
            .hover(|s| s.opacity(0.88))
            .child("安装")
            .on_click(cx.listener(|this, _, _, cx| this.api_submit(cx)));
        div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(16.0))
            .py(px(12.0))
            .border_t_1()
            .border_color(c::border())
            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CAPTION)).text_color(if u.error.is_some() { c::danger() } else { c::ghost() }).child(SharedString::from(u.error.clone().unwrap_or_else(|| "镜像要先拉取,第一次安装可能要几分钟".into()))))
            .child(ok)
    }

    fn api_job_foot(&self, cx: &mut Context<Self>) -> Div {
        let Some(j) = self.dock.api.job.as_ref() else { return div() };
        let mut right = div().flex().flex_row().gap(px(8.0));
        match j.done {
            None => {
                right = right.child(crate::node_form::ctl_btn("api-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
                    this.api_cancel();
                    cx.notify();
                })));
            }
            Some(0) => {
                right = right.child(crate::node_form::ctl_btn("api-done", None, "完成", 34.0).on_click(cx.listener(|this, _, _, cx| this.api_close(cx))));
            }
            Some(_) => {
                right = right
                    .child(crate::node_form::ctl_btn("api-back", None, "返回修改", 34.0).on_click(cx.listener(|this, _, _, cx| {
                        this.dock.api.job = None;
                        cx.notify();
                    })))
                    .child(crate::node_form::ctl_btn("api-close", None, "关闭", 34.0).on_click(cx.listener(|this, _, _, cx| this.api_close(cx))));
            }
        }
        let hint = match j.done {
            None => "任务在服务器上独立运行,关掉 Otter 也不会中断".to_string(),
            _ => String::new(),
        };
        div().flex_shrink_0().flex().flex_row().items_center().gap(px(10.0)).px(px(16.0)).py(px(12.0)).border_t_1().border_color(c::border()).child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(hint))).child(right)
    }

    fn api_job_body(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let Some(j) = self.dock.api.job.as_ref() else { return div().id("api-job-body") };
        let running = j.done.is_none();
        let failed = j.done.is_some_and(|c| c != 0);
        let (ic, color) = match j.done {
            None => (IconName::Clock, c::accent()),
            Some(0) => (IconName::Check, c::online()),
            Some(_) => (IconName::TriangleAlert, c::danger()),
        };
        let steps: Vec<String> = j.log.lines().filter(|l| l.starts_with("==>")).map(|l| l.trim_start_matches("==>").trim().to_string()).collect();
        let (headline, sub) = match j.done {
            None => (j.title.clone(), format!("{} · 已运行 {} 秒", steps.last().cloned().unwrap_or_else(|| "正在启动…".into()), j.started.elapsed().as_secs())),
            Some(0) => (format!("{} 完成", j.title), "已在服务器上启动".to_string()),
            Some(-1) => (format!("{} 中断", j.title), "任务被中断(服务器重启或被结束)".into()),
            Some(c) => (format!("{} 失败", j.title), format!("退出码 {c}{}", web::explain(&j.log).map(|e| format!(":{e}")).unwrap_or_default())),
        };
        let hero = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.0))
            // 进行中不放图标(转圈放在当前步骤前面);结束后才显示结果图标
            .children((!running).then(|| div().size(px(44.0)).flex_shrink_0().flex().items_center().justify_center().rounded_full().bg(color.opacity(0.14)).child(icon(ic, 20.0, color))))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_size(px(t::TYPE_TITLE)).font_weight(FontWeight::BOLD).text_color(if failed { c::danger() } else { c::text() }).child(SharedString::from(headline)))
                    .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(sub))),
            );
        let mut col = div().id("api-job-body").flex_1().min_h_0().overflow_y_scroll().flex().flex_col().gap(px(18.0)).p(px(20.0)).child(hero);

        // 进行中和失败时列出步骤:做完的打勾,当前的转圈,失败的标红
        if !steps.is_empty() && j.done != Some(0) {
            let n = steps.len();
            col = col.child(div().flex_shrink_0().flex().flex_col().gap(px(6.0)).children(steps.into_iter().enumerate().map(|(i, st)| {
                let last = i + 1 == n;
                let (sc, sic) = if last && running {
                    (c::accent(), IconName::Clock)
                } else if last && failed {
                    (c::danger(), IconName::TriangleAlert)
                } else {
                    (c::online(), IconName::Check)
                };
                div()
                    .h(px(22.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().size(px(13.0)).flex_shrink_0().flex().items_center().justify_center().child(if last && running { crate::ui::dot_spinner(("api-step-spin", 0), sc).into_any_element() } else { icon(sic, 13.0, sc).into_any_element() }))
                    .child(div().text_size(px(t::TYPE_CONTROL)).text_color(if last && running { c::text() } else { c::muted() }).child(SharedString::from(st)))
            })));
        }

        // 上次安装留下了目录和数据卷:让用户选择清理后重装
        if failed && j.log.contains("留着上次的数据卷") {
            col = col.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .p(px(14.0))
                    .rounded(px(t::RADIUS_MD))
                    .border_1()
                    .border_color(c::field_border())
                    .child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::MEDIUM).child("上次安装留下了残留"))
                    .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child("服务器上没有运行的容器,但留着上次的堆栈目录和数据卷。里面如果没有要保留的数据,可以清理后重新安装;清理会删除这个目录和数据卷,不能恢复。"))
                    .child(
                        div()
                            .id("api-purge")
                            .flex_shrink_0()
                            .h(px(34.0))
                            .px(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .self_start()
                            .rounded(px(t::RADIUS_MD))
                            .bg(c::danger())
                            .text_color(c::on_accent())
                            .text_size(px(t::TYPE_CONTROL))
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.88))
                            .child("清理残留并重新安装")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.dock.api.job = None;
                                this.api_submit_with(true, cx);
                            })),
                    ),
            );
        }

        if j.done == Some(0) {
            let row = |k: String, v: String, id: usize, first: bool, cx: &mut Context<Self>| {
                let vc = v.clone();
                div()
                    .h(px(40.0))
                    .px(px(14.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .when(!first, |d| d.border_t_1().border_color(c::border()))
                    .child(div().w(px(72.0)).flex_shrink_0().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(k)))
                    .child(div().flex_1().min_w_0().truncate().font_family(MONO).text_size(px(t::TYPE_CONTROL)).child(SharedString::from(v)))
                    .child(super::syspages::tip(super::dockerpages::icon_btn(("api-copy", id), IconName::Copy, c::muted()).on_click(cx.listener(move |_, _, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(vc.clone())))), "复制"))
            };
            let mut card = div().flex_shrink_0().flex().flex_col().rounded(px(t::RADIUS_XL)).border_1().border_color(c::border()).overflow_hidden();
            card = card.child(row("容器".into(), j.container.clone(), 0, true, cx));
            if !j.url.is_empty() {
                card = card.child(row("访问地址".into(), j.url.clone(), 1, false, cx));
            }
            for (i, (k, v)) in j.secrets.iter().enumerate() {
                card = card.child(row(k.clone(), v.clone(), 2 + i, false, cx));
            }
            col = col.child(card);
            if !j.secrets.is_empty() {
                col = col.child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .p(px(12.0))
                        .rounded(px(t::RADIUS_MD))
                        .bg(c::warn_soft())
                        .child(icon(IconName::TriangleAlert, 14.0, c::warn()))
                        .child(div().flex_1().min_w_0().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child("密码只在这里显示这一次,Otter 不会保存。请现在复制;之后可以在服务器上该堆栈目录的 .env 里找到。")),
                );
            }
            if !j.notes.is_empty() && !(!j.secrets.is_empty() && j.notes.contains("只显示这一次")) {
                col = col.child(div().flex_shrink_0().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(SharedString::from(j.notes.clone())));
            }
            // 下一步
            let next = match j.kind.as_deref() {
                Some("website") => Some((Page::Websites, "去「网站」页添加站点")),
                Some("database") => Some((Page::Database, "去「数据库」页管理")),
                _ => None,
            };
            if let Some((pg, text)) = next {
                col = col.child(
                    div().flex_shrink_0().flex().flex_row().child(
                        div()
                            .id("api-next")
                            .h(px(30.0))
                            .px(px(12.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .rounded(px(t::RADIUS_MD))
                            .border_1()
                            .border_color(c::field_border())
                            .bg(c::raised())
                            .text_size(px(t::TYPE_CONTROL))
                            .text_color(c::secondary())
                            .cursor_pointer()
                            .hover(|s| s.bg(c::raised_hi()))
                            .child(text)
                            .child(icon(IconName::ChevronRight, 13.0, c::muted()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.page = pg;
                                this.api_close(cx);
                            })),
                    ),
                );
            }
        }

        // 详细输出:成功后默认收起,失败时默认展开
        let lines: Vec<&str> = j.log.lines().collect();
        if !lines.is_empty() {
            let open = self.dock.api.log_open || failed;
            let tail = lines[lines.len().saturating_sub(LOG_LINES)..].join("\n");
            col = col.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id("api-log-toggle")
                            .h(px(24.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .cursor_pointer()
                            .text_size(px(t::TYPE_CAPTION))
                            .text_color(c::ghost())
                            .hover(|s| s.text_color(c::secondary()))
                            .child(icon(if open { IconName::ChevronDown } else { IconName::ChevronRight }, 13.0, c::ghost()))
                            .child(SharedString::from(format!("详细输出({} 行)", lines.len())))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.dock.api.log_open = !this.dock.api.log_open;
                                cx.notify();
                            })),
                    )
                    .when(open, |d| d.child(div().rounded(px(t::RADIUS_MD)).bg(c::field()).border_1().border_color(c::field_border()).p(px(10.0)).font_family(MONO).text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(SharedString::from(tail)))),
            );
        }
        col
    }

    fn api_form_body(&self, app: &AppDef, cx: &mut Context<Self>) -> Stateful<Div> {
        let u = &self.dock.api;
        let input = |i: Option<Entity<InputState>>| i.map(|i| div().flex_1().min_w_0().child(Input::new(&i).appearance(false).with_size(px(12.0 / 0.875))).into_any_element()).unwrap_or_else(|| div().into_any_element());
        let label = |s: &'static str| div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(s);
        let row2 = |a: Div, b: Div| div().flex_shrink_0().flex().flex_row().gap(px(12.0)).child(a).child(b);
        let full = |d: Div| div().flex_shrink_0().flex().flex_row().child(d);
        let half = |l: &str, inner: AnyElement, mono: bool| field_col(SharedString::from(l.to_string()), None, inner, mono, false);

        // 版本
        let none_ver = matches!(app.versions, appstore::Versions::None { .. });
        let vers = self.api_versions();
        let failed_all = u.tag_fail.contains(&app.id) && !u.tags.contains_key(&app.id);
        let custom = (u.version >= vers.len() && !vers.is_empty()) || (vers.is_empty() && failed_all);
        let loading = !u.tags.contains_key(&app.id) && !failed_all;
        let mut items: Vec<crate::ui::MenuItem> = if loading { vec![crate::ui::MenuItem::new("正在获取版本列表…")] } else { vers.iter().map(|v| crate::ui::MenuItem::new(v.clone())).collect() };
        if !loading {
            items.push(crate::ui::MenuItem::new("自定义版本号…"));
        }
        let (t1, t2, t3) = (cx.entity(), cx.entity(), cx.entity());
        let search = u.ver_search.clone().map(|i| {
            let q = i.read(cx).value().to_string();
            (i, q)
        });
        let ver_toggle = move |window: &mut Window, cx: &mut App| {
            t1.update(cx, |h, cx| {
                let id = h.dock.api.app.clone();
                if h.dock.api.tags.contains_key(&id) || h.dock.api.tag_fail.contains(&id) {
                    h.dock.api.ver_open = !h.dock.api.ver_open;
                    if h.dock.api.ver_open {
                        if let Some(s) = h.dock.api.ver_search.clone() {
                            s.update(cx, |s, cx| s.set_value("", window, cx));
                            let f = s.focus_handle(cx);
                            window.focus(&f, cx);
                        }
                    }
                    cx.notify();
                }
            })
        };
        let ver_pick = move |i: usize, window: &mut Window, cx: &mut App| {
            t2.update(cx, |h, cx| {
                h.dock.api.version = i;
                h.dock.api.ver_open = false;
                if i >= h.api_versions().len() {
                    if let Some(vc) = h.dock.api.ver_custom.clone() {
                        let f = vc.focus_handle(cx);
                        window.focus(&f, cx);
                    }
                }
                cx.notify();
            })
        };
        let ver_dismiss = move |_: &mut Window, cx: &mut App| {
            t3.update(cx, |h, cx| {
                h.dock.api.ver_open = false;
                cx.notify();
            })
        };
        let spin = loading.then(|| crate::ui::dot_spinner(("api-ver-load", 0), c::muted()).into_any_element());
        let (cur, open) = (if loading { 0 } else { u.version.min(vers.len()) }, u.ver_open && !loading);
        let select = match search {
            Some(sq) => crate::ui::select_search(34.0, "api-ver", IconName::Tag, items, cur, open, sq, ver_toggle, ver_pick, ver_dismiss, spin),
            None => crate::ui::select_sized(34.0, "api-ver", IconName::Tag, items, cur, open, move |w, cx| ver_toggle(w, cx), ver_pick, ver_dismiss, spin),
        };
        let ver_col = div().flex_1().min_w_0().flex().flex_col().gap(px(6.0)).child(label("版本")).child(div().h(px(34.0)).child(select));
        let src_note = match u.tags.get(&app.id) {
            Some(t) => format!("版本列表来自{}", match t.1 {
                "目录" => "应用目录".to_string(),
                "本地" => "本地(24 小时内更新过)".to_string(),
                f => format!("{f}的 Docker Hub"),
            }),
            None if loading => "服务器和本机都会尝试访问 Docker Hub".to_string(),
            None => "服务器和本机都没能访问 Docker Hub,请手填版本号".to_string(),
        };
        let failed = !loading && !u.tags.contains_key(&app.id);
        let ver_hint = if loading { "" } else if custom { "填写镜像的 tag" } else { "" };

        // 内存单位
        let gb = u.mem_gb;
        let seg = |id: &'static str, text: &'static str, on: bool, to_gb: bool| {
            div()
                .id(id)
                .h(px(22.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .rounded(px(t::RADIUS_XS))
                .cursor_pointer()
                .text_size(px(t::TYPE_CAPTION))
                .font_family(MONO)
                .when(on, |d| d.bg(c::selected()).text_color(c::text()))
                .when(!on, |d| d.text_color(c::ghost()).hover(|s| s.text_color(c::secondary())))
                .child(text)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.dock.api.mem_gb = to_gb;
                    cx.notify();
                }))
        };
        let mem_unit = div().flex().flex_row().gap(px(2.0)).child(seg("api-mem-mb", "MB", !gb, false)).child(seg("api-mem-gb", "GB", gb, true)).into_any_element();
        let core_unit = div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("核").into_any_element();

        let mut body = div().id("api-body").flex_1().min_h_0().overflow_y_scroll().flex().flex_col().gap(px(16.0)).p(px(16.0));
        if !app.summary.is_empty() {
            body = body.child(div().flex_shrink_0().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(SharedString::from(app.summary.clone())));
        }
        if !none_ver {
            body = body.child(full(div().flex_1().min_w_0().flex().flex_row().child(ver_col))).child(
                div()
                    .flex_shrink_0()
                    .mt(px(-8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(t::TYPE_CAPTION))
                    .text_color(c::ghost())
                    .child(SharedString::from(if ver_hint.is_empty() { src_note.clone() } else { format!("{ver_hint}  ·  {src_note}") }))
                    .when(failed, |d| {
                        d.child(div().id("api-ver-retry").cursor_pointer().text_color(c::accent()).hover(|s| s.opacity(0.8)).child("重试").on_click(cx.listener(|this, _, _, cx| {
                            let id = this.dock.api.app.clone();
                            let u = &mut this.dock.api;
                            u.tag_req.retain(|x| *x != id);
                            u.tag_srv_done.retain(|x| *x != id);
                            u.tag_errs.remove(&id);
                            u.tag_fail.retain(|x| *x != id);
                            if let Some(app) = this.api_app() {
                                this.api_fetch_tags(&app, cx);
                            }
                            cx.notify();
                        })))
                    }),
            );
            if custom {
                body = body.child(full(half("自定义版本号", input(u.ver_custom.clone()), true)));
            }
        }
        // 应用参数:小的两两并排,密码、开关、选择各占一行
        let mut pending: Option<Div> = None;
        let mut helps: Vec<String> = Vec::new();
        let help_row = |h: String| div().flex_shrink_0().mt(px(-8.0)).text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(h));
        for (i, p) in app.params.iter().enumerate() {
            match p.ty {
                PType::Bool | PType::Password | PType::Select => {
                    if let Some(a) = pending.take() {
                        body = body.child(full(a));
                        for h in helps.drain(..) {
                            body = body.child(help_row(h));
                        }
                    }
                    body = body.child(self.api_param_row(p, i, cx));
                }
                _ => {
                    let f = half(&p.label, input(u.inputs.get(&p.id).cloned()), true);
                    if !p.help.is_empty() {
                        helps.push(p.help.clone());
                    }
                    match pending.take() {
                        Some(a) => {
                            body = body.child(row2(a, f));
                            for h in helps.drain(..) {
                                body = body.child(help_row(h));
                            }
                        }
                        None => pending = Some(f),
                    }
                }
            }
        }
        if let Some(a) = pending.take() {
            body = body.child(full(a));
            for h in helps.drain(..) {
                body = body.child(help_row(h));
            }
        }
        body = body.child(row2(half("堆栈名称", input(u.project.clone()), true), half("容器名称", input(u.container.clone()), true)));
        if app.limits {
            body = body.child(row2(field_col_with("CPU 限制", None, input(u.cpus.clone()), true, false, Some(core_unit)), field_col_with("内存限制", None, input(u.memory.clone()), true, false, Some(mem_unit))));
        }
        let dir = appstore::stack_dir(app, &u.project.as_ref().map(|p| p.read(cx).value().trim().to_string()).filter(|p| !p.is_empty()).unwrap_or_else(|| format!("otter-{}", app.id)));
        body = body.child(full(field_col("存储地址", None, div().flex_1().min_w_0().px(px(8.0)).truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(dir)).into_any_element(), true, true)));

        // compose 预览
        let shown = u.show_compose;
        let toggle = div()
            .id("api-show-compose")
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .text_size(px(t::TYPE_CAPTION))
            .text_color(c::muted())
            .hover(|s| s.text_color(c::secondary()))
            .child(icon(if shown { IconName::ChevronDown } else { IconName::ChevronRight }, 12.0, c::ghost()))
            .child(if shown { "收起 compose 文件" } else { "查看将创建的 compose 文件" })
            .on_click(cx.listener(|this, _, _, cx| {
                this.dock.api.show_compose = !this.dock.api.show_compose;
                cx.notify();
            }));
        let rendered = if shown { appstore::render(app, &self.api_form(app, cx)).map_err(|e| e) } else { Err(String::new()) };
        let preview = div().flex_shrink_0().flex().flex_col().gap(px(8.0)).child(toggle).when(shown, |d| {
            let (path, env) = match &rendered {
                Ok(p) => (format!("{}/{}", p.dir, appstore::COMPOSE_FILE), (!p.env_masked.is_empty()).then(|| format!("密码保存在 {}/.env,不在这里显示", p.dir))),
                Err(e) => (e.clone(), None),
            };
            let edited = self.dock.api.compose_ed.as_ref().is_some_and(|e| e.read(cx).value() != self.dock.api.compose_seed.as_str());
            let mut cap = div().flex().flex_row().items_center().gap(px(8.0)).child(div().flex_1().min_w_0().truncate().font_family(MONO).text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(SharedString::from(path)));
            if edited {
                cap = cap.child(div().text_size(px(t::TYPE_MICRO)).text_color(c::warn()).child("已手动编辑,上面表单的改动不再同步"));
                cap = cap.child(div().id("api-compose-reset").cursor_pointer().text_size(px(t::TYPE_MICRO)).text_color(c::muted()).hover(|s| s.text_color(c::text())).child("重置").on_click(cx.listener(|this, _, _, cx| {
                    this.dock.api.compose_ed = None;
                    cx.notify();
                })));
            }
            d.child(cap)
                .children(self.dock.api.compose_ed.clone().map(|e| {
                    div().h(px(300.0)).flex_shrink_0().overflow_hidden().rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).bg(c::field()).child(Editor::new(&e).appearance(false).h_full())
                }))
                .children(env.map(|t| div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(t)))
        });
        body.child(preview)
    }

    /// 密码、开关、选择三类参数的整行控件
    fn api_param_row(&self, p: &Param, idx: usize, cx: &mut Context<Self>) -> AnyElement {
        let u = &self.dock.api;
        let pid = p.id.clone();
        match p.ty {
            PType::Password => {
                let show = u.show_pwd;
                let eye = super::syspages::tip(
                    super::dockerpages::icon_btn(("api-eye", idx), if show { IconName::EyeOff } else { IconName::Eye }, c::muted()).on_click(cx.listener(|this, _, window, cx| {
                        this.dock.api.show_pwd = !this.dock.api.show_pwd;
                        let masked = !this.dock.api.show_pwd;
                        for (id, i) in this.dock.api.inputs.clone() {
                            if this.api_app().is_some_and(|a| a.params.iter().any(|p| p.id == id && p.ty == PType::Password)) {
                                i.update(cx, |s, cx| s.set_masked(masked, window, cx));
                            }
                        }
                        cx.notify();
                    })),
                    if show { "隐藏密码" } else { "显示密码" },
                );
                let pid2 = pid.clone();
                let regen = p.generate.then(|| {
                    super::syspages::tip(
                        super::dockerpages::icon_btn(("api-regen", idx), IconName::RefreshCw, c::muted()).on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(i) = this.dock.api.inputs.get(&pid2).cloned() {
                                i.update(cx, |s, cx| s.set_value(&crate::dbinstall::gen_password(), window, cx));
                            }
                        })),
                        "重新生成",
                    )
                });
                let inner = div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .child(u.inputs.get(&p.id).map(|i| div().flex_1().min_w_0().child(Input::new(i).appearance(false).with_size(px(12.0 / 0.875))).into_any_element()).unwrap_or_else(|| div().into_any_element()))
                    .child(eye)
                    .children(regen)
                    .into_any_element();
                let l = SharedString::from(p.label.clone());
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(div().flex().flex_row().child(field_col(l, None, inner, true, false)))
                    .child(div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(if p.secret { "密码已打码,只写进同目录的 .env" } else { "" }))
                    .into_any_element()
            }
            PType::Bool => {
                let on = u.flags.get(&p.id).copied().unwrap_or(false);
                let desc = if p.help.is_empty() { String::new() } else { p.help.clone() };
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .p(px(12.0))
                    .rounded(px(t::RADIUS_MD))
                    .border_1()
                    .border_color(if on && !p.warn.is_empty() { c::danger() } else { c::field_border() })
                    .bg(c::field())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(12.0))
                            .child(div().flex_1().min_w_0().flex().flex_col().gap(px(2.0)).child(div().text_size(px(t::TYPE_CONTROL)).child(SharedString::from(p.label.clone()))).children((!desc.is_empty()).then(|| div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(desc)))))
                            .child(Self::pill_switch(FLAG_IDS[idx.min(7)], on).on_click(cx.listener(move |this, _, _, cx| {
                                let e = this.dock.api.flags.entry(pid.clone()).or_insert(false);
                                *e = !*e;
                                cx.notify();
                            }))),
                    )
                    .when(on && !p.warn.is_empty(), |d| d.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child(SharedString::from(p.warn.clone()))))
                    .into_any_element()
            }
            _ => {
                let cur = u.picks.get(&p.id).copied().unwrap_or(0);
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .gap(px(6.0))
                    .children(p.options.iter().enumerate().map(|(i, o)| {
                        let pid = pid.clone();
                        super::syspages::chip(("api-pick", idx * 100 + i), if o.label.is_empty() { o.value.clone() } else { o.label.clone() }, cur == i).on_click(cx.listener(move |this, _, _, cx| {
                            this.dock.api.picks.insert(pid.clone(), i);
                            cx.notify();
                        }))
                    }))
                    .into_any_element()
            }
        }
    }
}
