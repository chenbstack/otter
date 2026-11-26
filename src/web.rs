//! 网站管理的纯逻辑:站点配置的生成与解析、证书与账号的解析、读取状态和各种操作的脚本。
//! 页面在 `pages/websites.rs`;目录布局见 `srvdata.rs`;设计见 `docs/modules/websites.md`。
//! 所有脚本都以 root 执行(`sys_exec(root=true)`),路径一律取自 `srvdata`,名字先过 `valid_name`。

use serde_json::{Value, json};

use crate::srvdata::{self, b64, valid_name, write_file_cmd};
use crate::sysops::quote;

pub const CONTAINER: &str = "otter-openresty";
pub const ACME_IMAGE: &str = "neilpang/acme.sh";
/// 证书目录在容器里的挂载点
pub const CERT_MOUNT: &str = "/etc/otter/certs";
/// 续期 cron 文件(root 的系统任务,「计划任务」页能看到)
pub const CRON_FILE: &str = "/etc/cron.d/otter-websites";
pub const RENEW_SH: &str = "/opt/otter/websites/bin/renew.sh";
/// 离到期这么多天才续期
pub const RENEW_DAYS: u32 = 30;

/// 所有脚本开头的环境:读 `target.env`(接管信息),没有就用 Otter 自己装的 OpenResty 的默认值。
/// `$N` 容器名、`$B` 容器里的 nginx 命令、`$CONF` 站点配置目录、`$CERTS` 证书目录(宿主机)、`$CM` 证书在容器里的挂载点、`$ACME` 验证目录、`$SITES` 静态站点根。目录不存在时值为 `NONE`
fn pre() -> String {
    let w = srvdata::WEB;
    format!("W={w}; [ -f $W/target.env ] && . $W/target.env; N=${{OT_CONTAINER:-{CONTAINER}}}; B=${{OT_BIN:-nginx}}; CONF=${{OT_CONF:-$W/conf/conf.d}}; CERTS=${{OT_CERTS:-$W/certs}}; CM=${{OT_CERT_MOUNT:-{CERT_MOUNT}}}; ACME=${{OT_ACME:-$W/acme-webroot}}; SITES=${{OT_SITES:-$W/sites}}")
}

/// 没有证书目录时拒绝(接管的容器没有可写的证书挂载)
const GUARD_CERTS: &str = "[ \"$CERTS\" = NONE ] && { echo 'ERR:这个 OpenResty 没有挂载证书目录,Otter 管不了它的证书'; exit 0; }";

fn fill(s: &str) -> String {
    s.replace("@PRE@", &pre()).replace("@GC@", GUARD_CERTS).replace("@W@", srvdata::WEB).replace("@R@", srvdata::ROOT).replace("@C@", CONTAINER).replace("@T@", srvdata::TMP)
}


// ---------------------------------------------------------------- 被管理的 OpenResty(自己装的,或接管的)

/// 被管理的 nginx/OpenResty 容器的路径。`certs`/`acme`/`sites` 是(宿主机路径, 容器内挂载点);没有就是 None,对应功能不可用
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub container: String,
    pub bin: String,
    /// 站点配置目录(宿主机路径)
    pub conf: String,
    pub certs: Option<(String, String)>,
    pub acme: Option<(String, String)>,
    pub sites: Option<(String, String)>,
    /// 接管的(不是 Otter 装的)
    pub adopted: bool,
    /// 默认站点和新站点默认监听的 (HTTP, HTTPS) 端口
    pub ports: (u16, u16),
}

impl Default for Target {
    fn default() -> Self {
        Target::own()
    }
}

fn path_ok(p: &str) -> bool {
    p.starts_with('/') && !p.contains("..") && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '@' | '+'))
}

impl Target {
    /// Otter 自己安装的 OpenResty
    pub fn own() -> Target {
        let w = srvdata::WEB;
        Target {
            container: CONTAINER.into(),
            bin: "nginx".into(),
            conf: format!("{w}/conf/conf.d"),
            certs: Some((format!("{w}/certs"), CERT_MOUNT.into())),
            acme: Some((format!("{w}/acme-webroot"), "/www/acme".into())),
            sites: Some((format!("{w}/sites"), "/www/sites".into())),
            adopted: false,
            ports: (80, 443),
        }
    }

    /// 写进 `target.env` 的内容(shell 可 source;值都过 `path_ok`/名字校验)
    pub fn env_text(&self) -> Result<String, String> {
        let name_ok = !self.container.is_empty() && self.container.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        if !name_ok || !matches!(self.bin.as_str(), "nginx" | "openresty") || !path_ok(&self.conf) {
            return Err("容器名或路径里有不支持的字符".into());
        }
        let pair = |v: &Option<(String, String)>, k: &str, km: &str| -> Result<String, String> {
            match v {
                Some((h, m)) if path_ok(h) && path_ok(m) => Ok(format!("{k}={h}\n{km}={m}\n")),
                Some(_) => Err("路径里有不支持的字符".into()),
                None => Ok(format!("{k}=NONE\n")),
            }
        };
        Ok(format!(
            "# managed by otter (layout {})\n# {}\n{}OT_HTTP={}\nOT_HTTPS={}\nOT_CONTAINER={}\nOT_BIN={}\nOT_CONF={}\n{}{}{}",
            srvdata::LAYOUT,
            if self.adopted { "接管的 OpenResty/nginx 容器;删掉这个文件就是取消接管" } else { "Otter 安装的 OpenResty:记录容器名和端口" },
            if self.adopted { "" } else { "OT_OWN=1\n" },
            self.ports.0,
            self.ports.1,
            self.container,
            self.bin,
            self.conf,
            pair(&self.certs, "OT_CERTS", "OT_CERT_MOUNT")?,
            pair(&self.acme, "OT_ACME", "OT_ACME_MOUNT")?,
            pair(&self.sites, "OT_SITES", "OT_SITES_MOUNT")?
        ))
    }

    pub fn from_env(text: &str) -> Option<Target> {
        let kv: std::collections::HashMap<&str, &str> = text.lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.split_once('=')).collect();
        let port = |k: &str, d: u16| kv.get(k).and_then(|v| v.parse::<u16>().ok()).filter(|p| *p > 0).unwrap_or(d);
        let pair = |k: &str, km: &str| -> Option<(String, String)> {
            let h = kv.get(k)?;
            if *h == "NONE" { None } else { Some((h.to_string(), kv.get(km).map(|m| m.to_string()).unwrap_or_default())) }
        };
        Some(Target { container: kv.get("OT_CONTAINER")?.to_string(), bin: kv.get("OT_BIN").unwrap_or(&"nginx").to_string(), conf: kv.get("OT_CONF")?.to_string(), certs: pair("OT_CERTS", "OT_CERT_MOUNT"), acme: pair("OT_ACME", "OT_ACME_MOUNT"), sites: pair("OT_SITES", "OT_SITES_MOUNT"), adopted: !kv.contains_key("OT_OWN"), ports: (port("OT_HTTP", 80), port("OT_HTTPS", 443)) })
    }

    fn cert_mount(&self) -> &str {
        self.certs.as_ref().map(|c| c.1.as_str()).unwrap_or(CERT_MOUNT)
    }
    fn acme_mount(&self) -> Option<&str> {
        self.acme.as_ref().map(|c| c.1.as_str())
    }
    fn sites_mount(&self) -> &str {
        self.sites.as_ref().map(|c| c.1.as_str()).unwrap_or("/www/sites")
    }
}

// ---------------------------------------------------------------- 校验

/// 域名:小写字母数字和 `-` 组成的标签,可以以 `*.` 开头(泛域名);至少两段
pub fn valid_domain(d: &str) -> bool {
    let body = d.strip_prefix("*.").unwrap_or(d);
    body.len() <= 253 && body.contains('.') && body.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
}

/// 反向代理目标/跳转地址:`http(s)://…`,不含会破坏 nginx 配置的字符
pub fn valid_url(u: &str) -> bool {
    (u.starts_with("http://") || u.starts_with("https://")) && u.len() <= 500 && u.len() > 8 && !u.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, ';' | '{' | '}' | '"' | '\'' | '\\' | '$' | '`' | '#'))
}

/// 把用户输入的一串域名(空格、逗号、换行分隔)拆开、转小写、去重
pub fn split_domains(s: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for d in s.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '，') {
        let d = d.trim().to_lowercase();
        if !d.is_empty() && !v.contains(&d) {
            v.push(d);
        }
    }
    v
}

// ---------------------------------------------------------------- 站点

#[derive(Clone, Debug, PartialEq)]
pub enum SiteKind {
    Proxy { target: String, ws: bool, host: bool },
    Static { spa: bool, index: bool },
    Redirect { to: String, code: u16 },
}

impl SiteKind {
    pub fn label(&self) -> &'static str {
        match self {
            SiteKind::Proxy { .. } => "反向代理",
            SiteKind::Static { .. } => "静态站点",
            SiteKind::Redirect { .. } => "跳转",
        }
    }
}

/// 一组监听:这些域名在这个端口上共用一张证书(`cert` 有值就是 SSL)
#[derive(Clone, Debug, PartialEq)]
pub struct Listener {
    pub domains: Vec<String>,
    pub port: u16,
    /// 用的证书名(`certs/<名>/`);有值就是这个端口启用 SSL
    pub cert: Option<String>,
    /// 只对不带 SSL 的监听有意义:整个站点改成跳转到 `https://$host`(443)
    pub force_https: bool,
}

