//! 节点导入(CSV):解析、校验、预览弹窗、写库。
//!
//! 格式是 Termius 导出的超集(多了 `Note`、`Key`、`Agent` 三列),设计见 `docs/modules/import.md`。
//! 解析部分(`parse_csv`)不依赖界面,有单测;弹窗只做预览和确认,点确认才一次事务写库。

use std::collections::HashSet;

use gpui_kit::assets::IconName;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zeroize::Zeroizing;

use crate::mock::{Group, Node};
use crate::node_form::ctl_btn;
use crate::terminal::ssh::Conn;
use crate::ui::{Glass as _, GlassHover as _, GlassStyle, modal_anim};
use crate::{Home, MONO, c, icon, store, t};

/// 空分组列时落到的分组(迁移 v1 建的默认分组同名)
pub const DEFAULT_GROUP: &str = "未分组";

/// 「下载模板」写出的文件:表头 + 两行示例
pub const TEMPLATE: &str = "Groups,Label,Tags,Hostname/IP,Protocol,Port,Username,Password,Note,Key,Agent\n\
Project/Dev,Web Server,\"dev,web\",192.168.1.1,ssh,22,admin,password123,开发环境,,\n\
Project/Prod,Database,\"prod,db\",db.example.com,ssh,2222,root,,主库,~/.ssh/id_ed25519,\n";

// ---- 解析 --------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum RowStatus {
    /// 会新增
    New,
    /// 不论分组,主机 + 端口 + 用户都相同(库里已有,或文件里前面已出现)
    Dup,
    /// 协议不是 ssh
    Unsupported(String),
    Error(String),
}

/// 一行里的私钥来源
pub enum KeyRef {
    None,
    /// 钥匙串里的密钥
    Keychain { id: i64, name: String },
    /// 本机私钥文件(解析时已读出并校验)
    File { path: String, pem: Zeroizing<String> },
}

pub struct ImportRow {
    /// CSV 里的行号(从 1 起,表头是第 1 行)
    pub line: usize,
    pub status: RowStatus,
    pub group: String,
    /// 分组在库里还不存在,导入时会新建
    pub new_group: bool,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub tags: Vec<String>,
    pub note: String,
    pub password: Zeroizing<String>,
    pub key: KeyRef,
    pub agent: bool,
    /// 预览里是否勾选(只有 New 和 Dup 能勾)
    pub selected: bool,
}

impl ImportRow {
    pub fn selectable(&self) -> bool {
        matches!(self.status, RowStatus::New | RowStatus::Dup)
    }
}

/// 解析时要对照的现有数据
#[derive(Default)]
pub struct Existing {
    pub groups: Vec<String>,
    /// (主机小写, 端口, 用户),不分分组
    pub nodes: HashSet<(String, u16, String)>,
    /// 钥匙串里的 (id, 名称)
    pub keys: Vec<(i64, String)>,
}

impl Existing {
    pub fn from_home(groups: &[Group], keys: &[store::KeyRow]) -> Self {
        let mut e = Existing { groups: groups.iter().map(|g| g.name.clone()).collect(), keys: keys.iter().map(|k| (k.id, k.name.clone())).collect(), ..Default::default() };
        for g in groups {
            for n in &g.nodes {
                let c = Conn::parse(&n.user_host);
                e.nodes.insert((c.host.to_lowercase(), c.port, c.user));
            }
        }
        e
    }
}

/// 表头名 → 列(不区分大小写,接受常见别名)
#[derive(Clone, Copy, PartialEq)]
enum Col {
    Group,
    Label,
    Tags,
    Host,
    Protocol,
    Port,
    User,
    Password,
    Note,
    Key,
    Agent,
}

fn col_of(header: &str) -> Option<Col> {
    Some(match header.trim().to_lowercase().as_str() {
        "groups" | "group" => Col::Group,
        "label" | "name" => Col::Label,
        "tags" | "tag" => Col::Tags,
        "hostname/ip" | "hostname" | "host" | "ip" | "address" => Col::Host,
        "protocol" => Col::Protocol,
        "port" => Col::Port,
        "username" | "user" => Col::User,
        "password" => Col::Password,
        "note" | "notes" => Col::Note,
        "key" => Col::Key,
        "agent" => Col::Agent,
        _ => return None,
    })
}

fn truthy(s: &str) -> bool {
    matches!(s.trim().to_lowercase().as_str(), "yes" | "y" | "true" | "1" | "on")
}

