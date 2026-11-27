//! 应用商店:目录(`apps/catalog.json`)的类型、校验、渲染和安装 / 升级 / 卸载脚本。
//!
//! 全是纯函数,不碰网络和界面。设计见 `docs/modules/appstore.md`。
//! 目录里只有数据,没有可执行的东西:compose 模板经变量替换后还要过危险字段扫描和镜像仓库白名单。

use crate::dbinstall::{valid_cpus, valid_memory, valid_password, valid_tag};
use crate::srvdata::{self, b64, write_file_cmd};
use crate::sysops::quote;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const SCHEMA: u32 = 1;
/// 内置快照(编译进程序,联网和缓存都失败时用)
pub const EMBEDDED: &str = include_str!("../apps/catalog.json");
/// 目录地址:主地址 + 备用
pub const CATALOG_URLS: [&str; 2] = ["https://raw.githubusercontent.com/chenbstack/otter/main/apps/catalog.json", "https://cdn.jsdelivr.net/gh/chenbstack/otter@main/apps/catalog.json"];
/// 安装任务的目录(和网站页的任务分开,两边互不干扰)
pub const JOB_DIR: &str = "/opt/otter/jobs/app";
pub const COMPOSE_FILE: &str = "compose.yml";
pub const RECORD_FILE: &str = "otter-app.json";

// ---------------------------------------------------------------- 当前目录

static CURRENT: std::sync::RwLock<Option<std::sync::Arc<Catalog>>> = std::sync::RwLock::new(None);

/// 当前使用的目录:启动时是本机缓存(没有或解析失败用内置快照),拉到新的之后换掉
pub fn current() -> std::sync::Arc<Catalog> {
    if let Some(c) = CURRENT.read().ok().and_then(|g| g.clone()) {
        return c;
    }
    let c = std::sync::Arc::new(load_cached().unwrap_or_else(embedded));
    if let Ok(mut g) = CURRENT.write() {
        *g = Some(c.clone());
    }
    c
}

pub fn set_current(c: Catalog) {
    if let Ok(mut g) = CURRENT.write() {
        *g = Some(std::sync::Arc::new(c));
    }
}

fn cache_path() -> Option<std::path::PathBuf> {
    crate::store::data_dir().map(|d| d.join("catalog.json"))
}

fn load_cached() -> Option<Catalog> {
    parse_catalog(&std::fs::read_to_string(cache_path()?).ok()?).ok()
}

/// 本地缓存的目录多久没更新了(没有缓存返回 None)
pub fn cache_age() -> Option<std::time::Duration> {
    std::fs::metadata(cache_path()?).ok()?.modified().ok()?.elapsed().ok()
}

/// 目录超过一天没更新(或还没有缓存)
pub fn cache_stale() -> bool {
    cache_age().is_none_or(|a| a > std::time::Duration::from_secs(86400))
}

/// 后台线程:启动时发现超过一天没更新就立刻从仓库更新一次,之后每天一次;失败 1 小时后重试
pub fn start_daily_refresh() {
    std::thread::spawn(|| loop {
        let wait = if cache_stale() {
            match refresh_remote() {
                Ok(()) => 86400,
                Err(_) => 3600,
            }
        } else {
            86400u64.saturating_sub(cache_age().map_or(0, |a| a.as_secs())).max(60)
        };
        std::thread::sleep(std::time::Duration::from_secs(wait));
    });
}

