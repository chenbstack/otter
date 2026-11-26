//! 站点配置正文 → `SiteSpec`:配置文件是事实源,手工改过的、Otter 支持的指令在表单里照实显示。
//! 解析出的 spec 再 `render_site` 一遍,原文里有而渲染结果里没有的叶子指令就是「Otter 不管理的配置」。
//! 认不出的结构(如 `location ~ …`)返回 None,调用方回退到头部里的数据。
//! 设计见 `docs/plans/active/2026-11-26-config-as-truth.md`。

use super::*;

pub struct Parsed {
    pub spec: SiteSpec,
    /// Otter 不管理的指令(带所在块路径),保存重新生成后会丢失
    pub unknown: Vec<String>,
    /// 非 Otter 的注释行数(重新生成后会丢失)
    pub comments: usize,
}

fn name_of<'a>(n: &'a NgxNode) -> &'a str {
    n.words.first().map(String::as_str).unwrap_or("")
}

fn args(n: &NgxNode) -> &[String] {
    n.words.get(1..).unwrap_or(&[])
}

fn kids(n: &NgxNode) -> &[NgxNode] {
    n.children.as_deref().unwrap_or(&[])
}

fn all<'a>(ns: &'a [NgxNode], name: &str) -> Vec<&'a NgxNode> {
    ns.iter().filter(|n| name_of(n) == name).collect()
}

fn one<'a>(ns: &'a [NgxNode], name: &str) -> Option<&'a NgxNode> {
    ns.iter().find(|n| name_of(n) == name)
}

fn arg0<'a>(ns: &'a [NgxNode], name: &str) -> Option<&'a str> {
    one(ns, name).and_then(|n| args(n).first()).map(String::as_str)
}

struct Srv<'a> {
    node: &'a NgxNode,
    port: u16,
    ssl: bool,
    v6: bool,
    quic: bool,
}

/// 一个 server 块的监听:只认 `PORT [ssl]`、`[::]:PORT [ssl]`、`PORT quic`、`[::]:PORT quic`;多个不同端口、绑定地址等返回 None
fn server_listen(n: &NgxNode) -> Option<Srv<'_>> {
    let (mut port, mut ssl, mut v6, mut quic) = (None, false, false, false);
    for l in all(kids(n), "listen") {
        let a = args(l);
        let addr = a.first()?;
        let (is6, p) = match addr.strip_prefix("[::]:") {
            Some(p) => (true, p),
            None => (false, addr.as_str()),
        };
        let p: u16 = p.parse().ok()?;
        v6 |= is6;
        let flags = &a[1..];
        if flags.iter().any(|f| f == "quic") {
            quic = true;
        } else {
            if port.is_some_and(|x| x != p) {
                return None;
            }
            port = Some(p);
            ssl |= flags.iter().any(|f| f == "ssl");
        }
    }
    Some(Srv { node: n, port: port?, ssl, v6, quic })
}

fn cert_name(path: &str) -> Option<String> {
    let rest = path.strip_prefix(CERT_MOUNT)?.strip_prefix('/')?;
    let name = rest.strip_suffix("/fullchain.pem")?;
    valid_name(name).then(|| name.to_string())
}

/// 这个 server 是不是只做「跳转到 HTTPS」
fn is_force_https(n: &NgxNode) -> bool {
    all(kids(n), "location").iter().any(|l| args(l) == ["/"] && kids(l).iter().any(|r| name_of(r) == "return" && args(r).first().map(String::as_str) == Some("301") && args(r).get(1).is_some_and(|u| u.starts_with("https://$host"))))
        && all(kids(n), "location").iter().all(|l| args(l).first().map(String::as_str) == Some("^~") || args(l) == ["/"])
}

fn is_acme(l: &NgxNode) -> bool {
    args(l) == ["^~", "/.well-known/acme-challenge/"]
}

fn secs(v: &str) -> Option<u32> {
    v.strip_suffix('s').unwrap_or(v).parse().ok()
}