fn split_tags(s: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for t in s.split(|c| c == ',' || c == '，' || c == ';') {
        let t = t.trim();
        if !t.is_empty() && !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// 解析 Key 列:先当钥匙串密钥名,再当本机私钥文件路径。
fn resolve_key(v: &str, ex: &Existing) -> Result<KeyRef, String> {
    if let Some((id, name)) = ex.keys.iter().find(|(_, n)| n == v).or_else(|| ex.keys.iter().find(|(_, n)| n.eq_ignore_ascii_case(v))) {
        return Ok(KeyRef::Keychain { id: *id, name: name.clone() });
    }
    let path = crate::terminal::ssh::expand(v);
    let pem = std::fs::read_to_string(&path).map_err(|_| format!("私钥「{v}」既不是钥匙串里的密钥,也不是可读取的文件"))?;
    crate::keychain::inspect(&pem).map_err(|e| format!("{v}:{e}"))?;
    Ok(KeyRef::File { path: v.to_string(), pem: Zeroizing::new(pem) })
}

/// 解析 CSV 文本。表头缺少主机列时整体报错;其余问题记在各行的状态里,不中断。
pub fn parse_csv(text: &str, ex: &Existing) -> Result<Vec<ImportRow>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rd = csv::ReaderBuilder::new().has_headers(true).flexible(true).trim(csv::Trim::None).from_reader(text.as_bytes());
    let headers = rd.headers().map_err(|e| format!("读不了表头:{e}"))?.clone();
    let cols: Vec<Option<Col>> = headers.iter().map(col_of).collect();
    if !cols.contains(&Some(Col::Host)) {
        return Err("表头里没有主机列(Hostname/IP)。可以点「下载模板」查看格式".into());
    }
    let mut seen = ex.nodes.clone();
    let mut rows = vec![];
    for rec in rd.records() {
        let rec = match rec {
            Ok(r) => r,
            Err(e) => return Err(format!("CSV 格式有误:{e}")),
        };
        let line = rec.position().map_or(rows.len() + 2, |p| p.line() as usize);
        let get = |want: Col| -> String { cols.iter().position(|c| *c == Some(want)).and_then(|i| rec.get(i)).unwrap_or("").trim().to_string() };
        if rec.iter().all(|f| f.trim().is_empty()) {
            continue;
        }
        let group = Some(get(Col::Group)).filter(|g| !g.is_empty()).unwrap_or_else(|| DEFAULT_GROUP.to_string());
        let host = get(Col::Host);
        let protocol = get(Col::Protocol);
        let mut row = ImportRow {
            line,
            status: RowStatus::New,
            new_group: !ex.groups.contains(&group),
            name: Some(get(Col::Label)).filter(|n| !n.is_empty()).unwrap_or_else(|| host.clone()),
            group,
            host,
            port: 22,
            user: Some(get(Col::User)).filter(|u| !u.is_empty()).unwrap_or_else(|| "root".into()),
            tags: split_tags(&get(Col::Tags)),
            note: get(Col::Note),
            password: Zeroizing::new(rec.get(cols.iter().position(|c| *c == Some(Col::Password)).unwrap_or(usize::MAX)).unwrap_or("").to_string()),
            key: KeyRef::None,
            agent: truthy(&get(Col::Agent)),
            selected: false,
        };
        let err = if !protocol.is_empty() && !protocol.eq_ignore_ascii_case("ssh") {
            row.status = RowStatus::Unsupported(format!("{protocol} 协议"));
            None
        } else if row.host.is_empty() {
            Some("缺少主机(Hostname/IP)".to_string())
        } else {
            let port = get(Col::Port);
            if !port.is_empty() {
                match port.parse::<u16>().ok().filter(|p| *p > 0) {
                    Some(p) => row.port = p,
                    None => {
                        row.status = RowStatus::Error(format!("端口「{port}」不是 1–65535 的整数"));
                    }
                }
            }
            let key = get(Col::Key);
            if matches!(row.status, RowStatus::New) && !key.is_empty() {
                match resolve_key(&key, ex) {
                    Ok(k) => row.key = k,
                    Err(e) => row.status = RowStatus::Error(e),
                }
            }
            None
        };
        if let Some(e) = err {
            row.status = RowStatus::Error(e);
        }
        if matches!(row.status, RowStatus::New) && !seen.insert((row.host.to_lowercase(), row.port, row.user.clone())) {
            row.status = RowStatus::Dup;
        }
        row.selected = matches!(row.status, RowStatus::New);
        rows.push(row);
    }
    Ok(rows)
}

// ---- ~/.ssh/config -------------------------------------------------------------

/// 从 ssh config 导入时落到的分组
pub const SSH_GROUP: &str = "SSH Config";

/// OpenSSH 风格通配:`*` 任意串,`?` 单字符。
fn wild(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    fn go(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => (0..=t.len()).any(|i| go(rest, &t[i..])),
            Some(('?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    go(&p, &t)
}

/// 一行配置拆成 (关键字小写, 值):接受 `Key value` 和 `Key=value`,值可带引号。
fn kv(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let i = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let (k, rest) = line.split_at(i);
    let v = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=').trim();
    Some((k.to_lowercase(), v.trim_matches('"').to_string()))
}

/// 展开 `Include`(只一层),返回 (行号, 行文本)。
fn flatten_config(text: &str, ssh_dir: &std::path::Path) -> Vec<(usize, String)> {
    let mut out = vec![];
    for (n, line) in text.lines().enumerate() {
        match kv(line) {
            Some((k, v)) if k == "include" => {
                for pat in v.split_whitespace() {
                    let path = if pat.starts_with('/') || pat.starts_with('~') { crate::terminal::ssh::expand(pat) } else { ssh_dir.join(pat) };
                    let (dir, name) = (path.parent().map(|d| d.to_path_buf()).unwrap_or_default(), path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
                    let mut files: Vec<_> = std::fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|f| f.is_file() && f.file_name().is_some_and(|n| wild(&name, &n.to_string_lossy()))).collect();
                    files.sort();
                    for f in files {
                        if let Ok(t) = std::fs::read_to_string(&f) {
                            out.extend(t.lines().enumerate().map(|(i, l)| (i + 1, l.to_string())));
                        }
                    }
                }
            }
            _ => out.push((n + 1, line.to_string())),
        }
    }
    out
}

/// 解析 ssh config:每个不含通配符的 Host 别名一行;按 OpenSSH 规则「先出现的值生效」。
pub fn parse_ssh_config(text: &str, ssh_dir: &std::path::Path, ex: &Existing) -> Vec<ImportRow> {
    struct Block {
        pats: Vec<String>,
        line: usize,
        opts: Vec<(String, String)>,
    }
    // 第一个 Host 之前的行对所有主机生效
    let mut blocks = vec![Block { pats: vec!["*".into()], line: 0, opts: vec![] }];
    let mut in_match = false;
    for (line, l) in flatten_config(text, ssh_dir) {
        let Some((k, v)) = kv(&l) else { continue };
        match k.as_str() {
            "host" => {
                in_match = false;
                blocks.push(Block { pats: v.split_whitespace().map(|p| p.trim_matches('"').to_string()).collect(), line, opts: vec![] });
            }
            "match" => in_match = true,
            _ if !in_match => blocks.last_mut().expect("至少有一块").opts.push((k, v)),
            _ => {}
        }
    }
    let matches = |b: &Block, alias: &str| {
        let hit = b.pats.iter().any(|p| !p.starts_with('!') && wild(p, alias));
        hit && !b.pats.iter().any(|p| p.strip_prefix('!').is_some_and(|n| wild(n, alias)))
    };
    let mut seen = ex.nodes.clone();
    let mut done: HashSet<String> = HashSet::new();
    let mut rows = vec![];
    for b in &blocks {
        for alias in b.pats.iter().filter(|p| !p.contains(['*', '?', '!'])) {
            if !done.insert(alias.clone()) {
                continue;
            }
            let get = |key: &str| blocks.iter().filter(|x| matches(x, alias)).flat_map(|x| x.opts.iter()).find(|(k, _)| k == key).map(|(_, v)| v.clone());
            let host = get("hostname").map(|h| h.replace("%h", alias)).unwrap_or_else(|| alias.clone());
            let mut row = ImportRow {
                line: b.line,
                status: RowStatus::New,
                group: SSH_GROUP.to_string(),
                new_group: !ex.groups.iter().any(|g| g == SSH_GROUP),
                name: alias.clone(),
                host,
                port: 22,
                user: get("user").unwrap_or_else(|| "root".into()),
                tags: vec![],
                note: String::new(),
                password: Zeroizing::new(String::new()),
                key: KeyRef::None,
                agent: false,
                selected: false,
            };
            if let Some(j) = get("proxyjump").filter(|j| !j.eq_ignore_ascii_case("none")) {
                row.status = RowStatus::Unsupported(format!("ProxyJump {j}"));
            } else if get("proxycommand").is_some_and(|c| !c.eq_ignore_ascii_case("none")) {
                row.status = RowStatus::Unsupported("ProxyCommand".to_string());
            }
            if let Some(p) = get("port") {
                match p.parse::<u16>().ok().filter(|p| *p > 0) {
                    Some(p) => row.port = p,
                    None => row.status = RowStatus::Error(format!("端口「{p}」不是 1–65535 的整数")),
                }
            }
            if matches!(row.status, RowStatus::New) {
                if let Some(f) = get("identityfile").filter(|f| !f.eq_ignore_ascii_case("none")) {
                    match resolve_key(&f, ex) {
                        Ok(k) => row.key = k,
                        Err(e) => row.status = RowStatus::Error(e),
                    }
                }
            }
            if matches!(row.status, RowStatus::New) && !seen.insert((row.host.to_lowercase(), row.port, row.user.clone())) {
                row.status = RowStatus::Dup;
            }
            row.selected = matches!(row.status, RowStatus::New);
            rows.push(row);
        }
    }
    rows
}

// ---- 界面状态 ------------------------------------------------------------------

const IMPORT_NARROW_W: f32 = 560.0;
const IMPORT_WIDE_W: f32 = 960.0;
/// 三步窗口的固定高度(切换时高度也做过渡,所以不能随内容变)
const IMPORT_METHODS_H: f32 = 272.0;
const IMPORT_PICKER_H: f32 = 344.0;
const IMPORT_PREVIEW_H: f32 = 860.0;

pub struct ImportDialog {
    /// 文件名(标题下的说明)
    pub file: String,
    pub rows: Vec<ImportRow>,
    /// 整体错误(读不了文件、表头不对),有它时列表为空
    pub error: Option<SharedString>,
    /// false = 还在「选择文件」那一步;true = 已解析,显示预览
    pub loaded: bool,
    /// 选了 CSV 方式(false 且未 loaded = 还在选导入方式)
    pub csv: bool,
    /// 上一步窗口尺寸,切换步骤时从它过渡到当前尺寸(新开窗口时等于当前宽度)
    pub from: (f32, f32),
}

impl ImportDialog {
    /// 当前步骤的窗口尺寸
    pub fn size(&self) -> (f32, f32) {
        if self.loaded {
            (IMPORT_WIDE_W, IMPORT_PREVIEW_H)
        } else if self.csv {
            (IMPORT_NARROW_W, IMPORT_PICKER_H)
        } else {
            (IMPORT_NARROW_W, IMPORT_METHODS_H)
        }
    }

    fn count(&self, f: impl Fn(&ImportRow) -> bool) -> usize {
        self.rows.iter().filter(|r| f(r)).count()
    }
    fn selected(&self) -> usize {
        self.count(|r| r.selected)
    }
}

impl Home {
    /// 节点页「导入」:打开导入窗口(先是选文件这一步)。
    pub(crate) fn open_import(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let from = self.import.as_ref().map_or((IMPORT_NARROW_W, IMPORT_METHODS_H), |d| d.size());
        self.import = Some(ImportDialog { file: String::new(), rows: vec![], error: None, loaded: false, csv: false, from });
        cx.notify();
    }

    /// 选了 CSV:进入选文件这一步。
    fn import_choose_csv(&mut self, cx: &mut Context<Self>) {
        if let Some(d) = &mut self.import {
            d.from = d.size();
            d.csv = true;
            d.error = None;
        }
        cx.notify();
    }

    /// 选了 ~/.ssh/config:读取并解析成预览(不写库)。
    pub(crate) fn load_ssh_config(&mut self, cx: &mut Context<Self>) {
        let dir = crate::terminal::ssh::expand("~/.ssh");
        let path = dir.join("config");
        let ex = Existing::from_home(&self.groups, &self.keys);
        let res = std::fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败:{e}", path.display())).and_then(|t| {
            let rows = parse_ssh_config(&t, &dir, &ex);
            if rows.is_empty() { Err("~/.ssh/config 里没有可导入的 Host(带通配符的 Host 会被忽略)".to_string()) } else { Ok(rows) }
        });
        let from = self.import.as_ref().map_or((IMPORT_NARROW_W, IMPORT_METHODS_H), |d| d.size());
        self.import = Some(match res {
            Ok(rows) => ImportDialog { file: "~/.ssh/config".into(), rows, error: None, loaded: true, csv: false, from },
            Err(e) => ImportDialog { file: String::new(), rows: vec![], error: Some(e.into()), loaded: false, csv: false, from },
        });
        cx.notify();
    }

    /// 弹系统文件选择器选 CSV。
    fn pick_import_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("选择 CSV 文件".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |this, cx| this.load_import_file(&path, cx));
        })
        .detach();
    }

    /// 读文件并解析成预览(不写库)。
    pub(crate) fn load_import_file(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let ex = Existing::from_home(&self.groups, &self.keys);
        let parsed = std::fs::read(path).map_err(|e| format!("读取 {} 失败:{e}", path.display())).and_then(|b| String::from_utf8(b).map_err(|_| "文件不是 UTF-8 编码,请在表格软件里另存为「CSV UTF-8」".to_string())).and_then(|s| parse_csv(&s, &ex));
        let from = self.import.as_ref().map_or((IMPORT_NARROW_W, IMPORT_METHODS_H), |d| d.size());
        self.import = Some(match parsed {
            Ok(rows) => ImportDialog { file, rows, error: None, loaded: true, csv: true, from },
            Err(e) => ImportDialog { file, rows: vec![], error: Some(e.into()), loaded: false, csv: true, from },
        });
        cx.notify();
    }

    fn toggle_import_row(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Some(r) = self.import.as_mut().and_then(|d| d.rows.get_mut(i)).filter(|r| r.selectable()) {
            r.selected = !r.selected;
            cx.notify();
        }
    }

    fn save_template(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = std::env::var_os("HOME").map(std::path::PathBuf::from).map(|h| h.join("Downloads")).unwrap_or_default();
        let rx = cx.prompt_for_new_path(&dir, Some("otter_import.csv"));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else { return };
            let res = std::fs::write(&path, TEMPLATE);
            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(()) => crate::logs::push(crate::logs::Level::Info, "导入", format!("模板已保存到 {}", path.display())),
                    Err(e) => {
                        crate::logs::push(crate::logs::Level::Error, "导入", format!("保存模板失败: {e}"));
                        if let Some(d) = &mut this.import {
                            d.error = Some(format!("保存模板失败:{e}").into());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 确认导入:加密秘密、一个事务写库,再把新节点并进内存列表。
    fn submit_import(&mut self, cx: &mut Context<Self>) {
        let Some(d) = &self.import else { return };
        let chosen: Vec<&ImportRow> = d.rows.iter().filter(|r| r.selected && r.selectable()).collect();
        if chosen.is_empty() {
            return;
        }
        if self.store.is_none() {
            return self.import_error("演示模式不保存节点", cx);
        }
        let items: Vec<store::ImportItem> = chosen
            .iter()
            .map(|r| {
                let mut secrets = vec![];
                if !r.password.is_empty() {
                    secrets.push((store::SLOT_PASSWORD.to_string(), r.password.clone()));
                }
                let (mut key_id, mut key_label) = (None, String::new());
                match &r.key {
                    KeyRef::None => {}
                    KeyRef::Keychain { id, .. } => key_id = Some(*id),
                    KeyRef::File { path, pem } => {
                        key_label = path.clone();
                        secrets.push((store::SLOT_KEY.to_string(), pem.clone()));
                    }
                }
                store::ImportItem { group: r.group.clone(), name: r.name.clone(), user: r.user.clone(), host: r.host.clone(), port: r.port, note: r.note.clone(), tags: r.tags.clone(), use_agent: r.agent, key_id, key_label, secrets }
            })
            .collect();
        if items.iter().any(|i| !i.secrets.is_empty()) {
            if let Err(e) = self.ensure_vault() {
                return self.import_error(e, cx);
            }
        }
        let vault = self.vault.as_ref();
        let seal = |id: crate::mock::NodeId, slot: &str, plain: &[u8]| vault.ok_or("主密钥未就绪".to_string())?.seal(plain, &crate::node_form::aad(id, slot));
        let res = self.store.as_mut().expect("上面检查过").import_nodes(&items, seal);
        match res {
            Ok(done) => {
                let n = done.len();
                for (it, got) in items.iter().zip(&done) {
                    let node = Node { id: got.id, name: it.name.clone(), user_host: store::user_host(&it.user, &it.host, it.port), note: it.note.clone(), tags: it.tags.clone(), stacks: vec![] };
                    match self.groups.iter_mut().find(|g| g.id == got.group_id) {
                        Some(g) => g.nodes.push(node),
                        None => self.groups.push(Group { id: got.group_id, name: got.group_name.clone(), color: got.color, nodes: vec![node] }),
                    }
                }
                if let Some(first) = done.first() {
                    self.sel = first.id;
                }
                self.scroll = gpui_kit::component::VirtualListScrollHandle::new();
                self.reload_keys();
                crate::logs::push(crate::logs::Level::Info, "导入", format!("已导入 {n} 个节点"));
                self.import = None;
                cx.notify();
            }
            Err(e) => {
                crate::logs::push(crate::logs::Level::Error, "导入", format!("导入失败: {e}"));
                self.import_error(format!("导入失败,没有写入任何节点:{e}"), cx);
            }
        }
    }

    fn import_error(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        if let Some(d) = &mut self.import {
            d.error = Some(msg.into());
        }
        cx.notify();
    }

    pub(crate) fn import_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let d = self.import.as_ref()?;
        if !d.loaded {
            return Some(if d.csv { self.import_picker(d.error.clone(), d.from, cx) } else { self.import_methods(d.error.clone(), d.from, cx) });
        }
        let (n_new, n_dup, n_skip, n_err) = (
            d.count(|r| r.status == RowStatus::New),
            d.count(|r| r.status == RowStatus::Dup),
            d.count(|r| matches!(r.status, RowStatus::Unsupported(_))),
            d.count(|r| matches!(r.status, RowStatus::Error(_))),
        );
        let n_sel = d.selected();

        let chip = |text: String, fg: Rgba, bg: Rgba| div().flex_shrink_0().px(px(8.0)).py(px(3.0)).rounded(px(t::RADIUS_SM)).bg(bg).text_size(px(t::TYPE_CAPTION)).text_color(fg).child(text);
        let summary = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(24.0))
            .pb(px(10.0))
            .child(chip(format!("将新增 {n_new}"), c::online(), c::online_soft()))
            .when(n_dup > 0, |s| s.child(chip(format!("重复 {n_dup}"), c::warn(), c::warn_soft())))
            .when(n_skip > 0, |s| s.child(chip(format!("不支持 {n_skip}"), c::muted(), c::idle_soft())))
            .when(n_err > 0, |s| s.child(chip(format!("错误 {n_err}"), c::danger(), c::danger().opacity(0.16))))
            .child(div().flex_1())
            .child(ctl_btn("imp-pick", Some(IconName::FileUp), "重新选择", 28.0).on_click(cx.listener(|this, _, window, cx| this.open_import(window, cx))))
            .when(d.csv, |s| s.child(ctl_btn("imp-tpl", Some(IconName::Download), "下载模板", 28.0).on_click(cx.listener(|this, _, window, cx| this.save_template(window, cx)))));
        let head = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(8.0))
            .border_b_1()
            .border_color(c::line())
            .bg(c::hover())
            .text_size(px(t::TYPE_CAPTION))
            .text_color(c::ghost())
            .children(["", "状态", "名称", "地址", "分组", "标签", "认证"].into_iter().enumerate().map(|(k, h)| imp_col(k).child(h)));
        let mut body = div().id("imp-list").flex_1().min_h_0().overflow_y_scroll().flex().flex_col();
        if d.rows.is_empty() && d.error.is_none() {
            body = body.child(div().py(px(24.0)).px(px(12.0)).text_size(px(t::TYPE_BODY)).text_color(c::muted()).child("文件里没有可导入的行"));
        }
        for (i, r) in d.rows.iter().enumerate() {
            body = body.child(self.import_row(i, r, cx));
        }
        let list = div().flex_1().min_h_0().flex().flex_col().px(px(24.0)).pb(px(14.0)).child(div().flex_1().min_h_0().flex().flex_col().rounded(px(t::RADIUS_LG)).border_1().border_color(c::line()).overflow_hidden().child(head).child(body));

        let title = div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .px(px(24.0))
            .pt(px(24.0))
            .pb(px(14.0))
            .child(div().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).child("导入节点"))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(format!("{} · 确认前不会写入任何内容", d.file)));
        let enabled = n_sel > 0;
        let submit = div()
            .id("imp-submit")
            .flex()
            .items_center()
            .h(px(34.0))
            .px(px(16.0))
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .when(enabled, |d| d.bg(c::accent()).text_color(c::on_accent()).cursor_pointer().hover(|s| s.opacity(0.88)))
            .when(!enabled, |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()))
            .child(format!("导入 {n_sel} 个节点"))
            .on_click(cx.listener(|this, _, _, cx| this.submit_import(cx)));
        let footer = div()
            .mt_auto()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(24.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(c::line())
            .when_some(d.error.clone(), |f, e| {
                f.child(div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(5.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::danger()).child(icon(IconName::CircleX, 13.0, c::danger())).child(e))
            })
            .child(div().flex_1())
            .child(ctl_btn("imp-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
                this.import = None;
                cx.notify();
            })))
            .child(submit);

        Some(modal_anim(
            "import-dialog",
            d.from,
            d.size(),
            cx.listener(|this, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    this.import = None;
                    cx.notify();
                    cx.stop_propagation();
                }
            }),
            [title.into_any_element(), summary.into_any_element(), list.into_any_element(), footer.into_any_element()],
        ))
    }

    /// 第一步:选导入方式(~/.ssh/config 或 CSV)。
    fn import_methods(&self, error: Option<SharedString>, from: (f32, f32), cx: &mut Context<Self>) -> Stateful<Div> {
        let title = div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .px(px(24.0))
            .pt(px(24.0))
            .pb(px(16.0))
            .child(div().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).child("导入节点"))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("选择导入方式,先预览,确认前不会写入任何内容"));
        let card = |id: &'static str, ic: IconName, name: &'static str, desc: &'static str| {
            div()
                .id(id)
                .glass(t::RADIUS_LG, GlassStyle::CARD)
                .glass_hover(GlassStyle::CARD)
                .flex_1()
                .min_w_0()
                .h(px(96.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(5.0))
                .cursor_pointer()
                .child(icon(ic, 22.0, c::accent()))
                .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(name))
                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(desc))
        };
        let cards = div()
            .flex()
            .flex_row()
            .gap(px(12.0))
            .px(px(24.0))
            .pb(px(20.0))
            .child(card("imp-m-ssh", IconName::FileTerminal, "~/.ssh/config", "读取本机 SSH 配置").on_click(cx.listener(|this, _, _, cx| this.load_ssh_config(cx))))
            .child(card("imp-m-csv", IconName::FileSpreadsheet, "CSV", "兼容 Termius 导出").on_click(cx.listener(|this, _, _, cx| this.import_choose_csv(cx))));
        let footer = div()
            .mt_auto()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(24.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(c::line())
            .when_some(error, |f, e| {
                f.child(div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(5.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::danger()).child(icon(IconName::CircleX, 13.0, c::danger())).child(e))
            })
            .child(div().flex_1())
            .child(ctl_btn("imp-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
                this.import = None;
                cx.notify();
            })));
        modal_anim(
            "import-methods",
            from,
            (IMPORT_NARROW_W, IMPORT_METHODS_H),
            cx.listener(|this, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    this.import = None;
                    cx.notify();
                    cx.stop_propagation();
                }
            }),
            [title.into_any_element(), cards.into_any_element(), footer.into_any_element()],
        )
    }

    /// 第二步(CSV):下载模板 + 拖入或点击选择 CSV。
    fn import_picker(&self, error: Option<SharedString>, from: (f32, f32), cx: &mut Context<Self>) -> Stateful<Div> {
        let title = div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .px(px(24.0))
            .pt(px(24.0))
            .pb(px(14.0))
            .child(div().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).child("导入节点"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .text_size(px(t::TYPE_CONTROL))
                    .child(div().flex_1().min_w_0().text_color(c::muted()).child("从 CSV 文件批量添加节点,选择文件后先预览,确认前不会写入任何内容"))
                    .child(
                        div()
                            .id("imp-tpl")
                            .flex_shrink_0()
                            .cursor_pointer()
                            .text_color(c::accent())
                            .hover(|s| s.opacity(0.8))
                            .on_click(cx.listener(|this, _, window, cx| this.save_template(window, cx)))
                            .child("下载模板"),
                    ),
            );
        let zone = div().px(px(24.0)).pb(px(20.0)).child(
            div()
                .id("imp-drop")
                .h(px(180.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .rounded(px(t::RADIUS_MD))
                .border_1()
                .border_dashed()
                .border_color(c::field_border())
                .cursor_pointer()
                .hover(|s| s.bg(c::hover()))
                .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(c::accent_soft()).border_color(c::accent()))
                .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                    if let Some(p) = paths.paths().first() {
                        this.load_import_file(p, cx);
                    }
                }))
                .on_click(cx.listener(|this, _, window, cx| this.pick_import_file(window, cx)))
                .child(icon(IconName::FileUp, 28.0, c::muted()))
                .child(div().text_size(px(t::TYPE_BODY)).text_color(c::secondary()).child("拖入 CSV 文件,或点击选择文件"))
                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("UTF-8 编码,第一行是表头")),
        );
        let footer = div()
            .mt_auto()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(24.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(c::line())
            .when_some(error, |f, e| {
                f.child(div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(5.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::danger()).child(icon(IconName::CircleX, 13.0, c::danger())).child(e))
            })
            .child(div().flex_1())
            .child(ctl_btn("imp-back", None, "返回", 34.0).on_click(cx.listener(|this, _, window, cx| this.open_import(window, cx))))
            .child(ctl_btn("imp-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
                this.import = None;
                cx.notify();
            })));
        modal_anim(
            "import-picker",
            from,
            (IMPORT_NARROW_W, IMPORT_PICKER_H),
            cx.listener(|this, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    this.import = None;
                    cx.notify();
                    cx.stop_propagation();
                }
            }),
            [title.into_any_element(), zone.into_any_element(), footer.into_any_element()],
        )
    }

    /// 预览表格里的一行:该节点将被写入的全部信息;备注和问题说明放在行下方。
    fn import_row(&self, i: usize, r: &ImportRow, cx: &mut Context<Self>) -> Stateful<Div> {
        let (label, fg, bg) = match &r.status {
            RowStatus::New => ("新增".to_string(), c::online(), c::online_soft()),
            RowStatus::Dup => ("重复".to_string(), c::warn(), c::warn_soft()),
            RowStatus::Unsupported(_) => ("不支持".to_string(), c::muted(), c::idle_soft()),
            RowStatus::Error(_) => ("错误".to_string(), c::danger(), c::danger().opacity(0.16)),
        };
        let selectable = r.selectable();
        let auth: AnyElement = {
            let mut parts: Vec<String> = vec![];
            if !r.password.is_empty() {
                parts.push("密码".into());
            }
            match &r.key {
                KeyRef::None => {}
                KeyRef::Keychain { name, .. } => parts.push(format!("钥匙串「{name}」")),
                KeyRef::File { path, .. } => parts.push(format!("私钥 {path}")),
            }
            if r.agent {
                parts.push("Agent".into());
            }
            if parts.is_empty() {
                div().text_color(c::ghost()).child("未配置").into_any_element()
            } else {
                div().child(parts.join(" · ")).into_any_element()
            }
        };
        let tags: AnyElement = if r.tags.is_empty() {
            div().text_color(c::ghost()).child("—").into_any_element()
        } else {
            div().flex().flex_row().gap(px(4.0)).children(r.tags.iter().map(|t| crate::ui::tag(t.clone()))).into_any_element()
        };
        let group: AnyElement = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(r.group.clone()))
            .when(r.new_group, |d| d.child(div().flex_shrink_0().px(px(5.0)).rounded(px(t::RADIUS_XS)).bg(c::accent_soft()).text_color(c::accent()).text_size(px(t::TYPE_MICRO)).child("新")))
            .into_any_element();
        let msg = match &r.status {
            RowStatus::Error(e) => Some(e.clone()),
            RowStatus::Unsupported(p) => Some(format!("暂不支持 {p},这一行会被跳过")),
            RowStatus::Dup => Some("已有相同的主机、端口和用户(不分分组)".to_string()),
            RowStatus::New => None,
        };
        let bad = matches!(r.status, RowStatus::Error(_));

        div()
            .id(("imp-row", i))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .px(px(12.0))
            .py(px(9.0))
            .border_b_1()
            .border_color(c::line())
            .text_size(px(t::TYPE_CAPTION))
            .text_color(c::secondary())
            .when(selectable, |d| d.cursor_pointer().hover(|s| s.bg(c::hover())))
            .when(!selectable, |d| d.opacity(0.7))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_import_row(i, cx)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(imp_col(0).child(match (selectable, r.selected) {
                        (false, _) => icon(IconName::Square, 16.0, c::ghost()),
                        (true, true) => icon(IconName::SquareCheck, 16.0, c::accent()),
                        (true, false) => icon(IconName::Square, 16.0, c::muted()),
                    }))
                    .child(imp_col(1).flex().flex_row().child(div().flex_shrink_0().px(px(6.0)).py(px(2.0)).rounded(px(t::RADIUS_XS)).bg(bg).text_color(fg).text_size(px(t::TYPE_MICRO)).child(label)))
                    .child(imp_col(2).text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(r.name.clone()))
                    .child(imp_col(3).font_family(MONO).child(store::user_host(&r.user, &r.host, r.port)))
                    .child(imp_col(4).child(group))
                    .child(imp_col(5).child(tags))
                    .child(imp_col(6).child(auth)),
            )
            .when(!r.note.is_empty(), |d| d.child(div().pl(px(76.0)).text_color(c::muted()).child(format!("备注:{}", r.note))))
            .when_some(msg, |d, m| d.child(div().pl(px(76.0)).text_color(if bad { c::danger() } else { c::warn() }).child(m)))
    }
}