/// 阻塞:依次请求目录地址,第一个合法的就写缓存并换成当前目录。放后台线程调用
pub fn refresh_remote() -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(10))).user_agent("otter").build().into();
    let mut last = String::from("没有可用的目录地址");
    for u in CATALOG_URLS {
        let got = agent.get(u).call().map_err(|e| e.to_string()).and_then(|mut r| r.body_mut().read_to_string().map_err(|e| e.to_string()));
        match got.and_then(|t| parse_catalog(&t).map(|c| (t, c))) {
            Ok((text, c)) => {
                if let Some(p) = cache_path() {
                    if let Some(d) = p.parent() {
                        let _ = std::fs::create_dir_all(d);
                    }
                    let _ = std::fs::write(p, text);
                }
                set_current(c);
                return Ok(());
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

// ---------------------------------------------------------------- 类型

#[derive(Clone, Debug, Deserialize)]
pub struct Catalog {
    pub schema: u32,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub categories: Vec<Category>,
    pub apps: Vec<App>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Category {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct App {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub icon: String,
    /// 详细介绍(详情页顶部的一段话)
    #[serde(default)]
    pub description: String,
    /// 主要功能,每条一行
    #[serde(default)]
    pub features: Vec<String>,
    /// 官网
    #[serde(default)]
    pub website: Option<String>,
    /// 开源项目地址
    #[serde(default)]
    pub repo: Option<String>,
    /// 支持的 CPU 架构:`amd64` / `arm64`(也认 x86_64、aarch64)。缺省或空 = 不限
    #[serde(default)]
    pub arch: Vec<String>,
    /// `database` / `website`:和对应页面联动
    #[serde(default)]
    pub kind: Option<String>,
    /// 内置的安装前后处理;只认 `openresty`
    #[serde(default)]
    pub profile: Option<String>,
    pub versions: Versions,
    #[serde(default)]
    pub params: Vec<Param>,
    #[serde(default)]
    pub limits: bool,
    #[serde(default)]
    pub derive: Vec<Derive>,
    pub compose: String,
    #[serde(default)]
    pub files: Vec<AppFile>,
    #[serde(default)]
    pub post: Option<Post>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum Versions {
    Dockerhub {
        repo: String,
        #[serde(rename = "match")]
        pattern: String,
        #[serde(default = "latest")]
        default: String,
        #[serde(default)]
        extra: Vec<String>,
        #[serde(default)]
        pages: Option<u32>,
        /// Hub 接口的 `name=` 过滤(子串),用于 tag 特别多的镜像
        #[serde(default)]
        search: Option<String>,
        /// 在线取版本失败时用的备份(tag 原名,同样按 `match` 过滤排序)。写在目录文件里,随目录更新
        #[serde(default)]
        fallback: Vec<String>,
    },
    Static {
        list: Vec<String>,
        #[serde(default = "latest")]
        default: String,
    },
    None {
        tag: String,
    },
}

fn latest() -> String {
    "latest".into()
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PType {
    Text,
    Number,
    Port,
    Password,
    Select,
    Bool,
    Size,
    Path,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Param {
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub ty: PType,
    #[serde(default)]
    pub default: Value,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub warn: String,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub min: Option<i64>,
    #[serde(default)]
    pub max: Option<i64>,
    #[serde(default)]
    pub options: Vec<Opt>,
    #[serde(default)]
    pub generate: bool,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub auto_free: bool,
    #[serde(default)]
    pub show_once: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Opt {
    pub value: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Derive {
    pub id: String,
    pub from: String,
    #[serde(default)]
    pub map: BTreeMap<String, String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub default: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Rule {
    pub major_gte: u32,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AppFile {
    pub path: String,
    pub content: String,
    #[serde(default = "mode644")]
    pub mode: String,
}

fn mode644() -> String {
    "644".into()
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Post {
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub show: Vec<String>,
}

// ---------------------------------------------------------------- 目录解析和校验

/// 解析并校验目录。不认识的 schema、重复 id、模板错误都拒绝
pub fn parse_catalog(text: &str) -> Result<Catalog, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("目录不是合法的 JSON:{e}"))?;
    let schema = v.get("schema").and_then(|s| s.as_u64()).unwrap_or(0) as u32;
    if schema > SCHEMA {
        return Err("目录格式比当前版本新,请升级 Otter".into());
    }
    let cat: Catalog = serde_json::from_value(v).map_err(|e| format!("目录格式不对:{e}"))?;
    let mut seen = Vec::new();
    for a in &cat.apps {
        check_app(a).map_err(|e| format!("应用 {}:{e}", a.id))?;
        if seen.contains(&a.id) {
            return Err(format!("应用 {} 重复", a.id));
        }
        seen.push(a.id.clone());
    }
    Ok(cat)
}

pub fn embedded() -> Catalog {
    parse_catalog(EMBEDDED).expect("内置目录必须合法")
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') && !s.starts_with(['-', '_'])
}

fn valid_env_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// 目录自身的静态检查:id、参数、模板里的变量、用默认值渲染一遍并过安全扫描
pub fn check_app(a: &App) -> Result<(), String> {
    if !valid_id(&a.id) {
        return Err("id 只能用小写字母、数字、- 和 _".into());
    }
    if a.name.trim().is_empty() {
        return Err("缺名称".into());
    }
    if let Some(p) = &a.profile {
        if p != "openresty" {
            return Err(format!("不认识的 profile:{p}"));
        }
    }
    let mut ids: Vec<&str> = vec!["version", "project", "container", "limits"];
    for p in &a.params {
        if !valid_id(&p.id) || ids.contains(&p.id.as_str()) {
            return Err(format!("参数 id「{}」不合法或重复", p.id));
        }
        ids.push(&p.id);
        if p.secret && !p.env.as_deref().is_some_and(valid_env_name) {
            return Err(format!("秘密参数「{}」要用大写的 env 变量名", p.id));
        }
        if p.ty == PType::Select && p.options.is_empty() {
            return Err(format!("选择参数「{}」没有选项", p.id));
        }
        if let Some(re) = &p.pattern {
            regex::Regex::new(re).map_err(|e| format!("参数「{}」的 pattern 不合法:{e}", p.id))?;
        }
    }
    for d in &a.derive {
        if !valid_id(&d.id) || ids.contains(&d.id.as_str()) {
            return Err(format!("派生变量「{}」不合法或重复", d.id));
        }
        if d.from != "version" && !a.params.iter().any(|p| p.id == d.from) {
            return Err(format!("派生变量「{}」的来源「{}」不存在", d.id, d.from));
        }
        ids.push(&d.id);
    }
    if let Versions::Dockerhub { pattern, repo, .. } = &a.versions {
        regex::Regex::new(pattern).map_err(|e| format!("版本 match 不合法:{e}"))?;
        if repo.is_empty() || repo.chars().any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))) {
            return Err("镜像仓库名不合法".into());
        }
    }
    for f in &a.files {
        if !f.path.starts_with(|c: char| c.is_ascii_alphanumeric()) || f.path.contains("..") || f.path.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(format!("文件路径「{}」不合法", f.path));
        }
    }
    // 用默认值渲染一遍:模板错误、危险字段在这里就暴露
    let v = default_values(a, &[], &[]);
    render(a, &v).map(|_| ()).map_err(|e| format!("模板渲染失败:{e}"))
}

// ---------------------------------------------------------------- 版本

/// 取版本列表需要的信息;`None` 表示不用取(静态列表、固定 tag)
pub fn hub_pages(a: &App) -> Option<(String, Option<String>, u32)> {
    match &a.versions {
        Versions::Dockerhub { repo, pages, search, .. } => Some((repo.clone(), search.clone(), pages.unwrap_or(1).clamp(1, 10))),
        _ => None,
    }
}

/// Docker Hub 某镜像的 tag 列表第 `page` 页
pub fn hub_url(repo: &str, search: Option<&str>, page: u32) -> String {
    let name = search.map(|s| format!("&name={s}")).unwrap_or_default();
    format!("https://hub.docker.com/v2/repositories/{repo}/tags?page_size=100&ordering=last_updated{name}&page={page}")
}

/// 服务器上取 tag 的脚本:多页并行(curl,没有就 wget),按页序输出一串 JSON
pub fn hub_script(repo: &str, search: Option<&str>, pages: u32) -> String {
    let u = hub_url(repo, search, 0);
    let u = u.trim_end_matches('0');
    format!(
        "d=$(mktemp -d); for i in $(seq 1 {pages}); do u='{u}'$i; ( curl -fsSL --max-time 15 \"$u\" || wget -qO- -T 15 \"$u\" ) > $d/$i.json 2>/dev/null & done; wait; for i in $(seq 1 {pages}); do cat $d/$i.json 2>/dev/null; echo; done; rm -rf $d"
    )
}

/// 本机请求 Docker Hub(阻塞,放后台线程调用;多页并行)
pub fn fetch_hub_local(repo: &str, search: Option<&str>, pages: u32) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(15))).user_agent("otter").build().into();
    let res: Vec<Result<String, String>> = std::thread::scope(|sc| {
        let hs: Vec<_> = (1..=pages)
            .map(|p| {
                let (agent, url) = (&agent, hub_url(repo, search, p));
                sc.spawn(move || -> Result<String, String> {
                    let mut resp = agent.get(&url).call().map_err(|e| format!("请求 Docker Hub 失败:{e}"))?;
                    resp.body_mut().read_to_string().map_err(|e| format!("读取响应失败:{e}"))
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("线程异常".into()))).collect()
    });
    let first_err = res.iter().find_map(|r| r.as_ref().err().cloned());
    let all: Vec<String> = res.into_iter().filter_map(Result::ok).collect();
    if all.is_empty() {
        return Err(first_err.unwrap_or_else(|| "Docker Hub 没有响应".into()));
    }
    Ok(all.join("\n"))
}

/// tags 接口响应(一页,或多页 JSON 连在一起)里的全部 tag 名
pub fn parse_tag_names(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for v in serde_json::Deserializer::from_str(raw).into_iter::<Value>().map_while(Result::ok) {
        if let Some(rs) = v.get("results").and_then(|r| r.as_array()) {
            out.extend(rs.iter().filter_map(|r| r.get("name")?.as_str().map(String::from)));
        }
    }
    out
}

/// 版本号里的数字段,用来排序:`1.27.1.2-alpine` → [1,27,1,2],`RELEASE.2025-01-02T03-04-05Z` → [2025,1,2,3,4,5]
fn ver_key(tag: &str) -> Vec<u64> {
    tag.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect()
}

/// 按目录的 `match` 过滤 tag、新的在前,`extra` 排最前。没有可选版本返回 None
pub fn select_versions(a: &App, names: &[String]) -> Option<Vec<String>> {
    match &a.versions {
        Versions::Dockerhub { pattern, extra, .. } => {
            let re = regex::Regex::new(pattern).ok()?;
            let mut v: Vec<(Vec<u64>, String)> = names.iter().filter(|n| re.is_match(n) && !extra.contains(n)).map(|n| (ver_key(n), n.clone())).collect();
            v.sort_by(|x, y| y.0.cmp(&x.0).then_with(|| y.1.cmp(&x.1)));
            v.dedup_by(|x, y| x.1 == y.1);
            if v.is_empty() {
                return None;
            }
            let mut list: Vec<String> = extra.clone();
            list.extend(v.into_iter().take(80).map(|t| t.1));
            Some(list)
        }
        Versions::Static { list, .. } => Some(list.clone()),
        Versions::None { tag } => Some(vec![tag.clone()]),
    }
}

/// 版本列表里默认选哪一项。默认不选浮动标签(`17`、`8.4`、`latest` 这类会随新版本移动的):
/// - `latest`:列表里最新的具体版本(跳过 `extra` 和位数较少的浮动标签)
/// - `pin:P`:`P` 这条线上最新的具体版本,如 `pin:8.4` → `8.4.9`
/// - 其他:按 tag 原名精确匹配(想默认用浮动标签时这样写)
pub fn default_index(a: &App, list: &[String]) -> usize {
    let (def, extra) = match &a.versions {
        Versions::Dockerhub { default, extra, .. } => (default.as_str(), extra.len().min(list.len())),
        Versions::Static { default, .. } => (default.as_str(), 0),
        Versions::None { .. } => return 0,
    };
    // 浮动标签 = 列表里还有别的 tag 以它加「.」开头(17 之于 17.5、8.4 之于 8.4.9);具体版本取列表里第一个非浮动的(列表已按新到旧排好)
    let floating = |i: usize| {
        let dot = format!("{}.", list[i]);
        list.iter().any(|t| t.starts_with(&dot))
    };
    let concrete = |cands: Vec<usize>| -> Option<usize> { cands.into_iter().find(|&i| !floating(i)) };
    if let Some(p) = def.strip_prefix("pin:") {
        let dot = format!("{p}.");
        let cands: Vec<usize> = (extra..list.len()).filter(|&i| list[i] == p || list[i].starts_with(&dot)).collect();
        return concrete(cands).unwrap_or(0);
    }
    if def == "latest" {
        return concrete((extra..list.len()).collect()).unwrap_or(extra.min(list.len().saturating_sub(1)));
    }
    list.iter().position(|t| t == def).unwrap_or(0)
}

fn versions_path() -> Option<std::path::PathBuf> {
    crate::store::data_dir().map(|d| d.join("versions.json"))
}

/// 本地保存的某个应用的在线版本列表:上次从 Hub 取到的时间(unix 秒)和列表
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SavedVersions {
    at: u64,
    list: Vec<String>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn load_versions() -> std::collections::HashMap<String, SavedVersions> {
    versions_path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// 把在线取到的版本列表连同时间存到本地
pub fn save_versions(id: &str, list: &[String]) {
    let Some(p) = versions_path() else { return };
    let mut m = load_versions();
    m.insert(id.to_string(), SavedVersions { at: now_secs(), list: list.to_vec() });
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(t) = serde_json::to_string(&m) {
        let _ = std::fs::write(p, t);
    }
}

/// 在线取到的 tag 名并上目录里的版本列表和本地保存的上次在线列表(目录和本地是基础,Hub 补更新的)
pub fn with_catalog_names(a: &App, mut names: Vec<String>) -> Vec<String> {
    if let Versions::Dockerhub { fallback, .. } = &a.versions {
        names.extend(fallback.iter().cloned());
        names.extend(load_versions().remove(&a.id).map(|s| s.list).unwrap_or_default());
    }
    names
}

/// 版本列表的统一入口(第一步):本地保存的在线列表 24 小时内更新过,就用「目录 + 本地」直接算出列表,不请求 Hub;
/// 没有或过期返回 None,由调用方请求 Hub 补最新的(`with_catalog_names` 合并后 `save_versions`)。
/// 不是 Docker Hub 来源的应用不走这里
pub fn fresh_versions(a: &App) -> Option<Vec<String>> {
    if !matches!(a.versions, Versions::Dockerhub { .. }) {
        return None;
    }
    let saved = load_versions().remove(&a.id)?;
    if now_secs().saturating_sub(saved.at) >= 86400 {
        return None;
    }
    select_versions(a, &with_catalog_names(a, Vec::new()))
}

/// 在线取版本失败时,用目录里的备份列表;没有备份返回 None
pub fn fallback_versions(a: &App) -> Option<Vec<String>> {
    match &a.versions {
        Versions::Dockerhub { fallback, .. } if !fallback.is_empty() => select_versions(a, fallback),
        _ => None,
    }
}

/// 取不到版本列表时,表单里先放的那个 tag:优先用目录里的备份列表算出的具体版本
pub fn fallback_version(a: &App) -> String {
    if let Some(l) = fallback_versions(a) {
        if let Some(t) = l.get(default_index(a, &l)) {
            return t.clone();
        }
    }
    match &a.versions {
        Versions::Dockerhub { default, extra, .. } => match default.strip_prefix("pin:") {
            Some(p) => p.to_string(),
            None if default == "latest" => extra.first().cloned().unwrap_or_else(|| "latest".into()),
            None => default.clone(),
        },
        Versions::Static { default, list } => {
            if default == "latest" {
                list.first().cloned().unwrap_or_else(|| "latest".into())
            } else {
                default.clone()
            }
        }
        Versions::None { tag } => tag.clone(),
    }
}

// ---------------------------------------------------------------- 表单值和渲染

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Values {
    pub version: String,
    pub project: String,
    pub container: String,
    pub cpus: String,
    pub memory: String,
    pub params: BTreeMap<String, String>,
}

/// 用参数默认值填一份表单值(密码参数 `generate` 的当场生成)。`taken` 是已占用的名字,`used` 是已占用的端口
pub fn default_values(a: &App, taken: &[String], used: &[u16]) -> Values {
    let base = format!("otter-{}", a.id);
    let name = if taken.contains(&base) { gen_suffix_name(&base, taken) } else { base };
    let mut params = BTreeMap::new();
    let mut claimed: Vec<u16> = used.to_vec();
    for p in &a.params {
        let mut v = match &p.default {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        };
        if p.ty == PType::Port {
            if let Ok(mut port) = v.parse::<u16>() {
                if p.auto_free {
                    while claimed.contains(&port) && port < u16::MAX {
                        port += 1;
                    }
                }
                claimed.push(port);
                v = port.to_string();
            }
        }
        if p.ty == PType::Password && p.generate {
            v = crate::dbinstall::gen_password();
        }
        if p.ty == PType::Bool && v.is_empty() {
            v = "false".into();
        }
        params.insert(p.id.clone(), v);
    }
    Values { version: fallback_version(a), project: name.clone(), container: name, cpus: String::new(), memory: String::new(), params }
}

pub fn gen_suffix_name(base: &str, taken: &[String]) -> String {
    use rand::RngExt as _;
    let mut rng = rand::rng();
    loop {
        let suf: String = (0..4).map(|_| b"0123456789abcdef"[rng.random_range(0..16)] as char).collect();
        let n = format!("{base}-{suf}");
        if !taken.contains(&n) {
            return n;
        }
    }
}

/// 渲染结果:要写到服务器上的内容和前置检查
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub app: String,
    pub project: String,
    pub container: String,
    pub version: String,
    /// 堆栈目录
    pub dir: String,
    pub compose: String,
    /// `.env` 文本(含密码);没有秘密参数时为空
    pub env: String,
    /// 预览用:密码打码
    pub env_masked: String,
    pub files: Vec<(String, String, String)>,
    /// 安装前要检查没被占用的宿主机端口
    pub ports: Vec<u16>,
    /// 非秘密的参数值(记录里存)
    pub public_values: BTreeMap<String, String>,
}

fn bad_text(s: &str) -> bool {
    s.chars().any(|c| c.is_control() || matches!(c, '$' | '"' | '\\' | '`'))
}

/// 校验并转成模板里用的值
fn check_param(p: &Param, raw: &str) -> Result<String, String> {
    let l = &p.label;
    let v = raw.trim();
    if v.is_empty() {
        if p.required || matches!(p.ty, PType::Password | PType::Port | PType::Number | PType::Select) {
            return Err(format!("请填写「{l}」"));
        }
        return Ok(String::new());
    }
    match p.ty {
        PType::Text => {
            if bad_text(v) {
                return Err(format!("「{l}」里不能有引号、$、\\ 和控制字符"));
            }
            if let Some(re) = &p.pattern {
                if !regex::Regex::new(re).map(|r| r.is_match(v)).unwrap_or(false) {
                    return Err(format!("「{l}」的格式不对"));
                }
            }
        }
        PType::Number => {
            let n: i64 = v.parse().map_err(|_| format!("「{l}」要填整数"))?;
            if p.min.is_some_and(|m| n < m) || p.max.is_some_and(|m| n > m) {
                return Err(format!("「{l}」要在 {} 到 {} 之间", p.min.unwrap_or(i64::MIN), p.max.unwrap_or(i64::MAX)));
            }
        }
        PType::Port => {
            let n: u32 = v.parse().map_err(|_| format!("「{l}」要填端口号"))?;
            let min = p.min.unwrap_or(1).max(1) as u32;
            if n < min || n > 65535 {
                return Err(format!("「{l}」要在 {min} 到 65535 之间"));
            }
        }
        PType::Password => valid_password(raw)?,
        PType::Select => {
            if !p.options.iter().any(|o| o.value == v) {
                return Err(format!("「{l}」不在可选范围内"));
            }
        }
        PType::Bool => {
            if v != "true" && v != "false" {
                return Err(format!("「{l}」要填 true 或 false"));
            }
        }
        PType::Size => valid_memory(v).map_err(|_| format!("「{l}」填整数加 m 或 g,如 512m、2g"))?,
        PType::Path => {
            if !v.starts_with('/') || v.contains("..") || bad_text(v) {
                return Err(format!("「{l}」要是以 / 开头的绝对路径"));
            }
        }
    }
    Ok(if p.ty == PType::Password { raw.to_string() } else { v.to_string() })
}

fn major(version: &str) -> Option<u32> {
    version.split(['.', '-']).next()?.parse().ok()
}

/// 把模板里的 `${...}` 换成值。`${limits}` 单独成行时展开,`$$` 原样保留,其余的 `$` 不允许
fn substitute(tpl: &str, vars: &BTreeMap<String, String>, envs: &BTreeMap<String, String>, limits: &[String]) -> Result<String, String> {
    let mut out = String::new();
    for line in tpl.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        if body.trim() == "${limits}" {
            let indent = &body[..body.len() - body.trim_start().len()];
            for l in limits {
                out += &format!("{indent}{l}\n");
            }
            continue;
        }
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '$' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('$') => out.push_str("$$"),
                Some('{') => {
                    let mut name = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some(c) => name.push(c),
                            None => return Err("模板里有没闭合的 ${".into()),
                        }
                    }
                    if let Some(id) = name.strip_prefix("env.") {
                        let e = envs.get(id).ok_or_else(|| format!("模板用了 ${{env.{id}}},但「{id}」不是带 env 的秘密参数"))?;
                        out += &format!("${{{e}}}");
                    } else {
                        let v = vars.get(&name).ok_or_else(|| format!("模板里有未知变量 ${{{name}}}"))?;
                        out += v;
                    }
                }
                _ => return Err("模板里的 $ 要写成 $$ 或 ${变量}".into()),
            }
        }
        if line.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

/// 校验表单值并渲染。出错的消息直接给用户看
pub fn render(a: &App, v: &Values) -> Result<Plan, String> {
    if !valid_tag(&v.version) {
        return Err("版本号只能用字母、数字、. _ -".into());
    }
    for (what, n) in [("堆栈名", &v.project), ("容器名", &v.container)] {
        if !crate::docker::valid_project(n) {
            return Err(format!("{what}只能用小写字母、数字、- 和 _,并以字母或数字开头"));
        }
    }
    if !a.limits && (!v.cpus.is_empty() || !v.memory.is_empty()) {
        return Err("这个应用不支持资源限制".into());
    }
    valid_cpus(&v.cpus)?;
    valid_memory(&v.memory)?;
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    vars.insert("version".into(), v.version.clone());
    vars.insert("project".into(), v.project.clone());
    vars.insert("container".into(), v.container.clone());
    let (mut envs, mut env_lines, mut masked, mut ports, mut public_values) = (BTreeMap::new(), String::new(), String::new(), Vec::new(), BTreeMap::new());
    let mut show_secret_ids = Vec::new();
    for p in &a.params {
        let raw = v.params.get(&p.id).cloned().unwrap_or_default();
        let val = check_param(p, &raw)?;
        if p.secret {
            let name = p.env.clone().unwrap_or_default();
            env_lines += &format!("{name}={val}\n");
            masked += &format!("{name}=••••••••\n");
            envs.insert(p.id.clone(), name);
            show_secret_ids.push(p.id.clone());
        } else {
            vars.insert(p.id.clone(), val.clone());
            public_values.insert(p.id.clone(), val.clone());
        }
        if p.ty == PType::Port {
            if let Ok(n) = val.parse::<u16>() {
                if ports.contains(&n) {
                    return Err(format!("「{}」和别的端口重复了", p.label));
                }
                ports.push(n);
            }
        }
    }
    for d in &a.derive {
        let from = if d.from == "version" { v.version.clone() } else { vars.get(&d.from).cloned().ok_or_else(|| format!("派生变量来源「{}」是秘密参数", d.from))? };
        let out = if !d.map.is_empty() {
            d.map.get(&from).cloned().ok_or_else(|| format!("派生变量「{}」没有对应「{from}」的取值", d.id))?
        } else {
            let m = major(&from);
            d.rules.iter().find(|r| m.is_some_and(|m| m >= r.major_gte)).map(|r| r.value.clone()).unwrap_or_else(|| d.default.clone())
        };
        vars.insert(d.id.clone(), out);
    }
    let mut limits = Vec::new();
    if !v.cpus.is_empty() {
        limits.push(format!("cpus: \"{}\"", v.cpus));
    }
    if !v.memory.is_empty() {
        limits.push(format!("mem_limit: {}", v.memory.to_ascii_lowercase()));
    }
    let compose = substitute(&a.compose, &vars, &envs, &limits)?;
    scan_compose(&compose, a.profile.is_some())?;
    let mut files = Vec::new();
    for f in &a.files {
        files.push((f.path.clone(), substitute(&f.content, &vars, &envs, &[])?, f.mode.clone()));
    }
    let _ = show_secret_ids;
    Ok(Plan {
        app: a.id.clone(),
        project: v.project.clone(),
        container: v.container.clone(),
        version: v.version.clone(),
        dir: stack_dir(a, &v.project),
        compose,
        env: env_lines,
        env_masked: masked,
        files,
        ports,
        public_values,
    })
}

/// 堆栈目录:都在 Otter 的 compose 目录下
pub fn stack_dir(_a: &App, project: &str) -> String {
    srvdata::compose_dir(project)
}

// ---------------------------------------------------------------- 安全扫描

const REGISTRIES: [&str; 4] = ["docker.io", "index.docker.io", "ghcr.io", "quay.io"];

/// 对渲染后的 compose 做文本扫描。拒绝:特权容器、挂 docker.sock、pid: host、host 网络(内置 profile 除外)、
/// 危险 capability、把宿主机根目录挂进去、白名单之外的镜像仓库
pub fn scan_compose(text: &str, profile: bool) -> Result<(), String> {
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        let low = line.to_ascii_lowercase();
        let kv = |k: &str| low.strip_prefix(k).map(|r| r.trim().trim_matches(|c| c == '"' || c == '\'').to_string());
        if kv("privileged:").is_some_and(|v| v != "false") {
            return Err("compose 里有 privileged,已拒绝".into());
        }
        if low.contains("docker.sock") {
            return Err("compose 里挂载了 docker.sock,已拒绝".into());
        }
        if kv("pid:").is_some_and(|v| v == "host") {
            return Err("compose 里用了 pid: host,已拒绝".into());
        }
        if kv("network_mode:").is_some_and(|v| v == "host") && !profile {
            return Err("compose 里用了 host 网络,已拒绝".into());
        }
        if kv("ipc:").is_some_and(|v| v == "host") || kv("userns_mode:").is_some_and(|v| v == "host") {
            return Err("compose 里用了宿主机命名空间,已拒绝".into());
        }
        if low.contains("sys_admin") || low.contains("sys_ptrace") || low.contains("sys_module") || low.trim_start_matches("- ").trim_matches(|c| c == '"' || c == '\'') == "all" {
            return Err("compose 里申请了危险的 capability,已拒绝".into());
        }
        let item = low.trim_start_matches("- ").trim_matches(|c| c == '"' || c == '\'');
        if item == "/" || item.starts_with("/:") || item.starts_with("/ :") {
            return Err("compose 里把宿主机根目录挂进了容器,已拒绝".into());
        }
        if kv("devices:").is_some() {
            return Err("compose 里挂载了宿主机设备,已拒绝".into());
        }
        if let Some(img) = raw.trim().strip_prefix("image:") {
            check_image(img.trim().trim_matches(|c| c == '"' || c == '\''))?;
        }
    }
    Ok(())
}

fn check_image(img: &str) -> Result<(), String> {
    if img.is_empty() || img.contains("${") {
        return Err("镜像名不合法".into());
    }
    let first = img.split('/').next().unwrap_or("");
    let has_host = img.contains('/') && (first.contains('.') || first.contains(':') || first == "localhost");
    if has_host && !REGISTRIES.contains(&first) {
        return Err(format!("镜像仓库 {first} 不在允许的范围内(Docker Hub、ghcr.io、quay.io)"));
    }
    Ok(())
}

// ---------------------------------------------------------------- 实例记录

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub app: String,
    pub schema: u32,
    pub version: String,
    pub project: String,
    pub container: String,
    #[serde(default)]
    pub cpus: String,
    #[serde(default)]
    pub memory: String,
    /// 非秘密参数。秘密参数在 `.env` 里,升级时不重写
    pub values: BTreeMap<String, String>,
    pub installed: u64,
}

impl Record {
    pub fn new(plan: &Plan, v: &Values) -> Record {
        let installed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        Record { app: plan.app.clone(), schema: SCHEMA, version: plan.version.clone(), project: plan.project.clone(), container: plan.container.clone(), cpus: v.cpus.clone(), memory: v.memory.clone(), values: plan.public_values.clone(), installed }
    }

    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default() + "\n"
    }

    pub fn parse(text: &str) -> Option<Record> {
        serde_json::from_str(text.trim()).ok()
    }

    /// 升级时的表单值:记录里的参数 + 新版本。秘密参数不在记录里,用占位(渲染只用它的 env 名)
    pub fn values_for(&self, a: &App, version: &str) -> Values {
        let mut params = self.values.clone();
        for p in a.params.iter().filter(|p| p.secret) {
            params.insert(p.id.clone(), "placeholder-secret".into());
        }
        Values { version: version.into(), project: self.project.clone(), container: self.container.clone(), cpus: self.cpus.clone(), memory: self.memory.clone(), params }
    }
}

/// 升级:大版本不同的数据库拒绝(数据目录格式不兼容,需要导出导入)
pub fn check_upgrade(a: &App, old: &str, new: &str) -> Result<(), String> {
    if old == new {
        return Err("已经是这个版本".into());
    }
    if a.kind.as_deref() == Some("database") {
        if let (Some(x), Some(y)) = (major(old), major(new)) {
            if x != y {
                return Err(format!("{} 主版本从 {x} 到 {y} 的数据格式不兼容,不能原地升级;请新装一个再迁移数据", a.name));
            }
        }
    }
    Ok(())
}

/// 逐行比较两份文本:`(' '|'+'|'-', 行)`。只给预览用,文本很短,用最长公共子序列
pub fn diff_lines(old: &str, new: &str) -> Vec<(char, String)> {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let mut t = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            t[i][j] = if a[i] == b[j] { t[i + 1][j + 1] + 1 } else { t[i + 1][j].max(t[i][j + 1]) };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push((' ', a[i].to_string()));
            i += 1;
            j += 1;
        } else if t[i + 1][j] >= t[i][j + 1] {
            out.push(('-', a[i].to_string()));
            i += 1;
        } else {
            out.push(('+', b[j].to_string()));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| ('-', l.to_string())));
    out.extend(b[j..].iter().map(|l| ('+', l.to_string())));
    out
}

