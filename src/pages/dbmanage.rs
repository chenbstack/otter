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
    /// 备份与恢复弹窗(见 `dbbackup`)
    pub(super) bk: Option<super::dbbackup::BackupDlg>,
    /// 新建用户弹窗、等用户确认删除的用户 (用户名, 主机)
    uform: Option<UserForm>,
    udrop_ask: Option<(String, String)>,
    /// 一次性结果弹窗的种类:0 数据库已创建,1 密码已重置,2 用户已创建
    done_kind: u8,
    /// 连接信息弹窗:打开时才从容器环境变量读 (用户名, 密码),只放内存,关掉就丢
    conn_open: bool,
    conn: Option<Result<(String, zeroize::Zeroizing<String>), String>>,
    conn_show: bool,
    /// 服务器上各网卡的 IPv4(一行一个,读回来才有)和「复制连接串」菜单是否展开
    conn_addrs: Option<String>,
    conn_menu: bool,
    /// 数据库还在启动(刚装完):读取失败不报错,到点自动重试
    starting: bool,
    retry_at: Option<std::time::Instant>,
    tries: u32,
    /// 已发出还没收到结果的请求 tag
    pub(super) inflight: Vec<String>,
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

/// 新建用户弹窗里的输入
#[derive(Clone)]
struct UserForm {
    name: Entity<InputState>,
    host: Entity<InputState>,
    password: Entity<InputState>,
    /// 授权的库和各自的权限档位,空 = 不授权
    grants: Vec<dbinst::Grant>,
    add_open: bool,
    show_pw: bool,
    /// 编辑已有用户:(用户名, 主机);新建时为 None
    edit: Option<(String, String)>,
    /// 编辑时读回来的现有授权(读到之前为 None,保存时拿它和 `grants` 比出差异)
    orig: Option<Vec<dbinst::Grant>>,
}

/// 凭据卡片的内容(见 `dbm_cred_card`)
struct CredCard {
    title: &'static str,
    desc: Option<&'static str>,
    db: Option<String>,
    user: String,
    pw: Option<String>,
    status: CredStatus,
    /// 一次性:底部有「我已保存」
    one_time: bool,
    close: CredClose,
    /// 取不到密码时的说明
    pw_missing: &'static str,
}

enum CredStatus {
    Loading,
    Error(String),
    Ready,
}

#[derive(Clone, Copy)]
enum CredClose {
    Conn,
    Done,
}

const SUBS: [&str; 2] = ["数据库", "用户"];

impl DbMgr {
    fn reset(&mut self, cur: &str) {
        *self = DbMgr { cur: cur.to_string(), ..Default::default() };
    }
}

