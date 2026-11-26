//! 安装 OpenResty:右侧抽屉表单,版式和数据库安装抽屉(`dbinstallui.rs`)一致。
//! 提交后走网站页的后台安装任务(`web_job_start`),抽屉关闭、任务面板显示进度。

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};

use super::super::*;
use super::composeedit::{field_col, field_col_with};
use crate::web::{self, InstallSpec};

#[derive(Default)]
pub(crate) struct WebInstallUi {
    open: bool,
    version: usize,
    ver_open: bool,
    ver_custom: Option<Entity<InputState>>,
    ver_search: Option<Entity<InputState>>,
    project: Option<Entity<InputState>>,
    container: Option<Entity<InputState>>,
    http: Option<Entity<InputState>>,
    https: Option<Entity<InputState>>,
    cpus: Option<Entity<InputState>>,
    memory: Option<Entity<InputState>>,
    /// 内存单位:false = MB,true = GB
    mem_gb: bool,
    /// 上一次自动填进去的堆栈名、容器名:框里还等于它,说明没手改过,改堆栈名时容器名跟着变
    auto_container: String,
    last_project: String,
    show_compose: bool,
    /// 版本列表和来源;没取到前是 None
    tags: Option<(Vec<String>, &'static str)>,
    tag_req: bool,
    tag_srv_done: bool,
    /// (已失败的路数, 发起的路数):全部失败才算失败
    tag_errs: (u8, u8),
    tag_fail: bool,
    error: Option<String>,
}

impl WebInstallUi {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }
}

const TAG_KEY: &str = "wbtags";