// ---------------------------------------------------------------- 服务器脚本

const COMPOSE_UP: &str = "if docker compose version >/dev/null 2>&1; then DC='docker compose'; elif command -v docker-compose >/dev/null 2>&1; then DC=docker-compose; else echo '服务器上没有 docker compose,请先安装 compose 插件'; exit 1; fi";

fn port_checks(ports: &[u16]) -> String {
    ports.iter().map(|p| format!("l=$(ss -ltnpH \"sport = :{p}\" 2>/dev/null | head -n1); [ -z \"$l\" ] || {{ echo \"端口 {p} 已被占用:$l\"; exit 1; }}\n")).collect()
}

/// 内置 profile 在安装前要做的事(OpenResty:初始化数据目录、写主配置、默认站点、target.env)
fn profile_pre(a: &App, plan: &Plan) -> String {
    if a.profile.as_deref() != Some("openresty") {
        return String::new();
    }
    let (http, https) = (plan.public_values.get("http").and_then(|s| s.parse().ok()).unwrap_or(80), plan.public_values.get("https").and_then(|s| s.parse().ok()).unwrap_or(443));
    let target = crate::web::Target { container: plan.container.clone(), ports: (http, https), ..crate::web::Target::own() }.env_text().unwrap_or_default();
    let w = srvdata::WEB;
    format!(
        "echo \"==> 初始化数据目录 {r}\"\n{init}\n[ -f {w}/conf/nginx.conf ] || {{ {nginx}; }}\n{default}\n{target}\n",
        r = srvdata::ROOT,
        init = srvdata::init_script(),
        nginx = write_file_cmd(&format!("{w}/conf/nginx.conf"), &crate::web::nginx_conf(), "644"),
        default = write_file_cmd(&format!("{w}/conf/conf.d/00-default.conf"), &crate::web::default_conf(http, https), "644"),
        target = write_file_cmd(&format!("{w}/target.env"), &target, "644"),
    )
}

