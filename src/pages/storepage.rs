//! 应用商店页:应用目录的卡片列表、搜索和分类;已安装实例的升级、卸载。
//! 安装走统一的抽屉(`installui.rs`),升级和卸载的任务也在那个抽屉里显示进度。

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};

use super::super::*;
use super::composeedit::field_col;
use crate::appstore::{self, App as AppDef, Record};

pub(crate) struct Upgrade {
    app: AppDef,
    container: String,
    project: String,
    dir: String,
    /// 服务器上的实例记录和当前 compose;还没读到是 None
    rec: Option<(Record, String)>,
    err: Option<String>,
    ver: Entity<InputState>,
    /// 版本输入框是否已经按版本列表预填过
    filled: bool,
}

pub(crate) struct Uninstall {
    app: AppDef,
    container: String,
    dir: String,
    delete: bool,
    confirm: Entity<InputState>,
    err: Option<String>,
}

#[derive(Default)]
pub(crate) struct StoreUi {
    q: Option<Entity<InputState>>,
    cat: usize,
    /// 0 没拉过 / 1 拉取中 / 2 成功 / 3 失败
    remote: u8,
    remote_err: String,
    up: Option<Upgrade>,
    un: Option<Uninstall>,
}

const T_REC: &str = "apprec";

impl Home {
    /// 每次渲染前:商店页第一次出现时去拉在线目录;收升级需要的实例记录
    pub(crate) fn store_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Store {
            return;
        }
        if self.dock.api.st.q.is_none() {
            let q = cx.new(|cx| InputState::new(window, cx).placeholder("搜索应用"));
            cx.observe(&q, |_, _, cx| cx.notify()).detach();
            self.dock.api.st.q = Some(q);
        }
        if self.dock.api.st.remote == 0 && !self.demo {
            self.store_refresh(cx);
        }
        if let Some(u) = self.dock.api.st.up.as_ref().filter(|u| u.rec.is_none() && u.err.is_none()) {
            let r = self.api_docker().and_then(|d| d.db_take(T_REC));
            let _ = u;
            if let Some(r) = r {
                let up = self.dock.api.st.up.as_mut().unwrap();
                match r.ok().and_then(|o| appstore::parse_record_output(&o)) {
                    Some(rec) => up.rec = Some(rec),
                    None => up.err = Some("这个实例没有 Otter 的安装记录(不是用商店装的,或记录被删了),不能升级".into()),
                }
            }
        }
        // 版本列表到了就预填一个目标版本
        let fill = self.dock.api.st.up.as_ref().filter(|u| !u.filled).and_then(|u| {
            let rec = &u.rec.as_ref()?.0;
            let list = self.dock.api.tags_of(&u.app.id)?;
            let cur_major = rec.version.split(['.', '-']).next().unwrap_or("").to_string();
            let pick = if u.app.kind.as_deref() == Some("database") { list.iter().find(|v| v.split(['.', '-']).next() == Some(cur_major.as_str())).cloned() } else { None };
            Some(pick.unwrap_or_else(|| list.get(appstore::default_index(&u.app, &list)).cloned().unwrap_or_default()))
        });
        if let Some(v) = fill {
            if let Some(u) = self.dock.api.st.up.as_mut() {
                u.filled = true;
                let e = u.ver.clone();
                e.update(cx, |s, cx| s.set_value(&v, window, cx));
            }
        }
    }

    pub(crate) fn store_refresh(&mut self, cx: &mut Context<Self>) {
        self.dock.api.st.remote = 1;
        cx.spawn(async move |this, cx| {
            let r = cx.background_executor().spawn(async move { appstore::refresh_remote() }).await;
            let _ = this.update(cx, |h, cx| {
                match r {
                    Ok(()) => h.dock.api.st.remote = 2,
                    Err(e) => {
                        h.dock.api.st.remote = 3;
                        h.dock.api.st.remote_err = e;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn store_upgrade_open(&mut self, container: &str, app_id: &str, project: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(app) = appstore::current().apps.iter().find(|a| a.id == app_id).cloned() else { return };
        let dir = appstore::stack_dir(&app, project);
        let ver = cx.new(|cx| InputState::new(window, cx).placeholder("目标版本"));
        cx.observe(&ver, |_, _, cx| cx.notify()).detach();
        if let Some(d) = self.api_docker() {
            d.host_run(T_REC, appstore::read_record_script(&dir), 30);
        }
        self.api_fetch_tags(&app, cx);
        self.dock.api.st.up = Some(Upgrade { app, container: container.into(), project: project.into(), dir, rec: None, err: None, ver, filled: false });
        cx.notify();
    }

    fn store_uninstall_open(&mut self, container: &str, app_id: &str, project: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(app) = appstore::current().apps.iter().find(|a| a.id == app_id).cloned() else { return };
        let confirm = cx.new(|cx| InputState::new(window, cx).placeholder(container.to_string()));
        cx.observe(&confirm, |_, _, cx| cx.notify()).detach();
        let dir = appstore::stack_dir(&app, project);
        self.dock.api.st.un = Some(Uninstall { app, container: container.into(), dir, delete: false, confirm, err: None });
        cx.notify();
    }

    fn store_upgrade_go(&mut self, cx: &mut Context<Self>) {
        let Some(u) = self.dock.api.st.up.as_ref() else { return };
        let Some((rec, old)) = u.rec.clone() else { return };
        let target = u.ver.read(cx).value().trim().to_string();
        let app = u.app.clone();
        let dir = u.dir.clone();
        let err = appstore::check_upgrade(&app, &rec.version, &target).err().or_else(|| appstore::render(&app, &rec.values_for(&app, &target)).err());
        if let Some(e) = err {
            self.dock.api.st.up.as_mut().unwrap().err = Some(e);
            cx.notify();
            return;
        }
        let v = rec.values_for(&app, &target);
        let plan = appstore::render(&app, &v).unwrap();
        let _ = old;
        let mut rec2 = rec.clone();
        rec2.version = target.clone();
        let script = appstore::upgrade_script(&app, &plan, &rec2);
        let _ = dir;
        self.dock.api.st.up = None;
        self.api_job_raw(&app, &plan.container, String::new(), format!("升级 {}", app.name), script, Vec::new(), false, cx);
    }

    fn store_uninstall_go(&mut self, cx: &mut Context<Self>) {
        let Some(u) = self.dock.api.st.un.as_ref() else { return };
        if u.delete && u.confirm.read(cx).value().trim() != u.container {
            self.dock.api.st.un.as_mut().unwrap().err = Some("名称不一致".into());
            cx.notify();
            return;
        }
        let (app, container, dir, delete) = (u.app.clone(), u.container.clone(), u.dir.clone(), u.delete);
        match appstore::uninstall_script(&dir, &container, delete) {
            Ok(script) => {
                self.dock.api.st.un = None;
                self.api_job_raw(&app, &container, String::new(), format!("卸载 {}", app.name), script, Vec::new(), true, cx);
            }
            Err(e) => {
                self.dock.api.st.un.as_mut().unwrap().err = Some(e);
                cx.notify();
            }
        }
    }

    // ---------------------------------------------------------------- 界面

    pub(crate) fn store_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let cat = appstore::current();
        let u = &self.dock.api.st;
        let q = u.q.as_ref().map(|q| q.read(cx).value().trim().to_lowercase()).unwrap_or_default();
        let mut chips: Vec<(String, String)> = vec![("".into(), "全部".into())];
        chips.extend(cat.categories.iter().map(|c| (c.id.clone(), c.name.clone())));
        let cur = u.cat.min(chips.len() - 1);
        let sel = chips[cur].0.clone();
        // 第一行:搜索 + 目录状态;第二行:分类
        let search = u.q.clone().map(|q| {
            div().flex_1().min_w_0().h(px(34.0)).flex().items_center().px(px(10.0)).rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).bg(c::field()).child(icon(IconName::Search, 14.0, c::ghost())).child(div().flex_1().min_w_0().ml(px(6.0)).child(Input::new(&q).appearance(false).with_size(px(12.0 / 0.875))))
        });
        let row1 = div().flex_shrink_0().flex().flex_row().items_center().gap(px(12.0)).children(search);
        let row2 = div().flex_shrink_0().flex().flex_row().items_center().gap(px(4.0)).children(chips.iter().enumerate().map(|(i, (_, n))| {
            super::syspages::chip(("st-cat", i), n.clone(), i == cur).on_click(cx.listener(move |this, _, _, cx| {
                this.dock.api.st.cat = i;
                cx.notify();
            }))
        }));

        // 已安装的实例
        let mut inst_rows = Vec::new();
        for ct in self.dock.snap.containers.iter().filter(|c| c.app.is_some()) {
            let Some(app) = cat.apps.iter().find(|a| Some(&a.id) == ct.app.as_ref()) else { continue };
            let k = inst_rows.len();
            inst_rows.push(self.store_instance_row(app, ct, k, cx));
        }
        let installed = (!inst_rows.is_empty()).then(|| {
            div()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(format!("已安装 {}", inst_rows.len()))))
                .child(div().flex().flex_col().rounded(px(t::RADIUS_XL)).border_1().border_color(c::border()).children(inst_rows))
        });

        // 应用卡片:按窗口宽度算列数,每行等分,最后一行用空块补齐
        let cols = (((self.dock.view_w - 56.0) / 300.0).floor() as usize).clamp(1, 4);
        let mut cards = Vec::new();
        for (ai, a) in cat.apps.iter().enumerate() {
            if !sel.is_empty() && a.category != sel {
                continue;
            }
            if !q.is_empty() && !format!("{} {} {}", a.name, a.summary, a.id).to_lowercase().contains(&q) {
                continue;
            }
            cards.push(self.store_card(a, ai, &cat, cx));
        }
        let total = cards.len();
        let mut rows = Vec::new();
        let mut it = cards.into_iter();
        loop {
            let mut row = div().flex_shrink_0().flex().flex_row().gap(px(12.0));
            let mut n = 0;
            for c in it.by_ref().take(cols) {
                row = row.child(c);
                n += 1;
            }
            if n == 0 {
                break;
            }
            for _ in n..cols {
                row = row.child(div().flex_1().min_w_0().p(px(14.0)).border_1());
            }
            rows.push(row);
        }
        let list = if total == 0 {
            div().py(px(40.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child("没有匹配的应用").into_any_element()
        } else {
            div().flex_shrink_0().flex().flex_col().gap(px(12.0)).children(rows).into_any_element()
        };
        div().flex_1().flex().flex_col().gap(px(14.0)).child(row1).child(row2).children(installed).child(list).into_any_element()
    }

    fn store_instance_row(&self, app: &AppDef, ct: &crate::docker::Container, k: usize, cx: &mut Context<Self>) -> Div {
        let (name, app_id, project) = (ct.name.clone(), app.id.clone(), ct.project.clone().unwrap_or_else(|| ct.name.clone()));
        let (n2, a2, p2, n3, a3, p3) = (name.clone(), app_id.clone(), project.clone(), project.clone(), app_id.clone(), project.clone());
        let _ = (&n3, &a3, &p3);
        let ver = ct.image.rsplit(':').next().unwrap_or("").to_string();
        let link = |id: &'static str, text: &'static str, color: Rgba| div().id((id, k)).h(px(24.0)).px(px(8.0)).flex().items_center().rounded(px(t::RADIUS_SM)).cursor_pointer().text_size(px(t::TYPE_CAPTION)).text_color(color).hover(|s| s.bg(c::field_hover())).child(text);
        div()
            .h(px(40.0))
            .px(px(14.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .when(k > 0, |d| d.border_t_1().border_color(c::border()))
            .child(div().size(px(6.0)).flex_shrink_0().rounded_full().bg(if ct.running() { c::online() } else { c::ghost() }))
            .child(div().w(px(200.0)).flex_shrink_0().truncate().font_family(MONO).text_size(px(t::TYPE_CONTROL)).child(SharedString::from(name)))
            .child(div().w(px(120.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(SharedString::from(app.name.clone())))
            .child(div().flex_1().min_w_0().truncate().font_family(MONO).text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(ver)))
            .child(link("st-up", "升级", c::secondary()).on_click(cx.listener(move |this, _, window, cx| this.store_upgrade_open(&n2, &a2, &p2, window, cx))))
            .child(link("st-un", "卸载", c::danger()).on_click(cx.listener(move |this, _, window, cx| this.store_uninstall_open(&project, &app_id, &project.clone(), window, cx))))
    }

    fn store_card(&self, a: &AppDef, ai: usize, cat: &appstore::Catalog, cx: &mut Context<Self>) -> Stateful<Div> {
        let ic = match a.icon.as_str() {
            "database" => IconName::Database,
            "globe" => IconName::Globe,
            "activity" => IconName::Activity,
            "hard-drive" => IconName::HardDrive,
            "notebook-pen" => IconName::NotebookPen,
            _ => IconName::Package,
        };
        let n = self.dock.snap.containers.iter().filter(|c| c.app.as_deref() == Some(a.id.as_str())).count();
        let id = a.id.clone();
        let cname = cat.categories.iter().find(|c| c.id == a.category).map(|c| c.name.clone()).unwrap_or_else(|| a.category.clone());
        let pill = (n > 0).then(|| div().h(px(20.0)).px(px(8.0)).flex().items_center().gap(px(5.0)).rounded_full().bg(c::online().opacity(0.14)).text_size(px(t::TYPE_MICRO)).text_color(c::online()).child(SharedString::from(format!("已安装 {n}"))));
        div()
            .id(("st-card", ai))
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(14.0))
            .rounded(px(t::RADIUS_XL))
            .border_1()
            .border_color(c::border())
            .cursor_pointer()
            .hover(|s| s.bg(c::field_hover()).border_color(c::field_border()))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(div().size(px(36.0)).flex_shrink_0().flex().items_center().justify_center().rounded(px(t::RADIUS_MD)).bg(c::field()).child(icon(ic, 18.0, c::secondary())))
                    .child(div().flex_1().min_w_0().flex().flex_col().child(div().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(SharedString::from(a.name.clone()))).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(cname))))
                    .children(pill),
            )
            .child(div().h(px(34.0)).overflow_hidden().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(SharedString::from(a.summary.clone())))
            .child(
                div().flex().flex_row().justify_end().child(
                    div()
                        .h(px(26.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .rounded(px(t::RADIUS_MD))
                        .border_1()
                        .border_color(c::field_border())
                        .text_size(px(t::TYPE_CAPTION))
                        .text_color(c::secondary())
                        .child(icon(IconName::Plus, 12.0, c::muted()))
                        .child(if n == 0 { "安装" } else { "再装一个" }),
                ),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.api_open(&id, window, cx)))
    }

    /// 升级、卸载的确认框
    pub(crate) fn store_dialogs(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v = Vec::new();
        if let Some(u) = &self.dock.api.st.up {
            let title = format!("升级 {}({})", u.app.name, u.container);
            let (cur, body): (String, AnyElement) = match (&u.rec, &u.err) {
                (_, Some(e)) => (String::new(), div().text_size(px(t::TYPE_CONTROL)).text_color(c::danger()).child(SharedString::from(e.clone())).into_any_element()),
                (None, _) => (String::new(), div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("正在读取实例记录…").into_any_element()),
                (Some((rec, old)), _) => {
                    let target = u.ver.read(cx).value().trim().to_string();
                    let diff: Vec<(char, String)> = appstore::render(&u.app, &rec.values_for(&u.app, &target)).map(|p| appstore::diff_lines(old, &p.compose).into_iter().filter(|(k, _)| *k != ' ').collect()).unwrap_or_default();
                    let lines = diff.into_iter().map(|(k, l)| div().whitespace_nowrap().text_color(if k == '+' { c::online() } else { c::danger() }).child(SharedString::from(format!("{k} {l}"))));
                    (
                        rec.version.clone(),
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .child(field_col("目标版本", None, div().flex_1().min_w_0().child(Input::new(&u.ver).appearance(false).with_size(px(12.0 / 0.875))).into_any_element(), true, false))
                            .child(div().p(px(10.0)).rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).bg(c::field()).font_family(MONO).text_size(px(t::TYPE_CAPTION)).children(lines))
                            .into_any_element(),
                    )
                }
            };
            let ready = u.rec.is_some();
            let content = div()
                .p(px(20.0))
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(SharedString::from(title)))
                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(if cur.is_empty() { String::new() } else { format!("当前版本 {cur};会拉取新镜像并重建容器,失败时自动还原配置(compose.yml.bak)") })))
                .child(body)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(crate::node_form::ctl_btn("st-up-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
                            this.dock.api.st.up = None;
                            cx.notify();
                        })))
                        .children(ready.then(|| crate::node_form::ctl_btn("st-up-go", None, "升级", 34.0).on_click(cx.listener(|this, _, _, cx| this.store_upgrade_go(cx))))),
                );
            let _ = &u.project;
            v.push(crate::ui::modal("st-up-dialog", 560.0, |_, _, _| {}, [content.into_any_element()]).into_any_element());
        }
        if let Some(u) = &self.dock.api.st.un {
            let name = u.container.clone();
            let delete = u.delete;
            let content = div()
                .p(px(20.0))
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(SharedString::from(format!("卸载 {}({})", u.app.name, name))))
                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child("会停止并删除容器。默认保留数据卷和堆栈目录,以后可以原样装回来。"))
                .child(
                    div()
                        .id("st-un-del")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .cursor_pointer()
                        .child(Self::pill_switch("st-un-switch", delete))
                        .child(div().text_size(px(t::TYPE_CONTROL)).child("同时删除数据(数据卷和堆栈目录,不能恢复)"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(u) = this.dock.api.st.un.as_mut() {
                                u.delete = !u.delete;
                            }
                            cx.notify();
                        })),
                )
                .children(delete.then(|| field_col(format!("输入 {name} 确认"), None, div().flex_1().min_w_0().child(Input::new(&u.confirm).appearance(false).with_size(px(12.0 / 0.875))).into_any_element(), true, false)))
                .children(u.err.clone().map(|e| div().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child(SharedString::from(e))))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(crate::node_form::ctl_btn("st-un-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
                            this.dock.api.st.un = None;
                            cx.notify();
                        })))
                        .child(crate::node_form::ctl_btn("st-un-go", None, "卸载", 34.0).on_click(cx.listener(|this, _, _, cx| this.store_uninstall_go(cx)))),
                );
            v.push(crate::ui::modal("st-un-dialog", 480.0, |_, _, _| {}, [content.into_any_element()]).into_any_element());
        }
        v
    }
}

/// 卸载用的容器名:Otter 装的应用堆栈名和容器名相同;这里取堆栈名
fn name_of(project: &str, _app: &str) -> String {
    project.to_string()
}
