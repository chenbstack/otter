//! 应用商店页:应用目录的卡片列表、搜索和分类;已安装实例的升级、卸载。
//! 安装走统一的抽屉(`installui.rs`),升级和卸载的任务也在那个抽屉里显示进度。

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};

use super::super::*;
use super::composeedit::field_col;
use crate::appstore::{self, App as AppDef, Record};
use crate::appupdate::{self, Status};

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
    /// 「可升级」检查给出的目标版本(预填用)和「同一个 tag 重新拉取」(浮动标签有新镜像)
    suggest: Option<String>,
    repull: bool,
    /// 目标版本下拉是否展开;选了「自定义版本号…」后显示输入框
    open: bool,
    custom: bool,
}

/// 一个已装实例的升级检查进度(见 `appupdate`)
#[derive(Default)]
pub(crate) struct Upd {
    status: Option<Status>,
    asked: bool,
    /// 检查时实例用的 tag;实例的 tag 变了(升级过)结果就作废
    tag: String,
    local: Option<String>,
    hub: Option<Option<std::collections::HashSet<String>>>,
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
    /// 进入商店页的时间(离开页面清掉):为空就是刚进来,要拉一次在线目录
    seen: Option<std::time::Instant>,
    up: Option<Upgrade>,
    un: Option<Uninstall>,
    /// 各已装实例(容器名)的升级检查
    upd: std::collections::HashMap<String, Upd>,
    checked_at: Option<std::time::Instant>,
    /// 后台轮询检查结果的任务在跑
    polling: bool,
    /// 正在看详情的应用 id
    detail: Option<String>,
}

const T_REC: &str = "apprec";