/// 安装任务脚本(经 `job_start_script` 脱离运行)
/// `purge`:目录里有上次的残留时,连同数据卷一起清掉再装(用户在界面上确认过)
pub fn install_script(a: &App, plan: &Plan, rec: &Record, purge: bool) -> String {
    let d = quote(&plan.dir);
    let n = quote(&plan.container);
    let mut s = format!("set -e\nD={d}; N={n}\n{COMPOSE_UP}\necho \"==> 检查\"\n");
    s += &port_checks(&plan.ports);
    s += &format!("if docker ps -a --format '{{{{.Names}}}}' | grep -qx \"$N\"; then echo \"容器名 $N 已被占用\"; exit 1; fi\n");
    // 目录里已有 compose.yml:多半是上次没装完的残留。还有容器或留着数据卷(沿用旧数据和旧密码)就拒绝,否则把残留目录挪开继续
    s += &format!("P={}\nPURGE={}\n", quote(&plan.project), if purge { 1 } else { 0 });
    s += &format!(
        "if [ -e \"$D/{COMPOSE_FILE}\" ]; then\n  if [ -n \"$(docker ps -aq --filter \"label=com.docker.compose.project=$P\")\" ]; then echo \"堆栈 $P 还有容器,请先在堆栈里卸载,或换一个名字\"; exit 1; fi\n  V=$(docker volume ls -q | grep -E \"^($N-|${{P}}_)\" | head -n3 | tr '\\n' ' ') || true\n  [ -z \"$(ls -A \"$D/data\" 2>/dev/null)\" ] || V=\"$V$D/data \"\n  if [ -n \"$V\" ] && [ \"$PURGE\" != 1 ]; then echo \"堆栈目录 $D 里已有 {COMPOSE_FILE},并且留着上次的数据卷或数据目录:$V\"; echo \"继续安装会沿用旧数据和旧密码。换一个名字,或先卸载并勾选「同时删除数据」\"; exit 1; fi\n  if [ \"$PURGE\" = 1 ]; then\n    echo \"清理上次的残留(目录和数据卷)\"; (cd \"$D\" && $DC down -v >/dev/null 2>&1) || true; [ -z \"$V\" ] || docker volume rm $V >/dev/null 2>&1 || true; rm -rf \"$D\"\n  else\n  B=\"{root}/trash/$(basename \"$D\")-$(date +%Y%m%d%H%M%S)\"; mkdir -p \"{root}/trash\"; echo \"上次没装完的残留目录挪到 $B\"; mv \"$D\" \"$B\"\n  fi\nfi\n",
        root = srvdata::ROOT
    );
    s += &profile_pre(a, plan);
    // 从这里开始如果失败,清掉这次建的目录和容器(没动过数据卷)
    s += "CREATED=1\ntrap 'rc=$?; if [ $rc -ne 0 ] && [ \"$CREATED\" = 1 ] && [ -n \"$D\" ] && [ \"$D\" != / ]; then cd /; [ -f \"$D/compose.yml\" ] && (cd \"$D\" && $DC down >/dev/null 2>&1) || true; docker rm -f \"$N\" >/dev/null 2>&1 || true; docker network rm \"${P}_default\" >/dev/null 2>&1 || true; rm -rf \"$D\"; echo \"安装失败,已清理 $D\"; fi' EXIT\n";
    s += "echo \"==> 写入配置\"\nmkdir -p \"$D\"\n";
    for (path, content, mode) in &plan.files {
        s += &format!("mkdir -p \"$D/$(dirname {p})\"\n{w}\n", p = quote(path), w = write_file_cmd(&format!("{}/{path}", plan.dir), content, mode));
    }
    if !plan.env.is_empty() {
        s += &format!("(umask 077; printf %s {} | base64 -d > \"$D/.env\")\n", quote(&b64(plan.env.as_bytes())));
    }
    s += &write_file_cmd(&format!("{}/{COMPOSE_FILE}", plan.dir), &plan.compose, "644");
    s += "\n";
    s += &write_file_cmd(&format!("{}/{RECORD_FILE}", plan.dir), &rec.json(), "644");
    s += "\ncd \"$D\"\necho \"==> 拉取镜像\"\n$DC pull\necho \"==> 启动容器\"\n$DC up -d\nsleep 2\n";
    s += &format!("docker inspect -f '{{{{.State.Running}}}}' \"$N\" | grep -q true || {{ echo \"容器没有在运行,日志:\"; docker logs --tail 20 \"$N\" 2>&1; exit 1; }}\n");
    if a.profile.as_deref() == Some("openresty") {
        s += "docker exec \"$N\" nginx -t\n";
    }
    s += &format!("echo \"==> {} 已启动\"\n", a.name);
    s
}

