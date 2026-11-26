//! 站点管理抽屉:左侧菜单(域名设置 / 反向代理 / 负载均衡 / HTTPS / 高级),右侧是当前菜单的内容。
//! 只用于编辑已有的(Otter 生成的)站点;新建仍是 `web_dlg_site` 的简单弹窗。
//! 表单状态在 `SiteDrawer`,保存时 `to_spec` 转成 `SiteSpec`,经 `web::site_save_script` 走和弹窗一样的事务。
//! 设计见 `docs/plans/active/2026-11-26-site-drawer.md`。

use super::*;

mod extra;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Menu {
    Domains,
    Proxy,
    Balance,
    Content,
    Https,
    Advanced,
    Limits,
    /// 没有 Otter 头部的外部配置:只能改原文
    Raw,
}

pub(super) struct SiteDrawer {
    file: String,
    name: String,
    enabled: bool,
    /// 打开时配置正文已被手工改过:保存会按表单重新生成,覆盖手工改动
    modified: bool,
    /// 表单读自手改过的正文;`lost` 是保存后会丢失的内容(Otter 不管理的指令、注释)
    from_conf: bool,
    lost: Vec<String>,
    /// 手改过且读得懂的正文:保存时就地修改它
    old_text: Option<String>,
    /// 外部配置的原文编辑框(有值时抽屉只有「配置原文」一个菜单)
    raw: Option<Entity<TextareaState>>,
    menu: Menu,
    /// 0 反向代理 1 静态 2 跳转
    kind: u8,
    ls: Vec<LForm>,
    ipv6: bool,
    hsts: bool,
    /// 站点级 HTTPS:开关、证书、HTTP 访问方式(0 跳转 1 直接访问 2 禁止)、TLS 版本(对应 `web::TLS_VERSIONS`)、加密套件(0..3 预设,3 自定义)
    cert: Option<String>,
    https_port: Entity<InputState>,
    h2: bool,
    h3: bool,
    ocsp: bool,
    hsts_age: u32,
    hsts_sub: bool,
    hsts_pre: bool,
    http_mode: u8,
    tls: [bool; 4],
    cipher_mode: usize,
    ciphers: Entity<TextareaState>,
    /// 打开时各组监听的 HTTPS 设置不一致,保存会统一成站点级设置
    warn: Option<String>,
    max_body: Entity<InputState>,
    timeout: Entity<InputState>,
    locs: Vec<LocForm>,
    ups: Vec<UpForm>,
    spa: bool,
    index: bool,
    to: Entity<InputState>,
    to_https: bool,
    code: u16,
    /// 后加的高级项(压缩/访问控制/响应头/…/流量限制)
    x: extra::Extra,
    /// 正在编辑的列表项(编辑窗口)
    ed: Option<SdEdit>,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum EdKind {
    Listener,
    Loc,
    Up,
    Header,
    ErrPage,
    Rewrite,
    User,
}

enum Draft {
    L(LForm),
    Loc(LocForm),
    Up(UpForm),
    Kv(extra::KvForm),
}

/// 编辑窗口的草稿:点「确定」才写回列表(`slot` 为 None 是新增)
struct SdEdit {
    kind: EdKind,
    slot: Option<usize>,
    draft: Draft,
    err: Option<String>,
}

/// 表格操作列宽(两个图标按钮)
const COL_ACT: f32 = 56.0;

struct LocForm {
    path: Entity<InputState>,
    exact: bool,
    /// 后端组(`ups` 的下标);None 是直接填地址
    group: Option<usize>,
    addr: Entity<InputState>,
    https: bool,
    ws: bool,
    host: bool,
    strip: bool,
    timeout: Entity<InputState>,
}

struct UpForm {
    name: Entity<InputState>,
    policy: web::Policy,
    servers: Vec<SrvForm>,
    max_fails: Entity<InputState>,
    fail_timeout: Entity<InputState>,
}

struct SrvForm {
    addr: Entity<InputState>,
    weight: Entity<InputState>,
    backup: bool,
}

fn mk_in(ph: &str, v: &str, window: &mut Window, cx: &mut Context<Home>) -> Entity<InputState> {
    let e = cx.new(|cx| InputState::new(window, cx).placeholder(ph.to_string()));
    if !v.is_empty() {
        e.update(cx, |s, cx| s.set_value(v, window, cx));
    }
    cx.observe(&e, |_, _, cx| cx.notify()).detach();
    e
}

fn val(e: &Entity<InputState>, cx: &App) -> String {
    e.read(cx).value().trim().to_string()
}

fn opt_num(e: &Entity<InputState>, what: &str, cx: &App) -> Result<Option<u32>, String> {
    let v = val(e, cx);
    if v.is_empty() { Ok(None) } else { v.parse::<u32>().map(Some).map_err(|_| format!("{what}要填整数")) }
}

fn new_loc(path: &str, group: Option<usize>, https: bool, addr: &str, window: &mut Window, cx: &mut Context<Home>) -> LocForm {
    LocForm { path: mk_in("/", path, window, cx), exact: false, group, addr: mk_in("127.0.0.1:8080", addr, window, cx), https, ws: true, host: true, strip: false, timeout: mk_in("秒", "", window, cx) }
}

fn new_srv(addr: &str, weight: u32, backup: bool, window: &mut Window, cx: &mut Context<Home>) -> SrvForm {
    SrvForm { addr: mk_in("10.0.0.1:8080", addr, window, cx), weight: mk_in("1", &weight.to_string(), window, cx), backup }
}

fn new_up(name: &str, window: &mut Window, cx: &mut Context<Home>) -> UpForm {
    UpForm { name: mk_in("名称,如 api", name, window, cx), policy: web::Policy::RoundRobin, servers: vec![new_srv("", 1, false, window, cx)], max_fails: mk_in("默认 1", "", window, cx), fail_timeout: mk_in("默认 10", "", window, cx) }
}

fn dup_l(f: &LForm, window: &mut Window, cx: &mut Context<Home>) -> LForm {
    LForm { domains: mk_in("", &val(&f.domains, cx), window, cx), port: mk_in("", &val(&f.port, cx), window, cx), ssl: f.ssl, cert: f.cert.clone(), force: f.force }
}

fn dup_loc(l: &LocForm, window: &mut Window, cx: &mut Context<Home>) -> LocForm {
    LocForm { path: mk_in("/", &val(&l.path, cx), window, cx), exact: l.exact, group: l.group, addr: mk_in("127.0.0.1:8080", &val(&l.addr, cx), window, cx), https: l.https, ws: l.ws, host: l.host, strip: l.strip, timeout: mk_in("秒", &val(&l.timeout, cx), window, cx) }
}

fn dup_up(u: &UpForm, window: &mut Window, cx: &mut Context<Home>) -> UpForm {
    UpForm {
        name: mk_in("名称,如 api", &val(&u.name, cx), window, cx),
        policy: u.policy,
        servers: u.servers.iter().map(|s| SrvForm { addr: mk_in("10.0.0.1:8080", &val(&s.addr, cx), window, cx), weight: mk_in("1", &val(&s.weight, cx), window, cx), backup: s.backup }).collect(),
        max_fails: mk_in("默认 1", &val(&u.max_fails, cx), window, cx),
        fail_timeout: mk_in("默认 10", &val(&u.fail_timeout, cx), window, cx),
    }
}

/// 表格操作列:点击不冒泡到整行(整行点击 = 打开编辑窗口)
fn act_box(id: &'static str, i: usize) -> Stateful<Div> {
    div().id((id, i)).flex_shrink_0().flex().flex_row().justify_end().gap(px(2.0)).on_click(|_, _, cx| cx.stop_propagation())
}

impl SiteDrawer {
    /// 任何一组监听开了 HTTPS 就算站点启用了 HTTPS
    fn https(&self) -> bool {
        self.ls.iter().any(|f| f.ssl)
    }