impl Home {
    /// 每次渲染前:商店页第一次出现时去拉在线目录;收升级需要的实例记录
    pub(crate) fn store_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Store {
            // 离开了商店页:下次进来要重新拉一次在线目录
            self.dock.api.st.seen = None;
            return;
        }
        if self.dock.api.st.q.is_none() {
            let q = cx.new(|cx| InputState::new(window, cx).placeholder("搜索应用"));
            cx.observe(&q, |_, _, cx| cx.notify()).detach();
            self.dock.api.st.q = Some(q);
            // 截图用:OTTER_STORE_DETAIL=<应用 id> 直接打开详情抽屉
            self.dock.api.st.detail = std::env::var("OTTER_STORE_DETAIL").ok();
            if let Ok(m) = std::env::var("OTTER_SHOT_JOB") {
                self.api_demo_job(&m);
            }
        }
        // 目录每天从仓库更新一次(启动时和后台定时,见 appstore::start_daily_refresh);进入商店页时只在缓存过期或上次失败时补拉一次。拉取期间继续显示已有目录
        let entered = self.dock.api.st.seen.is_none();
        self.dock.api.st.seen = Some(std::time::Instant::now());
        if entered && self.dock.api.st.remote != 1 && !self.demo && (self.dock.api.st.remote == 3 || appstore::cache_stale()) {
            self.store_refresh(cx);
        }
        // 检查结果缓存 10 分钟,过期后进商店页再重新检查(旧结果和缓存的版本列表都丢掉)
        if entered && self.dock.api.st.checked_at.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(600)) {
            self.dock.api.st.checked_at = Some(std::time::Instant::now());
            self.dock.api.st.upd.clear();
            let ids: Vec<String> = self.dock.snap.containers.iter().filter_map(|c| c.app.clone()).collect();
            for id in ids {
                self.dock.api.forget_tags(&id);
            }
        }
        self.store_check_updates(cx);
        // 检查结果到达时页面不一定会重绘:有没出结果的就每 0.5 秒推进一次,最多 60 秒
        let pending = self.dock.snap.containers.iter().any(|c| c.app.is_some() && self.dock.api.st.upd.get(&c.name).is_none_or(|u| u.status.is_none()));
        if pending && !self.dock.api.st.polling && !self.demo {
            self.dock.api.st.polling = true;
            cx.spawn(async move |this, cx| {
                for _ in 0..120 {
                    cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                    let done = this
                        .update(cx, |h, cx| {
                            h.store_check_updates(cx);
                            cx.notify();
                            !h.dock.snap.containers.iter().any(|c| c.app.is_some() && h.dock.api.st.upd.get(&c.name).is_none_or(|u| u.status.is_none()))
                        })
                        .unwrap_or(true);
                    if done {
                        break;
                    }
                }
                let _ = this.update(cx, |h, _| h.dock.api.st.polling = false);
            })
            .detach();
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
            if u.repull {
                return Some(rec.version.clone());
            }
            if let Some(s) = &u.suggest {
                return Some(s.clone());
            }
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

    /// 逐个已装实例做升级检查:版本列表到了先按 tag 判断,浮动标签再比镜像 digest
    fn store_check_updates(&mut self, cx: &mut Context<Self>) {
        if self.demo {
            return;
        }
        let cat = appstore::current();
        let list: Vec<crate::docker::Container> = self.dock.snap.containers.iter().filter(|c| c.app.is_some()).cloned().collect();
        for ct in list {
            let Some(app) = cat.apps.iter().find(|a| Some(&a.id) == ct.app.as_ref()).cloned() else { continue };
            let tag = ct.image.rsplit_once(':').map(|x| x.1.to_string()).unwrap_or_else(|| "latest".into());
            let name = ct.name.clone();
            if self.dock.api.st.upd.get(&name).is_some_and(|u| u.tag != tag) {
                self.dock.api.st.upd.remove(&name);
            }
            self.dock.api.st.upd.entry(name.clone()).or_default().tag.clone_from(&tag);
            if self.dock.api.st.upd.get(&name).is_some_and(|u| u.status.is_some()) {
                continue;
            }
            // 版本列表走统一入口(24 小时内更新过用本地的,否则用 Hub 补最新)
            self.api_fetch_tags(&app, cx);
            let Some(versions) = self.dock.api.tags_of(&app.id) else {
                if self.dock.api.tags_failed(&app.id) {
                    self.dock.api.st.upd.entry(name).or_default().status = Some(Status::Unknown);
                }
                continue;
            };
            let key = format!("stdig-{name}");
            let u = self.dock.api.st.upd.entry(name.clone()).or_default();
            if !u.asked {
                match appupdate::check_tag(&tag, &versions) {
                    Status::NeedDigest => {
                        u.asked = true;
                        let Some((repo, _, _)) = appstore::hub_pages(&app) else {
                            u.status = Some(Status::Unknown);
                            continue;
                        };
                        if let Some(d) = self.api_docker() {
                            d.host_run(&key, appupdate::local_digest_script(&ct.image), 30);
                        }
                        let (n2, t2) = (name.clone(), tag.clone());
                        cx.spawn(async move |this, cx| {
                            let r = cx.background_executor().spawn(async move { appupdate::fetch_hub_digests(&repo, &t2) }).await;
                            let _ = this.update(cx, |h, cx| {
                                if let Some(u) = h.dock.api.st.upd.get_mut(&n2) {
                                    u.hub = Some(r);
                                }
                                cx.notify();
                            });
                        })
                        .detach();
                    }
                    st => {
                        u.status = Some(st);
                        u.asked = true;
                    }
                }
                continue;
            }
            if u.local.is_none() {
                if let Some(r) = self.api_docker().and_then(|d| d.db_take(&key)) {
                    self.dock.api.st.upd.entry(name.clone()).or_default().local = Some(r.unwrap_or_default());
                }
            }
            let u = self.dock.api.st.upd.entry(name).or_default();
            if let (Some(local), Some(hub)) = (&u.local, &u.hub) {
                u.status = Some(match hub {
                    Some(h) => appupdate::check_digest(local, h),
                    None => Status::Unknown,
                });
                cx.notify();
            }
        }
    }

    /// 某个实例的升级检查结果作废(装/升/卸之后)
    pub(crate) fn store_forget_check(&mut self, container: &str) {
        self.dock.api.st.upd.remove(container);
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
        let st = self.dock.api.st.upd.get(container).and_then(|u| u.status.clone());
        let (suggest, repull) = match st {
            Some(Status::NewTag(t)) => (Some(t), false),
            Some(Status::NewImage) => (None, true),
            _ => (None, false),
        };
        self.dock.api.st.up = Some(Upgrade { app, container: container.into(), project: project.into(), dir, rec: None, err: None, ver, filled: false, suggest, repull, open: false, custom: false });
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
        let repull = self.dock.api.st.up.as_ref().is_some_and(|u| u.repull) && target == rec.version;
        let err = (if repull { Ok(()) } else { appstore::check_upgrade(&app, &rec.version, &target) }).err().or_else(|| appstore::render(&app, &rec.values_for(&app, &target)).err());
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
        // 已装实例及其升级状态(「已安装」「可升级」页签用)
        let installed: Vec<(&AppDef, &crate::docker::Container)> = self.dock.snap.containers.iter().filter(|c| c.app.is_some()).filter_map(|ct| cat.apps.iter().find(|a| Some(&a.id) == ct.app.as_ref()).map(|a| (a, ct))).collect();
        let upgradable = |ct: &crate::docker::Container| matches!(self.dock.api.st.upd.get(&ct.name).and_then(|u| u.status.as_ref()), Some(Status::NewTag(_) | Status::NewImage));
        let up_n = installed.iter().filter(|(_, ct)| upgradable(ct)).count();
        let mut chips: Vec<(String, String)> = vec![("".into(), "全部".into())];
        chips.push(("@installed".into(), if installed.is_empty() { "已安装".into() } else if up_n > 0 { format!("已安装 {} · {up_n} 个可升级", installed.len()) } else { format!("已安装 {}", installed.len()) }));
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

        // 已安装:实例列表(搜索对容器名、应用名生效),不分类;后台静默检查,有新版本的排在最上面
        if sel == "@installed" {
            let mut rows = Vec::new();
            let mut ordered: Vec<&(&AppDef, &crate::docker::Container)> = installed.iter().collect();
            ordered.sort_by_key(|(_, ct)| !upgradable(ct));
            for (app, ct) in ordered {
                if !q.is_empty() && !format!("{} {} {}", ct.name, app.name, app.id).to_lowercase().contains(&q) {
                    continue;
                }
                let k = rows.len();
                rows.push(self.store_instance_row(app, ct, k, cx));
            }
            let body: AnyElement = if rows.is_empty() {
                let msg = if installed.is_empty() { "还没有用商店安装的应用" } else { "没有匹配的实例" };
                div().py(px(40.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(msg).into_any_element()
            } else {
                div().flex().flex_col().gap(px(10.0)).children(rows).into_any_element()
            };
            return div().flex_1().flex().flex_col().gap(px(14.0)).child(row1).child(row2).child(body).into_any_element();
        }

        // 应用卡片:按窗口宽度算列数,每行等分,最后一行用空块补齐
        let cols = (((self.dock.view_w - 56.0) / 300.0).floor() as usize).clamp(1, 4);
        let mut cards = Vec::new();
        for (ai, a) in cat.apps.iter().enumerate() {
            if !a.supports(&self.dock.snap.arch) {
                continue;
            }
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
        div().flex_1().flex().flex_col().gap(px(14.0)).child(row1).child(row2).child(list).into_any_element()
    }

    /// 已安装列表的一行:图标、应用名+容器名、版本(有更新时 `当前 → 新版本`)、状态、端口、操作
    fn store_instance_row(&self, app: &AppDef, ct: &crate::docker::Container, k: usize, cx: &mut Context<Self>) -> Div {
        let (name, app_id, project) = (ct.name.clone(), app.id.clone(), ct.project.clone().unwrap_or_else(|| ct.name.clone()));
        let (n2, a2, p2) = (name.clone(), app_id.clone(), project.clone());
        let ver = ct.image.rsplit_once(':').map(|x| x.1.to_string()).unwrap_or_else(|| "latest".into());
        let status = self.dock.api.st.upd.get(&ct.name).and_then(|u| u.status.clone());
        let upgradable = matches!(status, Some(Status::NewTag(_) | Status::NewImage));
        let ic = match app.icon.as_str() {
            "database" => IconName::Database,
            "globe" => IconName::Globe,
            "activity" => IconName::Activity,
            "hard-drive" => IconName::HardDrive,
            "notebook-pen" => IconName::NotebookPen,
            _ => IconName::Package,
        };
        let mono = |s: String, color: Rgba| div().truncate().font_family(MONO).text_size(px(t::TYPE_CAPTION)).text_color(color).child(SharedString::from(s));
        let ver_cell = match &status {
            Some(Status::NewTag(n)) => div().flex().flex_row().items_center().gap(px(6.0)).child(mono(ver, c::muted())).child(icon(IconName::ArrowRight, 12.0, c::accent())).child(mono(n.clone(), c::accent())),
            Some(Status::NewImage) => div().flex().flex_row().items_center().gap(px(6.0)).child(mono(ver, c::muted())).child(icon(IconName::ArrowRight, 12.0, c::accent())).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::accent()).child("新镜像")),
            _ => div().flex().flex_row().items_center().child(mono(ver, c::muted())),
        };
        // 这个实例正在跑升级/卸载任务:对应按钮显示加载动画
        let job = self.dock.api.job_of(&ct.name);
        let (up_busy, un_busy) = (job.is_some_and(|t| t.starts_with("升级")), job.is_some_and(|t| t.starts_with("卸载")));
        let (st_text, st_color) = if ct.running() { ("运行中", c::online()) } else { ("已停止", c::muted()) };
        let port = ct.ports.first().and_then(|p| p.split("->").next()).map(str::to_string).unwrap_or_default();
        let up_btn = div()
            .id(("st-up", k))
            .h(px(28.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .rounded(px(t::RADIUS_MD))
            .cursor_pointer()
            .text_size(px(t::TYPE_CONTROL))
            .when(upgradable, |d| d.bg(c::accent()).text_color(c::on_accent()).font_weight(FontWeight::SEMIBOLD).hover(|s| s.opacity(0.88)))
            .when(!upgradable, |d| d.text_color(c::secondary()).hover(|s| s.bg(c::field_hover()).text_color(c::text())))
            .gap(px(6.0))
            .when(up_busy, |d| d.child(crate::ui::dot_spinner(("st-up-spin", k), if upgradable { c::on_accent() } else { c::muted() })))
            .child(if upgradable { "升级" } else { "更改版本" })
            .when(!(up_busy || un_busy), |d| d.on_click(cx.listener(move |this, _, window, cx| this.store_upgrade_open(&n2, &a2, &p2, window, cx))));
        let un_btn = div()
            .id(("st-un", k))
            .h(px(28.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .rounded(px(t::RADIUS_MD))
            .cursor_pointer()
            .text_size(px(t::TYPE_CONTROL))
            .text_color(c::danger())
            .hover(|s| s.bg(c::field_hover()))
            .gap(px(6.0))
            .when(un_busy, |d| d.child(crate::ui::dot_spinner(("st-un-spin", k), c::danger())))
            .child("卸载")
            .when(!(up_busy || un_busy), |d| d.on_click(cx.listener(move |this, _, window, cx| this.store_uninstall_open(&project, &app_id, &project.clone(), window, cx))));
        div()
            .flex_shrink_0()
            .h(px(64.0))
            .px(px(14.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.0))
            .rounded(px(t::RADIUS_XL))
            .border_1()
            .border_color(if upgradable { c::accent().opacity(0.45) } else { c::border() })
            .child(div().size(px(36.0)).flex_shrink_0().flex().items_center().justify_center().rounded(px(t::RADIUS_MD)).bg(c::field()).border_1().border_color(c::field_border()).child(icon(ic, 18.0, c::secondary())))
            .child(
                div()
                    .w(px(240.0))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(div().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(app.name.clone())))
                    .child(mono(name, c::ghost())),
            )
            .child(div().flex_1().min_w(px(180.0)).child(ver_cell))
            .child(div().w(px(84.0)).flex_shrink_0().flex().flex_row().items_center().gap(px(6.0)).child(div().size(px(6.0)).rounded_full().bg(st_color)).child(div().text_size(px(t::TYPE_CAPTION)).text_color(st_color).child(st_text)))
            .child(div().w(px(140.0)).flex_shrink_0().child(mono(port, c::ghost())))
            .child(div().w(px(170.0)).flex_shrink_0().flex().flex_row().items_center().justify_end().gap(px(6.0)).child(up_btn).child(un_btn))
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
        let id2 = a.id.clone();
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
                        .id(("st-inst", ai))
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
                        .cursor_pointer()
                        .hover(|s| s.bg(c::field_hover()))
                        .child(icon(IconName::Plus, 12.0, c::muted()))
                        .child(if n == 0 { "安装" } else { "再装一个" })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.api_open(&id2, window, cx)
                        })),
                ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.dock.api.st.detail = Some(id.clone());
                cx.notify();
            }))
    }

    /// 升级、卸载的确认框
    /// 应用详情抽屉:介绍、主要功能、官网和开源地址
    fn store_detail(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.dock.api.st.detail.clone()?;
        let cat = appstore::current();
        let a = cat.apps.iter().find(|a| a.id == id)?.clone();
        let cname = cat.categories.iter().find(|c| c.id == a.category).map(|c| c.name.clone()).unwrap_or_else(|| a.category.clone());
        let ic = match a.icon.as_str() {
            "database" => IconName::Database,
            "globe" => IconName::Globe,
            "activity" => IconName::Activity,
            "hard-drive" => IconName::HardDrive,
            "notebook-pen" => IconName::NotebookPen,
            _ => IconName::Package,
        };
        let close = |this: &mut Self, cx: &mut Context<Self>| {
            this.dock.api.st.detail = None;
            cx.notify();
        };
        let tag = |t: String| div().h(px(20.0)).px(px(8.0)).flex().items_center().rounded(px(t::RADIUS_SM)).bg(c::idle_soft()).font_family(MONO).text_size(px(t::TYPE_MICRO)).text_color(c::muted()).child(SharedString::from(t));
        let mut tags = vec![tag(cname)];
        if !a.arch.is_empty() {
            tags.push(tag(a.arch.join(" / ")));
        }
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
            .child(div().flex_1().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(SharedString::from(a.name.clone())))
            .child(super::syspages::tip(super::dockerpages::icon_btn(("st-d-x", 0), IconName::X, c::muted()).on_click(cx.listener(move |this, _, _, cx| close(this, cx))), "关闭"));
        let section = |title: &'static str| div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).child(title);
        let intro = if a.description.is_empty() { a.summary.clone() } else { a.description.clone() };
        let mut body = div().id("st-detail-body").flex_1().min_h_0().overflow_y_scroll().p(px(16.0)).flex().flex_col().gap(px(16.0));
        body = body.child(div().flex().flex_row().gap(px(6.0)).children(tags));
        body = body.child(div().flex().flex_col().gap(px(6.0)).child(section("介绍")).child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).line_height(px(20.0)).child(SharedString::from(intro))));
        if !a.features.is_empty() {
            body = body.child(
                div().flex().flex_col().gap(px(6.0)).child(section("主要功能")).children(a.features.iter().map(|f| {
                    div().flex().flex_row().gap(px(8.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(div().mt(px(8.0)).size(px(4.0)).flex_shrink_0().rounded_full().bg(c::ghost())).child(div().flex_1().min_w_0().child(SharedString::from(f.clone())))
                })),
            );
        }
        let links: Vec<(&'static str, &'static str, IconName, String)> = [("st-d-web", "官网", IconName::Globe, a.website.clone()), ("st-d-repo", "开源项目", IconName::ExternalLink, a.repo.clone())].into_iter().filter_map(|(i, l, ic, u)| u.map(|u| (i, l, ic, u))).collect();
        if !links.is_empty() {
            body = body.child(div().flex().flex_col().gap(px(6.0)).child(section("链接")).children(links.into_iter().map(|(i, l, ic, u)| {
                let u2 = u.clone();
                div()
                    .id(i)
                    .h(px(28.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .rounded(px(t::RADIUS_SM))
                    .hover(|s| s.bg(c::field_hover()))
                    .child(icon(ic, 13.0, c::muted()))
                    .child(div().w(px(56.0)).flex_shrink_0().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(l))
                    .child(div().flex_1().min_w_0().truncate().font_family(MONO).text_size(px(t::TYPE_CAPTION)).text_color(c::secondary()).child(SharedString::from(u)))
                    .on_click(move |_, _, _| crate::update::open_page(&u2))
            })));
        }
        let aid = a.id.clone();
        let n = self.dock.snap.containers.iter().filter(|c| c.app.as_deref() == Some(a.id.as_str())).count();
        let foot = div().h(px(56.0)).flex_shrink_0().flex().flex_row().items_center().justify_end().px(px(16.0)).border_t_1().border_color(c::border()).child(
            div()
                .id("st-d-install")
                .h(px(34.0))
                .px(px(18.0))
                .flex()
                .items_center()
                .rounded(px(t::RADIUS_MD))
                .bg(c::accent())
                .text_size(px(t::TYPE_CONTROL))
                .text_color(c::on_accent())
                .cursor_pointer()
                .hover(|s| s.opacity(0.9))
                .child(if n == 0 { "安装" } else { "再装一个" })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.dock.api.st.detail = None;
                    this.api_open(&aid, window, cx);
                })),
        );
        Some(
            crate::ui::drawer("st-detail", 520.0, cx.listener(move |this, _, _, cx| close(this, cx)), [head.into_any_element(), body.into_any_element(), foot.into_any_element()])
                .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                    if e.keystroke.key == "escape" {
                        this.dock.api.st.detail = None;
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .into_any_element(),
        )
    }

    pub(crate) fn store_dialogs(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v = Vec::new();
        v.extend(self.store_detail(cx));
        if let Some(u) = &self.dock.api.st.up {
            let title = format!("升级 {}({})", u.app.name, u.container);
            let (cur, body): (String, AnyElement) = match (&u.rec, &u.err) {
                (_, Some(e)) => (String::new(), div().text_size(px(t::TYPE_CONTROL)).text_color(c::danger()).child(SharedString::from(e.clone())).into_any_element()),
                (None, _) => (String::new(), div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("正在读取实例记录…").into_any_element()),
                (Some((rec, old)), _) => {
                    let target = u.ver.read(cx).value().trim().to_string();
                    let diff: Vec<(char, String)> = appstore::render(&u.app, &rec.values_for(&u.app, &target)).map(|p| appstore::diff_lines(old, &p.compose).into_iter().filter(|(k, _)| *k != ' ').collect()).unwrap_or_default();
                    let empty = diff.is_empty();
                    let list = self.dock.api.tags_of(&u.app.id);
                    let mut items: Vec<crate::ui::MenuItem> = match &list {
                        Some(l) => l.iter().map(|v| crate::ui::MenuItem::new(v.clone())).collect(),
                        None => vec![crate::ui::MenuItem::new("正在获取版本列表…")],
                    };
                    let n = list.as_ref().map_or(0, |l| l.len());
                    if list.is_some() {
                        items.push(crate::ui::MenuItem::new("自定义版本号…"));
                    }
                    let sel = if u.custom { n } else { list.as_ref().and_then(|l| l.iter().position(|v| *v == target)).unwrap_or(n) };
                    let (e1, e2, e3, ver_in) = (cx.entity(), cx.entity(), cx.entity(), u.ver.clone());
                    let ver_select = crate::ui::select_sized(
                        34.0,
                        "st-up-ver",
                        IconName::Tag,
                        items,
                        sel,
                        u.open && list.is_some(),
                        move |_, cx| {
                            e1.update(cx, |h, cx| {
                                if let Some(u) = h.dock.api.st.up.as_mut() {
                                    u.open = !u.open;
                                }
                                cx.notify();
                            })
                        },
                        move |i, window, cx| {
                            let l = e2.read(cx).dock.api.tags_of(&e2.read(cx).dock.api.st.up.as_ref().map(|u| u.app.id.clone()).unwrap_or_default()).unwrap_or_default();
                            e2.update(cx, |h, cx| {
                                if let Some(u) = h.dock.api.st.up.as_mut() {
                                    u.open = false;
                                    u.custom = i >= l.len();
                                    if let Some(v) = l.get(i) {
                                        ver_in.update(cx, |s, cx| s.set_value(v, window, cx));
                                    }
                                }
                                cx.notify();
                            })
                        },
                        move |_, cx| {
                            e3.update(cx, |h, cx| {
                                if let Some(u) = h.dock.api.st.up.as_mut() {
                                    u.open = false;
                                }
                                cx.notify();
                            })
                        },
                        list.is_none().then(|| crate::ui::dot_spinner(("st-up-ver-load", 0), c::muted()).into_any_element()),
                    );
                    let lines = diff.into_iter().map(|(k, l)| div().whitespace_nowrap().text_color(if k == '+' { c::online() } else { c::danger() }).child(SharedString::from(format!("{k} {l}"))));
                    (
                        rec.version.clone(),
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .child(div().flex().flex_col().gap(px(6.0)).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child("目标版本")).child(div().h(px(34.0)).child(ver_select)))
                            .children(u.custom.then(|| field_col("自定义版本号", None, div().flex_1().min_w_0().child(Input::new(&u.ver).appearance(false).with_size(px(12.0 / 0.875))).into_any_element(), true, false)))
                            .child(if empty {
                                div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(if target == rec.version { "和当前版本相同,配置没有变化" } else { "配置没有变化" }).into_any_element()
                            } else {
                                div().p(px(10.0)).rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).bg(c::field()).font_family(MONO).text_size(px(t::TYPE_CAPTION)).children(lines).into_any_element()
                            })
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
