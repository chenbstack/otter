//! 站点抽屉里后加的高级项:压缩、访问控制、响应头、日志、错误页、真实 IP、重写规则(「高级」菜单),
//! 静态站点的首页/缓存/防盗链(「站点内容」菜单),流量限制(「流量限制」菜单)。
//! 枚举和列表直接存在 `Extra.a`(一份 `web::Advanced`),文本框是 `Entity`,保存时 `fill` 合并回 `Advanced`。

use super::*;

pub(super) struct Extra {
    pub a: web::Advanced,
    gz_level: Entity<InputState>,
    gz_types: Entity<InputState>,
    realm: Entity<InputState>,
    idx: Entity<InputState>,
    cache_exts: Entity<InputState>,
    cache_time: Entity<InputState>,
    hot_exts: Entity<InputState>,
    lim_rate: Entity<InputState>,
    lim_burst: Entity<InputState>,
    lim_conn: Entity<InputState>,
    lim_speed: Entity<InputState>,
    lim_after: Entity<InputState>,
    ip_list: Entity<TextareaState>,
    real_ip: Entity<TextareaState>,
    hot_allow: Entity<TextareaState>,
    exempt: Entity<TextareaState>,
}

/// 编辑窗口的草稿(响应头 / 错误页 / 重写规则 / 认证用户):两个输入框 + 一个选项
pub(super) struct KvForm {
    a: Entity<InputState>,
    b: Entity<InputState>,
    sel: usize,
}

fn mk_area(lines: &[String], window: &mut Window, cx: &mut Context<Home>) -> Entity<TextareaState> {
    let e = cx.new(|cx| TextareaState::new(window, cx).rows(4));
    if !lines.is_empty() {
        let text = lines.join("\n");
        e.update(cx, |s, cx| s.set_value(text.as_str(), window, cx));
    }
    cx.observe(&e, |_, _, cx| cx.notify()).detach();
    e
}

fn lines_of(e: &Entity<TextareaState>, cx: &App) -> Vec<String> {
    e.read(cx).value().split(|c: char| c.is_whitespace() || c == ',').filter(|x| !x.is_empty()).map(str::to_string).collect()
}

fn num_text<T: ToString>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

impl Extra {
    pub(super) fn new(adv: &web::Advanced, window: &mut Window, cx: &mut Context<Home>) -> Extra {
        let l = &adv.limits;
        Extra {
            a: adv.clone(),
            gz_level: mk_in("默认 6", &num_text(adv.gzip_level), window, cx),
            gz_types: mk_in("留空用常见文本类型", adv.gzip_types.as_deref().unwrap_or(""), window, cx),
            realm: mk_in("如 Private area", adv.auth.as_ref().map(|a| a.realm.as_str()).unwrap_or("Restricted"), window, cx),
            idx: mk_in("index.html index.htm", adv.index_files.as_deref().unwrap_or(""), window, cx),
            cache_exts: mk_in("js css png jpg", adv.cache_exts.as_deref().unwrap_or(""), window, cx),
            cache_time: mk_in("30d", adv.cache_time.as_deref().unwrap_or(""), window, cx),
            hot_exts: mk_in("jpg png gif webp", adv.hotlink_exts.as_deref().unwrap_or(""), window, cx),
            lim_rate: mk_in("不限制", &num_text(l.req_rate), window, cx),
            lim_burst: mk_in("0", &if l.req_rate.is_some() { l.burst.to_string() } else { String::new() }, window, cx),
            lim_conn: mk_in("不限制", &num_text(l.conn), window, cx),
            lim_speed: mk_in("不限速,如 512k", l.speed.as_deref().unwrap_or(""), window, cx),
            lim_after: mk_in("如 1m", l.speed_after.as_deref().unwrap_or(""), window, cx),
            ip_list: mk_area(&adv.ip_list, window, cx),
            real_ip: mk_area(&adv.real_ip, window, cx),
            hot_allow: mk_area(&adv.hotlink_allow, window, cx),
            exempt: mk_area(&l.exempt, window, cx),
        }
    }