    fn from_spec(file: &str, enabled: bool, s: &SiteSpec, window: &mut Window, cx: &mut Context<Home>, ports: (u16, u16), new_listener: &dyn Fn(&str, u16, &mut Window, &mut Context<Home>) -> LForm) -> SiteDrawer {
        // 站点级 HTTPS:同一组域名的明文监听 = 一行(HTTP 端口);SSL 监听折叠成开关 + 证书
        let mut groups: Vec<(Vec<String>, Option<u16>)> = Vec::new();
        for l in &s.listeners {
            let at = match groups.iter().position(|g| g.0 == l.domains) {
                Some(i) => i,
                None => {
                    groups.push((l.domains.clone(), None));
                    groups.len() - 1
                }
            };
            if !l.ssl() && groups[at].1.is_none() {
                groups[at].1 = Some(l.port);
            }
        }
        let ssl_ls: Vec<&web::Listener> = s.listeners.iter().filter(|l| l.ssl()).collect();
        let plain: Vec<&web::Listener> = s.listeners.iter().filter(|l| !l.ssl()).collect();
        let https = !ssl_ls.is_empty();
        let cert = ssl_ls.first().and_then(|l| l.cert.clone());
        let http_mode = if !https || plain.iter().any(|l| l.force_https) { 0 } else if plain.is_empty() { 2 } else { 1 };
        let mixed = https && (ssl_ls.iter().any(|l| l.cert != cert || l.port != ssl_ls[0].port));
        let warn = mixed.then(|| "各组监听原来用了不同的证书或 HTTPS 端口,保存后会统一成「HTTPS」里的证书和 HTTPS 端口".to_string());
        let mut ls: Vec<LForm> = groups.iter().map(|g| new_listener(&g.0.join(" "), g.1.unwrap_or(ports.0), window, cx)).collect();
        for (f, g) in ls.iter_mut().zip(&groups) {
            f.ssl = ssl_ls.iter().any(|l| l.domains == g.0);
        }
        let tls = {
            let mut t = [false, false, true, true];
            if let Some(p) = &s.adv.tls_protocols {
                t = web::TLS_VERSIONS.map(|v| p.iter().any(|x| x == v));
            }
            t
        };
        let cur = s.adv.ciphers.clone().unwrap_or_default();
        let cipher_mode = web::CIPHER_PRESETS.iter().position(|(_, v)| *v == cur).unwrap_or(3);
        let https_port = mk_in("443", &ssl_ls.first().map(|l| l.port).unwrap_or(ports.1).to_string(), window, cx);
        let ciphers = cx.new(|cx| TextareaState::new(window, cx).rows(4));
        if cipher_mode == 3 {
            ciphers.update(cx, |s, cx| s.set_value(cur.as_str(), window, cx));
        }
        cx.observe(&ciphers, |_, _, cx| cx.notify()).detach();
        let (mut kind, mut locs, mut ups, mut spa, mut index, mut to, mut to_https, mut code) = (0u8, Vec::new(), Vec::new(), true, false, String::new(), false, 301u16);
        match &s.kind {
            SiteKind::Proxy { locations, upstreams } => {
                for u in upstreams {
                    let mut f = new_up(&u.name, window, cx);
                    f.policy = u.policy;
                    f.servers = u.servers.iter().map(|v| new_srv(&v.addr, v.weight, v.backup, window, cx)).collect();
                    if let Some(m) = u.max_fails {
                        f.max_fails = mk_in("默认 1", &m.to_string(), window, cx);
                    }
                    if let Some(m) = u.fail_timeout {
                        f.fail_timeout = mk_in("默认 10", &m.to_string(), window, cx);
                    }
                    ups.push(f);
                }
                for l in locations {
                    let (group, https, addr) = match &l.backend {
                        web::Backend::Url(u) => {
                            let (sch, a) = split_scheme(u);
                            (None, sch == 1, a)
                        }
                        web::Backend::Group { https, name } => (upstreams.iter().position(|u| &u.name == name), *https, String::new()),
                    };
                    let mut f = new_loc(&l.path, group, https, &addr, window, cx);
                    (f.exact, f.ws, f.host, f.strip) = (l.exact, l.ws, l.host, l.strip);
                    if let Some(t) = l.timeout {
                        f.timeout = mk_in("秒", &t.to_string(), window, cx);
                    }
                    locs.push(f);
                }
            }
            SiteKind::Static { spa: a, index: b } => (kind, spa, index) = (1, *a, *b),
            SiteKind::Redirect { to: t, code: c } => {
                kind = 2;
                let (sch, a) = split_scheme(t);
                (to, to_https, code) = (a, sch == 1, *c);
            }
        }
        SiteDrawer {
            file: file.to_string(),
            name: s.name.clone(),
            enabled,
            modified: false,
            from_conf: false,
            lost: Vec::new(),
            old_text: None,
            raw: None,
            menu: Menu::Domains,
            ed: None,
            kind,
            ls,
            ipv6: s.ipv6,
            hsts: s.hsts,
            cert,
            https_port,
            h2: !s.adv.no_http2,
            h3: s.adv.http3,
            ocsp: s.adv.ocsp,
            hsts_age: s.adv.hsts_age.unwrap_or(31536000),
            hsts_sub: s.adv.hsts_sub,
            hsts_pre: s.adv.hsts_preload,
            http_mode,
            tls,
            cipher_mode,
            ciphers,
            warn,
            max_body: mk_in("如 100m,留空沿用全局设置", s.adv.max_body.as_deref().unwrap_or(""), window, cx),
            timeout: mk_in("默认 300", &s.adv.read_timeout.map(|t| t.to_string()).unwrap_or_default(), window, cx),
            locs,
            ups,
            spa,
            index,
            to: mk_in("example.com", &to, window, cx),
            to_https,
            code,
            x: extra::Extra::new(&s.adv, window, cx),
        }
    }

    /// 表单 → 站点模型(并检查)
    fn to_spec(&self, cx: &App) -> Result<SiteSpec, String> {
        let mut listeners = Vec::new();
        if self.https() && self.cert.is_none() {
            return Err("开启 HTTPS 还没选证书".into());
        }
        let https_port: u16 = if self.https() { val(&self.https_port, cx).parse().map_err(|_| "HTTPS 端口要填 1–65535 的数字".to_string())? } else { 443 };
        for (i, f) in self.ls.iter().enumerate() {
            let port: u16 = val(&f.port, cx).parse().map_err(|_| format!("第 {} 组监听的 HTTP 端口要填 1–65535 的数字", i + 1))?;
            let domains = web::split_domains(&f.domains.read(cx).value());
            // 这组开了 HTTPS:按「HTTP 访问」决定明文监听是跳转、直接访问还是不监听
            if !(f.ssl && self.http_mode == 2) {
                listeners.push(web::Listener { domains: domains.clone(), port, cert: None, force_https: f.ssl && self.http_mode == 0 });
            }
            if f.ssl {
                listeners.push(web::Listener { domains, port: https_port, cert: self.cert.clone(), force_https: false });
            }
        }
        let kind = match self.kind {
            0 => {
                let mut upstreams = Vec::new();
                for u in &self.ups {
                    let mut servers = Vec::new();
                    for s in &u.servers {
                        let w = val(&s.weight, cx);
                        servers.push(web::UServer { addr: val(&s.addr, cx), weight: if w.is_empty() { 1 } else { w.parse().map_err(|_| "权重要填 1–100 的整数".to_string())? }, backup: s.backup });
                    }
                    upstreams.push(web::Upstream { name: val(&u.name, cx), policy: u.policy, servers, max_fails: opt_num(&u.max_fails, "失败次数", cx)?, fail_timeout: opt_num(&u.fail_timeout, "摘除时间", cx)? });
                }
                let mut locations = Vec::new();
                for l in &self.locs {
                    let backend = match l.group {
                        Some(g) => web::Backend::Group { https: l.https, name: upstreams.get(g).map(|u| u.name.clone()).unwrap_or_default() },
                        None => {
                            let a = val(&l.addr, cx);
                            web::Backend::Url(if a.contains("://") { a } else { format!("{}://{a}", if l.https { "https" } else { "http" }) })
                        }
                    };
                    locations.push(web::Location { path: val(&l.path, cx), exact: l.exact, backend, ws: l.ws, host: l.host, timeout: opt_num(&l.timeout, "读取超时", cx)?, strip: l.strip });
                }
                SiteKind::Proxy { locations, upstreams }
            }
            1 => SiteKind::Static { spa: self.spa, index: self.index },
            _ => {
                let a = val(&self.to, cx);
                SiteKind::Redirect { to: if a.contains("://") { a } else { format!("{}://{a}", if self.to_https { "https" } else { "http" }) }, code: self.code }
            }
        };
        let tls_protocols = (self.tls != [false, false, true, true]).then(|| web::TLS_VERSIONS.iter().zip(self.tls).filter(|(_, on)| *on).map(|(v, _)| v.to_string()).collect::<Vec<_>>());
        let ciphers = if self.cipher_mode < 3 { Some(web::CIPHER_PRESETS[self.cipher_mode].1.to_string()) } else { Some(self.ciphers.read(cx).value().split_whitespace().collect::<String>()) }.filter(|v| !v.is_empty());
        let adv = web::Advanced { max_body: Some(val(&self.max_body, cx)).filter(|v| !v.is_empty()), read_timeout: opt_num(&self.timeout, "读取超时", cx)?, tls_protocols, ciphers, no_http2: !self.h2, http3: self.h3, ocsp: self.ocsp, hsts_age: (self.hsts_age != 31536000).then_some(self.hsts_age), hsts_sub: self.hsts_sub, hsts_preload: self.hsts_pre, ..Default::default() };
        let adv = self.x.fill(adv, self.kind == 1, cx)?;
        let spec = SiteSpec { name: self.name.clone(), kind, hsts: self.hsts && self.https(), listeners, ipv6: self.ipv6, adv };
        spec.check()?;
        Ok(spec)
    }

