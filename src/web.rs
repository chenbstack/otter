//! 网站管理的纯逻辑:站点配置的生成与解析、证书与账号的解析、读取状态和各种操作的脚本。
//! 页面在 `pages/websites.rs`;目录布局见 `srvdata.rs`;设计见 `docs/modules/websites.md`。
//! 所有脚本都以 root 执行(`sys_exec(root=true)`),路径一律取自 `srvdata`,名字先过 `valid_name`。

use serde_json::{Value, json};

use crate::srvdata::{self, b64, valid_name, write_file_cmd};
use crate::sysops::quote;

pub const CONTAINER: &str = "otter-openresty";
pub const ACME_IMAGE: &str = "neilpang/acme.sh";
pub mod conf;
pub mod global;
pub mod patch;

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
            if self.adopted { "接管的 OpenResty/nginx 容器;删掉这个文件会在下次读取时被自动重新接入" } else { "Otter 安装的 OpenResty:记录容器名和端口" },
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
    /// 反向代理:多个 location(路径 → 后端),后端可以是一组负载均衡的 upstream
    Proxy { locations: Vec<Location>, upstreams: Vec<Upstream> },
    Static { spa: bool, index: bool },
    Redirect { to: String, code: u16 },
}

/// location 的后端:一个地址,或本站点的一个后端组(upstream)
#[derive(Clone, Debug, PartialEq)]
pub enum Backend {
    Url(String),
    /// `https`:转发到组里的成员时用 https;`name`:后端组名
    Group { https: bool, name: String },
}

/// 反向代理的一个 location
#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    /// 以 `/` 开头;`exact` 为 true 时精确匹配(`location = /path`),否则前缀匹配
    pub path: String,
    pub exact: bool,
    pub backend: Backend,
    pub ws: bool,
    /// 透传原始 Host
    pub host: bool,
    /// 读取超时(秒);None 用站点的高级设置(默认 300)
    pub timeout: Option<u32>,
    /// 去掉路径前缀再转发(`/api/x` → 后端的 `/x`);只对前缀匹配且路径不是 `/` 时有意义
    pub strip: bool,
}

impl Location {
    pub fn new(path: &str, backend: Backend) -> Location {
        Location { path: path.into(), exact: false, backend, ws: true, host: true, timeout: None, strip: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Policy {
    #[default]
    RoundRobin,
    LeastConn,
    IpHash,
}

impl Policy {
    pub fn key(self) -> &'static str {
        match self {
            Policy::RoundRobin => "rr",
            Policy::LeastConn => "lc",
            Policy::IpHash => "ip",
        }
    }
    pub fn from_key(k: &str) -> Policy {
        match k {
            "lc" => Policy::LeastConn,
            "ip" => Policy::IpHash,
            _ => Policy::RoundRobin,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Policy::RoundRobin => "轮询",
            Policy::LeastConn => "最少连接",
            Policy::IpHash => "IP 哈希",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UServer {
    /// `host:port`
    pub addr: String,
    /// 1–100
    pub weight: u32,
    pub backup: bool,
}

/// 后端组(nginx `upstream`),只在本站点内使用;写进配置时名字带站点前缀
#[derive(Clone, Debug, PartialEq)]
pub struct Upstream {
    pub name: String,
    pub policy: Policy,
    pub servers: Vec<UServer>,
    /// 被动健康检查:`fail_timeout` 秒内失败 `max_fails` 次就暂时摘掉成员;None 用 nginx 默认
    pub max_fails: Option<u32>,
    pub fail_timeout: Option<u32>,
}

/// 后端组名:小写字母、数字、短横线、下划线
pub fn valid_group(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// location 路径:`/` 开头,只含字母数字和 `/._~-`
pub fn valid_loc_path(p: &str) -> bool {
    p.starts_with('/') && p.len() <= 200 && !p.contains("//") && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '~' | '-'))
}

/// 后端组成员 `host:port`(域名、IPv4 或 `[IPv6]`)
pub fn valid_server_addr(a: &str) -> bool {
    let Some((h, port)) = a.rsplit_once(':') else { return false };
    let host_ok = if h.starts_with('[') && h.ends_with(']') {
        h.len() > 2 && h[1..h.len() - 1].chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
    } else {
        !h.is_empty() && h.len() <= 253 && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    };
    host_ok && port.parse::<u16>().is_ok_and(|p| p > 0)
}

impl SiteKind {
    /// 单个后端的代理(新建站点的简单弹窗用):一个 `/` location
    pub fn proxy(target: &str, ws: bool, host: bool) -> SiteKind {
        SiteKind::Proxy { locations: vec![Location { ws, host, ..Location::new("/", Backend::Url(target.into())) }], upstreams: Vec::new() }
    }

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
    /// 反向代理读取超时(秒);None 不写,沿用全局默认(OpenResty 全局设置)
    pub read_timeout: Option<u32>,
    /// 默认站点:没匹配到域名的请求交给它(监听写 `default_server`)
    pub default_server: bool,
    /// 连接后端 / 向后端发送的超时(秒);None 沿用全局默认
    pub connect_timeout: Option<u32>,
    pub send_timeout: Option<u32>,
    /// 允许的 TLS 协议版本(`TLSv1`、`TLSv1.1`、`TLSv1.2`、`TLSv1.3`);None 用 TLSv1.2 + TLSv1.3
    pub tls_protocols: Option<Vec<String>>,
    /// `ssl_ciphers` 的值(只影响 TLS 1.2 及以下);None 用 nginx 默认
    pub ciphers: Option<String>,
    /// 关掉 HTTP/2(默认开)
    pub no_http2: bool,
    /// HTTP/3(QUIC):额外监听 UDP 的同一个端口并发 Alt-Svc;需要 nginx 带 http_v3_module,防火墙要放行 UDP
    pub http3: bool,
    /// OCSP Stapling(默认关;证书里没有 OCSP 地址的颁发机构不适用)
    pub ocsp: bool,
    /// HSTS 有效期(秒);None 一年
    pub hsts_age: Option<u32>,
    pub hsts_sub: bool,
    pub hsts_preload: bool,
    /// 压缩:None 沿用全局(Otter 装的 nginx.conf 里是开);级别 1–9;类型(空格分隔的 MIME)
    pub gzip: Option<bool>,
    pub gzip_level: Option<u8>,
    pub gzip_types: Option<String>,
    /// IP 访问控制:0 不限制 1 白名单(只允许列表) 2 黑名单(拒绝列表);列表是 IP 或 CIDR
    pub ip_mode: u8,
    pub ip_list: Vec<String>,
    /// 基本认证(用户名 + 密码)
    pub auth: Option<BasicAuth>,
    /// 自定义响应头(名, 值)
    pub headers: Vec<(String, String)>,
    /// 关闭访问日志
    pub log_off: bool,
    /// 错误日志级别;None 用 warn
    pub error_level: Option<String>,
    /// 自定义错误页:(状态码, 站内路径或完整 URL)
    pub err_pages: Vec<(u16, String)>,
    /// 可信代理(IP/CIDR)和取真实 IP 的请求头;列表为空就不启用
    pub real_ip: Vec<String>,
    pub real_ip_header: Option<String>,
    pub rewrites: Vec<Rewrite>,
    /// 静态站点的默认首页文件(空格分隔);None 用 `index.html index.htm`
    pub index_files: Option<String>,
    /// 静态资源缓存:扩展名(空格分隔)和时长(`30d`、`12h`、`max`)
    pub cache_exts: Option<String>,
    pub cache_time: Option<String>,
    /// 防盗链(静态站点):保护的扩展名、允许的来源域名、是否允许没有 Referer 的请求
    pub hotlink_exts: Option<String>,
    pub hotlink_allow: Vec<String>,
    pub hotlink_none: bool,
    pub limits: Limits,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct BasicAuth {
    pub realm: String,
    pub users: Vec<AuthUser>,
}

/// `pass` 只在新增用户或改密码时有值,不写进配置头;没有值就沿用服务器上 htpasswd 里的旧密码
#[derive(Clone, Debug, PartialEq, Default)]
pub struct AuthUser {
    pub name: String,
    pub pass: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Rewrite {
    pub regex: String,
    pub to: String,
    /// `last` `break` `redirect`(302) `permanent`(301)
    pub flag: String,
}

/// 流量限制:每个客户端 IP 的请求速率、并发连接、下载限速;`exempt` 里的 IP 不受限
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Limits {
    pub req_rate: Option<u32>,
    /// 速率单位是每分钟(否则每秒)
    pub req_min: bool,
    pub burst: u32,
    pub nodelay: bool,
    pub conn: Option<u32>,
    /// 单连接限速,如 `512k`、`2m`
    pub speed: Option<String>,
    /// 传输这么多之后才开始限速
    pub speed_after: Option<String>,
    pub exempt: Vec<String>,
}

impl Limits {
    pub fn active(&self) -> bool {
        self.req_rate.is_some() || self.conn.is_some() || self.speed.is_some()
    }
}

pub const REWRITE_FLAGS: [(&str, &str); 4] = [("last", "继续匹配"), ("break", "就地处理"), ("redirect", "302 跳转"), ("permanent", "301 跳转")];
pub const REAL_IP_HEADERS: [&str; 3] = ["X-Forwarded-For", "X-Real-IP", "CF-Connecting-IP"];

pub fn valid_ip_item(v: &str) -> bool {
    let (ip, pre) = match v.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (v, None),
    };
    let Ok(addr) = ip.parse::<std::net::IpAddr>() else { return false };
    pre.is_none_or(|p| p.parse::<u8>().is_ok_and(|n| n <= if addr.is_ipv4() { 32 } else { 128 }))
}

fn word_list_ok(v: &str, ok: impl Fn(&str) -> bool) -> bool {
    let w: Vec<&str> = v.split_whitespace().collect();
    !w.is_empty() && w.len() <= 30 && w.iter().all(|x| ok(x))
}

fn plain_ok(v: &str) -> bool {
    !v.is_empty() && v.len() <= 500 && !v.chars().any(|c| c.is_control() || matches!(c, '"' | '$' | '`')) && !v.ends_with('\\')
}

/// 重写规则的正则和目标允许 `$`(`$1`),其余同 `plain_ok`
fn re_ok(v: &str) -> bool {
    !v.is_empty() && v.len() <= 300 && !v.chars().any(|c| c.is_control() || matches!(c, '"' | '`')) && !v.ends_with('\\')
}

fn target_ok(v: &str) -> bool {
    v.len() <= 300 && !v.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '$' | ';' | '{' | '}' | '\\')) && (v.starts_with('/') || valid_url(v))
}

fn limit_size_ok(v: &str) -> bool {
    let d = v.trim_end_matches(['k', 'K', 'm', 'M']);
    !d.is_empty() && d.len() >= v.len().saturating_sub(1) && d.len() <= 7 && d.chars().all(|c| c.is_ascii_digit()) && d.parse::<u64>().is_ok_and(|n| n > 0)
}

pub fn valid_user(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'))
}

pub const TLS_VERSIONS: [&str; 4] = ["TLSv1", "TLSv1.1", "TLSv1.2", "TLSv1.3"];
/// 加密套件预设:(名称, 值);值为空 = 用 nginx 默认
pub const CIPHER_PRESETS: [(&str, &str); 3] = [
    ("默认", ""),
    ("现代", "ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305"),
    ("兼容", "HIGH:!aNULL:!MD5:!RC4"),
];

fn valid_ciphers(v: &str) -> bool {
    !v.is_empty() && v.len() <= 1024 && v.chars().all(|c| c.is_ascii_alphanumeric() || ":!+-_@=.".contains(c))
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
        for (v, what) in [(self.adv.read_timeout, "读取超时"), (self.adv.connect_timeout, "连接超时"), (self.adv.send_timeout, "发送超时")] {
            if v.is_some_and(|t| t == 0 || t > 86400) {
                return Err(format!("{what}要在 1–86400 秒之间"));
            }
        }
        if let Some(p) = &self.adv.tls_protocols
            && (p.is_empty() || p.iter().any(|v| !TLS_VERSIONS.contains(&v.as_str())))
        {
            return Err("HTTPS 至少要允许一个 TLS 协议版本".into());
        }
        if self.adv.hsts_age.is_some_and(|a| a == 0 || a > 63072000) {
            return Err("HSTS 有效期要在 1 秒到 2 年之间".into());
        }
        if self.adv.hsts_preload && (!self.adv.hsts_sub || self.adv.hsts_age.unwrap_or(31536000) < 31536000) {
            return Err("HSTS preload 要求包含子域名,且有效期至少一年".into());
        }
        if self.adv.ciphers.as_deref().is_some_and(|c| !valid_ciphers(c)) {
            return Err("HTTPS 加密套件只能写字母、数字和 : ! + - _ @ = .".into());
        }
        self.check_extras()?;
        match &self.kind {
            SiteKind::Proxy { locations, upstreams } => check_proxy(locations, upstreams),
            SiteKind::Redirect { to, code } if !valid_url(to) || !matches!(code, 301 | 302 | 307 | 308) => Err("跳转地址要以 http:// 或 https:// 开头".into()),
            _ => Ok(()),
        }
    }
}

impl SiteSpec {
    fn check_extras(&self) -> Result<(), String> {
        let a = &self.adv;
        let ips = |what: &str, v: &[String]| -> Result<(), String> {
            if v.len() > 200 {
                return Err(format!("{what}最多 200 条"));
            }
            match v.iter().find(|x| !valid_ip_item(x)) {
                Some(x) => Err(format!("{what}里的「{x}」不是 IP 或 CIDR(如 10.0.0.0/8)")),
                None => Ok(()),
            }
        };
        if a.gzip_level.is_some_and(|l| !(1..=9).contains(&l)) {
            return Err("压缩级别要在 1–9 之间".into());
        }
        if let Some(t) = &a.gzip_types
            && !word_list_ok(t, |x| x.contains('/') && x.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '+' | '-' | '*')))
        {
            return Err("压缩类型要写成 MIME,用空格分隔,如 text/css application/json".into());
        }
        ips("IP 访问控制", &a.ip_list)?;
        if a.ip_mode > 2 || (a.ip_mode != 0 && a.ip_list.is_empty()) {
            return Err("IP 访问控制至少要填一条".into());
        }
        if let Some(au) = &a.auth {
            if !plain_ok(&au.realm) || au.realm.len() > 64 {
                return Err("认证提示语不能为空,且不能含引号、$、反引号".into());
            }
            if au.users.is_empty() {
                return Err("基本认证至少要有一个用户".into());
            }
            for (i, u) in au.users.iter().enumerate() {
                if !valid_user(&u.name) {
                    return Err(format!("认证用户名「{}」只能用字母、数字和 . _ @ -(最长 32)", u.name));
                }
                if au.users[..i].iter().any(|x| x.name == u.name) {
                    return Err(format!("认证用户 {} 重复了", u.name));
                }
                if u.pass.as_ref().is_some_and(|p| p.len() < 6 || p.len() > 128 || p.chars().any(|c| c.is_control())) {
                    return Err(format!("用户 {} 的密码要 6–128 个字符,不能有控制字符", u.name));
                }
            }
        }
        if a.headers.len() > 30 {
            return Err("自定义响应头最多 30 条".into());
        }
        for (n, v) in &a.headers {
            if n.is_empty() || n.len() > 64 || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Err(format!("响应头名「{n}」只能用字母、数字和 -"));
            }
            if !plain_ok(v) {
                return Err(format!("响应头 {n} 的值不能为空,也不能含引号、$、反引号和换行"));
            }
        }
        if a.error_level.as_deref().is_some_and(|l| !matches!(l, "info" | "warn" | "error" | "crit")) {
            return Err("错误日志级别只能是 info、warn、error、crit".into());
        }
        for (i, (c, t)) in a.err_pages.iter().enumerate() {
            if !(400..=599).contains(c) {
                return Err("错误页的状态码要在 400–599 之间".into());
            }
            if a.err_pages[..i].iter().any(|(x, _)| x == c) {
                return Err(format!("状态码 {c} 的错误页重复了"));
            }
            if !target_ok(t) {
                return Err(format!("状态码 {c} 的错误页要写站内路径(以 / 开头)或 http(s):// 地址,不能含空格和引号"));
            }
        }
        ips("可信代理", &a.real_ip)?;
        if let Some(h) = &a.real_ip_header
            && (h.is_empty() || h.len() > 64 || !h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        {
            return Err("真实 IP 请求头名不合法".into());
        }
        if a.rewrites.len() > 30 {
            return Err("重写规则最多 30 条".into());
        }
        for r in &a.rewrites {
            if !re_ok(&r.regex) || !re_ok(&r.to) || !REWRITE_FLAGS.iter().any(|(f, _)| *f == r.flag) {
                return Err(format!("重写规则「{}」不合法:正则和目标不能为空,不能含引号、换行,标记只能是 last/break/redirect/permanent", r.regex));
            }
        }
        let word = |x: &str| x.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if let Some(v) = &a.index_files
            && !word_list_ok(v, word)
        {
            return Err("默认首页文件用空格分隔,只能写文件名".into());
        }
        let ext = |x: &str| x.chars().all(|c| c.is_ascii_alphanumeric());
        if let Some(v) = &a.cache_exts
            && !word_list_ok(v, ext)
        {
            return Err("缓存的扩展名用空格分隔,只能是字母数字(不带点)".into());
        }
        if let Some(t) = &a.cache_time
            && !(t == "max" || (t.len() >= 2 && t.len() <= 7 && t[..t.len() - 1].chars().all(|c| c.is_ascii_digit()) && t.ends_with(['s', 'm', 'h', 'd', 'w', 'M', 'y'])))
        {
            return Err("缓存时长写成 30d、12h、1y,或 max".into());
        }
        if a.cache_exts.is_some() != a.cache_time.is_some() {
            return Err("静态资源缓存要同时填扩展名和时长".into());
        }
        if let Some(v) = &a.hotlink_exts {
            if !word_list_ok(v, ext) {
                return Err("防盗链的扩展名用空格分隔,只能是字母数字(不带点)".into());
            }
            if a.hotlink_allow.iter().any(|d| !valid_domain(d)) || a.hotlink_allow.len() > 50 {
                return Err("防盗链的允许来源要填域名(可用 *. 开头),最多 50 个".into());
            }
        }
        if (a.cache_exts.is_some() || a.hotlink_exts.is_some() || a.index_files.is_some()) && !matches!(self.kind, SiteKind::Static { .. }) {
            return Err("静态资源缓存、防盗链和默认首页只用于静态站点".into());
        }
        let l = &a.limits;
        if l.req_rate.is_some_and(|r| r == 0 || r > 100000) || l.burst > 10000 {
            return Err("请求速率要在 1–100000 之间,突发不超过 10000".into());
        }
        if l.conn.is_some_and(|c| c == 0 || c > 10000) {
            return Err("并发连接数要在 1–10000 之间".into());
        }
        if l.speed.as_deref().is_some_and(|v| !limit_size_ok(v)) || l.speed_after.as_deref().is_some_and(|v| !limit_size_ok(v)) {
            return Err("限速要写成 512k、2m 这样".into());
        }
        ips("不限制的 IP", &l.exempt)?;
        Ok(())
    }
}