    /// 把输入框里的值合并进 `adv`(其余字段已经在 `self.a`)
    pub(super) fn fill(&self, base: web::Advanced, is_static: bool, cx: &App) -> Result<web::Advanced, String> {
        let mut a = web::Advanced { gzip: self.a.gzip, ip_mode: self.a.ip_mode, auth: self.a.auth.clone(), headers: self.a.headers.clone(), log_off: self.a.log_off, error_level: self.a.error_level.clone(), err_pages: self.a.err_pages.clone(), real_ip_header: self.a.real_ip_header.clone(), rewrites: self.a.rewrites.clone(), hotlink_none: self.a.hotlink_none, ..base };
        let some = |e: &Entity<InputState>| Some(val(e, cx)).filter(|v| !v.is_empty());
        a.gzip_level = opt_num(&self.gz_level, "压缩级别", cx)?.map(|v| v.min(255) as u8);
        a.gzip_types = some(&self.gz_types);
        a.ip_list = if a.ip_mode != 0 { lines_of(&self.ip_list, cx) } else { Vec::new() };
        if let Some(au) = &mut a.auth {
            au.realm = val(&self.realm, cx);
        }
        a.real_ip = lines_of(&self.real_ip, cx);
        if a.real_ip.is_empty() {
            a.real_ip_header = None;
        }
        if is_static {
            a.index_files = some(&self.idx);
            a.cache_exts = some(&self.cache_exts);
            a.cache_time = some(&self.cache_time);
            a.hotlink_exts = some(&self.hot_exts);
            a.hotlink_allow = if a.hotlink_exts.is_some() { lines_of(&self.hot_allow, cx) } else { Vec::new() };
            a.hotlink_none = a.hotlink_exts.is_some() && a.hotlink_none;
        } else {
            a.hotlink_none = false;
        }
        let l = &self.a.limits;
        a.limits = web::Limits {
            req_rate: opt_num(&self.lim_rate, "请求速率", cx)?,
            req_min: l.req_min,
            burst: opt_num(&self.lim_burst, "突发", cx)?.unwrap_or(0),
            nodelay: l.nodelay,
            conn: opt_num(&self.lim_conn, "并发连接数", cx)?,
            speed: some(&self.lim_speed),
            speed_after: some(&self.lim_speed).and(some(&self.lim_after)),
            exempt: lines_of(&self.exempt, cx),
        };
        if a.limits.req_rate.is_none() {
            a.limits.burst = 0;
            a.limits.nodelay = false;
        }
        Ok(a)
    }
}

pub(super) fn dialog_title(kind: EdKind, new: bool) -> (&'static str, &'static str) {
    match kind {
        EdKind::Header => (if new { "添加响应头" } else { "编辑响应头" }, "给每个响应加上这个头(always:错误响应也带)"),
        EdKind::ErrPage => (if new { "添加错误页" } else { "编辑错误页" }, "出现这个状态码时,改为显示另一个页面"),
        EdKind::Rewrite => (if new { "添加重写规则" } else { "编辑重写规则" }, "按正则改写请求路径,在匹配 location 之前执行"),
        _ => (if new { "添加认证用户" } else { "编辑认证用户" }, "访问站点要输入用户名和密码"),
    }
}

/// 打开编辑窗口前造草稿
pub(super) fn open_draft(sd: &SiteDrawer, kind: EdKind, slot: Option<usize>, window: &mut Window, cx: &mut Context<Home>) -> Option<Draft> {
    let a = &sd.x.a;
    let (x, y, sel): (String, String, usize) = match (kind, slot) {
        (EdKind::Header, Some(i)) => a.headers.get(i).map(|(n, v)| (n.clone(), v.clone(), 0))?,
        (EdKind::Header, None) => (String::new(), String::new(), 0),
        (EdKind::ErrPage, Some(i)) => a.err_pages.get(i).map(|(c, t)| (c.to_string(), t.clone(), 0))?,
        (EdKind::ErrPage, None) => ("404".into(), "/404.html".into(), 0),
        (EdKind::Rewrite, Some(i)) => a.rewrites.get(i).map(|r| (r.regex.clone(), r.to.clone(), web::REWRITE_FLAGS.iter().position(|(f, _)| *f == r.flag).unwrap_or(0)))?,
        (EdKind::Rewrite, None) => (String::new(), String::new(), 3),
        (EdKind::User, Some(i)) => a.auth.as_ref()?.users.get(i).map(|u| (u.name.clone(), String::new(), 0))?,
        (EdKind::User, None) => (String::new(), String::new(), 0),
        _ => return None,
    };
    let (pa, pb) = match kind {
        EdKind::Header => ("如 X-Frame-Options", "如 SAMEORIGIN"),
        EdKind::ErrPage => ("状态码,如 404", "/404.html 或 https://…"),
        EdKind::Rewrite => ("如 ^/old/(.*)$", "如 /new/$1"),
        _ => ("用户名", if slot.is_some() { "留空表示不改密码" } else { "至少 6 位" }),
    };
    Some(Draft::Kv(KvForm { a: mk_in(pa, &x, window, cx), b: mk_in(pb, &y, window, cx), sel }))
}