    /// 编辑窗口开着就关掉它,返回是否关了
    pub(super) fn close_edit(&mut self) -> bool {
        self.ed.take().is_some()
    }

    pub(super) fn set_menu(&mut self, m: Menu) {
        self.menu = m;
    }

    fn menus(&self) -> Vec<(Menu, IconName, &'static str)> {
        if self.raw.is_some() {
            return vec![(Menu::Raw, IconName::FileText, "配置原文")];
        }
        let mut v = vec![(Menu::Domains, IconName::Globe, "域名设置")];
        match self.kind {
            0 => {
                v.push((Menu::Proxy, IconName::ArrowRightLeft, "反向代理"));
                v.push((Menu::Balance, IconName::Network, "负载均衡"));
            }
            1 => v.push((Menu::Content, IconName::Folder, "站点内容")),
            _ => v.push((Menu::Content, IconName::Route, "跳转")),
        }
        v.push((Menu::Https, IconName::Lock, "HTTPS"));
        v.push((Menu::Limits, IconName::Gauge, "流量限制"));
        v.push((Menu::Advanced, IconName::SlidersHorizontal, "高级"));
        v
    }
}

/// 出错时跳到相关菜单
fn menu_for_error(e: &str) -> Option<Menu> {
    if e.contains("后端组") || e.contains("成员") || e.contains("权重") || e.contains("摘除") {
        Some(Menu::Balance)
    } else if e.contains("location") || e.contains("后端地址") || e.contains("路径") || e.contains("去掉前缀") {
        Some(Menu::Proxy)
    } else if e.contains("证书") || e.contains("HTTPS") {
        Some(Menu::Https)
    } else if e.contains("域名") || e.contains("监听") || e.contains("端口") || e.contains("重复") {
        Some(Menu::Domains)
    } else if e.contains("请求速率") || e.contains("并发连接") || e.contains("限速") || e.contains("不限制的 IP") {
        Some(Menu::Limits)
    } else if e.contains("缓存") || e.contains("防盗链") || e.contains("首页") {
        Some(Menu::Content)
    } else if e.contains("上传") || e.contains("超时") || e.contains("压缩") || e.contains("IP") || e.contains("认证") || e.contains("响应头") || e.contains("日志") || e.contains("错误页") || e.contains("重写") || e.contains("代理") {
        Some(Menu::Advanced)
    } else {
        None
    }
}

fn ibox(i: &Entity<InputState>) -> Div {
    div()
        .h(px(34.0))
        .flex()
        .flex_row()
        .items_center()
        .pl(px(4.0))
        .pr(px(6.0))
        .rounded(px(t::RADIUS_MD))
        .bg(c::field())
        .border_1()
        .border_color(c::field_border())
        .text_size(px(t::TYPE_CONTROL))
        .child(div().flex_1().min_w_0().child(Input::new(i).appearance(false).with_size(px(12.0 / 0.875))))
}

fn caption(s: impl Into<SharedString>) -> Div {
    div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(s.into())
}

fn sub_card() -> Div {
    div().flex_shrink_0().flex().flex_col().gap(px(10.0)).p(px(12.0)).rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border())
}

fn add_btn(id: &'static str, text: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
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
        .child(text)
}

fn section(title: &'static str, sub: &'static str) -> Div {
    div().flex_shrink_0().flex().flex_col().gap(px(3.0)).child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(title)).child(caption(sub))
}

impl Home {
    fn sdm(&mut self) -> Option<&mut SiteDrawer> {
        self.web_mut().sd.as_deref_mut()
    }

    fn sd(&self) -> Option<&SiteDrawer> {
        self.web_ui().sd.as_deref()
    }

    pub(super) fn web_open_drawer(&mut self, e: &SiteEntry, window: &mut Window, cx: &mut Context<Self>) {
        // 没有 Otter 头部的外部配置:抽屉里只放原文编辑
        let dummy;
        let spec = match &e.spec {
            Some(s) => s,
            None => {
                dummy = SiteSpec { name: e.file.trim_end_matches(".off").trim_end_matches(".conf").to_string(), kind: SiteKind::Static { spa: false, index: false }, listeners: Vec::new(), ipv6: false, hsts: false, adv: web::Advanced::default() };
                &dummy
            }
        };
        let sd = SiteDrawer::from_spec(&e.file, e.enabled, spec, window, cx, self.web_target().ports, &|d, p, w, cx| {
            let mk = |v: &str, w: &mut Window, cx: &mut Context<Home>| mk_in("", v, w, cx);
            LForm { domains: mk(d, w, cx), port: mk(&p.to_string(), w, cx), ssl: false, cert: None, force: false }
        });
        let mut sd = sd;
        sd.modified = e.modified;
        sd.from_conf = e.from_conf;
        sd.lost = e.lost.clone();
        sd.old_text = (e.modified && e.from_conf).then(|| e.text.clone());
        if e.spec.is_none() {
            let text = e.text.clone();
            let area = cx.new(|cx| TextareaState::new(window, cx).rows(24));
            area.update(cx, |s, cx| s.set_value(text.as_str(), window, cx));
            cx.observe(&area, |_, _, cx| cx.notify()).detach();
            sd.raw = Some(area);
            sd.menu = Menu::Raw;
        }
        let w = self.web_mut();
        w.sd = Some(Box::new(sd));
        w.err = None;
        w.ins.clear();
        w.area.clear();
        w.stack.clear();
        w.dlg = Some(Dlg::Drawer);
        cx.notify();
    }

    pub(super) fn web_sd_save(&mut self, cx: &mut Context<Self>) {
        let tg = self.web_target();
        let Some(sd) = self.sd() else { return };
        let enabled = sd.enabled;
        if let Some(raw) = &sd.raw {
            let (file, text) = (sd.file.clone(), raw.read(cx).value().to_string());
            match web::site_raw_script(&file, &text) {
                Ok(script) => {
                    self.web_mut().err = None;
                    self.web_run("保存配置", script, OpTag::Save, cx);
                }
                Err(e) => self.web_mut().err = Some(e),
            }
            cx.notify();
            return;
        }
        match sd.to_spec(cx).and_then(|s| web::site_save_script_from(&s, enabled, &tg, sd.old_text.as_deref())) {
            Ok(script) => {
                self.web_mut().err = None;
                self.web_run("保存站点", script, OpTag::Save, cx);
            }
            Err(e) => {
                if let (Some(m), Some(sd)) = (menu_for_error(&e), self.sdm()) {
                    sd.menu = m;
                }
                self.web_mut().err = Some(e);
            }
        }
        cx.notify();
    }