/// 升级任务脚本:备份旧 compose,写新的,拉取、重建;失败还原
pub fn upgrade_script(a: &App, plan: &Plan, rec: &Record) -> String {
    let d = quote(&plan.dir);
    let mut s = format!("set -e\nD={d}; N={}\n{COMPOSE_UP}\ncd \"$D\"\n[ -f {COMPOSE_FILE} ] || {{ echo '找不到 {COMPOSE_FILE}'; exit 1; }}\ncp -f {COMPOSE_FILE} {COMPOSE_FILE}.bak\n", quote(&plan.container));
    s += "echo \"==> 写入新配置\"\n";
    s += &write_file_cmd(&format!("{}/{COMPOSE_FILE}", plan.dir), &plan.compose, "644");
    s += "\n";
    s += "echo \"==> 拉取镜像\"\nif ! $DC pull; then cp -f compose.yml.bak compose.yml; echo '拉取失败,已还原配置'; exit 1; fi\necho \"==> 重建容器\"\n";
    s += "if ! $DC up -d; then cp -f compose.yml.bak compose.yml; $DC up -d || true; echo '启动失败,已还原到旧版本'; exit 1; fi\nsleep 2\n";
    s += "if ! docker inspect -f '{{.State.Running}}' \"$N\" | grep -q true; then docker logs --tail 20 \"$N\" 2>&1 || true; cp -f compose.yml.bak compose.yml; $DC up -d || true; echo '新版本没有跑起来,已还原到旧版本'; exit 1; fi\n";
    s += &write_file_cmd(&format!("{}/{RECORD_FILE}", plan.dir), &rec.json(), "644");
    s += &format!("\necho \"==> {} 已升级到 {}\"\n", a.name, plan.version);
    s
}

