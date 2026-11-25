//! 安装数据库:右侧抽屉表单,生成 compose 堆栈(见 `crate::dbinstall`)。装完弹一次管理员密码,Otter 不保存。

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};

use super::super::*;
use super::composeedit::{field_col, field_col_with};
use crate::dbinst::DbKind;
use crate::dbinstall::{self, Spec, KINDS};
use crate::docker::Action;

#[derive(Default)]
pub(crate) struct DbInstallUi {
    open: bool,
    kind: usize,
    version: usize,
    ver_open: bool,
    ver_custom: Option<Entity<InputState>>,
    /// 版本下拉展开时顶部的搜索框
    ver_search: Option<Entity<InputState>>,
    public: bool,
    project: Option<Entity<InputState>>,
    container: Option<Entity<InputState>>,
    port: Option<Entity<InputState>>,
    password: Option<Entity<InputState>>,
    cpus: Option<Entity<InputState>>,
    memory: Option<Entity<InputState>>,
    /// 内存单位:false = MB,true = GB
    mem_gb: bool,
    dir: Option<Entity<InputState>>,
    /// 上一次自动填进去的名字、端口:框里还等于它,说明用户没改过,换类型/改项目名时可以跟着变
    auto_name: String,
    auto_container: String,
    auto_port: String,
    auto_dir: String,
    last_project: String,
    /// 是否展开 compose 预览(默认收起)
    show_compose: bool,
    show_pwd: bool,
    /// 各类型的版本列表(类型序号 → (版本, 来源));没取到就用内置的
    tags: std::collections::HashMap<usize, (Vec<String>, &'static str)>,
    /// 已经发起过获取的类型 / 已经转到本机获取的 / 获取失败的
    tag_req: Vec<usize>,
    /// 服务器那一路已经出结果的类型
    tag_srv_done: Vec<usize>,
    /// 各类型 (已失败的路数, 发起的路数):全部失败才算失败
    tag_errs: std::collections::HashMap<usize, (u8, u8)>,
    tag_fail: Vec<usize>,
    creating: bool,
    error: Option<String>,
    /// 装完要一次性展示的 (容器名, 类型, 端口, 密码)
    done: Option<(String, DbKind, u16, String)>,
    /// 正在创建的实例,等结果回来再决定展示什么
    pending: Option<(String, DbKind, u16, String)>,
}

const DEFAULT_ROOT: &str = "/opt/otter/docker/compose";

impl Home {
    pub(crate) fn dbi_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let kind = match self.db_tab {
            1 => 1,
            2 => 3,
            _ => 0,
        };
        let mk = |ph: &str, window: &mut Window, cx: &mut Context<Self>| {
            let s = cx.new(|cx| InputState::new(window, cx).placeholder(ph.to_string()));
            cx.observe(&s, |_, _, cx| cx.notify()).detach();
            s
        };
        if self.dock.dbi.project.is_none() {
            let vc = mk("如 17.5-alpine", window, cx);
            self.dock.dbi.ver_custom = Some(vc);
            let vs = mk("搜索版本,如 8.4", window, cx);
            self.dock.dbi.ver_search = Some(vs);
            let (cp, me) = (mk("不限制", window, cx), mk("不限制", window, cx));
            self.dock.dbi.cpus = Some(cp);
            self.dock.dbi.memory = Some(me);
            let (a, b, c2, d, e) = (mk("小写字母、数字、- 和 _", window, cx), mk("小写字母、数字、- 和 _", window, cx), mk("宿主机端口", window, cx), { let s = cx.new(|cx| InputState::new(window, cx).placeholder("管理员密码").masked(true)); cx.observe(&s, |_, _, cx| cx.notify()).detach(); s }, mk("堆栈文件所在的目录", window, cx));
            let u = &mut self.dock.dbi;
            u.project = Some(a);
            u.container = Some(b);
            u.port = Some(c2);
            u.password = Some(d);
            u.dir = Some(e);
        }
        let u = &mut self.dock.dbi;
        u.open = true;
        u.creating = false;
        u.show_compose = false;
        u.show_pwd = false;
        u.error = None;
        u.public = false;
        u.kind = kind;
        u.ver_open = false;
        u.last_project.clear();
        self.dock.dbi.version = self.dbi_default_version(kind);
        self.dbi_fill(window, cx);
        self.dbi_reset_limits(window, cx);
        if let Some(p) = self.dock.dbi.password.clone() {
            p.update(cx, |s, cx| s.set_masked(true, window, cx));
        }
        self.dbi_fetch_tags(self.dock.dbi.kind, cx);
        cx.notify();
    }