impl Home {
    pub(super) fn wbi_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mk = |ph: &str, window: &mut Window, cx: &mut Context<Self>| {
            let s = cx.new(|cx| InputState::new(window, cx).placeholder(ph.to_string()));
            cx.observe(&s, |_, _, cx| cx.notify()).detach();
            s
        };
        if self.dock.wbi.project.is_none() {
            let u = (mk("如 1.27.1.2-alpine", window, cx), mk("搜索版本,如 1.27", window, cx), mk("不限制", window, cx), mk("不限制", window, cx));
            let n = (mk("小写字母、数字、- 和 _", window, cx), mk("小写字母、数字、- 和 _", window, cx), mk("", window, cx), mk("", window, cx));
            let w = &mut self.dock.wbi;
            (w.ver_custom, w.ver_search, w.cpus, w.memory) = (Some(u.0), Some(u.1), Some(u.2), Some(u.3));
            (w.project, w.container, w.http, w.https) = (Some(n.0), Some(n.1), Some(n.2), Some(n.3));
        }
        let d = InstallSpec::default();
        let (http, https) = (d.http.to_string(), d.https.to_string());
        let w = &mut self.dock.wbi;
        w.open = true;
        w.show_compose = false;
        w.error = None;
        w.ver_open = false;
        w.version = 0;
        w.mem_gb = true;
        w.auto_container = d.container.clone();
        w.last_project = d.project.clone();
        let set = |i: &Option<Entity<InputState>>, v: &str, window: &mut Window, cx: &mut Context<Self>| {
            if let Some(i) = i {
                i.update(cx, |s, cx| s.set_value(v, window, cx));
            }
        };
        let (p, c2, h, hs, cp, me) = (w.project.clone(), w.container.clone(), w.http.clone(), w.https.clone(), w.cpus.clone(), w.memory.clone());
        set(&p, &d.project, window, cx);
        set(&c2, &d.container, window, cx);
        set(&h, &http, window, cx);
        set(&hs, &https, window, cx);
        set(&cp, "", window, cx);
        set(&me, "", window, cx);
        self.wbi_fetch_tags(cx);
        cx.notify();
    }

    fn wbi_close(&mut self, cx: &mut Context<Self>) {
        self.dock.wbi.open = false;
        cx.notify();
    }

    fn wbi_versions(&self) -> Vec<String> {
        self.dock.wbi.tags.as_ref().map(|t| t.0.clone()).unwrap_or_default()
    }

    /// 取版本列表:服务器和本机同时请求 Docker Hub,谁先拿到就用谁;两边都失败才算失败。演示模式只有本机
    fn wbi_fetch_tags(&mut self, cx: &mut Context<Self>) {
        if self.dock.wbi.tags.is_some() || self.dock.wbi.tag_req {
            return;
        }
        self.dock.wbi.tag_req = true;
        let d = if self.demo { None } else { self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref()) };
        let routes = if let Some(d) = d {
            d.shell_run(TAG_KEY, &web::tags_script());
            2
        } else {
            1
        };
        self.dock.wbi.tag_errs = (0, routes);
        cx.spawn(async move |this, cx| {
            let r = cx.background_executor().spawn(async move { web::fetch_tags_local() }).await;
            let _ = this.update(cx, |h, cx| match r {
                Ok(l) => h.wbi_tags_arrived(l, "本机", cx),
                Err(_) => h.wbi_tag_error(cx),
            });
        })
        .detach();
    }

    fn wbi_tags_arrived(&mut self, list: Vec<String>, from: &'static str, cx: &mut Context<Self>) {
        let w = &mut self.dock.wbi;
        if w.tags.is_none() {
            w.version = 0;
            w.tags = Some((list, from));
            cx.notify();
        }
    }

    fn wbi_tag_error(&mut self, cx: &mut Context<Self>) {
        let w = &mut self.dock.wbi;
        w.tag_errs.0 += 1;
        if w.tag_errs.0 >= w.tag_errs.1 && w.tags.is_none() {
            w.tag_fail = true;
        }
        cx.notify();
    }

    fn wbi_poll_tags(&mut self, cx: &mut Context<Self>) {
        let w = &self.dock.wbi;
        if !w.tag_req || w.tags.is_some() || w.tag_srv_done || w.tag_errs.1 != 2 {
            return;
        }
        let r = self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref()).and_then(|d| d.db_take(TAG_KEY));
        let Some(r) = r else { return };
        self.dock.wbi.tag_srv_done = true;
        match r.ok().and_then(|out| web::parse_tags(&out)) {
            Some(l) => self.wbi_tags_arrived(l, "服务器", cx),
            None => self.wbi_tag_error(cx),
        }
    }

    /// 每次渲染前调用:堆栈名变了,没手改过的容器名跟着变;收服务器那一路的版本列表
    pub(crate) fn wbi_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dock.wbi.open {
            return;
        }
        self.wbi_poll_tags(cx);
        let w = &self.dock.wbi;
        let (Some(a), Some(b)) = (w.project.clone(), w.container.clone()) else { return };
        let project = a.read(cx).value().trim().to_string();
        if project != w.last_project {
            let old = w.auto_container.clone();
            let w = &mut self.dock.wbi;
            w.last_project = project.clone();
            if b.read(cx).value() == old {
                w.auto_container = project.clone();
                b.update(cx, |s, cx| s.set_value(&project, window, cx));
            }
        }
    }

    /// 读表单。端口解析失败返回 None
    fn wbi_form(&self, cx: &App) -> Option<InstallSpec> {
        let w = &self.dock.wbi;
        let v = |i: &Option<Entity<InputState>>| i.as_ref().map(|i| i.read(cx).value().trim().to_string()).unwrap_or_default();
        let vers = self.wbi_versions();
        let version = vers.get(w.version).cloned().unwrap_or_else(|| v(&w.ver_custom));
        let memory = {
            let n = v(&w.memory);
            if n.is_empty() { n } else { format!("{n}{}", if w.mem_gb { "g" } else { "m" }) }
        };
        Some(InstallSpec { version, project: v(&w.project), container: v(&w.container), http: v(&w.http).parse().ok()?, https: v(&w.https).parse().ok()?, cpus: v(&w.cpus), memory })
    }

    fn wbi_submit(&mut self, cx: &mut Context<Self>) {
        let Some(spec) = self.wbi_form(cx) else {
            self.dock.wbi.error = Some("端口要是 1 到 65535 的数字".into());
            cx.notify();
            return;
        };
        if self.dock.wbi.tags.is_none() && !self.dock.wbi.tag_fail {
            self.dock.wbi.error = Some("版本列表还在获取,请稍候".into());
            cx.notify();
            return;
        }
        let mut used = self.used_ports();
        if let Some(st) = self.web_state_ref() {
            used.extend(st.ports.iter().map(|p| p.0));
        }
        if let Err(e) = spec.validate(&self.taken_names(), &used) {
            self.dock.wbi.error = Some(e);
            cx.notify();
            return;
        }
        self.dock.wbi.open = false;
        self.web_job_start("安装 OpenResty", web::install_job_script(&spec), None, cx);
        cx.notify();
    }

    pub(crate) fn wbi_overlays(&mut self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.wbi_drawer(window, cx).map(|d| d.into_any_element()).into_iter().collect()
    }

    fn wbi_drawer(&self, window: &Window, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let u = &self.dock.wbi;
        if !u.open {
            return None;
        }
        let w = (f32::from(window.viewport_size().width) * 0.5).clamp(480.0, 720.0);
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
            .child(icon(IconName::Globe, 16.0, c::secondary()))
            .child(div().flex_1().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child("安装 OpenResty"))
            .child(super::syspages::tip(super::dockerpages::icon_btn(("wbi-x", 0), IconName::X, c::muted()).on_click(cx.listener(|this, _, _, cx| this.wbi_close(cx))), "关闭"));
        let input = |i: Option<Entity<InputState>>| i.map(|i| div().flex_1().min_w_0().child(Input::new(&i).appearance(false).with_size(px(12.0 / 0.875))).into_any_element()).unwrap_or_else(|| div().into_any_element());
        let label = |s: &'static str| div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(s);

        // 版本:下拉(真实列表)+ 自定义
        let vers = self.wbi_versions();
        let custom = u.version >= vers.len() && !vers.is_empty() || (vers.is_empty() && u.tag_fail);
        let loading = u.tags.is_none() && !u.tag_fail;
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
                if h.dock.wbi.tags.is_some() || h.dock.wbi.tag_fail {
                    h.dock.wbi.ver_open = !h.dock.wbi.ver_open;
                    if h.dock.wbi.ver_open {
                        if let Some(s) = h.dock.wbi.ver_search.clone() {
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
                h.dock.wbi.version = i;
                h.dock.wbi.ver_open = false;
                if i >= h.wbi_versions().len() {
                    if let Some(vc) = h.dock.wbi.ver_custom.clone() {
                        let f = vc.focus_handle(cx);
                        window.focus(&f, cx);
                    }
                }
                cx.notify();
            })
        };
        let ver_dismiss = move |_: &mut Window, cx: &mut App| {
            t3.update(cx, |h, cx| {
                h.dock.wbi.ver_open = false;
                cx.notify();
            })
        };
        let spin = loading.then(|| crate::ui::dot_spinner(("wbi-ver-load", 0), c::muted()).into_any_element());
        let (cur, open) = (if loading { 0 } else { u.version.min(vers.len()) }, u.ver_open && !loading);
        let select = match search {
            Some(sq) => crate::ui::select_search(34.0, "wbi-ver", IconName::Tag, items, cur, open, sq, ver_toggle, ver_pick, ver_dismiss, spin),
            None => crate::ui::select_sized(34.0, "wbi-ver", IconName::Tag, items, cur, open, move |w, cx| ver_toggle(w, cx), ver_pick, ver_dismiss, spin),
        };
        let ver_col = div().flex_1().min_w_0().flex().flex_col().gap(px(6.0)).child(label("版本")).child(div().h(px(34.0)).child(select));
        let src_note = match &u.tags {
            Some(t) => format!("版本列表来自{}的 Docker Hub", t.1),
            None if loading => "服务器和本机都会尝试访问 Docker Hub".to_string(),
            None => "服务器和本机都没能访问 Docker Hub,请手填版本号".to_string(),
        };
        let failed = !loading && u.tags.is_none();
        let ver_hint = if loading {
            ""
        } else if custom {
            "填写镜像的 tag"
        } else if vers.get(u.version).is_some_and(|v| v == "alpine") {
            "跟随最新的 OpenResty"
        } else {
            "固定在这个具体版本"
        };

        let row2 = |a: Div, b: Div| div().flex_shrink_0().flex().flex_row().gap(px(12.0)).child(a).child(b);
        let full = |d: Div| div().flex_shrink_0().flex().flex_row().child(d);
        let half = |l: &'static str, inner: AnyElement, mono: bool| field_col(l, None, inner, mono, false);

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
                    this.dock.wbi.mem_gb = to_gb;
                    cx.notify();
                }))
        };
        let mem_unit = div().flex().flex_row().gap(px(2.0)).child(seg("wbi-mem-mb", "MB", !gb, false)).child(seg("wbi-mem-gb", "GB", gb, true)).into_any_element();
        let core_unit = div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("核").into_any_element();

        // 存储地址只读:由堆栈名决定
        let dir = crate::srvdata::compose_dir(u.project.as_ref().map(|p| p.read(cx).value().trim().to_string()).filter(|p| !p.is_empty()).as_deref().unwrap_or("openresty"));
        let dir_view = div().flex_1().min_w_0().px(px(8.0)).truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(dir)).into_any_element();

        let shown = u.show_compose;
        let toggle = div()
            .id("wbi-show-compose")
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
                this.dock.wbi.show_compose = !this.dock.wbi.show_compose;
                cx.notify();
            }));
        let preview = div().flex_shrink_0().flex().flex_col().gap(px(8.0)).child(toggle).when(shown, |d| {
            let text = self.wbi_form(cx).map(|f| format!("# {}/compose.yml\n{}", f.dir(), f.compose())).unwrap_or_else(|| "端口要是数字".into());
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
        });

        let body = div()
            .id("wbi-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .p(px(16.0))
            .child(row2(ver_col, div().flex_1().min_w_0()))
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
                        d.child(div().id("wbi-ver-retry").cursor_pointer().text_color(c::accent()).hover(|s| s.opacity(0.8)).child("重试").on_click(cx.listener(|this, _, _, cx| {
                            let u = &mut this.dock.wbi;
                            (u.tag_req, u.tag_srv_done, u.tag_fail) = (false, false, false);
                            this.wbi_fetch_tags(cx);
                            cx.notify();
                        })))
                    }),
            )
            .children(custom.then(|| full(half("自定义版本号", input(u.ver_custom.clone()), true))))
            .child(row2(half("HTTP 端口", input(u.http.clone()), true), half("HTTPS 端口", input(u.https.clone()), true)))
            .child(div().flex_shrink_0().mt(px(-8.0)).text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("容器用 host 网络,这两个端口由 OpenResty 直接占用:默认站点监听它们,新建站点也以它们为默认端口。证书的 HTTP 验证需要公网能访问 80"))
            .child(row2(half("堆栈名称", input(u.project.clone()), true), half("容器名称", input(u.container.clone()), true)))
            .child(row2(field_col_with("CPU 限制", None, input(u.cpus.clone()), true, false, Some(core_unit)), field_col_with("内存限制", None, input(u.memory.clone()), true, false, Some(mem_unit))))
            .child(full(field_col("存储地址", None, dir_view, true, true)))
            .child(preview);
        let ok = div()
            .id("wbi-ok")
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
            .on_click(cx.listener(|this, _, _, cx| this.wbi_submit(cx)));
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
        Some(crate::ui::drawer("wbi-drawer", w, cx.listener(|this, _, _, cx| this.wbi_close(cx)), [head.into_any_element(), body.into_any_element(), foot.into_any_element()]).on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
            if e.keystroke.key == "escape" {
                this.wbi_close(cx);
                cx.stop_propagation();
            }
        })))
    }
}