fn parse_location(site: &str, l: &NgxNode, ups: &[Upstream]) -> Option<(Location, Option<u32>)> {
    let (exact, path) = match args(l) {
        [p] => (false, p.clone()),
        [m, p] if m == "=" => (true, p.clone()),
        _ => return None,
    };
    let c = kids(l);
    let pass = arg0(c, "proxy_pass")?;
    let (scheme, rest) = pass.split_once("://")?;
    let https = scheme == "https";
    let (hostport, uri) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let strip = !uri.is_empty();
    let backend = match ups.iter().find(|u| upstream_name(site, &u.name) == hostport) {
        Some(u) => Backend::Group { https, name: u.name.clone() },
        None => Backend::Url(format!("{scheme}://{hostport}")),
    };
    let host = c.iter().any(|n| name_of(n) == "proxy_set_header" && args(n).first().map(String::as_str) == Some("Host") && args(n).get(1).map(String::as_str) == Some("$host"));
    let ws = c.iter().any(|n| name_of(n) == "proxy_set_header" && args(n).first().map(String::as_str) == Some("Upgrade"));
    let timeout = arg0(c, "proxy_read_timeout").and_then(secs);
    Some((Location { path, exact, backend, ws, host, timeout: None, strip }, timeout))
}

fn parse_upstreams(site: &str, top: &[NgxNode]) -> Vec<Upstream> {
    let pre = format!("{site}_");
    let mut out = Vec::new();
    for u in all(top, "upstream") {
        let Some(full) = args(u).first() else { continue };
        let Some(name) = full.strip_prefix(&pre) else { continue };
        let c = kids(u);
        let policy = if one(c, "least_conn").is_some() {
            Policy::LeastConn
        } else if one(c, "ip_hash").is_some() {
            Policy::IpHash
        } else {
            Policy::RoundRobin
        };
        let (mut max_fails, mut fail_timeout) = (None, None);
        let mut servers = Vec::new();
        for s in all(c, "server") {
            let a = args(s);
            let Some(addr) = a.first() else { continue };
            let (mut weight, mut backup) = (1u32, false);
            for f in &a[1..] {
                if let Some(v) = f.strip_prefix("weight=") {
                    weight = v.parse().unwrap_or(1);
                } else if let Some(v) = f.strip_prefix("max_fails=") {
                    max_fails = v.parse().ok();
                } else if let Some(v) = f.strip_prefix("fail_timeout=") {
                    fail_timeout = secs(v);
                } else if f == "backup" {
                    backup = true;
                }
            }
            servers.push(UServer { addr: addr.clone(), weight, backup });
        }
        out.push(Upstream { name: name.to_string(), policy, servers, max_fails, fail_timeout });
    }
    out
}

fn parse_rate(v: &str) -> Option<(u32, bool)> {
    let (n, unit) = v.strip_prefix("rate=").unwrap_or(v).split_once("r/")?;
    Some((n.parse().ok()?, unit == "m"))
}