/// 卸载任务脚本。`delete_data`:同时删数据卷和堆栈目录(OpenResty 的站点、证书在网站数据目录里,不动)
pub fn uninstall_script(dir: &str, container: &str, delete_data: bool) -> Result<String, String> {
    let prefix = format!("{}/", srvdata::compose_dir(""));
    let prefix = prefix.trim_end_matches('/').to_string() + "/";
    if !dir.starts_with(&prefix) || dir.contains("..") || dir.len() <= prefix.len() || dir[prefix.len()..].contains('/') {
        return Err("堆栈目录不在 Otter 的目录下,不能删除".into());
    }
    let v = if delete_data { " -v" } else { "" };
    let mut s = format!("set -e\nD={}\n{COMPOSE_UP}\ncd \"$D\"\necho \"==> 停止并删除容器\"\n$DC down{v}\n", quote(dir));
    if delete_data {
        s += &format!("cd /\necho \"==> 删除堆栈目录\"\nrm -rf \"$D\"\ndocker rm -f {} >/dev/null 2>&1 || true\n", quote(container));
    }
    s += "echo \"==> 已卸载\"\n";
    Ok(s)
}

/// 读实例记录和当前 compose(升级预览用)
pub fn read_record_script(dir: &str) -> String {
    format!("D={}\ncat \"$D/{RECORD_FILE}\" 2>/dev/null || echo 'NO-RECORD'\necho '--COMPOSE--'\ncat \"$D/{COMPOSE_FILE}\" 2>/dev/null\n", quote(dir))
}

pub fn parse_record_output(raw: &str) -> Option<(Record, String)> {
    let (a, b) = raw.split_once("--COMPOSE--\n")?;
    Some((Record::parse(a)?, b.to_string()))
}

// ---------------------------------------------------------------- 后台任务

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

pub fn job_poll_script(offset: usize) -> String {
    let d = JOB_DIR;
    format!(
        "D={d}\necho \"ST=$(cat $D/status 2>/dev/null)\"\npid=$(cat $D/pid 2>/dev/null)\necho \"ALIVE=$([ -n \"$pid\" ] && ps -eo pgid= 2>/dev/null | awk -v g=\"$pid\" '$1==g{{f=1}} END{{exit !f}}' && echo 1 || echo 0)\"\necho \"HAS=$([ -d $D ] && echo 1 || echo 0)\"\necho '--LOG--'\ntail -c +{} $D/log 2>/dev/null\nexit 0\n",
        offset + 1
    )
}

/// 进页面时探测服务器上有没有在跑的任务:先输出 `META=`(任务脚本第一行的 `# otter-job: 应用id|容器名|标题`,老任务没有),再是 `job_poll_script(0)` 的内容
pub fn job_probe_script() -> String {
    format!("D={JOB_DIR}\necho \"META=$(head -n1 $D/run.sh 2>/dev/null)\"\n{}", job_poll_script(0).replacen(&format!("D={JOB_DIR}\n"), "", 1))
}

/// 任务脚本第一行的标记(`job_probe_script` 读它,接上服务器上已有的任务)
pub fn job_meta_line(app_id: &str, container: &str, title: &str) -> String {
    format!("# otter-job: {app_id}|{container}|{}\n", title.replace(['|', '\n'], " "))
}

