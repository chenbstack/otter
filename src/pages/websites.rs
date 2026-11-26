//! 网站页:安装 OpenResty、站点(反向代理/静态/跳转)、证书(申请/上传/自签)、自动续期、DNS 账号。
//! 纯逻辑和脚本在 `crate::web`,服务器目录在 `crate::srvdata`,设计见 `docs/modules/websites.md`。
//! 所有操作以 root 身份在服务器上执行(`Kind::Web*`);界面从不读回 DNS 密钥,只显示「已保存」。

use std::time::{Duration, Instant};

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};

use super::daemonpage::{field, labeled, switch};
use super::super::*;
use super::dockerpages::{head, icon_btn, row, small, table_flow};
use super::syspages::tip;
use crate::confirm::{Confirm, ConfirmKind};
use crate::sysops::Kind;
use crate::web::{self, CertEntry, DnsAcct, IssueReq, SiteEntry, SiteKind, SiteSpec, WebState};

// 尺寸 token(见 docs/design/tokens.md「网站页」)
const DLG_W_SELF: f32 = 520.0;
const DLG_W_DNS: f32 = 540.0;
const DLG_W_SITE: f32 = 560.0;
const DLG_W_SITE_FORM: f32 = 540.0;
const DLG_W_ISSUE: f32 = 580.0;
const DLG_W_RAW: f32 = 720.0;
const DLG_W_LOG: f32 = 760.0;
const LOG_H: f32 = 360.0;
const COL_SITE_DOMAIN: f32 = 250.0;
const COL_SITE_KIND: f32 = 190.0;
const COL_SITE_ACT: f32 = 130.0;
const COL_CERT_NAME: f32 = 210.0;
const COL_CERT_SRC: f32 = 150.0;
const COL_CERT_EXP: f32 = 150.0;
const COL_CERT_USE: f32 = 70.0;
const COL_CERT_ACT: f32 = 60.0;

/// 后台任务的轮询间隔
const POLL_EVERY: Duration = Duration::from_secs(2);
/// 任务日志面板显示的行数
const JOB_LINES: usize = 12;

#[derive(Clone, PartialEq)]
pub(crate) enum Dlg {
    /// 新建(None)或编辑(Some(配置文件名))站点
    Site(Option<String>),
    /// 直接编辑配置原文
    Raw(String),
    CertUpload,
    CertSelf,
    Issue,
    Dns,
    Logs,
}

#[derive(Clone, Copy, PartialEq, Default)]
enum OpTag {
    #[default]
    Plain,
    /// 站点/证书/账号保存:成功后关掉弹窗
    Save,
    Preflight,
    Logs,
    /// 启动后台任务
    JobStart,
}

#[derive(Clone, Copy, PartialEq)]
enum ConfAct {
    Uninstall,
    Stop,
    RemoveCron,
    FixPerms,
}

enum Pending {
    Act(ConfAct),
    DeleteSite(String),
    DeleteCert(String),
    DeleteDns(String),
    Adopt(web::Target),
    Unadopt,
}

struct WebConfirm {
    title: &'static str,
    name: String,
    detail: String,
    points: Vec<(IconName, Rgba, String)>,
    label: &'static str,
    danger: bool,
    act: Pending,
}

/// 后台任务(安装、申请证书)的进度
pub(crate) struct JobView {
    title: String,
    offset: usize,
    log: String,
    last: Instant,
    started: Instant,
    /// 退出码;None 是还在跑
    done: Option<i32>,
    /// 证书申请任务的证书名(成功后提示去站点里启用)
    cert: Option<String>,
}

/// 站点表单里的一组监听
#[derive(Clone)]
struct LForm {
    domains: Entity<InputState>,
    port: Entity<InputState>,
    ssl: bool,
    cert: Option<String>,
    force: bool,
}

#[derive(Default)]
pub(crate) struct WebUi {
    tab: u8,
    st: Option<Result<WebState, String>>,
    busy: Option<&'static str>,
    tag: OpTag,
    err: Option<String>,
    dlg: Option<Dlg>,
    ins: Vec<Entity<InputState>>,
    area: Vec<Entity<TextareaState>>,
    // 站点表单
    s_kind: u8,
    s_ws: bool,
    s_host: bool,
    s_spa: bool,
    s_index: bool,
    s_code: u16,
    s_ls: Vec<LForm>,
    s_ipv6: bool,
    s_hsts: bool,
    /// 后端地址/跳转地址的协议:0 http,1 https
    s_scheme: u8,
    s_scheme_open: bool,
    /// 高级区是否展开
    s_adv: bool,
    s_enabled: bool,
    // 申请证书表单
    i_zero: bool,
    i_dns: bool,
    i_acct: Option<String>,
    i_staging: bool,
    i_auto: bool,
    i_warn: Option<Vec<String>>,
    // DNS 账号表单
    d_prov: usize,
    // 日志窗口
    log_text: String,
    log_site: Option<String>,
    log_err: bool,
    job: Option<JobView>,
    job_checked: bool,
    auto_adopt_tried: bool,
    confirm: Option<WebConfirm>,
    demo_filled: bool,
}

/// `https://x:1` → (1, `x:1`);没有协议当 http
fn split_scheme(url: &str) -> (u8, String) {
    match url.strip_prefix("https://") {
        Some(r) => (1, r.to_string()),
        None => (0, url.strip_prefix("http://").unwrap_or(url).to_string()),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn opt_chip(id: (&'static str, usize), label: impl Into<SharedString>, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.0))
        .px(px(11.0))
        .flex()
        .items_center()
        .cursor_pointer()
        .text_size(px(t::TYPE_CONTROL))
        .when(active, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::SEMIBOLD))
        .when(!active, |d| d.rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).text_color(c::muted()).hover(|s| s.bg(c::field_hover()).text_color(c::secondary())))
        .child(label.into())
}

fn pill(text: impl Into<SharedString>, fg: Rgba, bg: Rgba) -> Div {
    div().h(px(20.0)).px(px(7.0)).flex().items_center().rounded(px(t::RADIUS_XS)).bg(bg).text_size(px(t::TYPE_MICRO)).text_color(fg).child(text.into())
}

fn card(title: &'static str, sub: impl Into<SharedString>, right: Option<AnyElement>, body: impl IntoElement) -> Div {
    div()
        .flex_shrink_0()
        .rounded(px(t::RADIUS_XL))
        .border_1()
        .border_color(c::dialog_border())
        .p(px(14.0))
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(title))
                .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(sub.into()))
                .children(right),
        )
        .child(body)
}

fn area_field(label: &'static str, hint: &'static str, e: Option<&Entity<TextareaState>>, h: f32) -> Div {
    labeled(
        label,
        hint,
        div()
            .h(px(h))
            .rounded(px(t::RADIUS_MD))
            .bg(c::field())
            .border_1()
            .border_color(c::field_border())
            .font_family(MONO)
            .text_size(px(t::TYPE_CONTROL))
            .children(e.map(|e| Textarea::new(e).appearance(false).with_size(px(12.0 / 0.875)).h_full())),
    )
}

fn toggle(id: &'static str, label: &'static str, hint: &'static str, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .cursor_pointer()
        .child(switch(id, on))
        .child(div().flex().flex_col().child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(label)).child(div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(hint)))
}

fn days_color(d: i64) -> Rgba {
    if d < 7 {
        c::danger()
    } else if d < 30 {
        c::warn()
    } else {
        c::online()
    }
}

fn fmt_days(d: i64) -> String {
    if d < 0 { format!("已过期 {} 天", -d) } else if d == 0 { "今天到期".into() } else { format!("{d} 天后到期") }
}

impl Home {
    pub(super) fn web_installed(&self) -> bool {
        matches!(&self.web_ui().st, Some(Ok(s)) if s.installed())
    }

    fn web_ui(&self) -> &WebUi {
        &self.dock.sys.web
    }

    fn web_mut(&mut self) -> &mut WebUi {
        &mut self.dock.sys.web
    }

    fn web_target(&self) -> web::Target {
        self.web_state().map(|s| s.target.clone()).unwrap_or_default()
    }

    pub(super) fn web_state_ref(&self) -> Option<&WebState> {
        self.web_state()
    }

    fn web_state(&self) -> Option<&WebState> {
        match &self.dock.sys.web.st {
            Some(Ok(s)) => Some(s),
            _ => None,
        }
    }

    fn web_notice(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.dock.notice = Some((msg.into(), Instant::now()));
        cx.notify();
    }

    // ---------------------------------------------------------------- 数据流

    pub(super) fn web_loaded(&mut self, res: Result<String, String>, cx: &mut Context<Self>) {
        let w = self.web_mut();
        w.st = Some(res.map(|t| web::parse_state(&t)));
        // 第一次读到状态:看服务器上有没有没做完(或没关掉)的后台任务
        if !w.job_checked && matches!(w.st, Some(Ok(_))) {
            w.job_checked = true;
            self.sys_ask_poll(0);
        }
        // 已有的 nginx/OpenResty 容器:直接接管(只写一个 target.env,容器不动),不用用户点确认;每次打开只试一次
        let auto = if self.web_ui().auto_adopt_tried || self.demo { None } else { self.web_state().and_then(WebState::auto_adopt_target) };
        if let Some(t) = auto {
            self.web_mut().auto_adopt_tried = true;
            match web::adopt_script(&t) {
                Ok(s) => self.web_run("接入 OpenResty", s, OpTag::Plain, cx),
                Err(e) => self.web_notice(e, cx),
            }
        }
        cx.notify();
    }

    fn sys_ask_poll(&mut self, offset: usize) {
        self.sys_ask(Kind::WebPoll, &web::job_poll_script(offset), true);
    }

    /// 要读取一次状态
    fn web_refresh(&mut self) {
        self.dock.sys.asked.remove(&Kind::Web);
    }

    fn web_run(&mut self, label: &'static str, script: String, tag: OpTag, cx: &mut Context<Self>) {
        if self.demo {
            return self.web_notice("演示数据,不会真的执行", cx);
        }
        if self.web_ui().busy.is_some() {
            return;
        }
        let w = self.web_mut();
        w.busy = Some(label);
        w.tag = tag;
        w.err = None;
        self.sys_ask(Kind::WebOp, &script, true);
        cx.notify();
    }

    pub(super) fn web_op_done(&mut self, res: Result<String, String>, cx: &mut Context<Self>) {
        let label = self.web_ui().busy.unwrap_or("操作");
        let tag = self.web_ui().tag;
        self.web_mut().busy = None;
        let parsed = res.and_then(|t| web::parse_op(&t));
        match parsed {
            Err(e) => {
                if tag == OpTag::JobStart {
                    self.web_mut().job = None;
                }
                if self.web_ui().dlg.is_some() {
                    self.web_mut().err = Some(e);
                } else {
                    self.web_notice(format!("{label}失败:{e}"), cx);
                }
                // 申请前的预检失败不影响继续
                if tag == OpTag::Preflight {
                    self.web_mut().err = None;
                    self.web_mut().i_warn = Some(Vec::new());
                }
            }
            Ok(out) => match tag {
                OpTag::Preflight => {
                    let running = self.web_state().is_some_and(WebState::running);
                    let warns = web::preflight_warnings(&out, running);
                    if warns.is_empty() {
                        self.web_mut().i_warn = Some(Vec::new());
                        self.web_issue_start(cx);
                    } else {
                        self.web_mut().i_warn = Some(warns);
                    }
                }
                OpTag::Logs => {
                    self.web_mut().log_text = out;
                }
                OpTag::JobStart => {
                    self.web_mut().dlg = None;
                    self.web_mut().job.get_or_insert_with(|| JobView { title: label.into(), offset: 0, log: String::new(), last: Instant::now() - POLL_EVERY, started: Instant::now(), done: None, cert: None });
                }
                OpTag::Save | OpTag::Plain => {
                    if tag == OpTag::Save {
                        self.web_mut().dlg = None;
                    }
                    let msg = if out.is_empty() { format!("已完成:{label}") } else { format!("{label}:{}", out.lines().next().unwrap_or("")) };
                    self.web_notice(msg, cx);
                    self.web_refresh();
                }
            },
        }
        cx.notify();
    }

    pub(super) fn web_polled(&mut self, res: Result<String, String>, cx: &mut Context<Self>) {
        let Ok(raw) = res else { return };
        let p = web::parse_job_poll(&raw);
        let w = self.web_mut();
        if w.job.is_none() {
            // 刚连上就发现服务器上留着任务(没跑完,或跑完了没关)
            if !p.has {
                return;
            }
            w.job = Some(JobView { title: "后台任务".into(), offset: 0, log: String::new(), last: Instant::now() - POLL_EVERY, started: Instant::now(), done: None, cert: None });
        }
        let Some(j) = w.job.as_mut() else { return };
        j.offset += p.log.len();
        j.log.push_str(&p.log);
        j.last = Instant::now();
        if let Some(code) = p.status {
            j.done = Some(code);
        } else if !p.alive && j.started.elapsed() > Duration::from_secs(10) {
            // 进程没了又没有退出码:被中断(服务器重启、被杀)
            j.done = Some(-1);
        }
        if j.done.is_some() {
            self.web_refresh();
        }
        cx.notify();
    }