/// 解析站点正文。`hint` 是头部里读出的 spec,只用来补正文里没有的信息(认证用户名)
pub fn parse_conf(text: &str, site: &str, hint: Option<&SiteSpec>) -> Option<Parsed> {
    let toks = ngx_tokens(text);
    let mut i = 0;
    let top = ngx_parse(&toks, &mut i);
    let servers: Vec<Srv> = all(&top, "server").into_iter().map(server_listen).collect::<Option<Vec<_>>>()?;
    if servers.is_empty() {
        return None;
    }
    let upstreams = parse_upstreams(site, &top);

    // 监听
    let mut listeners = Vec::new();
    for s in &servers {
        let c = kids(s.node);
        let domains: Vec<String> = one(c, "server_name")?.words[1..].to_vec();
        let cert = if s.ssl { Some(cert_name(arg0(c, "ssl_certificate")?)?) } else { None };
        let force_https = !s.ssl && is_force_https(s.node);
        listeners.push(Listener { domains, port: s.port, cert, force_https });
    }
    let ipv6 = servers.iter().any(|s| s.v6);
    let http3 = servers.iter().any(|s| s.quic);
    let primary = servers.iter().find(|s| !is_force_https(s.node) || s.ssl)?;
    let ssl_srv = servers.iter().find(|s| s.ssl);
    let pc = kids(primary.node);

    let mut adv = Advanced { http3, ..Default::default() };
    // HTTPS 相关(在 SSL server 里)
    let mut hsts = false;
    if let Some(s) = ssl_srv {
        let c = kids(s.node);
        if let Some(n) = one(c, "ssl_protocols") {
            let p: Vec<String> = args(n).to_vec();
            if p != ["TLSv1.2", "TLSv1.3"] {
                adv.tls_protocols = Some(p);
            }
        }
        adv.ciphers = arg0(c, "ssl_ciphers").map(str::to_string);
        adv.ocsp = arg0(c, "ssl_stapling") == Some("on");
        adv.no_http2 = arg0(c, "http2") != Some("on");
        for h in all(c, "add_header") {
            if args(h).first().map(String::as_str) == Some("Strict-Transport-Security") {
                hsts = true;
                let v = args(h).get(1).cloned().unwrap_or_default();
                let age = v.split(';').next().and_then(|m| m.trim().strip_prefix("max-age=")).and_then(|a| a.parse::<u32>().ok());
                adv.hsts_age = age.filter(|a| *a != 31536000);
                adv.hsts_sub = v.contains("includeSubDomains");
                adv.hsts_preload = v.contains("preload");
            }
        }
    }

    // 站点级(取 primary server)
    adv.max_body = arg0(pc, "client_max_body_size").map(str::to_string);
    match arg0(pc, "gzip") {
        Some("on") => {
            adv.gzip = Some(true);
            adv.gzip_level = arg0(pc, "gzip_comp_level").and_then(|v| v.parse().ok());
            adv.gzip_types = one(pc, "gzip_types").map(|n| args(n).join(" ")).filter(|t| t != DEFAULT_GZIP_TYPES);
        }
        Some("off") => adv.gzip = Some(false),
        _ => {}
    }
    let allows: Vec<String> = all(pc, "allow").iter().filter_map(|n| args(n).first().cloned()).collect();
    let denies: Vec<String> = all(pc, "deny").iter().filter_map(|n| args(n).first().cloned()).collect();
    if denies.iter().any(|d| d == "all") && !allows.is_empty() {
        adv.ip_mode = 1;
        adv.ip_list = allows;
    } else if !denies.is_empty() && !denies.iter().any(|d| d == "all") {
        adv.ip_mode = 2;
        adv.ip_list = denies;
    }
    if let Some(realm) = arg0(pc, "auth_basic").filter(|r| *r != "off") {
        let users = hint.and_then(|h| h.adv.auth.as_ref()).map(|a| a.users.clone()).unwrap_or_default();
        adv.auth = Some(BasicAuth { realm: realm.to_string(), users: users.into_iter().map(|u| AuthUser { name: u.name, pass: None }).collect() });
    }
    for h in all(pc, "add_header") {
        let a = args(h);
        if let [n, v, ..] = a
            && n != "Strict-Transport-Security"
            && n != "Alt-Svc"
        {
            adv.headers.push((n.clone(), v.clone()));
        }
    }
    adv.log_off = arg0(pc, "access_log") == Some("off");
    adv.error_level = one(pc, "error_log").and_then(|n| args(n).get(1)).filter(|l| *l != "warn").cloned();
    for e in all(pc, "error_page") {
        if let [code, target] = args(e)
            && let Ok(c) = code.parse::<u16>()
        {
            adv.err_pages.push((c, target.clone()));
        }
    }
    adv.real_ip = all(pc, "set_real_ip_from").iter().filter_map(|n| args(n).first().cloned()).collect();
    adv.real_ip_header = arg0(pc, "real_ip_header").filter(|h| *h != "X-Forwarded-For" && !adv.real_ip.is_empty()).map(str::to_string);
    for r in all(pc, "rewrite") {
        if let [re, to, flag] = args(r) {
            adv.rewrites.push(Rewrite { regex: re.clone(), to: to.clone(), flag: flag.clone() });
        }
    }
    // 流量限制
    let l = &mut adv.limits;
    if let Some(n) = one(pc, "limit_req") {
        let zone = args(n).iter().find_map(|a| a.strip_prefix("zone=")).unwrap_or("");
        let z = all(&top, "limit_req_zone").into_iter().find(|z| args(z).iter().any(|a| a == &format!("zone={zone}:10m")));
        if let Some((r, m)) = z.and_then(|z| args(z).iter().find_map(|a| parse_rate(a))) {
            l.req_rate = Some(r);
            l.req_min = m;
            l.burst = args(n).iter().find_map(|a| a.strip_prefix("burst=")).and_then(|v| v.parse().ok()).unwrap_or(0);
            l.nodelay = args(n).iter().any(|a| a == "nodelay");
        }
    }
    l.conn = one(pc, "limit_conn").and_then(|n| args(n).get(1)).and_then(|v| v.parse().ok());
    l.speed = arg0(pc, "limit_rate").map(str::to_string);
    l.speed_after = l.speed.as_ref().and_then(|_| arg0(pc, "limit_rate_after")).map(str::to_string);
    if l.req_rate.is_some() || l.conn.is_some() {
        for g in all(&top, "geo") {
            for e in kids(g) {
                if let [ip, zero] = &e.words[..]
                    && zero == "0"
                    && ip != "default"
                {
                    l.exempt.push(ip.clone());
                }
            }
        }
    }

    // 站点类型
    let locs: Vec<&NgxNode> = all(pc, "location").into_iter().filter(|l| !is_acme(l)).collect();
    let kind = if locs.iter().any(|l| arg0(kids(l), "proxy_pass").is_some()) {
        let mut locations = Vec::new();
        let mut timeouts = Vec::new();
        for l in locs.iter().filter(|l| arg0(kids(l), "proxy_pass").is_some()) {
            // 认不出的 location(正则匹配、非常规 proxy_pass 等)不进表单,留在正文里原样保留(会列入「Otter 不管理的内容」)
            let Some((loc, t)) = parse_location(site, l, &upstreams) else { continue };
            locations.push(loc);
            timeouts.push(t);
        }
        if locations.is_empty() {
            return None;
        }
        // 读取超时:出现最多的值是站点默认(300 视为没设),其余的记在各自的路径上
        let mut best: Option<(u32, usize)> = None;
        for t in timeouts.iter().flatten() {
            let n = timeouts.iter().flatten().filter(|x| *x == t).count();
            if best.is_none_or(|b| n > b.1) {
                best = Some((*t, n));
            }
        }
        let dflt = best.map(|b| b.0).unwrap_or(300);
        adv.read_timeout = (dflt != 300).then_some(dflt);
        for (l, t) in locations.iter_mut().zip(&timeouts) {
            l.timeout = t.filter(|x| *x != dflt);
        }
        SiteKind::Proxy { locations, upstreams }
    } else if one(pc, "root").is_some() {
        let main = locs.iter().find(|l| args(l) == ["/"]).copied()?;
        let spa = one(kids(main), "try_files").is_some_and(|t| args(t).last().map(String::as_str) == Some("/index.html"));
        let index = arg0(kids(main), "autoindex") == Some("on");
        adv.index_files = one(pc, "index").map(|n| args(n).join(" ")).filter(|v| v != "index.html index.htm");
        for l in locs.iter().filter(|l| args(l).first().map(String::as_str) == Some("~*")) {
            let Some(re) = args(l).get(1) else { continue };
            let Some(exts) = re.strip_prefix("\\.(").and_then(|r| r.strip_suffix(")$")) else { continue };
            let exts = exts.replace('|', " ");
            let c = kids(l);
            let exp = arg0(c, "expires").map(str::to_string);
            if let Some(v) = one(c, "valid_referers") {
                adv.hotlink_exts = Some(exts);
                adv.hotlink_none = args(v).iter().any(|a| a == "none");
                adv.hotlink_allow = args(v).iter().filter(|a| !matches!(a.as_str(), "none" | "blocked" | "server_names")).cloned().collect();
                if adv.cache_time.is_none() {
                    adv.cache_time = exp;
                }
            } else if exp.is_some() {
                adv.cache_exts = Some(exts);
                adv.cache_time = exp;
            }
        }
        if adv.cache_exts.is_none() {
            adv.cache_time = None;
        }
        SiteKind::Static { spa, index }
    } else {
        let main = locs.iter().find(|l| args(l) == ["/"]).copied()?;
        let r = one(kids(main), "return")?;
        let code: u16 = args(r).first()?.parse().ok()?;
        let to = args(r).get(1)?.strip_suffix("$request_uri")?.to_string();
        SiteKind::Redirect { to, code }
    };

    let spec = SiteSpec { name: site.to_string(), kind, listeners, ipv6, hsts, adv };
    let unknown = unknown_directives(text, &top, &spec);
    let comments = text.lines().filter(|l| l.trim_start().starts_with('#') && !l.starts_with("# managed by otter") && !l.starts_with("# otter:")).count();
    Some(Parsed { spec, unknown, comments })
}