/// `job_probe_script` 的 META 行 → (应用 id, 容器名, 标题)
pub fn parse_job_meta(raw: &str) -> Option<(String, String, String)> {
    let line = raw.lines().find_map(|l| l.strip_prefix("META="))?;
    let rest = line.trim().strip_prefix("# otter-job:")?.trim();
    let mut it = rest.splitn(3, '|');
    Some((it.next()?.to_string(), it.next()?.to_string(), it.next()?.to_string()))
}

pub fn job_cancel_script() -> String {
    format!("D={JOB_DIR}; pid=$(cat $D/pid 2>/dev/null); [ -n \"$pid\" ] && {{ kill -s TERM -- -$pid 2>/dev/null || kill -s TERM $pid 2>/dev/null; }}; sleep 1; echo OK")
}

pub fn job_clear_script() -> String {
    format!("rm -rf {JOB_DIR}; echo OK")
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbinst::DbKind;

    fn app(id: &str) -> App {
        embedded().apps.into_iter().find(|a| a.id == id).unwrap()
    }

    fn vals(a: &App, version: &str, port: &str, public: bool) -> Values {
        let mut v = default_values(a, &[], &[]);
        v.version = version.into();
        v.project = "pg-main".into();
        v.container = "pg-main".into();
        v.params.insert("port".into(), port.into());
        v.params.insert("password".into(), "abcDEF123-_xyz".into());
        v.params.insert("public".into(), public.to_string());
        v
    }

    #[test]
    fn job_meta_roundtrip() {
        let m = job_meta_line("postgres", "pg-main", "安装 PostgreSQL");
        assert_eq!(parse_job_meta(&format!("META={}ST=\n", m)), Some(("postgres".into(), "pg-main".into(), "安装 PostgreSQL".into())));
        assert_eq!(parse_job_meta("META=\nST=\n"), None);
        let p = job_probe_script();
        assert!(p.starts_with("D=") && p.contains("META=") && p.contains("--LOG--") && p.matches("D=").count() == 1);
    }

    #[test]
    fn embedded_catalog_is_valid() {
        let c = embedded();
        assert_eq!(c.schema, 1);
        for id in ["postgres", "mysql", "mariadb", "redis", "openresty", "uptime-kuma", "memos", "minio"] {
            assert!(c.apps.iter().any(|a| a.id == id), "缺 {id}");
        }
    }

    /// 目录渲染出的数据库 compose,和原来写死的 `dbinstall::Spec::compose()` 只差 `otter.app` 标签一行
    #[test]
    fn db_compose_matches_hardcoded_spec() {
        let cases = [("postgres", DbKind::Postgres, "17", 15432u16, false, "", ""), ("postgres", DbKind::Postgres, "18.1", 15432, true, "1.5", "512m"), ("mysql", DbKind::Mysql, "8.4", 13306, false, "", "2G"), ("mariadb", DbKind::Mariadb, "11.8", 13307, true, "2", ""), ("redis", DbKind::Redis, "7", 16379, false, "0.5", "256m")];
        for (id, kind, ver, port, public, cpus, mem) in cases {
            let a = app(id);
            let mut v = vals(&a, ver, &port.to_string(), public);
            v.cpus = cpus.into();
            v.memory = mem.into();
            let plan = render(&a, &v).unwrap_or_else(|e| panic!("{id} {ver}: {e}"));
            let spec = crate::dbinstall::Spec { kind, version: ver, project: "pg-main", container: "pg-main", port, public, password: "abcDEF123-_xyz", cpus, memory: mem };
            let want = spec.compose();
            let got: String = plan.compose.lines().filter(|l| !l.contains("otter.app:")).map(|l| format!("{l}\n")).collect();
            assert_eq!(got, want, "{id} {ver}");
            assert!(plan.compose.contains(&format!("otter.app: {id}")));
            assert_eq!(plan.env, spec.env());
            assert_eq!(plan.env_masked, spec.env_masked());
        }
    }

    /// OpenResty 渲染和 `web::InstallSpec::compose()` 只差 `otter.app` 标签
    #[test]
    fn openresty_compose_matches_install_spec() {
        let a = app("openresty");
        let mut v = default_values(&a, &[], &[]);
        v.version = "alpine".into();
        v.project = "otter-openresty".into();
        v.container = "otter-openresty".into();
        v.cpus = "1".into();
        v.memory = "256m".into();
        v.params.insert("http".into(), "18080".into());
        v.params.insert("https".into(), "18443".into());
        let plan = render(&a, &v).unwrap();
        let spec = crate::web::InstallSpec { version: "alpine".into(), project: "otter-openresty".into(), container: "otter-openresty".into(), http: 18080, https: 18443, cpus: "1".into(), memory: "256m".into() };
        let got: String = plan.compose.lines().filter(|l| !l.contains("otter.app:")).map(|l| format!("{l}\n")).collect();
        assert_eq!(got, spec.compose());
        assert_eq!(plan.ports, vec![18080, 18443]);
        assert_eq!(plan.dir, spec.dir());
    }

    #[test]
    fn param_validation() {
        let a = app("postgres");
        let mut v = vals(&a, "17", "15432", false);
        v.params.insert("port".into(), "80".into());
        assert!(render(&a, &v).unwrap_err().contains("端口"));
        v.params.insert("port".into(), "15432".into());
        v.params.insert("password".into(), "short".into());
        assert!(render(&a, &v).is_err());
        v.params.insert("password".into(), "abcDEF123-_xyz".into());
        v.version = "bad tag".into();
        assert!(render(&a, &v).is_err());
        let m = app("memos");
        let mut v = default_values(&m, &[], &[]);
        v.cpus = "9999".into();
        assert!(render(&m, &v).is_err());
    }

    #[test]
    fn template_rules() {
        let vars: BTreeMap<String, String> = [("a".to_string(), "X".to_string())].into();
        let envs: BTreeMap<String, String> = [("pw".to_string(), "DB_PASSWORD".to_string())].into();
        assert_eq!(substitute("a: ${a} $$HOME ${env.pw}\n", &vars, &envs, &[]).unwrap(), "a: X $$HOME ${DB_PASSWORD}\n");
        assert!(substitute("a: ${nope}", &vars, &envs, &[]).is_err());
        assert!(substitute("a: $HOME", &vars, &envs, &[]).is_err());
        assert!(substitute("a: ${a", &vars, &envs, &[]).is_err());
        assert_eq!(substitute("  x:\n    ${limits}\n  y\n", &vars, &envs, &[]).unwrap(), "  x:\n  y\n");
        assert_eq!(substitute("  x:\n    ${limits}\n", &vars, &envs, &["cpus: \"1\"".into(), "mem_limit: 1g".into()]).unwrap(), "  x:\n    cpus: \"1\"\n    mem_limit: 1g\n");
    }

    #[test]
    fn danger_scan() {
        assert!(scan_compose("services:\n  a:\n    image: nginx:1\n", false).is_ok());
        for bad in ["    privileged: true", "    volumes:\n      - /var/run/docker.sock:/var/run/docker.sock", "    pid: host", "    network_mode: host", "    cap_add:\n      - SYS_ADMIN", "    cap_add:\n      - ALL", "    volumes:\n      - /:/host", "    image: evil.example.com/x/y:1", "    devices:\n      - /dev/mem"] {
            assert!(scan_compose(&format!("services:\n  a:\n    image: nginx:1\n{bad}\n"), false).is_err(), "{bad}");
        }
        assert!(scan_compose("services:\n  a:\n    image: ghcr.io/x/y:1\n    network_mode: host\n", true).is_ok());
        assert!(scan_compose("services:\n  a:\n    privileged: false\n    image: quay.io/x/y:1\n", false).is_ok());
        assert!(scan_compose("services:\n  a:\n    image: quay.io.evil.com/x/y:1\n", false).is_err());
    }

    #[test]
    fn versions() {
        let names: Vec<String> = ["latest", "17", "17.5", "16.9", "9.6.24", "17.5-alpine", "18.0", "17.10"].map(String::from).to_vec();
        let a = app("postgres");
        let list = select_versions(&a, &names).unwrap();
        assert_eq!(list, ["18.0", "17.10", "17.5", "17", "16.9", "9.6.24"]);
        assert_eq!(list[default_index(&a, &list)], "18.0");
        let m = app("mysql");
        let names: Vec<String> = ["9.1", "8.4.5", "8.4", "8.0.40"].map(String::from).to_vec();
        let list = select_versions(&m, &names).unwrap();
        assert_eq!(list[default_index(&m, &list)], "8.4.5");
        assert!(!fallback_version(&m).ends_with("8.4"));
        // 多页 JSON 连在一起
        let raw = r#"{"results":[{"name":"1.27.1.2-alpine"},{"name":"1.27.1.2-3-alpine"},{"name":"alpine"}]} {"results":[{"name":"1.25.3.1-alpine"},{"name":"1.27.1.2-fat"}]}"#;
        let o = app("openresty");
        let list = select_versions(&o, &parse_tag_names(raw)).unwrap();
        assert_eq!(list, ["alpine", "1.27.1.2-alpine", "1.25.3.1-alpine"]);
        assert_eq!(list[default_index(&o, &list)], "1.27.1.2-alpine");
        let mn = app("minio");
        let list = select_versions(&mn, &["RELEASE.2025-01-02T03-04-05Z".into(), "RELEASE.2024-12-01T00-00-00Z".into(), "latest".into()]).unwrap();
        assert_eq!(list[0], "RELEASE.2025-01-02T03-04-05Z");
    }

    #[test]
    fn upgrade_rules_and_diff() {
        let pg = app("postgres");
        assert!(check_upgrade(&pg, "16.2", "17.1").is_err());
        assert!(check_upgrade(&pg, "16.2", "16.9").is_ok());
        assert!(check_upgrade(&pg, "16.2", "16.2").is_err());
        assert!(check_upgrade(&app("memos"), "0.24.0", "0.25.0").is_ok());
        let d = diff_lines("a\nb\nc\n", "a\nx\nc\n");
        assert_eq!(d, vec![(' ', "a".into()), ('-', "b".into()), ('+', "x".into()), (' ', "c".into())]);
    }

    #[test]
    fn record_roundtrip_and_upgrade_values() {
        let a = app("postgres");
        let v = vals(&a, "17", "15432", false);
        let plan = render(&a, &v).unwrap();
        let rec = Record::new(&plan, &v);
        assert!(!rec.json().contains("abcDEF123"), "记录里不能有密码");
        let back = Record::parse(&rec.json()).unwrap();
        assert_eq!(back, rec);
        let nv = back.values_for(&a, "17.5");
        let plan2 = render(&a, &nv).unwrap();
        let strip = |s: &str| s.replace("17.5", "17");
        assert_eq!(strip(&plan2.compose), plan.compose);
    }

    #[test]
    fn scripts_shape() {
        let a = app("postgres");
        let v = vals(&a, "17", "15432", false);
        let plan = render(&a, &v).unwrap();
        let rec = Record::new(&plan, &v);
        let s = install_script(&a, &plan, &rec, false);
        if let Ok(dir) = std::env::var("OTTER_DUMP_INSTALL") {
            std::fs::write(format!("{dir}/install.sh"), &s).unwrap();
        }
        let chk = std::process::Command::new("bash").arg("-n").arg("-c").arg(&s).status().unwrap();
        assert!(chk.success(), "install 脚本语法错误");

        assert!(s.contains("15432") && s.contains("$DC up -d") && s.contains("umask 077"));
        assert!(!s.contains("abcDEF123"), "密码只以 base64 出现在脚本里");
        let u = upgrade_script(&a, &plan, &rec);
        assert!(u.contains("compose.yml.bak"));
        assert!(uninstall_script(&plan.dir, "pg-main", true).unwrap().contains("down -v"));
        assert!(uninstall_script("/etc", "x", true).is_err());
        assert!(uninstall_script(&format!("{}/../..", srvdata::compose_dir("x")), "x", true).is_err());
        assert!(!uninstall_script(&plan.dir, "pg-main", false).unwrap().contains("rm -rf"));
    }

    #[test]
    fn bad_catalogs_are_rejected() {
        let ok = EMBEDDED;
        assert!(parse_catalog(&ok.replace("\"schema\": 1", "\"schema\": 99")).unwrap_err().contains("升级"));
        assert!(parse_catalog("{").is_err());
        assert!(parse_catalog(&ok.replace("privileged-placeholder", "x")).is_ok());
        let evil = r#"{"schema":1,"apps":[{"id":"x","name":"X","versions":{"source":"none","tag":"1"},"compose":"services:\n  a:\n    image: x:${version}\n    privileged: true\n"}]}"#;
        assert!(parse_catalog(evil).unwrap_err().contains("privileged"));
        let evil2 = r#"{"schema":1,"apps":[{"id":"x","name":"X","versions":{"source":"none","tag":"1"},"profile":"shell","compose":"services:\n"}]}"#;
        assert!(parse_catalog(evil2).is_err());
    }
}