/// 窗口里的表单
pub(super) fn dialog_body(kind: EdKind, f: &KvForm, cx: &mut Context<Home>) -> Div {
    let mut body = div().flex().flex_col().gap(px(14.0));
    match kind {
        EdKind::Header => body = body.child(field("名称", "字母、数字和 -", &f.a)).child(field("值", "不能含引号和 $", &f.b)),
        EdKind::ErrPage => body = body.child(field("状态码", "400–599", &f.a)).child(field("页面", "站内路径(以 / 开头,站点能访问到)或完整 URL(302 跳转)", &f.b)),
        EdKind::Rewrite => {
            let sel = f.sel;
            let chips = div().flex().flex_row().flex_wrap().gap(px(4.0)).children(web::REWRITE_FLAGS.iter().enumerate().map(|(k, (_, label))| {
                opt_chip(("web-sd-ed-rwf", k), *label, sel == k).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(f) = this.ed_kv() {
                        f.sel = k;
                    }
                    cx.notify();
                }))
            }));
            body = body.child(field("匹配(正则)", "对请求路径匹配", &f.a)).child(field("改写成", "可用 $1 引用括号里匹配到的内容", &f.b)).child(labeled("处理方式", "继续匹配:改写后重新找 location;跳转:直接 301/302 让浏览器去新地址", chips));
        }
        _ => body = body.child(field("用户名", "字母、数字和 . _ @ -", &f.a)).child(field("密码", "", &f.b)),
    }
    body
}

/// 检查草稿;通过就写回列表
pub(super) fn commit(sd: &mut SiteDrawer, kind: EdKind, slot: Option<usize>, f: &KvForm, cx: &App) -> Result<(), String> {
    let (x, y) = (val(&f.a, cx), val(&f.b, cx));
    let a = &mut sd.x.a;
    fn put<T>(v: &mut Vec<T>, slot: Option<usize>, item: T) {
        match slot {
            Some(i) if i < v.len() => v[i] = item,
            _ => v.push(item),
        }
    }
    match kind {
        EdKind::Header => {
            if x.is_empty() || y.is_empty() {
                return Err("名称和值都不能为空".into());
            }
            put(&mut a.headers, slot, (x, y));
        }
        EdKind::ErrPage => {
            let c: u16 = x.parse().map_err(|_| "状态码要填 400–599 的数字".to_string())?;
            if y.is_empty() {
                return Err("页面不能为空".into());
            }
            if a.err_pages.iter().enumerate().any(|(i, (k, _))| *k == c && Some(i) != slot) {
                return Err(format!("状态码 {c} 已经有错误页了"));
            }
            put(&mut a.err_pages, slot, (c, y));
        }
        EdKind::Rewrite => {
            if x.is_empty() || y.is_empty() {
                return Err("匹配和改写目标都不能为空".into());
            }
            put(&mut a.rewrites, slot, web::Rewrite { regex: x, to: y, flag: web::REWRITE_FLAGS[f.sel.min(3)].0.to_string() });
        }
        _ => {
            if !web::valid_user(&x) {
                return Err("用户名只能用字母、数字和 . _ @ -".into());
            }
            let au = a.auth.get_or_insert_with(|| web::BasicAuth { realm: "Restricted".into(), users: Vec::new() });
            if au.users.iter().enumerate().any(|(i, u)| u.name == x && Some(i) != slot) {
                return Err(format!("用户 {x} 已经存在"));
            }
            let old = slot.and_then(|i| au.users.get(i)).and_then(|u| u.pass.clone());
            if slot.is_none() && y.len() < 6 {
                return Err("新用户的密码至少 6 位".into());
            }
            if !y.is_empty() && y.len() < 6 {
                return Err("密码至少 6 位".into());
            }
            put(&mut au.users, slot, web::AuthUser { name: x, pass: if y.is_empty() { old } else { Some(y) } });
        }
    }
    Ok(())
}