/// 叶子指令的路径化文本(块头用 `>` 连接)
fn flatten(ns: &[NgxNode], prefix: &str, out: &mut Vec<String>) {
    for n in ns {
        match &n.children {
            Some(c) => flatten(c, &format!("{prefix}{} > ", n.words.join(" ")), out),
            None if !n.words.is_empty() => out.push(format!("{prefix}{}", n.words.join(" "))),
            None => {}
        }
    }
}

fn unknown_directives(_text: &str, top: &[NgxNode], spec: &SiteSpec) -> Vec<String> {
    let rendered = render_site(spec, &Target::own());
    let toks = ngx_tokens(&rendered);
    let mut i = 0;
    let rnodes = ngx_parse(&toks, &mut i);
    let (mut have, mut want) = (Vec::new(), Vec::new());
    flatten(top, "", &mut have);
    flatten(&rnodes, "", &mut want);
    let mut unknown = Vec::new();
    for h in have {
        match want.iter().position(|w| *w == h) {
            Some(p) => {
                want.swap_remove(p);
            }
            None => unknown.push(h),
        }
    }
    unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ls(d: &[&str], port: u16, cert: Option<&str>, force: bool) -> Listener {
        Listener { domains: d.iter().map(|x| x.to_string()).collect(), port, cert: cert.map(str::to_string), force_https: force }
    }

    fn proxy_spec() -> SiteSpec {
        SiteSpec {
            name: "app".into(),
            kind: SiteKind::Proxy {
                locations: vec![
                    Location { timeout: Some(60), ..Location::new("/api/", Backend::Group { https: false, name: "pool".into() }) },
                    Location { strip: true, ws: false, host: false, ..Location::new("/files/", Backend::Url("http://127.0.0.1:9000".into())) },
                    Location { exact: true, ..Location::new("/health", Backend::Url("https://10.0.0.5:8443".into())) },
                    Location::new("/", Backend::Url("http://127.0.0.1:3000".into())),
                ],
                upstreams: vec![Upstream { name: "pool".into(), policy: Policy::LeastConn, servers: vec![UServer { addr: "10.0.0.1:80".into(), weight: 3, backup: false }, UServer { addr: "10.0.0.2:80".into(), weight: 1, backup: true }], max_fails: Some(3), fail_timeout: Some(20) }],
            },
            listeners: vec![ls(&["a.com", "www.a.com"], 80, None, true), ls(&["a.com", "www.a.com"], 443, Some("a.com"), false)],
            ipv6: true,
            hsts: true,
            adv: Advanced { max_body: Some("50m".into()), read_timeout: Some(120), http3: true, ocsp: true, tls_protocols: Some(vec!["TLSv1.3".into()]), hsts_age: Some(63072000), hsts_sub: true, hsts_preload: true, ..Default::default() },
        }
    }

    fn assert_roundtrip(spec: &SiteSpec) {
        let text = render_site(spec, &Target::own());
        let p = parse_conf(&text, &spec.name, Some(spec)).unwrap_or_else(|| panic!("解析失败\n{text}"));
        assert!(p.unknown.is_empty(), "未识别:{:?}\n{text}", p.unknown);
        assert_eq!(&p.spec, spec, "\n{text}");
    }

    #[test]
    fn roundtrip_proxy_with_upstream_and_https() {
        assert_roundtrip(&proxy_spec());
    }

    #[test]
    fn roundtrip_static_redirect_and_plain() {
        let mut s = SiteSpec { name: "st".into(), kind: SiteKind::Static { spa: true, index: true }, listeners: vec![ls(&["s.com"], 80, None, false)], ipv6: false, hsts: false, adv: Advanced { index_files: Some("home.html".into()), cache_exts: Some("js css".into()), cache_time: Some("7d".into()), hotlink_exts: Some("jpg png".into()), hotlink_allow: vec!["*.ok.com".into()], hotlink_none: true, ..Default::default() } };
        assert_roundtrip(&s);
        s.kind = SiteKind::Redirect { to: "https://b.com".into(), code: 308 };
        s.adv = Advanced::default();
        assert_roundtrip(&s);
        s.kind = SiteKind::Proxy { locations: vec![Location::new("/", Backend::Url("http://127.0.0.1:1".into()))], upstreams: vec![] };
        assert_roundtrip(&s);
    }

    #[test]
    fn roundtrip_all_extras() {
        let mut s = proxy_spec();
        s.adv = Advanced {
            gzip: Some(true),
            gzip_level: Some(4),
            gzip_types: Some("text/css".into()),
            ip_mode: 1,
            ip_list: vec!["10.0.0.0/8".into(), "::1".into()],
            auth: Some(BasicAuth { realm: "Private area".into(), users: vec![AuthUser { name: "bob".into(), pass: None }] }),
            headers: vec![("X-A".into(), "a b".into()), ("Content-Security-Policy".into(), "default-src 'self'; img-src *".into())],
            error_level: Some("error".into()),
            err_pages: vec![(404, "/404.html".into()), (502, "https://x.com/d".into())],
            real_ip: vec!["172.16.0.0/12".into()],
            real_ip_header: Some("CF-Connecting-IP".into()),
            rewrites: vec![Rewrite { regex: "^/old/(.*)$".into(), to: "/new/$1".into(), flag: "permanent".into() }],
            limits: Limits { req_rate: Some(5), req_min: true, burst: 10, nodelay: true, conn: Some(20), speed: Some("1m".into()), speed_after: Some("5m".into()), exempt: vec!["192.168.0.0/16".into()] },
            ..Default::default()
        };
        assert_roundtrip(&s);
        s.adv.log_off = true;
        s.adv.ip_mode = 2;
        s.adv.gzip = Some(false);
        s.adv.gzip_level = None;
        s.adv.gzip_types = None;
        assert_roundtrip(&s);
    }

    /// 手改:改端口、加 gzip、改 location 目标——表单照实反映;手加的不支持指令被列出
    #[test]
    fn hand_edits_are_read_back() {
        let spec = proxy_spec();
        let text = render_site(&spec, &Target::own());
        let edited = text.replace("client_max_body_size 50m;", "client_max_body_size 8m;\n    gzip on;\n    proxy_buffering off;\n    # 我的注释").replace("proxy_pass http://127.0.0.1:3000;", "proxy_pass http://127.0.0.1:4000;");
        let p = parse_conf(&edited, "app", Some(&spec)).unwrap();
        assert_eq!(p.spec.adv.max_body.as_deref(), Some("8m"));
        assert_eq!(p.spec.adv.gzip, Some(true));
        let SiteKind::Proxy { locations, .. } = &p.spec.kind else { panic!() };
        assert_eq!(locations.last().unwrap().backend, Backend::Url("http://127.0.0.1:4000".into()));
        // 不支持的指令被列出(两个 server 各一条),注释被计数
        assert!(p.unknown.iter().any(|u| u.contains("proxy_buffering off")), "{:?}", p.unknown);
        assert_eq!(p.comments, 2);
        // 能读回的手改不算「未识别」;但跳转用的 80 server 里加的 gzip 表单管不到,会被列出
        assert!(!p.unknown.iter().any(|u| u.contains("client_max_body_size")), "{:?}", p.unknown);
        assert_eq!(p.unknown.iter().filter(|u| u.contains("gzip on")).count(), 1, "{:?}", p.unknown);
    }

    #[test]
    fn unsupported_structures_fall_back() {
        let spec = proxy_spec();
        let text = render_site(&spec, &Target::own()).replace("location = /health", "location ~ /health");
        // 认不出的 location 不进表单,只列入未识别
        let p = parse_conf(&text, "app", Some(&spec)).unwrap();
        assert!(p.unknown.iter().any(|u| u.contains("location ~ /health")), "{:?}", p.unknown);
        assert!(parse_conf("server { listen 80; listen 81; server_name a.com; }", "x", None).is_none());
    }
}