    /// MySQL、MariaDB 默认选长期支持版(MySQL 8.4、MariaDB 11.8,即官方镜像 `lts` 标签指向的版本);最新的 MySQL 9.x/26.x、MariaDB 12.x/13.x 是滚动或创新版,不适合默认装生产库。其他取最新
    fn default_version_in(k: usize, list: &[String]) -> usize {
        let lts = match k {
            1 => "8.4",
            2 => "11.8",
            _ => return 0,
        };
        list.iter().position(|v| v == lts).unwrap_or(0)
    }

    fn dbi_default_version(&self, k: usize) -> usize {
        self.dock.dbi.tags.get(&k).map(|t| Self::default_version_in(k, &t.0)).unwrap_or(0)
    }

    /// 这个类型的版本列表:取到的真实列表,没取到(加载中或失败)是空的
    fn dbi_versions(&self, k: usize) -> Vec<String> {
        self.dock.dbi.tags.get(&k).map(|t| t.0.clone()).unwrap_or_default()
    }

    /// 取版本列表:服务器和本机同时请求 Docker Hub,谁先拿到就用谁;两边都失败才算失败。演示模式只有本机
    fn dbi_fetch_tags(&mut self, k: usize, cx: &mut Context<Self>) {
        let u = &mut self.dock.dbi;
        if u.tags.contains_key(&k) || u.tag_req.contains(&k) {
            return;
        }
        u.tag_req.push(k);
        let d = if self.demo { None } else { self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref()) };
        let routes = if let Some(d) = d {
            d.shell_run(&format!("dbtags:{k}"), &dbinstall::tags_script(KINDS[k]));
            2
        } else {
            1
        };
        self.dock.dbi.tag_errs.insert(k, (0, routes));
        let kind = KINDS[k];
        cx.spawn(async move |this, cx| {
            let r = cx.background_executor().spawn(async move { dbinstall::fetch_tags_local(kind) }).await;
            let _ = this.update(cx, |h, cx| match r {
                Ok(l) => h.dbi_tags_arrived(k, l, "本机", cx),
                Err(_) => h.dbi_tag_error(k, cx),
            });
        })
        .detach();
    }

    /// 某一路拿到了列表:已经有结果就忽略(先到先用)
    fn dbi_tags_arrived(&mut self, k: usize, list: Vec<String>, from: &'static str, cx: &mut Context<Self>) {
        let u = &mut self.dock.dbi;
        if u.tags.contains_key(&k) {
            return;
        }
        if u.kind == k {
            // MySQL 默认选长期支持版 8.4(最新的 9.x、26.x 是创新版,不适合默认装生产库);其他取最新
            u.version = Self::default_version_in(k, &list);
        }
        u.tags.insert(k, (list, from));
        cx.notify();
    }

    fn dbi_tag_error(&mut self, k: usize, cx: &mut Context<Self>) {
        let u = &mut self.dock.dbi;
        let e = u.tag_errs.entry(k).or_insert((0, 1));
        e.0 += 1;
        if e.0 >= e.1 && !u.tags.contains_key(&k) && !u.tag_fail.contains(&k) {
            u.tag_fail.push(k);
        }
        cx.notify();
    }

    /// 轮询服务器那一路的结果
    fn dbi_poll_tags(&mut self, cx: &mut Context<Self>) {
        let u = &self.dock.dbi;
        let pending: Vec<usize> = u.tag_req.iter().filter(|k| !u.tags.contains_key(k) && !u.tag_srv_done.contains(k) && u.tag_errs.get(k).is_some_and(|e| e.1 == 2)).copied().collect();
        for k in pending {
            let r = self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref()).and_then(|d| d.db_take(&format!("dbtags:{k}")));
            let Some(r) = r else { continue };
            self.dock.dbi.tag_srv_done.push(k);
            match r.ok().and_then(|out| dbinstall::parse_tags(&out)) {
                Some(l) => self.dbi_tags_arrived(k, l, "服务器", cx),
                None => self.dbi_tag_error(k, cx),
            }
        }
    }

    fn dbi_close(&mut self, cx: &mut Context<Self>) {
        self.dock.dbi.open = false;
        cx.notify();
    }

    fn taken_names(&self) -> Vec<String> {
        let snap = &self.dock.snap;
        snap.containers.iter().map(|c| c.name.clone()).chain(self.node().stacks.iter().map(|s| s.name.clone())).chain(snap.containers.iter().filter_map(|c| c.project.clone())).collect()
    }

    fn used_ports(&self) -> Vec<u16> {
        self.dock.snap.containers.iter().flat_map(|c| dbinstall::host_ports(&c.ports)).collect()
    }

    /// 第一个没被占用的宿主机端口(从类型默认端口起)
    fn free_port(&self, kind: DbKind) -> u16 {
        let used = self.used_ports();
        (kind.port()..kind.port().saturating_add(200)).find(|p| !used.contains(p)).unwrap_or(kind.port())
    }

    /// 换类型或刚打开:名字、端口、目录、密码全部重新生成
    fn dbi_fill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let kind = KINDS[self.dock.dbi.kind];
        let name = dbinstall::gen_name(kind, &self.taken_names());
        let port = self.free_port(kind).to_string();
        let dir = format!("{DEFAULT_ROOT}/{name}");
        let pw = dbinstall::gen_password();
        let u = &mut self.dock.dbi;
        u.auto_name = name.clone();
        u.auto_container = name.clone();
        u.auto_port = port.clone();
        u.auto_dir = dir.clone();
        u.last_project = name.clone();
        let (Some(a), Some(b), Some(c2), Some(d), Some(e)) = (u.project.clone(), u.container.clone(), u.port.clone(), u.password.clone(), u.dir.clone()) else { return };
        a.update(cx, |s, cx| s.set_value(&name, window, cx));
        b.update(cx, |s, cx| s.set_value(&name, window, cx));
        c2.update(cx, |s, cx| s.set_value(&port, window, cx));
        d.update(cx, |s, cx| s.set_value(&pw, window, cx));
        e.update(cx, |s, cx| s.set_value(&dir, window, cx));
    }

    fn dbi_reset_limits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dock.dbi.mem_gb = true;
        for i in [self.dock.dbi.cpus.clone(), self.dock.dbi.memory.clone()].into_iter().flatten() {
            i.update(cx, |s, cx| s.set_value("", window, cx));
        }
    }

    /// 每次渲染前调用:项目名变了,没手改过的容器名和目录跟着变;预览跟着表单变
    pub(crate) fn dbi_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dock.dbi.open {
            return;
        }
        self.dbi_poll_tags(cx);
        let u = &self.dock.dbi;
        let (Some(a), Some(b), Some(e)) = (u.project.clone(), u.container.clone(), u.dir.clone()) else { return };
        let project = a.read(cx).value().trim().to_string();
        if project != u.last_project {
            let (old_c, old_d) = (u.auto_container.clone(), u.auto_dir.clone());
            let new_d = format!("{DEFAULT_ROOT}/{project}");
            let u = &mut self.dock.dbi;
            u.last_project = project.clone();
            if b.read(cx).value() == old_c {
                u.auto_container = project.clone();
                b.update(cx, |s, cx| s.set_value(&project, window, cx));
            }
            if e.read(cx).value() == old_d {
                u.auto_dir = new_d.clone();
                e.update(cx, |s, cx| s.set_value(&new_d, window, cx));
            }
        }
    }

    /// 读表单。端口解析失败返回 None(预览不更新)
    fn dbi_form(&self, cx: &App) -> Option<(FormSpec, String)> {
        let u = &self.dock.dbi;
        let v = |i: &Option<Entity<InputState>>| i.as_ref().map(|i| i.read(cx).value().trim().to_string()).unwrap_or_default();
        let port: u16 = v(&u.port).parse().ok()?;
        let kind = KINDS[u.kind];
        let vers = self.dbi_versions(u.kind);
        let version = vers.get(u.version).cloned().unwrap_or_else(|| v(&u.ver_custom));
        let spec = FormSpec { kind, version, project: v(&u.project), container: v(&u.container), port, public: u.public, password: v(&u.password), cpus: v(&u.cpus), memory: { let n = v(&u.memory); if n.is_empty() { n } else { format!("{n}{}", if u.mem_gb { "g" } else { "m" }) } } };
        Some((spec, v(&u.dir)))
    }

    fn dbi_submit(&mut self, cx: &mut Context<Self>) {
        if self.dock.dbi.creating {
            return;
        }
        let Some((f, dir)) = self.dbi_form(cx) else {
            self.dock.dbi.error = Some("端口要是数字".into());
            cx.notify();
            return;
        };
        let k = self.dock.dbi.kind;
        if !self.dock.dbi.tags.contains_key(&k) && !self.dock.dbi.tag_fail.contains(&k) {
            self.dock.dbi.error = Some("版本列表还在获取,请稍候".into());
            cx.notify();
            return;
        }
        let spec = f.spec();
        let err = spec.validate(&self.taken_names(), &self.used_ports()).err().or_else(|| (!dir.starts_with('/') || dir.chars().any(|c| c.is_control())).then(|| "存储地址要是以 / 开头的绝对路径".to_string()));
        if let Some(e) = err {
            self.dock.dbi.error = Some(e);
            cx.notify();
            return;
        }
        let (text, env) = (spec.compose(), spec.env());
        self.dock.dbi.error = None;
        if self.demo {
            self.dock.dbi.open = false;
            self.dock.dbi.done = Some((f.container.clone(), f.kind, f.port, f.password.clone()));
        } else {
            self.dock.dbi.creating = true;
            self.dock.dbi.pending = Some((f.project.clone(), f.kind, f.port, f.password.clone()));
        }
        self.dock_act(Action::ComposeCreate { project: f.project, dir, text, env }, cx);
    }

    /// 收到「创建」的结果:成功就关抽屉、弹一次密码,失败留着表单(原因在通知条)
    pub(super) fn dbi_applied(&mut self, notice: &str, cx: &mut Context<Self>) {
        if !self.dock.dbi.creating {
            return;
        }
        if notice.starts_with("已创建") {
            let u = &mut self.dock.dbi;
            u.creating = false;
            u.open = false;
            if let Some((name, kind, port, pw)) = u.pending.take() {
                u.done = Some((name, kind, port, pw));
            }
            cx.notify();
        } else if notice.starts_with("创建失败") {
            self.dock.dbi.creating = false;
            self.dock.dbi.pending = None;
            cx.notify();
        }
    }

    pub(crate) fn dbi_overlays(&mut self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v = Vec::new();
        v.extend(self.dbi_drawer(window, cx).map(|d| d.into_any_element()));
        v.extend(self.dbi_done_dialog(cx).map(|d| d.into_any_element()));
        if self.page == Page::Database {
            v.extend(self.dbm_overlays(window, cx));
        }
        v
    }

    fn dbi_drawer(&self, window: &Window, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let u = &self.dock.dbi;
        if !u.open {
            return None;
        }
        let w = (f32::from(window.viewport_size().width) * 0.5).clamp(480.0, 720.0);
        let kind = KINDS[u.kind];
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
            .child(icon(IconName::Database, 16.0, c::secondary()))
            .child(div().flex_1().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(format!("安装 {}", kind.label())))
            .child(super::syspages::tip(super::dockerpages::icon_btn(("dbi-x", 0), IconName::X, c::muted()).on_click(cx.listener(|this, _, _, cx| this.dbi_close(cx))), "关闭"));
        let input = |i: Option<Entity<InputState>>| i.map(|i| div().flex_1().min_w_0().child(Input::new(&i).appearance(false).with_size(px(12.0 / 0.875))).into_any_element()).unwrap_or_else(|| div().into_any_element());
        let label = |s: &'static str| div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(s);

        // 类型跟着当前页签走;只有 MySQL 页签里有 MySQL / MariaDB 两种可选
        let opts: Vec<usize> = KINDS.iter().enumerate().filter(|(_, k)| super::dbpages::DB_TABS[self.db_tab.min(2)].1.contains(k)).map(|(i, _)| i).collect();
        let kinds = (opts.len() > 1).then(|| {
            div().flex().flex_row().gap(px(6.0)).children(opts.iter().map(|&i| {
                super::syspages::chip(("dbi-kind", i), KINDS[i].label(), u.kind == i).on_click(cx.listener(move |this, _, window, cx| {
                    if this.dock.dbi.kind != i {
                        this.dock.dbi.kind = i;
                        this.dock.dbi.version = this.dbi_default_version(i);
                        this.dbi_fill(window, cx);
                        this.dbi_fetch_tags(i, cx);
                        cx.notify();
                    }
                }))
            }))
        });

        // 版本:下拉(真实列表或内置列表)+ 自定义
        let vers = self.dbi_versions(u.kind);
        let custom = u.version >= vers.len();
        let loading = !u.tags.contains_key(&u.kind) && !u.tag_fail.contains(&u.kind);
        // 加载中:选框里显示提示、不可展开,后缀放我们自己的加载动画
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
                let k = h.dock.dbi.kind;
                if h.dock.dbi.tags.contains_key(&k) || h.dock.dbi.tag_fail.contains(&k) {
                    h.dock.dbi.ver_open = !h.dock.dbi.ver_open;
                    if h.dock.dbi.ver_open {
                        if let Some(s) = h.dock.dbi.ver_search.clone() {
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
                h.dock.dbi.version = i;
                h.dock.dbi.ver_open = false;
                if i >= h.dbi_versions(h.dock.dbi.kind).len() {
                    if let Some(vc) = h.dock.dbi.ver_custom.clone() {
                        let f = vc.focus_handle(cx);
                        window.focus(&f, cx);
                    }
                }
                cx.notify();
            })
        };
        let ver_dismiss = move |_: &mut Window, cx: &mut App| {
            t3.update(cx, |h, cx| {
                h.dock.dbi.ver_open = false;
                cx.notify();
            })
        };
        let spin = loading.then(|| crate::ui::dot_spinner(("dbi-ver-load", 0), c::muted()).into_any_element());
        let (cur, open) = (if loading { 0 } else { u.version.min(vers.len()) }, u.ver_open && !loading);
        let select = match search {
            Some(sq) => crate::ui::select_search(34.0, "dbi-ver", IconName::Tag, items, cur, open, sq, ver_toggle, ver_pick, ver_dismiss, spin),
            None => crate::ui::select_sized(34.0, "dbi-ver", IconName::Tag, items, cur, open, move |w, cx| ver_toggle(w, cx), ver_pick, ver_dismiss, spin),
        };
        let ver_col = div().flex_1().min_w_0().flex().flex_col().gap(px(6.0)).child(label("版本")).child(div().h(px(34.0)).child(select));
        let src_note = match u.tags.get(&u.kind) {
            Some(t) => format!("版本列表来自{}的 Docker Hub", t.1),
            None if loading => "服务器和本机都会尝试访问 Docker Hub".to_string(),
            None => "服务器和本机都没能访问 Docker Hub,请手填版本号".to_string(),
        };
        let floating = !custom && vers[u.version].matches('.').count() == usize::from(kind != DbKind::Postgres);
        let failed = !loading && !u.tags.contains_key(&u.kind);
        let ver_hint = if loading { "" } else if custom { "填写镜像的 tag" } else if floating { "跟随该大版本的最新补丁" } else { "固定在这个具体版本" };

        let row2 = |a: Div, b: Div| div().flex_shrink_0().flex().flex_row().gap(px(12.0)).child(a).child(b);
        // 整行的字段:field_col 自带 flex_1,直接放进纵向容器会被拉高,外面包一层横向的
        let full = |d: Div| div().flex_shrink_0().flex().flex_row().child(d);
        let half = |l: &'static str, inner: AnyElement, mono: bool| field_col(l, None, inner, mono, false);

        // 内存单位:框内右侧的 MB / GB 两段选择
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
                    this.dock.dbi.mem_gb = to_gb;
                    cx.notify();
                }))
        };
        let mem_unit = div().flex().flex_row().gap(px(2.0)).child(seg("dbi-mem-mb", "MB", !gb, false)).child(seg("dbi-mem-gb", "GB", gb, true)).into_any_element();
        let core_unit = div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("核").into_any_element();
        // 密码:默认隐藏,小眼睛切换,右边再放「重新生成」
        let show = u.show_pwd;
        let eye = super::syspages::tip(
            super::dockerpages::icon_btn(("dbi-eye", 0), if show { IconName::EyeOff } else { IconName::Eye }, c::muted()).on_click(cx.listener(|this, _, window, cx| {
                this.dock.dbi.show_pwd = !this.dock.dbi.show_pwd;
                let masked = !this.dock.dbi.show_pwd;
                if let Some(p) = this.dock.dbi.password.clone() {
                    p.update(cx, |s, cx| s.set_masked(masked, window, cx));
                }
                cx.notify();
            })),
            if show { "隐藏密码" } else { "显示密码" },
        );
        let regen = super::syspages::tip(
            super::dockerpages::icon_btn(("dbi-regen", 0), IconName::RefreshCw, c::muted()).on_click(cx.listener(|this, _, window, cx| {
                if let Some(p) = this.dock.dbi.password.clone() {
                    p.update(cx, |s, cx| s.set_value(&dbinstall::gen_password(), window, cx));
                }
            })),
            "重新生成",
        );
        let pw_inner = div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(4.0)).child(input(u.password.clone())).child(eye).child(regen).into_any_element();

        // 对外开放:一张卡片,开关 + 说明
        let public = u.public;
        let public_card = div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(12.0))
            .rounded(px(t::RADIUS_MD))
            .border_1()
            .border_color(if public { c::danger() } else { c::field_border() })
            .bg(c::field())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .child(div().flex_1().min_w_0().flex().flex_col().gap(px(2.0)).child(div().text_size(px(t::TYPE_CONTROL)).child("对外开放端口")).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(if public { "绑定 0.0.0.0,公网可以直接连" } else { "只绑定 127.0.0.1,服务器本机和 SSH 隧道能连" })))
                    .child(Self::pill_switch("dbi-public", public).on_click(cx.listener(|this, _, _, cx| {
                        this.dock.dbi.public = !this.dock.dbi.public;
                        cx.notify();
                    }))),
            )
            .when(public, |d| d.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child("数据库暴露在公网上有被扫描、爆破的风险,请确认防火墙和密码强度")));

        let shown = u.show_compose;
        let toggle = div()
            .id("dbi-show-compose")
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
                this.dock.dbi.show_compose = !this.dock.dbi.show_compose;
                cx.notify();
            }));
        let preview = div().flex_shrink_0().flex().flex_col().gap(px(8.0)).child(toggle).when(shown, |d| {
            // 只读预览:按内容自动撑高,不自带滚动条,超出抽屉高度时跟着整个表单滚
            let text = self.dbi_form(cx).map(|f| format!("# docker-compose.yml\n{}\n# .env\n{}", f.0.compose(&f.1), f.0.env_masked())).unwrap_or_else(|| "端口要是数字".into());
            d.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .px(px(12.0))
                    .py(px(10.0))
                    .rounded(px(t::RADIUS_MD))
                    .border_1()
                    .border_color(c::field_border())
                    .bg(c::field())
                    .font_family(MONO)
                    .text_size(px(t::TYPE_CAPTION))
                    .text_color(c::secondary())
                    .children(text.lines().map(|l| div().min_h(px(16.0)).whitespace_nowrap().child(if l.is_empty() { " ".to_string() } else { l.to_string() }))),
            )
                .child(div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child("密码已打码,只写进同目录的 .env"))
        });

        let body = div()
            .id("dbi-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .p(px(16.0))
            .children(kinds)
            .child(row2(ver_col, div().flex_1().min_w_0().child(half("宿主机端口", input(u.port.clone()), true))))
            .child(
                div()
                    .flex_shrink_0()
                    .mt(px(-8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(t::TYPE_CAPTION))
                    .text_color(c::ghost())
                    .child(if ver_hint.is_empty() { src_note.clone() } else { format!("{ver_hint}  ·  {src_note}") })
                    .when(failed, |d| {
                        d.child(div().id("dbi-ver-retry").cursor_pointer().text_color(c::accent()).hover(|s| s.opacity(0.8)).child("重试").on_click(cx.listener(|this, _, _, cx| {
                            let k = this.dock.dbi.kind;
                            let u = &mut this.dock.dbi;
                            u.tag_req.retain(|x| *x != k);
                            u.tag_srv_done.retain(|x| *x != k);
                            u.tag_errs.remove(&k);
                            u.tag_fail.retain(|x| *x != k);
                            this.dbi_fetch_tags(k, cx);
                            cx.notify();
                        })))
                    }),
            )
            .children(custom.then(|| full(half("自定义版本号", input(u.ver_custom.clone()), true))))
            .child(row2(half("堆栈名称", input(u.project.clone()), true), half("容器名称", input(u.container.clone()), true)))
            .child(row2(field_col_with("CPU 限制", None, input(u.cpus.clone()), true, false, Some(core_unit)), field_col_with("内存限制", None, input(u.memory.clone()), true, false, Some(mem_unit))))
            .child(full(half("管理员密码", pw_inner, true)))
            .child(full(half("存储地址", input(u.dir.clone()), true)))
            .child(public_card)
            .child(preview);
        let creating = u.creating;
        let ok = div()
            .id("dbi-ok")
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
            .when(creating, |d| d.opacity(0.6))
            .gap(px(8.0))
            .when(creating, |d| d.child(crate::ui::dot_spinner(("dbi-ok-spin", 0), c::on_accent())))
            .child(if creating { "安装中" } else { "安装" })
            .on_click(cx.listener(|this, _, _, cx| this.dbi_submit(cx)));
        let foot = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(16.0))
            .py(px(12.0))
            .border_t_1()
            .border_color(c::border())
            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CAPTION)).text_color(if u.error.is_some() { c::danger() } else { c::ghost() }).child(u.error.clone().unwrap_or_else(|| "镜像要先拉取,第一次安装可能要几分钟".into())))
            .child(ok);
        Some(crate::ui::drawer("dbi-drawer", w, cx.listener(|this, _, _, cx| this.dbi_close(cx)), [head.into_any_element(), body.into_any_element(), foot.into_any_element()]).on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
            if e.keystroke.key == "escape" {
                this.dbi_close(cx);
                cx.stop_propagation();
            }
        })))
    }

    /// 装完一次性展示密码
    fn dbi_done_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let (name, kind, port, pw) = self.dock.dbi.done.clone()?;
        let row = |k: &'static str, v: String| div().flex().flex_row().gap(px(12.0)).child(div().w(px(70.0)).text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(k)).child(div().flex_1().min_w_0().font_family(MONO).text_size(px(t::TYPE_CONTROL)).child(v));
        let pwc = pw.clone();
        let body = div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(format!("{} 已安装", kind.label())))
            .child(row("容器", name))
            .child(row("端口", port.to_string()))
            .child(row("密码", pw))
            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child("密码只在这里显示这一次,Otter 不会保存。请现在复制;之后可以在服务器上该堆栈目录的 .env 里找到。"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(button("dbi-copy", IconName::Copy, "复制密码").on_click(cx.listener(move |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(pwc.clone()));
                    })))
                    .child(button("dbi-done", IconName::Check, "我已保存").on_click(cx.listener(|this, _, _, cx| {
                        this.dock.dbi.done = None;
                        cx.notify();
                    }))),
            );
        Some(crate::ui::modal("dbi-done-dialog", 460.0, |_, _, _| {}, [div().p(px(20.0)).child(body).into_any_element()]))
    }
}

/// 表单读出来的值(拥有字符串,`spec()` 借出给 `dbinstall::Spec`)
pub(crate) struct FormSpec {
    kind: DbKind,
    version: String,
    project: String,
    container: String,
    port: u16,
    public: bool,
    password: String,
    cpus: String,
    memory: String,
}

impl FormSpec {
    fn spec(&self) -> Spec<'_> {
        Spec { kind: self.kind, version: &self.version, project: &self.project, container: &self.container, port: self.port, public: self.public, password: &self.password, cpus: &self.cpus, memory: &self.memory }
    }
    fn compose(&self, _dir: &str) -> String {
        self.spec().compose()
    }
    fn env_masked(&self) -> String {
        self.spec().env_masked()
    }
}