impl Listener {
    pub fn ssl(&self) -> bool {
        self.cert.is_some()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SiteSpec {
    /// 站点名,也是配置文件名(`<name>.conf`)和静态目录名
    pub name: String,
    pub kind: SiteKind,
    pub listeners: Vec<Listener>,
    /// 每个监听同时写 `listen [::]:端口`
    pub ipv6: bool,
    pub hsts: bool,
    pub adv: Advanced,
}

/// 站点的高级选项;都是空/None 就按默认(不写对应指令)
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Advanced {
    /// 上传大小上限,如 `100m`、`2g`、`0`(不限);None 沿用全局设置
    pub max_body: Option<String>,
    /// 反向代理读取超时(秒);None 用 300
    pub read_timeout: Option<u32>,
}

/// `100m` / `512k` / `2g` / `0`
pub fn valid_size(v: &str) -> bool {
    let n = v.strip_suffix(|c: char| matches!(c, 'k' | 'K' | 'm' | 'M' | 'g' | 'G')).unwrap_or(v);
    !n.is_empty() && n.len() <= 9 && n.chars().all(|c| c.is_ascii_digit())
}

impl SiteSpec {
    /// 所有监听里的域名(去重,保持出现顺序)
    pub fn domains(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for l in &self.listeners {
            for d in &l.domains {
                if !v.contains(d) {
                    v.push(d.clone());
                }
            }
        }
        v
    }

    /// 用到的证书名(去重)
    pub fn certs(&self) -> Vec<&str> {
        let mut v: Vec<&str> = Vec::new();
        for c in self.listeners.iter().filter_map(|l| l.cert.as_deref()) {
            if !v.contains(&c) {
                v.push(c);
            }
        }
        v
    }

    pub fn uses_cert(&self, name: &str) -> bool {
        self.listeners.iter().any(|l| l.cert.as_deref() == Some(name))
    }

    /// 检查能不能生成配置;返回第一个问题
    pub fn check(&self) -> Result<(), String> {
        if !valid_name(&self.name) {
            return Err("站点名只能用小写字母、数字、点、下划线和短横线".into());
        }
        if self.listeners.is_empty() {
            return Err("至少要有一组监听".into());
        }
        let mut seen: Vec<(u16, &str)> = Vec::new();
        for l in &self.listeners {
            if l.port == 0 {
                return Err("端口要在 1–65535 之间".into());
            }
            if l.domains.is_empty() {
                return Err(format!("端口 {} 这一组至少填一个域名", l.port));
            }
            for d in &l.domains {
                if !valid_domain(d) {
                    return Err(format!("域名不合法:{d}"));
                }
                if seen.contains(&(l.port, d.as_str())) {
                    return Err(format!("{d} 在端口 {} 上重复了", l.port));
                }
                seen.push((l.port, d.as_str()));
            }
            if let Some(c) = &l.cert
                && !valid_name(c)
            {
                return Err("证书名不合法".into());
            }
        }
        if let Some(b) = &self.adv.max_body
            && !valid_size(b)
        {
            return Err("上传大小要写成 100m、512k、2g 这样,0 表示不限".into());
        }
        if self.adv.read_timeout.is_some_and(|t| t == 0 || t > 86400) {
            return Err("读取超时要在 1–86400 秒之间".into());
        }
        match &self.kind {
            SiteKind::Proxy { target, .. } if !valid_url(target) => Err("后端地址要以 http:// 或 https:// 开头,不能有空格和特殊符号".into()),
            SiteKind::Redirect { to, code } if !valid_url(to) || !matches!(code, 301 | 302 | 307 | 308) => Err("跳转地址要以 http:// 或 https:// 开头".into()),
            _ => Ok(()),
        }
    }
}

fn fnv(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn esc(v: &str) -> String {
    v.replace('%', "%25").replace(' ', "%20").replace('\n', "%0A").replace('\r', "%0D").replace('\t', "%09")
}

fn unesc(v: &str) -> String {
    v.replace("%09", "\t").replace("%0D", "\r").replace("%0A", "\n").replace("%20", " ").replace("%25", "%")
}

fn header_line(s: &SiteSpec) -> String {
    let b = |v: bool| if v { "1" } else { "0" };
    let mut h = format!("# otter:site v2 name={}", esc(&s.name));
    match &s.kind {
        SiteKind::Proxy { target, ws, host } => h += &format!(" type=proxy target={} ws={} host={}", esc(target), b(*ws), b(*host)),
        SiteKind::Static { spa, index } => h += &format!(" type=static spa={} index={}", b(*spa), b(*index)),
        SiteKind::Redirect { to, code } => h += &format!(" type=redirect to={} code={code}", esc(to)),
    }
    let ls: Vec<String> = s.listeners.iter().map(|l| format!("{}@{}@{}@{}", l.domains.join(","), l.port, l.cert.as_deref().unwrap_or("-"), b(l.force_https))).collect();
    h += &format!(" ipv6={} hsts={} ls={}", b(s.ipv6), b(s.hsts), esc(&ls.join(";")));
    // 高级选项:没填的不写
    if let Some(v) = &s.adv.max_body {
        h += &format!(" body={}", esc(v));
    }
    if let Some(v) = s.adv.read_timeout {
        h += &format!(" timeout={v}");
    }
    h
}

fn acme_loc(t: &Target) -> String {
    match t.acme_mount() {
        Some(m) => format!("    location ^~ /.well-known/acme-challenge/ {{\n        root {m};\n        default_type text/plain;\n    }}\n"),
        None => String::new(),
    }
}

fn locations(s: &SiteSpec, t: &Target) -> String {
    match &s.kind {
        SiteKind::Proxy { target, ws, host } => {
            let tg = t;
            let mut t = String::from("    location / {\n");
            t += &format!("        proxy_pass {target};\n        proxy_http_version 1.1;\n");
            t += &format!("        proxy_set_header Host {};\n", if *host { "$host" } else { "$proxy_host" });
            t += "        proxy_set_header X-Real-IP $remote_addr;\n        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n        proxy_set_header X-Forwarded-Proto $scheme;\n";
            if *ws {
                // 自己装的 nginx.conf 里有 map $connection_upgrade;接管的不一定有,直接写死
                t += &format!("        proxy_set_header Upgrade $http_upgrade;\n        proxy_set_header Connection {};\n", if tg.adopted { "\"upgrade\"" } else { "$connection_upgrade" });
            }
            if target.starts_with("https://") {
                t += "        proxy_ssl_server_name on;\n";
            }
            t += &format!("        proxy_read_timeout {}s;\n    }}\n", s.adv.read_timeout.unwrap_or(300));
            t
        }
        SiteKind::Static { spa, index } => {
            let fallback = if *spa { "/index.html" } else { "=404" };
            format!("    root {}/{};\n    index index.html index.htm;\n    location / {{\n        try_files $uri $uri/ {fallback};\n{}    }}\n", t.sites_mount(), s.name, if *index { "        autoindex on;\n" } else { "" })
        }
        SiteKind::Redirect { to, code } => format!("    location / {{\n        return {code} {}$request_uri;\n    }}\n", to.trim_end_matches('/')),
    }
}

fn body(s: &SiteSpec, tg: &Target) -> String {
    let fmt = if tg.adopted { "" } else { " main" };
    let logs = format!("    access_log /var/log/nginx/{n}.access.log{fmt};\n    error_log /var/log/nginx/{n}.error.log warn;\n", n = s.name);
    // 有 443 的 SSL 监听时,「强制 HTTPS」才有地方可跳
    let has_443 = s.listeners.iter().any(|l| l.ssl() && l.port == tg.ports.1);
    let https_at = if tg.ports.1 == 443 { String::new() } else { format!(":{}", tg.ports.1) };
    let mut out: Vec<String> = Vec::new();
    for l in &s.listeners {
        let ssl = if l.ssl() { " ssl" } else { "" };
        let mut t = format!("server {{\n    listen {}{ssl};\n", l.port);
        if s.ipv6 {
            t += &format!("    listen [::]:{}{ssl};\n", l.port);
        }
        if l.ssl() {
            t += "    http2 on;\n";
        }
        t += &format!("    server_name {};\n", l.domains.join(" "));
        if let Some(c) = &l.cert {
            let cm = tg.cert_mount();
            t += &format!("    ssl_certificate {cm}/{c}/fullchain.pem;\n    ssl_certificate_key {cm}/{c}/privkey.pem;\n    ssl_protocols TLSv1.2 TLSv1.3;\n");
        }
        t += &logs;
        if let Some(b) = &s.adv.max_body {
            t += &format!("    client_max_body_size {b};\n");
        }
        if l.port == tg.ports.0 && !l.ssl() {
            t += &acme_loc(tg);
        }
        if l.ssl() && s.hsts {
            t += "    add_header Strict-Transport-Security \"max-age=31536000\" always;\n";
        }
        if !l.ssl() && l.force_https && has_443 {
            t += &format!("    location / {{\n        return 301 https://$host{https_at}$request_uri;\n    }}\n");
        } else {
            t += &locations(s, tg);
        }
        t += "}\n";
        out.push(t);
    }
    out.join("\n")
}

/// 生成站点配置文件全文
pub fn render_site(s: &SiteSpec, t: &Target) -> String {
    let b = body(s, t);
    format!("# managed by otter (layout {})\n{}\n# otter:hash {}\n{}", srvdata::LAYOUT, header_line(s), fnv(&b), b)
}

/// 从配置文件里读回 Otter 生成的站点;第二项是正文被手工改过。没有 Otter 头部的返回 None
pub fn parse_site(text: &str) -> Option<(SiteSpec, bool)> {
    let mut lines = text.lines();
    let mut head = None;
    let mut hash = None;
    let mut consumed = 0usize;
    let mut v2 = false;
    for l in lines.by_ref() {
        consumed += l.len() + 1;
        if let Some(h) = l.strip_prefix("# otter:site v1 ").or_else(|| l.strip_prefix("# otter:site v2 ").inspect(|_| v2 = true)) {
            head = Some(h.to_string());
        } else if let Some(h) = l.strip_prefix("# otter:hash ") {
            hash = Some(h.trim().to_string());
            break;
        }
    }
    let head = head?;
    let kv: std::collections::HashMap<&str, String> = head.split(' ').filter_map(|p| p.split_once('=')).map(|(k, v)| (k, unesc(v))).collect();
    let g = |k: &str| kv.get(k).cloned().unwrap_or_default();
    let flag = |k: &str| kv.get(k).is_some_and(|v| v == "1");
    let kind = match g("type").as_str() {
        "proxy" => SiteKind::Proxy { target: g("target"), ws: flag("ws"), host: flag("host") },
        "static" => SiteKind::Static { spa: flag("spa"), index: flag("index") },
        "redirect" => SiteKind::Redirect { to: g("to"), code: g("code").parse().unwrap_or(301) },
        _ => return None,
    };
    let listeners: Vec<Listener> = if v2 {
        g("ls")
            .split(';')
            .filter(|x| !x.is_empty())
            .filter_map(|x| {
                let f: Vec<&str> = x.split('@').collect();
                let [d, port, cert, force] = f[..] else { return None };
                Some(Listener { domains: d.split(',').filter(|d| !d.is_empty()).map(str::to_string).collect(), port: port.parse().ok()?, cert: Some(cert).filter(|c| !c.is_empty() && *c != "-").map(str::to_string), force_https: force == "1" })
            })
            .collect()
    } else {
        // v1:一组域名 + 可选证书 → 80 一组,有证书再加 443 一组
        let domains: Vec<String> = g("domains").split(',').filter(|d| !d.is_empty()).map(str::to_string).collect();
        let cert = Some(g("cert")).filter(|c| !c.is_empty() && c != "-");
        let mut v = vec![Listener { domains: domains.clone(), port: 80, cert: None, force_https: flag("force") && cert.is_some() }];
        if cert.is_some() {
            v.push(Listener { domains, port: 443, cert, force_https: false });
        }
        v
    };
    let adv = Advanced { max_body: Some(g("body")).filter(|v| !v.is_empty()), read_timeout: g("timeout").parse().ok() };
    let spec = SiteSpec { name: g("name"), kind, listeners, ipv6: flag("ipv6"), hsts: flag("hsts"), adv };
    let rest = text.get(consumed.min(text.len())..).unwrap_or("");
    let modified = hash.is_none_or(|h| fnv(rest) != h);
    Some((spec, modified))
}

// ---------------------------------------------------------------- 配置文件、compose

pub fn nginx_conf() -> String {
    "# managed by otter (layout 1)\nworker_processes auto;\nerror_log /var/log/nginx/error.log warn;\nevents {\n    worker_connections 4096;\n}\nhttp {\n    include mime.types;\n    default_type application/octet-stream;\n    server_tokens off;\n    sendfile on;\n    keepalive_timeout 65;\n    client_max_body_size 100m;\n    log_format main '$remote_addr - $remote_user [$time_local] \"$request\" $status $body_bytes_sent \"$http_referer\" \"$http_user_agent\" $request_time';\n    access_log /var/log/nginx/access.log main;\n    map $http_upgrade $connection_upgrade {\n        default upgrade;\n        '' close;\n    }\n    ssl_session_cache shared:SSL:10m;\n    ssl_session_timeout 1d;\n    gzip on;\n    gzip_types text/plain text/css application/json application/javascript text/xml application/xml image/svg+xml;\n    include /etc/nginx/conf.d/*.conf;\n}\n".to_string()
}

/// 默认站点:没匹配到域名的请求(包括直接用 IP 访问)一律断开;证书验证用的路径仍然放行
pub fn default_conf(http: u16, https: u16) -> String {
    let t = Target::own();
    format!("# managed by otter (layout 1)\n# otter:default\nserver {{\n    listen {http} default_server;\n    server_name _;\n{}    location / {{\n        return 444;\n    }}\n}}\n\nserver {{\n    listen {https} ssl default_server;\n    server_name _;\n    ssl_reject_handshake on;\n}}\n", acme_loc(&t))
}

/// 安装 OpenResty 的表单(安装抽屉填的东西)
#[derive(Clone, Debug, PartialEq)]
pub struct InstallSpec {
    /// 镜像 tag,如 `alpine`、`1.27.1.2-alpine`
    pub version: String,
    /// 堆栈名,也是 compose 目录名
    pub project: String,
    pub container: String,
    pub http: u16,
    pub https: u16,
    /// CPU 核数上限(空 = 不限制)
    pub cpus: String,
    /// 内存上限,如 `512m`、`2g`(空 = 不限制)
    pub memory: String,
}

impl Default for InstallSpec {
    fn default() -> Self {
        InstallSpec { version: "alpine".into(), project: CONTAINER.into(), container: CONTAINER.into(), http: 80, https: 443, cpus: String::new(), memory: String::new() }
    }
}

/// 默认的堆栈名/容器名:`otter-openresty`;已被占用就在后面加 4 位随机数
pub fn gen_install_name(taken: &[String]) -> String {
    use rand::RngExt as _;
    if !taken.iter().any(|n| n == CONTAINER) {
        return CONTAINER.into();
    }
    let mut rng = rand::rng();
    loop {
        let suf: String = (0..4).map(|_| b"0123456789abcdef"[rng.random_range(0..16)] as char).collect();
        let n = format!("{CONTAINER}-{suf}");
        if !taken.contains(&n) {
            return n;
        }
    }
}

impl InstallSpec {
    pub fn image(&self) -> String {
        format!("openresty/openresty:{}", self.version)
    }

    /// compose 文件所在目录(只读,由堆栈名决定)
    pub fn dir(&self) -> String {
        srvdata::compose_dir(&self.project)
    }

    /// 检查表单;`taken_names` 是已有的堆栈名和容器名,`used_ports` 是服务器上已被占用的端口
    pub fn validate(&self, taken_names: &[String], used_ports: &[u16]) -> Result<(), String> {
        if !crate::dbinstall::valid_tag(&self.version) {
            return Err("版本号只能用字母、数字、. _ -".into());
        }
        for (what, n) in [("堆栈名", &self.project), ("容器名", &self.container)] {
            if !crate::docker::valid_project(n) {
                return Err(format!("{what}只能用小写字母、数字、- 和 _,并以字母或数字开头"));
            }
        }
        if taken_names.iter().any(|n| *n == self.project) {
            return Err("已经有同名的堆栈".into());
        }
        if taken_names.iter().any(|n| *n == self.container) {
            return Err("已经有同名的容器".into());
        }
        if self.http == 0 || self.https == 0 {
            return Err("端口要在 1 到 65535 之间".into());
        }
        if self.http == self.https {
            return Err("HTTP 和 HTTPS 端口不能相同".into());
        }
        for (what, p) in [("HTTP", self.http), ("HTTPS", self.https)] {
            if used_ports.contains(&p) {
                return Err(format!("服务器上 {p} 端口已被占用,{what} 端口换一个"));
            }
        }
        crate::dbinstall::valid_cpus(&self.cpus)?;
        crate::dbinstall::valid_memory(&self.memory)
    }

    /// 安装后记在 `target.env` 里的信息
    pub fn target(&self) -> Target {
        Target { container: self.container.clone(), ports: (self.http, self.https), ..Target::own() }
    }

    /// `compose.yml` 文本(host 网络,所以没有 `ports`;端口由站点配置里的 `listen` 决定)
    pub fn compose(&self) -> String {
        let w = srvdata::WEB;
        let mut s = String::from("# managed by otter (layout 1)\nservices:\n  openresty:\n");
        s += &format!("    image: {}\n    container_name: {}\n    network_mode: host\n    restart: unless-stopped\n", self.image(), self.container);
        if !self.cpus.is_empty() {
            s += &format!("    cpus: \"{}\"\n", self.cpus);
        }
        if !self.memory.is_empty() {
            s += &format!("    mem_limit: {}\n", self.memory.to_ascii_lowercase());
        }
        s += &format!("    labels:\n      otter.managed: websites\n    volumes:\n      - {w}/conf/nginx.conf:/usr/local/openresty/nginx/conf/nginx.conf:ro\n      - {w}/conf/conf.d:/etc/nginx/conf.d:ro\n      - {w}/certs:/etc/otter/certs:ro\n      - {w}/sites:/www/sites:ro\n      - {w}/acme-webroot:/www/acme:ro\n      - {w}/logs:/var/log/nginx\n");
        s
    }
}

/// 查 Docker Hub 上 OpenResty 镜像的 tag 的地址(第 `page` 页;镜像有上千个 tag,稳定版本散在前十页里,第 11 页起接口拒绝访问)
pub fn tags_url(page: u32) -> String {
    format!("https://hub.docker.com/v2/repositories/openresty/openresty/tags?page_size=100&name=-alpine&page={page}")
}

const TAG_PAGES: u32 = 10;

/// 服务器上取 tag 列表的脚本:十页并行请求(curl,没有就 wget),按页序输出一串 JSON
pub fn tags_script() -> String {
    format!(
        "d=$(mktemp -d); for i in $(seq 1 {TAG_PAGES}); do u='https://hub.docker.com/v2/repositories/openresty/openresty/tags?page_size=100&name=-alpine&page='$i; ( curl -fsSL --max-time 15 \"$u\" || wget -qO- -T 15 \"$u\" ) > $d/$i.json 2>/dev/null & done; wait; for i in $(seq 1 {TAG_PAGES}); do cat $d/$i.json 2>/dev/null; echo; done; rm -rf $d"
    )
}

/// 从 tags 接口响应(一页,或多页 JSON 连在一起)里挑出可选版本:`alpine`(跟随最新)排第一,其后是 `1.27.1.2-alpine` 这样的固定版本,新的在前;带构建序号的(`1.27.1.2-3-alpine`)和 `-fat` 之类的变体不要
pub fn parse_tags(json: &str) -> Option<Vec<String>> {
    let mut fixed: Vec<(Vec<u32>, String)> = Vec::new();
    for v in serde_json::Deserializer::from_str(json).into_iter::<Value>().map_while(Result::ok) {
        let Some(rs) = v.get("results").and_then(|r| r.as_array()) else { continue };
        for n in rs.iter().filter_map(|r| r.get("name")?.as_str()) {
            let Some(ver) = n.strip_suffix("-alpine") else { continue };
            let Some(key) = ver.split('.').map(|p| p.parse::<u32>().ok()).collect::<Option<Vec<u32>>>() else { continue };
            if key.len() >= 4 {
                fixed.push((key, n.to_string()));
            }
        }
    }
    fixed.sort_by(|a, b| b.0.cmp(&a.0));
    fixed.dedup_by(|a, b| a.1 == b.1);
    if fixed.is_empty() {
        return None;
    }
    let mut list = vec!["alpine".to_string()];
    list.extend(fixed.into_iter().map(|t| t.1));
    Some(list)
}

/// 本机直接请求 Docker Hub(阻塞,放后台线程调用;十页并行)
pub fn fetch_tags_local() -> Result<Vec<String>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(15))).user_agent("otter").build().into();
    let pages: Vec<Result<String, String>> = std::thread::scope(|sc| {
        let hs: Vec<_> = (1..=TAG_PAGES)
            .map(|p| {
                let agent = &agent;
                sc.spawn(move || -> Result<String, String> {
                    let mut resp = agent.get(&tags_url(p)).call().map_err(|e| format!("请求 Docker Hub 失败:{e}"))?;
                    resp.body_mut().read_to_string().map_err(|e| format!("读取响应失败:{e}"))
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("线程异常".into()))).collect()
    });
    let first_err = pages.iter().find_map(|r| r.as_ref().err().cloned());
    let all: String = pages.into_iter().filter_map(Result::ok).collect::<Vec<_>>().join("\n");
    parse_tags(&all).ok_or_else(|| first_err.unwrap_or_else(|| "Docker Hub 返回的内容里没有版本".to_string()))
}

// ---------------------------------------------------------------- 读取状态

/// 读取全部状态的脚本(只读,要 root)
pub fn state_script() -> String {
    fill(
        r#"@PRE@
echo '@@DOCKER'
if command -v docker >/dev/null 2>&1; then if docker info >/dev/null 2>&1; then echo ok; else echo down; fi; else echo none; fi
echo '@@LAYOUT'; cat @R@/otter.json 2>/dev/null
echo '@@TARGET'; cat $W/target.env 2>/dev/null
echo '@@PORTS'
for p in 80 443; do
  l=$(ss -ltnpH "sport = :$p" 2>/dev/null | head -n1)
  if [ -n "$l" ]; then pr=$(echo "$l" | sed -n 's/.*users:(("\([^"]*\)".*/\1/p'); echo "$p ${pr:-?}"; fi
done
echo '@@PANEL'
docker ps --format '{{.Names}}' 2>/dev/null | grep -qi 1panel && echo 1panel
[ -d /opt/1panel ] && echo 1panel
[ -d /www/server/panel ] && echo baota
echo '@@CONTAINER'; docker inspect -f '{{.State.Status}} {{.Config.Image}}' $N 2>/dev/null
echo '@@VERSION'; docker exec $N $B -v 2>&1 | head -n1
echo '@@SITES'
for f in $CONF/*.conf $CONF/*.conf.off; do [ -f "$f" ] || continue; echo "@@F $(basename "$f")"; head -c 30000 "$f"; echo; done
echo '@@CERTS'
for d in $CERTS/*/; do [ -d "$d" ] || continue; n=$(basename "$d"); echo "@@C $n"; cat "$d/meta.json" 2>/dev/null; echo '@@X'; openssl x509 -in "$d/fullchain.pem" -noout -enddate -issuer -subject -ext subjectAltName 2>/dev/null || openssl x509 -in "$d/fullchain.pem" -noout -enddate -issuer -subject 2>/dev/null; done
echo '@@EXT'
ext_one() {
  echo "@@EN $1 $2"
  echo "@@EB $B"
  [ -n "$X" ] && docker inspect -f '{{range .Mounts}}@@EM {{.Source}}|{{.Destination}}{{"\n"}}{{end}}' $1 2>/dev/null
  $X $B -T 2>&1 | head -c 200000
  echo '@@EC-BEGIN'
  for p in $($X $B -T 2>/dev/null | sed -n 's/^[[:space:]]*ssl_certificate[[:space:]][[:space:]]*\([^;]*\);.*/\1/p' | sort -u | head -n 30); do
    echo "@@EC $p"
    c=$($X cat "$p" 2>/dev/null)
    [ -n "$c" ] && { echo "$c" | openssl x509 -noout -enddate -issuer -subject -ext subjectAltName 2>/dev/null || echo "$c" | openssl x509 -noout -enddate -issuer -subject 2>/dev/null || true; }
  done
}
for n in $(docker ps --format '{{.Names}} {{.Image}}' 2>/dev/null | awk -v me="$N" '$2 ~ /(openresty|nginx)/ && $1 != me {print $1}'); do
  B=nginx; docker exec $n sh -c 'command -v nginx' >/dev/null 2>&1 || B=openresty
  X="docker exec $n" ext_one "$n" "$(docker inspect -f '{{.Config.Image}}' $n 2>/dev/null)"
done
if [ "$N" != host-nginx ] && command -v nginx >/dev/null 2>&1 && pgrep -x nginx >/dev/null 2>&1; then B=nginx X="" ext_one host-nginx "$(nginx -v 2>&1 | head -n1)"; fi
echo '@@ACME'; cat $W/accounts/acme.json 2>/dev/null
echo '@@DNS'; cat $W/accounts/dns.json 2>/dev/null
echo '@@CRON'; cat @CRON@ 2>/dev/null
echo '@@RENEW'; tail -n 30 $W/logs/renew.log 2>/dev/null
echo '@@HEALTH'
[ -d $W/secrets ] && echo "secrets $(stat -c %a $W/secrets 2>/dev/null || stat -f %Lp $W/secrets 2>/dev/null)"
find $W/certs -name privkey.pem ! -perm 600 2>/dev/null | sed 's/^/key /'
find $W/secrets -type f -perm /077 2>/dev/null | sed 's/^/loose /'
echo '@@END'
"#,
    )
    .replace("@CRON@", CRON_FILE)
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct CertMeta {
    /// acme | upload | self
    pub source: String,
    /// ACME 申请时的第一个域名(续期用)
    pub main: String,
    pub domains: Vec<String>,
    pub acct: Option<String>,
    pub ca: Option<String>,
    /// http | dns
    pub method: Option<String>,
    pub dns: Option<String>,
    pub auto: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct CertEntry {
    pub name: String,
    pub meta: CertMeta,
    pub domains: Vec<String>,
    pub issuer: String,
    /// 到期时间(Unix 秒)
    pub not_after: Option<i64>,
}

impl CertEntry {
    pub fn days_left(&self, now: i64) -> Option<i64> {
        self.not_after.map(|t| (t - now).div_euclid(86400))
    }
    /// 这个证书能不能给这个域名用(精确或泛域名)
    pub fn covers(&self, domain: &str) -> bool {
        self.domains.iter().any(|d| d == domain || d.strip_prefix("*.").is_some_and(|b| domain.split_once('.').is_some_and(|(_, rest)| rest == b)))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SiteEntry {
    /// 配置文件名(`a.conf` 或 `a.conf.off`)
    pub file: String,
    pub enabled: bool,
    /// Otter 生成的站点;None 是外部配置(只读,能看能改原文)
    pub spec: Option<SiteSpec>,
    /// 正文被手工改过
    pub modified: bool,
    pub text: String,
}

impl SiteEntry {
    pub fn title(&self) -> String {
        match &self.spec {
            Some(s) => s.domains().first().cloned().unwrap_or_else(|| s.name.clone()),
            None => self.file.trim_end_matches(".off").trim_end_matches(".conf").to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AcmeAcct {
    pub id: String,
    pub ca: String,
    pub email: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DnsAcct {
    pub id: String,
    pub name: String,
    pub provider: String,
}

/// 已有 nginx/OpenResty 里的一个 server 块(只读展示)
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ExtSite {
    pub names: Vec<String>,
    pub listen: Vec<String>,
    pub cert: Option<String>,
    /// 反向代理地址、静态目录、跳转目标之一
    pub target: String,
    /// 反向代理 | 静态站点 | 跳转 | 其他
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ExtCert {
    pub path: String,
    pub domains: Vec<String>,
    pub issuer: String,
    pub not_after: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExtNginx {
    /// 容器名或 host-nginx
    pub name: String,
    pub image: String,
    /// 能接管就是接管用的 Target,不能就是原因
    pub adopt: Result<Target, String>,
    pub sites: Vec<ExtSite>,
    pub certs: Vec<ExtCert>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct WebState {
    /// ok | down | none
    pub docker: String,
    pub layout: Option<u32>,
    /// 占用 80/443 的 (端口, 进程名)
    pub ports: Vec<(u16, String)>,
    /// 1panel | baota
    pub panel: Option<String>,
    /// 容器状态和镜像
    pub container: Option<(String, String)>,
    pub version: String,
    pub sites: Vec<SiteEntry>,
    pub certs: Vec<CertEntry>,
    pub acme: Vec<AcmeAcct>,
    pub dns: Vec<DnsAcct>,
    /// 续期 cron 的计划(如 `17 3 * * *`);没装是 None
    pub cron: Option<String>,
    pub renew_log: Vec<String>,
    /// 权限体检发现的问题
    pub health: Vec<String>,
    /// 正在管理的 OpenResty:Otter 自己装的,或接管的
    pub target: Target,
    /// 服务器上已有的、没有被接管的 nginx/OpenResty(容器或本机进程)
    pub ext: Vec<ExtNginx>,
}

impl WebState {
    pub fn installed(&self) -> bool {
        self.container.is_some()
    }
    pub fn running(&self) -> bool {
        self.container.as_ref().is_some_and(|c| c.0 == "running")
    }
    /// Otter 版本太旧,服务器上的布局更新:只读
    pub fn too_new(&self) -> bool {
        self.layout.is_some_and(|l| l > srvdata::LAYOUT)
    }
    /// 用到这个证书的站点
    pub fn cert_users(&self, cert: &str) -> Vec<String> {
        self.sites.iter().filter_map(|s| s.spec.as_ref()).filter(|s| s.uses_cert(cert)).map(|s| s.name.clone()).collect()
    }
    /// 服务器上已有 nginx/OpenResty 容器、还没管理时,自动接管用的目标(不需要用户确认)。
    /// 面板(1Panel/宝塔)环境:只管站点配置,证书和 HTTP 验证目录不碰
    pub fn auto_adopt_target(&self) -> Option<Target> {
        if self.installed() || self.too_new() || self.docker != "ok" {
            return None;
        }
        let mut t = self.ext.first()?.adopt.clone().ok()?;
        if self.panel.is_some() {
            t.certs = None;
            t.acme = None;
        }
        Some(t)
    }
    /// 安装前的阻碍;None 是可以装
    pub fn install_blocker(&self) -> Option<String> {
        if self.docker == "none" {
            return Some("这台服务器没有安装 Docker,请先到「容器」页一键安装".into());
        }
        if self.docker == "down" {
            return Some("Docker 没有在运行,或当前账号没有权限使用".into());
        }
        if let Some(p) = &self.panel {
            return Some(format!("检测到 {} 面板:它管理的网站这里只读,为避免抢占 80/443,不在这台机器上安装", if p == "1panel" { "1Panel" } else { "宝塔" }));
        }
        if self.too_new() {
            return Some("服务器上的数据目录比这个版本的 Otter 新,请先升级 Otter".into());
        }
        if let Some(e) = self.ext.first() {
            return Some(format!("这台服务器上已有 {}({}),它占着 80/443。这里只读展示它的站点和证书;要改请到原来的管理方式里改", e.name, e.image));
        }
        if let Some((p, n)) = self.ports.first() {
            return Some(format!("{p} 端口已被 {} 占用,先停掉它再安装", if n == "?" { "其他程序" } else { n }));
        }
        None
    }
}

fn parse_end_date(s: &str) -> Option<i64> {
    // notAfter=Mar  5 12:00:00 2027 GMT
    let t = s.trim().strip_prefix("notAfter=")?.split_whitespace().collect::<Vec<_>>();
    if t.len() < 4 {
        return None;
    }
    let text = format!("{} {} {} {}", t[0], t[1], t[2], t[3]);
    chrono::NaiveDateTime::parse_from_str(&text, "%b %d %H:%M:%S %Y").ok().map(|d| d.and_utc().timestamp())
}

fn parse_san(s: &str) -> Vec<String> {
    let mut v = Vec::new();
    for p in s.split(',') {
        if let Some(d) = p.trim().strip_prefix("DNS:") {
            let d = d.trim().to_lowercase();
            if !d.is_empty() && !v.contains(&d) {
                v.push(d);
            }
        }
    }
    v
}

fn parse_meta(text: &str) -> CertMeta {
    let v: Value = serde_json::from_str(text.trim()).unwrap_or(Value::Null);
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    CertMeta {
        source: s("source").unwrap_or_default(),
        main: s("main").unwrap_or_default(),
        domains: v.get("domains").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        acct: s("acct"),
        ca: s("ca"),
        method: s("method"),
        dns: s("dns"),
        auto: v.get("auto").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn parse_cert(name: &str, block: &str) -> CertEntry {
    let (meta_text, x) = block.split_once("@@X\n").unwrap_or((block, ""));
    let meta = parse_meta(meta_text);
    let mut e = CertEntry { name: name.to_string(), meta, ..Default::default() };
    let lines: Vec<&str> = x.lines().collect();
    let mut san_next = false;
    for l in &lines {
        if l.starts_with("notAfter=") {
            e.not_after = parse_end_date(l);
        } else if let Some(i) = l.strip_prefix("issuer=") {
            e.issuer = cn_of(i);
        } else if let Some(sub) = l.strip_prefix("subject=") {
            if e.domains.is_empty() && !cn_of(sub).is_empty() {
                e.domains.push(cn_of(sub));
            }
        } else if l.contains("Subject Alternative Name") {
            san_next = true;
        } else if san_next {
            let san = parse_san(l);
            if !san.is_empty() {
                e.domains = san;
            }
            san_next = false;
        }
    }
    if e.domains.is_empty() {
        e.domains = e.meta.domains.clone();
    }
    e
}

/// `C=US, O=Let's Encrypt, CN=R11` / `CN = a.com` → 取 CN,没有就取 O
fn cn_of(s: &str) -> String {
    let find = |key: &str| {
        s.split(',').filter_map(|p| p.trim().split_once('=')).find(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string())
    };
    find("CN").or_else(|| find("O")).unwrap_or_default()
}

pub fn parse_state(out: &str) -> WebState {
    let mut st = WebState::default();
    let mut sections: Vec<(String, String)> = Vec::new();
    for l in out.lines() {
        if let Some(n) = l.strip_prefix("@@")
            && !n.starts_with("F ")
            && !n.starts_with("C ")
            && !(n.starts_with('E') && n != "EXT" && n != "END")
            && n != "X"
        {
            sections.push((n.trim().to_string(), String::new()));
            continue;
        }
        if let Some((_, t)) = sections.last_mut() {
            t.push_str(l);
            t.push('\n');
        }
    }
    for (name, text) in sections {
        match name.as_str() {
            "DOCKER" => st.docker = text.trim().to_string(),
            "LAYOUT" => st.layout = srvdata::parse_layout(&text),
            "TARGET" => st.target = Target::from_env(&text).unwrap_or_default(),
            "PORTS" => {
                st.ports = text.lines().filter_map(|l| l.split_once(' ')).filter_map(|(p, n)| Some((p.parse().ok()?, n.trim().to_string()))).collect();
            }
            "PANEL" => st.panel = text.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string),
            "CONTAINER" => st.container = text.trim().split_once(' ').map(|(a, b)| (a.to_string(), b.to_string())),
            "VERSION" => st.version = text.trim().rsplit('/').next().unwrap_or("").trim().to_string(),
            "SITES" => {
                for blk in text.split("@@F ").skip(1) {
                    let (file, content) = blk.split_once('\n').unwrap_or((blk, ""));
                    let file = file.trim().to_string();
                    if content.contains("# otter:default") {
                        continue;
                    }
                    let parsed = parse_site(content);
                    // 外部配置里没有 server 块(只放 map/upstream/日志格式等全局设置),或只有兜底的默认 server(server_name _)的文件不是站点,不列出
                    if parsed.is_none() {
                        let srv = parse_nginx_servers(content);
                        if srv.is_empty() || srv.iter().all(|x| !x.names.is_empty() && x.names.iter().all(|n| n == "_")) {
                            continue;
                        }
                    }
                    st.sites.push(SiteEntry { enabled: !file.ends_with(".off"), file, modified: parsed.as_ref().is_some_and(|p| p.1), spec: parsed.map(|p| p.0), text: content.trim_end_matches('\n').to_string() + "\n" });
                }
            }
            "CERTS" => {
                for blk in text.split("@@C ").skip(1) {
                    let (n, content) = blk.split_once('\n').unwrap_or((blk, ""));
                    st.certs.push(parse_cert(n.trim(), content));
                }
            }
            "EXT" => st.ext = parse_ext(&text),
            "ACME" => {
                let v: Value = serde_json::from_str(text.trim()).unwrap_or(Value::Null);
                st.acme = v.as_array().map(|a| a.iter().filter_map(|x| Some(AcmeAcct { id: x.get("id")?.as_str()?.to_string(), ca: x.get("ca")?.as_str()?.to_string(), email: x.get("email")?.as_str()?.to_string() })).collect()).unwrap_or_default();
            }
            "DNS" => {
                let v: Value = serde_json::from_str(text.trim()).unwrap_or(Value::Null);
                st.dns = v.as_array().map(|a| a.iter().filter_map(|x| Some(DnsAcct { id: x.get("id")?.as_str()?.to_string(), name: x.get("name")?.as_str()?.to_string(), provider: x.get("provider")?.as_str()?.to_string() })).collect()).unwrap_or_default();
            }
            "CRON" => {
                st.cron = text.lines().map(str::trim).find(|l| l.contains("renew.sh") && !l.starts_with('#')).map(|l| l.split_whitespace().take(5).collect::<Vec<_>>().join(" "));
            }
            "RENEW" => st.renew_log = text.lines().map(str::to_string).collect(),
            "HEALTH" => {
                for l in text.lines() {
                    if let Some(m) = l.strip_prefix("secrets ")
                        && m.trim() != "700"
                    {
                        st.health.push(format!("密钥目录权限是 {},应为 700", m.trim()));
                    } else if let Some(f) = l.strip_prefix("key ") {
                        st.health.push(format!("私钥权限过宽:{f}"));
                    } else if let Some(f) = l.strip_prefix("loose ") {
                        st.health.push(format!("密钥文件权限过宽:{f}"));
                    }
                }
            }
            _ => {}
        }
    }
    st.sites.sort_by(|a, b| a.title().cmp(&b.title()));
    st.certs.sort_by(|a, b| a.name.cmp(&b.name));
    st
}


// ---------------------------------------------------------------- 已有 nginx 配置的只读解析

#[derive(Debug)]
struct NgxNode {
    words: Vec<String>,
    children: Option<Vec<NgxNode>>,
}

fn ngx_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q: Option<char> = None;
    let mut chars = text.chars().peekable();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(std::mem::take(cur));
        }
    };
    while let Some(ch) = chars.next() {
        if let Some(qc) = q {
            if ch == qc {
                q = None;
            } else {
                cur.push(ch);
            }
            continue;
        }
        match ch {
            '#' => {
                flush(&mut cur, &mut out);
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            '"' | '\'' => q = Some(ch),
            ';' | '{' | '}' => {
                flush(&mut cur, &mut out);
                out.push(ch.to_string());
            }
            c if c.is_whitespace() => flush(&mut cur, &mut out),
            c => cur.push(c),
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn ngx_parse(toks: &[String], i: &mut usize) -> Vec<NgxNode> {
    let mut nodes = Vec::new();
    let mut words: Vec<String> = Vec::new();
    while *i < toks.len() {
        let t = toks[*i].as_str();
        *i += 1;
        match t {
            ";" => {
                if !words.is_empty() {
                    nodes.push(NgxNode { words: std::mem::take(&mut words), children: None });
                }
            }
            "{" => {
                let ch = ngx_parse(toks, i);
                nodes.push(NgxNode { words: std::mem::take(&mut words), children: Some(ch) });
            }
            "}" => return nodes,
            w => words.push(w.to_string()),
        }
    }
    nodes
}

fn ngx_find<'a>(nodes: &'a [NgxNode], name: &str, out: &mut Vec<&'a NgxNode>) {
    for n in nodes {
        if n.words.first().map(String::as_str) == Some(name) {
            out.push(n);
        }
        if let Some(c) = &n.children {
            ngx_find(c, name, out);
        }
    }
}

/// 解析 `nginx -T` 输出里的 server 块(只取展示用的几项,不求完整)
pub fn parse_nginx_servers(text: &str) -> Vec<ExtSite> {
    let cleaned: String = text.lines().filter(|l| !l.starts_with("nginx:") && !l.starts_with("openresty:")).collect::<Vec<_>>().join("\n");
    let toks = ngx_tokens(&cleaned);
    let mut i = 0;
    let tree = ngx_parse(&toks, &mut i);
    let mut servers = Vec::new();
    ngx_find(&tree, "server", &mut servers);
    let mut out = Vec::new();
    for sv in servers {
        let Some(body) = &sv.children else { continue };
        let mut e = ExtSite::default();
        for n in body {
            let w = &n.words;
            match w.first().map(String::as_str) {
                Some("server_name") => e.names.extend(w[1..].iter().cloned()),
                Some("listen") if w.len() > 1 => e.listen.push(w[1..].join(" ")),
                Some("ssl_certificate") if w.len() > 1 && e.cert.is_none() => e.cert = Some(w[1].clone()),
                _ => {}
            }
        }
        let mut proxies = Vec::new();
        ngx_find(body, "proxy_pass", &mut proxies);
        let mut roots = Vec::new();
        ngx_find(body, "root", &mut roots);
        let mut rets = Vec::new();
        ngx_find(body, "return", &mut rets);
        if e.names == ["_"] && proxies.is_empty() {
            e.kind = "默认站点".into();
        } else if let Some(p) = proxies.first().and_then(|n| n.words.get(1)) {
            e.kind = "反向代理".into();
            e.target = p.clone();
        } else if let Some(r) = rets.iter().find(|n| matches!(n.words.get(1).map(String::as_str), Some("301" | "302" | "307" | "308"))).and_then(|n| n.words.get(2)) {
            e.kind = "跳转".into();
            e.target = r.clone();
        } else if let Some(r) = roots.first().and_then(|n| n.words.get(1)) {
            e.kind = "静态站点".into();
            e.target = r.clone();
        } else {
            e.kind = "其他".into();
        }
        // 只有 HTTP→HTTPS 跳转的 80 块通常和同名的 443 块成对,保留它们以便看到全貌
        out.push(e);
    }
    out
}

fn parse_ext(text: &str) -> Vec<ExtNginx> {
    let mut v = Vec::new();
    for blk in text.split("@@EN ").skip(1) {
        let (head, rest) = blk.split_once('\n').unwrap_or((blk, ""));
        let (name, image) = head.trim().split_once(' ').unwrap_or((head.trim(), ""));
        let (conf_all, certs) = rest.split_once("@@EC-BEGIN\n").unwrap_or((rest, ""));
        let mut bin = "nginx".to_string();
        let mut mounts: Vec<(String, String)> = Vec::new();
        let mut conf = String::new();
        for l in conf_all.lines() {
            if let Some(b) = l.strip_prefix("@@EB ") {
                bin = b.trim().to_string();
            } else if let Some(m) = l.strip_prefix("@@EM ") {
                if let Some((src, dst)) = m.split_once('|') {
                    mounts.push((src.trim().to_string(), dst.trim().to_string()));
                }
            } else {
                conf.push_str(l);
                conf.push('\n');
            }
        }
        let mut x = ExtNginx { name: name.to_string(), image: image.trim().to_string(), adopt: Err(String::new()), sites: parse_nginx_servers(&conf), certs: Vec::new() };
        for cb in certs.split("@@EC ").skip(1) {
            let (path, body) = cb.split_once('\n').unwrap_or((cb, ""));
            let c = parse_cert(path.trim(), &format!("@@X\n{body}"));
            x.certs.push(ExtCert { path: path.trim().to_string(), domains: c.domains, issuer: c.issuer, not_after: c.not_after });
        }
        x.adopt = if name == "host-nginx" { Err("这是本机直接运行的 nginx(不是容器),暂不支持接管".into()) } else { detect_target(name, &bin, &mounts, &x, &conf) };
        v.push(x);
    }
    v
}

fn under(path: &str, dir: &str) -> bool {
    dir != "/" && (path == dir || path.starts_with(&format!("{}/", dir.trim_end_matches('/'))))
}

/// 从容器的挂载和它的配置推断要管理哪些目录;站点配置目录找不到就不能接管,其余找不到的功能禁用
pub fn detect_target(name: &str, bin: &str, mounts: &[(String, String)], x: &ExtNginx, conf: &str) -> Result<Target, String> {
    let ms: Vec<&(String, String)> = mounts.iter().filter(|m| !m.0.is_empty() && m.1.starts_with('/')).collect();
    let conf_dir = ms.iter().find(|m| m.1 == "/etc/nginx/conf.d").or_else(|| ms.iter().find(|m| m.1.ends_with("/conf.d"))).ok_or("没找到站点配置目录(conf.d)的挂载:它的站点配置不在宿主机上,Otter 没法持久地管理")?;
    let cert_paths: Vec<&str> = x.certs.iter().map(|c| c.path.as_str()).chain(x.sites.iter().filter_map(|s| s.cert.as_deref())).collect();
    let certs = ms.iter().find(|m| cert_paths.iter().any(|p| under(p, &m.1))).or_else(|| ms.iter().find(|m| m.1.contains("cert") || m.1.contains("ssl")));
    let acme_root = acme_root(conf);
    let acme = ms.iter().find(|m| acme_root.as_deref().is_some_and(|r| under(r, &m.1))).or_else(|| ms.iter().find(|m| m.1.contains("acme") || m.1.contains("webroot")));
    let roots = site_roots(conf);
    let others: Vec<&&(String, String)> = ms.iter().filter(|m| m.1 != conf_dir.1 && Some(&m.1) != certs.map(|c| &c.1) && Some(&m.1) != acme.map(|c| &c.1) && !m.1.contains("log")).collect();
    let sites = others.iter().find(|m| roots.iter().any(|r| under(r, &m.1))).or_else(|| others.iter().find(|m| m.1.ends_with("/sites") || m.1.ends_with("/www") || m.1.contains("/www/") || m.1.contains("/html"))).copied();
    let t = Target {
        container: name.to_string(),
        bin: if bin == "openresty" { "openresty".into() } else { "nginx".into() },
        conf: conf_dir.0.clone(),
        certs: certs.map(|m| (m.0.clone(), m.1.clone())),
        acme: acme.map(|m| (m.0.clone(), m.1.clone())),
        sites: sites.map(|m| (m.0.clone(), m.1.clone())),
        adopted: true,
        ports: (80, 443),
    };
    t.env_text()?;
    Ok(t)
}

fn config_tree(text: &str) -> Vec<NgxNode> {
    let cleaned: String = text.lines().filter(|l| !l.starts_with("nginx:") && !l.starts_with("openresty:")).collect::<Vec<_>>().join("\n");
    let toks = ngx_tokens(&cleaned);
    let mut i = 0;
    ngx_parse(&toks, &mut i)
}

/// `location … acme-challenge … { root X; }` 里的 X
fn acme_root(conf: &str) -> Option<String> {
    let tree = config_tree(conf);
    let mut locs = Vec::new();
    ngx_find(&tree, "location", &mut locs);
    locs.into_iter().filter(|l| l.words.iter().any(|w| w.contains("acme-challenge"))).find_map(|l| {
        let mut r = Vec::new();
        ngx_find(l.children.as_deref().unwrap_or(&[]), "root", &mut r);
        r.first().and_then(|n| n.words.get(1).cloned())
    })
}

fn site_roots(conf: &str) -> Vec<String> {
    let tree = config_tree(conf);
    let mut v = Vec::new();
    ngx_find(&tree, "root", &mut v);
    v.into_iter().filter_map(|n| n.words.get(1).cloned()).collect()
}

/// 一次操作的结果:输出最后一行是 `OK` 就成功(前面的行是附带信息);出现 `ERR:` 取其后全部作为失败原因
pub fn parse_op(out: &str) -> Result<String, String> {
    let t = out.trim();
    if let Some(i) = t.find("ERR:") {
        let m = t[i + 4..].trim();
        return Err(if m.is_empty() { "未知错误".into() } else { m.to_string() });
    }
    match t.lines().last() {
        Some("OK") => Ok(t.strip_suffix("OK").unwrap_or("").trim().to_string()),
        _ => Err(if t.is_empty() { "没有返回结果".into() } else { t.to_string() }),
    }
}

// ---------------------------------------------------------------- 改动站点:事务脚本

/// 站点配置改动的事务外壳:拿锁 → 快照 conf.d → 执行 `actions` → `nginx -t` → reload;
/// 任一步失败就恢复快照并输出 `ERR:原因`。成功输出 `OK`。`actions` 里可用 `$C`(conf.d)、`$W`、`bk 文件名`(备份)。
pub fn tx_script(actions: &str) -> String {
    format!(
        "{}\n{}\n",
        srvdata::lock_prelude(),
        fill(
            r#"@PRE@; C=$CONF
docker inspect -f '{{.State.Running}}' $N 2>/dev/null | grep -q true || { echo 'ERR:OpenResty/nginx 容器没有在运行'; exit 0; }
mkdir -p @T@ $W/conf/backup
S=$(mktemp -d @T@/tx.XXXXXX) || { echo 'ERR:建不了临时目录'; exit 0; }
cp -a $C/. $S/
rb() { rm -rf $C/* $C/.[!.]* 2>/dev/null; cp -a $S/. $C/ 2>/dev/null; rm -rf $S; }
bk() { [ -f "$C/$1" ] && cp -p "$C/$1" "$W/conf/backup/$1.$(date +%s)"; return 0; }
fail() { rb; echo "ERR:$*"; exit 0; }
@ACTIONS@
o=$(docker exec $N $B -t 2>&1) || fail "$o"
o=$(docker exec $N $B -s reload 2>&1) || { rb; docker exec $N $B -s reload >/dev/null 2>&1; echo "ERR:$o"; exit 0; }
rm -rf $S
echo OK
"#
        )
        .replace("@ACTIONS@", actions)
    )
}

fn site_file(name: &str, enabled: bool) -> String {
    if enabled { format!("{name}.conf") } else { format!("{name}.conf.off") }
}

/// 保存站点(新建或覆盖):写配置文件,静态站点顺手建目录和占位首页
pub fn site_save_script(spec: &SiteSpec, enabled: bool, t: &Target) -> Result<String, String> {
    spec.check()?;
    if matches!(spec.kind, SiteKind::Static { .. }) && t.sites.is_none() {
        return Err("这个 OpenResty 没有挂载静态站点目录,建不了静态站点".into());
    }
    if !spec.certs().is_empty() && t.certs.is_none() {
        return Err("这个 OpenResty 没有挂载证书目录,配不了 HTTPS".into());
    }
    let file = site_file(&spec.name, enabled);
    let other = site_file(&spec.name, !enabled);
    let mut a = format!("bk {f}; bk {o}; rm -f \"$C/{o}\"; {w}\n", f = quote(&file), o = quote(&other), w = write_file_cmd(&format!("$C/{file}"), &render_site(spec, t), "644").replace(&format!("'$C/{file}'"), &format!("\"$C/{file}\"")));
    if matches!(spec.kind, SiteKind::Static { .. }) {
        let d = format!("$SITES/{}", spec.name);
        a += &format!("mkdir -p \"{d}\"; [ -e \"{d}/index.html\" ] || printf '<!doctype html><meta charset=utf-8><title>{n}</title><h1>{n}</h1>' > \"{d}/index.html\"\n", n = spec.name);
    }
    Ok(tx_script(&a))
}

/// 直接覆盖配置原文(编辑原文;文件名只允许 `xxx.conf` / `xxx.conf.off`)
pub fn site_raw_script(file: &str, text: &str) -> Result<String, String> {
    if !valid_conf_file(file) {
        return Err("配置文件名不合法".into());
    }
    let a = format!("bk {f}; {w}\n", f = quote(file), w = write_file_cmd(&format!("$C/{file}"), text, "644").replace(&format!("'$C/{file}'"), &format!("\"$C/{file}\"")));
    Ok(tx_script(&a))
}

pub fn valid_conf_file(f: &str) -> bool {
    let base = f.strip_suffix(".conf.off").or_else(|| f.strip_suffix(".conf"));
    base.is_some_and(valid_name)
}

pub fn site_delete_script(file: &str) -> Result<String, String> {
    if !valid_conf_file(file) {
        return Err("配置文件名不合法".into());
    }
    Ok(tx_script(&format!("bk {f}; rm -f \"$C/\"{f}\n", f = quote(file))))
}

pub fn site_toggle_script(file: &str, enable: bool) -> Result<String, String> {
    if !valid_conf_file(file) {
        return Err("配置文件名不合法".into());
    }
    let (from, to) = if enable { (file.to_string(), file.trim_end_matches(".off").to_string()) } else { (file.to_string(), format!("{}.off", file.trim_end_matches(".off"))) };
    Ok(tx_script(&format!("bk {f}; mv \"$C/\"{f} \"$C/\"{t}\n", f = quote(&from), t = quote(&to))))
}

// ---------------------------------------------------------------- 证书

fn meta_json(m: &CertMeta) -> String {
    serde_json::to_string_pretty(&json!({
        "source": m.source, "main": m.main, "domains": m.domains, "acct": m.acct, "ca": m.ca, "method": m.method, "dns": m.dns, "auto": m.auto,
        "created": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    }))
    .unwrap_or_default()
        + "\n"
}

fn reload_cmd() -> String {
    "docker exec $N $B -s reload >/dev/null 2>&1 || true".to_string()
}

/// 上传证书:校验格式、私钥匹配、未过期,再落盘
pub fn cert_upload_script(name: &str, cert_pem: &str, key_pem: &str, replace: bool) -> Result<String, String> {
    if !valid_name(name) {
        return Err("证书名只能用小写字母、数字、点、下划线和短横线".into());
    }
    if !cert_pem.contains("BEGIN CERTIFICATE") || !key_pem.contains("PRIVATE KEY") {
        return Err("要选择 PEM 格式的证书(含 BEGIN CERTIFICATE)和私钥(含 PRIVATE KEY)".into());
    }
    let meta = meta_json(&CertMeta { source: "upload".into(), auto: false, ..Default::default() });
    Ok(format!(
        "{}\n{}",
        srvdata::lock_prelude(),
        fill(
            r#"@PRE@; D=$CERTS/@NAME@
@GC@
@EXISTS@
mkdir -p @T@; T=$(mktemp -d @T@/c.XXXXXX) || { echo 'ERR:建不了临时目录'; exit 0; }
trap 'rm -rf $T' EXIT
@CERT@
@KEY@
chmod 600 $T/privkey.pem
openssl x509 -in $T/fullchain.pem -noout 2>/dev/null || { echo 'ERR:证书文件无法解析'; exit 0; }
a=$(openssl x509 -in $T/fullchain.pem -noout -pubkey 2>/dev/null | openssl sha256 2>/dev/null)
b=$(openssl pkey -in $T/privkey.pem -pubout 2>/dev/null | openssl sha256 2>/dev/null)
[ -n "$a" ] && [ "$a" = "$b" ] || { echo 'ERR:私钥和证书不匹配'; exit 0; }
openssl x509 -in $T/fullchain.pem -noout -checkend 0 >/dev/null 2>&1 || { echo 'ERR:这个证书已经过期'; exit 0; }
mkdir -p $D; mv -f $T/fullchain.pem $D/fullchain.pem; mv -f $T/privkey.pem $D/privkey.pem; chmod 600 $D/privkey.pem; chmod 644 $D/fullchain.pem
@META@
@RELOAD@
echo OK
"#,
        )
        .replace("@NAME@", name)
        .replace("@EXISTS@", if replace { "" } else { "[ -e $D ] && { echo 'ERR:已经有同名证书'; exit 0; }" })
        .replace("@CERT@", &write_file_cmd("$T/fullchain.pem", cert_pem, "644").replace("'$T/fullchain.pem'", "\"$T/fullchain.pem\""))
        .replace("@KEY@", &write_file_cmd("$T/privkey.pem", key_pem, "600").replace("'$T/privkey.pem'", "\"$T/privkey.pem\""))
        .replace("@META@", &write_file_cmd("$D/meta.json", &meta, "644").replace("'$D/meta.json'", "\"$D/meta.json\""))
        .replace("@RELOAD@", &reload_cmd())
    ))
}

/// 生成自签证书
pub fn cert_self_script(name: &str, domains: &[String], days: u32) -> Result<String, String> {
    if !valid_name(name) {
        return Err("证书名只能用小写字母、数字、点、下划线和短横线".into());
    }
    if domains.is_empty() || domains.iter().any(|d| !valid_domain(d)) {
        return Err("域名不合法".into());
    }
    let san = domains.iter().map(|d| format!("DNS:{d}")).collect::<Vec<_>>().join(",");
    let cnf = format!("[req]\ndistinguished_name=dn\nx509_extensions=v3\nprompt=no\n[dn]\nCN={}\n[v3]\nsubjectAltName={san}\n", domains[0]);
    let meta = meta_json(&CertMeta { source: "self".into(), main: domains[0].clone(), domains: domains.to_vec(), auto: false, ..Default::default() });
    Ok(format!(
        "{}\n{}",
        srvdata::lock_prelude(),
        fill(
            r#"@PRE@; D=$CERTS/@NAME@
@GC@
[ -e $D ] && { echo 'ERR:已经有同名证书'; exit 0; }
mkdir -p @T@; T=$(mktemp -d @T@/c.XXXXXX) || { echo 'ERR:建不了临时目录'; exit 0; }
trap 'rm -rf $T' EXIT
@CNF@
o=$(openssl req -x509 -newkey rsa:2048 -nodes -days @DAYS@ -keyout $T/privkey.pem -out $T/fullchain.pem -config $T/o.cnf 2>&1) || { echo "ERR:$o"; exit 0; }
mkdir -p $D; mv $T/fullchain.pem $D/fullchain.pem; mv $T/privkey.pem $D/privkey.pem; chmod 600 $D/privkey.pem; chmod 644 $D/fullchain.pem
@META@
echo OK
"#,
        )
        .replace("@NAME@", name)
        .replace("@DAYS@", &days.clamp(1, 3650).to_string())
        .replace("@CNF@", &write_file_cmd("$T/o.cnf", &cnf, "600").replace("'$T/o.cnf'", "\"$T/o.cnf\""))
        .replace("@META@", &write_file_cmd("$D/meta.json", &meta, "644").replace("'$D/meta.json'", "\"$D/meta.json\""))
    ))
}

/// 删除证书:被站点用着就拒绝
pub fn cert_delete_script(name: &str) -> Result<String, String> {
    if !valid_name(name) {
        return Err("证书名不合法".into());
    }
    Ok(format!(
        "{}\n{}",
        srvdata::lock_prelude(),
        fill(r#"@PRE@; D=$CERTS/@NAME@
@GC@
[ -d $D ] || { echo 'ERR:证书不存在'; exit 0; }
u=$(grep -l "$CM/@NAME@/" $CONF/*.conf $CONF/*.conf.off 2>/dev/null | head -n1)
[ -n "$u" ] && { echo "ERR:还有站点在用这个证书($(basename $u))"; exit 0; }
rm -rf $D
echo OK
"#)
        .replace("@NAME@", name)
    ))
}

// ---------------------------------------------------------------- 账号

pub fn dns_providers() -> [(&'static str, &'static str, [&'static str; 2]); 3] {
    [("cloudflare", "Cloudflare", ["CF_Token", ""]), ("aliyun", "阿里云", ["Ali_Key", "Ali_Secret"]), ("dnspod", "DNSPod", ["DP_Id", "DP_Key"])]
}

/// acme.sh 的 dns 插件名
pub fn dns_plugin(provider: &str) -> &'static str {
    match provider {
        "cloudflare" => "dns_cf",
        "aliyun" => "dns_ali",
        _ => "dns_dp",
    }
}

/// 保存 DNS 账号:密钥写进 `secrets/dns/<id>.env`(0600),索引写进 `accounts/dns.json`
pub fn dns_account_script(id: &str, provider: &str, name: &str, values: [&str; 2], all: &[DnsAcct]) -> Result<String, String> {
    if !valid_name(id) {
        return Err("账号 id 不合法".into());
    }
    let (_, _, keys) = dns_providers().into_iter().find(|p| p.0 == provider).ok_or("不支持的 DNS 服务商")?;
    let mut env = String::new();
    for (k, v) in keys.iter().zip(values) {
        if k.is_empty() {
            continue;
        }
        if v.trim().is_empty() || v.contains(['\n', '\r', '\'', '"', ' ']) {
            return Err("密钥不能为空,也不能含空格、引号或换行".into());
        }
        env += &format!("{k}={}\n", v.trim());
    }
    let mut list: Vec<Value> = all.iter().filter(|a| a.id != id).map(|a| json!({"id": a.id, "name": a.name, "provider": a.provider})).collect();
    list.push(json!({"id": id, "name": name.trim(), "provider": provider}));
    let idx = serde_json::to_string_pretty(&list).unwrap_or_default() + "\n";
    Ok(format!(
        "{}\n{}",
        srvdata::lock_prelude(),
        fill("W=@W@; umask 077; mkdir -p $W/secrets/dns $W/accounts; chmod 700 $W/secrets\n@ENV@\n@IDX@\necho OK\n")
            .replace("@ENV@", &write_file_cmd(&format!("{}/secrets/dns/{id}.env", srvdata::WEB), &env, "600"))
            .replace("@IDX@", &write_file_cmd(&format!("{}/accounts/dns.json", srvdata::WEB), &idx, "644"))
    ))
}

pub fn dns_account_delete_script(id: &str, all: &[DnsAcct]) -> Result<String, String> {
    if !valid_name(id) {
        return Err("账号 id 不合法".into());
    }
    let list: Vec<Value> = all.iter().filter(|a| a.id != id).map(|a| json!({"id": a.id, "name": a.name, "provider": a.provider})).collect();
    let idx = serde_json::to_string_pretty(&list).unwrap_or_default() + "\n";
    Ok(format!(
        "{}\nrm -f {}/secrets/dns/{id}.env\n{}\necho OK\n",
        srvdata::lock_prelude(),
        srvdata::WEB,
        write_file_cmd(&format!("{}/accounts/dns.json", srvdata::WEB), &idx, "644")
    ))
}

// ---------------------------------------------------------------- 申请证书(ACME)

#[derive(Clone, Debug, PartialEq)]
pub struct IssueReq {
    pub name: String,
    pub domains: Vec<String>,
    /// letsencrypt | zerossl
    pub ca: String,
    pub email: String,
    /// http | dns
    pub method: String,
    pub dns: Option<DnsAcct>,
    pub staging: bool,
    pub auto: bool,
}

pub fn acme_acct_id(ca: &str, email: &str) -> String {
    format!("{ca}-{}", srvdata::name_of_domain(&email.replace('@', "-at-")))
}

impl IssueReq {
    pub fn check(&self) -> Result<(), String> {
        if !valid_name(&self.name) {
            return Err("证书名只能用小写字母、数字、点、下划线和短横线".into());
        }
        if self.domains.is_empty() || self.domains.iter().any(|d| !valid_domain(d)) {
            return Err("域名不合法".into());
        }
        if !self.email.contains('@') || self.email.contains(char::is_whitespace) || self.email.contains(['\'', '"', ';', '$', '`', '\\']) {
            return Err("请填写有效的邮箱(用于接收到期通知)".into());
        }
        if !matches!(self.ca.as_str(), "letsencrypt" | "zerossl") {
            return Err("不支持的证书颁发机构".into());
        }
        if self.method == "http" && self.domains.iter().any(|d| d.starts_with("*.")) {
            return Err("泛域名只能用 DNS 验证".into());
        }
        if self.method == "dns" && self.dns.is_none() {
            return Err("DNS 验证要选一个 DNS 账号".into());
        }
        Ok(())
    }
    pub fn acct_id(&self) -> String {
        acme_acct_id(&self.ca, &self.email)
    }
}

/// 申请证书的长任务脚本(经 `job_start_script` 脱离运行)。`accts` 是现有 ACME 账号索引(会合并新账号)
pub fn issue_job_script(r: &IssueReq, accts: &[AcmeAcct]) -> Result<String, String> {
    r.check()?;
    let acct = r.acct_id();
    let mut list: Vec<Value> = accts.iter().filter(|a| a.id != acct).map(|a| json!({"id": a.id, "ca": a.ca, "email": a.email, "created": ""})).collect();
    list.push(json!({"id": acct, "ca": r.ca, "email": r.email, "created": chrono::Utc::now().format("%Y-%m-%d").to_string()}));
    let acct_json = serde_json::to_string_pretty(&list).unwrap_or_default() + "\n";
    let meta = meta_json(&CertMeta { source: "acme".into(), main: r.domains[0].clone(), domains: r.domains.clone(), acct: Some(acct.clone()), ca: Some(r.ca.clone()), method: Some(r.method.clone()), dns: r.dns.as_ref().map(|d| d.id.clone()), auto: r.auto });
    let dargs = r.domains.iter().map(|d| format!("-d {}", quote(d))).collect::<Vec<_>>().join(" ");
    let mode = match (&r.method[..], &r.dns) {
        ("dns", Some(d)) => format!("--dns {}", dns_plugin(&d.provider)),
        _ => "-w /webroot".to_string(),
    };
    let envf = r.dns.as_ref().filter(|_| r.method == "dns").map(|d| format!("--env-file \"$W/secrets/dns/{}.env\"", d.id)).unwrap_or_default();
    let body = fill(
        r#"set -e
@PRE@; CN=@NAME@; A=$W/secrets/acme/@ACCT@
[ "$CERTS" = NONE ] && { echo '这个 OpenResty 没有挂载证书目录,Otter 管不了它的证书'; exit 1; }
@GA@
AW=$ACME; [ "$AW" = NONE ] && AW=$W/acme-webroot
mkdir -p $A $CERTS/$CN $W/accounts $W/bin $W/logs; chmod 700 $W/secrets $A
echo "==> 准备 acme.sh 镜像"
docker image inspect @IMG@ >/dev/null 2>&1 || docker pull @IMG@
acme() { docker run --rm --entrypoint acme.sh -v "$A":/acme.sh -v "$AW":/webroot -v "$CERTS":/certs @ENVF@ @IMG@ "$@"; }
echo "==> 向 @CAN@ 申请证书(@MODE_TEXT@)"
acme --issue @DARGS@ @MODE@ --server @CA@ --accountemail @EMAIL@ -k 2048 --force @STAGING@
echo "==> 安装证书"
acme --install-cert -d @MAIN@ --fullchain-file /certs/$CN/fullchain.pem --key-file /certs/$CN/privkey.pem
chmod 600 $CERTS/$CN/privkey.pem; chmod 644 $CERTS/$CN/fullchain.pem
@META@
@ACCTJSON@
@RELOAD@
echo "==> 证书已签发"
"#,
    )
    .replace("@GA@", if r.method == "http" { "[ \"$ACME\" = NONE ] && { echo '这个 OpenResty 没有挂载验证目录(acme-challenge),不能用 HTTP 验证;请改用 DNS 验证'; exit 1; }" } else { "" })
    .replace("@NAME@", &r.name)
    .replace("@ACCT@", &acct)
    .replace("@IMG@", ACME_IMAGE)
    .replace("@ENVF@", &envf)
    .replace("@CAN@", if r.ca == "zerossl" { "ZeroSSL" } else { "Let's Encrypt" })
    .replace("@MODE_TEXT@", if r.method == "dns" { "DNS 验证" } else { "HTTP 验证" })
    .replace("@DARGS@", &dargs)
    .replace("@MODE@", &mode)
    .replace("@CA@", &r.ca)
    .replace("@EMAIL@", &quote(&r.email))
    .replace("@STAGING@", if r.staging { "--staging" } else { "" })
    .replace("@MAIN@", &quote(&r.domains[0]))
    .replace("@META@", &write_file_cmd("$CERTS/$CN/meta.json", &meta, "644").replace("'$CERTS/$CN/meta.json'", "\"$CERTS/$CN/meta.json\""))
    .replace("@ACCTJSON@", &write_file_cmd(&format!("{}/accounts/acme.json", srvdata::WEB), &acct_json, "644"))
    .replace("@RELOAD@", &reload_cmd());
    Ok(body)
}

/// 申请前的预检:服务器的公网 IP、各域名解析到哪、80 端口是谁在听
pub fn preflight_script(domains: &[String]) -> String {
    let mut s = String::from("echo \"IP $(curl -fsS --max-time 6 https://www.cloudflare.com/cdn-cgi/trace 2>/dev/null | sed -n 's/^ip=//p')\"\n");
    for d in domains.iter().filter(|d| valid_domain(d) && !d.starts_with("*.")) {
        s += &format!("echo \"R {d} $(getent ahostsv4 {} 2>/dev/null | awk '{{print $1}}' | sort -u | tr '\\n' ' ')\"\n", quote(d));
    }
    s += "echo \"P80 $(ss -ltnH 'sport = :80' 2>/dev/null | head -n1 | wc -l)\"\necho OK\n";
    s
}

/// 预检结果里的问题(不拦截,只提示)
pub fn preflight_warnings(out: &str, running: bool) -> Vec<String> {
    let mut ip = String::new();
    let mut w = Vec::new();
    let mut p80 = true;
    for l in out.lines() {
        if let Some(v) = l.strip_prefix("IP ") {
            ip = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("P80 ") {
            p80 = v.trim() != "0";
        } else if let Some(v) = l.strip_prefix("R ")
            && let Some((d, ips)) = v.split_once(' ')
        {
            let ips: Vec<&str> = ips.split_whitespace().collect();
            if ips.is_empty() {
                w.push(format!("{d} 还没有解析(没有 A 记录),HTTP 验证会失败"));
            } else if !ip.is_empty() && !ips.contains(&ip.as_str()) {
                w.push(format!("{d} 解析到 {},不是这台服务器的公网 IP {ip}(用了 CDN 或解析还没生效?)", ips.join("、")));
            }
        }
    }
    if !p80 || !running {
        w.push("80 端口没有服务在监听,HTTP 验证需要 OpenResty 在运行".into());
    }
    w
}

// ---------------------------------------------------------------- 续期

pub fn renew_sh() -> String {
    fill(
        r##"#!/bin/sh
# managed by otter (layout 1) —— 由 Otter 生成,改了会被覆盖;要自定义请复制一份并改 cron
@PRE@; LOG=$W/logs/renew.log
[ "$CERTS" = NONE ] && exit 0
AW=$ACME; [ "$AW" = NONE ] && AW=$W/acme-webroot
mkdir -p $W/logs
log() { echo "$(date '+%F %T') $*" >> "$LOG"; }
jget() { sed -n "s/.*\"$2\": *\"\([^\"]*\)\".*/\1/p" "$1" | head -n1; }
changed=0
for d in $CERTS/*/; do
  n=$(basename "$d"); m="$d/meta.json"
  [ -f "$m" ] || continue
  [ "$(jget "$m" source)" = acme ] || continue
  grep -q '"auto": *true' "$m" || continue
  openssl x509 -in "$d/fullchain.pem" -noout -checkend @SECS@ >/dev/null 2>&1 && continue
  main=$(jget "$m" main); acct=$(jget "$m" acct); dns=$(jget "$m" dns)
  [ -n "$main" ] && [ -n "$acct" ] || { log "$n 缺少续期信息,跳过"; continue; }
  envf=""; [ -n "$dns" ] && [ -f "$W/secrets/dns/$dns.env" ] && envf="--env-file $W/secrets/dns/$dns.env"
  if out=$(docker run --rm --entrypoint acme.sh -v "$W/secrets/acme/$acct":/acme.sh -v "$AW":/webroot -v "$CERTS":/certs $envf @IMG@ --renew -d "$main" --force 2>&1); then
    chmod 600 "$d/privkey.pem"; log "$n 续期成功"; changed=1
  else
    log "$n 续期失败: $(echo "$out" | tail -n 3 | tr '\n' ' ')"
  fi
done
if [ $changed = 1 ]; then
  if docker exec $N $B -t >/dev/null 2>&1 && docker exec $N $B -s reload >/dev/null 2>&1; then log "已重载 OpenResty"; else log "重载 OpenResty 失败,请手工检查"; fi
fi
exit 0
"##,
    )
    .replace("@SECS@", &(RENEW_DAYS * 86400).to_string())
    .replace("@IMG@", ACME_IMAGE)
}

/// 安装或更新续期脚本和 cron(每天一次;分钟数固定在 `minute`,小时在凌晨)
pub fn cron_script(minute: u32, hour: u32) -> String {
    let line = format!("# otter:cert-renew 由 Otter 管理:每天检查证书,不到 {RENEW_DAYS} 天到期的自动续期\n{} {} * * * root {RENEW_SH}\n", minute % 60, hour % 24);
    format!(
        "{}\nmkdir -p {w}/bin {w}/logs; {sh}\n{cron}\necho OK\n",
        srvdata::lock_prelude(),
        w = srvdata::WEB,
        sh = write_file_cmd(RENEW_SH, &renew_sh(), "755"),
        cron = write_file_cmd(CRON_FILE, &line, "644")
    )
}

pub fn cron_remove_script() -> String {
    format!("rm -f {CRON_FILE}; echo OK")
}

/// 立即续期:跑一遍 `renew.sh`(它只续快到期的);输出日志尾部
pub fn renew_now_script() -> String {
    format!("[ -x {RENEW_SH} ] || {{ echo 'ERR:还没有续期脚本'; exit 0; }}\n{RENEW_SH}\ntail -n 8 {w}/logs/renew.log 2>/dev/null\necho OK\n", w = srvdata::WEB)
}

// ---------------------------------------------------------------- 安装 OpenResty、容器控制

/// 安装 OpenResty 的长任务脚本(经 `job_start_script` 脱离运行)
pub fn install_job_script(spec: &InstallSpec) -> String {
    let mut limits = String::new();
    if !spec.cpus.is_empty() {
        limits += &format!(" --cpus {}", spec.cpus);
    }
    if !spec.memory.is_empty() {
        limits += &format!(" --memory {}", spec.memory.to_ascii_lowercase());
    }
    let target = spec.target().env_text().unwrap_or_default();
    fill(
        r#"set -e
W=@W@; N=@NAME@
echo "==> 初始化数据目录 @R@"
@INIT@
for p in @HTTP@ @HTTPS@; do
  l=$(ss -ltnpH "sport = :$p" 2>/dev/null | head -n1)
  if [ -n "$l" ] && ! docker ps --format '{{.Names}}' | grep -qx $N; then echo "端口 $p 已被占用:$l"; exit 1; fi
done
if docker ps -a --format '{{.Names}}' | grep -qx $N && ! docker inspect -f '{{index .Config.Labels "otter.managed"}}' $N 2>/dev/null | grep -qx websites; then echo "容器名 $N 已被别的容器占用"; exit 1; fi
echo "==> 写入配置"
[ -f $W/conf/nginx.conf ] || { @NGINX@; }
@DEFAULT@
@TARGET@
mkdir -p @DIR@
@COMPOSE@
echo "==> 拉取镜像 @IMAGE@"
docker pull @IMAGE@
echo "==> 启动容器"
cd @DIR@
if docker compose version >/dev/null 2>&1; then docker compose up -d
elif command -v docker-compose >/dev/null 2>&1; then docker-compose up -d
else
  docker rm -f $N >/dev/null 2>&1 || true
  docker run -d --name $N --network host --restart unless-stopped@LIMITS@ --label otter.managed=websites -v $W/conf/nginx.conf:/usr/local/openresty/nginx/conf/nginx.conf:ro -v $W/conf/conf.d:/etc/nginx/conf.d:ro -v $W/certs:/etc/otter/certs:ro -v $W/sites:/www/sites:ro -v $W/acme-webroot:/www/acme:ro -v $W/logs:/var/log/nginx @IMAGE@
fi
sleep 2
docker exec $N nginx -t
docker inspect -f '{{.State.Running}}' $N | grep -q true
echo "==> OpenResty 已启动"
"#,
    )
    .replace("@INIT@", &srvdata::init_script())
    .replace("@NGINX@", &write_file_cmd(&format!("{}/conf/nginx.conf", srvdata::WEB), &nginx_conf(), "644"))
    .replace("@DEFAULT@", &write_file_cmd(&format!("{}/conf/conf.d/00-default.conf", srvdata::WEB), &default_conf(spec.http, spec.https), "644"))
    .replace("@TARGET@", &write_file_cmd(&format!("{}/target.env", srvdata::WEB), &target, "644"))
    .replace("@COMPOSE@", &write_file_cmd(&format!("{}/compose.yml", spec.dir()), &spec.compose(), "644"))
    .replace("@DIR@", &quote(&spec.dir()))
    .replace("@NAME@", &spec.container)
    .replace("@HTTP@", &spec.http.to_string())
    .replace("@HTTPS@", &spec.https.to_string())
    .replace("@LIMITS@", &limits)
    .replace("@IMAGE@", &spec.image())
}

/// 接管一个已有的 nginx/OpenResty 容器:建 Otter 的数据目录,把接管信息写进 `target.env`
pub fn adopt_script(t: &Target) -> Result<String, String> {
    let env = t.env_text()?;
    Ok(fill(&format!(
        "{}\n{}\ndocker inspect -f '{{{{.State.Running}}}}' {} 2>/dev/null | grep -q true || {{ echo 'ERR:这个容器没有在运行'; exit 0; }}\n[ -d {} ] || {{ echo 'ERR:找不到站点配置目录'; exit 0; }}\n{}\necho OK\n",
        srvdata::lock_prelude(),
        srvdata::init_script(),
        quote(&t.container),
        quote(&t.conf),
        write_file_cmd(&format!("{}/target.env", srvdata::WEB), &env, "644")
    )))
}

/// 取消接管:只删 `target.env`,容器、配置、证书都不动
pub fn unadopt_script() -> String {
    format!("rm -f {}/target.env; echo OK", srvdata::WEB)
}

/// 容器控制:reload / restart / test / stop / start
pub fn ctl_script(action: &str, t: &Target) -> Result<String, String> {
    let n = quote(&t.container);
    let b = &t.bin;
    if t.adopted && action == "remove" {
        return Err("接管的容器不是 Otter 装的,不能在这里删除".into());
    }
    Ok(match action {
        "test" => format!("o=$(docker exec {n} {b} -t 2>&1) && {{ echo \"$o\"; echo OK; }} || echo \"ERR:$o\""),
        "reload" => format!("o=$(docker exec {n} {b} -s reload 2>&1) && echo OK || echo \"ERR:$o\""),
        "restart" => format!("o=$(docker restart {n} 2>&1) && echo OK || echo \"ERR:$o\""),
        "stop" => format!("o=$(docker stop {n} 2>&1) && echo OK || echo \"ERR:$o\""),
        "start" => format!("o=$(docker start {n} 2>&1) && echo OK || echo \"ERR:$o\""),
        // 卸载只删容器,配置、站点、证书都保留
        "remove" => format!("o=$(docker rm -f {n} 2>&1) && echo OK || echo \"ERR:$o\""),
        _ => return Err("不支持的操作".into()),
    })
}

/// 看日志:站点的访问/错误日志(`name` 为空是全局)的最后 `n` 行
pub fn log_script(name: Option<&str>, err: bool, n: u32) -> Result<String, String> {
    let f = match name {
        Some(s) if valid_name(s) => format!("{s}.{}.log", if err { "error" } else { "access" }),
        Some(_) => return Err("站点名不合法".into()),
        None => format!("{}.log", if err { "error" } else { "access" }),
    };
    // 日志在容器的 /var/log/nginx 里(自己装的也挂到了 websites/logs);容器没在运行时回落到宿主机文件
    Ok(format!("{}\nn={}; f={}\n(docker exec $N tail -n $n /var/log/nginx/$f 2>/dev/null || tail -n $n $W/logs/$f) 2>&1; echo OK", pre(), n.clamp(10, 1000), quote(&f)))
}

/// 修复权限(体检发现问题时,用户确认后执行)
pub fn fix_perms_script() -> String {
    format!("chmod 700 {w}/secrets; find {w}/secrets -type d -exec chmod 700 {{}} + 2>/dev/null; find {w}/secrets -type f -exec chmod 600 {{}} + 2>/dev/null; find {w}/certs -name privkey.pem -exec chmod 600 {{}} + 2>/dev/null; echo OK", w = srvdata::WEB)
}

// ---------------------------------------------------------------- 脱离运行的长任务

pub const JOB_DIR: &str = "/opt/otter/jobs/web";

/// 启动一个长任务:脚本写到 `jobs/web/run.sh`,脱离 SSH 连接在后台跑,输出进 `log`,退出码进 `status`。已有任务在跑就不启动
pub fn job_start_script(body: &str) -> String {
    let d = JOB_DIR;
    format!(
        r#"D={d}
if [ -f $D/pid ] && [ ! -f $D/status ] && ps -eo pgid= 2>/dev/null | awk -v g="$(cat $D/pid)" '$1==g{{f=1}} END{{exit !f}}'; then echo 'ERR:已经有一个任务在进行'; exit 0; fi
rm -rf $D; mkdir -p $D; chmod 755 $D
printf %s {} | base64 -d > $D/run.sh
: > $D/log; chmod 644 $D/log
run='echo $$ > {d}/pid; sh {d}/run.sh >> {d}/log 2>&1 < /dev/null; echo $? > {d}/status'
if command -v setsid >/dev/null 2>&1; then setsid nohup sh -c "$run" >/dev/null 2>&1 < /dev/null &
else nohup sh -c "$run" >/dev/null 2>&1 < /dev/null &
fi
sleep 1
echo OK
"#,
        quote(&b64(body.as_bytes()))
    )
}

/// 轮询:退出码、进程是否还在、从 `offset` 字节起的新日志
pub fn job_poll_script(offset: usize) -> String {
    let d = JOB_DIR;
    format!(
        "D={d}\necho \"ST=$(cat $D/status 2>/dev/null)\"\npid=$(cat $D/pid 2>/dev/null)\necho \"ALIVE=$([ -n \"$pid\" ] && ps -eo pgid= 2>/dev/null | awk -v g=\"$pid\" '$1==g{{f=1}} END{{exit !f}}' && echo 1 || echo 0)\"\necho \"HAS=$([ -d $D ] && echo 1 || echo 0)\"\necho '--LOG--'\ntail -c +{} $D/log 2>/dev/null\nexit 0\n",
        offset + 1
    )
}

pub fn job_cancel_script() -> String {
    format!("D={JOB_DIR}; pid=$(cat $D/pid 2>/dev/null); [ -n \"$pid\" ] && {{ kill -s TERM -- -$pid 2>/dev/null || kill -s TERM $pid 2>/dev/null; }}; sleep 1; echo OK")
}

pub fn job_clear_script() -> String {
    format!("rm -rf {JOB_DIR}; echo OK")
}

#[derive(Debug, Default, PartialEq)]
pub struct JobPoll {
    pub status: Option<i32>,
    pub alive: bool,
    /// 任务目录还在(服务器上有这个任务)
    pub has: bool,
    pub log: String,
}

pub fn parse_job_poll(raw: &str) -> JobPoll {
    let (head, log) = raw.split_once("--LOG--\n").unwrap_or((raw, ""));
    let kv: std::collections::HashMap<&str, &str> = head.lines().filter_map(|l| l.split_once('=')).collect();
    JobPoll { status: kv.get("ST").and_then(|v| v.trim().parse().ok()), alive: kv.get("ALIVE") == Some(&"1"), has: kv.get("HAS") == Some(&"1"), log: log.to_string() }
}

/// 把失败日志里常见的原因翻成中文
pub fn explain(log: &str) -> Option<&'static str> {
    let low = log.to_lowercase();
    let has = |k: &str| low.contains(k);
    if has("端口 80 已被占用") || has("端口 443 已被占用") {
        Some("80/443 端口被其他程序占用,先停掉它再试")
    } else if has("too many failed authorizations") || has("rate limit") || has("too many certificates") {
        Some("触发了 Let's Encrypt 的频率限制,请等一段时间再试,或先勾选「测试环境」试申请")
    } else if has("dns problem") || has("nxdomain") || has("no valid ip addresses") {
        Some("域名解析有问题:确认域名已经解析到这台服务器")
    } else if has("connection refused") || has("timeout during connect") || has("fetching http") && has("invalid response") {
        Some("验证服务器连不上这台机器的 80 端口:检查防火墙/安全组是否放行 80,域名是否解析到这里")
    } else if has("no space left") {
        Some("服务器磁盘空间不足")
    } else if has("pull access denied") || has("error pulling image") || has("toomanyrequests") || has("i/o timeout") && has("registry") {
        Some("拉取镜像失败:检查服务器的网络,或在「容器 → 配置」里设置镜像加速")
    } else if has("unauthorized") && has("dns") || has("invalid api key") || has("authentication failed") {
        Some("DNS 服务商的密钥无效或权限不足")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn proxy(name: &str) -> SiteSpec {
        let d = vec!["a.example.com".to_string(), "www.a.example.com".to_string()];
        SiteSpec { name: name.into(), kind: SiteKind::Proxy { target: "http://127.0.0.1:8080".into(), ws: true, host: true }, listeners: vec![ls(&d, 80, None, true), ls(&d, 443, Some("a.example.com"), false)], ipv6: false, hsts: true, adv: Advanced::default() }
    }

    fn ls(d: &[String], port: u16, cert: Option<&str>, force: bool) -> Listener {
        Listener { domains: d.to_vec(), port, cert: cert.map(str::to_string), force_https: force }
    }

    fn simple(name: &str, domain: &str, kind: SiteKind, cert: Option<&str>) -> SiteSpec {
        let d = vec![domain.to_string()];
        let mut listeners = vec![ls(&d, 80, None, false)];
        if cert.is_some() {
            listeners.push(ls(&d, 443, cert, false));
        }
        SiteSpec { name: name.into(), kind, listeners, ipv6: false, hsts: false, adv: Advanced::default() }
    }

    #[test]
    fn site_roundtrips_through_conf() {
        for spec in [
            proxy("a.example.com"),
            simple("blog", "blog.x.org", SiteKind::Static { spa: true, index: false }, None),
            simple("old", "old.x.org", SiteKind::Redirect { to: "https://new.x.org/".into(), code: 301 }, Some("c1")),
        ] {
            let t = render_site(&spec, &Target::own());
            let (back, modified) = parse_site(&t).expect("parse");
            assert_eq!(back, spec);
            assert!(!modified);
            // 手改正文后被发现
            let (_, m2) = parse_site(&format!("{t}# hand edit\n")).unwrap();
            assert!(m2);
        }
        assert!(parse_site("server { listen 80; }").is_none());
    }

    #[test]
    fn rendered_conf_has_expected_blocks() {
        let t = render_site(&proxy("a.example.com"), &Target::own());
        assert!(t.contains("listen 443 ssl;") && t.contains("ssl_certificate /etc/otter/certs/a.example.com/fullchain.pem;"));
        assert!(t.contains("return 301 https://$host$request_uri;"));
        assert!(t.contains("Strict-Transport-Security") && t.contains("$connection_upgrade") && t.contains("acme-challenge"));
        let mut http_only = proxy("b");
        http_only.listeners = vec![ls(&["a.example.com".to_string()], 80, None, false)];
        let t = render_site(&http_only, &Target::own());
        assert!(!t.contains("443") && t.contains("proxy_pass http://127.0.0.1:8080;"));
        let st = render_site(&simple("s", "s.x.org", SiteKind::Static { spa: true, index: true }, None), &Target::own());
        assert!(st.contains("root /www/sites/s;") && st.contains("try_files $uri $uri/ /index.html;") && st.contains("autoindex on;"));
    }

    #[test]
    fn multi_port_listeners_render_and_roundtrip() {
        let a = vec!["a.com".to_string()];
        let b = vec!["b.com".to_string(), "www.b.com".to_string()];
        let spec = SiteSpec {
            name: "multi".into(),
            kind: SiteKind::Proxy { target: "https://127.0.0.1:9000".into(), ws: false, host: true },
            listeners: vec![ls(&a, 80, None, true), ls(&a, 443, Some("a.com"), false), ls(&b, 8080, None, false), ls(&b, 8443, Some("b.com"), false)],
            ipv6: true,
            hsts: true,
            adv: Advanced::default(),
        };
        spec.check().unwrap();
        let t = render_site(&spec, &Target::own());
        let (back, modified) = parse_site(&t).unwrap();
        assert_eq!(back, spec);
        assert!(!modified);
        assert!(t.contains("listen 8080;") && t.contains("listen [::]:8080;") && t.contains("listen 8443 ssl;") && t.contains("listen [::]:443 ssl;"));
        assert!(t.contains("server_name b.com www.b.com;") && t.contains("ssl_certificate /etc/otter/certs/b.com/fullchain.pem;"));
        // ACME 验证路径只在 80 端口的监听里;强制 HTTPS 只作用于 a.com 的 80
        assert_eq!(t.matches("acme-challenge").count(), 1);
        assert_eq!(t.matches("return 301 https://$host$request_uri;").count(), 1);
        assert_eq!(t.matches("Strict-Transport-Security").count(), 2);
        assert_eq!(spec.domains(), vec!["a.com", "b.com", "www.b.com"]);
        assert!(spec.uses_cert("b.com") && !spec.uses_cert("c.com"));
    }

    /// 把多端口站点渲染到 `OTTER_DUMP_SITE` 指定的文件,配合真机 `nginx -t` 检查语法
    #[test]
    #[ignore]
    fn dump_multi_site() {
        let Ok(path) = std::env::var("OTTER_DUMP_SITE") else { return };
        let a = vec!["a.com".to_string()];
        let b = vec!["b.com".to_string(), "www.b.com".to_string()];
        let spec = SiteSpec { name: "multi".into(), kind: SiteKind::Proxy { target: "https://127.0.0.1:9000".into(), ws: true, host: true }, listeners: vec![ls(&a, 80, None, true), ls(&a, 443, Some("a.com"), false), ls(&b, 8080, None, false), ls(&b, 8443, Some("b.com"), false)], ipv6: true, hsts: true, adv: Advanced::default() };
        std::fs::write(path, render_site(&spec, &Target::own())).unwrap();
    }

    #[test]
    fn advanced_options_render_and_roundtrip() {
        let mut spec = proxy("adv");
        spec.adv = Advanced { max_body: Some("200m".into()), read_timeout: Some(600) };
        spec.check().unwrap();
        let t = render_site(&spec, &Target::own());
        let (back, modified) = parse_site(&t).unwrap();
        assert_eq!(back, spec);
        assert!(!modified);
        assert_eq!(t.matches("client_max_body_size 200m;").count(), 2);
        assert!(t.contains("proxy_read_timeout 600s;"));
        // 没填就不写
        let plain = render_site(&proxy("p"), &Target::own());
        assert!(!plain.contains("client_max_body_size") && plain.contains("proxy_read_timeout 300s;"));
        let mut bad = proxy("b");
        bad.adv.max_body = Some("10x".into());
        assert!(bad.check().is_err());
        bad.adv.max_body = Some("0".into());
        bad.adv.read_timeout = Some(0);
        assert!(bad.check().is_err());
        assert!(valid_size("100m") && valid_size("0") && valid_size("512K") && !valid_size("m") && !valid_size("1.5m") && !valid_size(""));
    }

    #[test]
    fn listener_checks() {
        let d = vec!["a.com".to_string()];
        let mk = |listeners: Vec<Listener>| SiteSpec { name: "x".into(), kind: SiteKind::Static { spa: false, index: false }, listeners, ipv6: false, hsts: false, adv: Advanced::default() };
        assert!(mk(vec![]).check().is_err());
        assert!(mk(vec![ls(&d, 0, None, false)]).check().unwrap_err().contains("端口"));
        assert!(mk(vec![ls(&[], 80, None, false)]).check().is_err());
        assert!(mk(vec![ls(&d, 80, None, false), ls(&d, 80, None, false)]).check().unwrap_err().contains("重复"));
        assert!(mk(vec![ls(&d, 80, None, false), ls(&d, 8080, None, false)]).check().is_ok());
        // 没有 443 的 SSL 监听时,强制 HTTPS 不生效,照常提供内容
        let t = render_site(&mk(vec![ls(&d, 80, None, true)]), &Target::own());
        assert!(!t.contains("return 301 https://$host") && t.contains("try_files"));
    }

    #[test]
    fn v1_sites_are_read_as_listeners() {
        let v1 = "# managed by otter (layout 1)\n# otter:site v1 name=a domains=a.com,www.a.com type=static spa=0 index=0 cert=a force=1 hsts=1\n# otter:hash 0\nserver {}\n";
        let (s, _) = parse_site(v1).unwrap();
        assert_eq!(s.listeners.len(), 2);
        assert_eq!((s.listeners[0].port, s.listeners[0].force_https, s.listeners[0].cert.clone()), (80, true, None));
        assert_eq!((s.listeners[1].port, s.listeners[1].cert.as_deref()), (443, Some("a")));
        assert_eq!(s.listeners[1].domains, vec!["a.com", "www.a.com"]);
        assert!(s.hsts && !s.ipv6);
        let v1_plain = v1.replace("cert=a force=1", "cert=- force=0");
        assert_eq!(parse_site(&v1_plain).unwrap().0.listeners.len(), 1);
    }

    #[test]
    fn validation_rejects_injection() {
        assert!(valid_domain("a.com") && valid_domain("*.a.com") && valid_domain("x-y.a.b.org"));
        assert!(!valid_domain("a") && !valid_domain("a.com;rm") && !valid_domain("-a.com") && !valid_domain("a..com") && !valid_domain("A.com") && !valid_domain("a.com/x"));
        assert!(valid_url("http://127.0.0.1:8080") && valid_url("https://x.org/path?a=1"));
        assert!(!valid_url("ftp://x") && !valid_url("http://x;y") && !valid_url("http://x y") && !valid_url("http://x/$a") && !valid_url("http://x/\"") && !valid_url("http://x}"));
        assert_eq!(split_domains("A.com, b.com\nA.com  c.org"), vec!["a.com", "b.com", "c.org"]);
        let mut bad = proxy("n");
        bad.kind = SiteKind::Proxy { target: "http://x;evil".into(), ws: false, host: true };
        assert!(bad.check().is_err());
        bad.name = "../x".into();
        assert!(bad.check().is_err());
    }

    const STATE: &str = "@@DOCKER\nok\n@@LAYOUT\n{\"layout\":1,\"by\":\"otter\"}\n@@PORTS\n80 nginx\n@@PANEL\n@@CONTAINER\nrunning openresty/openresty:alpine\n@@VERSION\nnginx version: openresty/1.27.1.1\n@@SITES\n@@F a.example.com.conf\n__SITE__\n@@F 00-default.conf\n# otter:default\nserver {}\n@@F legacy.conf\nserver { listen 81; }\n@@CERTS\n@@C a.example.com\n{\"source\":\"acme\",\"main\":\"a.example.com\",\"domains\":[\"a.example.com\"],\"acct\":\"letsencrypt-me-at-x-com\",\"auto\":true}\n@@X\nnotAfter=Mar  5 12:00:00 2027 GMT\nissuer=C = US, O = Let's Encrypt, CN = R11\nsubject=CN = a.example.com\nX509v3 Subject Alternative Name: \n    DNS:a.example.com, DNS:*.b.example.com\n@@C self1\n{\"source\":\"self\"}\n@@X\nnotAfter=Jan 10 00:00:00 2030 GMT\nissuer=CN = self1\nsubject=CN = self1\n@@ACME\n[{\"id\":\"letsencrypt-me-at-x-com\",\"ca\":\"letsencrypt\",\"email\":\"me@x.com\"}]\n@@DNS\n[{\"id\":\"cf\",\"name\":\"主域\",\"provider\":\"cloudflare\"}]\n@@CRON\n# otter:cert-renew x\n17 3 * * * root /opt/otter/websites/bin/renew.sh\n@@RENEW\n2027-01-01 03:17:01 a 续期成功\n@@HEALTH\nsecrets 755\nkey /opt/otter/websites/certs/x/privkey.pem\n@@END\n";

    #[test]
    fn state_is_parsed() {
        let site = render_site(&proxy("a.example.com"), &Target::own());
        let st = parse_state(&STATE.replace("__SITE__", site.trim_end()));
        assert_eq!(st.docker, "ok");
        assert_eq!(st.layout, Some(1));
        assert_eq!(st.ports, vec![(80, "nginx".to_string())]);
        assert!(st.installed() && st.running());
        assert_eq!(st.version, "1.27.1.1");
        // 默认站点不进列表;外部配置要在
        assert_eq!(st.sites.len(), 2);
        let s = st.sites.iter().find(|s| s.spec.is_some()).unwrap();
        assert_eq!(s.spec.as_ref().unwrap().name, "a.example.com");
        assert!(!s.modified && s.enabled);
        let ext = st.sites.iter().find(|s| s.spec.is_none()).unwrap();
        assert_eq!(ext.title(), "legacy");
        assert_eq!(st.certs.len(), 2);
        let c = &st.certs[0];
        assert_eq!(c.name, "a.example.com");
        assert_eq!(c.domains, vec!["a.example.com", "*.b.example.com"]);
        assert_eq!(c.issuer, "R11");
        assert!(c.meta.auto && c.meta.acct.as_deref() == Some("letsencrypt-me-at-x-com"));
        assert_eq!(c.not_after, Some(1804248000));
        assert!(c.covers("a.example.com") && c.covers("x.b.example.com") && !c.covers("b.example.com") && !c.covers("y.a.example.com"));
        assert_eq!(c.days_left(1804248000 - 86400 * 10 - 5), Some(10));
        assert_eq!(st.certs[1].domains, vec!["self1"]);
        assert_eq!(st.acme[0].email, "me@x.com");
        assert_eq!(st.dns[0].provider, "cloudflare");
        assert_eq!(st.cron.as_deref(), Some("17 3 * * *"));
        assert_eq!(st.renew_log.len(), 1);
        assert_eq!(st.health.len(), 2);
        assert_eq!(st.cert_users("a.example.com"), vec!["a.example.com"]);
        assert!(st.install_blocker().is_some());
    }

    #[test]
    fn install_blockers() {
        let mut st = WebState { docker: "ok".into(), ..Default::default() };
        assert!(st.install_blocker().is_none());
        st.ports = vec![(80, "apache2".into())];
        assert!(st.install_blocker().unwrap().contains("apache2"));
        st.ports.clear();
        st.panel = Some("1panel".into());
        assert!(st.install_blocker().unwrap().contains("1Panel"));
        st.panel = None;
        st.docker = "none".into();
        assert!(st.install_blocker().unwrap().contains("Docker"));
        st.docker = "ok".into();
        st.layout = Some(99);
        assert!(st.too_new() && st.install_blocker().is_some());
    }

    #[test]
    fn op_results() {
        assert_eq!(parse_op("OK\n"), Ok(String::new()));
        assert_eq!(parse_op("nginx: ok\nOK\n"), Ok("nginx: ok".into()));
        assert_eq!(parse_op("ERR:nginx: [emerg] bad\nline2"), Err("nginx: [emerg] bad\nline2".into()));
        assert!(parse_op("").is_err());
    }

    #[test]
    fn scripts_are_valid_shell() {
        let spec = proxy("a.example.com");
        let req = IssueReq { name: "a.example.com".into(), domains: vec!["a.example.com".into(), "*.a.example.com".into()], ca: "letsencrypt".into(), email: "me@x.com".into(), method: "dns".into(), dns: Some(DnsAcct { id: "cf".into(), name: "主域".into(), provider: "cloudflare".into() }), staging: true, auto: true };
        let scripts = vec![
            state_script(),
            site_save_script(&spec, true, &Target::own()).unwrap(),
            site_raw_script("x.conf", "server {}\n").unwrap(),
            site_delete_script("x.conf").unwrap(),
            site_toggle_script("x.conf", false).unwrap(),
            site_toggle_script("x.conf.off", true).unwrap(),
            cert_upload_script("c1", "-----BEGIN CERTIFICATE-----\nx\n", "-----BEGIN PRIVATE KEY-----\nx\n", false).unwrap(),
            cert_self_script("c2", &["a.com".into()], 365).unwrap(),
            cert_delete_script("c1").unwrap(),
            dns_account_script("cf", "cloudflare", "主域", ["tok", ""], &[]).unwrap(),
            dns_account_delete_script("cf", &[]).unwrap(),
            issue_job_script(&req, &[]).unwrap(),
            preflight_script(&["a.com".into()]),
            renew_sh(),
            cron_script(17, 3),
            renew_now_script(),
            install_job_script(&InstallSpec::default()),
            job_start_script("echo hi"),
            job_poll_script(10),
            job_cancel_script(),
            ctl_script("reload", &Target::own()).unwrap(),
            log_script(Some("a.com"), false, 100).unwrap(),
            fix_perms_script(),
        ];
        for (i, s) in scripts.iter().enumerate() {
            let o = Command::new("sh").arg("-n").arg("-c").arg(s).output().unwrap();
            assert!(o.status.success(), "script #{i} syntax: {}\n{s}", String::from_utf8_lossy(&o.stderr));
        }
        assert!(ctl_script("rm -rf", &Target::own()).is_err() && site_delete_script("../x.conf").is_err() && cert_delete_script("a b").is_err());
    }

    #[test]
    fn issue_request_is_checked() {
        let mut r = IssueReq { name: "a".into(), domains: vec!["a.com".into()], ca: "letsencrypt".into(), email: "me@x.com".into(), method: "http".into(), dns: None, staging: false, auto: true };
        assert!(r.check().is_ok());
        r.domains = vec!["*.a.com".into()];
        assert!(r.check().unwrap_err().contains("DNS"));
        r.method = "dns".into();
        assert!(r.check().is_err());
        r.method = "http".into();
        r.domains = vec!["a.com".into()];
        r.email = "bad".into();
        assert!(r.check().is_err());
        assert_eq!(acme_acct_id("letsencrypt", "me@x.com"), "letsencrypt-me-at-x.com");
        assert!(valid_name(&acme_acct_id("zerossl", "A.B+c@x.com")));
    }

    #[test]
    fn preflight_reports_problems() {
        let out = "IP 1.2.3.4\nR a.com 1.2.3.4 \nR b.com 9.9.9.9 \nR c.com \nP80 1\nOK\n";
        let w = preflight_warnings(out, true);
        assert_eq!(w.len(), 2);
        assert!(w[0].contains("b.com") && w[1].contains("c.com"));
        assert_eq!(preflight_warnings("IP 1.2.3.4\nP80 0\nOK", false).len(), 1);
    }

    #[test]
    fn job_poll_is_parsed() {
        let p = parse_job_poll("ST=0\nALIVE=0\nHAS=1\n--LOG--\nhello\nworld\n");
        assert_eq!(p, JobPoll { status: Some(0), alive: false, has: true, log: "hello\nworld\n".into() });
        let p = parse_job_poll("ST=\nALIVE=1\nHAS=1\n--LOG--\n");
        assert!(p.status.is_none() && p.alive);
        assert!(explain("Error: port 80 already allocated\n端口 80 已被占用:x").is_some());
        assert!(explain("all fine").is_none());
    }

    /// 站点事务:用假的 docker 在本机跑一遍,验证写入、nginx -t 失败时回滚、成功时 reload
    #[test]
    fn site_transaction_commits_and_rolls_back() {
        let base = std::env::temp_dir().join(format!("otter-web-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let root = base.join("opt");
        let w = root.join("websites");
        std::fs::create_dir_all(w.join("conf/conf.d")).unwrap();
        std::fs::create_dir_all(w.join("sites")).unwrap();
        // 假 docker:inspect 成功;exec nginx -t 在 conf.d 里出现 BAD 时失败;reload 记录次数
        let fake = format!(
            "#!/bin/sh\ncase \"$1\" in\n inspect) echo true;;\n exec) case \"$3 $4\" in\n  \"nginx -t\") if grep -rq BAD {c}; then echo 'nginx: [emerg] bad directive' >&2; exit 1; fi; echo ok;;\n  \"nginx -s\") echo r >> {r}/reloads;;\n esac;;\nesac\n",
            c = w.join("conf/conf.d").display(),
            r = base.display()
        );
        std::fs::write(bin.join("docker"), fake).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("docker"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = |script: &str| -> String {
            let s = script.replace("/opt/otter", root.to_str().unwrap());
            let o = Command::new("sh").arg("-c").arg(&s).env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())).output().unwrap();
            format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
        };
        // 新建静态站点
        let spec = simple("blog".into(), "blog.x.org", SiteKind::Static { spa: false, index: false }, None);
        let out = run(&site_save_script(&spec, true, &Target::own()).unwrap());
        assert_eq!(parse_op(&out), Ok(String::new()), "{out}");
        let conf = std::fs::read_to_string(w.join("conf/conf.d/blog.conf")).unwrap();
        assert_eq!(parse_site(&conf).unwrap().0, spec);
        assert!(w.join("sites/blog/index.html").exists());
        assert_eq!(std::fs::read_to_string(base.join("reloads")).unwrap().lines().count(), 1);
        // 写一份坏配置:nginx -t 失败,原文件恢复,不 reload
        let before = std::fs::read_to_string(w.join("conf/conf.d/blog.conf")).unwrap();
        let out = run(&site_raw_script("blog.conf", "BAD\n").unwrap());
        assert!(parse_op(&out).unwrap_err().contains("bad directive"), "{out}");
        assert_eq!(std::fs::read_to_string(w.join("conf/conf.d/blog.conf")).unwrap(), before);
        assert_eq!(std::fs::read_to_string(base.join("reloads")).unwrap().lines().count(), 1);
        // 备份留下了旧版本
        assert!(std::fs::read_dir(w.join("conf/backup")).unwrap().count() >= 1);
        // 停用、启用、删除
        assert!(parse_op(&run(&site_toggle_script("blog.conf", false).unwrap())).is_ok());
        assert!(w.join("conf/conf.d/blog.conf.off").exists() && !w.join("conf/conf.d/blog.conf").exists());
        assert!(parse_op(&run(&site_toggle_script("blog.conf.off", true).unwrap())).is_ok());
        assert!(w.join("conf/conf.d/blog.conf").exists());
        assert!(parse_op(&run(&site_delete_script("blog.conf").unwrap())).is_ok());
        assert!(!w.join("conf/conf.d/blog.conf").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 证书上传/自签/删除(本机有 openssl 才跑)
    #[test]
    fn cert_scripts_work_with_openssl() {
        if Command::new("openssl").arg("version").output().is_err() {
            return;
        }
        let base = std::env::temp_dir().join(format!("otter-cert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("opt");
        std::fs::create_dir_all(root.join("websites/conf/conf.d")).unwrap();
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("docker"), "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("docker"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = |script: &str| -> String {
            let s = script.replace("/opt/otter", root.to_str().unwrap());
            let o = Command::new("sh").arg("-c").arg(&s).env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())).output().unwrap();
            format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
        };
        let out = run(&cert_self_script("self1", &["a.example.com".into(), "*.b.example.com".into()], 30).unwrap());
        assert_eq!(parse_op(&out), Ok(String::new()), "{out}");
        let d = root.join("websites/certs/self1");
        let mode = std::fs::metadata(d.join("privkey.pem")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let cert = std::fs::read_to_string(d.join("fullchain.pem")).unwrap();
        let key = std::fs::read_to_string(d.join("privkey.pem")).unwrap();
        // 同名再建被拒;上传匹配的一对成功;不匹配的被拒
        assert!(parse_op(&run(&cert_self_script("self1", &["a.example.com".into()], 30).unwrap())).is_err());
        assert!(parse_op(&run(&cert_upload_script("up1", &cert, &key, false).unwrap())).is_ok());
        let out = run(&format!("openssl genrsa 2048 2>/dev/null"));
        let other_key = out;
        let err = parse_op(&run(&cert_upload_script("up2", &cert, &other_key, false).unwrap())).unwrap_err();
        assert!(err.contains("不匹配"), "{err}");
        assert!(!root.join("websites/certs/up2").exists());
        // 状态里读得出来
        let state = run(&state_script());
        let st = parse_state(&state);
        let c = st.certs.iter().find(|c| c.name == "self1").expect("cert listed");
        assert!(c.covers("a.example.com") && c.covers("x.b.example.com"), "{c:?}");
        assert!(c.not_after.is_some());
        // 被站点用着不能删
        std::fs::write(root.join("websites/conf/conf.d/s.conf"), "ssl_certificate /etc/otter/certs/self1/fullchain.pem;\n").unwrap();
        assert!(parse_op(&run(&cert_delete_script("self1").unwrap())).unwrap_err().contains("在用"));
        std::fs::remove_file(root.join("websites/conf/conf.d/s.conf")).unwrap();
        assert!(parse_op(&run(&cert_delete_script("self1").unwrap())).is_ok());
        assert!(!d.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn external_nginx_is_read() {
        let conf = "nginx: the configuration file /etc/nginx/nginx.conf syntax is ok\n# configuration file /etc/nginx/nginx.conf:\nhttp {\n  upstream u { server 10.0.0.1:80; }\n  server {\n    listen 80;\n    server_name a.com www.a.com;\n    return 301 https://$host$request_uri;\n  }\n  server {\n    listen 443 ssl;\n    server_name a.com;\n    ssl_certificate /certs/a/fullchain.pem; # x\n    location / { proxy_pass http://127.0.0.1:8080; }\n  }\n  server { listen 80 default_server; server_name _; root /usr/share/nginx/html; }\n}\n";
        let s = parse_nginx_servers(conf);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].kind, "跳转");
        assert_eq!(s[0].names, vec!["a.com", "www.a.com"]);
        assert_eq!((s[1].kind.as_str(), s[1].target.as_str(), s[1].cert.as_deref()), ("反向代理", "http://127.0.0.1:8080", Some("/certs/a/fullchain.pem")));
        assert_eq!(s[2].kind, "默认站点");
        let out = format!("@@DOCKER\nok\n@@EXT\n@@EN web nginx:alpine\n{conf}@@EC-BEGIN\n@@EC /certs/a/fullchain.pem\nnotAfter=Mar  5 12:00:00 2027 GMT\nissuer=CN = R3\n@@ACME\n");
        let st = parse_state(&out);
        assert_eq!(st.ext.len(), 1);
        assert_eq!(st.ext[0].sites.len(), 3);
        assert_eq!(st.ext[0].certs[0].path, "/certs/a/fullchain.pem");
        assert!(st.ext[0].certs[0].not_after.is_some());
        assert!(st.install_blocker().unwrap().contains("只读"));
    }

    fn fp_target() -> Target {
        Target { container: "fp-x".into(), bin: "openresty".into(), conf: "/srv/fp/conf.d".into(), certs: Some(("/srv/fp/certs".into(), "/etc/openresty/certs".into())), acme: None, sites: Some(("/srv/fp/www".into(), "/var/www/sites".into())), adopted: true, ports: (80, 443) }
    }

    #[test]
    fn target_env_roundtrips() {
        let t = fp_target();
        let back = Target::from_env(&t.env_text().unwrap()).unwrap();
        assert_eq!(back, t);
        let mut bad = fp_target();
        bad.conf = "/srv/a b".into();
        assert!(bad.env_text().is_err());
        bad = fp_target();
        bad.container = "x;rm".into();
        assert!(bad.env_text().is_err());
    }

    #[test]
    fn own_target_env_keeps_name_and_ports() {
        let spec = InstallSpec { container: "web-gw".into(), http: 8080, https: 8443, ..Default::default() };
        let back = Target::from_env(&spec.target().env_text().unwrap()).unwrap();
        assert!(!back.adopted);
        assert_eq!((back.container.as_str(), back.ports), ("web-gw", (8080, 8443)));
        // 没有端口记录的旧 target.env 回落到 80/443
        assert_eq!(Target::from_env("OT_CONTAINER=x\nOT_CONF=/a\n").unwrap().ports, (80, 443));
    }

    #[test]
    fn install_spec_validate_and_compose() {
        let ok = InstallSpec::default();
        assert!(ok.validate(&[], &[]).is_ok());
        let other = InstallSpec { container: "web-2".into(), ..ok.clone() };
        assert!(other.validate(&["otter-openresty".into()], &[]).unwrap_err().contains("堆栈"));
        assert!(InstallSpec { project: "web-2".into(), ..ok.clone() }.validate(&["otter-openresty".into()], &[]).unwrap_err().contains("容器"));
        assert_eq!(gen_install_name(&[]), "otter-openresty");
        let n = gen_install_name(&["otter-openresty".into()]);
        assert!(n.starts_with("otter-openresty-") && n.len() == "otter-openresty-".len() + 4);
        assert!(ok.validate(&[], &[443]).unwrap_err().contains("443"));
        assert!(InstallSpec { https: 80, ..ok.clone() }.validate(&[], &[]).is_err());
        assert!(InstallSpec { cpus: "x".into(), ..ok.clone() }.validate(&[], &[]).is_err());
        assert!(InstallSpec { version: "a b".into(), ..ok.clone() }.validate(&[], &[]).is_err());
        let y = InstallSpec { version: "1.27.1.2-alpine".into(), cpus: "1.5".into(), memory: "512M".into(), ..ok }.compose();
        assert!(y.contains("image: openresty/openresty:1.27.1.2-alpine") && y.contains("cpus: \"1.5\"") && y.contains("mem_limit: 512m") && y.contains("network_mode: host"));
        let plain = InstallSpec::default().compose();
        assert!(!plain.contains("cpus") && !plain.contains("mem_limit"));
    }

    #[test]
    fn parse_openresty_tags() {
        let j = r#"{"results":[{"name":"alpine"},{"name":"1.27.1.1-alpine"},{"name":"1.27.1.2-alpine"},{"name":"1.27.1.2-alpine-fat"},{"name":"jammy"}]}
{"results":[{"name":"1.25.3.2-alpine"},{"name":"1.27.1.1-3-alpine"},{"name":"1.31.9.1-alpine"}]}
403 forbidden"#;
        assert_eq!(parse_tags(j).unwrap(), vec!["alpine", "1.31.9.1-alpine", "1.27.1.2-alpine", "1.27.1.1-alpine", "1.25.3.2-alpine"]);
        assert!(parse_tags(r#"{"results":[{"name":"jammy"}]}"#).is_none());
    }

    #[test]
    fn custom_ports_in_site_render() {
        let t = Target { ports: (8080, 8443), ..Target::own() };
        let spec = SiteSpec { listeners: vec![ls(&["a.x.org".to_string()], 8080, None, true), ls(&["a.x.org".to_string()], 8443, Some("a"), false)], ..proxy("a.x.org") };
        let r = render_site(&spec, &t);
        assert!(r.contains("return 301 https://$host:8443$request_uri") && r.contains("acme-challenge"));
    }

    #[test]
    fn detect_target_from_mounts() {
        let x = ExtNginx { name: "fp-x".into(), image: "openresty".into(), adopt: Err(String::new()), sites: vec![], certs: vec![ExtCert { path: "/etc/openresty/certs/a/fullchain.pem".into(), domains: vec![], issuer: String::new(), not_after: None }] };
        let m = |a: &str, b: &str| (a.to_string(), b.to_string());
        let mounts = vec![m("/srv/fp/conf.d", "/etc/nginx/conf.d"), m("/srv/fp/certs", "/etc/openresty/certs")];
        let t = detect_target("fp-x", "openresty", &mounts, &x, "").unwrap();
        assert_eq!(t.conf, "/srv/fp/conf.d");
        assert_eq!(t.certs.as_ref().unwrap().0, "/srv/fp/certs");
        assert!(t.acme.is_none() && t.adopted);
        assert!(detect_target("fp-x", "openresty", &mounts[1..], &x, "").is_err());
    }

    #[test]
    fn adopted_render_and_ctl() {
        let t = fp_target();
        let r = render_site(&proxy("a.example.com"), &t);
        assert!(!r.contains("$connection_upgrade") && !r.contains(" main"));
        assert!(ctl_script("remove", &t).is_err());
        let a = adopt_script(&t).unwrap();
        assert!(a.contains("target.env") && a.contains(&srvdata::b64(t.env_text().unwrap().as_bytes())));
    }

    /// 用真机抓到的状态输出验证解析(`OTTER_STATE=文件 cargo test real_state -- --ignored --nocapture`)
    #[test]
    #[ignore]
    fn real_state() {
        let st = parse_state(&std::fs::read_to_string(std::env::var("OTTER_STATE").unwrap()).unwrap());
        println!("blocker={:?}", st.install_blocker());
        for x in &st.ext {
            println!("{} {} sites={} certs={}", x.name, x.image, x.sites.len(), x.certs.len());
            println!("ADOPT {:?}", x.adopt);
            for s in &x.sites {
                println!("  {:?}", s);
            }
        }
    }

    /// 接管流程的脚本(`OTTER_DUMP=目录 cargo test dump_adopt -- --ignored`),配合真机上的假「外部」容器
    #[test]
    #[ignore]
    fn dump_adopt() {
        let d = std::env::var("OTTER_DUMP").unwrap();
        let w = |n: &str, s: String| std::fs::write(format!("{d}/{n}.sh"), s).unwrap();
        let t = Target { container: "fp-fake".into(), bin: "nginx".into(), conf: "/opt/fake/conf.d".into(), certs: Some(("/opt/fake/certs".into(), "/etc/openresty/certs".into())), acme: Some(("/opt/fake/acme".into(), "/var/www/acme".into())), sites: Some(("/opt/fake/www".into(), "/var/www/sites".into())), adopted: true, ports: (80, 443) };
        w("a1-adopt", adopt_script(&t).unwrap());
        let proxy = simple("a.test".into(), "a.test", SiteKind::Proxy { target: "http://127.0.0.1:9".into(), ws: true, host: true }, None);
        w("a2-site", site_save_script(&proxy, true, &t).unwrap());
        w("a3-static", site_save_script(&simple("s.test".into(), "s.test", SiteKind::Static { spa: false, index: true }, None), true, &t).unwrap());
        w("a4-bad", site_raw_script("bad.conf", "server { this_is_invalid on; }\n").unwrap());
        w("a5-self", cert_self_script("a.test", &["a.test".to_string()], 30).unwrap());
        w("a6-ssl", site_save_script(&SiteSpec { hsts: true, ..simple("a.test", "a.test", proxy.kind.clone(), Some("a.test")) }, true, &t).unwrap());
        w("a7-certdel", cert_delete_script("a.test").unwrap());
        w("a8-ctl-remove", ctl_script("remove", &t).unwrap_or_else(|e| format!("echo '{e}'")));
        w("a9-state", state_script());
        w("aa-cron", cron_script(17, 3));
        let req = IssueReq { name: "a.test".into(), domains: vec!["a.test".into()], ca: "letsencrypt".into(), email: "x@example.com".into(), method: "http".into(), dns: None, staging: false, auto: true };
        w("ab-issue", issue_job_script(&req, &[]).unwrap());
        w("ac-unadopt", unadopt_script());
    }

    /// 把真实脚本写到 `$OTTER_DUMP` 目录,供在真机上跑(`cargo test dump_scripts -- --ignored`)
    #[test]
    #[ignore]
    fn dump_scripts() {
        let d = std::env::var("OTTER_DUMP").unwrap();
        let w = |n: &str, s: String| std::fs::write(format!("{d}/{n}.sh"), s).unwrap();
        w("01-install", install_job_script(&InstallSpec { project: "gw".into(), container: "web-gw".into(), http: 18080, https: 18443, cpus: "1".into(), memory: "256m".into(), ..Default::default() }));
        let spec = |name: &str, dom: &str, target: &str| simple(name.into(), dom, SiteKind::Proxy { target: target.into(), ws: true, host: true }, None);
        w("02-site-proxy", site_save_script(&spec("a.test", "a.test", "http://127.0.0.1:9"), true, &Target::own()).unwrap());
        w("03-site-static", site_save_script(&simple("s.test".into(), "s.test", SiteKind::Static { spa: false, index: true }, None), true, &Target::own()).unwrap());
        w("04-site-bad", site_raw_script("bad.conf", "server { listen 80; this_is_invalid on; }\n").unwrap());
        w("05-cert-self", cert_self_script("a.test", &["a.test".to_string()], 30).unwrap());
        w("06-site-ssl", site_save_script(&SiteSpec { hsts: true, ..simple("a.test", "a.test", spec("a.test", "a.test", "http://127.0.0.1:9").kind, Some("a.test")) }, true, &Target::own()).unwrap());
        w("07-cron", cron_script(17, 3));
        w("08-renew-now", renew_now_script());
        w("09-state", state_script());
        w("10-toggle-off", site_toggle_script("s.test.conf", false).unwrap());
        w("11-delete", site_delete_script("s.test.conf.off").unwrap());
        w("12-cert-del", cert_delete_script("a.test").unwrap());
        w("13-cron-rm", cron_remove_script());
        let req = IssueReq { name: "a.test".into(), domains: vec!["a.test".into()], ca: "letsencrypt".into(), email: "x@example.com".into(), method: "http".into(), dns: None, staging: false, auto: true };
        w("14-issue", issue_job_script(&req, &[]).unwrap());
        w("15-renew-sh", renew_sh());
        w("16-dns-acct", dns_account_script("cf1", "cloudflare", "测试", ["faketoken", ""], &[]).unwrap());
        let acct = DnsAcct { id: "cf1".into(), name: "测试".into(), provider: "cloudflare".into() };
        let req = IssueReq { name: "_wildcard_.a.test".into(), domains: vec!["*.a.test".into(), "a.test".into()], ca: "letsencrypt".into(), email: "x@example.com".into(), method: "dns".into(), dns: Some(acct), staging: false, auto: true };
        w("17-issue-dns", issue_job_script(&req, &[]).unwrap());
    }
}

#[cfg(test)]
mod tag_live {
    /// 真网络:`OTTER_LIVE=1 cargo test live_tags -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_tags() {
        println!("{:?}", super::fetch_tags_local());
    }
}