impl Home {
    pub(super) fn dbm_docker(&self) -> Option<&crate::docker::Docker> {
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
        if have || m.inflight.contains(&tag) || !inst.container.running() || m.retry_at.is_some_and(|t| std::time::Instant::now() < t) {
            return;
        }
        self.dock.dbm.retry_at = None;
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
            if tag.starts_with("dbm:bk") {
                self.dbm_bk_result(&tag, r);
                continue;
            }
            if tag.starts_with("dbm:conn:") {
                m.conn = Some(r.map(|o| {
                    let (u, p) = o.split_once('\n').unwrap_or(("", &o));
                    (u.trim().to_string(), zeroize::Zeroizing::new(p.trim_end_matches(['\n', '\r']).to_string()))
                }));
                continue;
            }
            if tag.starts_with("dbm:addr:") {
                m.conn_addrs = Some(r.unwrap_or_default());
                continue;
            }
            if tag.starts_with("dbm:cred:") {
                m.cred = r.ok().map(|o| o.trim().to_string()).filter(|u| !u.is_empty());
                continue;
            }
            if matches!(tag.split(':').nth(1), Some("dbs" | "users" | "redis")) {
                match &r {
                    Err(e) if dbinst::is_starting(e) && m.tries < 40 => {
                        m.tries += 1;
                        m.starting = true;
                        m.retry_at = Some(std::time::Instant::now() + std::time::Duration::from_secs(3));
                        continue;
                    }
                    _ => {
                        m.starting = false;
                        m.tries = 0;
                    }
                }
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
            if tag.starts_with("dbm:ugrants:") {
                match r {
                    Ok(o) => {
                        if let Some(f) = m.uform.as_mut() {
                            f.grants = kind.parse_grants(&o);
                            f.orig = Some(f.grants.clone());
                        }
                    }
                    Err(e) => m.form_err = Some(e),
                }
                continue;
            }
            if tag.starts_with("dbm:updateuser:") {
                m.busy = false;
                match r {
                    Ok(_) => {
                        m.uform = None;
                        m.form_err = None;
                    }
                    Err(e) => m.form_err = Some(e),
                }
                continue;
            }
            if tag.starts_with("dbm:dropuser:") {
                m.busy = false;
                match r {
                    Ok(_) => m.users = None,
                    Err(e) => m.reset_err = Some(e),
                }
                continue;
            }
            if tag.starts_with("dbm:newuser:") {
                m.busy = false;
                match r {
                    Ok(_) => {
                        m.uform = None;
                        m.form_err = None;
                        m.users = None;
                        m.done = m.pending.take();
                        m.done_kind = 2;
                    }
                    Err(e) => m.form_err = Some(e),
                }
                continue;
            }
            if tag.starts_with("dbm:reset:") {
                m.busy = false;
                match r {
                    Ok(_) => {
                        m.done = m.pending.take();
                        m.done_kind = 1;
                    }
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
                        m.done_kind = 0;
                    }
                    Err(e) => m.form_err = Some(e),
                }
                continue;
            }
            match tag.split(':').nth(1) {
                Some("dbs") => m.dbs = Some(r.map(|o| dbinst::parse_dbs(kind, &o))),
                Some("users") => m.users = Some(r.map(|o| dbinst::parse_users(kind, &o))),
                _ => m.redis = Some(r.and_then(|o| dbinst::redis_check(&o).map(|_| dbinst::parse_redis(&o)))),
            }
        }
    }

    pub(crate) fn dbm_page(&mut self, list: Vec<DbInstance>, cx: &mut Context<Self>) -> AnyElement {
        if let Some(n) = self.dock.dbi.jump.clone() {
            if list.iter().any(|d| d.container.name == n) {
                self.dock.dbi.jump = None;
                self.dock.dbm.reset(&n);
            }
        }
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
        self.dbm_request_uform(&inst);
        self.dbm_want_addrs(&inst);
        self.dbm_bk_request();

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

        if self.dock.dbm.starting && (if inst.kind == DbKind::Redis { self.dock.dbm.redis.is_none() } else if self.dock.dbm.sub == 0 { self.dock.dbm.dbs.is_none() } else { self.dock.dbm.users.is_none() }) {
            return page.child(loading("数据库正在启动,稍后自动重试…")).into_any_element();
        }
        // 文本按钮:没有底色和图标,悬停才出底色(和页头的「卸载」同一样式,颜色用次要文字色)
        let conn_btn = div()
            .id("dbm-conn")
            .h(px(34.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .rounded(px(t::RADIUS_MD))
            .cursor_pointer()
            .text_size(px(t::TYPE_CONTROL))
            .text_color(c::secondary())
            .hover(|s| s.bg(c::field_hover()).text_color(c::text()))
            .child("连接信息")
            .on_click(cx.listener(|this, _, _, cx| this.dbm_conn_open(cx)));
        if inst.kind == DbKind::Redis {
            return page.child(div().flex().flex_row().justify_end().child(conn_btn)).child(self.dbm_redis()).into_any_element();
        }

        // 数据库 / 用户
        let mut tabs = div().flex().flex_row().items_center().gap(px(4.0));
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
        let tabs = tabs.child(div().flex_1()).child(div().flex().flex_row().items_center().gap(px(8.0)).child(conn_btn).child(if self.dock.dbm.sub == 0 {
            button("dbm-new-db", IconName::Plus, "新建数据库").on_click(cx.listener(|this, _, window, cx| this.dbm_open_form(window, cx)))
        } else {
            button("dbm-new-user", IconName::Plus, "新建用户").on_click(cx.listener(|this, _, window, cx| this.dbm_open_uform(window, cx)))
        }));
        page = page.child(tabs);
        page.child(if self.dock.dbm.sub == 0 { self.dbm_db_table(inst.kind, cx) } else { self.dbm_user_table(inst.kind, cx) }).into_any_element()
    }

    /// 清掉已读的库/用户/信息,下次渲染重新读(顶部「刷新」调用)
    /// 重试时间到了:让页面重绘一次,重新发起读取
    pub(crate) fn dbm_retry_due(&self) -> bool {
        self.dock.dbm.retry_at.is_some_and(|t| std::time::Instant::now() >= t) || self.dock.dbm.bk.as_ref().is_some_and(|b| b.poll_due())
    }

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

    pub(super) fn dbm_inst(&self) -> Option<DbInstance> {
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
            self.dock.dbm.done_kind = 0;
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
            self.dock.dbm.done_kind = 1;
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

    fn dbm_open_uform(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("例如 app_user"));
        let host = cx.new(|cx| InputState::new(window, cx).placeholder("%").default_value("%"));
        let password = cx.new(|cx| InputState::new(window, cx).placeholder("密码").masked(true).default_value(crate::dbinstall::gen_password()));
        let fh = name.focus_handle(cx);
        window.focus(&fh, cx);
        let m = &mut self.dock.dbm;
        m.uform = Some(UserForm { name, host, password, grants: Vec::new(), add_open: false, show_pw: false, edit: None, orig: None });
        m.busy = false;
        m.form_err = None;
        cx.notify();
    }

    /// 编辑已有用户的按库授权:先弹窗,现有授权在 `dbm_request_uform` 里读回来
    fn dbm_open_edit(&mut self, user: String, host: String, window: &mut Window, cx: &mut Context<Self>) {
        self.dbm_open_uform(window, cx);
        if let Some(f) = self.dock.dbm.uform.as_mut() {
            f.edit = Some((user, host));
        }
        if self.demo {
            if let Some(f) = self.dock.dbm.uform.as_mut() {
                f.grants = vec![dbinst::Grant { db: "app".into(), level: dbinst::Level::Write, perms: 0 }];
                f.orig = Some(f.grants.clone());
            }
        }
        cx.notify();
    }

    /// 新建/编辑用户弹窗要用的数据:库列表(添加数据库的候选)、编辑时用户现有的授权
    fn dbm_request_uform(&mut self, inst: &DbInstance) {
        let m = &self.dock.dbm;
        let Some(f) = &m.uform else { return };
        if self.demo || !inst.container.running() {
            return;
        }
        let kind = inst.kind;
        let dbs_tag = format!("dbm:dbs:{}", m.cur);
        let mut jobs: Vec<(String, String)> = Vec::new();
        if m.dbs.is_none() && !m.inflight.contains(&dbs_tag) {
            jobs.push((dbs_tag, kind.list_dbs_sql().to_string()));
        }
        let gtag = format!("dbm:ugrants:{}", m.cur);
        if let (Some((u, h)), None) = (&f.edit, &f.orig) {
            let names: Option<Vec<String>> = match &m.dbs {
                Some(Ok(v)) => Some(v.iter().map(|d| d.name.clone()).collect()),
                Some(Err(_)) => Some(Vec::new()),
                None => (kind != DbKind::Postgres).then(Vec::new),
            };
            if let (Some(names), false, None) = (names, m.inflight.contains(&gtag), &m.form_err) {
                match kind.read_grants_sql(u, h, &names) {
                    Ok(q) => jobs.push((gtag, q)),
                    Err(e) => {
                        self.dock.dbm.form_err = Some(e);
                        return;
                    }
                }
            }
        }
        for (tag, sql) in jobs {
            if let Some(d) = self.dbm_docker() {
                d.db_run(&tag, kind.exec_args(&inst.container.name), Some(sql));
                self.dock.dbm.inflight.push(tag);
            }
        }
    }

    fn dbm_close_uform(&mut self, cx: &mut Context<Self>) {
        self.dock.dbm.uform = None;
        cx.notify();
    }

    /// 提交新建用户:可顺带授予一个库的全部权限
    fn dbm_user_submit(&mut self, cx: &mut Context<Self>) {
        let Some(f) = self.dock.dbm.uform.clone() else { return };
        if self.dock.dbm.busy {
            return;
        }
        let Some(inst) = self.dbm_inst() else { return };
        if let Some((user, host)) = f.edit.clone() {
            // 编辑:拿读回来的授权和现在的比,只改有变化的
            let Some(orig) = f.orig.clone() else { return };
            let sql = match inst.kind.update_grants_sql(&user, &host, &orig, &f.grants) {
                Ok(q) => q,
                Err(e) => {
                    self.dock.dbm.form_err = Some(e);
                    cx.notify();
                    return;
                }
            };
            if sql.is_empty() || self.demo {
                self.dock.dbm.uform = None;
                cx.notify();
                return;
            }
            let tag = format!("dbm:updateuser:{}", inst.container.name);
            if let Some(d) = self.dbm_docker() {
                d.db_run(&tag, inst.kind.exec_args(&inst.container.name), Some(sql));
                let m = &mut self.dock.dbm;
                m.inflight.push(tag);
                m.busy = true;
                m.form_err = None;
            }
            cx.notify();
            return;
        }
        let name = f.name.read(cx).value().trim().to_string();
        let host = if inst.kind == DbKind::Postgres { String::new() } else { f.host.read(cx).value().trim().to_string() };
        let pw = f.password.read(cx).value().to_string();
        let sql = match inst.kind.create_user_with_grants_sql(&name, &host, &pw, &f.grants) {
            Ok(q) => q,
            Err(e) => {
                self.dock.dbm.form_err = Some(e);
                cx.notify();
                return;
            }
        };
        if self.demo {
            let m = &mut self.dock.dbm;
            m.uform = None;
            m.done = Some((f.grants.iter().map(|g| g.db.as_str()).collect::<Vec<_>>().join(", "), name, pw));
            m.done_kind = 2;
            cx.notify();
            return;
        }
        let tag = format!("dbm:newuser:{}", inst.container.name);
        if let Some(d) = self.dbm_docker() {
            d.db_run(&tag, inst.kind.exec_args(&inst.container.name), Some(sql));
            let m = &mut self.dock.dbm;
            m.inflight.push(tag);
            m.busy = true;
            m.form_err = None;
            m.pending = Some((f.grants.iter().map(|g| g.db.as_str()).collect::<Vec<_>>().join(", "), name, pw));
        }
        cx.notify();
    }

    /// 确认后删除用户
    fn dbm_udrop_run(&mut self, user: String, host: String, cx: &mut Context<Self>) {
        self.dock.dbm.udrop_ask = None;
        let Some(inst) = self.dbm_inst() else { return };
        let sql = match inst.kind.drop_user_sql(&user, &host) {
            Ok(q) => q,
            Err(e) => {
                self.dock.dbm.reset_err = Some(e);
                cx.notify();
                return;
            }
        };
        if self.demo {
            if let Some(Ok(v)) = &mut self.dock.dbm.users {
                v.retain(|u| !(u.name == user && u.host == host));
            }
            cx.notify();
            return;
        }
        let tag = format!("dbm:dropuser:{}", inst.container.name);
        if let Some(d) = self.dbm_docker() {
            d.db_run(&tag, inst.kind.exec_args(&inst.container.name), Some(sql));
            let m = &mut self.dock.dbm;
            m.inflight.push(tag);
            m.busy = true;
            m.reset_err = None;
        }
        cx.notify();
    }

    fn dbm_udrop_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (user, host) = self.dock.dbm.udrop_ask.clone()?;
        let inst = self.dbm_inst()?;
        let mysql = inst.kind != DbKind::Postgres;
        let spec = crate::confirm::Confirm {
            ids: ["dbm-udrop-cancel", "dbm-udrop-ok"],
            title: "删除用户",
            tile: Self::icon_tile(IconName::Users, c::danger(), c::danger().opacity(0.18)).into_any_element(),
            name: if mysql { format!("{user}@{host}").into() } else { user.clone().into() },
            detail: inst.kind.label().into(),
            points: vec![
                (IconName::KeyRound, c::danger(), "用这个用户连接的应用会立即连不上".into()),
                (IconName::Database, c::accent(), if mysql { "它的库不会被删除".into() } else { "它还拥有库或对象时会删除失败,需要先转移或删除那些库".into() }),
            ],
            ok_label: "删除用户",
            kind: crate::confirm::ConfirmKind::Danger,
            extra: None,
        };
        Some(self.delete_confirm("dbm-udrop-dialog", spec, |this| this.dock.dbm.udrop_ask = None, move |this, cx| this.dbm_udrop_run(user.clone(), host.clone(), cx), cx).into_any_element())
    }

    /// 「新建用户」弹窗:用户名、(MySQL)主机、密码,可选授权一个库
    fn dbm_user_dialog(&mut self, _window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let f = self.dock.dbm.uform.clone()?;
        let inst = self.dbm_inst()?;
        let mysql = inst.kind != DbKind::Postgres;
        let busy = self.dock.dbm.busy;
        let input = |i: &Entity<InputState>| div().flex_1().min_w_0().child(Input::new(i).appearance(false).with_size(px(12.0 / 0.875))).into_any_element();
        let col = |label: &'static str, inner: AnyElement| div().w_full().flex_none().child(super::composeedit::field_col_with(label, None, inner, false, false, None));
        let eye = super::syspages::tip(
            super::dockerpages::icon_btn(("dbm-uf-eye", 0), if f.show_pw { IconName::EyeOff } else { IconName::Eye }, c::muted()).on_click(cx.listener(|this, _, window, cx| {
                if let Some(f) = this.dock.dbm.uform.as_mut() {
                    f.show_pw = !f.show_pw;
                    let (masked, pw) = (!f.show_pw, f.password.clone());
                    pw.update(cx, |s, cx| s.set_masked(masked, window, cx));
                }
                cx.notify();
            })),
            if f.show_pw { "隐藏密码" } else { "显示密码" },
        );
        let regen = super::syspages::tip(
            super::dockerpages::icon_btn(("dbm-uf-regen", 0), IconName::RefreshCw, c::muted()).on_click(cx.listener(|this, _, window, cx| {
                if let Some(f) = this.dock.dbm.uform.as_ref() {
                    f.password.update(cx, |s, cx| s.set_value(&crate::dbinstall::gen_password(), window, cx));
                }
            })),
            "重新生成",
        );
        let pw_inner = div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(4.0)).child(input(&f.password)).child(eye).child(regen).into_any_element();

        // 授权:每个库一行(库名 + 四档权限),自定义档展开勾选项;下面「添加数据库」选还没加的库
        let pg = !mysql;
        // 一个库一张卡:第一行库名 + 移除;第二行四档分段(等宽);一句话说明这一档;自定义时再多一行等宽的勾选项
        let seg = |id: (&'static str, usize), text: &'static str, on: bool| {
            div()
                .id(id)
                .flex_1()
                .h(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(t::RADIUS_SM))
                .cursor_pointer()
                .text_size(px(t::TYPE_CONTROL))
                .when(on, |d| d.bg(c::accent().opacity(0.22)).text_color(c::text()).font_weight(FontWeight::SEMIBOLD))
                .when(!on, |d| d.text_color(c::muted()).hover(|s| s.bg(c::field_hover()).text_color(c::text())))
                .child(text)
        };
        let mut rows = div().flex().flex_col().gap(px(8.0));
        for (i, g) in f.grants.iter().enumerate() {
            let levels = [(dbinst::Level::Read, "只读"), (dbinst::Level::Write, "读写"), (dbinst::Level::All, "全部"), (dbinst::Level::Custom, "自定义")];
            let mut segs = div().flex().flex_row().items_center().gap(px(2.0)).p(px(2.0)).rounded(px(t::RADIUS_MD)).bg(c::field_hover());
            for (k, (lv, text)) in levels.into_iter().enumerate() {
                segs = segs.child(seg(("dbm-ug-lv", i * 4 + k), text, g.level == lv).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(g) = this.dock.dbm.uform.as_mut().and_then(|f| f.grants.get_mut(i)) {
                        if lv == dbinst::Level::Custom && g.level != dbinst::Level::Custom && g.perms == 0 {
                            g.perms = g.level.bits(0).max(dbinst::P_SELECT);
                        }
                        g.level = lv;
                    }
                    cx.notify();
                })));
            }
            let note = match g.level {
                dbinst::Level::Read => "只能查询数据",
                dbinst::Level::Write => "查询、新增、修改、删除数据,不能改表结构",
                dbinst::Level::All => if pg { "包括建表、改表、删表,拥有这个库的全部权限" } else { "包括建表、改表、删表、建索引等,拥有这个库的全部权限" },
                dbinst::Level::Custom => "勾选下面允许的操作",
            };
            let mut card = div()
                .flex()
                .flex_col()
                .gap(px(10.0))
                .p(px(12.0))
                .rounded(px(t::RADIUS_MD))
                .border_1()
                .border_color(c::field_border())
                .bg(c::field())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(icon(IconName::Database, 14.0, c::secondary()))
                        .child(div().flex_1().min_w_0().truncate().font_family(MONO).text_size(px(t::TYPE_BODY)).child(g.db.clone()))
                        .child(super::syspages::tip(
                            super::dockerpages::icon_btn(("dbm-ug-rm", i), IconName::X, c::muted()).on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(f) = this.dock.dbm.uform.as_mut() {
                                    if i < f.grants.len() {
                                        f.grants.remove(i);
                                    }
                                }
                                cx.notify();
                            })),
                            "移除这个库",
                        )),
                )
                .child(segs)
                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(note));
            if g.level == dbinst::Level::Custom {
                let mut chips = div().flex().flex_row().gap(px(6.0));
                let all = [(dbinst::P_SELECT, "查询"), (dbinst::P_INSERT, "新增"), (dbinst::P_UPDATE, "修改"), (dbinst::P_DELETE, "删除"), (dbinst::P_STRUCT, if pg { "建表" } else { "表结构" })];
                for (k, (bit, text)) in all.into_iter().enumerate() {
                    let on = g.perms & bit != 0;
                    chips = chips.child(
                        div()
                            .id(("dbm-ug-p", i * 8 + k))
                            .flex_1()
                            .h(px(30.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap(px(5.0))
                            .rounded(px(t::RADIUS_SM))
                            .cursor_pointer()
                            .border_1()
                            .border_color(if on { c::accent() } else { c::field_border() })
                            .when(on, |d| d.bg(c::accent().opacity(0.12)))
                            .when(!on, |d| d.hover(|s| s.bg(c::field_hover())))
                            .text_size(px(t::TYPE_CONTROL))
                            .text_color(if on { c::text() } else { c::muted() })
                            .when(on, |d| d.child(icon(IconName::Check, 12.0, c::accent())))
                            .child(text)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(g) = this.dock.dbm.uform.as_mut().and_then(|f| f.grants.get_mut(i)) {
                                    g.perms ^= bit;
                                }
                                cx.notify();
                            })),
                    );
                }
                card = card.child(chips);
            }
            rows = rows.child(card);
        }
        // 还能添加的库
        let free: Vec<String> = match &self.dock.dbm.dbs {
            Some(Ok(v)) => v.iter().filter(|d| !d.system && !f.grants.iter().any(|g| g.db == d.name)).map(|d| d.name.clone()).collect(),
            _ => Vec::new(),
        };
        let add_btn = div()
            .id("dbm-ug-add")
            .w_full()
            .h(px(36.0))
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .rounded(px(t::RADIUS_MD))
            .border_1()
            .border_dashed()
            .border_color(c::field_border())
            .text_size(px(t::TYPE_CONTROL))
            .when(!free.is_empty(), |d| d.cursor_pointer().text_color(c::secondary()).hover(|s| s.bg(c::field_hover()).text_color(c::text())))
            .when(free.is_empty(), |d| d.text_color(c::ghost()))
            .child(icon(IconName::Plus, 13.0, if free.is_empty() { c::ghost() } else { c::secondary() }))
            .child(if free.is_empty() { if f.grants.is_empty() { "还没有可授权的数据库" } else { "没有更多数据库了" } } else { "添加数据库" })
            .when(!free.is_empty(), |d| {
                d.on_click(cx.listener(|this, _, _, cx| {
                    if let Some(f) = this.dock.dbm.uform.as_mut() {
                        f.add_open = !f.add_open;
                    }
                    cx.notify();
                }))
            });
        let add_menu = (f.add_open && !free.is_empty()).then(|| {
            let (t1, t2) = (cx.entity(), cx.entity());
            let items: Vec<crate::ui::MenuItem> = free.iter().map(|n| crate::ui::MenuItem::new(n.clone())).collect();
            crate::ui::dropdown_menu(
                "dbm-ug-menu",
                items,
                None,
                crate::ui::MenuSide::Below,
                34.0,
                move |i, _, cx| {
                    t1.update(cx, |h, cx| {
                        if let (Some(f), Some(n)) = (h.dock.dbm.uform.as_mut(), free.get(i)) {
                            f.grants.push(dbinst::Grant { db: n.clone(), level: dbinst::Level::Read, perms: dbinst::P_SELECT });
                            f.add_open = false;
                        }
                        cx.notify();
                    })
                },
                move |_, cx| {
                    t2.update(cx, |h, cx| {
                        if let Some(f) = h.dock.dbm.uform.as_mut() {
                            f.add_open = false;
                        }
                        cx.notify();
                    })
                },
            )
        });
        let hint = if pg { "只对现有的库授权;表权限覆盖现有和以后管理员建的表" } else { "只对现有的库授权,以后新建的库不会自动授权" };
        let grant = div()
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("数据库权限(可不选)"))
            .child(div().id("dbm-ug-rows").max_h(px(300.0)).overflow_y_scroll().child(rows))
            .child(div().relative().child(add_btn).children(add_menu))
            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(hint));
        let err = self.dock.dbm.form_err.clone();
        let ok = div()
            .id("dbm-uf-ok")
            .h(px(34.0))
            .px(px(18.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .bg(c::accent())
            .text_color(c::on_accent())
            .cursor_pointer()
            .hover(|s| s.opacity(0.88))
            .when(busy, |d| d.opacity(0.6))
            .when(busy, |d| d.child(crate::ui::dot_spinner(("dbm-uf-spin", 0), c::on_accent())))
            .child(if f.edit.is_some() { if busy { "保存中" } else { "保存" } } else if busy { "创建中" } else { "创建" })
            .when(f.edit.is_some() && f.orig.is_none(), |d| d.opacity(0.5))
            .on_click(cx.listener(|this, _, _, cx| this.dbm_user_submit(cx)));
        let body = div()
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(if f.edit.is_some() { "编辑用户" } else { "新建用户" }))
            .children(match &f.edit {
                Some((u, h)) => vec![div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(icon(IconName::Users, 14.0, c::muted()))
                    .child(div().font_family(MONO).text_size(px(t::TYPE_BODY)).child(if mysql { format!("{u}@{h}") } else { u.clone() }))
                    .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("(改密码请用「重置密码」)"))
                    .into_any_element()],
                None => {
                    let mut v = vec![col("用户名", input(&f.name)).into_any_element()];
                    if mysql {
                        v.push(col("允许连接的主机(% 表示任意)", input(&f.host)).into_any_element());
                    }
                    v.push(col("密码", pw_inner).into_any_element());
                    v
                }
            })
            .child(if f.edit.is_some() && f.orig.is_none() && self.dock.dbm.form_err.is_none() { div().py(px(12.0)).child(loading("正在读取现有权限…")).into_any_element() } else { grant.into_any_element() })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(div().flex_1().min_w_0().text_size(px(t::TYPE_CAPTION)).text_color(if err.is_some() { c::danger() } else { c::ghost() }).child(err.unwrap_or_else(|| if f.edit.is_some() { "只改有变化的库的权限".into() } else { "密码只在创建成功后显示一次".into() })))
                    .child(button("dbm-uf-cancel", IconName::X, "取消").on_click(cx.listener(|this, _, _, cx| this.dbm_close_uform(cx))))
                    .child(ok),
            );
        let esc = cx.listener(|this: &mut Self, e: &KeyDownEvent, _, cx| {
            if e.keystroke.key == "escape" {
                this.dbm_close_uform(cx);
                cx.stop_propagation();
            }
        });
        Some(crate::ui::modal("dbm-uform", 440.0, esc, [body.into_any_element()]).into_any_element())
    }

    /// 调试(截图用):演示数据下直接打开某个弹层
    pub(crate) fn debug_dbm(&mut self, kind: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(inst) = self.dbm_current() else { return };
        self.dock.dbm.reset(&inst.container.name);
        self.dbm_demo(&inst);
        match kind {
            "newdb" => self.dbm_open_form(window, cx),
            "newuser" | "newuser2" => {
                self.dock.dbm.sub = 1;
                self.dbm_open_uform(window, cx);
                if kind == "newuser2" {
                    if let Some(f) = self.dock.dbm.uform.as_mut() {
                        f.grants = vec![
                            dbinst::Grant { db: "app".into(), level: dbinst::Level::Custom, perms: dbinst::P_SELECT | dbinst::P_INSERT },
                            dbinst::Grant { db: "analytics".into(), level: dbinst::Level::Read, perms: 0 },
                            dbinst::Grant { db: "billing".into(), level: dbinst::Level::Write, perms: 0 },
                        ];
                    }
                }
            }
            "conn" => self.dbm_conn_open(cx),
            "backup" => self.dbm_bk_open("app".into(), cx),
            "backing" => {
                self.dbm_bk_open("app".into(), cx);
                if let Some(b) = self.dock.dbm.bk.as_mut() {
                    b.starting = true;
                    if let Some(Ok(d)) = b.data.as_mut() {
                        d.running.push(("app_20261125-102000.dump.part".into(), 3_400_000));
                    }
                    b.dl = Some(("app_20261125-101500.dump".into(), "x".into()));
                    b.restoring = true;
                    b.rs_file = Some("app_20261124-030000-pre.dump".into());
                }
            }
            "backup0" => {
                self.dbm_bk_open("app".into(), cx);
                if let Some(b) = self.dock.dbm.bk.as_mut() {
                    b.data = Some(Ok(Default::default()));
                }
            }
            "restore" => {
                self.dbm_bk_open("app".into(), cx);
                if let Some(b) = self.dock.dbm.bk.as_mut() {
                    b.restore_ask = Some("app_20261125-101500.dump".into());
                }
            }
            "created" | "reset" | "usercreated" => {
                self.dock.dbm.done = Some((if kind == "reset" { String::new() } else { "app".into() }, "app_user".into(), "Xk3-demo-password-9aQ".into()));
                self.dock.dbm.done_kind = match kind {
                    "created" => 0,
                    "reset" => 1,
                    _ => 2,
                };
            }
            "edituser" => {
                self.dock.dbm.sub = 1;
                self.dbm_open_edit("app_owner".into(), "%".into(), window, cx);
            }
            "udrop" => {
                self.dock.dbm.sub = 1;
                self.dock.dbm.udrop_ask = Some(("app_user".into(), "%".into()));
            }
            _ => {}
        }
    }

    /// 凭据卡片要用到服务器的网卡地址(内网/公网连接串):弹窗出现时读一次
    fn dbm_want_addrs(&mut self, inst: &DbInstance) {
        let m = &self.dock.dbm;
        if m.conn_addrs.is_some() || !(m.conn_open || m.done.is_some()) {
            return;
        }
        if self.demo {
            self.dock.dbm.conn_addrs = Some("192.168.1.10\n".into());
            return;
        }
        let tag = format!("dbm:addr:{}", inst.container.name);
        if m.inflight.contains(&tag) {
            return;
        }
        if let Some(d) = self.dbm_docker() {
            d.shell_run(&tag, dbinst::ADDR_SCRIPT);
            self.dock.dbm.inflight.push(tag);
        }
    }

    /// 打开「连接信息」:现读容器环境变量里的管理员账号和密码
    fn dbm_conn_open(&mut self, cx: &mut Context<Self>) {
        let Some(inst) = self.dbm_inst() else { return };
        let m = &mut self.dock.dbm;
        m.conn_open = true;
        m.conn_show = false;
        m.conn_menu = false;
        m.conn = None;
        if self.demo {
            self.dock.dbm.conn = Some(Ok(("postgres".into(), zeroize::Zeroizing::new("demo-password-123".into()))));
        } else if let Some(d) = self.dbm_docker() {
            let tag = format!("dbm:conn:{}", inst.container.name);
            d.db_run(&tag, inst.kind.conn_args(&inst.container.name), None);
            self.dock.dbm.inflight.push(tag);
        }
        cx.notify();
    }

    /// 「连接信息」:现读的管理员账号和密码
    fn dbm_conn_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.dock.dbm.conn_open {
            return None;
        }
        let (status, user, pw) = match &self.dock.dbm.conn {
            None => (CredStatus::Loading, String::new(), None),
            Some(Err(e)) => (CredStatus::Error(e.clone()), String::new(), None),
            Some(Ok((u, p))) => (CredStatus::Ready, u.clone(), (!p.is_empty()).then(|| p.to_string())),
        };
        let card = CredCard { title: "连接信息", desc: Some("管理员账号取自容器的环境变量,只在这里显示,关闭后不保留。"), db: None, user, pw, status, one_time: false, close: CredClose::Conn, pw_missing: "容器环境变量里没有" };
        self.dbm_cred_card(card, cx)
    }

    /// 凭据卡片(「连接信息」「数据库已创建」「密码已重置」共用):标题行带关闭、说明、字段列表(主机/端口/用户名/密码,各带复制,
    /// 主机行有「复制连接串」,可选本机/内网/公网地址)。`one_time` 时底部多一个「我已保存」,提醒密码只显示这一次。
    fn dbm_cred_card(&mut self, card: CredCard, cx: &mut Context<Self>) -> Option<AnyElement> {
        let inst = self.dbm_inst()?;
        let host = self.workspace.active.and_then(|a| self.workspace.tabs.get(a)).and_then(|t| t.node()).and_then(|id| self.groups.iter().flat_map(|g| g.nodes.iter()).find(|n| n.id == id)).map(|n| crate::ctl::split_user_host(&n.user_host).1).unwrap_or_default();
        let local_only = inst.container.ports.first().is_some_and(|p| p.starts_with("127.0.0.1:"));
        let show = self.dock.dbm.conn_show;
        let CredCard { title, desc, db, user, pw, status, one_time, close, pw_missing } = card;
        let ready = matches!(status, CredStatus::Ready);
        let do_close = move |this: &mut Self, cx: &mut Context<Self>| {
            let m = &mut this.dock.dbm;
            match close {
                CredClose::Conn => {
                    m.conn_open = false;
                    m.conn = None;
                }
                CredClose::Done => m.done = None,
            }
            m.conn_show = false;
            m.conn_menu = false;
            cx.notify();
        };
        // 一行一个字段:描边小框,左边标签、中间等宽值、右边操作(显示/复制)
        let row = |id: &'static str, k: &'static str, v: String, copy: Option<String>, extra: Option<AnyElement>| {
            div()
                .h(px(42.0))
                .pl(px(14.0))
                .pr(px(8.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .rounded(px(t::RADIUS_MD))
                .bg(c::field())
                .border_1()
                .border_color(c::field_border())
                .child(div().w(px(52.0)).flex_shrink_0().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(k))
                .child(div().flex_1().min_w_0().truncate().font_family(MONO).text_size(px(t::TYPE_BODY)).child(v))
                .children(extra)
                .children(copy.map(|text| super::syspages::tip(super::dockerpages::icon_btn((id, 0), IconName::Copy, c::muted()), "复制").on_click(cx.listener(move |_, _, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))))))
        };
        let head = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(Self::icon_tile(IconName::Database, c::accent(), c::accent().opacity(0.16)).size(px(36.0)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(title))
                    .child(div().truncate().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(format!("{} · {}", inst.kind.label(), inst.container.name))),
            )
            .child(super::syspages::tip(super::dockerpages::icon_btn(("dbm-cred-x", 0), IconName::X, c::muted()).on_click(cx.listener(move |this, _, _, cx| do_close(this, cx))), "关闭"));

        // 主机行:复制按钮前面多一个「复制连接串」,点开选用哪个地址(本机 / 内网 / 公网)
        let node_host = host.clone();
        let (private, public) = self.dock.dbm.conn_addrs.as_deref().map(|o| dbinst::classify_addrs(o, &node_host)).unwrap_or_default();
        let mut addrs: Vec<(String, &'static str)> = vec![("127.0.0.1".into(), "本机")];
        if !local_only {
            addrs.extend(private.into_iter().map(|a| (a, "内网")));
            addrs.extend(public.into_iter().map(|a| (a, "公网")));
        }
        let uri_btn = ready.then(|| {
            super::syspages::tip(super::dockerpages::icon_btn(("dbm-cred-uri", 0), IconName::Link, c::muted()), "复制连接串")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.dock.dbm.conn_menu = !this.dock.dbm.conn_menu;
                    cx.notify();
                }))
                .into_any_element()
        });
        let port = inst_port(&inst);
        let secret = std::rc::Rc::new(zeroize::Zeroizing::new(pw.clone().unwrap_or_default()));
        let menu = (self.dock.dbm.conn_menu && ready).then(|| {
            let (t1, t2) = (cx.entity(), cx.entity());
            let items: Vec<crate::ui::MenuItem> = addrs.iter().map(|(a, k)| crate::ui::MenuItem::new(a.clone()).sub(*k)).collect();
            let (kind, port, user, secret) = (inst.kind, port.clone(), user.clone(), secret.clone());
            crate::ui::dropdown_menu(
                "dbm-cred-menu",
                items,
                None,
                crate::ui::MenuSide::Below,
                46.0,
                move |i, _, cx| {
                    t1.update(cx, |h, cx| {
                        if let Some((a, _)) = addrs.get(i) {
                            cx.write_to_clipboard(ClipboardItem::new_string(dbinst::conn_uri(kind, &user, &secret, a, &port, false)));
                        }
                        h.dock.dbm.conn_menu = false;
                        cx.notify();
                    })
                },
                move |_, cx| {
                    t2.update(cx, |h, cx| {
                        h.dock.dbm.conn_menu = false;
                        cx.notify();
                    })
                },
            )
        });
        let mut fields = div().flex().flex_col().gap(px(8.0));
        if let Some(db) = db.filter(|d| !d.is_empty()) {
            fields = fields.child(row("dbm-cred-db", "数据库", db.clone(), Some(db), None));
        }
        fields = fields.child(div().relative().child(row("dbm-cred-host", "主机", host.clone(), Some(host.clone()), uri_btn)).children(menu));
        fields = fields.child(row("dbm-cred-port", "端口", port.clone(), Some(port), None));
        match &status {
            CredStatus::Loading => fields = fields.child(div().py(px(10.0)).child(loading("正在读取账号…"))),
            CredStatus::Error(e) => fields = fields.child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::danger()).child(e.clone())),
            CredStatus::Ready => {
                if !user.is_empty() {
                    fields = fields.child(row("dbm-cred-user", "用户名", user.clone(), Some(user.clone()), None));
                }
                match &pw {
                    None => fields = fields.child(row("dbm-cred-pw", "密码", "未取到".into(), None, Some(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(pw_missing).into_any_element()))),
                    Some(pw) => {
                        let eye = super::dockerpages::icon_btn(("dbm-cred-eye", 0), if show { IconName::EyeOff } else { IconName::Eye }, c::muted()).on_click(cx.listener(|this, _, _, cx| {
                            this.dock.dbm.conn_show = !this.dock.dbm.conn_show;
                            cx.notify();
                        }));
                        fields = fields.child(row("dbm-cred-pw", "密码", if show { pw.clone() } else { "••••••••••••".into() }, Some(pw.clone()), Some(eye.into_any_element())));
                    }
                }
            }
        }
        let mut body = div().p(px(20.0)).flex().flex_col().gap(px(14.0)).child(head);
        if let Some(d) = desc {
            body = body.child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(d));
        }
        body = body.child(fields);
        if local_only {
            body = body.child(div().flex().flex_row().items_center().gap(px(8.0)).text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(icon(IconName::Info, 13.0, c::muted())).child("端口只绑定在服务器本机(127.0.0.1),外部要通过 SSH 隧道连接"));
        }
        if one_time {
            body = body.child(div().flex().flex_row().justify_end().child(button("dbm-cred-ok", IconName::Check, "我已保存").on_click(cx.listener(move |this, _, _, cx| do_close(this, cx)))));
        }
        let esc = cx.listener(move |this: &mut Self, e: &KeyDownEvent, _, cx| {
            if e.keystroke.key == "escape" {
                do_close(this, cx);
                cx.stop_propagation();
            }
        });
        Some(crate::ui::modal("dbm-cred", 480.0, esc, [body.into_any_element()]).into_any_element())
    }

    /// 数据库页的弹层:新建数据库抽屉、创建成功后的一次性密码
    pub(crate) fn dbm_overlays(&mut self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v = Vec::new();
        v.extend(self.dbm_drawer(window, cx));
        v.extend(self.dbm_done_dialog(cx));
        v.extend(self.dbm_reset_dialog(cx));
        v.extend(self.dbm_drop_dialog(cx));
        v.extend(self.dbm_udrop_dialog(cx));
        v.extend(self.dbm_user_dialog(window, cx));
        v.extend(self.dbm_conn_dialog(cx));
        v.extend(self.dbm_bk_overlays(cx));
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
            .gap(px(8.0))
            .when(busy, |d| d.child(crate::ui::dot_spinner(("dbm-ok-spin", 0), c::on_accent())))
            .child(if busy { "创建中" } else { "创建" })
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

    /// 创建数据库 / 重置密码成功后,一次性显示账号和密码(库里只存哈希,之后读不回来)
    fn dbm_done_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (db, user, pw) = self.dock.dbm.done.clone()?;
        let kind = self.dock.dbm.done_kind;
        let card = CredCard {
            title: ["数据库已创建", "密码已重置", "用户已创建"][kind.min(2) as usize],
            desc: Some(match kind {
                1 => "原密码已立即失效,使用旧密码的应用需要改成新密码。新密码只在这里显示这一次,Otter 不会保存。",
                2 => "用户已创建。密码只在这里显示这一次,Otter 不会保存,数据库里也只存哈希,请现在复制。忘了只能重置密码。",
                _ => "密码只在这里显示这一次,Otter 不会保存,数据库里也只存哈希,请现在复制。忘了只能重置密码。",
            }),
            db: Some(db),
            user,
            pw: Some(pw),
            status: CredStatus::Ready,
            one_time: true,
            close: CredClose::Done,
            pw_missing: "",
        };
        self.dbm_cred_card(card, cx)
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
        header = header.child(head("连接").w(px(60.0))).child(head("大小").w(px(90.0))).child(head(if pg { "描述" } else { "" }).flex_1().min_w_0()).child(head("操作").w(px(110.0)).text_right());
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
                        .w(px(110.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .items_center()
                        .gap(px(2.0))
                        .child(tip(icon_btn_c(("dbm-db-bk", i), IconName::DatabaseBackup, c::muted()), "备份与恢复").on_click(cx.listener({
                            let n = name.clone();
                            move |this, _, _, cx| this.dbm_bk_open(n.clone(), cx)
                        })))
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
        let header = div().child(head("用户").flex_1().min_w_0()).child(head(if mysql { "主机" } else { "权限" }).w(px(240.0))).child(head("操作").w(px(110.0)).text_right());
        let body = rows
            .iter()
            .enumerate()
            .map(|(i, u)| {
                row(("dbm-user", i))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(u.name.clone()))
                    .child(div().w(px(240.0)).truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(if mysql { u.host.clone() } else { u.flags.clone() }))
                    .child({
                        let (n, h) = (u.name.clone(), u.host.clone());
                        let (n2, h2, n3, h3) = (u.name.clone(), u.host.clone(), u.name.clone(), u.host.clone());
                        div()
                            .w(px(110.0))
                            .flex()
                            .flex_row()
                            .justify_end()
                            .items_center()
                            .children(u.system.then(|| tag("系统")))
                            .children((!u.system).then(|| {
                                super::syspages::tip(super::dockerpages::icon_btn_c(("dbm-uedit", i), IconName::Pencil, c::muted()), "编辑权限").on_click(cx.listener(move |this, _, window, cx| this.dbm_open_edit(n3.clone(), h3.clone(), window, cx)))
                            }))
                            .children((!u.system).then(|| {
                                super::syspages::tip(super::dockerpages::icon_btn_c(("dbm-reset", i), IconName::KeyRound, c::muted()), "重置密码").on_click(cx.listener(move |this, _, _, cx| {
                                    this.dock.dbm.reset_ask = Some((n.clone(), h.clone()));
                                    cx.notify();
                                }))
                            }))
                            .children((!u.system).then(|| {
                                super::syspages::tip(super::dockerpages::icon_btn_c(("dbm-udel", i), IconName::Trash, c::danger()), "删除用户").on_click(cx.listener(move |this, _, _, cx| {
                                    this.dock.dbm.udrop_ask = Some((n2.clone(), h2.clone()));
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
        // 一组「标签 + 值」:只看信息和统计,不列具体的 key
        let cell = |k: &'static str, v: String| {
            div()
                .w(px(150.0))
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(k))
                .child(div().truncate().text_size(px(t::TYPE_BODY)).font_family(MONO).text_color(c::text()).child(if v.is_empty() { "-".to_string() } else { v }))
        };
        let group = |title: &'static str, cells: Vec<Div>| {
            div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .p(px(16.0))
                .rounded(px(t::RADIUS_XL))
                .border_1()
                .border_color(c::dialog_border())
                .child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::BOLD).text_color(c::muted()).child(title))
                .child(div().flex().flex_row().flex_wrap().gap_x(px(24.0)).gap_y(px(14.0)).children(cells))
        };
        let total: u64 = r.keys.iter().map(|(_, n)| n).sum();
        let looked = r.hits + r.misses;
        let rate = if looked == 0 { "-".to_string() } else { format!("{:.1}%", r.hits as f64 * 100.0 / looked as f64) };
        let max = if r.max_memory == "0B" || r.max_memory.is_empty() { "不限".to_string() } else { r.max_memory.clone() };
        let spaces = if r.keys.is_empty() { "无".to_string() } else { r.keys.iter().map(|(n, k)| format!("{n}: {k}")).collect::<Vec<_>>().join(" · ") };
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(group("信息", vec![cell("版本", r.version.clone()), cell("角色", r.role.clone()), cell("模式", r.mode.clone()), cell("运行天数", r.uptime_days.clone())]))
            .child(group("内存与连接", vec![cell("已用内存", r.memory.clone()), cell("内存峰值", r.peak_memory.clone()), cell("内存上限", max), cell("连接数", r.clients.clone())]))
            .child(group("统计", vec![cell("Key 总数", total.to_string()), cell("每秒命令", r.ops.clone()), cell("累计命令", r.commands.clone()), cell("命中率", rate), cell("已过期", r.expired.clone()), cell("被淘汰", r.evicted.clone())]))
            .child(div().px(px(16.0)).text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(format!("各库 Key 数  {spaces}")))
            .into_any_element()
    }

    /// 演示模式:不连服务器,给一份固定的示例数据
    fn dbm_demo(&mut self, inst: &DbInstance) {
        let m = &mut self.dock.dbm;
        let db = |n: &str, e: &str, s: u64, sys: bool| DbRow { name: n.into(), extra: e.into(), enc: if e.starts_with("utf") { String::new() } else { "UTF8".into() }, conns: Some(if sys { 0 } else { 2 }), size: Some(s), collate: "en_US.utf8".into(), tables: Some(5), desc: if sys { String::new() } else { "演示数据库".into() }, system: sys };
        m.dbs = Some(Ok(vec![db("app", if inst.kind == DbKind::Postgres { "app_owner" } else { "utf8mb4" }, 48 << 20, false), db("postgres", "postgres", 7 << 20, true)]));
        m.users = Some(Ok(vec![UserRow { name: "app_owner".into(), host: "%".into(), flags: "可建库".into(), system: false }, UserRow { name: "postgres".into(), host: "localhost".into(), flags: "超级用户".into(), system: true }]));
        m.cred = Some("postgres".into());
        m.redis = Some(Ok(RedisInfo { version: "7.2.4".into(), memory: "1.2M".into(), clients: "3".into(), uptime_days: "12".into(), role: "master".into(), mode: "standalone".into(), max_memory: "0B".into(), peak_memory: "1.5M".into(), ops: "2".into(), commands: "10432".into(), hits: 900, misses: 100, expired: "4".into(), evicted: "0".into(), keys: vec![("db0".into(), 128)] }));
    }
}

/// 实例的主端口(如 `5432`),没有映射时显示容器名
fn inst_port(d: &DbInstance) -> String {
    d.container.ports.first().and_then(|p| p.split("->").next()).map(|p| p.rsplit(':').next().unwrap_or(p).to_string()).unwrap_or_else(|| d.kind.port().to_string())
}