    /// 页面前台时要不要重绘:后台任务到了该轮询的时间
    pub(crate) fn web_due(&self) -> bool {
        self.page == Page::Websites && !self.demo && self.web_ui().job.as_ref().is_some_and(|j| j.done.is_none() && j.last.elapsed() >= POLL_EVERY) && !self.dock.sys.inflight.contains(&Kind::WebPoll)
    }

    /// 每次渲染前:到时间就去轮询后台任务
    pub(super) fn web_pump(&mut self) {
        if self.demo {
            return;
        }
        let due = self.web_ui().job.as_ref().filter(|j| j.done.is_none() && j.last.elapsed() >= POLL_EVERY).map(|j| j.offset);
        if let Some(off) = due {
            if !self.dock.sys.inflight.contains(&Kind::WebPoll) {
                self.sys_ask_poll(off);
            }
        }
    }

    pub(super) fn web_job_start(&mut self, title: &'static str, body: String, cert: Option<String>, cx: &mut Context<Self>) {
        self.web_mut().job = None;
        self.web_run(title, web::job_start_script(&body), OpTag::JobStart, cx);
        // 先放一个任务视图,启动失败时 `web_op_done` 会提示
        if self.web_ui().busy.is_some() {
            self.web_mut().job = Some(JobView { title: title.into(), offset: 0, log: String::new(), last: Instant::now(), started: Instant::now(), done: None, cert });
        }
    }

    fn web_job_close(&mut self, cx: &mut Context<Self>) {
        self.web_mut().job = None;
        self.web_mut().job_checked = true;
        if !self.demo {
            self.web_run("清理任务", web::job_clear_script(), OpTag::Plain, cx);
        }
        cx.notify();
    }

    // ---------------------------------------------------------------- 表单打开