    pub(super) fn web_sd_overlay(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(sd) = self.sd() else { return Vec::new() };
        let menu = sd.menu;
        let kind_label = if sd.raw.is_some() { "外部配置" } else { ["反向代理", "静态站点", "跳转"][sd.kind.min(2) as usize] };
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
            .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(SharedString::from(format!("站点设置 · {}", sd.name))))
            .child(pill(kind_label, c::secondary(), c::idle_soft()))
            .child(div().flex_1())
            .child(tip(icon_btn(("web-sd-x", 0), IconName::X, c::muted()).on_click(cx.listener(|this, _, _, cx| this.web_close(cx))), "关闭"));
        let mut nav = div().w(px(176.0)).flex_shrink_0().h_full().flex().flex_col().gap(px(2.0)).p(px(8.0)).border_r_1().border_color(c::border());
        for (i, (m, ic, label)) in sd.menus().into_iter().enumerate() {
            let badge = match m {
                Menu::Proxy => Some(sd.locs.len()),
                Menu::Balance => Some(sd.ups.len()),
                _ => None,
            }
            .filter(|n| *n > 0);
            let on = m == menu;
            nav = nav.child(
                div()
                    .id(("web-sd-menu", i))
                    .h(px(34.0))
                    .px(px(10.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(t::RADIUS_MD))
                    .cursor_pointer()
                    .text_size(px(t::TYPE_CONTROL))
                    .when(on, |d| d.bg(c::accent_soft()).text_color(c::text()).font_weight(FontWeight::SEMIBOLD))
                    .when(!on, |d| d.text_color(c::muted()).hover(|s| s.bg(c::field_hover()).text_color(c::text())))
                    .child(icon(ic, 14.0, if on { c::text() } else { c::muted() }))
                    .child(div().flex_1().child(label))
                    .children(badge.map(|n| pill(n.to_string(), c::muted(), c::idle_soft())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(sd) = this.sdm() {
                            sd.menu = m;
                        }
                        cx.notify();
                    })),
            );
        }
        let content = match menu {
            Menu::Domains => self.sd_domains(cx),
            Menu::Proxy => self.sd_proxy(cx),
            Menu::Balance => self.sd_balance(cx),
            Menu::Content => self.sd_content(cx),
            Menu::Https => self.sd_https(cx),
            Menu::Advanced => self.sd_advanced(cx),
            Menu::Limits => self.sd_limits(cx),
            Menu::Raw => self.sd_raw(cx),
        };
        let body = div().flex_1().min_h_0().flex().flex_row().child(nav).child(div().id("web-sd-body").flex_1().min_w_0().h_full().overflow_y_scroll().p(px(20.0)).flex().flex_col().gap(px(14.0)).child(content));
        let warn = self
            .sd()
            .and_then(|sd| match (sd.modified, sd.from_conf) {
                (true, true) if sd.lost.is_empty() => sd.warn.clone(),
                (true, true) => {
                    let shown: Vec<String> = sd.lost.iter().take(3).cloned().collect();
                    Some(format!("配置里有 {} 项 Otter 不管理的内容,保存时会原样保留:{}{}", sd.lost.len(), shown.join(";"), if sd.lost.len() > 3 { " …" } else { "" }))
                }
                (true, false) => Some("配置被手工改过,且有 Otter 读不懂的结构:保存会按表单重新生成,手工改动会被覆盖".to_string()),
                _ => sd.warn.clone(),
            })
            .map(|w| (w, c::warn()));
        let err = self.web_ui().err.clone().map(|e| (e, c::danger())).or(warn);
        let foot = self.web_footer(err, "保存", true, cx);
        let mut v = vec![crate::ui::drawer("web-sd", 860.0, |_, _, _| {}, [head.into_any_element(), body.into_any_element(), foot.into_any_element()]).into_any_element()];
        v.extend(self.sd_edit_dialog(cx));
        v
    }

    // ---------------------------------------------------------------- 表格与编辑窗口的公用部件

    pub(super) fn web_sd_edit(&mut self, kind: EdKind, slot: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let (hp, sp) = self.web_target().ports;
        let Some(sd) = self.sd() else { return };
        let draft = match (kind, slot) {
            (EdKind::Listener, Some(i)) => sd.ls.get(i).map(|f| Draft::L(dup_l(f, window, cx))),
            (EdKind::Listener, None) => {
                let used: Vec<u16> = sd.ls.iter().filter_map(|f| val(&f.port, cx).parse().ok()).collect();
                let port = [hp, sp, 8080, 8443, 8000, 3000].into_iter().find(|p| !used.contains(p)).unwrap_or(8081);
                Some(Draft::L(LForm { domains: mk_in("example.com,多个用空格分隔", "", window, cx), port: mk_in("", &port.to_string(), window, cx), ssl: false, cert: None, force: false }))
            }
            (EdKind::Loc, Some(i)) => sd.locs.get(i).map(|l| Draft::Loc(dup_loc(l, window, cx))),
            (EdKind::Loc, None) => {
                let used: Vec<String> = sd.locs.iter().map(|l| val(&l.path, cx)).collect();
                let path = ["/api/", "/static/", "/admin/", "/ws/"].into_iter().find(|p| !used.iter().any(|u| u == p)).unwrap_or("/new/");
                Some(Draft::Loc(new_loc(path, None, false, "", window, cx)))
            }
            (EdKind::Up, Some(i)) => sd.ups.get(i).map(|u| Draft::Up(dup_up(u, window, cx))),
            (EdKind::Up, None) => Some(Draft::Up(new_up(&format!("group{}", sd.ups.len() + 1), window, cx))),
            (EdKind::Header | EdKind::ErrPage | EdKind::Rewrite | EdKind::User, _) => extra::open_draft(sd, kind, slot, window, cx),
            _ => None,
        };
        if let (Some(draft), Some(sd)) = (draft, self.sdm()) {
            sd.ed = Some(SdEdit { kind, slot, draft, err: None });
        }
        cx.notify();
    }

    /// 编辑窗口点「确定」:校验草稿,通过就写回列表
    fn web_sd_edit_ok(&mut self, cx: &mut Context<Self>) {
        let certs: Vec<CertEntry> = self.web_state().map(|s| s.certs.clone()).unwrap_or_default();
        let Some(sd) = self.sdm() else { return };
        let Some(mut ed) = sd.ed.take() else { return };
        let check: Result<(), String> = match (&ed.kind, &ed.draft) {
            (EdKind::Listener, Draft::L(f)) => {
                if web::split_domains(&f.domains.read(cx).value()).is_empty() {
                    Err("域名不能为空".into())
                } else if val(&f.port, cx).parse::<u16>().is_err() {
                    Err("HTTP 端口要填 1–65535 的数字".into())
                } else {
                    Ok(())
                }
            }
            (_, Draft::Loc(l)) => {
                if !val(&l.path, cx).starts_with('/') {
                    Err("路径要以 / 开头".into())
                } else if l.group.is_none() && val(&l.addr, cx).is_empty() {
                    Err("后端地址不能为空".into())
                } else {
                    opt_num(&l.timeout, "读取超时", cx).map(|_| ())
                }
            }
            (_, Draft::Up(u)) => {
                if val(&u.name, cx).is_empty() {
                    Err("名称不能为空".into())
                } else if u.servers.is_empty() || u.servers.iter().any(|s| val(&s.addr, cx).is_empty()) {
                    Err("成员地址不能为空,至少要有一个成员".into())
                } else {
                    Ok(())
                }
            }
            (_, Draft::Kv(f)) => extra::commit(sd, ed.kind, ed.slot, f, cx),
            _ => Ok(()),
        };
        if let Err(e) = check {
            ed.err = Some(e);
            sd.ed = Some(ed);
            cx.notify();
            return;
        }
        // 第一次给站点开 HTTPS:顺手选一张覆盖这些域名的证书
        if let Draft::L(f) = &ed.draft {
            if f.ssl && sd.cert.is_none() {
                let doms = web::split_domains(&f.domains.read(cx).value());
                sd.cert = certs.iter().find(|c| !doms.is_empty() && doms.iter().all(|d| c.covers(d))).map(|c| c.name.clone());
            }
        }
        match ed.draft {
            Draft::L(f) => match ed.slot {
                Some(i) => {
                    if let Some(x) = sd.ls.get_mut(i) {
                        *x = f;
                    }
                }
                None => sd.ls.push(f),
            },
            Draft::Loc(f) => match ed.slot {
                Some(i) => {
                    if let Some(x) = sd.locs.get_mut(i) {
                        *x = f;
                    }
                }
                None => sd.locs.push(f),
            },
            Draft::Up(f) => match ed.slot {
                Some(i) => {
                    if let Some(x) = sd.ups.get_mut(i) {
                        *x = f;
                    }
                }
                None => sd.ups.push(f),
            },
            Draft::Kv(_) => {}
        }
        cx.notify();
    }

    fn ed_l(&mut self) -> Option<&mut LForm> {
        match &mut self.sdm()?.ed.as_mut()?.draft {
            Draft::L(f) => Some(f),
            _ => None,
        }
    }

    fn ed_loc(&mut self) -> Option<&mut LocForm> {
        match &mut self.sdm()?.ed.as_mut()?.draft {
            Draft::Loc(f) => Some(f),
            _ => None,
        }
    }

    fn ed_up(&mut self) -> Option<&mut UpForm> {
        match &mut self.sdm()?.ed.as_mut()?.draft {
            Draft::Up(f) => Some(f),
            _ => None,
        }
    }

    fn chip_loc(&self, id: (&'static str, usize), text: impl Into<SharedString>, on: bool, f: impl Fn(&mut LocForm) + 'static, cx: &mut Context<Self>) -> Stateful<Div> {
        opt_chip(id, text, on).on_click(cx.listener(move |this, _, _, cx| {
            if let Some(l) = this.ed_loc() {
                f(l);
            }
            cx.notify();
        }))
    }

    fn chip_up(&self, id: (&'static str, usize), text: impl Into<SharedString>, on: bool, f: impl Fn(&mut UpForm) + 'static, cx: &mut Context<Self>) -> Stateful<Div> {
        opt_chip(id, text, on).on_click(cx.listener(move |this, _, _, cx| {
            if let Some(u) = this.ed_up() {
                f(u);
            }
            cx.notify();
        }))
    }

    /// 编辑窗口(盖在抽屉上):内容按 `SdEdit.kind` 不同
    fn sd_edit_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sd = self.sd()?;
        let ed = sd.ed.as_ref()?;
        let new = ed.slot.is_none();
        let (title, sub): (&'static str, &'static str) = match ed.kind {
            EdKind::Listener => (if new { "添加监听" } else { "编辑监听" }, "一组域名 + HTTP 端口,可同时开启 HTTPS"),
            EdKind::Loc => (if new { "添加路径" } else { "编辑路径" }, "按路径把请求转发到后端;最长的路径优先匹配"),
            EdKind::Up => (if new { "添加后端组" } else { "编辑后端组" }, "把一组后端定义成 upstream,多条路径可以共用"),
            k => extra::dialog_title(k, new),
        };
        let mut body = div().flex().flex_col().gap(px(14.0));
        match &ed.draft {
            Draft::L(f) => {
                let sp = val(&sd.https_port, cx);
                let mode = ["跳转到 HTTPS", "可直接访问", "禁止访问"][sd.http_mode.min(2) as usize];
                let summary = if f.ssl {
                    format!("端口 {sp} · 证书 {} · HTTP {mode}", sd.cert.as_deref().unwrap_or("(确定后自动选择)"))
                } else {
                    "关闭:只提供 HTTP 访问".to_string()
                };
                let line = |label: &'static str, sub: SharedString, right: AnyElement| {
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(12.0))
                        .h(px(56.0))
                        .px(px(14.0))
                        .child(div().flex_1().min_w_0().flex().flex_col().child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::MEDIUM).child(label)).child(div().truncate().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(sub)))
                        .child(right)
                };
                let https_sw = div().id("web-sd-ed-ssl").cursor_pointer().child(switch("web-sd-ed-ssl-sw", f.ssl)).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(m) = this.ed_l() {
                        m.ssl = !m.ssl;
                    }
                    cx.notify();
                }));
                let card = div()
                    .flex()
                    .flex_col()
                    .rounded(px(t::RADIUS_MD))
                    .border_1()
                    .border_color(c::field_border())
                    .child(line("HTTP", "明文访问的端口".into(), div().w(px(110.0)).child(ibox(&f.port)).into_any_element()))
                    .child(div().h(px(1.0)).bg(c::line()))
                    .child(line("HTTPS", summary.into(), https_sw.into_any_element()));
                body = body.child(field("域名", "多个用空格分隔", &f.domains)).child(labeled("监听端口", "证书、HTTP 访问方式、TLS 版本等对整个站点生效,在「HTTPS」菜单里设置", card));
            }
            Draft::Loc(l) => {
                let groups: Vec<String> = sd.ups.iter().map(|u| val(&u.name, cx)).collect();
                let exact = l.exact;
                let mut be = div().flex().flex_row().flex_wrap().items_center().gap(px(6.0));
                be = be.child(self.chip_loc(("web-sd-ed-be", 0), "地址", l.group.is_none(), |l| l.group = None, cx));
                for (gi, g) in groups.iter().enumerate() {
                    let label = if g.is_empty() { format!("后端组 {}", gi + 1) } else { format!("后端组 {g}") };
                    be = be.child(self.chip_loc(("web-sd-ed-be", gi + 1), label, l.group == Some(gi), move |l| l.group = Some(gi), cx));
                }
                let https = l.https;
                let proto = div().flex().flex_row().gap(px(4.0)).children([(false, "http"), (true, "https")].into_iter().enumerate().map(|(k, (v, text))| self.chip_loc(("web-sd-ed-sch", k), text, https == v, move |l| l.https = v, cx)));
                body = body
                    .child(field("路径", "以 / 开头,如 /、/api/", &l.path))
                    .child(labeled(
                        "匹配方式",
                        "前缀:这个路径下的所有请求;精确:只匹配这一个地址",
                        div().flex().flex_row().gap(px(4.0)).child(self.chip_loc(("web-sd-ed-m", 0), "前缀", !exact, |l| l.exact = false, cx)).child(self.chip_loc(
                            ("web-sd-ed-m", 1),
                            "精确",
                            exact,
                            |l| {
                                l.exact = true;
                                l.strip = false;
                            },
                            cx,
                        )),
                    ))
                    .child(labeled("后端", "", be));
                if l.group.is_none() {
                    body = body.child(labeled("后端地址", "不写协议时用左边选的;也可以直接写 http://主机:端口", div().flex().flex_row().items_center().gap(px(6.0)).child(proto).child(div().flex_1().min_w_0().child(ibox(&l.addr)))));
                } else {
                    body = body.child(labeled("协议", "转发给后端组成员用的协议;成员在「负载均衡」里设置", proto));
                }
                let (ws, host, strip) = (l.ws, l.host, l.strip);
                body = body
                    .child(labeled(
                        "选项",
                        "",
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(6.0))
                            .child(self.chip_loc(("web-sd-ed-f", 0), "WebSocket", ws, |l| l.ws = !l.ws, cx))
                            .child(self.chip_loc(("web-sd-ed-f", 1), "透传 Host", host, |l| l.host = !l.host, cx))
                            .child(self.chip_loc(
                                ("web-sd-ed-f", 2),
                                "去掉路径前缀",
                                strip,
                                |l| {
                                    if !l.exact {
                                        l.strip = !l.strip
                                    }
                                },
                                cx,
                            )),
                    ))
                    .child(field("读取超时(秒)", "留空沿用站点的默认值", &l.timeout));
            }
            Draft::Up(u) => {
                let pol = u.policy;
                let mut policy = div().flex().flex_row().gap(px(4.0));
                for (k, p) in [web::Policy::RoundRobin, web::Policy::LeastConn, web::Policy::IpHash].into_iter().enumerate() {
                    policy = policy.child(self.chip_up(("web-sd-ed-pol", k), p.label(), pol == p, move |u| u.policy = p, cx));
                }
                let many = u.servers.len() > 1;
                let mut srvs = div().flex().flex_col().gap(px(6.0));
                srvs = srvs.child(div().flex().flex_row().gap(px(6.0)).child(caption("主机:端口").flex_1()).child(caption("权重").w(px(56.0))).child(div().w(px(if many { 30.0 + 52.0 } else { 52.0 }))));
                for (si, s) in u.servers.iter().enumerate() {
                    srvs = srvs.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .child(div().flex_1().min_w_0().child(ibox(&s.addr)))
                            .child(div().w(px(56.0)).flex_none().child(ibox(&s.weight)))
                            .child(self.chip_up(
                                ("web-sd-ed-bk", si),
                                "备用",
                                s.backup,
                                move |u| {
                                    if let Some(s) = u.servers.get_mut(si) {
                                        s.backup = !s.backup;
                                    }
                                },
                                cx,
                            ))
                            .when(many, |d| {
                                d.child(tip(
                                    icon_btn(("web-sd-ed-srv-rm", si), IconName::X, c::muted()).on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(u) = this.ed_up() {
                                            if si < u.servers.len() {
                                                u.servers.remove(si);
                                            }
                                        }
                                        cx.notify();
                                    })),
                                    "删除这个成员",
                                ))
                            }),
                    );
                }
                let add = div().id("web-sd-ed-srv-add").h(px(28.0)).flex().flex_row().items_center().gap(px(6.0)).cursor_pointer().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).hover(|s| s.text_color(c::text())).child(icon(IconName::Plus, 13.0, c::secondary())).child("添加成员").on_click(cx.listener(|this, _, window, cx| {
                    let s = new_srv("", 1, false, window, cx);
                    if let Some(u) = this.ed_up() {
                        u.servers.push(s);
                    }
                    cx.notify();
                }));
                let health = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().w(px(80.0)).child(ibox(&u.max_fails)))
                    .child(caption("次失败后,摘除"))
                    .child(div().w(px(80.0)).child(ibox(&u.fail_timeout)))
                    .child(caption("秒"));
                body = body
                    .child(field("名称", "字母、数字、- 和 _,在「反向代理」里按这个名字选", &u.name))
                    .child(labeled("负载策略", "轮询按权重轮流;最少连接优先给连接少的;IP 哈希让同一访客固定到同一成员", policy))
                    .child(labeled("成员", "权重越大分到的请求越多;备用成员只在其它成员都不可用时才用", div().flex().flex_col().gap(px(4.0)).child(srvs).child(add)))
                    .child(labeled("被动健康检查", "留空为默认(1 次失败、摘除 10 秒)", health));
            }
            Draft::Kv(f) => body = extra::dialog_body(ed.kind, f, cx),
        }
        let err = ed.err.clone();
        let cancel = crate::node_form::ctl_btn("web-sd-ed-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
            if let Some(sd) = this.sdm() {
                sd.ed = None;
            }
            cx.notify();
        }));
        let ok = div()
            .id("web-sd-ed-ok")
            .flex()
            .flex_row()
            .items_center()
            .h(px(34.0))
            .px(px(16.0))
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .bg(c::accent())
            .text_color(c::on_accent())
            .cursor_pointer()
            .hover(|s| s.opacity(0.88))
            .child(if new { "添加" } else { "确定" })
            .on_click(cx.listener(|this, _, _, cx| this.web_sd_edit_ok(cx)));
        let foot = div().px(px(24.0)).py(px(14.0)).border_t_1().border_color(c::line()).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .child(div().flex_1().min_w_0().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child(SharedString::from(err.unwrap_or_default())))
                .child(cancel)
                .child(ok),
        );
        let scroll = div().id("web-sd-ed-body").max_h(px(560.0)).overflow_y_scroll().px(px(24.0)).pb(px(20.0)).child(body);
        let m = crate::ui::modal(
            "web-sd-edit",
            DLG_W_SITE,
            cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                "escape" => {
                    if let Some(sd) = this.sdm() {
                        sd.ed = None;
                    }
                    cx.notify();
                    cx.stop_propagation();
                }
                "enter" => {
                    this.web_sd_edit_ok(cx);
                    cx.stop_propagation();
                }
                _ => {}
            }),
            [self.web_title(title, sub).into_any_element(), scroll.into_any_element(), foot.into_any_element()],
        );
        Some(m.into_any_element())
    }

    // ---------------------------------------------------------------- 域名设置

    fn sd_domains(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        let many = sd.ls.len() > 1;
        let header = div().child(head("域名").flex_1().min_w_0()).child(head("HTTP 端口").w(px(90.0))).child(head("HTTPS").w(px(70.0))).child(div().w(px(COL_ACT)));
        let mut rows = Vec::new();
        for (i, f) in sd.ls.iter().enumerate() {
            let doms = web::split_domains(&f.domains.read(cx).value());
            let text = if doms.is_empty() { "(没填域名)".to_string() } else { doms.join(" ") };
            let act = act_box("web-sd-l-act", i)
                .w(px(COL_ACT))
                .child(tip(
                    icon_btn(("web-sd-l-ed", i), IconName::Pencil, c::secondary()).on_click(cx.listener(move |this, _, window, cx| this.web_sd_edit(EdKind::Listener, Some(i), window, cx))),
                    "编辑",
                ))
                .when(many, |d| {
                    d.child(tip(
                        icon_btn(("web-sd-l-rm", i), IconName::Trash, c::danger()).on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(sd) = this.sdm() {
                                if i < sd.ls.len() {
                                    sd.ls.remove(i);
                                }
                            }
                            cx.notify();
                        })),
                        "删除这组监听",
                    ))
                });
            rows.push(
                row(("web-sd-l-row", i))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| this.web_sd_edit(EdKind::Listener, Some(i), window, cx)))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).child(SharedString::from(text)))
                    .child(div().w(px(90.0)).text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::secondary()).child(val(&f.port, cx)))
                    .child(div().w(px(70.0)).text_size(px(t::TYPE_CONTROL)).text_color(if f.ssl { c::online() } else { c::ghost() }).child(if f.ssl { "开" } else { "—" }))
                    .child(act),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section("域名设置", "每组监听 = 一组域名 + 一个端口;HTTPS 在「HTTPS」里按监听开启"))
            .child(table_flow(header, rows))
            .child(add_btn("web-sd-l-add", "添加监听").on_click(cx.listener(|this, _, window, cx| this.web_sd_edit(EdKind::Listener, None, window, cx))))
            .into_any_element()
    }

    // ---------------------------------------------------------------- 反向代理

    fn sd_proxy(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        let many = sd.locs.len() > 1;
        let header = div().child(head("路径").w(px(170.0))).child(head("后端").flex_1().min_w_0()).child(head("选项").w(px(170.0))).child(div().w(px(COL_ACT)));
        let mut rows = Vec::new();
        for (i, l) in sd.locs.iter().enumerate() {
            let path = format!("{}{}", if l.exact { "= " } else { "" }, val(&l.path, cx));
            let be = match l.group {
                Some(g) => format!("后端组 {}", sd.ups.get(g).map(|u| val(&u.name, cx)).unwrap_or_default()),
                None => {
                    let a = val(&l.addr, cx);
                    if a.contains("://") { a } else { format!("{}://{a}", if l.https { "https" } else { "http" }) }
                }
            };
            let mut opts: Vec<String> = Vec::new();
            if l.ws {
                opts.push("WebSocket".into());
            }
            if l.host {
                opts.push("透传 Host".into());
            }
            if l.strip {
                opts.push("去前缀".into());
            }
            let to = val(&l.timeout, cx);
            if !to.is_empty() {
                opts.push(format!("超时 {to}s"));
            }
            let act = act_box("web-sd-loc-act", i)
                .w(px(COL_ACT))
                .child(tip(
                    icon_btn(("web-sd-loc-ed", i), IconName::Pencil, c::secondary()).on_click(cx.listener(move |this, _, window, cx| this.web_sd_edit(EdKind::Loc, Some(i), window, cx))),
                    "编辑",
                ))
                .when(many, |d| {
                    d.child(tip(
                        icon_btn(("web-sd-loc-rm", i), IconName::Trash, c::danger()).on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(sd) = this.sdm() {
                                if i < sd.locs.len() {
                                    sd.locs.remove(i);
                                }
                            }
                            cx.notify();
                        })),
                        "删除这条路径",
                    ))
                });
            rows.push(
                row(("web-sd-loc-row", i))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| this.web_sd_edit(EdKind::Loc, Some(i), window, cx)))
                    .child(div().w(px(170.0)).flex_none().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).child(SharedString::from(path)))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::secondary()).child(SharedString::from(be)))
                    .child(div().w(px(170.0)).flex_none().truncate().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(if opts.is_empty() { "—".to_string() } else { opts.join(" · ") })))
                    .child(act),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section("反向代理", "按路径把请求转发到不同的后端;最长的路径优先匹配"))
            .child(table_flow(header, rows))
            .child(add_btn("web-sd-loc-add", "添加路径").on_click(cx.listener(|this, _, window, cx| this.web_sd_edit(EdKind::Loc, None, window, cx))))
            .into_any_element()
    }

    // ---------------------------------------------------------------- 负载均衡

    fn sd_balance(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        let header = div().child(head("名称").w(px(130.0))).child(head("策略").w(px(80.0))).child(head("成员").flex_1().min_w_0()).child(head("使用").w(px(90.0))).child(div().w(px(COL_ACT)));
        let mut rows = Vec::new();
        for (ui, u) in sd.ups.iter().enumerate() {
            let used = sd.locs.iter().filter(|l| l.group == Some(ui)).count();
            let members: Vec<String> = u.servers.iter().map(|s| val(&s.addr, cx)).filter(|a| !a.is_empty()).collect();
            let act = act_box("web-sd-up-act", ui)
                .w(px(COL_ACT))
                .child(tip(
                    icon_btn(("web-sd-up-ed", ui), IconName::Pencil, c::secondary()).on_click(cx.listener(move |this, _, window, cx| this.web_sd_edit(EdKind::Up, Some(ui), window, cx))),
                    "编辑",
                ))
                .child(tip(
                    icon_btn(("web-sd-up-rm", ui), IconName::Trash, c::danger()).on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(sd) = this.sdm() {
                            if ui < sd.ups.len() {
                                sd.ups.remove(ui);
                                // 用它的路径改回直接填地址;更靠后的组下标前移
                                for l in sd.locs.iter_mut() {
                                    l.group = match l.group {
                                        Some(g) if g == ui => None,
                                        Some(g) if g > ui => Some(g - 1),
                                        o => o,
                                    };
                                }
                            }
                        }
                        cx.notify();
                    })),
                    "删除这个后端组",
                ));
            rows.push(
                row(("web-sd-up-row", ui))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| this.web_sd_edit(EdKind::Up, Some(ui), window, cx)))
                    .child(div().w(px(130.0)).flex_none().truncate().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::MEDIUM).child(val(&u.name, cx)))
                    .child(div().w(px(80.0)).flex_none().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(u.policy.label()))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::secondary()).child(SharedString::from(members.join(", "))))
                    .child(div().w(px(90.0)).flex_none().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(if used > 0 { format!("{used} 条路径") } else { "未使用".to_string() })))
                    .child(act),
            );
        }
        let empty = sd.ups.is_empty();
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section("负载均衡", "把一组后端定义成 upstream,多条路径可以共用;在「反向代理」里让某条路径选它"))
            .children((!empty).then(|| table_flow(header, rows)))
            .children(empty.then(|| caption("还没有后端组")))
            .child(add_btn("web-sd-up-add", "添加后端组").on_click(cx.listener(|this, _, window, cx| this.web_sd_edit(EdKind::Up, None, window, cx))))
            .into_any_element()
    }

    // ---------------------------------------------------------------- 站点内容(静态 / 跳转)

    fn sd_content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        if sd.kind == 1 {
            let more = self.sd_static_more(cx);
            let Some(sd) = self.sd() else { return div().into_any_element() };
            return div()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .child(section("站点内容", "静态文件放在服务器的站点目录里,可用文件页上传"))
                .child(caption(format!("目录:/opt/otter/websites/sites/{}", sd.name)))
                .child(toggle("web-sd-spa", "单页应用", "找不到的路径回落到 index.html", sd.spa).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(sd) = this.sdm() {
                        sd.spa = !sd.spa;
                    }
                    cx.notify();
                })))
                .child(toggle("web-sd-index", "目录浏览", "没有 index.html 时列出目录内容", sd.index).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(sd) = this.sdm() {
                        sd.index = !sd.index;
                    }
                    cx.notify();
                })))
                .children(more)
                .into_any_element();
        }
        let https = sd.to_https;
        let proto = div().flex().flex_row().gap(px(4.0)).children([(false, "http"), (true, "https")].into_iter().enumerate().map(|(k, (v, text))| {
            opt_chip(("web-sd-to-sch", k), text, https == v).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(sd) = this.sdm() {
                    sd.to_https = v;
                }
                cx.notify();
            }))
        }));
        let code = sd.code;
        let codes = div().flex().flex_row().gap(px(4.0)).children([301u16, 302, 307, 308].into_iter().map(|cd| {
            opt_chip(("web-sd-code", cd as usize), cd.to_string(), code == cd).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(sd) = this.sdm() {
                    sd.code = cd;
                }
                cx.notify();
            }))
        }));
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section("跳转", "把所有请求跳转到另一个地址,保留原来的路径和参数"))
            .child(labeled("目标地址", "", div().flex().flex_row().items_center().gap(px(6.0)).child(proto).child(div().flex_1().min_w_0().child(ibox(&sd.to)))))
            .child(labeled("状态码", "301/308 永久跳转,302/307 临时跳转", codes))
            .into_any_element()
    }

    // ---------------------------------------------------------------- HTTPS(站点级)

    fn sd_https(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        let certs: Vec<CertEntry> = self.web_state().map(|s| s.certs.clone()).unwrap_or_default();
        let mut all_doms: Vec<String> = Vec::new();
        for f in sd.ls.iter().filter(|f| f.ssl) {
            for d in web::split_domains(&f.domains.read(cx).value()) {
                if !all_doms.contains(&d) {
                    all_doms.push(d);
                }
            }
        }
        let mut v = div().flex().flex_col().gap(px(14.0)).child(section("HTTPS", "这个站点的 HTTPS 设置,对站点的所有域名生效"));
        v = v.child(toggle("web-sd-https", "启用 HTTPS", "所有域名组同时提供 HTTPS,需要一张覆盖这些域名的证书;也可以在「域名设置」里按组开启", sd.https()).on_click(cx.listener(|this, _, _, cx| {
            let certs: Vec<CertEntry> = this.web_state().map(|s| s.certs.clone()).unwrap_or_default();
            let Some(sd) = this.sd() else { return };
            let mut doms: Vec<String> = Vec::new();
            for f in &sd.ls {
                doms.extend(web::split_domains(&f.domains.read(cx).value()));
            }
            let pick = certs.iter().find(|c| !doms.is_empty() && doms.iter().all(|d| c.covers(d))).map(|c| c.name.clone());
            if let Some(sd) = this.sdm() {
                let on = !sd.https();
                for f in sd.ls.iter_mut() {
                    f.ssl = on;
                }
                if on && sd.cert.is_none() {
                    sd.cert = pick;
                }
            }
            cx.notify();
        })));
        if !sd.https() {
            return v.into_any_element();
        }
        v = v.child(labeled("HTTPS 端口", "默认 443;改成别的端口时,HTTP 跳转也会带上这个端口", div().w(px(140.0)).child(ibox(&sd.https_port))));
        // 证书:能用的做成选项,不能用的说明缺哪些域名;下面显示选中证书的到期情况
        let now = now_secs();
        let mut chips = div().flex().flex_row().flex_wrap().items_center().gap(px(6.0));
        let mut skipped: Vec<String> = Vec::new();
        let mut any = false;
        for (ci, ce) in certs.iter().enumerate() {
            let missing: Vec<&String> = all_doms.iter().filter(|d| !ce.covers(d)).collect();
            if !missing.is_empty() && sd.cert.as_deref() != Some(&ce.name) {
                skipped.push(format!("{}(缺 {})", ce.name, missing.iter().take(3).map(|d| d.as_str()).collect::<Vec<_>>().join("、")));
                continue;
            }
            any = true;
            let n = ce.name.clone();
            chips = chips.child(opt_chip(("web-sd-cert", ci), ce.name.clone(), sd.cert.as_deref() == Some(&ce.name)).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(sd) = this.sdm() {
                    sd.cert = Some(n.clone());
                }
                cx.notify();
            })));
        }
        if !any {
            chips = chips.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::warn()).child("没有覆盖这些域名的证书"));
        }
        let doms_text = all_doms.join(" ");
        let apply = div().id("web-sd-cert-apply").h(px(28.0)).flex().flex_row().items_center().gap(px(6.0)).cursor_pointer().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).hover(|s| s.text_color(c::text())).child(icon(IconName::Plus, 13.0, c::secondary())).child("申请证书").on_click(cx.listener(move |this, _, window, cx| this.web_open_issue_over(&doms_text, window, cx)));
        let mut cert_col = div().flex().flex_col().gap(px(6.0)).child(chips);
        if let Some(ce) = sd.cert.as_ref().and_then(|n| certs.iter().find(|c| &c.name == n)) {
            let (txt, col) = match ce.days_left(now) {
                Some(d) => (fmt_days(d), days_color(d)),
                None => ("到期时间未知".to_string(), c::ghost()),
            };
            let src = match ce.meta.source.as_str() {
                "acme" => if ce.meta.auto { "自动续期" } else { "手动续期" },
                "self" => "自签",
                _ => "上传的证书,到期要手动更换",
            };
            cert_col = cert_col.child(div().flex().flex_row().gap(px(8.0)).text_size(px(t::TYPE_CAPTION)).child(div().text_color(col).child(SharedString::from(txt))).child(div().text_color(c::ghost()).child(format!("{} · {src}", ce.issuer))));
        }
        if !skipped.is_empty() {
            cert_col = cert_col.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(format!("不能用:{}", skipped.join(";")))));
        }
        cert_col = cert_col.child(apply);
        v = v.child(labeled("证书", "", cert_col));
        // HTTP 访问方式
        let mode = sd.http_mode;
        let mut modes = div().flex().flex_row().gap(px(4.0));
        for (k, l) in ["跳转到 HTTPS", "可直接访问", "禁止访问"].into_iter().enumerate() {
            modes = modes.child(opt_chip(("web-sd-mode", k), l, mode as usize == k).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(sd) = this.sdm() {
                    sd.http_mode = k as u8;
                }
                cx.notify();
            })));
        }
        v = v.child(labeled("HTTP 访问", "跳转:访问 http 自动 301 到 https;可直接访问:http 和 https 都能用;禁止:不监听 HTTP 端口(也就不能用 HTTP 验证申请证书)", modes));
        // HSTS
        v = v.child(toggle("web-sd-hsts", "HSTS", "告诉浏览器在有效期内只用 HTTPS 访问(只写在 HTTPS 里)", sd.hsts).on_click(cx.listener(|this, _, _, cx| {
            if let Some(sd) = this.sdm() {
                sd.hsts = !sd.hsts;
            }
            cx.notify();
        })));
        if sd.hsts {
            let age = sd.hsts_age;
            let mut ages = div().flex().flex_row().gap(px(4.0));
            for (k, (secs, l)) in [(15768000u32, "6 个月"), (31536000, "1 年"), (63072000, "2 年")].into_iter().enumerate() {
                ages = ages.child(opt_chip(("web-sd-hsts-age", k), l, age == secs).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(sd) = this.sdm() {
                        sd.hsts_age = secs;
                        if secs < 31536000 {
                            sd.hsts_pre = false;
                        }
                    }
                    cx.notify();
                })));
            }
            let (sub, pre) = (sd.hsts_sub, sd.hsts_pre);
            let opts = div()
                .flex()
                .flex_row()
                .gap(px(6.0))
                .child(opt_chip(("web-sd-hsts-opt", 0), "包含子域名", sub).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(sd) = this.sdm() {
                        sd.hsts_sub = !sd.hsts_sub;
                        if !sd.hsts_sub {
                            sd.hsts_pre = false;
                        }
                    }
                    cx.notify();
                })))
                .child(opt_chip(("web-sd-hsts-opt", 1), "preload", pre).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(sd) = this.sdm() {
                        // preload 要求包含子域名且有效期至少一年
                        sd.hsts_pre = !sd.hsts_pre;
                        if sd.hsts_pre {
                            sd.hsts_sub = true;
                            sd.hsts_age = sd.hsts_age.max(31536000);
                        }
                    }
                    cx.notify();
                })));
            v = v.child(labeled("HSTS 有效期", "", ages)).child(labeled("HSTS 选项", "", opts));
            if pre {
                v = v.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::warn()).child("preload 只是声明;提交到浏览器预加载名单后很难撤销,整个域名及子域名都必须长期支持 HTTPS"));
            }
        }
        // TLS 版本
        let mut vers = div().flex().flex_row().gap(px(4.0));
        for (k, name) in ["TLS 1.0", "TLS 1.1", "TLS 1.2", "TLS 1.3"].into_iter().enumerate() {
            vers = vers.child(opt_chip(("web-sd-tls", k), name, sd.tls[k]).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(sd) = this.sdm() {
                    sd.tls[k] = !sd.tls[k];
                }
                cx.notify();
            })));
        }
        v = v.child(labeled("TLS 协议版本", "默认只开 1.2 和 1.3;1.0 / 1.1 已不安全,只在必须兼容老客户端时才开", vers));
        // 加密套件
        let cm = sd.cipher_mode;
        let mut cs = div().flex().flex_row().gap(px(4.0));
        for (k, name) in ["默认", "现代", "兼容", "自定义"].into_iter().enumerate() {
            cs = cs.child(opt_chip(("web-sd-cipher", k), name, cm == k).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(sd) = this.sdm() {
                    sd.cipher_mode = k;
                }
                cx.notify();
            })));
        }
        let hint = match cm {
            0 => "用 nginx 默认套件;只影响 TLS 1.2 及以下,TLS 1.3 的套件由 nginx 自动选择",
            1 => "只留 ECDHE + AES-GCM / ChaCha20,更安全,老设备可能连不上;只影响 TLS 1.2 及以下",
            2 => "HIGH:!aNULL:!MD5:!RC4,兼容性更好;只影响 TLS 1.2 及以下",
            _ => "OpenSSL 套件串,用冒号分隔;只影响 TLS 1.2 及以下",
        };
        v = v.child(labeled("加密套件", hint, cs));
        if cm == 3 {
            v = v.child(area_field("套件串", "OpenSSL 格式,用冒号分隔;可以换行,保存时会去掉空白", Some(&sd.ciphers), 96.0));
        }
        // 其它
        v = v.child(toggle("web-sd-h2", "HTTP/2", "浏览器用同一个连接并行加载,默认开启", sd.h2).on_click(cx.listener(|this, _, _, cx| {
            if let Some(sd) = this.sdm() {
                sd.h2 = !sd.h2;
            }
            cx.notify();
        })));
        v = v.child(toggle("web-sd-h3", "HTTP/3 (QUIC)", "额外监听 UDP 的 HTTPS 端口,浏览器首次访问后自动升级;要在防火墙/安全组放行 UDP。OpenResty 官方镜像支持,接管的其它 nginx 不一定带 http_v3_module(保存时 nginx -t 会检查,不通过自动还原)", sd.h3).on_click(cx.listener(|this, _, _, cx| {
            if let Some(sd) = this.sdm() {
                sd.h3 = !sd.h3;
            }
            cx.notify();
        })));
        v = v.child(toggle("web-sd-ocsp", "OCSP Stapling", "服务器代客户端查询证书吊销状态,加快握手;证书里没有 OCSP 地址时(如 Let's Encrypt 已逐步取消)不会生效,nginx 只记一条警告", sd.ocsp).on_click(cx.listener(|this, _, _, cx| {
            if let Some(sd) = this.sdm() {
                sd.ocsp = !sd.ocsp;
            }
            cx.notify();
        })));
        v.into_any_element()
    }

    // ---------------------------------------------------------------- 配置原文(外部配置)

    fn sd_raw(&mut self, _cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section("配置原文", "这个站点不是 Otter 生成的,只能直接改配置;保存前会用 nginx -t 检查,不通过自动还原"))
            .child(area_field("配置文件", "", sd.raw.as_ref(), 460.0))
            .into_any_element()
    }

    // ---------------------------------------------------------------- 高级

    fn sd_advanced(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let more = self.sd_advanced_more(cx);
        let Some(sd) = self.sd() else { return div().into_any_element() };
        let ipv6 = sd.ipv6;
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section("高级", "不填就用全局默认"))
            .child(field("上传大小上限", "如 100m、2g,0 表示不限;留空沿用全局设置", &sd.max_body))
            .child(field("默认读取超时(秒)", "反向代理等后端响应的最长时间;每条路径可以单独设置,留空为 300", &sd.timeout))
            .child(toggle("web-sd-ipv6", "监听 IPv6", "每个端口同时写 listen [::]:端口", ipv6).on_click(cx.listener(|this, _, _, cx| {
                if let Some(sd) = this.sdm() {
                    sd.ipv6 = !sd.ipv6;
                }
                cx.notify();
            })))
            .children(more)
            .into_any_element()
    }
}