fn check_proxy(locations: &[Location], upstreams: &[Upstream]) -> Result<(), String> {
    if locations.is_empty() {
        return Err("反向代理至少要有一个 location".into());
    }
    let mut seen: Vec<(&str, bool)> = Vec::new();
    for l in locations {
        if !valid_loc_path(&l.path) {
            return Err(format!("location 路径不合法:{}(以 / 开头,只能含字母、数字和 / . _ ~ -)", l.path));
        }
        if seen.contains(&(l.path.as_str(), l.exact)) {
            return Err(format!("location {} 重复了", l.path));
        }
        seen.push((&l.path, l.exact));
        if l.timeout.is_some_and(|t| t == 0 || t > 86400) {
            return Err(format!("{} 的读取超时要在 1–86400 秒之间", l.path));
        }
        if l.strip && (l.exact || l.path == "/") {
            return Err(format!("{}:「去掉前缀」只对前缀匹配且不是 / 的路径有意义", l.path));
        }
        match &l.backend {
            Backend::Url(u) if !valid_url(u) => return Err(format!("{} 的后端地址要以 http:// 或 https:// 开头,不能有空格和特殊符号", l.path)),
            Backend::Group { name, .. } if !upstreams.iter().any(|u| &u.name == name) => return Err(format!("{} 用了不存在的后端组 {name}", l.path)),
            _ => {}
        }
    }
    let mut names: Vec<&str> = Vec::new();
    for u in upstreams {
        if !valid_group(&u.name) {
            return Err(format!("后端组名不合法:{}(小写字母、数字、- _)", u.name));
        }
        if names.contains(&u.name.as_str()) {
            return Err(format!("后端组 {} 重复了", u.name));
        }
        names.push(&u.name);
        if u.servers.is_empty() {
            return Err(format!("后端组 {} 至少要有一个成员", u.name));
        }
        if u.servers.iter().all(|s| s.backup) {
            return Err(format!("后端组 {} 不能全是备用成员", u.name));
        }
        for sv in &u.servers {
            if !valid_server_addr(&sv.addr) {
                return Err(format!("后端组 {} 的成员地址不合法:{}(写成 主机:端口)", u.name, sv.addr));
            }
            if !(1..=100).contains(&sv.weight) {
                return Err(format!("后端组 {} 的权重要在 1–100 之间", u.name));
            }
        }
        if u.max_fails.is_some_and(|v| v > 1000) || u.fail_timeout.is_some_and(|v| v == 0 || v > 86400) {
            return Err(format!("后端组 {} 的失败次数/摘除时间不合法", u.name));
        }
    }
    Ok(())
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

/// 字段内容转义:分隔符 `|` `;` `,` `~` 和 `%`
fn fesc(v: &str) -> String {
    v.replace('%', "%25").replace('|', "%7C").replace(';', "%3B").replace(',', "%2C").replace('~', "%7E")
}

fn funesc(v: &str) -> String {
    v.replace("%7E", "~").replace("%2C", ",").replace("%3B", ";").replace("%7C", "|").replace("%25", "%")
}

fn encode_locs(ls: &[Location]) -> String {
    let b = |v: bool| if v { "1" } else { "0" };
    ls.iter()
        .map(|l| {
            let be = match &l.backend {
                Backend::Url(u) => format!("u~{}", fesc(u)),
                Backend::Group { https, name } => format!("g~{}~{}", if *https { "https" } else { "http" }, fesc(name)),
            };
            format!("{}|{}|{}|{}|{}|{}|{}", fesc(&l.path), b(l.exact), be, b(l.ws), b(l.host), l.timeout.map(|t| t.to_string()).unwrap_or_default(), b(l.strip))
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn decode_locs(v: &str) -> Vec<Location> {
    v.split(';')
        .filter(|x| !x.is_empty())
        .filter_map(|x| {
            let f: Vec<&str> = x.split('|').collect();
            let [path, exact, be, ws, host, timeout, strip] = f[..] else { return None };
            let parts: Vec<&str> = be.split('~').collect();
            let backend = match parts[..] {
                ["u", u] => Backend::Url(funesc(u)),
                ["g", scheme, name] => Backend::Group { https: scheme == "https", name: funesc(name) },
                _ => return None,
            };
            Some(Location { path: funesc(path), exact: exact == "1", backend, ws: ws == "1", host: host == "1", timeout: timeout.parse().ok(), strip: strip == "1" })
        })
        .collect()
}

fn encode_ups(us: &[Upstream]) -> String {
    us.iter()
        .map(|u| {
            let svs: Vec<String> = u.servers.iter().map(|s| format!("{}~{}~{}", fesc(&s.addr), s.weight, if s.backup { "1" } else { "0" })).collect();
            format!("{}|{}|{}|{}|{}", fesc(&u.name), u.policy.key(), u.max_fails.map(|v| v.to_string()).unwrap_or_default(), u.fail_timeout.map(|v| v.to_string()).unwrap_or_default(), svs.join(","))
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn decode_ups(v: &str) -> Vec<Upstream> {
    v.split(';')
        .filter(|x| !x.is_empty())
        .filter_map(|x| {
            let f: Vec<&str> = x.split('|').collect();
            let [name, policy, mf, ft, svs] = f[..] else { return None };
            let servers = svs
                .split(',')
                .filter(|x| !x.is_empty())
                .filter_map(|x| {
                    let p: Vec<&str> = x.split('~').collect();
                    let [addr, w, bk] = p[..] else { return None };
                    Some(UServer { addr: funesc(addr), weight: w.parse().ok()?, backup: bk == "1" })
                })
                .collect();
            Some(Upstream { name: funesc(name), policy: Policy::from_key(policy), servers, max_fails: mf.parse().ok(), fail_timeout: ft.parse().ok() })
        })
        .collect()
}

fn header_line(s: &SiteSpec) -> String {
    let b = |v: bool| if v { "1" } else { "0" };
    let mut h = format!("# otter:site v3 name={}", esc(&s.name));
    match &s.kind {
        SiteKind::Proxy { locations, upstreams } => h += &format!(" type=proxy locs={} ups={}", esc(&encode_locs(locations)), esc(&encode_ups(upstreams))),
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
    if s.adv.default_server {
        h += " ds=1";
    }
    if let Some(v) = s.adv.connect_timeout {
        h += &format!(" ctimeout={v}");
    }
    if let Some(v) = s.adv.send_timeout {
        h += &format!(" stimeout={v}");
    }
    if let Some(v) = &s.adv.tls_protocols {
        h += &format!(" tlsv={}", esc(&v.join(",")));
    }
    if let Some(v) = &s.adv.ciphers {
        h += &format!(" ciphers={}", esc(v));
    }
    if s.adv.no_http2 {
        h += " h2=0";
    }
    if s.adv.http3 {
        h += " h3=1";
    }
    if s.adv.ocsp {
        h += " ocsp=1";
    }
    if let Some(v) = s.adv.hsts_age {
        h += &format!(" hsts_age={v}");
    }
    if s.adv.hsts_sub {
        h += " hsts_sub=1";
    }
    if s.adv.hsts_preload {
        h += " hsts_pre=1";
    }
    h += &extras_header(&s.adv);
    h
}

fn enc_list(v: &[String]) -> String {
    v.iter().map(|x| fesc(x)).collect::<Vec<_>>().join(",")
}

fn dec_list(v: &str) -> Vec<String> {
    v.split(',').filter(|x| !x.is_empty()).map(funesc).collect()
}

/// 后加的高级项:没设置的不写
fn extras_header(a: &Advanced) -> String {
    let mut h = String::new();
    let mut kv = |k: &str, v: &str| h += &format!(" {k}={}", esc(v));
    if let Some(g) = a.gzip {
        kv("gz", if g { "1" } else { "0" });
    }
    if let Some(v) = a.gzip_level {
        kv("gzl", &v.to_string());
    }
    if let Some(v) = &a.gzip_types {
        kv("gzt", v);
    }
    if a.ip_mode != 0 {
        kv("ipm", &a.ip_mode.to_string());
        kv("ipl", &enc_list(&a.ip_list));
    }
    if let Some(au) = &a.auth {
        kv("ar", &au.realm);
        kv("au", &enc_list(&au.users.iter().map(|u| u.name.clone()).collect::<Vec<_>>()));
    }
    if !a.headers.is_empty() {
        kv("hdr", &a.headers.iter().map(|(n, v)| format!("{}|{}", fesc(n), fesc(v))).collect::<Vec<_>>().join(";"));
    }
    if a.log_off {
        kv("logoff", "1");
    }
    if let Some(v) = &a.error_level {
        kv("errlog", v);
    }
    if !a.err_pages.is_empty() {
        kv("errp", &a.err_pages.iter().map(|(c, t)| format!("{c}|{}", fesc(t))).collect::<Vec<_>>().join(";"));
    }
    if !a.real_ip.is_empty() {
        kv("rip", &enc_list(&a.real_ip));
        if let Some(v) = &a.real_ip_header {
            kv("riph", v);
        }
    }
    if !a.rewrites.is_empty() {
        kv("rw", &a.rewrites.iter().map(|r| format!("{}|{}|{}", fesc(&r.regex), fesc(&r.to), r.flag)).collect::<Vec<_>>().join(";"));
    }
    if let Some(v) = &a.index_files {
        kv("idx", v);
    }
    if let (Some(e), Some(t)) = (&a.cache_exts, &a.cache_time) {
        kv("cexts", e);
        kv("ctime", t);
    }
    if let Some(e) = &a.hotlink_exts {
        kv("hexts", e);
        kv("hallow", &enc_list(&a.hotlink_allow));
        if a.hotlink_none {
            kv("hnone", "1");
        }
    }
    let l = &a.limits;
    if let Some(r) = l.req_rate {
        kv("lrq", &format!("{r}{}", if l.req_min { "m" } else { "s" }));
        kv("lrb", &l.burst.to_string());
        if l.nodelay {
            kv("lrn", "1");
        }
    }
    if let Some(c) = l.conn {
        kv("lcn", &c.to_string());
    }
    if let Some(v) = &l.speed {
        kv("lsp", v);
        if let Some(a) = &l.speed_after {
            kv("lspa", a);
        }
    }
    if !l.exempt.is_empty() {
        kv("lex", &enc_list(&l.exempt));
    }
    h
}

fn extras_parse(g: &dyn Fn(&str) -> String, flag: &dyn Fn(&str) -> bool, adv: &mut Advanced) {
    let some = |k: &str| Some(g(k)).filter(|v| !v.is_empty());
    adv.gzip = match g("gz").as_str() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    };
    adv.gzip_level = g("gzl").parse().ok();
    adv.gzip_types = some("gzt");
    adv.ip_mode = g("ipm").parse().unwrap_or(0);
    adv.ip_list = dec_list(&g("ipl"));
    if some("ar").is_some() || some("au").is_some() {
        adv.auth = Some(BasicAuth { realm: g("ar"), users: dec_list(&g("au")).into_iter().map(|name| AuthUser { name, pass: None }).collect() });
    }
    adv.headers = g("hdr").split(';').filter_map(|x| x.split_once('|')).map(|(n, v)| (funesc(n), funesc(v))).collect();
    adv.log_off = flag("logoff");
    adv.error_level = some("errlog");
    adv.err_pages = g("errp").split(';').filter_map(|x| x.split_once('|')).filter_map(|(c, t)| Some((c.parse().ok()?, funesc(t)))).collect();
    adv.real_ip = dec_list(&g("rip"));
    adv.real_ip_header = some("riph");
    adv.rewrites = g("rw")
        .split(';')
        .filter_map(|x| {
            let f: Vec<&str> = x.split('|').collect();
            let [re, to, fl] = f[..] else { return None };
            Some(Rewrite { regex: funesc(re), to: funesc(to), flag: fl.to_string() })
        })
        .collect();
    adv.index_files = some("idx");
    adv.cache_exts = some("cexts");
    adv.cache_time = some("ctime");
    adv.hotlink_exts = some("hexts");
    adv.hotlink_allow = dec_list(&g("hallow"));
    adv.hotlink_none = flag("hnone");
    let rq = g("lrq");
    let l = &mut adv.limits;
    if let Some(n) = rq.strip_suffix(['s', 'm']).and_then(|n| n.parse().ok()) {
        l.req_rate = Some(n);
        l.req_min = rq.ends_with('m');
        l.burst = g("lrb").parse().unwrap_or(0);
        l.nodelay = flag("lrn");
    }
    l.conn = g("lcn").parse().ok();
    l.speed = some("lsp");
    l.speed_after = some("lspa");
    l.exempt = dec_list(&g("lex"));
}

/// Otter 自己装的 OpenResty 里 conf.d 在容器内的位置(htpasswd 放在那里)
pub const CONF_MOUNT: &str = "/etc/nginx/conf.d";
pub const DEFAULT_GZIP_TYPES: &str = "text/plain text/css application/json application/javascript text/xml application/xml image/svg+xml";

/// `open`:站点有访问控制时,验证目录要放行(否则签发证书会被拦)
fn acme_loc(t: &Target, open: bool) -> String {
    match t.acme_mount() {
        Some(m) => format!("    location ^~ /.well-known/acme-challenge/ {{\n        root {m};\n        default_type text/plain;\n{}    }}\n", if open { "        auth_basic off;\n        allow all;\n" } else { "" }),
        None => String::new(),
    }
}

/// 后端组在配置里的名字:带站点名前缀,避免不同站点的组重名
fn upstream_name(site: &str, group: &str) -> String {
    format!("{site}_{group}")
}

fn proxy_location(s: &SiteSpec, l: &Location, tg: &Target) -> String {
    let (target, https) = match &l.backend {
        Backend::Url(u) => (u.clone(), u.starts_with("https://")),
        Backend::Group { https, name } => (format!("{}://{}", if *https { "https" } else { "http" }, upstream_name(&s.name, name)), *https),
    };
    // 去掉前缀:proxy_pass 带 URI(`/`),nginx 会把匹配到的前缀换成它
    let pass = if l.strip { format!("{}/", target.trim_end_matches('/')) } else { target };
    let mut t = format!("    location {}{} {{\n", if l.exact { "= " } else { "" }, l.path);
    t += &format!("        proxy_pass {pass};\n        proxy_http_version 1.1;\n");
    t += &format!("        proxy_set_header Host {};\n", if l.host { "$host" } else { "$proxy_host" });
    t += "        proxy_set_header X-Real-IP $remote_addr;\n        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n        proxy_set_header X-Forwarded-Proto $scheme;\n";
    if l.ws {
        // 自己装的 nginx.conf 里有 map $connection_upgrade;接管的不一定有,直接写死
        t += &format!("        proxy_set_header Upgrade $http_upgrade;\n        proxy_set_header Connection {};\n", if tg.adopted { "\"upgrade\"" } else { "$connection_upgrade" });
    }
    if https {
        t += "        proxy_ssl_server_name on;\n";
    }
    if let Some(v) = s.adv.connect_timeout {
        t += &format!("        proxy_connect_timeout {v}s;\n");
    }
    if let Some(v) = s.adv.send_timeout {
        t += &format!("        proxy_send_timeout {v}s;\n");
    }
    if let Some(v) = l.timeout.or(s.adv.read_timeout) {
        t += &format!("        proxy_read_timeout {v}s;\n");
    }
    t += "    }\n";
    t
}

fn upstream_blocks(s: &SiteSpec) -> String {
    let SiteKind::Proxy { upstreams, locations } = &s.kind else { return String::new() };
    let mut out = String::new();
    for u in upstreams.iter().filter(|u| locations.iter().any(|l| matches!(&l.backend, Backend::Group { name, .. } if name == &u.name))) {
        out += &format!("upstream {} {{\n", upstream_name(&s.name, &u.name));
        match u.policy {
            Policy::RoundRobin => {}
            Policy::LeastConn => out += "    least_conn;\n",
            Policy::IpHash => out += "    ip_hash;\n",
        }
        for sv in &u.servers {
            let mut line = format!("    server {}", sv.addr);
            if sv.weight != 1 {
                line += &format!(" weight={}", sv.weight);
            }
            if let Some(m) = u.max_fails {
                line += &format!(" max_fails={m}");
            }
            if let Some(f) = u.fail_timeout {
                line += &format!(" fail_timeout={f}s");
            }
            if sv.backup {
                line += " backup";
            }
            out += &line;
            out += ";\n";
        }
        out += "}\n\n";
    }
    out
}

fn locations(s: &SiteSpec, t: &Target) -> String {
    match &s.kind {
        SiteKind::Proxy { locations, .. } => locations.iter().map(|l| proxy_location(s, l, t)).collect::<String>(),
        SiteKind::Static { spa, index } => {
            let fallback = if *spa { "/index.html" } else { "=404" };
            format!("    root {}/{};\n    index {};\n    location / {{\n        try_files $uri $uri/ {fallback};\n{}    }}\n{}", t.sites_mount(), s.name, s.adv.index_files.as_deref().unwrap_or("index.html index.htm"), if *index { "        autoindex on;\n" } else { "" }, static_asset_locations(s))
        }
        SiteKind::Redirect { to, code } => format!("    location / {{\n        return {code} {}$request_uri;\n    }}\n", to.trim_end_matches('/')),
    }
}

/// 配置里用的名字后缀:站点名的哈希,避免 zone/变量在不同站点间重名
fn zone_id(s: &SiteSpec) -> String {
    format!("otter_{}", &fnv(&s.name)[..8])
}

/// 流量限制要用的 zone 和变量,写在站点文件里 server 之前(站点文件在 http 块里)
fn limit_blocks(s: &SiteSpec) -> String {
    let l = &s.adv.limits;
    if l.req_rate.is_none() && l.conn.is_none() {
        return String::new();
    }
    let z = zone_id(s);
    let mut out = String::new();
    let key = if l.exempt.is_empty() {
        "$binary_remote_addr".to_string()
    } else {
        out += &format!("geo ${z}_lim {{\n    default 1;\n{}}}\nmap ${z}_lim ${z}_key {{\n    1 $binary_remote_addr;\n    0 \"\";\n}}\n", l.exempt.iter().map(|x| format!("    {x} 0;\n")).collect::<String>());
        format!("${z}_key")
    };
    if let Some(r) = l.req_rate {
        out += &format!("limit_req_zone {key} zone={z}_req:10m rate={r}r/{};\n", if l.req_min { "m" } else { "s" });
    }
    if l.conn.is_some() {
        out += &format!("limit_conn_zone {key} zone={z}_conn:10m;\n");
    }
    out + "\n"
}

/// 站点级的各项高级指令(写在 server 块里、location 之前)
fn extras_server(s: &SiteSpec, tg: &Target) -> String {
    let a = &s.adv;
    let mut t = String::new();
    if !a.real_ip.is_empty() {
        for ip in &a.real_ip {
            t += &format!("    set_real_ip_from {ip};\n");
        }
        t += &format!("    real_ip_header {};\n    real_ip_recursive on;\n", a.real_ip_header.as_deref().unwrap_or("X-Forwarded-For"));
    }
    match a.ip_mode {
        1 => {
            for ip in &a.ip_list {
                t += &format!("    allow {ip};\n");
            }
            t += "    deny all;\n";
        }
        2 => {
            for ip in &a.ip_list {
                t += &format!("    deny {ip};\n");
            }
        }
        _ => {}
    }
    if let Some(au) = &a.auth {
        t += &format!("    auth_basic \"{}\";\n    auth_basic_user_file {}/{}.htpasswd;\n", au.realm, CONF_MOUNT, s.name);
    }
    let _ = tg;
    match a.gzip {
        Some(true) => {
            t += "    gzip on;\n    gzip_vary on;\n    gzip_proxied any;\n";
            if let Some(l) = a.gzip_level {
                t += &format!("    gzip_comp_level {l};\n");
            }
            t += &format!("    gzip_types {};\n", a.gzip_types.as_deref().unwrap_or(DEFAULT_GZIP_TYPES));
        }
        Some(false) => t += "    gzip off;\n",
        None => {}
    }
    for (n, v) in &a.headers {
        t += &format!("    add_header {n} \"{v}\" always;\n");
    }
    for (c, p) in &a.err_pages {
        t += &format!("    error_page {c} {p};\n");
    }
    let l = &a.limits;
    let z = zone_id(s);
    if let Some(r) = l.req_rate {
        let _ = r;
        t += &format!("    limit_req zone={z}_req{}{};\n    limit_req_status 429;\n", if l.burst > 0 { format!(" burst={}", l.burst) } else { String::new() }, if l.nodelay { " nodelay" } else { "" });
    }
    if let Some(c) = l.conn {
        t += &format!("    limit_conn {z}_conn {c};\n    limit_conn_status 429;\n");
    }
    if let Some(v) = &l.speed {
        if let Some(af) = &l.speed_after {
            t += &format!("    limit_rate_after {af};\n");
        }
        t += &format!("    limit_rate {v};\n");
    }
    for r in &a.rewrites {
        t += &format!("    rewrite \"{}\" \"{}\" {};\n", r.regex, r.to, r.flag);
    }
    t
}

/// 静态站点的缓存和防盗链:按扩展名的正则 location
fn static_asset_locations(s: &SiteSpec) -> String {
    let a = &s.adv;
    let re = |v: &str| format!("\\.({})$", v.split_whitespace().collect::<Vec<_>>().join("|"));
    let mut t = String::new();
    if let Some(e) = &a.hotlink_exts {
        t += &format!("    location ~* {} {{\n", re(e));
        let mut refs = String::new();
        if a.hotlink_none {
            refs += "none blocked ";
        }
        refs += "server_names";
        for d in &a.hotlink_allow {
            refs += &format!(" {d}");
        }
        t += &format!("        valid_referers {refs};\n        if ($invalid_referer) {{\n            return 403;\n        }}\n");
        if let Some(c) = &a.cache_time {
            t += &format!("        expires {c};\n");
        }
        t += "        try_files $uri =404;\n    }\n";
    }
    if let (Some(e), Some(c)) = (&a.cache_exts, &a.cache_time) {
        t += &format!("    location ~* {} {{\n        expires {c};\n        try_files $uri =404;\n    }}\n", re(e));
    }
    t
}

fn body(s: &SiteSpec, tg: &Target) -> String {
    let fmt = if tg.adopted { "" } else { " main" };
    let access = if s.adv.log_off { "    access_log off;\n".to_string() } else { format!("    access_log /var/log/nginx/{n}.access.log{fmt};\n", n = s.name) };
    let logs = format!("{access}    error_log /var/log/nginx/{n}.error.log {lv};\n", n = s.name, lv = s.adv.error_level.as_deref().unwrap_or("warn"));
    // 有 443 的 SSL 监听时,「强制 HTTPS」才有地方可跳
    let https_port = s.listeners.iter().find(|l| l.ssl()).map(|l| l.port);
    let has_443 = https_port.is_some();
    let https_at = match https_port {
        Some(443) | None => String::new(),
        Some(p) => format!(":{p}"),
    };
    let hsts_v = format!("max-age={}{}{}", s.adv.hsts_age.unwrap_or(31536000), if s.adv.hsts_sub { "; includeSubDomains" } else { "" }, if s.adv.hsts_preload { "; preload" } else { "" });
    let mut out: Vec<String> = Vec::new();
    for l in &s.listeners {
        let ssl = if l.ssl() { " ssl" } else { "" };
        let ds = if s.adv.default_server { " default_server" } else { "" };
        let mut t = format!("server {{\n    listen {}{ssl}{ds};\n", l.port);
        if s.ipv6 {
            t += &format!("    listen [::]:{}{ssl}{ds};\n", l.port);
        }
        if l.ssl() && s.adv.http3 {
            t += &format!("    listen {} quic;\n", l.port);
            if s.ipv6 {
                t += &format!("    listen [::]:{} quic;\n", l.port);
            }
            t += &format!("    add_header Alt-Svc 'h3=\":{}\"; ma=86400' always;\n", l.port);
        }
        if l.ssl() && !s.adv.no_http2 {
            t += "    http2 on;\n";
        }
        t += &format!("    server_name {};\n", l.domains.join(" "));
        if let Some(c) = &l.cert {
            let cm = tg.cert_mount();
            t += &format!("    ssl_certificate {cm}/{c}/fullchain.pem;\n    ssl_certificate_key {cm}/{c}/privkey.pem;\n    ssl_protocols {};\n", s.adv.tls_protocols.as_ref().map(|p| p.join(" ")).unwrap_or_else(|| "TLSv1.2 TLSv1.3".into()));
            if s.adv.ocsp {
                t += &format!("    ssl_stapling on;\n    ssl_stapling_verify on;\n    ssl_trusted_certificate {cm}/{c}/fullchain.pem;\n    resolver 1.1.1.1 8.8.8.8 valid=300s;\n    resolver_timeout 5s;\n");
            }
            if let Some(ci) = &s.adv.ciphers {
                t += &format!("    ssl_ciphers {ci};\n    ssl_prefer_server_ciphers on;\n");
            }
        }
        t += &logs;
        if let Some(b) = &s.adv.max_body {
            t += &format!("    client_max_body_size {b};\n");
        }
        if l.port == tg.ports.0 && !l.ssl() {
            t += &acme_loc(tg, s.adv.auth.is_some() || s.adv.ip_mode != 0);
        }
        if l.ssl() && s.hsts {
            t += &format!("    add_header Strict-Transport-Security \"{hsts_v}\" always;\n");
        }
        if !l.ssl() && l.force_https && has_443 {
            t += &format!("    location / {{\n        return 301 https://$host{https_at}$request_uri;\n    }}\n");
        } else {
            t += &extras_server(s, tg);
            t += &locations(s, tg);
        }
        t += "}\n";
        out.push(t);
    }
    limit_blocks(s) + &upstream_blocks(s) + &out.join("\n")
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
        if let Some(h) = l.strip_prefix("# otter:site v1 ").or_else(|| l.strip_prefix("# otter:site v2 ").or_else(|| l.strip_prefix("# otter:site v3 ")).inspect(|_| v2 = true)) {
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
        "proxy" if kv.contains_key("locs") => SiteKind::Proxy { locations: decode_locs(&g("locs")), upstreams: decode_ups(&g("ups")) },
        "proxy" => SiteKind::Proxy { locations: vec![Location { ws: flag("ws"), host: flag("host"), ..Location::new("/", Backend::Url(g("target"))) }], upstreams: Vec::new() },
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
    let mut adv = Advanced {
        max_body: Some(g("body")).filter(|v| !v.is_empty()),
        read_timeout: g("timeout").parse().ok(),
        default_server: g("ds") == "1",
        connect_timeout: g("ctimeout").parse().ok(),
        send_timeout: g("stimeout").parse().ok(),
        tls_protocols: Some(g("tlsv")).filter(|v| !v.is_empty()).map(|v| v.split(',').map(str::to_string).collect()),
        ciphers: Some(g("ciphers")).filter(|v| !v.is_empty()),
        no_http2: g("h2") == "0",
        http3: flag("h3"),
        ocsp: flag("ocsp"),
        hsts_age: g("hsts_age").parse().ok(),
        hsts_sub: flag("hsts_sub"),
        hsts_preload: flag("hsts_pre"),
        ..Default::default()
    };
    extras_parse(&g, &flag, &mut adv);
    let spec = SiteSpec { name: g("name"), kind, listeners, ipv6: flag("ipv6"), hsts: flag("hsts"), adv };
    let rest = text.get(consumed.min(text.len())..).unwrap_or("");
    let modified = hash.is_none_or(|h| fnv(rest) != h);
    Some((spec, modified))
}

// ---------------------------------------------------------------- 配置文件、compose

pub fn nginx_conf() -> String {
    "# managed by otter (layout 1)\nworker_processes auto;\nerror_log /var/log/nginx/error.log warn;\nevents {\n    worker_connections 4096;\n}\nhttp {\n    include mime.types;\n    default_type application/octet-stream;\n    server_tokens off;\n    sendfile on;\n    keepalive_timeout 65;\n    proxy_read_timeout 300s;\n    client_max_body_size 100m;\n    log_format main '$remote_addr - $remote_user [$time_local] \"$request\" $status $body_bytes_sent \"$http_referer\" \"$http_user_agent\" $request_time';\n    access_log /var/log/nginx/access.log main;\n    map $http_upgrade $connection_upgrade {\n        default upgrade;\n        '' close;\n    }\n    ssl_session_cache shared:SSL:10m;\n    ssl_session_timeout 1d;\n    gzip on;\n    gzip_comp_level 5;\n    gzip_min_length 1k;\n    gzip_types text/plain text/css application/json application/javascript text/xml application/xml image/svg+xml;\n    include /etc/nginx/conf.d/*.conf;\n}\n".to_string()
}

/// 默认站点:没匹配到域名的请求(包括直接用 IP 访问)一律断开;证书验证用的路径仍然放行
pub fn default_conf(http: u16, https: u16) -> String {
    default_conf_with(http, https, true, true, &[])
}

/// 默认站点配置(没匹配到域名的请求落到这里)。
/// `guard`(HTTPS 防窜站):写默认 HTTPS 站点;不写的话,用没绑定的域名/IP 访问 HTTPS 会落到第一个 HTTPS 站点上(窜站)。
/// `reject`(拒绝默认 SSL 握手):默认 HTTPS 站点直接拒绝握手,不暴露证书;否则用自签证书接住再 444。
/// `covered`:已被某个站点标了 `default_server` 的端口,这里不再写,避免同端口两个默认
pub fn default_conf_with(http: u16, https: u16, guard: bool, reject: bool, covered: &[u16]) -> String {
    let t = Target::own();
    let ds = |p: u16| if covered.contains(&p) { "" } else { " default_server" };
    let mut out = format!("# managed by otter (layout 1)\n# otter:default\nserver {{\n    listen {http}{};\n    server_name _;\n{}    location / {{\n        return 444;\n    }}\n}}\n", ds(http), acme_loc(&t, false));
    if guard {
        let tls = if reject {
            "    ssl_reject_handshake on;\n".to_string()
        } else {
            format!("    ssl_certificate {CONF_MOUNT}/{DEFAULT_PEM};\n    ssl_certificate_key {CONF_MOUNT}/{DEFAULT_KEY};\n    location / {{\n        return 444;\n    }}\n")
        };
        out += &format!("\nserver {{\n    listen {https} ssl{};\n    server_name _;\n{tls}}}\n", ds(https));
    }
    out
}

/// 默认 HTTPS 站点的自签证书(放在站点配置目录里,不是 `.conf` 所以不会被当配置加载)
pub const DEFAULT_PEM: &str = "00-default.pem";
pub const DEFAULT_KEY: &str = "00-default.key";

/// 读出默认站点配置的当前状态:(HTTPS 防窜站, 拒绝默认 SSL 握手)
pub fn default_state(conf: &str, https: u16) -> (bool, bool) {
    let guard = conf.lines().any(|l| {
        let l = l.trim();
        l.starts_with(&format!("listen {https} ssl")) && l.ends_with(';')
    });
    (guard, guard && conf.lines().any(|l| l.trim() == "ssl_reject_handshake on;"))
}

/// 当前被标为默认站点的站点(启用的)
pub fn default_site(sites: &[SiteEntry]) -> Option<&SiteEntry> {
    sites.iter().find(|e| e.enabled && e.spec.as_ref().is_some_and(|s| s.adv.default_server))
}

/// 已被默认站点占着 `default_server` 的端口;`00-default.conf` 在这些端口上不再写 `default_server`
pub fn default_covered(sites: &[SiteEntry]) -> Vec<u16> {
    let mut v: Vec<u16> = default_site(sites).and_then(|e| e.spec.as_ref()).map(|s| s.listeners.iter().map(|l| l.port).collect()).unwrap_or_default();
    v.sort_unstable();
    v.dedup();
    v
}

/// 设置/取消默认站点:同一个事务里改站点文件(新旧默认站点的 `default_server`)和 `00-default.conf`
pub fn default_site_script(sites: &[SiteEntry], target: Option<&str>, http: u16, https: u16, guard: bool, reject: bool, t: &Target) -> Result<String, String> {
    if t.adopted {
        return Err("接管的 nginx 不在这里改默认站点".into());
    }
    let mut a = String::new();
    let mut covered: Vec<u16> = Vec::new();
    for e in sites {
        let Some(spec) = &e.spec else { continue };
        let want = target == Some(spec.name.as_str());
        if want && !e.enabled {
            return Err("这个站点没有启用,不能设为默认站点".into());
        }
        if want {
            covered = spec.listeners.iter().map(|l| l.port).collect();
        }
        if spec.adv.default_server != want {
            let mut ns = spec.clone();
            ns.adv.default_server = want;
            a += &site_save_actions(&ns, e.enabled, t, (e.modified && e.from_conf).then_some(e.text.as_str()))?;
        }
    }
    if target.is_some() && covered.is_empty() {
        return Err("没找到这个站点".into());
    }
    covered.sort_unstable();
    covered.dedup();
    a += &default_conf_actions(http, https, guard, reject, &covered);
    Ok(tx_script(&a))
}

/// 改默认站点配置:事务写 `00-default.conf`;需要自签证书时按需生成
pub fn default_conf_script(http: u16, https: u16, guard: bool, reject: bool, covered: &[u16], t: &Target) -> Result<String, String> {
    if t.adopted {
        return Err("接管的 nginx 不在这里改默认站点".into());
    }
    Ok(tx_script(&default_conf_actions(http, https, guard, reject, covered)))
}

fn default_conf_actions(http: u16, https: u16, guard: bool, reject: bool, covered: &[u16]) -> String {
    let f = "00-default.conf";
    let mut a = format!("bk {f}; {}\n", write_file_cmd(&format!("$C/{f}"), &default_conf_with(http, https, guard, reject, covered), "644").replace(&format!("'$C/{f}'"), &format!("\"$C/{f}\"")));
    if guard && !reject {
        a += &format!(
            "[ -f \"$C/{k}\" ] || {{ openssl req -x509 -nodes -newkey rsa:2048 -days 3650 -subj /CN=localhost -keyout \"$C/{k}\" -out \"$C/{p}\" >/dev/null 2>&1 || fail '生成自签证书失败(服务器上要有 openssl)'; chmod 600 \"$C/{k}\"; }}\n",
            k = DEFAULT_KEY,
            p = DEFAULT_PEM
        );
    }
    a
}

/// 安装 OpenResty 的表单(安装抽屉填的东西)
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
pub fn tags_url(page: u32) -> String {
    format!("https://hub.docker.com/v2/repositories/openresty/openresty/tags?page_size=100&name=-alpine&page={page}")
}

#[cfg_attr(not(test), allow(dead_code))]
const TAG_PAGES: u32 = 10;

/// 服务器上取 tag 列表的脚本:十页并行请求(curl,没有就 wget),按页序输出一串 JSON
#[cfg_attr(not(test), allow(dead_code))]
pub fn tags_script() -> String {
    format!(
        "d=$(mktemp -d); for i in $(seq 1 {TAG_PAGES}); do u='https://hub.docker.com/v2/repositories/openresty/openresty/tags?page_size=100&name=-alpine&page='$i; ( curl -fsSL --max-time 15 \"$u\" || wget -qO- -T 15 \"$u\" ) > $d/$i.json 2>/dev/null & done; wait; for i in $(seq 1 {TAG_PAGES}); do cat $d/$i.json 2>/dev/null; echo; done; rm -rf $d"
    )
}

/// 从 tags 接口响应(一页,或多页 JSON 连在一起)里挑出可选版本:`alpine`(跟随最新)排第一,其后是 `1.27.1.2-alpine` 这样的固定版本,新的在前;带构建序号的(`1.27.1.2-3-alpine`)和 `-fat` 之类的变体不要
#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
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
echo '@@NGINXCONF'; cat $W/conf/nginx.conf 2>/dev/null | head -c 200000
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
    /// 表单数据读自正文(手改过且能解析);否则读自头部
    pub from_conf: bool,
    /// 手改过时,保存重新生成后会丢失的内容(Otter 不管理的指令、注释)
    pub lost: Vec<String>,
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
    /// Otter 装的 OpenResty 的 `nginx.conf` 原文(全局设置的事实源);读不到是 None
    pub nginx_conf: Option<String>,
    /// 默认站点配置 `00-default.conf` 的原文(Otter 生成的);没有是 None
    pub default_conf: Option<String>,
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
            "NGINXCONF" => st.nginx_conf = Some(text).filter(|t| !t.trim().is_empty()),
            "SITES" => {
                for blk in text.split("@@F ").skip(1) {
                    let (file, content) = blk.split_once('\n').unwrap_or((blk, ""));
                    let file = file.trim().to_string();
                    if content.contains("# otter:default") {
                        st.default_conf = Some(content.to_string());
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
                    let modified = parsed.as_ref().is_some_and(|p| p.1);
                    let (mut from_conf, mut lost) = (false, Vec::new());
                    let mut spec = parsed.map(|p| p.0);
                    // 手改过:以正文为准,把能读懂的改动读回表单,读不懂的列出来
                    if modified && let Some(h) = &spec {
                        if let Some(p) = conf::parse_conf(content, &h.name, Some(h)) {
                            from_conf = true;
                            lost = p.unknown;
                            if p.comments > 0 {
                                lost.push(format!("{} 行注释", p.comments));
                            }
                            spec = Some(p.spec);
                        }
                    }
                    st.sites.push(SiteEntry { enabled: !file.ends_with(".off"), file, modified, from_conf, lost, spec, text: content.trim_end_matches('\n').to_string() + "\n" });
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

/// 基本认证的密码文件 `<站点>.htpasswd`:新密码在服务器上用 `openssl passwd -apr1` 生成,没改的用户沿用旧行;没有基本认证就删掉文件
fn htpasswd_actions(spec: &SiteSpec) -> String {
    let f = format!("{}.htpasswd", spec.name);
    let Some(au) = &spec.adv.auth else { return format!("bk {q}; rm -f \"$C/\"{q}\n", q = quote(&f)) };
    let new = format!("$C/{f}.new");
    let mut s = format!("bk {q}; : > \"{new}\"\n", q = quote(&f));
    for u in &au.users {
        match &u.pass {
            Some(p) => s += &format!("h=$(printf %s {} | openssl passwd -apr1 -stdin) || fail \"生成 {n} 的密码失败(服务器上要有 openssl)\"; printf '%s:%s\\n' {} \"$h\" >> \"{new}\"\n", quote(p), quote(&u.name), n = u.name),
            None => s += &format!("l=$(grep -m1 {} \"$S/{f}\" 2>/dev/null) || fail \"用户 {n} 在服务器上没有密码,请重新设置\"; printf '%s\\n' \"$l\" >> \"{new}\"\n", quote(&format!("^{}:", u.name)), n = u.name),
        }
    }
    s += &format!("chmod 644 \"{new}\" && mv \"{new}\" \"$C/{f}\"\n");
    s
}

/// 保存站点(新建或覆盖):写配置文件,静态站点顺手建目录和占位首页
pub fn site_save_script(spec: &SiteSpec, enabled: bool, t: &Target) -> Result<String, String> {
    site_save_script_from(spec, enabled, t, None)
}

/// 同 `site_save_script`;`old` 是站点现有的、被手工改过的正文:就地修改它(保留 Otter 不管理的内容),而不是整体重新生成
pub fn site_save_script_from(spec: &SiteSpec, enabled: bool, t: &Target, old: Option<&str>) -> Result<String, String> {
    Ok(tx_script(&site_save_actions(spec, enabled, t, old)?))
}

/// 保存站点的事务动作(不含事务外壳),可以和别的动作拼在同一个事务里
fn site_save_actions(spec: &SiteSpec, enabled: bool, t: &Target, old: Option<&str>) -> Result<String, String> {
    spec.check()?;
    if matches!(spec.kind, SiteKind::Static { .. }) && t.sites.is_none() {
        return Err("这个 OpenResty 没有挂载静态站点目录,建不了静态站点".into());
    }
    if !spec.certs().is_empty() && t.certs.is_none() {
        return Err("这个 OpenResty 没有挂载证书目录,配不了 HTTPS".into());
    }
    if spec.adv.auth.is_some() && t.adopted {
        return Err("接管的 nginx 暂不支持基本认证(密码文件要放在 Otter 装的 OpenResty 的配置目录里)".into());
    }
    let text = match old {
        Some(o) => patch::patch_text(o, spec, t).map_err(|e| format!("{e}。可以在「配置原文」里直接改,或把手工改动去掉后再用表单保存"))?,
        None => render_site(spec, t),
    };
    let file = site_file(&spec.name, enabled);
    let other = site_file(&spec.name, !enabled);
    let mut a = String::new();
    a += &htpasswd_actions(spec);
    a += &format!("bk {f}; bk {o}; rm -f \"$C/{o}\"; {w}\n", f = quote(&file), o = quote(&other), w = write_file_cmd(&format!("$C/{file}"), &text, "644").replace(&format!("'$C/{file}'"), &format!("\"$C/{file}\"")));
    if matches!(spec.kind, SiteKind::Static { .. }) {
        let d = format!("$SITES/{}", spec.name);
        a += &format!("mkdir -p \"{d}\"; [ -e \"{d}/index.html\" ] || printf '<!doctype html><meta charset=utf-8><title>{n}</title><h1>{n}</h1>' > \"{d}/index.html\"\n", n = spec.name);
    }
    Ok(a)
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
    let base = file.trim_end_matches(".off").trim_end_matches(".conf");
    Ok(tx_script(&format!("bk {f}; rm -f \"$C/\"{f}; rm -f \"$C/\"{h}\n", f = quote(file), h = quote(&format!("{base}.htpasswd")))))
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
#[allow(dead_code)] // 界面已去掉「自签」入口,脚本和测试暂留
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
@CRON@
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
    .replace("@RELOAD@", &reload_cmd())
    .replace("@CRON@", &if r.auto { cron_ensure_cmd(&r.name) } else { String::new() });
    Ok(body)
}

fn acme_index(list: &[AcmeAcct], extra: Option<&AcmeAcct>, created: &str) -> String {
    let mut v: Vec<Value> = list.iter().filter(|a| extra.map_or(true, |e| e.id != a.id)).map(|a| json!({"id": a.id, "ca": a.ca, "email": a.email, "created": ""})).collect();
    if let Some(e) = extra {
        v.push(json!({"id": e.id, "ca": e.ca, "email": e.email, "created": created}));
    }
    serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
}

/// 单独注册 ACME 账号(不申请证书):在 acme.sh 容器里 `--register-account`,账号数据放 `secrets/acme/<id>`,索引写 `accounts/acme.json`。
/// 失败时输出 `ERR:` 开头的行(不写索引)
pub fn acme_account_script(ca: &str, email: &str, all: &[AcmeAcct]) -> Result<String, String> {
    if !email.contains('@') || email.contains(char::is_whitespace) || email.contains(['\'', '"', ';', '$', '`', '\\']) {
        return Err("请填写有效的邮箱(用于接收到期通知)".into());
    }
    if !matches!(ca, "letsencrypt" | "zerossl") {
        return Err("不支持的证书颁发机构".into());
    }
    let id = acme_acct_id(ca, email);
    if !valid_name(&id) {
        return Err("邮箱里有不支持的字符".into());
    }
    if all.iter().any(|a| a.id == id) {
        return Err("这个邮箱在该颁发机构下已经有账号了".into());
    }
    let acct = AcmeAcct { id: id.clone(), ca: ca.into(), email: email.into() };
    let idx = acme_index(all, Some(&acct), &chrono::Utc::now().format("%Y-%m-%d").to_string());
    Ok(format!(
        "{}\n{}",
        srvdata::lock_prelude(),
        fill(
            r#"@PRE@; A=$W/secrets/acme/@ACCT@
umask 077; mkdir -p $A $W/accounts; chmod 700 $W/secrets $A
docker image inspect @IMG@ >/dev/null 2>&1 || docker pull @IMG@ >/dev/null 2>&1 || { rmdir $A 2>/dev/null; echo 'ERR:拉取 acme.sh 镜像失败,请检查服务器能否访问 Docker Hub'; exit 0; }
if out=$(docker run --rm --entrypoint acme.sh -v "$A":/acme.sh @IMG@ --register-account --server @CA@ --accountemail @EMAIL@ 2>&1); then
@IDX@
  echo OK
else
  rm -rf $A; echo "ERR:向 @CAN@ 注册失败:$(printf '%s' "$out" | tail -n 3 | tr '\n' ' ')"
fi
"#,
        )
        .replace("@ACCT@", &id)
        .replace("@IMG@", ACME_IMAGE)
        .replace("@CA@", ca)
        .replace("@CAN@", if ca == "zerossl" { "ZeroSSL" } else { "Let's Encrypt" })
        .replace("@EMAIL@", &quote(email))
        .replace("@IDX@", &write_file_cmd(&format!("{}/accounts/acme.json", srvdata::WEB), &idx, "644"))
    ))
}

/// 删除 ACME 账号:有证书还在用它(续期要用)就拒绝
pub fn acme_account_delete_script(id: &str, all: &[AcmeAcct], certs: &[CertEntry]) -> Result<String, String> {
    if !valid_name(id) {
        return Err("账号 id 不合法".into());
    }
    let used: Vec<&str> = certs.iter().filter(|c| c.meta.acct.as_deref() == Some(id)).map(|c| c.name.as_str()).collect();
    if !used.is_empty() {
        return Err(format!("证书 {} 还在用这个账号续期,先删除这些证书", used.join("、")));
    }
    let idx = acme_index(&all.iter().filter(|a| a.id != id).cloned().collect::<Vec<_>>(), None, "");
    Ok(format!(
        "{}\nrm -rf {}/secrets/acme/{id}\n{}\necho OK\n",
        srvdata::lock_prelude(),
        srvdata::WEB,
        write_file_cmd(&format!("{}/accounts/acme.json", srvdata::WEB), &idx, "644")
    ))
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

/// 申请证书成功后顺手装上续期任务(已经有就不动):分钟数按证书名取个固定值,避免都在同一分钟请求 CA
fn cron_ensure_cmd(name: &str) -> String {
    let h: u32 = name.bytes().fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    let line = format!("# otter:cert-renew 由 Otter 管理:每天检查证书,不到 {RENEW_DAYS} 天到期的自动续期\n{} {} * * * root {RENEW_SH}\n", h % 60, 2 + (h / 60) % 3);
    format!("if [ ! -f {CRON_FILE} ]; then\n{}\n{}\nfi", write_file_cmd(RENEW_SH, &renew_sh(), "755"), write_file_cmd(CRON_FILE, &line, "644"))
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
#[cfg_attr(not(test), allow(dead_code))]
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

/// 容器控制:reload / restart / test / stop / start
pub fn ctl_script(action: &str, t: &Target) -> Result<String, String> {
    let n = quote(&t.container);
    let b = &t.bin;
    Ok(match action {
        "test" => format!("o=$(docker exec {n} {b} -t 2>&1) && {{ echo \"$o\"; echo OK; }} || echo \"ERR:$o\""),
        "reload" => format!("o=$(docker exec {n} {b} -s reload 2>&1) && echo OK || echo \"ERR:$o\""),
        "restart" => format!("o=$(docker restart {n} 2>&1) && echo OK || echo \"ERR:$o\""),
        "stop" => format!("o=$(docker stop {n} 2>&1) && echo OK || echo \"ERR:$o\""),
        "start" => format!("o=$(docker start {n} 2>&1) && echo OK || echo \"ERR:$o\""),
        // 卸载只删容器,配置、站点、证书都保留
        // 接管的容器(面板装的)也能卸载:同时删 target.env,页面回到「未安装」,可以用 Otter 重装
        "remove" if t.adopted => format!("o=$(docker rm -f {n} 2>&1) && {{ rm -f {}/target.env; echo OK; }} || echo \"ERR:$o\"", srvdata::WEB),
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
        SiteSpec { name: name.into(), kind: SiteKind::proxy("http://127.0.0.1:8080", true, true), listeners: vec![ls(&d, 80, None, true), ls(&d, 443, Some("a.example.com"), false)], ipv6: false, hsts: true, adv: Advanced::default() }
    }

    fn multi_loc_spec() -> SiteSpec {
        let d = vec!["a.example.com".to_string()];
        let mut api = Location::new("/api/", Backend::Group { https: false, name: "api".into() });
        api.strip = true;
        api.timeout = Some(60);
        let mut ws = Location::new("/ws", Backend::Url("https://10.0.0.5:9000".into()));
        ws.exact = true;
        let ups = vec![Upstream { name: "api".into(), policy: Policy::LeastConn, servers: vec![UServer { addr: "10.0.0.1:8080".into(), weight: 3, backup: false }, UServer { addr: "10.0.0.2:8080".into(), weight: 1, backup: true }], max_fails: Some(3), fail_timeout: Some(20) }];
        SiteSpec { name: "multi-loc".into(), kind: SiteKind::Proxy { locations: vec![Location::new("/", Backend::Url("http://127.0.0.1:3000".into())), api, ws], upstreams: ups }, listeners: vec![ls(&d, 80, None, false)], ipv6: false, hsts: false, adv: Advanced::default() }
    }

    #[test]
    fn multi_location_and_upstream_render() {
        let spec = multi_loc_spec();
        spec.check().unwrap();
        let text = render_site(&spec, &Target::own());
        if let Ok(p) = std::env::var("OTTER_DUMP_SITE") {
            std::fs::write(p, &text).unwrap();
        }
        assert!(text.contains("upstream multi-loc_api {\n    least_conn;\n    server 10.0.0.1:8080 weight=3 max_fails=3 fail_timeout=20s;\n    server 10.0.0.2:8080 max_fails=3 fail_timeout=20s backup;\n}"));
        assert!(text.contains("location / {\n        proxy_pass http://127.0.0.1:3000;"));
        assert!(text.contains("location /api/ {\n        proxy_pass http://multi-loc_api/;"));
        assert!(text.contains("proxy_read_timeout 60s;"));
        assert!(text.contains("location = /ws {\n        proxy_pass https://10.0.0.5:9000;"));
        assert!(text.contains("proxy_ssl_server_name on;"));
        // 往返
        let (back, modified) = parse_site(&text).unwrap();
        assert!(!modified);
        assert_eq!(back, spec);
    }

    #[test]
    fn unused_upstream_not_rendered_and_checks() {
        let mut spec = multi_loc_spec();
        if let SiteKind::Proxy { locations, upstreams } = &mut spec.kind {
            locations.remove(1);
            assert!(!render_site(&spec_with(locations.clone(), upstreams.clone()), &Target::own()).contains("upstream "));
        }
        let bad = |f: &dyn Fn(&mut Vec<Location>, &mut Vec<Upstream>)| {
            let mut s = multi_loc_spec();
            if let SiteKind::Proxy { locations, upstreams } = &mut s.kind {
                f(locations, upstreams);
            }
            s.check().unwrap_err()
        };
        assert!(bad(&|l, _| l.push(Location::new("/api/", Backend::Url("http://x.y".into())))).contains("重复"));
        assert!(bad(&|l, _| l[0].path = "api".into()).contains("路径"));
        assert!(bad(&|l, _| l[0].path = "/a b".into()).contains("路径"));
        assert!(bad(&|l, _| l[0].strip = true).contains("去掉前缀"));
        assert!(bad(&|l, _| l[1].backend = Backend::Group { https: false, name: "nope".into() }).contains("不存在"));
        assert!(bad(&|_, u| u[0].servers.clear()).contains("至少"));
        assert!(bad(&|_, u| u[0].servers[0].addr = "x;y:80".into()).contains("成员地址"));
        assert!(bad(&|_, u| u[0].servers[0].weight = 0).contains("权重"));
        assert!(bad(&|l, u| {
            u[0].name = "A b".into();
            l[1].backend = Backend::Group { https: false, name: "A b".into() };
        })
        .contains("组名"));
        assert!(bad(&|l, _| l.clear()).contains("至少"));
    }

    fn spec_with(locations: Vec<Location>, upstreams: Vec<Upstream>) -> SiteSpec {
        SiteSpec { kind: SiteKind::Proxy { locations, upstreams }, ..multi_loc_spec() }
    }

    fn full_adv() -> Advanced {
        Advanced {
            gzip: Some(true),
            gzip_level: Some(5),
            gzip_types: Some("text/css application/json".into()),
            ip_mode: 2,
            ip_list: vec!["10.0.0.0/8".into(), "2001:db8::/32".into()],
            auth: Some(BasicAuth { realm: "Private area".into(), users: vec![AuthUser { name: "bob".into(), pass: Some("s3cret!pw".into()) }, AuthUser { name: "amy".into(), pass: None }] }),
            headers: vec![("X-Frame-Options".into(), "SAMEORIGIN".into()), ("Content-Security-Policy".into(), "default-src 'self'; img-src *".into())],
            log_off: false,
            error_level: Some("error".into()),
            err_pages: vec![(404, "/404.html".into()), (502, "https://example.com/down".into())],
            real_ip: vec!["172.16.0.0/12".into()],
            real_ip_header: Some("X-Real-IP".into()),
            rewrites: vec![Rewrite { regex: "^/old/(.*)$".into(), to: "/new/$1".into(), flag: "permanent".into() }],
            limits: Limits { req_rate: Some(10), req_min: false, burst: 20, nodelay: true, conn: Some(30), speed: Some("512k".into()), speed_after: Some("1m".into()), exempt: vec!["192.168.0.0/16".into()] },
            ..Default::default()
        }
    }

    #[test]
    fn extras_render_and_roundtrip() {
        let mut spec = proxy("ex");
        spec.adv = full_adv();
        spec.check().unwrap();
        let t = render_site(&spec, &Target::own());
        let (back, modified) = parse_site(&t).unwrap();
        assert!(!modified);
        // 密码不进配置头;其余原样读回
        assert!(!t.contains("s3cret"));
        let mut want = spec.clone();
        want.adv.auth.as_mut().unwrap().users[0].pass = None;
        assert_eq!(back, want);
        for need in ["gzip on;", "gzip_comp_level 5;", "deny 10.0.0.0/8;", "auth_basic \"Private area\";", "auth_basic_user_file /etc/nginx/conf.d/ex.htpasswd;", "add_header X-Frame-Options \"SAMEORIGIN\" always;", "error_page 404 /404.html;", "set_real_ip_from 172.16.0.0/12;", "real_ip_header X-Real-IP;", "rewrite \"^/old/(.*)$\" \"/new/$1\" permanent;", "limit_req zone=", "burst=20 nodelay;", "limit_conn_status 429;", "limit_rate_after 1m;", "limit_rate 512k;", "geo $otter_", "limit_req_zone $otter_", "error_log /var/log/nginx/ex.error.log error;"] {
            assert!(t.contains(need), "缺少 {need}\n{t}");
        }
        assert!(t.contains("auth_basic off;"), "验证目录要放行");
        let plain = render_site(&proxy("p"), &Target::own());
        for no in ["gzip", "auth_basic", "limit_", "set_real_ip", "rewrite", "error_page", "geo "] {
            assert!(!plain.contains(no), "{no}");
        }
        spec.adv.log_off = true;
        assert!(render_site(&spec, &Target::own()).contains("access_log off;"));
    }

    #[test]
    fn static_extras_render_and_checks() {
        let mut spec = simple("st", "a.com", SiteKind::Static { spa: false, index: false }, None);
        spec.adv = Advanced { index_files: Some("home.html index.html".into()), cache_exts: Some("js css png".into()), cache_time: Some("30d".into()), hotlink_exts: Some("jpg png".into()), hotlink_allow: vec!["*.good.com".into()], hotlink_none: true, ..Default::default() };
        spec.check().unwrap();
        let t = render_site(&spec, &Target::own());
        assert!(t.contains("index home.html index.html;"));
        assert!(t.contains("location ~* \\.(js|css|png)$ {\n        expires 30d;"));
        assert!(t.contains("valid_referers none blocked server_names *.good.com;"));
        assert_eq!(parse_site(&t).unwrap().0, spec);
        // 代理站点不能用静态项
        let mut p = proxy("px");
        p.adv.cache_exts = Some("js".into());
        p.adv.cache_time = Some("1d".into());
        assert!(p.check().is_err());
        spec.adv.cache_time = None;
        assert!(spec.check().is_err());
    }

    #[test]
    fn extras_validation() {
        let bad = |f: &dyn Fn(&mut Advanced)| {
            let mut s = proxy("v");
            f(&mut s.adv);
            assert!(s.check().is_err());
        };
        bad(&|a| a.gzip_level = Some(10));
        bad(&|a| a.gzip_types = Some("css".into()));
        bad(&|a| {
            a.ip_mode = 1;
            a.ip_list = vec!["999.1.1.1".into()];
        });
        bad(&|a| {
            a.ip_mode = 2;
        });
        bad(&|a| a.auth = Some(BasicAuth { realm: "x".into(), users: vec![] }));
        bad(&|a| a.auth = Some(BasicAuth { realm: "x".into(), users: vec![AuthUser { name: "a b".into(), pass: Some("123456".into()) }] }));
        bad(&|a| a.auth = Some(BasicAuth { realm: "x".into(), users: vec![AuthUser { name: "a".into(), pass: Some("123".into()) }] }));
        bad(&|a| a.headers = vec![("X Bad".into(), "v".into())]);
        bad(&|a| a.headers = vec![("X-A".into(), "a\"b".into())]);
        bad(&|a| a.headers = vec![("X-A".into(), "a$b".into())]);
        bad(&|a| a.err_pages = vec![(200, "/x".into())]);
        bad(&|a| a.err_pages = vec![(404, "x.html".into())]);
        bad(&|a| a.err_pages = vec![(404, "/a".into()), (404, "/b".into())]);
        bad(&|a| a.rewrites = vec![Rewrite { regex: "a\"b".into(), to: "/x".into(), flag: "last".into() }]);
        bad(&|a| a.rewrites = vec![Rewrite { regex: "^/a".into(), to: "/x".into(), flag: "nope".into() }]);
        bad(&|a| a.limits.req_rate = Some(0));
        bad(&|a| a.limits.speed = Some("fast".into()));
        bad(&|a| a.limits.exempt = vec!["x".into()]);
        bad(&|a| a.error_level = Some("debug".into()));
        assert!(valid_ip_item("1.2.3.4") && valid_ip_item("10.0.0.0/8") && valid_ip_item("::1") && !valid_ip_item("1.2.3.4/33") && !valid_ip_item("a"));
        // 接管的 nginx 不支持基本认证
        let mut s = proxy("ad");
        s.adv.auth = Some(BasicAuth { realm: "x".into(), users: vec![AuthUser { name: "a".into(), pass: Some("123456".into()) }] });
        let mut t = Target::own();
        t.adopted = true;
        assert!(site_save_script(&s, true, &t).is_err());
        let sc = site_save_script(&s, true, &Target::own()).unwrap();
        assert!(sc.contains("openssl passwd -apr1") && sc.contains("htpasswd"));
        s.adv.auth = None;
        assert!(site_save_script(&s, true, &Target::own()).unwrap().contains("rm -f \"$C/\"'ad.htpasswd'"));
        assert!(site_delete_script("ad.conf").unwrap().contains("ad.htpasswd"));
    }

    /// 取出保存脚本里 htpasswd 的那几行,在假的事务环境里真跑一遍(需要本机有 openssl)
    #[test]
    #[ignore]
    fn htpasswd_actions_work() {
        let mut spec = proxy("hp");
        spec.adv.auth = Some(BasicAuth { realm: "x".into(), users: vec![AuthUser { name: "bob".into(), pass: Some("s3cret!pw".into()) }, AuthUser { name: "amy".into(), pass: None }] });
        let dir = std::env::temp_dir().join(format!("otter-hp-{}", std::process::id()));
        let (c, s) = (dir.join("c"), dir.join("s"));
        std::fs::create_dir_all(&c).unwrap();
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("hp.htpasswd"), "amy:$apr1$old$hash\nzed:$apr1$z$z\n").unwrap();
        let script = format!("C={c}; S={s}; W={w}\nmkdir -p $W\nbk() {{ :; }}\nfail() {{ echo \"FAIL $*\"; exit 3; }}\n{a}", c = c.display(), s = s.display(), w = dir.join("w").display(), a = htpasswd_actions(&spec));
        let out = std::process::Command::new("bash").arg("-c").arg(&script).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        let f = std::fs::read_to_string(c.join("hp.htpasswd")).unwrap();
        assert!(f.starts_with("bob:$apr1$") && f.contains("amy:$apr1$old$hash\n") && !f.contains("zed"), "{f}");
        // 没有旧密码的沿用用户要报错
        std::fs::remove_file(s.join("hp.htpasswd")).unwrap();
        let out = std::process::Command::new("bash").arg("-c").arg(&script).output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("FAIL"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tls_options_render_and_roundtrip() {
        let mut sp = SiteSpec { listeners: vec![ls(&["a.com".to_string()], 80, None, true), ls(&["a.com".to_string()], 443, Some("a.com"), false)], ..multi_loc_spec() };
        sp.adv.tls_protocols = Some(vec!["TLSv1.2".into()]);
        sp.adv.ciphers = Some("HIGH:!aNULL:!MD5".into());
        sp.check().unwrap();
        let text = render_site(&sp, &Target::own());
        assert!(text.contains("ssl_protocols TLSv1.2;") && text.contains("ssl_ciphers HIGH:!aNULL:!MD5;"));
        let (back, modified) = parse_site(&text).unwrap();
        assert!(!modified && back.adv == sp.adv);
        sp.adv.ciphers = Some("bad cipher;".into());
        assert!(sp.check().is_err());
        sp.adv.ciphers = None;
        sp.adv.tls_protocols = Some(vec![]);
        assert!(sp.check().is_err());
    }

    #[test]
    fn https_extras_render_and_roundtrip() {
        let mut sp = SiteSpec { hsts: true, listeners: vec![ls(&["a.com".to_string()], 80, None, true), ls(&["a.com".to_string()], 8443, Some("a.com"), false)], ..multi_loc_spec() };
        sp.adv.no_http2 = true;
        sp.adv.http3 = true;
        sp.adv.ocsp = true;
        sp.adv.hsts_age = Some(63072000);
        sp.adv.hsts_sub = true;
        sp.adv.hsts_preload = true;
        sp.check().unwrap();
        let text = render_site(&sp, &Target::own());
        assert!(text.contains("listen 8443 quic;") && text.contains("Alt-Svc 'h3=\":8443\"; ma=86400'"));
        assert!(!text.contains("http2 on;") && text.contains("ssl_stapling on;") && text.contains("ssl_trusted_certificate"));
        assert!(text.contains("max-age=63072000; includeSubDomains; preload"));
        assert!(text.contains("return 301 https://$host:8443$request_uri;"));
        let (back, modified) = parse_site(&text).unwrap();
        assert!(!modified && back.adv == sp.adv);
        sp.adv.hsts_sub = false;
        assert!(sp.check().is_err());
        sp.adv.hsts_preload = false;
        sp.adv.hsts_age = Some(0);
        assert!(sp.check().is_err());
    }

    #[test]
    fn v2_header_still_reads_as_single_location() {
        let v2 = "# managed by otter (layout 1)\n# otter:site v2 name=a type=proxy target=http://127.0.0.1:8080 ws=0 host=1 ipv6=0 hsts=0 ls=a.com@80@-@0\n# otter:hash 0\nserver {}\n";
        let (s, _) = parse_site(v2).unwrap();
        assert_eq!(s.kind, SiteKind::proxy("http://127.0.0.1:8080", false, true));
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
            kind: SiteKind::proxy("https://127.0.0.1:9000", false, true),
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
        let spec = SiteSpec { name: "multi".into(), kind: SiteKind::proxy("https://127.0.0.1:9000", true, true), listeners: vec![ls(&a, 80, None, true), ls(&a, 443, Some("a.com"), false), ls(&b, 8080, None, false), ls(&b, 8443, Some("b.com"), false)], ipv6: true, hsts: true, adv: Advanced::default() };
        std::fs::write(path, render_site(&spec, &Target::own())).unwrap();
    }

    /// 把默认站点配置的几种组合写到目录里,给 `nginx -t` 真跑(`OTTER_DUMP_DIR=… cargo test dump_global -- --ignored`)
    #[test]
    #[ignore]
    fn dump_global() {
        let Ok(dir) = std::env::var("OTTER_DUMP_DIR") else { return };
        std::fs::write(format!("{dir}/nginx.conf"), nginx_conf()).unwrap();
        std::fs::create_dir_all(format!("{dir}/conf.d")).unwrap();
        std::fs::write(format!("{dir}/conf.d/00-default.conf"), default_conf_with(80, 443, true, false, &[])).unwrap();
    }

    #[test]
    #[ignore]
    fn dump_extras_sites() {
        let Ok(dir) = std::env::var("OTTER_DUMP_DIR") else { return };
        let mut a = proxy("exp");
        a.adv = full_adv();
        std::fs::write(format!("{dir}/exp.conf"), render_site(&a, &Target::own())).unwrap();
        let mut b = simple("exs", "s.com", SiteKind::Static { spa: true, index: true }, None);
        b.adv = Advanced { index_files: Some("home.html index.html".into()), cache_exts: Some("js css png".into()), cache_time: Some("30d".into()), hotlink_exts: Some("jpg png".into()), hotlink_allow: vec!["*.good.com".into()], hotlink_none: true, gzip: Some(false), ..Default::default() };
        std::fs::write(format!("{dir}/exs.conf"), render_site(&b, &Target::own())).unwrap();
    }

    #[test]
    fn advanced_options_render_and_roundtrip() {
        let mut spec = proxy("adv");
        spec.adv = Advanced { max_body: Some("200m".into()), read_timeout: Some(600), ..Default::default() };
        spec.check().unwrap();
        let t = render_site(&spec, &Target::own());
        let (back, modified) = parse_site(&t).unwrap();
        assert_eq!(back, spec);
        assert!(!modified);
        assert_eq!(t.matches("client_max_body_size 200m;").count(), 2);
        assert!(t.contains("proxy_read_timeout 600s;"));
        // 没填就不写
        let plain = render_site(&proxy("p"), &Target::own());
        assert!(!plain.contains("client_max_body_size") && !plain.contains("proxy_read_timeout"));
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
        bad.kind = SiteKind::proxy("http://x;evil", false, true);
        assert!(bad.check().is_err());
        bad.name = "../x".into();
        assert!(bad.check().is_err());
    }

    const STATE: &str = "@@DOCKER\nok\n@@LAYOUT\n{\"layout\":1,\"by\":\"otter\"}\n@@PORTS\n80 nginx\n@@PANEL\n@@CONTAINER\nrunning openresty/openresty:alpine\n@@VERSION\nnginx version: openresty/1.27.1.1\n@@SITES\n@@F a.example.com.conf\n__SITE__\n@@F 00-default.conf\n# otter:default\nserver {}\n@@F legacy.conf\nserver { listen 81; }\n@@CERTS\n@@C a.example.com\n{\"source\":\"acme\",\"main\":\"a.example.com\",\"domains\":[\"a.example.com\"],\"acct\":\"letsencrypt-me-at-x-com\",\"auto\":true}\n@@X\nnotAfter=Mar  5 12:00:00 2027 GMT\nissuer=C = US, O = Let's Encrypt, CN = R11\nsubject=CN = a.example.com\nX509v3 Subject Alternative Name: \n    DNS:a.example.com, DNS:*.b.example.com\n@@C self1\n{\"source\":\"self\"}\n@@X\nnotAfter=Jan 10 00:00:00 2030 GMT\nissuer=CN = self1\nsubject=CN = self1\n@@ACME\n[{\"id\":\"letsencrypt-me-at-x-com\",\"ca\":\"letsencrypt\",\"email\":\"me@x.com\"}]\n@@DNS\n[{\"id\":\"cf\",\"name\":\"主域\",\"provider\":\"cloudflare\"}]\n@@CRON\n# otter:cert-renew x\n17 3 * * * root /opt/otter/websites/bin/renew.sh\n@@RENEW\n2027-01-01 03:17:01 a 续期成功\n@@HEALTH\nsecrets 755\nkey /opt/otter/websites/certs/x/privkey.pem\n@@END\n";

    #[test]
    fn acme_account_scripts() {
        let a = AcmeAcct { id: acme_acct_id("letsencrypt", "me@x.com"), ca: "letsencrypt".into(), email: "me@x.com".into() };
        let s = acme_account_script("zerossl", "you@y.org", &[a.clone()]).unwrap();
        assert!(s.contains("--register-account --server zerossl"));
        assert!(s.contains("secrets/acme/zerossl-you-at-y.org"));
        assert!(acme_account_script("letsencrypt", "me@x.com", &[a.clone()]).is_err());
        assert!(acme_account_script("letsencrypt", "a b@x.com", &[]).is_err());
        assert!(acme_account_script("other", "me@x.com", &[]).is_err());
        let used = CertEntry { name: "c".into(), meta: CertMeta { acct: Some(a.id.clone()), ..Default::default() }, ..Default::default() };
        assert!(acme_account_delete_script(&a.id, &[a.clone()], &[used]).is_err());
        let d = acme_account_delete_script(&a.id, &[a.clone()], &[]).unwrap();
        assert!(d.contains("rm -rf") && d.contains(&format!("secrets/acme/{}", a.id)));
    }

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
        // 手改过:以正文为准,能读懂的进表单,读不懂的列出
        let edited = site.replace("proxy_http_version 1.1;\n", "proxy_http_version 1.1;\n        proxy_read_timeout 90s;\n        proxy_buffering off;\n");
        assert_ne!(edited, site);
        let st2 = parse_state(&STATE.replace("__SITE__", edited.trim_end()));
        let e = st2.sites.iter().find(|s| s.spec.is_some()).unwrap();
        assert!(e.modified && e.from_conf);
        assert_eq!(e.spec.as_ref().unwrap().adv.read_timeout, Some(90));
        assert!(e.lost.iter().any(|l| l.contains("proxy_buffering off")), "{:?}", e.lost);
        // 保存:就地修改,手加的指令留在脚本写出的正文里
        let mut spec = e.spec.clone().unwrap();
        spec.adv.gzip = Some(true);
        let sc = site_save_script_from(&spec, true, &Target::own(), Some(&e.text)).unwrap();
        let b = srvdata::b64(patch::patch_text(&e.text, &spec, &Target::own()).unwrap().as_bytes());
        assert!(sc.contains(&b));
        assert!(patch::patch_text(&e.text, &spec, &Target::own()).unwrap().contains("proxy_buffering off;"));
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
    fn default_conf_options() {
        let on = default_conf(80, 443);
        assert!(on.contains("listen 80 default_server;") && on.contains("listen 443 ssl default_server;") && on.contains("ssl_reject_handshake on;"));
        assert_eq!(default_state(&on, 443), (true, true));
        // 拒绝握手关:用自签证书;80 被站点占了就不写 default_server
        let cert = default_conf_with(80, 443, true, false, &[80]);
        assert_eq!(default_state(&cert, 443), (true, false));
        assert!(cert.contains("listen 80;") && cert.contains("listen 443 ssl default_server;") && cert.contains("00-default.pem"));
        // 防窜站关:没有默认 HTTPS 站点,拒绝握手也无从谈起
        let off = default_conf_with(80, 443, false, true, &[]);
        assert_eq!(default_state(&off, 443), (false, false));
        assert!(!off.contains("443") && !off.contains("ssl"));
        let sc = default_conf_script(80, 443, true, false, &[], &Target::own()).unwrap();
        assert!(sc.contains("openssl req -x509") && sc.contains("bk 00-default.conf"));
        let o = std::process::Command::new("bash").arg("-n").arg("-c").arg(&sc).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(!default_conf_script(80, 443, true, true, &[], &Target::own()).unwrap().contains("openssl req"));
        let mut ad = Target::own();
        ad.adopted = true;
        assert!(default_conf_script(80, 443, true, true, &[], &ad).is_err());
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
        let rm = ctl_script("remove", &t).unwrap();
        assert!(rm.contains("docker rm -f") && rm.contains("target.env"));
        assert!(!ctl_script("remove", &Target::own()).unwrap().contains("target.env"));
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
        let proxy = simple("a.test".into(), "a.test", SiteKind::proxy("http://127.0.0.1:9", true, true), None);
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
    }

    /// 把真实脚本写到 `$OTTER_DUMP` 目录,供在真机上跑(`cargo test dump_scripts -- --ignored`)
    #[test]
    #[ignore]
    fn dump_scripts() {
        let d = std::env::var("OTTER_DUMP").unwrap();
        let w = |n: &str, s: String| std::fs::write(format!("{d}/{n}.sh"), s).unwrap();
        w("01-install", install_job_script(&InstallSpec { project: "gw".into(), container: "web-gw".into(), http: 18080, https: 18443, cpus: "1".into(), memory: "256m".into(), ..Default::default() }));
        let spec = |name: &str, dom: &str, target: &str| simple(name.into(), dom, SiteKind::proxy(target, true, true), None);
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