    fn web_inputs(&mut self, hints: &[&str], masked: &[usize], window: &mut Window, cx: &mut Context<Self>) {
        let v: Vec<Entity<InputState>> = hints
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let m = masked.contains(&i);
                let e = cx.new(|cx| {
                    let s = InputState::new(window, cx).placeholder(*h);
                    if m { s.masked(true) } else { s }
                });
                cx.observe(&e, |_, _, cx| cx.notify()).detach();
                e
            })
            .collect();
        self.web_mut().ins = v;
        self.web_mut().area.clear();
        self.web_mut().err = None;
    }

    fn web_areas(&mut self, rows: &[(usize, &'static str)], window: &mut Window, cx: &mut Context<Self>) {
        let v: Vec<Entity<TextareaState>> = rows
            .iter()
            .map(|(r, ph)| {
                let e = cx.new(|cx| TextareaState::new(window, cx).placeholder(*ph).rows(*r));
                cx.observe(&e, |_, _, cx| cx.notify()).detach();
                e
            })
            .collect();
        self.web_mut().area = v;
    }

    fn web_set(&self, i: usize, v: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(e) = self.web_ui().ins.get(i) {
            e.update(cx, |s, cx| s.set_value(v, window, cx));
        }
    }

    fn web_val(&self, i: usize, cx: &App) -> String {
        self.web_ui().ins.get(i).map(|e| e.read(cx).value().trim().to_string()).unwrap_or_default()
    }

    fn web_area_val(&self, i: usize, cx: &App) -> String {
        self.web_ui().area.get(i).map(|e| e.read(cx).value().to_string()).unwrap_or_default()
    }

    fn web_new_listener(&self, domains: &str, port: u16, ssl: bool, cert: Option<String>, force: bool, window: &mut Window, cx: &mut Context<Self>) -> LForm {
        let mk = |ph: &str, v: &str, window: &mut Window, cx: &mut Context<Self>| {
            let e = cx.new(|cx| InputState::new(window, cx).placeholder(ph.to_string()));
            e.update(cx, |s, cx| s.set_value(v, window, cx));
            cx.observe(&e, |_, _, cx| cx.notify()).detach();
            e
        };
        LForm { domains: mk("", domains, window, cx), port: mk("", &port.to_string(), window, cx), ssl, cert, force }
    }

    fn web_open_site(&mut self, edit: Option<SiteEntry>, window: &mut Window, cx: &mut Context<Self>) {
        self.web_inputs(&["留空则用第一个域名", "127.0.0.1:8080", "如 100m,留空沿用全局设置", "默认 300"], &[], window, cx);
        let w = self.web_mut();
        (w.s_kind, w.s_ws, w.s_host, w.s_spa, w.s_index, w.s_code, w.s_ipv6, w.s_hsts, w.s_enabled, w.s_scheme, w.s_scheme_open, w.s_adv) = (0, true, true, true, false, 301, false, false, true, 0, false, false);
        let mut forms: Vec<LForm> = Vec::new();
        if let Some(e) = &edit {
            if let Some(s) = &e.spec {
                self.web_mut().s_enabled = e.enabled;
                self.web_mut().s_ipv6 = s.ipv6;
                self.web_mut().s_hsts = s.hsts;
                let w = self.web_mut();
                let target = match &s.kind {
                    SiteKind::Proxy { target, ws, host } => {
                        (w.s_kind, w.s_ws, w.s_host) = (0, *ws, *host);
                        target.clone()
                    }
                    SiteKind::Static { spa, index } => {
                        (w.s_kind, w.s_spa, w.s_index) = (1, *spa, *index);
                        String::new()
                    }
                    SiteKind::Redirect { to, code } => {
                        (w.s_kind, w.s_code) = (2, *code);
                        to.clone()
                    }
                };
                let (scheme, addr) = split_scheme(&target);
                self.web_mut().s_scheme = scheme;
                let name = s.name.clone();
                self.web_set(0, &name, window, cx);
                self.web_set(1, &addr, window, cx);
                self.web_set(2, s.adv.max_body.as_deref().unwrap_or(""), window, cx);
                self.web_set(3, &s.adv.read_timeout.map(|t| t.to_string()).unwrap_or_default(), window, cx);
                // 有改过的高级项就默认展开
                let non_default = s.ipv6 || s.hsts || s.adv != web::Advanced::default() || matches!(s.kind, SiteKind::Proxy { ws: false, .. });
                self.web_mut().s_adv = non_default;
                for l in &s.listeners {
                    forms.push(self.web_new_listener(&l.domains.join(" "), l.port, l.ssl(), l.cert.clone(), l.force_https, window, cx));
                }
            }
        }
        if forms.is_empty() {
            forms.push(self.web_new_listener("", self.web_target().ports.0, false, None, false, window, cx));
        }
        self.web_mut().s_ls = forms;
        self.web_mut().dlg = Some(Dlg::Site(edit.map(|e| e.file)));
        cx.notify();
    }

    fn web_open_raw(&mut self, e: &SiteEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.web_inputs(&[], &[], window, cx);
        self.web_areas(&[(18, "")], window, cx);
        if let Some(a) = self.web_ui().area.first().cloned() {
            let text = e.text.clone();
            a.update(cx, |s, cx| s.set_value(text.as_str(), window, cx));
        }
        self.web_mut().dlg = Some(Dlg::Raw(e.file.clone()));
        cx.notify();
    }

    fn web_open_upload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.web_inputs(&["证书名,如 example.com"], &[], window, cx);
        self.web_areas(&[(6, "-----BEGIN CERTIFICATE-----"), (6, "-----BEGIN PRIVATE KEY-----")], window, cx);
        self.web_mut().dlg = Some(Dlg::CertUpload);
        cx.notify();
    }

    fn web_open_self(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.web_inputs(&["证书名,留空则用第一个域名", "example.com 或 192.168.1.1.nip.io", "365"], &[], window, cx);
        self.web_set(2, "365", window, cx);
        self.web_mut().dlg = Some(Dlg::CertSelf);
        cx.notify();
    }

    fn web_open_issue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.web_inputs(&["example.com *.example.com", "证书名,留空则用第一个域名", "you@example.com,用于接收到期通知"], &[], window, cx);
        let w = self.web_mut();
        (w.i_zero, w.i_dns, w.i_acct, w.i_staging, w.i_auto, w.i_warn) = (false, false, None, false, true, None);
        // 默认选第一个已有的 DNS 账号
        let first = self.web_state().and_then(|s| s.dns.first().map(|d| d.id.clone()));
        self.web_mut().i_acct = first;
        self.web_mut().dlg = Some(Dlg::Issue);
        cx.notify();
    }

    fn web_open_dns(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.web_inputs(&["账号名,如 主域名", "", ""], &[1, 2], window, cx);
        self.web_mut().d_prov = 0;
        self.web_mut().dlg = Some(Dlg::Dns);
        cx.notify();
    }

    fn web_open_logs(&mut self, site: Option<String>, err: bool, cx: &mut Context<Self>) {
        let w = self.web_mut();
        w.log_site = site.clone();
        w.log_err = err;
        w.log_text.clear();
        w.dlg = Some(Dlg::Logs);
        if let Ok(s) = web::log_script(site.as_deref(), err, 300) {
            self.web_run("读取日志", s, OpTag::Logs, cx);
        }
        cx.notify();
    }

    fn web_close(&mut self, cx: &mut Context<Self>) {
        let w = self.web_mut();
        if w.busy.is_some() && !matches!(w.dlg, Some(Dlg::Logs)) {
            return;
        }
        w.dlg = None;
        w.err = None;
        cx.notify();
    }

    // ---------------------------------------------------------------- 提交

    fn web_site_spec(&self, cx: &App) -> Result<(SiteSpec, bool), String> {
        let w = self.web_ui();
        let mut listeners: Vec<web::Listener> = Vec::new();
        for (i, f) in w.s_ls.iter().enumerate() {
            let domains = web::split_domains(&f.domains.read(cx).value());
            let port: u16 = f.port.read(cx).value().trim().parse().map_err(|_| format!("第 {} 组的端口要填 1–65535 的数字", i + 1))?;
            if f.ssl && f.cert.is_none() {
                return Err(format!("第 {} 组选了 HTTPS,还没选证书", i + 1));
            }
            listeners.push(web::Listener { domains, port, cert: f.cert.clone().filter(|_| f.ssl), force_https: f.force && !f.ssl });
        }
        let first_domain = listeners.iter().find_map(|l| l.domains.first().cloned());
        let name = match &w.dlg {
            Some(Dlg::Site(Some(file))) => file.trim_end_matches(".off").trim_end_matches(".conf").to_string(),
            _ => {
                let n = self.web_val(0, cx);
                if n.is_empty() { first_domain.map(|d| crate::srvdata::name_of_domain(&d)).unwrap_or_default() } else { n }
            }
        };
        let addr = self.web_val(1, cx);
        let target = if addr.contains("://") { addr } else { format!("{}://{addr}", if w.s_scheme == 1 { "https" } else { "http" }) };
        let kind = match w.s_kind {
            0 => SiteKind::Proxy { target, ws: w.s_ws, host: w.s_host },
            1 => SiteKind::Static { spa: w.s_spa, index: w.s_index },
            _ => SiteKind::Redirect { to: target, code: w.s_code },
        };
        let timeout = self.web_val(3, cx);
        let read_timeout = if timeout.is_empty() { None } else { Some(timeout.parse::<u32>().map_err(|_| "读取超时要填秒数".to_string())?) };
        let adv = web::Advanced { max_body: Some(self.web_val(2, cx)).filter(|v| !v.is_empty()), read_timeout };
        let spec = SiteSpec { name, kind, listeners, ipv6: w.s_ipv6, hsts: w.s_hsts && w.s_ls.iter().any(|f| f.ssl), adv };
        spec.check()?;
        // 新建时不能和现有站点重名
        if matches!(w.dlg, Some(Dlg::Site(None))) && self.web_state().is_some_and(|s| s.sites.iter().any(|e| e.file.trim_end_matches(".off").trim_end_matches(".conf") == spec.name)) {
            return Err(format!("已经有叫 {} 的站点", spec.name));
        }
        Ok((spec, w.s_enabled))
    }

    fn web_submit(&mut self, cx: &mut Context<Self>) {
        if self.web_ui().busy.is_some() {
            return;
        }
        let Some(dlg) = self.web_ui().dlg.clone() else { return };
        let tg = self.web_target();
        let res: Result<(&'static str, String), String> = match &dlg {
            Dlg::Site(_) => self.web_site_spec(cx).and_then(|(s, en)| web::site_save_script(&s, en, &tg).map(|sc| ("保存站点", sc))),
            Dlg::Raw(file) => {
                let text = self.web_area_val(0, cx);
                web::site_raw_script(file, &text).map(|s| ("保存配置", s))
            }
            Dlg::CertUpload => {
                let (n, cert, key) = (self.web_val(0, cx), self.web_area_val(0, cx), self.web_area_val(1, cx));
                web::cert_upload_script(&n, &cert, &key, false).map(|s| ("上传证书", s))
            }
            Dlg::CertSelf => {
                let doms = web::split_domains(&self.web_val(1, cx));
                let name = Some(self.web_val(0, cx)).filter(|n| !n.is_empty()).or_else(|| doms.first().map(|d| crate::srvdata::name_of_domain(d))).unwrap_or_default();
                let days: u32 = self.web_val(2, cx).parse().unwrap_or(0);
                if days == 0 { Err("有效期要填天数".into()) } else { web::cert_self_script(&name, &doms, days).map(|s| ("生成自签证书", s)) }
            }
            Dlg::Dns => {
                let (name, k1, k2) = (self.web_val(0, cx), self.web_val(1, cx), self.web_val(2, cx));
                let prov = web::dns_providers()[self.web_ui().d_prov.min(2)];
                if name.is_empty() {
                    Err("给账号起个名字".into())
                } else {
                    let id = crate::srvdata::name_of_domain(&format!("{}-{}", prov.0, name));
                    let all = self.web_state().map(|s| s.dns.clone()).unwrap_or_default();
                    web::dns_account_script(&id, prov.0, &name, [&k1, &k2], &all).map(|s| ("保存 DNS 账号", s))
                }
            }
            Dlg::Issue => return self.web_issue(cx),
            Dlg::Logs => return,
        };
        match res {
            Ok((label, script)) => self.web_run(label, script, OpTag::Save, cx),
            Err(e) => {
                self.web_mut().err = Some(e);
                cx.notify();
            }
        }
    }

    fn web_issue_req(&self, cx: &App) -> Result<IssueReq, String> {
        let w = self.web_ui();
        let domains = web::split_domains(&self.web_val(0, cx));
        let name = Some(self.web_val(1, cx)).filter(|n| !n.is_empty()).or_else(|| domains.first().map(|d| crate::srvdata::name_of_domain(d))).unwrap_or_default();
        let dns = w.i_acct.as_ref().and_then(|id| self.web_state().and_then(|s| s.dns.iter().find(|d| &d.id == id).cloned()));
        let req = IssueReq {
            name,
            domains,
            ca: if w.i_zero { "zerossl" } else { "letsencrypt" }.into(),
            email: self.web_val(2, cx),
            method: if w.i_dns { "dns" } else { "http" }.into(),
            dns: if w.i_dns { dns } else { None },
            staging: w.i_staging,
            auto: w.i_auto,
        };
        req.check()?;
        if self.web_state().is_some_and(|s| s.certs.iter().any(|c| c.name == req.name)) {
            return Err(format!("已经有叫 {} 的证书", req.name));
        }
        Ok(req)
    }

    /// 点「申请」:HTTP 验证先预检(解析、80 端口),有问题提示一次再确认;DNS 验证直接开始
    fn web_issue(&mut self, cx: &mut Context<Self>) {
        let req = match self.web_issue_req(cx) {
            Ok(r) => r,
            Err(e) => {
                self.web_mut().err = Some(e);
                cx.notify();
                return;
            }
        };
        if req.method == "http" && self.web_ui().i_warn.is_none() {
            return self.web_run("检查域名解析", web::preflight_script(&req.domains), OpTag::Preflight, cx);
        }
        self.web_issue_start(cx);
    }

    fn web_issue_start(&mut self, cx: &mut Context<Self>) {
        let req = match self.web_issue_req(cx) {
            Ok(r) => r,
            Err(e) => {
                self.web_mut().err = Some(e);
                return;
            }
        };
        let accts = self.web_state().map(|s| s.acme.clone()).unwrap_or_default();
        match web::issue_job_script(&req, &accts) {
            Ok(body) => {
                let name = req.name.clone();
                self.web_mut().busy = None;
                self.web_job_start("申请证书", body, Some(name), cx);
            }
            Err(e) => self.web_mut().err = Some(e),
        }
    }

    fn web_ctl(&mut self, label: &'static str, action: &str, cx: &mut Context<Self>) {
        match web::ctl_script(action, &self.web_target()) {
            Ok(s) => self.web_run(label, s, OpTag::Plain, cx),
            Err(e) => self.web_notice(e, cx),
        }
    }

    fn web_ask(&mut self, c: WebConfirm, cx: &mut Context<Self>) {
        self.web_mut().confirm = Some(c);
        cx.notify();
    }

    fn web_confirmed(&mut self, cx: &mut Context<Self>) {
        let Some(c) = self.web_mut().confirm.take() else { return };
        match c.act {
            Pending::Act(ConfAct::Uninstall) => self.web_ctl("卸载 OpenResty", "remove", cx),
            Pending::Act(ConfAct::Stop) => self.web_ctl("停止 OpenResty", "stop", cx),
            Pending::Act(ConfAct::RemoveCron) => self.web_run("移除自动续期", web::cron_remove_script(), OpTag::Plain, cx),
            Pending::Act(ConfAct::FixPerms) => self.web_run("修复权限", web::fix_perms_script(), OpTag::Plain, cx),
            Pending::DeleteSite(file) => match web::site_delete_script(&file) {
                Ok(s) => self.web_run("删除站点", s, OpTag::Plain, cx),
                Err(e) => self.web_notice(e, cx),
            },
            Pending::DeleteCert(n) => match web::cert_delete_script(&n) {
                Ok(s) => self.web_run("删除证书", s, OpTag::Plain, cx),
                Err(e) => self.web_notice(e, cx),
            },
            Pending::Adopt(t) => match web::adopt_script(&t) {
                Ok(s) => self.web_run("接管 OpenResty", s, OpTag::Plain, cx),
                Err(e) => self.web_notice(e, cx),
            },
            Pending::Unadopt => self.web_run("取消接管", web::unadopt_script(), OpTag::Plain, cx),
            Pending::DeleteDns(id) => {
                let all = self.web_state().map(|s| s.dns.clone()).unwrap_or_default();
                match web::dns_account_delete_script(&id, &all) {
                    Ok(s) => self.web_run("删除 DNS 账号", s, OpTag::Plain, cx),
                    Err(e) => self.web_notice(e, cx),
                }
            }
        }
        cx.notify();
    }

    // ---------------------------------------------------------------- 演示数据

    pub(super) fn web_demo(&mut self) {
        if self.dock.sys.web.demo_filled {
            return;
        }
        let w = &mut self.dock.sys.web;
        w.demo_filled = true;
        w.job_checked = true;
        let ds = |v: &[&str]| v.iter().map(|d| d.to_string()).collect::<Vec<String>>();
        let ls = |d: &[&str], port: u16, cert: Option<&str>, force: bool| web::Listener { domains: ds(d), port, cert: cert.map(str::to_string), force_https: force };
        let proxy = SiteSpec { name: "app.example.com".into(), kind: SiteKind::Proxy { target: "http://127.0.0.1:3000".into(), ws: true, host: true }, listeners: vec![ls(&["app.example.com"], 80, None, true), ls(&["app.example.com"], 443, Some("app.example.com"), false), ls(&["app.example.com"], 8443, Some("app.example.com"), false)], ipv6: true, hsts: true, adv: Default::default() };
        let blog = SiteSpec { name: "blog".into(), kind: SiteKind::Static { spa: false, index: false }, listeners: vec![ls(&["blog.example.com", "www.example.com"], 80, None, true), ls(&["blog.example.com", "www.example.com"], 443, Some("example.com"), false)], ipv6: false, hsts: false, adv: Default::default() };
        let old = SiteSpec { name: "old".into(), kind: SiteKind::Redirect { to: "https://app.example.com".into(), code: 301 }, listeners: vec![ls(&["old.example.com"], 80, None, false)], ipv6: false, hsts: false, adv: Default::default() };
        let t = now_secs();
        let notafter = |days: i64| {
            let s = t + days * 86400;
            chrono::DateTime::from_timestamp(s, 0).map(|d| d.format("%b %e %H:%M:%S %Y GMT").to_string()).unwrap_or_default()
        };
        let out = format!(
            "@@DOCKER\nok\n@@LAYOUT\n{{\"layout\":1}}\n@@PORTS\n@@PANEL\n@@CONTAINER\nrunning openresty/openresty:alpine\n@@VERSION\nnginx version: openresty/1.27.1.1\n@@SITES\n@@F app.example.com.conf\n{}@@F blog.conf\n{}@@F old.conf.off\n{}@@F legacy.conf\nserver {{ listen 8081; location / {{ return 200 'ok'; }} }}\n@@CERTS\n@@C app.example.com\n{{\"source\":\"acme\",\"main\":\"app.example.com\",\"acct\":\"letsencrypt-me\",\"auto\":true}}\n@@X\nnotAfter={}\nissuer=C = US, O = Let's Encrypt, CN = R11\nsubject=CN = app.example.com\nX509v3 Subject Alternative Name: \n    DNS:app.example.com\n@@C example.com\n{{\"source\":\"acme\",\"main\":\"example.com\",\"acct\":\"letsencrypt-me\",\"method\":\"dns\",\"dns\":\"cloudflare-main\",\"auto\":true}}\n@@X\nnotAfter={}\nissuer=C = US, O = Let's Encrypt, CN = R11\nsubject=CN = example.com\nX509v3 Subject Alternative Name: \n    DNS:example.com, DNS:*.example.com\n@@C internal\n{{\"source\":\"self\"}}\n@@X\nnotAfter={}\nissuer=CN = internal.lan\nsubject=CN = internal.lan\n@@ACME\n[{{\"id\":\"letsencrypt-me\",\"ca\":\"letsencrypt\",\"email\":\"me@example.com\"}}]\n@@DNS\n[{{\"id\":\"cloudflare-main\",\"name\":\"主域名\",\"provider\":\"cloudflare\"}}]\n@@CRON\n17 3 * * * root /opt/otter/websites/bin/renew.sh\n@@RENEW\n2026-11-24 03:17:02 example.com 续期成功\n2026-11-24 03:17:09 已重载 OpenResty\n@@HEALTH\n@@END\n",
            web::render_site(&proxy, &web::Target::own()),
            web::render_site(&blog, &web::Target::own()),
            web::render_site(&old, &web::Target::own()),
            notafter(61),
            notafter(12),
            notafter(300)
        );
        w.st = Some(Ok(web::parse_state(&out)));
        // 调试:OTTER_WEB=install|certs|settings|site|issue|job
        let mode = std::env::var("OTTER_WEB").unwrap_or_default();
        match mode.as_str() {
            "install" => {
                let st = web::WebState { docker: "ok".into(), ..Default::default() };
                w.st = Some(Ok(st));
            }
            "ext" => {
                let ex = |p: &str, d: i64| web::ExtCert { path: p.into(), domains: vec![], issuer: "Let's Encrypt".into(), not_after: Some(now_secs() + d * 86400) };
                let site = |n: &str, k: &str, t: &str, cert: Option<&str>| web::ExtSite { names: vec![n.into()], listen: if cert.is_some() { vec!["443 ssl".into()] } else { vec!["80".into()] }, cert: cert.map(Into::into), target: t.into(), kind: k.into() };
                let x = web::ExtNginx { name: "fp-openresty-8703d5-openresty-1".into(), image: "openresty/openresty:1.27.1.1-alpine".into(), adopt: Ok(web::Target { container: "fp-openresty-8703d5-openresty-1".into(), bin: "openresty".into(), conf: "/opt/fleet-agent/openresty/conf.d".into(), certs: Some(("/opt/fleet-agent/acme/certs".into(), "/etc/openresty/certs".into())), acme: Some(("/opt/fleet-agent/acme/webroot".into(), "/var/www/acme".into())), sites: Some(("/opt/fleet-agent/openresty/www".into(), "/var/www/sites".into())), adopted: true, ports: (80, 443) }), sites: vec![site("blog.example.com", "静态站点", "/var/www/blog", Some("/c/blog.pem")), site("api.example.com", "反向代理", "http://127.0.0.1:8080", Some("/c/api.pem")), site("_", "其他", "", None)], certs: vec![ex("/c/blog.pem", 70), ex("/c/api.pem", 9)] };
                w.st = Some(Ok(web::WebState { docker: "ok".into(), ext: vec![x], ..Default::default() }));
            }
            "panel" => w.st = Some(Ok(web::WebState { docker: "ok".into(), panel: Some("1panel".into()), ..Default::default() })),
            "certs" => w.tab = 1,
            "settings" => w.tab = 2,
            "job" => {
                w.job = Some(JobView { title: "安装 OpenResty".into(), offset: 0, log: "==> 初始化数据目录 /opt/otter\n==> 写入配置\n==> 拉取镜像 openresty/openresty:alpine\nalpine: Pulling from openresty/openresty\n5d3e392a13a0: Pull complete\nc0a1e1f4b3d2: Downloading [=====>      ] 4.2MB/12MB\n".into(), last: Instant::now(), started: Instant::now(), done: None, cert: None })
            }
            _ => {}
        }
    }

    pub(super) fn web_demo_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match std::env::var("OTTER_WEB").unwrap_or_default().as_str() {
            "site" if self.web_ui().dlg.is_none() => self.web_open_site(None, window, cx),
            "siteadv" if self.web_ui().dlg.is_none() => {
                self.web_open_site(None, window, cx);
                self.web_mut().s_adv = true;
            }
            "install" => {
                if !self.dock.wbi.is_open() {
                    self.wbi_open(window, cx);
                }
            }
            "issue" if self.web_ui().dlg.is_none() => self.web_open_issue(window, cx),
            "dns" if self.web_ui().dlg.is_none() => self.web_open_dns(window, cx),
            "upload" if self.web_ui().dlg.is_none() => self.web_open_upload(window, cx),
            _ => {}
        }
    }

    // ---------------------------------------------------------------- 页头

    pub(crate) fn web_header_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v: Vec<AnyElement> = Vec::new();
        if !self.web_installed() {
            return v;
        }
        match self.web_ui().tab {
            0 => v.push(button("web-site-add", IconName::Plus, "新建站点").on_click(cx.listener(|this, _, window, cx| this.web_open_site(None, window, cx))).into_any_element()),
            1 => {
                v.push(button("web-cert-self", IconName::FileKey, "自签").on_click(cx.listener(|this, _, window, cx| this.web_open_self(window, cx))).into_any_element());
                v.push(button("web-cert-up", IconName::Upload, "上传").on_click(cx.listener(|this, _, window, cx| this.web_open_upload(window, cx))).into_any_element());
                v.push(button("web-cert-issue", IconName::ShieldCheck, "申请证书").on_click(cx.listener(|this, _, window, cx| this.web_open_issue(window, cx))).into_any_element());
            }
            _ => {}
        }
        v
    }

    /// 站点 / 证书 / 设置页签:版式和数据库页的页签一致,挂在页头下面
    pub(crate) fn web_tabs(&self, cx: &mut Context<Self>) -> Div {
        let mut row = div().flex().flex_row().items_center().gap(px(4.0)).pb(px(10.0)).flex_shrink_0();
        for (i, (name, ic)) in [("站点", IconName::Globe), ("证书", IconName::ShieldCheck), ("设置", IconName::Settings)].into_iter().enumerate() {
            let active = self.web_ui().tab == i as u8;
            row = row.child(
                div()
                    .id(("web-tab", i))
                    .h(px(30.0))
                    .px(px(12.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .text_size(px(t::TYPE_CONTROL))
                    .when(active, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::SEMIBOLD))
                    .when(!active, |d| d.rounded(px(t::RADIUS_MD)).border_1().border_color(gpui_kit::transparent_black()).text_color(c::muted()).hover(|s| s.bg(c::field_hover()).text_color(c::secondary())))
                    .child(icon(ic, 14.0, if active { c::accent() } else { c::ghost() }))
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.web_mut().tab = i as u8;
                        cx.notify();
                    })),
            );
        }
        row
    }

    pub(crate) fn web_subtitle(&self) -> String {
        format!("{} 上的 OpenResty 站点、证书和自动续期", self.node().name)
    }

    // ---------------------------------------------------------------- 页面

    pub(crate) fn web_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let st = match &self.web_ui().st {
            None => return crate::ui::loading("正在读取网站信息…").into_any_element(),
            Some(Err(e)) => {
                return div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(empty_state(IconName::TriangleAlert, "读取不了网站信息", "要 root 权限(免密 sudo,或在节点里保存密码)。处理后点右上角「刷新」重试"))
                    .child(div().pb(px(60.0)).px(px(24.0)).flex().justify_center().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child(SharedString::from(e.clone())))
                    .into_any_element();
            }
            Some(Ok(s)) => s.clone(),
        };
        let mut col = div().flex_grow(1.0).flex_shrink_0().pb(px(14.0)).flex().flex_col().gap(px(14.0));
        if let Some(j) = self.web_job_panel(cx) {
            col = col.child(j);
        }
        if !st.installed() {
            if !st.ext.is_empty() {
                return col.child(self.web_external(&st, cx)).into_any_element();
            }
            return col.child(self.web_install_card(&st, cx)).into_any_element();
        }
        if st.target.adopted {
            col = col.child(self.web_banner(IconName::Link, c::accent(), &format!("正在管理已有的 {}:站点配置直接改它的 {}。原来创建它的面板如果会重写这个目录,可能覆盖这里的改动。", st.target.container, st.target.conf)));
            let mut missing = Vec::new();
            if st.target.certs.is_none() {
                missing.push("证书目录");
            }
            if st.target.acme.is_none() {
                missing.push("HTTP 验证目录");
            }
            if st.target.sites.is_none() {
                missing.push("静态站点目录");
            }
            if st.panel.is_some() {
                col = col.child(self.web_banner(IconName::Info, c::warn(), "这台服务器由面板管理:这里只改站点,证书和申请续期不在这里做,请用面板的证书功能。"));
            } else if !missing.is_empty() {
                col = col.child(self.web_banner(IconName::Info, c::warn(), &format!("这个容器没有挂载{}:对应功能不可用(要用得先重建容器加挂载)。", missing.join("、"))));
            }
        }
        if st.too_new() {
            col = col.child(self.web_banner(IconName::Lock, c::warn(), "服务器上的数据目录比这个版本的 Otter 新,为避免破坏数据,网站管理暂时只读。请升级 Otter。"));
        }
        if !st.running() {
            col = col.child(self.web_banner(IconName::TriangleAlert, c::danger(), "OpenResty 没有在运行,站点暂时访问不了。到「设置」里启动它。"));
        }
        for h in &st.health {
            col = col.child(self.web_banner(IconName::ShieldAlert, c::warn(), &format!("{h};到「设置」里一键修复权限")));
        }
        match self.web_ui().tab {
            0 => col.child(self.web_sites(&st, cx)),
            1 => col.child(self.web_certs(&st, cx)),
            _ => col.child(self.web_settings(&st, cx)),
        }
        .into_any_element()
    }

    fn web_banner(&self, ic: IconName, color: Rgba, text: &str) -> Div {
        div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(14.0))
            .py(px(9.0))
            .rounded(px(t::RADIUS_LG))
            .border_1()
            .border_color(color.opacity(0.3))
            .bg(color.opacity(0.08))
            .child(icon(ic, 14.0, color))
            .child(div().flex_1().min_w_0().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(SharedString::from(text.to_string())))
    }

    // ---- 已有的 nginx/OpenResty(只读) ----

    fn web_external(&self, st: &WebState, cx: &mut Context<Self>) -> AnyElement {
        let now = now_secs();
        let mut out = div().flex_shrink_0().flex().flex_col().gap(px(14.0));
        for (xi, x) in st.ext.iter().enumerate() {
            out = out.child(self.web_banner(IconName::Lock, c::accent(), &format!("检测到已有的 {}({}),占用着 80/443,Otter 不会再装第二个 OpenResty。自动接入没成功时可以点「接管它」重试;不能接入的只能看下面读到的站点和证书。", x.name, x.image)));
            out = out.child(match &x.adopt {
                Ok(t) => {
                    let t = t.clone();
                    let busy = self.web_ui().busy.is_some();
                    let mut notes: Vec<String> = vec![format!("站点配置写进 {}", t.conf)];
                    notes.push(match &t.certs { Some((h, _)) => format!("证书目录 {h}"), None => "没有证书目录挂载:不能管证书、配不了 HTTPS".into() });
                    notes.push(match &t.acme { Some((h, _)) => format!("HTTP 验证目录 {h}"), None => "没有验证目录挂载:申请证书只能用 DNS 验证".into() });
                    notes.push(match &t.sites { Some((h, _)) => format!("静态站点根目录 {h}"), None => "没有静态站点目录挂载:建不了静态站点".into() });
                    let name = t.container.clone();
                    let points: Vec<(IconName, Rgba, String)> = notes.into_iter().map(|n| (IconName::Folder, c::muted(), n)).chain([(IconName::TriangleAlert, c::warn(), "原来创建它的面板如果会重写 conf.d,可能覆盖 Otter 的改动".to_string()), (IconName::Check, c::online(), "容器不会被重建或重启,随时可以取消接管".to_string())]).collect();
                    div().flex_shrink_0().flex().flex_row().child(button("web-adopt", IconName::Link, "接管它").when(busy, |d| d.opacity(0.5)).on_click(cx.listener(move |this, _, _, cx| {
                        this.web_ask(WebConfirm { title: "接管 OpenResty", name: name.clone(), detail: "已有的 nginx/OpenResty 容器".into(), points: points.clone(), label: "接管", danger: false, act: Pending::Adopt(t.clone()) }, cx);
                    }))).into_any_element()
                }
                Err(why) => small(&format!("不能接管:{why}"), c::warn()).into_any_element(),
            });
            let header = div().child(head("域名").w(px(COL_SITE_DOMAIN))).child(head("类型").w(px(COL_SITE_KIND))).child(head("监听 / 证书").flex_1().min_w_0());
            let mut rows = Vec::new();
            for (i, sv) in x.sites.iter().enumerate() {
                let names = if sv.names.is_empty() { "(未写 server_name)".to_string() } else { sv.names.join(" ") };
                let cert = sv.cert.as_ref().and_then(|p| x.certs.iter().find(|c| &c.path == p));
                let (cert_text, cert_color) = match (&sv.cert, cert) {
                    (None, _) => (sv.listen.join(" · "), c::muted()),
                    (Some(_), Some(ce)) => match ce.not_after.map(|t| (t - now).div_euclid(86400)) {
                        Some(d) => (format!("{} · {}", sv.listen.join(" · "), fmt_days(d)), days_color(d)),
                        None => (sv.listen.join(" · "), c::muted()),
                    },
                    (Some(_), None) => (format!("{} · 证书读取不了", sv.listen.join(" · ")), c::warn()),
                };
                rows.push(
                    row(("wx-row", xi * 1000 + i))
                        .child(div().w(px(COL_SITE_DOMAIN)).flex_shrink_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(SharedString::from(names)))
                        .child(div().w(px(COL_SITE_KIND)).flex_shrink_0().flex().flex_col().child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(SharedString::from(sv.kind.clone()))).child(div().truncate().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::ghost()).child(SharedString::from(sv.target.clone()))))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(cert_color).child(SharedString::from(cert_text))),
                );
            }
            out = out.child(if rows.is_empty() { empty_state(IconName::Globe, "没有读到站点", "它的配置里没有 server 块").into_any_element() } else { table_flow(header, rows).into_any_element() });
        }
        out.into_any_element()
    }

    fn web_install_card(&self, st: &WebState, cx: &mut Context<Self>) -> AnyElement {
        let blocker = st.install_blocker();
        let busy = self.web_ui().busy.is_some() || self.web_ui().job.is_some();
        let (desc, warn) = match (&blocker, st.panel.is_some()) {
            (Some(b), _) => (b.clone(), true),
            (None, true) => ("这台服务器上的网站由面板管理,Otter 只读不改,避免和面板互相覆盖配置".to_string(), false),
            (None, false) => ("用 Docker 运行 OpenResty,站点直接监听 80/443;配置和证书放在 /opt/otter/websites/,证书续期交给服务器自己的 cron".to_string(), false),
        };
        let mut e = empty(IconName::Globe, "这台服务器还没有网站服务").desc(desc);
        if warn {
            e = e.warn();
        }
        if blocker.is_none() {
            e = e.action(
                button("web-install", IconName::Plus, "安装 OpenResty").when(busy, |d| d.opacity(0.5)).on_click(cx.listener(move |this, _, window, cx| {
                    if !busy {
                        this.wbi_open(window, cx);
                    }
                })),
            );
        }
        div().flex_1().flex().flex_col().child(e).into_any_element()
    }

    // ---- 后台任务面板 ----

    fn web_job_panel(&self, cx: &mut Context<Self>) -> Option<Div> {
        let j = self.web_ui().job.as_ref()?;
        let running = j.done.is_none();
        let (ic, color) = match j.done {
            None => (IconName::Clock, c::accent()),
            Some(0) => (IconName::Check, c::online()),
            Some(_) => (IconName::TriangleAlert, c::danger()),
        };
        let step = j.log.lines().rev().find(|l| l.starts_with("==>")).map(|l| l.trim_start_matches("==>").trim().to_string());
        let status = match j.done {
            None => step.clone().unwrap_or_else(|| "正在启动…".into()),
            Some(0) => "已完成".into(),
            Some(-1) => "任务被中断(服务器重启或被结束)".into(),
            Some(c) => format!("失败(退出码 {c}){}", web::explain(&j.log).map(|e| format!(":{e}")).unwrap_or_default()),
        };
        let lines: Vec<&str> = j.log.lines().collect();
        let tail = lines[lines.len().saturating_sub(JOB_LINES)..].join("\n");
        let mut right = div().flex().flex_row().gap(px(6.0));
        if running {
            right = right.child(crate::node_form::ctl_btn("web-job-cancel", None, "取消", 28.0).on_click(cx.listener(|this, _, _, cx| {
                this.web_run("取消任务", web::job_cancel_script(), OpTag::Plain, cx);
            })));
        } else {
            right = right.child(crate::node_form::ctl_btn("web-job-close", None, "关闭", 28.0).on_click(cx.listener(|this, _, _, cx| this.web_job_close(cx))));
        }
        let hint = match (j.done, &j.cert) {
            (Some(0), Some(n)) => format!("证书「{n}」已签发,到「站点」里编辑站点并选择它即可启用 HTTPS"),
            (None, _) => format!("已运行 {} 秒;任务在服务器上独立运行,关掉 Otter 也不会中断", j.started.elapsed().as_secs()),
            _ => String::new(),
        };
        Some(
            div()
                .flex_shrink_0()
                .rounded(px(t::RADIUS_XL))
                .border_1()
                .border_color(color.opacity(0.35))
                .p(px(14.0))
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .child(if running { crate::ui::dot_spinner(("web-job-spin", 0), color).into_any_element() } else { icon(ic, 15.0, color).into_any_element() })
                        .child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(j.title.clone())))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(if j.done.is_some_and(|c| c != 0) { c::danger() } else { c::muted() }).child(SharedString::from(status)))
                        .child(right),
                )
                .when(!tail.is_empty(), |d| {
                    d.child(div().rounded(px(t::RADIUS_MD)).bg(c::field()).border_1().border_color(c::field_border()).p(px(10.0)).font_family(MONO).text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(SharedString::from(tail)))
                })
                .when(!hint.is_empty(), |d| d.child(small(hint, c::ghost()))),
        )
    }

    // ---- 站点 ----

    fn web_sites(&self, st: &WebState, cx: &mut Context<Self>) -> AnyElement {
        if st.sites.is_empty() {
            return empty_state(IconName::Globe, "还没有站点", "点右上角「新建站点」,添加反向代理、静态站点或跳转").into_any_element();
        }
        let now = now_secs();
        let narrow = self.dock.view_w < 980.0;
        let mut header = div().child(head("域名").w(px(COL_SITE_DOMAIN))).child(head("类型").w(px(COL_SITE_KIND)));
        if !narrow {
            header = header.child(head("HTTPS").flex_1().min_w_0());
        }
        header = header.child(div().w(px(COL_SITE_ACT)));
        let busy = self.web_ui().busy.is_some();
        let mut rows = Vec::new();
        for (i, s) in st.sites.iter().enumerate() {
            let (kind_text, target) = match &s.spec {
                Some(sp) => match &sp.kind {
                    SiteKind::Proxy { target, .. } => (sp.kind.label().to_string(), target.clone()),
                    SiteKind::Static { .. } => (sp.kind.label().to_string(), format!("/opt/otter/websites/sites/{}", sp.name)),
                    SiteKind::Redirect { to, code } => (format!("{}({code})", sp.kind.label()), to.clone()),
                },
                None => ("外部配置".to_string(), s.file.clone()),
            };
            let (https, https_color) = match s.spec.as_ref().filter(|sp| !sp.certs().is_empty()) {
                None => ("仅 HTTP".to_string(), c::ghost()),
                Some(sp) => {
                    let mut missing: Option<&str> = None;
                    let mut uncovered = false;
                    let mut min_days: Option<i64> = None;
                    for l in sp.listeners.iter().filter(|l| l.ssl()) {
                        let name = l.cert.as_deref().unwrap_or_default();
                        match st.certs.iter().find(|c| c.name == name) {
                            None => missing = missing.or(Some(name)),
                            Some(ce) => {
                                uncovered |= l.domains.iter().any(|d| !ce.covers(d));
                                if let Some(d) = ce.days_left(now) {
                                    min_days = Some(min_days.map_or(d, |m| m.min(d)));
                                }
                            }
                        }
                    }
                    let certs = sp.certs();
                    let label = if certs.len() == 1 { certs[0].to_string() } else { format!("{} 张证书", certs.len()) };
                    match (missing, min_days) {
                        (Some(m), _) => (format!("证书「{m}」不存在"), c::danger()),
                        (None, Some(d)) if uncovered => (format!("证书不覆盖全部域名 · {}", fmt_days(d)), c::warn()),
                        (None, Some(d)) => (format!("{label} · {}", fmt_days(d)), days_color(d)),
                        (None, None) => (label, c::muted()),
                    }
                }
            };
            let all_domains = s.spec.as_ref().map(SiteSpec::domains).unwrap_or_default();
            let domains = match &s.spec {
                Some(_) if all_domains.len() > 1 => format!("{} 等 {} 个", all_domains[0], all_domains.len()),
                _ => s.title(),
            };
            // 不是只有默认 HTTP/HTTPS 端口时,在域名下面列出监听端口
            let tp = st.target.ports;
            let ports: Option<String> = s.spec.as_ref().filter(|sp| sp.listeners.iter().any(|l| l.port != tp.0 && l.port != tp.1)).map(|sp| {
                let mut v: Vec<u16> = sp.listeners.iter().map(|l| l.port).collect();
                v.sort_unstable();
                v.dedup();
                v.iter().map(u16::to_string).collect::<Vec<_>>().join(" · ")
            });
            let mut act = div().w(px(COL_SITE_ACT)).flex_shrink_0().flex().flex_row().justify_end().gap(px(2.0));
            let file = s.file.clone();
            let enabled = s.enabled;
            act = act.child(tip(icon_btn(("ws-pw", i), IconName::Power, if enabled { c::online() } else { c::ghost() }), if enabled { "停用" } else { "启用" }).on_click(cx.listener(move |this, _, _, cx| {
                if let Ok(sc) = web::site_toggle_script(&file, !enabled) {
                    this.web_run(if enabled { "停用站点" } else { "启用站点" }, sc, OpTag::Plain, cx);
                }
            })));
            let entry = s.clone();
            let editable = s.spec.is_some() && !s.modified;
            act = act.child(tip(icon_btn(("ws-ed", i), if editable { IconName::Pencil } else { IconName::FileText }, c::secondary()), if editable { "编辑" } else { "编辑配置原文" }).on_click(cx.listener(move |this, _, window, cx| {
                if editable { this.web_open_site(Some(entry.clone()), window, cx) } else { this.web_open_raw(&entry, window, cx) }
            })));
            if let Some(sp) = &s.spec {
                let name = sp.name.clone();
                act = act.child(tip(icon_btn(("ws-lg", i), IconName::ScrollText, c::secondary()), "访问日志").on_click(cx.listener(move |this, _, _, cx| this.web_open_logs(Some(name.clone()), false, cx))));
            }
            let (f2, t2) = (s.file.clone(), s.title());
            act = act.child(tip(icon_btn(("ws-dl", i), IconName::Trash, c::danger()), "删除").on_click(cx.listener(move |this, _, _, cx| {
                this.web_ask(
                    WebConfirm {
                        title: "删除站点",
                        name: t2.clone(),
                        detail: "OpenResty 站点".into(),
                        points: vec![(IconName::Trash, c::danger(), "删除站点配置,域名将不再由这台服务器响应".into()), (IconName::Check, c::online(), "证书和静态站点目录里的文件不会删除".into()), (IconName::FileText, c::muted(), "配置会先备份到 websites/conf/backup/".into())],
                        label: "删除站点",
                        danger: true,
                        act: Pending::DeleteSite(f2.clone()),
                    },
                    cx,
                );
            })));
            let mut r = row(("ws-row", i))
                .child(
                    div()
                        .w(px(COL_SITE_DOMAIN))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .child(div().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).text_color(if enabled { c::text() } else { c::ghost() }).child(SharedString::from(domains)))
                        .when_some(ports, |d, p| d.child(div().truncate().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::ghost()).child(SharedString::from(format!("端口 {p}")))))
                        .when(!enabled, |d| d.child(small("已停用", c::warn())))
                        .when(s.modified, |d| d.child(small("配置被手工改过", c::ghost()))),
                )
                .child(div().w(px(COL_SITE_KIND)).flex_shrink_0().flex().flex_col().child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(SharedString::from(kind_text))).child(div().truncate().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::ghost()).child(SharedString::from(target))));
            if !narrow {
                r = r.child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(https_color).child(SharedString::from(https)));
            }
            rows.push(r.child(act));
        }
        let _ = busy;
        div().flex_shrink_0().child(table_flow(header, rows)).into_any_element()
    }

    // ---- 证书 ----

    fn web_certs(&self, st: &WebState, cx: &mut Context<Self>) -> AnyElement {
        if st.certs.is_empty() {
            return empty_state(IconName::ShieldCheck, "还没有证书", "点右上角「申请证书」向 Let's Encrypt 免费申请,或上传/自签").into_any_element();
        }
        let now = now_secs();
        let header = div().child(head("证书").w(px(COL_CERT_NAME))).child(head("域名").flex_1().min_w_0()).child(head("来源").w(px(COL_CERT_SRC))).child(head("到期").w(px(COL_CERT_EXP))).child(head("使用").w(px(COL_CERT_USE))).child(div().w(px(COL_CERT_ACT)));
        let mut rows = Vec::new();
        for (i, ce) in st.certs.iter().enumerate() {
            let src = match ce.meta.source.as_str() {
                "acme" => format!("{}{}", ce.issuer, if ce.meta.auto { " · 自动续期" } else { " · 手动" }),
                "self" => "自签".to_string(),
                "upload" => format!("上传 · {}", ce.issuer),
                _ => ce.issuer.clone(),
            };
            let (exp, color) = match ce.days_left(now) {
                Some(d) => (fmt_days(d), days_color(d)),
                None => ("读取不了".into(), c::muted()),
            };
            let users = st.cert_users(&ce.name);
            let name = ce.name.clone();
            let mut act = div().w(px(COL_CERT_ACT)).flex_shrink_0().flex().flex_row().justify_end().gap(px(2.0));
            if ce.meta.source == "acme" && ce.meta.auto {
                act = act.child(tip(icon_btn(("wc-rn", i), IconName::RefreshCw, c::secondary()), "立即检查续期").on_click(cx.listener(|this, _, _, cx| this.web_run("检查续期", web::renew_now_script(), OpTag::Plain, cx))));
            }
            let (n2, u2) = (ce.name.clone(), users.clone());
            act = act.child(tip(icon_btn(("wc-dl", i), IconName::Trash, c::danger()), "删除").on_click(cx.listener(move |this, _, _, cx| {
                if !u2.is_empty() {
                    return this.web_notice(format!("还有站点在用这个证书:{}", u2.join("、")), cx);
                }
                this.web_ask(
                    WebConfirm {
                        title: "删除证书",
                        name: n2.clone(),
                        detail: "证书".into(),
                        points: vec![(IconName::Trash, c::danger(), "删除证书和私钥文件,无法恢复".into()), (IconName::Info, c::muted(), "ACME 证书之后需要重新申请(受 Let's Encrypt 频率限制)".into())],
                        label: "删除证书",
                        danger: true,
                        act: Pending::DeleteCert(n2.clone()),
                    },
                    cx,
                );
            })));
            let _ = name;
            rows.push(
                row(("wc-row", i))
                    .child(div().w(px(COL_CERT_NAME)).flex_shrink_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(SharedString::from(ce.name.clone())))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(SharedString::from(ce.domains.join("  "))))
                    .child(div().w(px(COL_CERT_SRC)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(src)))
                    .child(div().w(px(COL_CERT_EXP)).flex_shrink_0().text_size(px(t::TYPE_CONTROL)).text_color(color).child(SharedString::from(exp)))
                    .child(div().w(px(COL_CERT_USE)).flex_shrink_0().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(if users.is_empty() { "未使用".to_string() } else { format!("{} 个站点", users.len()) })))
                    .child(act),
            );
        }
        div().flex_shrink_0().child(table_flow(header, rows)).into_any_element()
    }

    // ---- 设置 ----

    fn web_settings(&self, st: &WebState, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.web_ui().busy.is_some();
        let btn = |id: &'static str, ic: IconName, label: &'static str| {
            let b = button(id, ic, label);
            if busy { b.opacity(0.5) } else { b }
        };
        let running = st.running();
        let (state_text, state_color) = match st.container.as_ref().map(|c| c.0.as_str()) {
            Some("running") => ("运行中", c::online()),
            Some(s) if s == "exited" || s == "created" => ("已停止", c::warn()),
            Some(_) => ("异常", c::danger()),
            None => ("未安装", c::ghost()),
        };
        let mut ctl = div().flex().flex_row().flex_wrap().gap(px(8.0));
        ctl = ctl.child(btn("web-reload", IconName::RefreshCw, "重载配置").on_click(cx.listener(|this, _, _, cx| this.web_ctl("重载配置", "reload", cx))));
        ctl = ctl.child(btn("web-test", IconName::Check, "测试配置").on_click(cx.listener(|this, _, _, cx| this.web_ctl("测试配置", "test", cx))));
        ctl = ctl.child(btn("web-restart", IconName::RefreshCw, "重启").on_click(cx.listener(|this, _, _, cx| this.web_ctl("重启 OpenResty", "restart", cx))));
        if running {
            ctl = ctl.child(btn("web-stop", IconName::Square, "停止").on_click(cx.listener(|this, _, _, cx| {
                this.web_ask(
                    WebConfirm {
                        title: "停止 OpenResty",
                        name: this.web_target().container,
                        detail: "容器".into(),
                        points: vec![(IconName::TriangleAlert, c::danger(), "所有站点会立刻无法访问".into()), (IconName::Check, c::online(), "配置、站点和证书都保留,可以随时启动".into())],
                        label: "停止",
                        danger: true,
                        act: Pending::Act(ConfAct::Stop),
                    },
                    cx,
                );
            })));
        } else {
            ctl = ctl.child(btn("web-start", IconName::Play, "启动").on_click(cx.listener(|this, _, _, cx| this.web_ctl("启动 OpenResty", "start", cx))));
        }
        ctl = ctl.child(btn("web-log-err", IconName::ScrollText, "错误日志").on_click(cx.listener(|this, _, _, cx| this.web_open_logs(None, true, cx))));
        if st.target.adopted {
            ctl = ctl.child(btn("web-unadopt", IconName::X, "取消接管").on_click(cx.listener(|this, _, _, cx| {
                this.web_ask(
                    WebConfirm {
                        title: "取消接管",
                        name: this.web_target().container,
                        detail: "OpenResty/nginx 容器".into(),
                        points: vec![(IconName::Check, c::online(), "容器、站点配置和证书都保持原样,继续运行".into()), (IconName::TriangleAlert, c::warn(), "Otter 之后不再管理它;自动续期任务会改用 Otter 自己的目录,建议先关闭".into())],
                        label: "取消接管",
                        danger: false,
                        act: Pending::Unadopt,
                    },
                    cx,
                );
            })));
        } else {
        ctl = ctl.child(btn("web-uninstall", IconName::Trash, "卸载").on_click(cx.listener(|this, _, _, cx| {
            this.web_ask(
                WebConfirm {
                    title: "卸载 OpenResty",
                    name: "otter-openresty".into(),
                    detail: "容器".into(),
                    points: vec![(IconName::Trash, c::danger(), "删除容器,站点停止服务".into()), (IconName::Check, c::online(), "/opt/otter/websites/ 里的配置、站点文件和证书不会删除,重新安装后恢复".into())],
                    label: "卸载",
                    danger: true,
                    act: Pending::Act(ConfAct::Uninstall),
                },
                cx,
            );
        })));
        }
        let engine = card(
            "OpenResty",
            format!("{} · {}", st.version, st.container.as_ref().map(|c| c.1.clone()).unwrap_or_default()),
            Some(pill(state_text, state_color, state_color.opacity(0.14)).into_any_element()),
            ctl,
        );

        // 自动续期
        let cron_text = match &st.cron {
            Some(c) => format!("已开启:cron 计划 {c}(每天一次),不到 {} 天到期的证书自动续期", web::RENEW_DAYS),
            None => "还没有开启。开启后,服务器自己的 cron 每天检查证书并续期,Otter 不需要开着".to_string(),
        };
        let mut rn = div().flex().flex_col().gap(px(10.0)).child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(cron_text)));
        let mut rb = div().flex().flex_row().flex_wrap().gap(px(8.0));
        rb = rb.child(btn("web-cron-on", IconName::CalendarClock, if st.cron.is_some() { "更新续期任务" } else { "开启自动续期" }).on_click(cx.listener(|this, _, _, cx| {
            // 分钟数按节点名取个固定值,避免所有服务器同一分钟去请求 CA
            let h: u32 = this.node().name.bytes().fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
            this.web_run("开启自动续期", web::cron_script(h % 60, 2 + (h / 60) % 3), OpTag::Plain, cx);
        })));
        if st.cron.is_some() {
            rb = rb.child(btn("web-renew-now", IconName::RefreshCw, "立即检查").on_click(cx.listener(|this, _, _, cx| this.web_run("检查续期", web::renew_now_script(), OpTag::Plain, cx))));
            rb = rb.child(btn("web-cron-off", IconName::X, "关闭").on_click(cx.listener(|this, _, _, cx| {
                this.web_ask(
                    WebConfirm {
                        title: "关闭自动续期",
                        name: "证书续期任务".into(),
                        detail: "/etc/cron.d/otter-websites".into(),
                        points: vec![(IconName::TriangleAlert, c::warn(), "之后证书到期前不会自动续期,过期后站点会提示不安全".into())],
                        label: "关闭",
                        danger: true,
                        act: Pending::Act(ConfAct::RemoveCron),
                    },
                    cx,
                );
            })));
        }
        rn = rn.child(rb);
        if !st.renew_log.is_empty() {
            let log = st.renew_log[st.renew_log.len().saturating_sub(6)..].join("\n");
            rn = rn.child(div().rounded(px(t::RADIUS_MD)).bg(c::field()).border_1().border_color(c::field_border()).p(px(10.0)).font_family(MONO).text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(SharedString::from(log)));
        }
        let renew = card("自动续期", "写在服务器的 /etc/cron.d,不依赖 Otter 常驻", None, rn);

        // DNS 账号
        let mut dns = div().flex().flex_col().gap(px(8.0));
        if st.dns.is_empty() {
            dns = dns.child(small("还没有 DNS 账号。申请泛域名证书(*.example.com)需要它", c::ghost()));
        }
        for (i, d) in st.dns.iter().enumerate() {
            let (id, nm) = (d.id.clone(), d.name.clone());
            let prov = web::dns_providers().into_iter().find(|p| p.0 == d.provider).map(|p| p.1).unwrap_or("未知");
            dns = dns.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .h(px(34.0))
                    .child(icon(IconName::Key, 14.0, c::ghost()))
                    .child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::MEDIUM).child(SharedString::from(d.name.clone())))
                    .child(pill(prov, c::secondary(), c::idle_soft()))
                    .child(div().flex_1().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child("密钥已保存在服务器上"))
                    .child(tip(icon_btn(("wd-del", i), IconName::Trash, c::danger()), "删除").on_click(cx.listener(move |this, _, _, cx| {
                        this.web_ask(
                            WebConfirm {
                                title: "删除 DNS 账号",
                                name: nm.clone(),
                                detail: "DNS 账号".into(),
                                points: vec![(IconName::Trash, c::danger(), "从服务器删除保存的 API 密钥".into()), (IconName::TriangleAlert, c::warn(), "用到它的泛域名证书之后无法自动续期".into())],
                                label: "删除账号",
                                danger: true,
                                act: Pending::DeleteDns(id.clone()),
                            },
                            cx,
                        );
                    }))),
            );
        }
        let dns_card = card(
            "DNS 账号",
            "用于 DNS 验证和泛域名证书",
            Some(btn("web-dns-add", IconName::Plus, "添加").on_click(cx.listener(|this, _, window, cx| this.web_open_dns(window, cx))).into_any_element()),
            dns,
        );

        // ACME 账号
        let mut ac = div().flex().flex_col().gap(px(6.0));
        if st.acme.is_empty() {
            ac = ac.child(small("第一次申请证书时自动创建", c::ghost()));
        }
        for a in &st.acme {
            ac = ac.child(div().flex().flex_row().gap(px(10.0)).items_center().h(px(28.0)).child(icon(IconName::ShieldCheck, 14.0, c::ghost())).child(div().text_size(px(t::TYPE_CONTROL)).child(SharedString::from(a.email.clone()))).child(pill(if a.ca == "zerossl" { "ZeroSSL" } else { "Let's Encrypt" }, c::secondary(), c::idle_soft())));
        }
        let acme = card("证书账号", "ACME 账号,私钥在服务器 secrets/ 里(权限 700)", None, ac);

        // 数据目录
        let mut dir = div().flex().flex_col().gap(px(8.0)).child(div().font_family(MONO).text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(format!("{}   布局版本 {}", crate::srvdata::WEB, st.layout.unwrap_or(0)))));
        if !st.health.is_empty() {
            for h in &st.health {
                dir = dir.child(small(h.clone(), c::warn()));
            }
            dir = dir.child(
                div().flex().flex_row().child(btn("web-fix", IconName::ShieldCheck, "修复权限").on_click(cx.listener(|this, _, _, cx| {
                    this.web_ask(
                        WebConfirm {
                            title: "修复权限",
                            name: "websites/secrets、私钥".into(),
                            detail: "权限".into(),
                            points: vec![(IconName::Lock, c::online(), "密钥目录改为 700,私钥和密钥文件改为 600".into())],
                            label: "修复",
                            danger: false,
                            act: Pending::Act(ConfAct::FixPerms),
                        },
                        cx,
                    );
                }))),
            );
        } else {
            dir = dir.child(small("权限检查通过", c::online()));
        }
        let data = card("数据目录", "配置、证书、账号都在这里;整目录可以打包迁到另一台服务器", None, dir);
        div().flex().flex_col().gap(px(14.0)).child(engine).child(renew).child(dns_card).child(acme).child(data).into_any_element()
    }

    // ---------------------------------------------------------------- 弹窗

    /// 弹窗底部的一行:左边提示/错误,右边取消和主按钮(无外框,数据库弹窗版式用;分栏底栏见 `web_footer`)
    fn web_actions(&self, left: Option<(String, Rgba)>, ok_label: &'static str, can: bool, cx: &mut Context<Self>) -> Div {
        let busy = self.web_ui().busy;
        let cancel = crate::node_form::ctl_btn("wd-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| this.web_close(cx)));
        let ok = div()
            .id("wd-ok")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(34.0))
            .px(px(16.0))
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .when(can && busy.is_none(), |d| d.bg(c::accent()).text_color(c::on_accent()).cursor_pointer().hover(|s| s.opacity(0.88)))
            .when(!(can && busy.is_none()), |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()))
            .when(busy.is_some(), |d| d.child(crate::ui::dot_spinner(("wd-spin", 0), c::muted())))
            .child(if busy.is_some() { busy.unwrap_or(ok_label) } else { ok_label })
            .on_click(cx.listener(|this, _, _, cx| this.web_submit(cx)));
        let (text, color) = left.unwrap_or_default();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .child(div().flex_1().min_w_0().text_size(px(t::TYPE_CAPTION)).text_color(color).child(SharedString::from(text)))
            .child(cancel)
            .child(ok)
    }

    fn web_footer(&self, left: Option<(String, Rgba)>, ok_label: &'static str, can: bool, cx: &mut Context<Self>) -> Div {
        div().px(px(24.0)).py(px(14.0)).border_t_1().border_color(c::line()).child(self.web_actions(left, ok_label, can, cx))
    }

    fn web_title(&self, title: &'static str, sub: &'static str) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .px(px(24.0))
            .pt(px(24.0))
            .pb(px(14.0))
            .child(div().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).child(title))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(sub))
    }

    fn web_body(&self) -> Div {
        div().flex().flex_col().gap(px(14.0)).px(px(24.0)).pb(px(20.0))
    }

    fn web_modal(&self, id: &'static str, w: f32, parts: [AnyElement; 3], cx: &mut Context<Self>) -> Stateful<Div> {
        crate::ui::modal(
            id,
            w,
            cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                "escape" => {
                    this.web_close(cx);
                    cx.stop_propagation();
                }
                "enter" if !e.keystroke.modifiers.shift && !matches!(this.web_ui().dlg, Some(Dlg::Raw(_)) | Some(Dlg::CertUpload) | Some(Dlg::Logs)) => {
                    this.web_submit(cx);
                    cx.stop_propagation();
                }
                _ => {}
            }),
            parts,
        )
    }

    /// 当前打开的弹窗;确认框、任务面板不在这里
    pub(crate) fn web_overlays(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v: Vec<AnyElement> = Vec::new();
        if self.page != Page::Websites {
            return v;
        }
        if self.demo {
            self.web_demo_dialog(window, cx);
        }
        if let Some(d) = self.web_ui().dlg.clone() {
            let m = match d {
                Dlg::Site(edit) => self.web_dlg_site(edit.is_some(), cx),
                Dlg::Raw(file) => self.web_dlg_raw(&file, cx),
                Dlg::CertUpload => self.web_dlg_upload(cx),
                Dlg::CertSelf => self.web_dlg_self(cx),
                Dlg::Issue => self.web_dlg_issue(cx),
                Dlg::Dns => self.web_dlg_dns(cx),
                Dlg::Logs => self.web_dlg_logs(cx),
            };
            v.push(m.into_any_element());
        }
        if let Some(m) = self.web_confirm_dialog(cx) {
            v.push(m.into_any_element());
        }
        v
    }

    fn web_confirm_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let w = self.web_ui().confirm.as_ref()?;
        let spec = Confirm {
            ids: ["web-cf-cancel", "web-cf-ok"],
            title: w.title,
            tile: Self::icon_tile(if w.danger { IconName::Trash } else { IconName::ShieldCheck }, if w.danger { c::danger() } else { c::accent() }, c::idle_soft()).into_any_element(),
            name: w.name.clone().into(),
            detail: w.detail.clone().into(),
            points: w.points.iter().map(|(i, col, s)| (*i, *col, s.clone().into())).collect(),
            ok_label: w.label,
            kind: if w.danger { ConfirmKind::Danger } else { ConfirmKind::Action },
            extra: None,
        };
        Some(self.delete_confirm(
            "web-confirm",
            spec,
            |this| this.web_mut().confirm = None,
            |this, cx| this.web_confirmed(cx),
            cx,
        ))
    }

    /// 新建/编辑站点:版式跟数据库页的弹窗一致(窄、粗标题、小字段标题 + 34 高输入框、分段选择、右下角按钮)
    fn web_dlg_site(&self, editing: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let w = self.web_ui();
        let Some(ins) = (w.ins.len() >= 2).then(|| w.ins.clone()) else { return div().id("web-dlg-empty") };
        let kind = w.s_kind;
        let input = |i: &Entity<InputState>| div().flex_1().min_w_0().child(Input::new(i).appearance(false).with_size(px(12.0 / 0.875))).into_any_element();
        let ibox = |i: &Entity<InputState>| {
            div().h(px(34.0)).flex().flex_row().items_center().pl(px(4.0)).pr(px(6.0)).rounded(px(t::RADIUS_MD)).bg(c::field()).border_1().border_color(c::field_border()).text_size(px(t::TYPE_CONTROL)).child(input(i))
        };
        let caption = |label: &'static str| div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(label);
        // 分段选择:一条轨道,选中块用强调色淡底
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
        let track = || div().flex().flex_row().items_center().gap(px(2.0)).p(px(2.0)).rounded(px(t::RADIUS_MD)).bg(c::field_hover());
        // 开关卡片:描边小框,开关 + 名称 + 一句说明
        let card_toggle = |id: &'static str, label: &'static str, hint: &'static str, on: bool| {
            div()
                .id(id)
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .p(px(10.0))
                .rounded(px(t::RADIUS_MD))
                .border_1()
                .border_color(c::field_border())
                .bg(c::field())
                .cursor_pointer()
                .child(switch(id, on))
                .child(div().flex_1().min_w_0().flex().flex_col().child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(label)).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(hint)))
        };

        let mut types = track();
        for (i, l) in ["反向代理", "静态站点", "跳转"].into_iter().enumerate() {
            types = types.child(seg(("ws-kind", i), l, kind == i as u8).on_click(cx.listener(move |this, _, _, cx| {
                this.web_mut().s_kind = i as u8;
                cx.notify();
            })));
        }

        // ---- 监听列表:每组 = 域名 + 端口 + HTTP/HTTPS(+ 证书)
        let certs: Vec<CertEntry> = self.web_state().map(|s| s.certs.clone()).unwrap_or_default();
        let many = w.s_ls.len() > 1;
        let port_of = |f: &LForm| f.port.read(cx).value().trim().parse::<u16>().ok();
        let https_p = self.web_target().ports.1;
        let has_443_ssl = w.s_ls.iter().any(|f| f.ssl && port_of(f) == Some(https_p));
        let any_ssl = w.s_ls.iter().any(|f| f.ssl);
        let mut lcards = div().flex().flex_col().gap(px(8.0));
        let mut all_ok = !w.s_ls.is_empty();
        for (i, f) in w.s_ls.iter().enumerate() {
            let doms = web::split_domains(&f.domains.read(cx).value());
            all_ok &= !doms.is_empty() && port_of(f).is_some();
            let mut ssl_seg = track().w(px(112.0)).flex_none();
            for (k, l) in ["HTTP", "HTTPS"].into_iter().enumerate() {
                let on = f.ssl == (k == 1);
                ssl_seg = ssl_seg.child(seg(("ws-l-ssl", i * 2 + k), l, on).on_click(cx.listener(move |this, _, window, cx| {
                    let certs: Vec<CertEntry> = this.web_state().map(|s| s.certs.clone()).unwrap_or_default();
                    let Some(f) = this.web_ui().s_ls.get(i).cloned() else { return };
                    let doms = web::split_domains(&f.domains.read(cx).value());
                    let ssl = k == 1;
                    let port = f.port.read(cx).value().trim().to_string();
                    // 开 HTTPS:自动选第一张覆盖这些域名的证书;端口还是默认的 80 就顺手改成 443,关掉时反过来
                    let pick = if ssl { certs.iter().find(|c| !doms.is_empty() && doms.iter().all(|d| c.covers(d))).map(|c| c.name.clone()) } else { None };
                    let (hp, sp) = this.web_target().ports;
                    if ssl && port == hp.to_string() {
                        f.port.update(cx, |s, cx| s.set_value(&sp.to_string(), window, cx));
                    } else if !ssl && port == sp.to_string() {
                        f.port.update(cx, |s, cx| s.set_value(&hp.to_string(), window, cx));
                    }
                    if let Some(m) = this.web_mut().s_ls.get_mut(i) {
                        m.ssl = ssl;
                        m.cert = pick;
                        if ssl {
                            m.force = false;
                        }
                    }
                    cx.notify();
                })));
            }
            let mut line1 = div().flex().flex_row().items_center().gap(px(6.0)).child(div().flex_1().min_w_0().child(ibox(&f.domains))).child(div().w(px(72.0)).flex_none().child(ibox(&f.port)));
            line1 = line1.child(ssl_seg);
            if many {
                line1 = line1.child(tip(icon_btn(("ws-l-rm", i), IconName::X, c::muted()).on_click(cx.listener(move |this, _, _, cx| {
                    let w = this.web_mut();
                    if i < w.s_ls.len() {
                        w.s_ls.remove(i);
                    }
                    cx.notify();
                })), "删除这组监听"));
            }
            let mut line2 = div().flex().flex_row().flex_wrap().items_center().gap(px(6.0));
            let mut has2 = false;
            if f.ssl {
                has2 = true;
                let mut any = false;
                for (ci, ce) in certs.iter().enumerate() {
                    let fits = !doms.is_empty() && doms.iter().all(|d| ce.covers(d));
                    if !fits && f.cert.as_deref() != Some(&ce.name) {
                        continue;
                    }
                    any = true;
                    let n = ce.name.clone();
                    line2 = line2.child(opt_chip(("ws-l-cert", i * 100 + ci), ce.name.clone(), f.cert.as_deref() == Some(&ce.name)).on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(m) = this.web_mut().s_ls.get_mut(i) {
                            m.cert = Some(n.clone());
                        }
                        cx.notify();
                    })));
                }
                if !any {
                    line2 = line2.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::warn()).child("没有覆盖这些域名的证书,先到「证书」页申请或上传"));
                }
            } else if has_443_ssl {
                has2 = true;
                let on = f.force;
                line2 = line2.child(
                    opt_chip(("ws-l-force", i), "跳转到 HTTPS", on).on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(m) = this.web_mut().s_ls.get_mut(i) {
                            m.force = !m.force;
                        }
                        cx.notify();
                    })),
                );
            }
            lcards = lcards.child(div().flex().flex_col().gap(px(8.0)).p(px(10.0)).rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).child(line1).when(has2, |d| d.child(line2)));
        }
        let add_ls = div()
            .id("ws-l-add")
            .w_full()
            .h(px(34.0))
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
            .text_color(c::secondary())
            .cursor_pointer()
            .hover(|s| s.bg(c::field_hover()).text_color(c::text()))
            .child(icon(IconName::Plus, 13.0, c::secondary()))
            .child("添加监听")
            .on_click(cx.listener(|this, _, window, cx| {
                let used: Vec<u16> = this.web_ui().s_ls.iter().filter_map(|f| f.port.read(cx).value().trim().parse().ok()).collect();
                let (hp, sp) = this.web_target().ports;
                let port = [hp, sp, 8080, 8443, 8000, 3000].into_iter().find(|p| !used.contains(p)).unwrap_or(8081);
                let f = this.web_new_listener("", port, port == sp || port == 8443, None, false, window, cx);
                this.web_mut().s_ls.push(f);
                cx.notify();
            }));

        // ---- 后端地址 / 跳转地址:协议下拉 + 地址
        let scheme_menu = w.s_scheme_open.then(|| {
            let (t1, t2) = (cx.entity(), cx.entity());
            crate::ui::dropdown_menu(
                "ws-scheme-menu",
                vec![crate::ui::MenuItem::new("http"), crate::ui::MenuItem::new("https")],
                Some(w.s_scheme as usize),
                crate::ui::MenuSide::Below,
                36.0,
                move |i, _, cx| {
                    t1.update(cx, |h, cx| {
                        h.web_mut().s_scheme = i as u8;
                        h.web_mut().s_scheme_open = false;
                        cx.notify();
                    })
                },
                move |_, cx| {
                    t2.update(cx, |h, cx| {
                        h.web_mut().s_scheme_open = false;
                        cx.notify();
                    })
                },
            )
        });
        let scheme_btn = div()
            .id("ws-scheme")
            .h(px(34.0))
            .w_full()
            .px(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .rounded(px(t::RADIUS_MD))
            .bg(c::field())
            .border_1()
            .border_color(c::field_border())
            .cursor_pointer()
            .text_size(px(t::TYPE_CONTROL))
            .child(if w.s_scheme == 1 { "https" } else { "http" })
            .child(icon(IconName::ChevronDown, 14.0, c::ghost()))
            .on_click(cx.listener(|this, _, _, cx| {
                let v = &mut this.web_mut().s_scheme_open;
                *v = !*v;
                cx.notify();
            }));
        let url_row = |label: &'static str, hint: &'static str| {
            div()
                .w_full()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(5.0))
                .child(caption(label))
                .child(div().flex().flex_row().gap(px(6.0)).child(div().relative().w(px(96.0)).flex_none().child(scheme_btn).children(scheme_menu)).child(div().flex_1().min_w_0().child(ibox(&ins[1]))))
                .child(caption(hint))
        };

        let mut body = div().p(px(20.0)).flex().flex_col().gap(px(14.0)).child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(if editing { "编辑站点" } else { "新建站点" }));
        let mut form = div().flex().flex_col().gap(px(14.0));
        form = form.child(div().flex().flex_col().gap(px(6.0)).child(caption("监听:域名和端口,同一组里的域名共用一张证书")).child(lcards).child(add_ls));
        if !editing {
            form = form.child(div().w_full().flex_none().flex().flex_col().gap(px(5.0)).child(caption("站点名(可选)")).child(ibox(&ins[0])).child(caption("只用于配置文件名")));
        }
        form = form.child(div().flex().flex_col().gap(px(6.0)).child(caption("类型")).child(types));
        match kind {
            0 => {
                form = form.child(url_row("后端地址", "例:127.0.0.1:8080 或内网地址"));
            }
            1 => {
                form = form.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .child(card_toggle("ws-spa", "单页应用", "找不到的路径回到首页", w.s_spa).on_click(cx.listener(|this, _, _, cx| {
                            let v = &mut this.web_mut().s_spa;
                            *v = !*v;
                            cx.notify();
                        })))
                        .child(card_toggle("ws-idx", "目录列表", "没有首页时列出文件", w.s_index).on_click(cx.listener(|this, _, _, cx| {
                            let v = &mut this.web_mut().s_index;
                            *v = !*v;
                            cx.notify();
                        }))),
                );
                form = form.child(caption("网站文件放在服务器的 /opt/otter/websites/sites/<站点名>/,新建时会放一个占位首页"));
            }
            _ => {
                form = form.child(url_row("跳转到", "例:new.example.com(保留原路径)"));
                let mut codes = track();
                for (i, (code, l)) in [(301u16, "301 永久"), (302, "302 临时")].into_iter().enumerate() {
                    codes = codes.child(seg(("ws-code", i), l, w.s_code == code).on_click(cx.listener(move |this, _, _, cx| {
                        this.web_mut().s_code = code;
                        cx.notify();
                    })));
                }
                form = form.child(div().flex().flex_col().gap(px(6.0)).child(caption("跳转方式")).child(codes));
            }
        }
        // ---- 高级(默认收起)
        let adv_head = div()
            .id("ws-adv")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .text_size(px(t::TYPE_CONTROL))
            .text_color(c::secondary())
            .hover(|s| s.text_color(c::text()))
            .child(icon(if w.s_adv { IconName::ChevronDown } else { IconName::ChevronRight }, 14.0, c::ghost()))
            .child("高级")
            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("WebSocket、IPv6、上传大小、超时…"))
            .on_click(cx.listener(|this, _, _, cx| {
                let v = &mut this.web_mut().s_adv;
                *v = !*v;
                cx.notify();
            }));
        form = form.child(adv_head);
        if w.s_adv {
            let mut adv = div().flex().flex_col().gap(px(12.0)).pl(px(2.0));
            let mut toggles = div().flex().flex_row().gap(px(8.0));
            if kind == 0 {
                toggles = toggles.child(card_toggle("ws-ws", "WebSocket", "转发升级请求", w.s_ws).on_click(cx.listener(|this, _, _, cx| {
                    let v = &mut this.web_mut().s_ws;
                    *v = !*v;
                    cx.notify();
                })));
            }
            toggles = toggles.child(card_toggle("ws-ipv6", "监听 IPv6", "每个端口同时监听 [::]", w.s_ipv6).on_click(cx.listener(|this, _, _, cx| {
                let v = &mut this.web_mut().s_ipv6;
                *v = !*v;
                cx.notify();
            })));
            adv = adv.child(toggles);
            if any_ssl {
                adv = adv.child(div().flex().flex_row().gap(px(8.0)).child(card_toggle("ws-hsts", "HSTS", "浏览器长期只走 HTTPS", w.s_hsts).on_click(cx.listener(|this, _, _, cx| {
                    let v = &mut this.web_mut().s_hsts;
                    *v = !*v;
                    cx.notify();
                }))).child(div().flex_1().min_w_0()));
            }
            let lab = |label: &'static str, e: &Entity<InputState>| div().flex_1().min_w_0().flex().flex_col().gap(px(5.0)).child(caption(label)).child(ibox(e));
            let mut sizes = div().flex().flex_row().gap(px(8.0)).child(lab("上传大小上限", &ins[2]));
            if kind == 0 {
                sizes = sizes.child(lab("读取超时(秒)", &ins[3]));
            }
            adv = adv.child(sizes);
            form = form.child(adv);
        }
        body = body.child(div().id("ws-site-scroll").max_h(px(560.0)).overflow_y_scroll().child(form));
        let err = w.err.clone().map(|e| (e, c::danger()));
        body = body.child(self.web_actions(err.or(Some(("保存前会用 nginx -t 检查,不通过自动还原".into(), c::ghost()))), "保存", all_ok, cx));
        crate::ui::modal(
            "web-dlg-site",
            DLG_W_SITE_FORM,
            cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                "escape" => {
                    this.web_close(cx);
                    cx.stop_propagation();
                }
                "enter" if !e.keystroke.modifiers.shift => {
                    this.web_submit(cx);
                    cx.stop_propagation();
                }
                _ => {}
            }),
            [body.into_any_element()],
        )
    }

    fn web_dlg_raw(&self, file: &str, cx: &mut Context<Self>) -> Stateful<Div> {
        let w = self.web_ui();
        let body = self.web_body().child(area_field("配置原文", "保存前会用 nginx -t 检查,不通过自动还原;Otter 不再管理手工改过的站点结构", w.area.first(), 340.0));
        let err = w.err.clone().map(|e| (e, c::danger()));
        let foot = self.web_footer(err, "保存", true, cx);
        let sub = if file.ends_with(".off") { "已停用的站点配置" } else { "站点配置文件" };
        self.web_modal("web-dlg-raw", DLG_W_RAW, [self.web_title("编辑配置原文", sub).into_any_element(), body.into_any_element(), foot.into_any_element()], cx)
    }

    fn web_dlg_upload(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let w = self.web_ui();
        let Some(ins) = (!w.ins.is_empty()).then(|| w.ins.clone()) else { return div().id("web-dlg-empty") };
        let body = self
            .web_body()
            .child(field("证书名", "小写字母、数字、点和短横线", &ins[0]))
            .child(area_field("证书(含中间证书链)", "fullchain.pem 的内容", w.area.first(), 120.0))
            .child(area_field("私钥", "privkey.pem 的内容,上传后不再显示", w.area.get(1), 120.0));
        let err = w.err.clone().map(|e| (e, c::danger()));
        let foot = self.web_footer(err.or(Some(("会检查证书格式、私钥是否匹配、是否过期".into(), c::ghost()))), "上传", !self.web_val(0, cx).is_empty(), cx);
        self.web_modal("web-dlg-up", DLG_W_SITE, [self.web_title("上传证书", "已有的证书文件").into_any_element(), body.into_any_element(), foot.into_any_element()], cx)
    }

    fn web_dlg_self(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let w = self.web_ui();
        let Some(ins) = (w.ins.len() >= 3).then(|| w.ins.clone()) else { return div().id("web-dlg-empty") };
        let body = self.web_body().child(field("证书名", "留空则用第一个域名", &ins[0])).child(field("域名", "多个用空格分隔", &ins[1])).child(field("有效期(天)", "自签证书浏览器会提示不受信任,适合内网和测试", &ins[2]));
        let err = w.err.clone().map(|e| (e, c::danger()));
        let foot = self.web_footer(err, "生成", !self.web_val(1, cx).is_empty(), cx);
        self.web_modal("web-dlg-self", DLG_W_SELF, [self.web_title("生成自签证书", "用 openssl 在服务器上生成").into_any_element(), body.into_any_element(), foot.into_any_element()], cx)
    }

    fn web_dlg_issue(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let w = self.web_ui();
        let Some(ins) = (w.ins.len() >= 3).then(|| w.ins.clone()) else { return div().id("web-dlg-empty") };
        let dns_list: Vec<DnsAcct> = self.web_state().map(|s| s.dns.clone()).unwrap_or_default();
        let mut ca = div().flex().flex_row().gap(px(4.0));
        for (i, (z, l)) in [(false, "Let's Encrypt"), (true, "ZeroSSL")].into_iter().enumerate() {
            ca = ca.child(opt_chip(("wi-ca", i), l, w.i_zero == z).on_click(cx.listener(move |this, _, _, cx| {
                this.web_mut().i_zero = z;
                cx.notify();
            })));
        }
        let mut method = div().flex().flex_row().gap(px(4.0));
        for (i, (d, l)) in [(false, "HTTP 验证"), (true, "DNS 验证")].into_iter().enumerate() {
            method = method.child(opt_chip(("wi-m", i), l, w.i_dns == d).on_click(cx.listener(move |this, _, _, cx| {
                this.web_mut().i_dns = d;
                this.web_mut().i_warn = None;
                cx.notify();
            })));
        }
        let mut body = self.web_body().child(field("域名", "多个用空格分隔;泛域名写 *.example.com(只能用 DNS 验证)", &ins[0])).child(field("证书名", "留空则用第一个域名", &ins[1])).child(field("邮箱", "Let's Encrypt 用它发送到期提醒", &ins[2])).child(labeled("颁发机构", "", ca)).child(labeled("验证方式", if w.i_dns { "在 DNS 里添加一条临时记录,可申请泛域名" } else { "域名需已解析到这台服务器,且 80 端口能从外网访问" }, method));
        if w.i_dns {
            let mut chips = div().flex().flex_row().flex_wrap().gap(px(4.0));
            for (i, d) in dns_list.iter().enumerate() {
                let id = d.id.clone();
                chips = chips.child(opt_chip(("wi-d", i), d.name.clone(), w.i_acct.as_deref() == Some(&d.id)).on_click(cx.listener(move |this, _, _, cx| {
                    this.web_mut().i_acct = Some(id.clone());
                    cx.notify();
                })));
            }
            chips = chips.child(opt_chip(("wi-d", 999), "+ 添加账号", false).on_click(cx.listener(|this, _, window, cx| this.web_open_dns(window, cx))));
            body = body.child(labeled("DNS 账号", if dns_list.is_empty() { "还没有 DNS 账号,先添加一个" } else { "" }, chips));
        }
        body = body.child(
            div()
                .flex()
                .flex_row()
                .gap(px(24.0))
                .child(toggle("wi-auto", "自动续期", "到期前 30 天由服务器自动续", w.i_auto).on_click(cx.listener(|this, _, _, cx| {
                    let v = &mut this.web_mut().i_auto;
                    *v = !*v;
                    cx.notify();
                })))
                .child(toggle("wi-stg", "测试环境", "先试跑,签出的证书不受信任", w.i_staging).on_click(cx.listener(|this, _, _, cx| {
                    let v = &mut this.web_mut().i_staging;
                    *v = !*v;
                    cx.notify();
                }))),
        );
        if let Some(ws) = w.i_warn.as_ref().filter(|v| !v.is_empty()) {
            let mut b = div().flex().flex_col().gap(px(4.0)).p(px(10.0)).rounded(px(t::RADIUS_MD)).border_1().border_color(c::warn().opacity(0.35)).bg(c::warn().opacity(0.08));
            for m in ws {
                b = b.child(div().flex().flex_row().gap(px(6.0)).child(icon(IconName::TriangleAlert, 13.0, c::warn())).child(div().flex_1().text_size(px(t::TYPE_CAPTION)).text_color(c::secondary()).child(SharedString::from(m.clone()))));
            }
            body = body.child(b);
        }
        let warned = w.i_warn.as_ref().is_some_and(|v| !v.is_empty());
        let err = w.err.clone().map(|e| (e, c::danger()));
        let can = !self.web_val(0, cx).is_empty() && !self.web_val(2, cx).is_empty();
        let foot = self.web_footer(err, if warned { "仍然申请" } else { "申请" }, can, cx);
        self.web_modal("web-dlg-issue", DLG_W_ISSUE, [self.web_title("申请证书", "免费证书,通过 acme.sh 在服务器上申请").into_any_element(), body.into_any_element(), foot.into_any_element()], cx)
    }

    fn web_dlg_dns(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let w = self.web_ui();
        let Some(ins) = (w.ins.len() >= 3).then(|| w.ins.clone()) else { return div().id("web-dlg-empty") };
        let provs = web::dns_providers();
        let mut chips = div().flex().flex_row().gap(px(4.0));
        for (i, p) in provs.iter().enumerate() {
            chips = chips.child(opt_chip(("wn-p", i), p.1, w.d_prov == i).on_click(cx.listener(move |this, _, _, cx| {
                this.web_mut().d_prov = i;
                cx.notify();
            })));
        }
        let keys = provs[w.d_prov.min(2)].2;
        let hints: [&'static str; 3] = match w.d_prov {
            0 => ["", "具有 Zone.DNS 编辑权限的 API Token", ""],
            1 => ["", "AccessKey ID", "AccessKey Secret"],
            _ => ["", "DNSPod ID", "DNSPod Token"],
        };
        let mut body = self.web_body().child(field("账号名", "用来在申请证书时区分", &ins[0])).child(labeled("服务商", "", chips)).child(field(keys[0], hints[1], &ins[1]));
        if !keys[1].is_empty() {
            body = body.child(field(keys[1], hints[2], &ins[2]));
        }
        body = body.child(self.web_banner(IconName::Lock, c::muted(), "密钥保存在服务器 /opt/otter/websites/secrets/dns/,权限 600,只有 root 能读。这样 Otter 不开着,服务器也能自动续期;之后界面不会再显示它,要改就重新添加。"));
        let err = w.err.clone().map(|e| (e, c::danger()));
        let foot = self.web_footer(err, "保存", !self.web_val(0, cx).is_empty() && !self.web_val(1, cx).is_empty(), cx);
        self.web_modal("web-dlg-dns", DLG_W_DNS, [self.web_title("添加 DNS 账号", "用于 DNS 验证和泛域名证书").into_any_element(), body.into_any_element(), foot.into_any_element()], cx)
    }

    fn web_dlg_logs(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let w = self.web_ui();
        let title = match (&w.log_site, w.log_err) {
            (Some(s), _) => format!("{s} 的访问日志"),
            (None, true) => "OpenResty 错误日志".to_string(),
            _ => "OpenResty 访问日志".to_string(),
        };
        let text = if w.busy.is_some() { "读取中…".to_string() } else if w.log_text.trim().is_empty() { "(没有内容)".to_string() } else { w.log_text.clone() };
        let body = self.web_body().child(
            div()
                .id("web-log-box")
                .h(px(LOG_H))
                .overflow_y_scroll()
                .rounded(px(t::RADIUS_MD))
                .bg(c::field())
                .border_1()
                .border_color(c::field_border())
                .p(px(10.0))
                .font_family(MONO)
                .text_size(px(t::TYPE_CAPTION))
                .text_color(c::muted())
                .child(SharedString::from(text)),
        );
        let site = w.log_site.clone();
        let toggle_btn = crate::node_form::ctl_btn("web-log-flip", None, if w.log_err { "看访问日志" } else { "看错误日志" }, 34.0).on_click(cx.listener(move |this, _, _, cx| {
            let err = !this.web_ui().log_err;
            this.web_open_logs(site.clone(), err, cx);
        }));
        let foot = div().flex().flex_row().justify_end().gap(px(10.0)).px(px(24.0)).py(px(14.0)).border_t_1().border_color(c::line()).child(toggle_btn).child(crate::node_form::ctl_btn("wd-cancel", None, "关闭", 34.0).on_click(cx.listener(|this, _, _, cx| this.web_close(cx))));
        // 日志窗口只有一个「关闭」,不走带保存按钮的页脚
        let _ = &self;
        self.web_modal("web-dlg-log", DLG_W_LOG, [self.web_title("日志", "最近 300 行").child(div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(SharedString::from(title))).into_any_element(), body.into_any_element(), foot.into_any_element()], cx)
    }
}
