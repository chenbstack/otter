//! 数据库页的实例管理:选中一个数据库实例(按端口区分),直接显示它里面的库、用户(Redis 是运行信息)。
//! 所有读取都在容器里执行(见 `dbinst`),结果按 tag 从执行层取;密码不经过 Otter。

use super::super::*;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::Sizable as _;
use super::dockerpages::{head, loading, row, small, state_color, table_flow};
use crate::dbinst::{self, DbInstance, DbKind, DbRow, RedisInfo, UserRow};

type Loaded<T> = Option<Result<T, String>>;

#[derive(Default)]
pub(crate) struct DbMgr {
    /// 当前选中的容器名
    cur: String,
    /// 0 = 数据库,1 = 用户
    sub: usize,
    dbs: Loaded<Vec<DbRow>>,
    users: Loaded<Vec<UserRow>>,
    redis: Loaded<RedisInfo>,
    /// 管理员账号名(取自容器环境变量;不读密码)
    cred: Option<String>,
    /// 新建数据库抽屉
    form: Option<DbForm>,
    busy: bool,
    form_err: Option<String>,
    /// 提交中的 (库名, 用户名, 密码):成功后弹一次,密码只显示这一次
    pending: Option<(String, String, String)>,
    done: Option<(String, String, String)>,
    /// 等用户确认的重置密码 (用户名, 主机)
    reset_ask: Option<(String, String)>,
    reset_err: Option<String>,
    /// 等用户确认删除的库名
    drop_ask: Option<String>,
    /// 已发出还没收到结果的请求 tag
    inflight: Vec<String>,
}

/// 新建数据库抽屉里的输入
#[derive(Clone)]
struct DbForm {
    name: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    desc: Entity<TextareaState>,
    superuser: bool,
    show_pw: bool,
}

const SUBS: [&str; 2] = ["数据库", "用户"];

impl DbMgr {
    fn reset(&mut self, cur: &str) {
        *self = DbMgr { cur: cur.to_string(), ..Default::default() };
    }
}