/// 预览表格的列(表头和行共用,保证对齐):0 勾选 1 状态 2 名称 3 地址 4 分组 5 标签 6 认证
fn imp_col(k: usize) -> Div {
    let d = div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis();
    match k {
        0 => d.w(px(16.0)).flex_shrink_0(),
        1 => d.w(px(44.0)).flex_shrink_0(),
        2 | 6 => d.flex_1(),
        3 => d.w(px(190.0)).flex_shrink_0(),
        4 => d.w(px(120.0)).flex_shrink_0(),
        5 => d.w(px(110.0)).flex_shrink_0(),
        _ => d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn ex() -> Existing {
        Existing { groups: vec!["未分组".into(), "Project/Dev".into()], nodes: HashSet::new(), keys: vec![(7, "生产密钥".into())] }
    }

    const TERMIUS: &str = "Groups,Label,Tags,Hostname/IP,Protocol,Port,Username,Password\n\
Project/Dev,Web Server,dev,192.168.1.1,ssh,2300,admin,password123\n\
Project/Prod,Web Server,prod,serverauditor.com,ssh,,root,\n\
,Database,\"db,mysql\",db.serverauditor.com,ssh,4567,dbuser,\n";

    #[test]
    fn termius_template_parses() {
        let rows = parse_csv(TERMIUS, &ex()).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.status == RowStatus::New && r.selected));
        let r = &rows[0];
        assert_eq!((r.group.as_str(), r.name.as_str(), r.host.as_str(), r.port, r.user.as_str(), r.password.as_str()), ("Project/Dev", "Web Server", "192.168.1.1", 2300, "admin", "password123"));
        assert!(!r.new_group);
        assert!(rows[1].new_group, "Project/Prod 库里没有");
        assert_eq!(rows[1].port, 22, "端口为空用 22");
        assert_eq!(rows[2].group, DEFAULT_GROUP);
        assert_eq!(rows[2].tags, ["db", "mysql"]);
        assert_eq!(rows[2].line, 4);
    }

    #[test]
    fn bundled_template_parses_cleanly() {
        let mut e = ex();
        e.keys.clear();
        let rows = parse_csv(TEMPLATE, &e).unwrap();
        assert_eq!(rows.len(), 2);
        // 第二行的示例私钥路径在测试机上不一定存在:不存在就是错误行,存在也只能是新增
        assert_eq!(rows[0].status, RowStatus::New);
        assert_eq!(rows[0].note, "开发环境");
    }

    #[test]
    fn bom_crlf_quoted_newline_and_column_order() {
        let text = "\u{feff}hostname/ip,NAME,note\r\n1.1.1.1,\"a\",\"第一行\n第二行\"\r\n2.2.2.2,,\r\n";
        let rows = parse_csv(text, &ex()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].note, "第一行\n第二行");
        assert_eq!(rows[1].name, "2.2.2.2", "名称为空用主机");
        assert_eq!(rows[1].user, "root");
    }

    #[test]
    fn missing_host_column_is_an_error() {
        assert!(parse_csv("Label,Port\na,22\n", &ex()).is_err());
    }

    #[test]
    fn bad_rows_are_flagged_not_fatal() {
        let text = "Label,Hostname/IP,Protocol,Port\nok,1.1.1.1,,\nno-host,,ssh,\nbad-port,2.2.2.2,ssh,99999\ntel,3.3.3.3,telnet,\n,,,\n";
        let rows = parse_csv(text, &ex()).unwrap();
        assert_eq!(rows.len(), 4, "全空行直接忽略");
        assert_eq!(rows[0].status, RowStatus::New);
        assert!(matches!(rows[1].status, RowStatus::Error(_)));
        assert!(matches!(rows[2].status, RowStatus::Error(_)));
        assert!(matches!(rows[3].status, RowStatus::Unsupported(_)));
        assert!(rows[1..].iter().all(|r| !r.selected && !r.selectable() || r.status == RowStatus::Dup));
    }

    #[test]
    fn duplicates_against_library_and_within_file() {
        let mut e = ex();
        e.nodes.insert(("1.1.1.1".into(), 22, "root".into()));
        let text = "Hostname/IP\n1.1.1.1\n2.2.2.2\n2.2.2.2\n";
        let rows = parse_csv(text, &e).unwrap();
        assert_eq!(rows.iter().map(|r| r.status.clone()).collect::<Vec<_>>(), [RowStatus::Dup, RowStatus::New, RowStatus::Dup]);
        assert!(rows[0].selectable() && !rows[0].selected, "重复行可勾选但默认不勾");
        // 已有节点在别的分组也算重复
        let text = "Groups,Hostname/IP\n其他分组,1.1.1.1\n";
        assert_eq!(parse_csv(text, &e).unwrap()[0].status, RowStatus::Dup);
    }

    #[test]
    fn key_column_resolves_keychain_then_file() {
        let (pem, _) = crate::keychain::generate_ed25519("t").unwrap();
        let path = std::env::temp_dir().join(format!("otter-import-key-{}", std::process::id()));
        std::fs::write(&path, pem.as_bytes()).unwrap();
        let text = format!("Hostname/IP,Key,Agent\n1.1.1.1,生产密钥,yes\n2.2.2.2,{},\n3.3.3.3,/no/such/key,\n", path.display());
        let rows = parse_csv(&text, &ex()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(matches!(rows[0].key, KeyRef::Keychain { id: 7, .. }));
        assert!(rows[0].agent);
        assert!(matches!(rows[1].key, KeyRef::File { .. }));
        assert!(matches!(rows[2].status, RowStatus::Error(_)));
    }

    #[test]
    fn ssh_config_defaults_aliases_and_skips() {
        let dir = std::env::temp_dir().join("otter-ssh-test-none");
        let cfg = "Host web db\n  HostName 10.0.0.5\n  User deploy\nHost api\n  HostName api.example.com\n  Port 2222\nHost jump\n  ProxyJump bastion\nHost bad\n  Port x\nHost *\n  User fallback\n  Port 2200\n";
        let rows = parse_ssh_config(cfg, &dir, &ex());
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["web", "db", "api", "jump", "bad"]);
        assert_eq!((rows[0].host.as_str(), rows[0].user.as_str(), rows[0].port), ("10.0.0.5", "deploy", 2200));
        assert_eq!((rows[2].user.as_str(), rows[2].port), ("fallback", 2222));
        assert!(matches!(rows[3].status, RowStatus::Unsupported(_)));
        assert!(matches!(rows[4].status, RowStatus::Error(_)));
        // web 与 db 主机/端口/用户相同 → 后者重复
        assert_eq!(rows[1].status, RowStatus::Dup);
        assert!(rows.iter().all(|r| r.group == SSH_GROUP && r.new_group));
    }

    #[test]
    fn ssh_config_negation_match_and_eq_syntax() {
        let dir = std::env::temp_dir().join("otter-ssh-test-none");
        let cfg = "Host a b\n  HostName=%h.lan\nMatch host a\n  Port 1\nHost * !b\n  User star\n";
        let rows = parse_ssh_config(cfg, &dir, &ex());
        assert_eq!(rows[0].host, "a.lan");
        assert_eq!((rows[0].user.as_str(), rows[0].port), ("star", 22));
        assert_eq!(rows[1].user, "root");
    }
}
