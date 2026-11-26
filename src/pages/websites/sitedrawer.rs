//! 站点管理抽屉:左侧菜单(域名设置 / 反向代理 / 负载均衡 / HTTPS / 高级),右侧是当前菜单的内容。
//! 只用于编辑已有的(Otter 生成的)站点;新建仍是 `web_dlg_site` 的简单弹窗。
//! 表单状态在 `SiteDrawer`,保存时 `to_spec` 转成 `SiteSpec`,经 `web::site_save_script` 走和弹窗一样的事务。
//! 设计见 `docs/plans/active/2026-11-26-site-drawer.md`。

use super::*;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Menu {
    Domains,
    Proxy,
    Balance,
    Content,
    Https,
    Advanced,
    /// 没有 Otter 头部的外部配置:只能改原文
    Raw,
}

pub(super) struct SiteDrawer {
    file: String,
    name: String,
    enabled: bool,
    /// 打开时配置正文已被手工改过:保存会按表单重新生成,覆盖手工改动
    modified: bool,
    /// 外部配置的原文编辑框(有值时抽屉只有「配置原文」一个菜单)
    raw: Option<Entity<TextareaState>>,
    menu: Menu,
    /// 0 反向代理 1 静态 2 跳转
    kind: u8,
    ls: Vec<LForm>,
    ipv6: bool,
    hsts: bool,
    max_body: Entity<InputState>,
    timeout: Entity<InputState>,
    locs: Vec<LocForm>,
    ups: Vec<UpForm>,
    spa: bool,
    index: bool,
    to: Entity<InputState>,
    to_https: bool,
    code: u16,
}

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