impl Home {
    fn dbm_docker(&self) -> Option<&crate::docker::Docker> {
        self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref())
    }

    /// 要读的东西(tag, SQL/命令);已经有结果或在路上的不再发
    fn dbm_request(&mut self, inst: &DbInstance) {
        let m = &self.dock.dbm;
        let (what, have, sql) = match (inst.kind, m.sub) {
            (DbKind::Redis, _) => ("redis", m.redis.is_some(), "INFO\n"),
            (k, 0) => ("dbs", m.dbs.is_some(), k.list_dbs_sql()),
            (k, _) => ("users", m.users.is_some(), k.list_users_sql()),
        };
        // 管理员密码一并读(Redis 没有)
        if m.cred.is_none() && !m.inflight.iter().any(|t| t.starts_with("dbm:cred:")) && inst.container.running() {
            if let (Some(args), Some(d)) = (inst.kind.admin_args(&inst.container.name), self.dbm_docker()) {
                let tag = format!("dbm:cred:{}", m.cur);
                d.db_run(&tag, args, None);
                self.dock.dbm.inflight.push(tag);
            }
        }
        let m = &self.dock.dbm;
        let tag = format!("dbm:{what}:{}", m.cur);
        if have || m.inflight.contains(&tag) || !inst.container.running() {
            return;
        }
        if let Some(d) = self.dbm_docker() {
            d.db_run(&tag, inst.kind.exec_args(&inst.container.name), Some(sql.to_string()));
            self.dock.dbm.inflight.push(tag);
        }
    }

    fn dbm_poll(&mut self, kind: DbKind) {
        let tags = self.dock.dbm.inflight.clone();
        for tag in tags {
            let Some(r) = self.dbm_docker().and_then(|d| d.db_take(&tag)) else { continue };
            let m = &mut self.dock.dbm;
            m.inflight.retain(|t| *t != tag);
            if tag.starts_with("dbm:cred:") {
                m.cred = r.ok().map(|o| o.trim().to_string()).filter(|u| !u.is_empty());
                continue;
            }
            let r = r.map_err(|e| dbinst::friendly_error(kind, &e));
            if tag.starts_with("dbm:drop:") {
                m.busy = false;
                match r {
                    Ok(_) => m.dbs = None,
                    Err(e) => m.reset_err = Some(e),
                }
                continue;
            }
            if tag.starts_with("dbm:reset:") {
                m.busy = false;
                match r {
                    Ok(_) => m.done = m.pending.take(),
                    Err(e) => {
                        m.pending = None;
                        m.reset_err = Some(e);
                    }
                }
                continue;
            }
            if tag.starts_with("dbm:create:") {
                m.busy = false;
                match r {
                    Ok(_) => {
                        m.form = None;
                        m.form_err = None;
                        m.dbs = None;
                        m.users = None;
                        m.done = m.pending.take();
                    }
                    Err(e) => m.form_err = Some(e),
                }
                continue;
            }
            match tag.split(':').nth(1) {
                Some("dbs") => m.dbs = Some(r.map(|o| dbinst::parse_dbs(kind, &o))),
                Some("users") => m.users = Some(r.map(|o| dbinst::parse_users(kind, &o))),
                _ => m.redis = Some(r.map(|o| dbinst::parse_redis(&o))),
            }
        }
    }

    pub(crate) fn dbm_page(&mut self, list: Vec<DbInstance>, cx: &mut Context<Self>) -> AnyElement {
        if !list.iter().any(|d| d.container.name == self.dock.dbm.cur) {
            let first = list[0].container.name.clone();
            self.dock.dbm.reset(&first);
        }
        let inst = list.iter().find(|d| d.container.name == self.dock.dbm.cur).cloned().unwrap_or_else(|| list[0].clone());
        if self.demo {
            self.dbm_demo(&inst);
        } else {
            self.dbm_poll(inst.kind);
            self.dbm_request(&inst);
        }

        let mut page = div().flex_1().min_h_0().pb(px(14.0)).flex().flex_col().gap(px(12.0));
        // 同一类型有多个实例时,按端口切换
        if list.len() > 1 {
            let mut chips = div().flex().flex_row().flex_wrap().gap(px(6.0));
            for (i, d) in list.iter().enumerate() {
                let on = d.container.name == inst.container.name;
                let name = d.container.name.clone();
                chips = chips.child(
                    div()
                        .id(("dbm-inst", i))
                        .h(px(28.0))
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(t::RADIUS_MD))
                        .cursor_pointer()
                        .text_size(px(t::TYPE_CONTROL))
                        .border_1()
                        .border_color(if on { c::accent() } else { c::border() })
                        .text_color(if on { c::text() } else { c::muted() })
                        .child(div().size(px(7.0)).rounded_full().bg(state_color(&d.container).0))
                        .child(inst_port(d))
                        .child(small(d.container.name.clone(), c::ghost()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.dock.dbm.reset(&name);
                            cx.notify();
                        })),
                );
            }
            page = page.child(chips);
        }


        if !inst.container.running() {
            return page.child(empty(IconName::Database, "容器没有运行").desc("先在「容器」页启动它,才能读取里面的数据").warn()).into_any_element();
        }

        if inst.kind == DbKind::Redis {
            return page.child(self.dbm_redis()).into_any_element();
        }

        // 数据库 / 用户
        let mut tabs = div().flex().flex_row().gap(px(4.0));
        for (i, name) in SUBS.iter().enumerate() {
            let on = self.dock.dbm.sub == i;
            tabs = tabs.child(
                div()
                    .id(("dbm-sub", i))
                    .h(px(28.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .rounded(px(t::RADIUS_MD))
                    .text_size(px(t::TYPE_CONTROL))
                    .when(on, |d| d.bg(c::field()).text_color(c::text()))
                    .when(!on, |d| d.text_color(c::muted()).hover(|s| s.bg(c::field_hover())))
                    .child(*name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.dock.dbm.sub = i;
                        cx.notify();
                    })),
            );
        }
        let tabs = tabs.child(div().flex_1()).child(button("dbm-new-db", IconName::Plus, "新建数据库").on_click(cx.listener(|this, _, window, cx| this.dbm_open_form(window, cx))));
        page = page.child(tabs);
        page.child(if self.dock.dbm.sub == 0 { self.dbm_db_table(inst.kind, cx) } else { self.dbm_user_table(inst.kind, cx) }).into_any_element()
    }

    /// 清掉已读的库/用户/信息,下次渲染重新读(顶部「刷新」调用)
    pub(crate) fn dbm_invalidate(&mut self) {
        let (cur, sub) = (self.dock.dbm.cur.clone(), self.dock.dbm.sub);
        self.dock.dbm.reset(&cur);
        self.dock.dbm.sub = sub;
    }

    /// 页头副标题位置的实例信息:状态点、容器名、类型版本、运行时间、端口(面板管理的加「只读」)
    /// 当前页签下选中的实例(没有选过就取第一个)
    pub(crate) fn dbm_current(&self) -> Option<DbInstance> {
        let cur = self.db_tab.min(super::dbpages::DB_TABS.len() - 1);
        let list: Vec<DbInstance> = dbinst::detect(&self.dock.snap.containers).into_iter().filter(|d| super::dbpages::DB_TABS[cur].1.contains(&d.kind)).collect();
        list.iter().find(|d| d.container.name == self.dock.dbm.cur).or(list.first()).cloned()
    }

    pub(crate) fn dbm_header_info(&self) -> Option<Div> {
        let inst = &self.dbm_current()?;
        let (fg, _) = state_color(&inst.container);
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(div().size(px(7.0)).flex_shrink_0().rounded_full().bg(fg))
                .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(inst.container.name.clone()))
                .child(div().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::muted()).child(format!("{} {}", inst.kind.label(), inst.version)))
                .children(inst.container.ports.iter().take(2).map(|p| tag(p.clone())))
                .children(inst.container.panel.map(|p| tag(format!("{p} · 只读")))),
        )
    }

    fn dbm_inst(&self) -> Option<DbInstance> {
        dbinst::detect(&self.dock.snap.containers).into_iter().find(|d| d.container.name == self.dock.dbm.cur)
    }

    fn dbm_open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("例如 app"));
        let user = cx.new(|cx| InputState::new(window, cx).placeholder("留空则与库名相同"));
        let password = cx.new(|cx| InputState::new(window, cx).placeholder("密码").masked(true).default_value(crate::dbinstall::gen_password()));
        let desc = cx.new(|cx| TextareaState::new(window, cx).rows(4));
        // 库名失焦时,用户名还是空的就直接抄库名
        let (n2, u2) = (name.clone(), user.clone());
        cx.subscribe_in(&name, window, move |_, _, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Blur) && u2.read(cx).value().trim().is_empty() {
                let v = n2.read(cx).value().trim().to_string();
                if !v.is_empty() {
                    u2.update(cx, |s, cx| s.set_value(&v, window, cx));
                }
            }
        })
        .detach();
        let fh = name.focus_handle(cx);
        window.focus(&fh, cx);
        let m = &mut self.dock.dbm;
        m.form = Some(DbForm { name, user, password, desc, superuser: true, show_pw: false });
        m.busy = false;
        m.form_err = None;
        cx.notify();
    }

    fn dbm_close_form(&mut self, cx: &mut Context<Self>) {
        self.dock.dbm.form = None;
        cx.notify();
    }

    fn dbm_submit(&mut self, cx: &mut Context<Self>) {
        let Some(f) = self.dock.dbm.form.clone() else { return };
        if self.dock.dbm.busy {
            return;
        }
        let name = f.name.read(cx).value().trim().to_string();
        let user = f.user.read(cx).value().trim().to_string();
        let user = if user.is_empty() { name.clone() } else { user };
        let pw = f.password.read(cx).value().to_string();
        let desc = f.desc.read(cx).value().trim().to_string();
        let Some(inst) = self.dbm_inst() else { return };
        let sql = match inst.kind.create_db_with_user_sql(&name, &user, &pw, f.superuser, if inst.kind == DbKind::Postgres { &desc } else { "" }) {
            Ok(q) => q,
            Err(e) => {
                self.dock.dbm.form_err = Some(e);
                cx.notify();
                return;
            }
        };
        if self.demo {
            self.dock.dbm.form = None;
            self.dock.dbm.done = Some((name, user, pw));
            cx.notify();
            return;
        }
        let tag = format!("dbm:create:{}", inst.container.name);
        if let Some(d) = self.dbm_docker() {
            d.db_run(&tag, inst.kind.exec_args(&inst.container.name), Some(sql));
            let m = &mut self.dock.dbm;
            m.inflight.push(tag);
            m.busy = true;
            m.form_err = None;
            m.pending = Some((name, user, pw));
        }
        cx.notify();
    }

    /// 确认后重置密码:生成新的强密码,在容器里执行 ALTER,成功后只显示一次
    fn dbm_reset_run(&mut self, user: String, host: String, cx: &mut Context<Self>) {
        self.dock.dbm.reset_ask = None;
        let Some(inst) = self.dbm_inst() else { return };
        let pw = crate::dbinstall::gen_password();
        let sql = match inst.kind.alter_password_sql(&user, &host, &pw) {
            Ok(q) => q,
            Err(e) => {
                self.dock.dbm.reset_err = Some(e);
                cx.notify();
                return;
            }
        };
        if self.demo {
            self.dock.dbm.done = Some((String::new(), user, pw));
            cx.notify();
            return;
        }
        let tag = format!("dbm:reset:{}", inst.container.name);
        if let Some(d) = self.dbm_docker() {
            d.db_run(&tag, inst.kind.exec_args(&inst.container.name), Some(sql));
            let m = &mut self.dock.dbm;
            m.inflight.push(tag);
            m.busy = true;
            m.reset_err = None;
            m.pending = Some((String::new(), user, pw));
        }
        cx.notify();
    }

    /// 确认后删除数据库
    fn dbm_drop_run(&mut self, name: String, cx: &mut Context<Self>) {
        self.dock.dbm.drop_ask = None;
        let Some(inst) = self.dbm_inst() else { return };
        let sql = match inst.kind.drop_db_sql(&name) {
            Ok(q) => q,
            Err(e) => {
                self.dock.dbm.reset_err = Some(e);
                cx.notify();
                return;
            }
        };
        if self.demo {
            if let Some(Ok(v)) = &mut self.dock.dbm.dbs {
                v.retain(|d| d.name != name);
            }
            cx.notify();
            return;
        }
        let tag = format!("dbm:drop:{}", inst.container.name);
        if let Some(d) = self.dbm_docker() {
            d.db_run(&tag, inst.kind.exec_args(&inst.container.name), Some(sql));
            let m = &mut self.dock.dbm;
            m.inflight.push(tag);
            m.busy = true;
            m.reset_err = None;
        }
        cx.notify();
    }

    fn dbm_drop_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let name = self.dock.dbm.drop_ask.clone()?;
        let inst = self.dbm_inst()?;
        let spec = crate::confirm::Confirm {
            ids: ["dbm-drop-cancel", "dbm-drop-ok"],
            title: "删除数据库",
            tile: Self::icon_tile(IconName::Database, c::danger(), c::danger().opacity(0.18)).into_any_element(),
            name: name.clone().into(),
            detail: inst.kind.label().into(),
            points: vec![
                (IconName::Database, c::danger(), "库里的所有表和数据一并删除,无法恢复".into()),
                (IconName::Users, c::accent(), "所属用户不会被删除,可到「用户」里另行处理".into()),
            ],
            ok_label: "删除数据库",
            compact: false,
            kind: crate::confirm::ConfirmKind::Danger,
            extra: None,
        };
        Some(self.delete_confirm("dbm-drop-dialog", spec, |this| this.dock.dbm.drop_ask = None, move |this, cx| this.dbm_drop_run(name.clone(), cx), cx).into_any_element())
    }

    /// 重置密码前的确认框 / 失败提示
    fn dbm_reset_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if let Some(e) = self.dock.dbm.reset_err.clone() {
            let body = div()
                .p(px(20.0))
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child("操作失败"))
                .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(e))
                .child(div().flex().flex_row().justify_end().child(button("dbm-reset-err-ok", IconName::Check, "知道了").on_click(cx.listener(|this, _, _, cx| {
                    this.dock.dbm.reset_err = None;
                    cx.notify();
                }))));
            return Some(crate::ui::modal_glass("dbm-reset-err", 420.0, |_, _, _| {}, [body.into_any_element()]).into_any_element());
        }
        let (user, host) = self.dock.dbm.reset_ask.clone()?;
        let u2 = user.clone();
        let body = div()
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(format!("重置用户 {user} 的密码?")))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("会生成一个新的高强度密码,原密码立即失效;使用旧密码的应用需要改成新密码。新密码只显示一次。"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(button("dbm-reset-cancel", IconName::X, "取消").on_click(cx.listener(|this, _, _, cx| {
                        this.dock.dbm.reset_ask = None;
                        cx.notify();
                    })))
                    .child(button("dbm-reset-ok", IconName::RefreshCw, "重置").on_click(cx.listener(move |this, _, _, cx| this.dbm_reset_run(u2.clone(), host.clone(), cx)))),
            );
        Some(crate::ui::modal_glass("dbm-reset-ask", 440.0, |_, _, _| {}, [body.into_any_element()]).into_any_element())
    }

    /// 数据库页的弹层:新建数据库抽屉、创建成功后的一次性密码
    pub(crate) fn dbm_overlays(&mut self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v = Vec::new();
        v.extend(self.dbm_drawer(window, cx));
        v.extend(self.dbm_done_dialog(cx));
        v.extend(self.dbm_reset_dialog(cx));
        v.extend(self.dbm_drop_dialog(cx));
        v
    }

    fn dbm_drawer(&mut self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let f = self.dock.dbm.form.clone()?;
        let busy = self.dock.dbm.busy;
        let pg = self.dbm_inst().is_some_and(|i| i.kind == DbKind::Postgres);
        let input = |i: &Entity<InputState>| div().flex_1().min_w_0().child(Input::new(i).appearance(false).with_size(px(12.0 / 0.875))).into_any_element();
        let col = |label: &'static str, inner: AnyElement| div().w_full().flex_none().child(super::composeedit::field_col_with(label, None, inner, false, false, None));
        let eye = super::syspages::tip(
            super::dockerpages::icon_btn(("dbm-form-eye", 0), if f.show_pw { IconName::EyeOff } else { IconName::Eye }, c::muted()).on_click(cx.listener(|this, _, window, cx| {
                if let Some(f) = this.dock.dbm.form.as_mut() {
                    f.show_pw = !f.show_pw;
                    let (masked, pw) = (!f.show_pw, f.password.clone());
                    pw.update(cx, |s, cx| s.set_masked(masked, window, cx));
                }
                cx.notify();
            })),
            if f.show_pw { "隐藏密码" } else { "显示密码" },
        );
        let regen = super::syspages::tip(
            super::dockerpages::icon_btn(("dbm-form-regen", 0), IconName::RefreshCw, c::muted()).on_click(cx.listener(|this, _, window, cx| {
                if let Some(f) = this.dock.dbm.form.as_ref() {
                    f.password.update(cx, |s, cx| s.set_value(&crate::dbinstall::gen_password(), window, cx));
                }
            })),
            "重新生成",
        );
        let pw_inner = div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(4.0)).child(input(&f.password)).child(eye).child(regen).into_any_element();
        let su = f.superuser;
        let su_card = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .p(px(12.0))
            .rounded(px(t::RADIUS_MD))
            .border_1()
            .border_color(c::field_border())
            .bg(c::field())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_size(px(t::TYPE_CONTROL)).child("拥有该库的最高权限"))
                    .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(if su { "可以建表、改表结构、授权,只限这个库,不影响其他库" } else { "只能读写这个库的数据,不能改表结构" })),
            )
            .child(Self::pill_switch("dbm-form-su", su).on_click(cx.listener(|this, _, _, cx| {
                if let Some(f) = this.dock.dbm.form.as_mut() {
                    f.superuser = !f.superuser;
                }
                cx.notify();
            })));
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
            .child(div().flex_1().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child("新建数据库"))
            .child(super::syspages::tip(super::dockerpages::icon_btn(("dbm-form-x", 0), IconName::X, c::muted()).on_click(cx.listener(|this, _, _, cx| this.dbm_close_form(cx))), "关闭"));
        let body = div()
            .id("dbm-form-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .p(px(16.0))
            .child(col("数据库名称", input(&f.name)))
            .child(col("用户名", input(&f.user)))
            .child(col("密码", pw_inner))
            .child(su_card)
            .children(pg.then(|| {
                let focused = f.desc.focus_handle(cx).is_focused(window);
                let area = div()
                    .w_full()
                    .px(px(4.0))
                    .py(px(2.0))
                    .rounded(px(t::RADIUS_MD))
                    .bg(c::field())
                    .border_1()
                    .border_color(if focused { c::accent() } else { c::field_border() })
                    .h(px(110.0))
                    .overflow_hidden()
                    .text_color(c::text())
                    .text_size(px(t::TYPE_CONTROL))
                    .child(Textarea::new(&f.desc).appearance(false).with_size(px(12.0 / 0.875)).h_full());
                div().w_full().flex_none().flex().flex_col().gap(px(6.0)).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("描述")).child(area)
            }));
        let ok = div()
            .id("dbm-form-ok")
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
            .when(busy, |d| d.opacity(0.6))
            .child(if busy { "创建中…" } else { "创建" })
            .on_click(cx.listener(|this, _, _, cx| this.dbm_submit(cx)));
        let err = self.dock.dbm.form_err.clone();
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
            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CAPTION)).text_color(if err.is_some() { c::danger() } else { c::ghost() }).child(err.unwrap_or_else(|| "密码只在创建成功后显示一次".into())))
            .child(ok);
        Some(
            crate::ui::drawer("dbm-drawer", 380.0, cx.listener(|this, _, _, cx| this.dbm_close_form(cx)), [head.into_any_element(), body.into_any_element(), foot.into_any_element()])
                .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                    if e.keystroke.key == "escape" {
                        this.dbm_close_form(cx);
                        cx.stop_propagation();
                    }
                }))
                .into_any_element(),
        )
    }

    /// 创建成功后一次性显示用户名和密码(库里只存哈希,之后读不回来)
    fn dbm_done_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (db, user, pw) = self.dock.dbm.done.clone()?;
        let row = |k: &'static str, v: String| div().flex().flex_row().gap(px(12.0)).child(div().w(px(70.0)).text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(k)).child(div().flex_1().min_w_0().font_family(MONO).text_size(px(t::TYPE_CONTROL)).child(v));
        let pwc = pw.clone();
        let body = div()
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(if db.is_empty() { "密码已重置" } else { "数据库已创建" }))
            .children((!db.is_empty()).then(|| row("数据库", db)))
            .child(row("用户名", user))
            .child(row("密码", pw))
            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child("密码只在这里显示这一次,Otter 不会保存,数据库里也只存哈希。请现在复制;忘了只能再重置。"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(button("dbm-copy-pw", IconName::Copy, "复制密码").on_click(cx.listener(move |_, _, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(pwc.clone())))))
                    .child(button("dbm-done-ok", IconName::Check, "我已保存").on_click(cx.listener(|this, _, _, cx| {
                        this.dock.dbm.done = None;
                        cx.notify();
                    }))),
            );
        Some(crate::ui::modal("dbm-done", 460.0, |_, _, _| {}, [body.into_any_element()]).into_any_element())
    }

    fn dbm_state<T>(r: &Loaded<T>) -> Option<AnyElement> {
        match r {
            None => Some(loading("正在读取…").into_any_element()),
            Some(Err(e)) => Some(empty(IconName::TriangleAlert, "读取失败").desc(e.clone()).danger().into_any_element()),
            Some(Ok(_)) => None,
        }
    }

    fn dbm_db_table(&self, kind: DbKind, cx: &mut Context<Self>) -> AnyElement {
        if let Some(x) = Self::dbm_state(&self.dock.dbm.dbs) {
            return x;
        }
        let Some(Ok(rows)) = &self.dock.dbm.dbs else { return div().into_any_element() };
        // 默认的系统库不显示
        let rows: Vec<&DbRow> = rows.iter().filter(|d| !d.system).collect();
        if rows.is_empty() {
            return empty(IconName::Database, "还没有业务数据库").desc("系统自带的库已隐藏,点右上角「新建数据库」开始使用").into_any_element();
        }
        let pg = kind == DbKind::Postgres;
        // 列宽固定,多出来的宽度给「描述」(MySQL 没有描述,留一个空位),名称列不再拉宽
        let mut header = div().child(head("名称").w(px(220.0))).child(head(if pg { "所属用户" } else { "字符集" }).w(px(160.0)));
        if pg {
            header = header.child(head("编码").w(px(70.0)));
        }
        header = header.child(head("排序规则").w(px(170.0)));
        if !pg {
            header = header.child(head("表数").w(px(60.0)));
        }
        header = header.child(head("连接").w(px(60.0))).child(head("大小").w(px(90.0))).child(head(if pg { "描述" } else { "" }).flex_1().min_w_0()).child(head("操作").w(px(80.0)).text_right());
        let admin = self.dock.dbm.cred.clone();
        let body = rows
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let mut r = row(("dbm-db", i))
                    .child(div().w(px(220.0)).truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(d.name.clone()))
                    .child(div().w(px(160.0)).truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(d.extra.clone()));
                if pg {
                    r = r.child(div().w(px(70.0)).text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(d.enc.clone()));
                }
                let mono = |w: f32, v: String| div().w(px(w)).truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(v);
                r = r.child(mono(170.0, d.collate.clone()));
                if !pg {
                    r = r.child(mono(60.0, d.tables.map(|n| n.to_string()).unwrap_or_default()));
                }
                r = r.child(mono(60.0, d.conns.map(|n| n.to_string()).unwrap_or_default()));
                r = r.child(mono(90.0, d.size.map(dbinst::fmt_size).unwrap_or_default()));
                r = r.child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(d.desc.clone()));
                // 重置密码只对 PostgreSQL 有意义(库有所属用户);管理员账号的密码来自容器环境变量,重置会让它失效,不给重置
                let owner = d.extra.clone();
                let can_reset = pg && !owner.is_empty() && admin.as_deref() != Some(owner.as_str());
                let name = d.name.clone();
                use super::dockerpages::icon_btn_c;
                use super::syspages::tip;
                r.child(
                    div()
                        .w(px(80.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .items_center()
                        .gap(px(2.0))
                        .children(can_reset.then(|| {
                            tip(icon_btn_c(("dbm-db-reset", i), IconName::KeyRound, c::muted()), "重置密码").on_click(cx.listener(move |this, _, _, cx| {
                                this.dock.dbm.reset_ask = Some((owner.clone(), String::new()));
                                cx.notify();
                            }))
                        }))
                        .child(tip(icon_btn_c(("dbm-db-drop", i), IconName::Trash, c::danger()), "删除").on_click(cx.listener(move |this, _, _, cx| {
                            this.dock.dbm.drop_ask = Some(name.clone());
                            cx.notify();
                        }))),
                )
            })
            .collect();
        table_flow(header, body).into_any_element()
    }

    fn dbm_user_table(&self, kind: DbKind, cx: &mut Context<Self>) -> AnyElement {
        if let Some(x) = Self::dbm_state(&self.dock.dbm.users) {
            return x;
        }
        let Some(Ok(rows)) = &self.dock.dbm.users else { return div().into_any_element() };
        let mysql = kind != DbKind::Postgres;
        let header = div().child(head("用户").flex_1().min_w_0()).child(head(if mysql { "主机" } else { "权限" }).w(px(240.0))).child(head("操作").w(px(80.0)).text_right());
        let body = rows
            .iter()
            .enumerate()
            .map(|(i, u)| {
                row(("dbm-user", i))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(u.name.clone()))
                    .child(div().w(px(240.0)).truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(if mysql { u.host.clone() } else { u.flags.clone() }))
                    .child({
                        let (n, h) = (u.name.clone(), u.host.clone());
                        div().w(px(80.0)).flex().flex_row().justify_end().items_center().children(u.system.then(|| tag("系统"))).children((!u.system).then(|| {
                            super::syspages::tip(super::dockerpages::icon_btn_c(("dbm-reset", i), IconName::KeyRound, c::muted()), "重置密码").on_click(cx.listener(move |this, _, _, cx| {
                                this.dock.dbm.reset_ask = Some((n.clone(), h.clone()));
                                cx.notify();
                            }))
                        }))
                    })
            })
            .collect();
        table_flow(header, body).into_any_element()
    }

    fn dbm_redis(&self) -> AnyElement {
        if let Some(x) = Self::dbm_state(&self.dock.dbm.redis) {
            return x;
        }
        let Some(Ok(r)) = &self.dock.dbm.redis else { return div().into_any_element() };
        let cell = |k: &'static str, v: String| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(k))
                .child(div().text_size(px(t::TYPE_BODY)).font_family(MONO).text_color(c::text()).child(v))
        };
        let header = div().child(head("库").flex_1().min_w_0()).child(head("Key 数").w(px(120.0)));
        let body = r.keys.iter().enumerate().map(|(i, (n, k))| row(("dbm-rk", i)).child(div().flex_1().min_w_0().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(n.clone())).child(div().w(px(120.0)).text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(k.to_string()))).collect();
        div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(div().flex().flex_row().gap(px(40.0)).child(cell("版本", r.version.clone())).child(cell("内存", r.memory.clone())).child(cell("连接数", r.clients.clone())).child(cell("运行天数", r.uptime_days.clone())))
            .child(if r.keys.is_empty() { div().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child("还没有任何 key").into_any_element() } else { table_flow(header, body).into_any_element() })
            .into_any_element()
    }

    /// 演示模式:不连服务器,给一份固定的示例数据
    fn dbm_demo(&mut self, inst: &DbInstance) {
        let m = &mut self.dock.dbm;
        let db = |n: &str, e: &str, s: u64, sys: bool| DbRow { name: n.into(), extra: e.into(), enc: if e.starts_with("utf") { String::new() } else { "UTF8".into() }, conns: Some(if sys { 0 } else { 2 }), size: Some(s), collate: "en_US.utf8".into(), tables: Some(5), desc: if sys { String::new() } else { "演示数据库".into() }, system: sys };
        m.dbs = Some(Ok(vec![db("app", if inst.kind == DbKind::Postgres { "app_owner" } else { "utf8mb4" }, 48 << 20, false), db("postgres", "postgres", 7 << 20, true)]));
        m.users = Some(Ok(vec![UserRow { name: "app_owner".into(), host: "%".into(), flags: "可建库".into(), system: false }, UserRow { name: "postgres".into(), host: "localhost".into(), flags: "超级用户".into(), system: true }]));
        m.cred = Some("postgres".into());
        m.redis = Some(Ok(RedisInfo { version: "7.2.4".into(), memory: "1.2M".into(), clients: "3".into(), uptime_days: "12".into(), keys: vec![("db0".into(), 128)] }));
    }
}

/// 实例的主端口(如 `5432`),没有映射时显示容器名
fn inst_port(d: &DbInstance) -> String {
    d.container.ports.first().and_then(|p| p.split("->").next()).map(|p| p.rsplit(':').next().unwrap_or(p).to_string()).unwrap_or_else(|| d.kind.port().to_string())
}