fn block() -> Div {
    div().flex_shrink_0().flex().flex_col().gap(px(12.0)).pt(px(16.0)).border_t_1().border_color(c::line())
}

fn st(s: impl Into<SharedString>) -> Div {
    div().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).child(s.into())
}

impl Home {
    pub(super) fn ed_kv(&mut self) -> Option<&mut KvForm> {
        match &mut self.sdm()?.ed.as_mut()?.draft {
            Draft::Kv(f) => Some(f),
            _ => None,
        }
    }

    fn xm(&mut self) -> Option<&mut web::Advanced> {
        self.sdm().map(|sd| &mut sd.x.a)
    }

    /// 一排单选 chip:`set` 写回 `Extra.a`
    fn x_chips(&self, id: &'static str, opts: &[&str], cur: usize, set: fn(&mut web::Advanced, usize), cx: &mut Context<Self>) -> Div {
        div().flex().flex_row().flex_wrap().gap(px(4.0)).children(opts.iter().enumerate().map(|(k, label)| {
            opt_chip((id, k), label.to_string(), cur == k).on_click(cx.listener(move |this, _, _, cx| {
                if let Some(a) = this.xm() {
                    set(a, k);
                }
                cx.notify();
            }))
        }))
    }

    /// 一个简单的列表表格:每行 `[列1, 列2, 列3]`,点击行编辑,右侧编辑/删除
    #[allow(clippy::too_many_arguments)]
    fn x_table(&self, id: &'static str, cols: [(&'static str, Option<f32>); 3], rows: Vec<[String; 3]>, kind: EdKind, add: &'static str, remove: fn(&mut web::Advanced, usize), cx: &mut Context<Self>) -> Div {
        let colw = |d: Div, w: Option<f32>| match w {
            Some(w) => d.w(px(w)).flex_none(),
            None => d.flex_1(),
        };
        let header = div().child(colw(head(cols[0].0), cols[0].1)).child(colw(head(cols[1].0), cols[1].1)).child(colw(head(cols[2].0), cols[2].1)).child(div().w(px(COL_ACT)));
        let mut out = Vec::new();
        for (i, r) in rows.into_iter().enumerate() {
            let act = act_box(id, i)
                .w(px(COL_ACT))
                .child(tip(icon_btn((id, i * 2), IconName::Pencil, c::secondary()).on_click(cx.listener(move |this, _, window, cx| this.web_sd_edit(kind, Some(i), window, cx))), "编辑"))
                .child(tip(
                    icon_btn((id, i * 2 + 1), IconName::Trash, c::danger()).on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(a) = this.xm() {
                            remove(a, i);
                        }
                        cx.notify();
                    })),
                    "删除",
                ));
            let mut rw = row((id, 1000 + i)).cursor_pointer().on_click(cx.listener(move |this, _, window, cx| this.web_sd_edit(kind, Some(i), window, cx)));
            for (k, v) in r.into_iter().enumerate() {
                rw = rw.child(colw(st(v).font_family(MONO), cols[k].1));
            }
            out.push(rw.child(act));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(table_flow(header, out))
            .child(add_btn(id, add).on_click(cx.listener(move |this, _, window, cx| this.web_sd_edit(kind, None, window, cx))))
    }

    fn x_toggle(&self, id: &'static str, label: &'static str, hint: &'static str, on: bool, set: fn(&mut web::Advanced, bool), cx: &mut Context<Self>) -> Stateful<Div> {
        toggle(id, label, hint, on).on_click(cx.listener(move |this, _, _, cx| {
            if let Some(a) = this.xm() {
                set(a, !on);
            }
            cx.notify();
        }))
    }

    // ---------------------------------------------------------------- 高级(扩充)

    pub(super) fn sd_advanced_more(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(sd) = self.sd() else { return Vec::new() };
        let x = &sd.x;
        let a = x.a.clone();
        let mut out: Vec<AnyElement> = Vec::new();

        // 压缩
        let gz = match a.gzip {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        let mut b = block().child(section("压缩", "gzip 压缩文本类响应,省带宽;图片、视频本来就是压缩过的,不用开")).child(self.x_chips("web-sd-gz", &["沿用全局", "开启", "关闭"], gz, |a, k| a.gzip = [None, Some(true), Some(false)][k], cx));
        if a.gzip == Some(true) {
            b = b.child(field("压缩级别", "1–9,越大越省带宽越耗 CPU;留空为 6", &x.gz_level)).child(field("压缩的类型", "MIME 类型,空格分隔;留空用常见的文本、JSON、JS、CSS、XML、SVG", &x.gz_types));
        }
        out.push(b.into_any_element());

        // 访问控制
        let mut b = block().child(section("访问控制", "限制谁能访问这个站点;验证证书用的 /.well-known/acme-challenge/ 目录始终放行"));
        b = b.child(labeled("IP 限制", "只对本站生效;支持 IPv4/IPv6 和 CIDR(如 10.0.0.0/8)", self.x_chips("web-sd-ipm", &["不限制", "只允许下列 IP", "拒绝下列 IP"], a.ip_mode as usize, |a, k| a.ip_mode = k as u8, cx)));
        if a.ip_mode != 0 {
            b = b.child(area_field("IP 列表", "每行一个,也可用空格或逗号分隔", Some(&x.ip_list), 96.0));
        }
        let auth_on = a.auth.is_some();
        b = b.child(toggle("web-sd-auth", "基本认证", "访问时弹出用户名密码框;密码只在服务器上保存哈希", auth_on).on_click(cx.listener(move |this, _, _, cx| {
            if let Some(a) = this.xm() {
                a.auth = if auth_on { None } else { Some(web::BasicAuth { realm: "Restricted".into(), users: Vec::new() }) };
            }
            cx.notify();
        })));
        if let Some(au) = &a.auth {
            let rows = au.users.iter().map(|u| [u.name.clone(), if u.pass.is_some() { "将设置新密码".to_string() } else { "沿用服务器上的密码".to_string() }, String::new()]).collect();
            b = b
                .child(field("提示语", "浏览器弹窗里显示的文字", &x.realm))
                .child(self.x_table("web-sd-au", [("用户名", Some(180.0)), ("密码", None), ("", Some(1.0))], rows, EdKind::User, "添加用户", |a, i| {
                    if let Some(au) = &mut a.auth {
                        if i < au.users.len() {
                            au.users.remove(i);
                        }
                    }
                }, cx));
        }
        out.push(b.into_any_element());

        // 响应头
        let rows = a.headers.iter().map(|(n, v)| [n.clone(), v.clone(), String::new()]).collect();
        let b = block()
            .child(section("响应头", "给每个响应加上自定义头,如安全头(X-Frame-Options、CSP)或 CORS(Access-Control-Allow-Origin)"))
            .child(self.x_table("web-sd-hd", [("名称", Some(200.0)), ("值", None), ("", Some(1.0))], rows, EdKind::Header, "添加响应头", |a, i| {
                if i < a.headers.len() {
                    a.headers.remove(i);
                }
            }, cx));
        out.push(b.into_any_element());

        // 日志
        let lv = ["warn", "info", "error", "crit"];
        let cur = a.error_level.as_deref().and_then(|l| lv.iter().position(|x| *x == l)).unwrap_or(0);
        let b = block()
            .child(section("日志", "访问日志在 /opt/otter/websites/logs;关掉能省磁盘,但就没法排查访问问题了"))
            .child(self.x_toggle("web-sd-logoff", "关闭访问日志", "access_log off", a.log_off, |a, v| a.log_off = v, cx))
            .child(labeled("错误日志级别", "warn 为默认;info 更详细,error/crit 只记严重问题", self.x_chips("web-sd-lv", &["warn(默认)", "info", "error", "crit"], cur, |a, k| a.error_level = (k != 0).then(|| ["warn", "info", "error", "crit"][k].to_string()), cx)));
        out.push(b.into_any_element());

        // 错误页
        let rows = a.err_pages.iter().map(|(c, t)| [c.to_string(), t.clone(), String::new()]).collect();
        let b = block().child(section("错误页", "状态码出现时显示自定义页面;站内路径要能被这个站点访问到(静态站放个文件,代理站要让后端提供)")).child(self.x_table("web-sd-ep", [("状态码", Some(100.0)), ("页面", None), ("", Some(1.0))], rows, EdKind::ErrPage, "添加错误页", |a, i| {
            if i < a.err_pages.len() {
                a.err_pages.remove(i);
            }
        }, cx));
        out.push(b.into_any_element());

        // 真实 IP
        let hs = web::REAL_IP_HEADERS;
        let hcur = a.real_ip_header.as_deref().and_then(|h| hs.iter().position(|x| *x == h)).unwrap_or(0);
        let mut b = block().child(section("客户端真实 IP", "站点前面还有 CDN 或另一层代理时,日志、IP 限制、流量限制看到的都是代理的地址;在这里填可信代理,改用请求头里的真实 IP")).child(area_field("可信代理", "每行一个 IP 或 CIDR;留空表示不启用", Some(&x.real_ip), 80.0));
        if !lines_of(&x.real_ip, cx).is_empty() {
            b = b.child(labeled("取 IP 的请求头", "Cloudflare 用 CF-Connecting-IP,多数代理用 X-Forwarded-For", self.x_chips("web-sd-rih", &hs, hcur, |a, k| a.real_ip_header = Some(web::REAL_IP_HEADERS[k].to_string()), cx)));
        }
        out.push(b.into_any_element());

        // 重写
        let rows = a.rewrites.iter().map(|r| [r.regex.clone(), r.to.clone(), web::REWRITE_FLAGS.iter().find(|(f, _)| *f == r.flag).map(|(_, l)| l.to_string()).unwrap_or_default()]).collect();
        let b = block().child(section("重写规则", "简单的路径改写或跳转;更复杂的写法用「配置原文」")).child(self.x_table("web-sd-rw", [("匹配", None), ("改写成", None), ("方式", Some(90.0))], rows, EdKind::Rewrite, "添加重写规则", |a, i| {
            if i < a.rewrites.len() {
                a.rewrites.remove(i);
            }
        }, cx));
        out.push(b.into_any_element());
        out
    }

    // ---------------------------------------------------------------- 站点内容(静态站点补充)

    pub(super) fn sd_static_more(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(sd) = self.sd() else { return Vec::new() };
        let x = &sd.x;
        let hot_on = !val(&x.hot_exts, cx).is_empty();
        let none = x.a.hotlink_none;
        let mut hot = block().child(section("防盗链", "只允许本站和下列来源引用这些文件;其它站点引用会得到 403")).child(field("保护的扩展名", "空格分隔,不带点;留空表示不启用", &x.hot_exts));
        if hot_on {
            hot = hot
                .child(area_field("允许的来源域名", "每行一个,可用 *.example.com;本站自己始终允许", Some(&x.hot_allow), 80.0))
                .child(self.x_toggle("web-sd-hnone", "允许没有 Referer 的请求", "直接在地址栏打开、部分 App 和下载工具不带 Referer", none, |a, v| a.hotlink_none = v, cx));
        }
        vec![
            block().child(section("默认首页", "访问目录时依次尝试的文件")).child(field("首页文件", "空格分隔;留空为 index.html index.htm", &x.idx)).into_any_element(),
            block().child(section("静态资源缓存", "让浏览器缓存这些文件(Expires/Cache-Control);更新文件后要换文件名或等缓存过期")).child(field("扩展名", "空格分隔,不带点,如 js css png;留空表示不缓存", &x.cache_exts)).child(field("缓存时长", "如 30d、12h、1y,或 max;和扩展名一起填", &x.cache_time)).into_any_element(),
            hot.into_any_element(),
        ]
    }

    // ---------------------------------------------------------------- 流量限制

    /// 预设:点一下把下面的输入填好(不动「不受限制的 IP」),之后还能手改
    fn sd_limit_presets(&self, cx: &mut Context<Self>) -> AnyElement {
        // (名称, 说明, 每秒请求数, 突发, 不延迟, 并发连接, 限速, 前面不限速)
        type P = (&'static str, &'static str, &'static str, &'static str, bool, &'static str, &'static str, &'static str);
        const PRESETS: [P; 5] = [
            ("不限制", "清空下面所有限制", "", "", false, "", "", ""),
            ("宽松", "普通网站:每秒 20 个请求,突发 40,并发 50", "20", "40", true, "50", "", ""),
            ("标准", "防一般刷接口:每秒 10 个请求,突发 20,并发 20", "10", "20", true, "20", "", ""),
            ("严格", "登录、验证码等敏感站:每秒 2 个请求,突发 5,并发 10", "2", "5", false, "10", "", ""),
            ("下载站", "不限请求数:并发 5,每连接 1m/s(前 5m 不限速)", "", "", false, "5", "1m", "5m"),
        ];
        let mut row = div().flex().flex_row().flex_wrap().gap(px(4.0));
        for (k, p) in PRESETS.iter().enumerate() {
            let (name, tip_text, rate, burst, nodelay, conn, speed, after) = *p;
            row = row.child(crate::pages::syspages::tip(
                opt_chip(("web-sd-lpre", k), name, false).on_click(cx.listener(move |this, _, window, cx| {
                    let Some(sd) = this.sdm() else { return };
                    let mut set = |e: &Entity<InputState>, v: &str, cx: &mut Context<Home>| e.update(cx, |s, cx| s.set_value(v, window, cx));
                    set(&sd.x.lim_rate, rate, cx);
                    set(&sd.x.lim_burst, burst, cx);
                    set(&sd.x.lim_conn, conn, cx);
                    set(&sd.x.lim_speed, speed, cx);
                    set(&sd.x.lim_after, after, cx);
                    sd.x.a.limits.req_min = false;
                    sd.x.a.limits.nodelay = nodelay;
                    cx.notify();
                })),
                tip_text,
            ));
        }
        labeled("预设", "点一下填好下面的值,之后可以再改;不影响「不受限制的 IP」", row).into_any_element()
    }

    pub(super) fn sd_limits(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sd) = self.sd() else { return div().into_any_element() };
        let x = &sd.x;
        let l = x.a.limits.clone();
        let rate_on = !val(&x.lim_rate, cx).is_empty();
        let mut rate = block().child(section("请求速率", "限制每个客户端 IP 每秒/每分钟的请求数,超出的返回 429")).child(field("允许的请求数", "留空表示不限制", &x.lim_rate));
        if rate_on {
            rate = rate
                .child(labeled("时间单位", "", self.x_chips("web-sd-rmin", &["每秒", "每分钟"], l.req_min as usize, |a, k| a.limits.req_min = k == 1, cx)))
                .child(field("突发", "超出速率后还允许排队的请求数;留空为 0(超了立刻拒绝)", &x.lim_burst))
                .child(self.x_toggle("web-sd-nodelay", "突发请求不延迟", "不勾选时,突发的请求会被放慢到限定速率;勾选后立即处理,只是用完额度后才拒绝", l.nodelay, |a, v| a.limits.nodelay = v, cx));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(section("流量限制", "防刷、防爬、保护后端;只对本站生效,按客户端 IP 统计"))
            .child(self.sd_limit_presets(cx))
            .child(rate)
            .child(block().child(section("并发连接", "限制每个客户端 IP 同时打开的连接数,超出的返回 429")).child(field("最大并发连接数", "留空表示不限制", &x.lim_conn)))
            .child(block().child(section("下载限速", "限制每个连接的传输速度")).child(field("限速", "如 512k、2m(每秒);留空表示不限速", &x.lim_speed)).child(field("前面不限速的流量", "传完这么多之后才开始限速,如 1m;留空从头限速", &x.lim_after)))
            .child(block().child(section("不受限制的 IP", "办公室、监控、健康检查等;对请求速率和并发连接都不生效")).child(area_field("IP 列表", "每行一个 IP 或 CIDR", Some(&x.exempt), 96.0)))
            .into_any_element()
    }
}