/// 把 `uname -m` / `docker version` 里的各种写法归一成 `amd64` / `arm64` / `arm` 等
pub fn norm_arch(a: &str) -> String {
    match a.trim().to_lowercase().as_str() {
        "x86_64" | "x86-64" | "amd64" => "amd64".into(),
        "aarch64" | "arm64" | "arm64v8" => "arm64".into(),
        "armv7l" | "armv7" | "armhf" | "arm" => "arm".into(),
        o => o.to_string(),
    }
}

impl App {
    /// 这台服务器能不能装:没声明架构的应用不限;服务器架构未知时先显示
    pub fn supports(&self, server_arch: &str) -> bool {
        let sa = norm_arch(server_arch);
        sa.is_empty() || self.arch.is_empty() || self.arch.iter().any(|x| norm_arch(x) == sa)
    }
}

#[cfg(test)]
mod arch_tests {
    use super::*;
    #[test]
    fn arch_filter() {
        let cat = current();
        let a = cat.apps.iter().find(|a| a.id == "postgres").unwrap();
        assert!(a.supports("") && a.supports("x86_64") && a.supports("aarch64"));
        let mut b = a.clone();
        b.arch = vec!["amd64".into()];
        assert!(b.supports("x86_64") && b.supports("") && !b.supports("aarch64"));
        assert_eq!(norm_arch("arm64v8"), "arm64");
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    #[test]
    fn fallback_versions_from_catalog() {
        let cat = parse_catalog(EMBEDDED).unwrap();
        let pg = cat.apps.iter().find(|a| a.id == "postgres").unwrap();
        let l = fallback_versions(pg).unwrap();
        assert!(l.len() > 5 && l.iter().all(|t| regex::Regex::new(r"^\d+(\.\d+){0,2}$").unwrap().is_match(t)));
        // 每个带备份的应用,默认版本都不是浮动标签
        for a in cat.apps.iter().filter(|a| fallback_versions(a).is_some()) {
            let l = fallback_versions(a).unwrap();
            let d = fallback_version(a);
            eprintln!("default {} = {d}", a.id);
            assert!(!l.iter().any(|t| t.starts_with(&format!("{d}."))), "{} 默认 {d} 是浮动标签", a.id);
            assert_ne!(d, "latest");
        }
        // 静态/无版本的应用没有备份
        let none = cat.apps.iter().find(|a| matches!(a.versions, Versions::None { .. }));
        assert!(none.is_none_or(|a| fallback_versions(a).is_none()));
    }
}