impl SiteDrawer {
    fn from_spec(file: &str, enabled: bool, s: &SiteSpec, window: &mut Window, cx: &mut Context<Home>, new_listener: &dyn Fn(&str, u16, bool, Option<String>, bool, &mut Window, &mut Context<Home>) -> LForm) -> SiteDrawer {
        let ls: Vec<LForm> = s.listeners.iter().map(|l| new_listener(&l.domains.join(" "), l.port, l.ssl(), l.cert.clone(), l.force_https, window, cx)).collect();
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
            raw: None,
            menu: Menu::Domains,
            kind,
            ls,
            ipv6: s.ipv6,
            hsts: s.hsts,
            max_body: mk_in("如 100m,留空沿用全局设置", s.adv.max_body.as_deref().unwrap_or(""), window, cx),
            timeout: mk_in("默认 300", &s.adv.read_timeout.map(|t| t.to_string()).unwrap_or_default(), window, cx),
            locs,
            ups,
            spa,
            index,
            to: mk_in("example.com", &to, window, cx),
            to_https,
            code,
        }
    }

    /// 表单 → 站点模型(并检查)
    fn to_spec(&self, cx: &App) -> Result<SiteSpec, String> {
        let mut listeners = Vec::new();
        for (i, f) in self.ls.iter().enumerate() {
            let port: u16 = val(&f.port, cx).parse().map_err(|_| format!("第 {} 组监听的端口要填 1–65535 的数字", i + 1))?;
            if f.ssl && f.cert.is_none() {
                return Err(format!("第 {} 组监听选了 HTTPS,还没选证书", i + 1));
            }
            listeners.push(web::Listener { domains: web::split_domains(&f.domains.read(cx).value()), port, cert: f.cert.clone().filter(|_| f.ssl), force_https: f.force && !f.ssl });
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
        let adv = web::Advanced { max_body: Some(val(&self.max_body, cx)).filter(|v| !v.is_empty()), read_timeout: opt_num(&self.timeout, "读取超时", cx)? };
        let spec = SiteSpec { name: self.name.clone(), kind, hsts: self.hsts && self.ls.iter().any(|f| f.ssl), listeners, ipv6: self.ipv6, adv };
        spec.check()?;
        Ok(spec)
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
    } else if e.contains("上传") || e.contains("超时") {
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
        let sd = SiteDrawer::from_spec(&e.file, e.enabled, spec, window, cx, &|d, p, ssl, cert, force, w, cx| {
            let mk = |v: &str, w: &mut Window, cx: &mut Context<Home>| mk_in("", v, w, cx);
            LForm { domains: mk(d, w, cx), port: mk(&p.to_string(), w, cx), ssl, cert, force }
        });
        let mut sd = sd;
        sd.modified = e.modified;
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
        match sd.to_spec(cx).and_then(|s| web::site_save_script(&s, enabled, &tg)) {
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

    pub(super) fn web_sd_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sd = self.sd()?;
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
            Menu::Raw => self.sd_raw(cx),
        };
        let body = div().flex_1().min_h_0().flex().flex_row().child(nav).child(div().id("web-sd-body").flex_1().min_w_0().h_full().overflow_y_scroll().p(px(20.0)).flex().flex_col().gap(px(14.0)).child(content));
        let warn = self.sd().is_some_and(|sd| sd.modified).then(|| ("配置被手工改过:保存会按表单重新生成,手工改动会被覆盖".to_string(), c::warn()));
        let err = self.web_ui().err.clone().map(|e| (e, c::danger())).or(warn);
        let foot = self.web_footer(err, "保存", true, cx);
        Some(crate::ui::drawer("web-sd", 860.0, |_, _, _| {}, [head.into_any_element(), body.into_any_element(), foot.into_any_element()]).into_any_element())
    }

    // ---------------------------------------------------------------- 域名设置

    fn sd_domains(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        let many = sd.ls.len() > 1;
        let mut rows = div().flex().flex_col().gap(px(8.0));
        rows = rows.child(div().flex().flex_row().gap(px(6.0)).child(caption("域名(多个用空格分隔)").flex_1()).child(caption("端口").w(px(72.0))).child(div().w(px(if many { 30.0 } else { 0.0 }))));
        for (i, f) in sd.ls.iter().enumerate() {
            rows = rows.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().flex_1().min_w_0().child(ibox(&f.domains)))
                    .child(div().w(px(72.0)).flex_none().child(ibox(&f.port)))
                    .when(many, |d| {
                        d.child(tip(
                            icon_btn(("web-sd-l-rm", i), IconName::X, c::muted()).on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(sd) = this.sdm() {
                                    if i < sd.ls.len() {
                                        sd.ls.remove(i);
                                    }
                                }
                                cx.notify();
                            })),
                            "删除这组监听",
                        ))
                    }),
            );
        }
        let ipv6 = sd.ipv6;
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section("域名设置", "每组监听 = 一组域名 + 一个端口;HTTPS 在「HTTPS」里按监听开启"))
            .child(rows)
            .child(add_btn("web-sd-l-add", "添加监听").on_click(cx.listener(|this, _, window, cx| {
                let (hp, sp) = this.web_target().ports;
                let used: Vec<u16> = this.sd().map(|sd| sd.ls.iter().filter_map(|f| val(&f.port, cx).parse().ok()).collect()).unwrap_or_default();
                let port = [hp, sp, 8080, 8443, 8000, 3000].into_iter().find(|p| !used.contains(p)).unwrap_or(8081);
                let f = LForm { domains: mk_in("", "", window, cx), port: mk_in("", &port.to_string(), window, cx), ssl: false, cert: None, force: false };
                if let Some(sd) = this.sdm() {
                    sd.ls.push(f);
                }
                cx.notify();
            })))
            .child(toggle("web-sd-ipv6", "监听 IPv6", "每个端口同时写 listen [::]:端口", ipv6).on_click(cx.listener(|this, _, _, cx| {
                if let Some(sd) = this.sdm() {
                    sd.ipv6 = !sd.ipv6;
                }
                cx.notify();
            })))
            .into_any_element()
    }

    // ---------------------------------------------------------------- 反向代理

    fn sd_proxy(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        let groups: Vec<String> = sd.ups.iter().map(|u| val(&u.name, cx)).collect();
        let many = sd.locs.len() > 1;
        let mut list = div().flex().flex_col().gap(px(10.0));
        for (i, l) in sd.locs.iter().enumerate() {
            let mut line1 = div().flex().flex_row().items_center().gap(px(6.0)).child(caption("路径").w(px(34.0)).flex_none()).child(div().flex_1().min_w_0().child(ibox(&l.path)));
            line1 = line1.child(opt_chip(("web-sd-loc-exact", i), "精确匹配", l.exact).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(f) = this.sdm().and_then(|sd| sd.locs.get_mut(i)) {
                    f.exact = !f.exact;
                    if f.exact {
                        f.strip = false;
                    }
                }
                cx.notify();
            })));
            if many {
                line1 = line1.child(tip(
                    icon_btn(("web-sd-loc-rm", i), IconName::X, c::muted()).on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(sd) = this.sdm() {
                            if i < sd.locs.len() {
                                sd.locs.remove(i);
                            }
                        }
                        cx.notify();
                    })),
                    "删除这条路径",
                ));
            }
            // 后端:地址 / 后端组,和协议
            let mut be = div().flex().flex_row().flex_wrap().items_center().gap(px(6.0)).child(caption("后端").w(px(34.0)).flex_none());
            be = be.child(opt_chip(("web-sd-loc-be", i * 100), "地址", l.group.is_none()).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(f) = this.sdm().and_then(|sd| sd.locs.get_mut(i)) {
                    f.group = None;
                }
                cx.notify();
            })));
            for (gi, g) in groups.iter().enumerate() {
                let label = if g.is_empty() { format!("后端组 {}", gi + 1) } else { format!("后端组 {g}") };
                be = be.child(opt_chip(("web-sd-loc-be", i * 100 + gi + 1), label, l.group == Some(gi)).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(f) = this.sdm().and_then(|sd| sd.locs.get_mut(i)) {
                        f.group = Some(gi);
                    }
                    cx.notify();
                })));
            }
            let https = l.https;
            let proto = div().flex().flex_row().gap(px(4.0)).children([(false, "http"), (true, "https")].into_iter().enumerate().map(|(k, (v, text))| {
                opt_chip(("web-sd-loc-sch", i * 2 + k), text, https == v).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(f) = this.sdm().and_then(|sd| sd.locs.get_mut(i)) {
                        f.https = v;
                    }
                    cx.notify();
                }))
            }));
            let mut line_addr = div().flex().flex_row().items_center().gap(px(6.0)).child(div().w(px(34.0)).flex_none()).child(proto);
            if l.group.is_none() {
                line_addr = line_addr.child(div().flex_1().min_w_0().child(ibox(&l.addr)));
            } else {
                line_addr = line_addr.child(caption("转发给后端组里的成员(在「负载均衡」里设置)"));
            }
            let flag = |id: usize, text: &'static str, on: bool, f: fn(&mut LocForm)| {
                opt_chip(("web-sd-loc-flag", i * 10 + id), text, on).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(l) = this.sdm().and_then(|sd| sd.locs.get_mut(i)) {
                        f(l);
                    }
                    cx.notify();
                }))
            };
            let opts = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .child(flag(0, "WebSocket", l.ws, |l| l.ws = !l.ws))
                .child(flag(1, "透传 Host", l.host, |l| l.host = !l.host))
                .child(flag(2, "去掉路径前缀", l.strip, |l| {
                    if !l.exact {
                        l.strip = !l.strip
                    }
                }))
                .child(div().flex_1())
                .child(caption("读取超时"))
                .child(div().w(px(72.0)).child(ibox(&l.timeout)));
            list = list.child(sub_card().child(line1).child(be).child(line_addr).child(opts));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section("反向代理", "按路径把请求转发到不同的后端;最长的路径优先匹配"))
            .child(list)
            .child(add_btn("web-sd-loc-add", "添加路径").on_click(cx.listener(|this, _, window, cx| {
                let used: Vec<String> = this.sd().map(|sd| sd.locs.iter().map(|l| val(&l.path, cx)).collect()).unwrap_or_default();
                let path = ["/api/", "/static/", "/admin/", "/ws/"].into_iter().find(|p| !used.iter().any(|u| u == p)).unwrap_or("/new/");
                let f = new_loc(path, None, false, "", window, cx);
                if let Some(sd) = this.sdm() {
                    sd.locs.push(f);
                }
                cx.notify();
            })))
            .into_any_element()
    }

    // ---------------------------------------------------------------- 负载均衡

    fn sd_balance(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        let mut list = div().flex().flex_col().gap(px(10.0));
        if sd.ups.is_empty() {
            list = list.child(caption("还没有后端组。添加后,在「反向代理」里让某条路径选它"));
        }
        for (ui, u) in sd.ups.iter().enumerate() {
            let used = sd.locs.iter().filter(|l| l.group == Some(ui)).count();
            let mut policy = div().flex().flex_row().gap(px(4.0));
            for (k, p) in [web::Policy::RoundRobin, web::Policy::LeastConn, web::Policy::IpHash].into_iter().enumerate() {
                policy = policy.child(opt_chip(("web-sd-up-pol", ui * 3 + k), p.label(), u.policy == p).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(u) = this.sdm().and_then(|sd| sd.ups.get_mut(ui)) {
                        u.policy = p;
                    }
                    cx.notify();
                })));
            }
            let head = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(caption("名称").w(px(34.0)).flex_none())
                .child(div().w(px(180.0)).child(ibox(&u.name)))
                .child(div().flex_1())
                .children((used > 0).then(|| pill(format!("{used} 条路径在用"), c::secondary(), c::idle_soft())))
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
            let many = u.servers.len() > 1;
            let mut srvs = div().flex().flex_col().gap(px(6.0));
            srvs = srvs.child(div().flex().flex_row().gap(px(6.0)).child(caption("成员 主机:端口").flex_1()).child(caption("权重").w(px(56.0))).child(div().w(px(if many { 30.0 + 52.0 } else { 52.0 }))));
            for (si, s) in u.servers.iter().enumerate() {
                srvs = srvs.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(div().flex_1().min_w_0().child(ibox(&s.addr)))
                        .child(div().w(px(56.0)).flex_none().child(ibox(&s.weight)))
                        .child(opt_chip(("web-sd-srv-bk", ui * 100 + si), "备用", s.backup).on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(s) = this.sdm().and_then(|sd| sd.ups.get_mut(ui)).and_then(|u| u.servers.get_mut(si)) {
                                s.backup = !s.backup;
                            }
                            cx.notify();
                        })))
                        .when(many, |d| {
                            d.child(tip(
                                icon_btn(("web-sd-srv-rm", ui * 100 + si), IconName::X, c::muted()).on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(u) = this.sdm().and_then(|sd| sd.ups.get_mut(ui)) {
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
            let health = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(caption("被动健康检查"))
                .child(div().w(px(80.0)).child(ibox(&u.max_fails)))
                .child(caption("次失败后,摘除"))
                .child(div().w(px(80.0)).child(ibox(&u.fail_timeout)))
                .child(caption("秒"));
            list = list.child(
                sub_card()
                    .child(head)
                    .child(div().flex().flex_row().items_center().gap(px(6.0)).child(caption("策略").w(px(34.0)).flex_none()).child(policy))
                    .child(srvs)
                    .child(
                        div().id(("web-sd-srv-add", ui)).h(px(28.0)).flex().flex_row().items_center().gap(px(6.0)).cursor_pointer().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).hover(|s| s.text_color(c::text())).child(icon(IconName::Plus, 13.0, c::secondary())).child("添加成员").on_click(cx.listener(move |this, _, window, cx| {
                            let s = new_srv("", 1, false, window, cx);
                            if let Some(u) = this.sdm().and_then(|sd| sd.ups.get_mut(ui)) {
                                u.servers.push(s);
                            }
                            cx.notify();
                        })),
                    )
                    .child(health),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section("负载均衡", "把一组后端定义成 upstream,多条路径可以共用;权重越大分到的请求越多,备用成员只在其它成员都不可用时才用"))
            .child(list)
            .child(add_btn("web-sd-up-add", "添加后端组").on_click(cx.listener(|this, _, window, cx| {
                let n = this.sd().map(|sd| sd.ups.len()).unwrap_or(0);
                let f = new_up(&format!("group{}", n + 1), window, cx);
                if let Some(sd) = this.sdm() {
                    sd.ups.push(f);
                }
                cx.notify();
            })))
            .into_any_element()
    }

    // ---------------------------------------------------------------- 站点内容(静态 / 跳转)

    fn sd_content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        if sd.kind == 1 {
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

    // ---------------------------------------------------------------- HTTPS

    fn sd_https(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        let certs: Vec<CertEntry> = self.web_state().map(|s| s.certs.clone()).unwrap_or_default();
        let (hp, sp) = self.web_target().ports;
        let port_of = |f: &LForm| val(&f.port, cx).parse::<u16>().ok();
        let has_443_ssl = sd.ls.iter().any(|f| f.ssl && port_of(f) == Some(sp));
        let any_ssl = sd.ls.iter().any(|f| f.ssl);
        let mut list = div().flex().flex_col().gap(px(10.0));
        for (i, f) in sd.ls.iter().enumerate() {
            let doms = web::split_domains(&f.domains.read(cx).value());
            let title = format!("{}  端口 {}", if doms.is_empty() { "(没填域名)".to_string() } else { doms.join(" ") }, val(&f.port, cx));
            let mut seg = div().flex().flex_row().gap(px(4.0));
            for (k, l) in ["HTTP", "HTTPS"].into_iter().enumerate() {
                seg = seg.child(opt_chip(("web-sd-ssl", i * 2 + k), l, f.ssl == (k == 1)).on_click(cx.listener(move |this, _, window, cx| {
                    let certs: Vec<CertEntry> = this.web_state().map(|s| s.certs.clone()).unwrap_or_default();
                    let (hp, sp) = this.web_target().ports;
                    let Some(f) = this.sd().and_then(|sd| sd.ls.get(i)) else { return };
                    let doms = web::split_domains(&f.domains.read(cx).value());
                    let ssl = k == 1;
                    let (port, port_in) = (val(&f.port, cx), f.port.clone());
                    // 开 HTTPS:自动选第一张覆盖这些域名的证书;端口还是默认的 80 就顺手改成 443,关掉时反过来
                    let pick = if ssl { certs.iter().find(|c| !doms.is_empty() && doms.iter().all(|d| c.covers(d))).map(|c| c.name.clone()) } else { None };
                    if ssl && port == hp.to_string() {
                        port_in.update(cx, |s, cx| s.set_value(&sp.to_string(), window, cx));
                    } else if !ssl && port == sp.to_string() {
                        port_in.update(cx, |s, cx| s.set_value(&hp.to_string(), window, cx));
                    }
                    if let Some(m) = this.sdm().and_then(|sd| sd.ls.get_mut(i)) {
                        m.ssl = ssl;
                        m.cert = pick;
                        if ssl {
                            m.force = false;
                        }
                    }
                    cx.notify();
                })));
            }
            let mut card_ = sub_card().child(div().flex().flex_row().items_center().gap(px(10.0)).child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::MEDIUM).child(SharedString::from(title))).child(seg));
            if f.ssl {
                let mut chips = div().flex().flex_row().flex_wrap().items_center().gap(px(6.0)).child(caption("证书"));
                let mut any = false;
                for (ci, ce) in certs.iter().enumerate() {
                    let fits = !doms.is_empty() && doms.iter().all(|d| ce.covers(d));
                    if !fits && f.cert.as_deref() != Some(&ce.name) {
                        continue;
                    }
                    any = true;
                    let n = ce.name.clone();
                    chips = chips.child(opt_chip(("web-sd-cert", i * 100 + ci), ce.name.clone(), f.cert.as_deref() == Some(&ce.name)).on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(m) = this.sdm().and_then(|sd| sd.ls.get_mut(i)) {
                            m.cert = Some(n.clone());
                        }
                        cx.notify();
                    })));
                }
                if !any {
                    chips = chips.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::warn()).child("没有覆盖这些域名的证书,先到「证书」页申请或上传"));
                }
                card_ = card_.child(chips);
            } else if has_443_ssl {
                card_ = card_.child(div().flex().flex_row().child(opt_chip(("web-sd-force", i), "跳转到 HTTPS", f.force).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(m) = this.sdm().and_then(|sd| sd.ls.get_mut(i)) {
                        m.force = !m.force;
                    }
                    cx.notify();
                }))));
            }
            list = list.child(card_);
        }
        let hsts = sd.hsts;
        let _ = hp;
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section("HTTPS", "按监听开启:选 HTTPS 并选一张覆盖这些域名的证书;HTTP 监听可以整体跳转到 HTTPS"))
            .child(list)
            .children(any_ssl.then(|| toggle("web-sd-hsts", "HSTS", "告诉浏览器一年内只用 HTTPS 访问(只写在 HTTPS 监听里)", hsts).on_click(cx.listener(|this, _, _, cx| {
                if let Some(sd) = this.sdm() {
                    sd.hsts = !sd.hsts;
                }
                cx.notify();
            }))))
            .into_any_element()
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

    fn sd_advanced(&mut self, _cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section("高级", "不填就用全局默认"))
            .child(field("上传大小上限", "如 100m、2g,0 表示不限;留空沿用全局设置", &sd.max_body))
            .child(field("默认读取超时(秒)", "反向代理等后端响应的最长时间;每条路径可以单独设置,留空为 300", &sd.timeout))
            .into_any_element()
    }
}
