//! 文件管理抽屉:从右侧滑出,盖住内容区。
//!
//! 目标有两种:节点本机(走 SFTP)和某个容器(走 docker exec / docker cp)。
//! 抽屉宽度按目标固定:服务器 = 宽(常用位置 + 列表 + 预览),容器 = 窄(列表 + 预览),
//! 切换目标时宽度跟着变;窗口太窄时按可用宽度收起侧栏。
//! 抽屉不是模态的:开着的时候仍可点节点树换节点、点别的容器卡片上的"文件"换目标。
//! 把本地文件 / 文件夹拖进抽屉即上传到当前目录(mock 进度)。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ::gpui::{Animation, AnimationExt as _, bounce, ease_in_out, ease_out_quint};
use gpui_kit::assets::IconName;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

mod left;
use left::LeftSrv;
use super::{Home, MENU_W, MONO, TITLEBAR_H, c, icon, t};
use crate::mock::{Node, NodeId};
use crate::sftp::{Direction, RemoteEntry, Sftp, Status as SftpStatus, TransferState};
use crate::ui::{DROPDOWN_COMPACT_W, MenuItem, MenuSide, dropdown_menu_compact};

/// 服务器文件:三栏,通常正好盖满内容区
const HOST_W: f32 = 1100.0;
/// 容器文件:两栏,容器里通常只关心挂载卷和少数配置
const CT_W: f32 = 760.0;
const MIN_W: f32 = 480.0;

/// 筛选框展开/收起动画时长
const SEARCH_ANIM_MS: u64 = 180;
/// 文件列表骨架占位的呼吸周期
const SKELETON_MS: u64 = 1100;

const K: u64 = 1024;
const M: u64 = 1024 * K;
const G: u64 = 1024 * M;

// ---- 数据 --------------------------------------------------------------------
#[derive(Clone, PartialEq)]
pub enum Target {
    Host,
    Container { stack: String, svc: String },
}

impl Target {
    fn key(&self) -> String {
        match self {
            Target::Host => "host".into(),
            Target::Container { svc, .. } => format!("ct:{svc}"),
        }
    }
    /// 容器的挂载点:(容器内路径, 宿主机路径)
    fn mounts(&self) -> Vec<(String, String)> {
        match self {
            Target::Host => vec![],
            Target::Container { stack, svc } => vec![
                ("/config".into(), format!("/opt/stacks/{stack}/{svc}/config")),
                ("/data".into(), format!("/data/{stack}")),
            ],
        }
    }
    fn mount_of(&self, path: &str) -> Option<(String, String)> {
        self.mounts().into_iter().find(|(m, _)| path == m || path.starts_with(&format!("{m}/")))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Dir,
    File,
    Link,
}

#[derive(Clone)]
struct Entry {
    name: String,
    kind: Kind,
    size: u64,
    modified: String,
    mode: String,
    owner: String,
    /// Some = 上传中(0..1)
    progress: Option<f32>,
    /// 容器挂载卷对应的宿主机路径
    mount: Option<String>,
    link: Option<String>,
    /// 上传的本地源文件,预览时直接读它
    local: Option<PathBuf>,
}

impl Entry {
    fn is_dir(&self) -> bool {
        self.kind != Kind::File
    }
}

fn hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn stamp(seed: &str) -> String {
    let h = hash(seed);
    let month = 9 + h % 3;
    let day = 1 + (h >> 8) % if month == 11 { 11 } else { 28 };
    format!("2026-{month:02}-{day:02} {:02}:{:02}", (h >> 16) % 24, (h >> 24) % 60)
}

fn fmt_size(n: u64) -> String {
    match n {
        n if n >= G => format!("{:.1} GB", n as f64 / G as f64),
        n if n >= M => format!("{:.1} MB", n as f64 / M as f64),
        n if n >= K => format!("{:.1} KB", n as f64 / K as f64),
        n => format!("{n} B"),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir == "/" { format!("/{name}") } else { format!("{dir}/{name}") }
}

/// 本机路径用系统自己的规则(Windows 是反斜杠和盘符);远程路径永远是 `/`,用上面的 `join` / `parent`。
fn ljoin(dir: &str, name: &str) -> String {
    Path::new(dir).join(name).to_string_lossy().into_owned()
}

fn lparent(path: &str) -> String {
    Path::new(path).parent().map(|p| p.to_string_lossy().into_owned()).filter(|p| !p.is_empty()).unwrap_or_else(|| path.to_string())
}

/// 本机路径的面包屑:(显示名, 点进去的完整路径)。
fn local_crumbs(path: &str) -> Vec<(String, String)> {
    use std::path::Component;
    let mut acc = PathBuf::new();
    let mut out = Vec::new();
    for comp in Path::new(path).components() {
        acc.push(comp.as_os_str());
        match comp {
            Component::RootDir => {}
            Component::Prefix(_) => out.push((comp.as_os_str().to_string_lossy().into_owned(), format!("{}{}", acc.display(), std::path::MAIN_SEPARATOR))),
            _ => out.push((comp.as_os_str().to_string_lossy().into_owned(), acc.to_string_lossy().into_owned())),
        }
    }
    out
}

/// 本机地址栏输入的解析:先展开 `~`,相对路径接在当前目录后面,再抹掉 `.` 和 `..`。
fn local_resolve(input: &str, base: &str, home: &str) -> String {
    use std::path::Component;
    let input = input.trim();
    let full = if input == "~" {
        PathBuf::from(home)
    } else if let Some(r) = input.strip_prefix("~/").or_else(|| input.strip_prefix("~\\")) {
        Path::new(home).join(r)
    } else {
        Path::new(base).join(input)
    };
    let mut out = PathBuf::new();
    for comp in full.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.parent().is_some() {
                    out.pop();
                }
            }
            c => out.push(c.as_os_str()),
        }
    }
    out.to_string_lossy().into_owned()
}

fn parent(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].into(),
    }
}

/// 把用户输入的路径解析成绝对路径:`~` 展开成 `home`,相对路径接在 `base` 后面,处理 `.` 和 `..`。
fn resolve_path(input: &str, base: &str, home: &str) -> String {
    let input = input.trim();
    let full = if input == "~" {
        home.to_string()
    } else if let Some(r) = input.strip_prefix("~/") {
        join(home, r)
    } else if input.starts_with('/') {
        input.to_string()
    } else {
        join(base, input)
    };
    let mut out: Vec<&str> = Vec::new();
    for seg in full.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

fn ext(name: &str) -> &str {
    name.rsplit_once('.').map(|(_, e)| e).unwrap_or("")
}

fn is_secret(name: &str) -> bool {
    name.starts_with("id_") && !name.ends_with(".pub")
}

fn is_text(name: &str) -> bool {
    const EXT: [&str; 18] =
        ["yml", "yaml", "json", "xml", "toml", "conf", "ini", "sh", "py", "js", "rs", "log", "txt", "md", "env", "pub", "types", "csv"];
    const NAMES: [&str; 12] = [
        "hosts", "fstab", "hostname", "crontab", "config", "syslog", "passwd", "os-release", "authorized_keys", "known_hosts",
        "VERSION", "Dockerfile",
    ];
    EXT.contains(&ext(name)) || NAMES.contains(&name) || name == ".bashrc" || name == ".profile"
}

fn is_image(name: &str) -> bool {
    ["jpg", "jpeg", "png", "gif", "webp", "svg"].contains(&ext(name))
}

fn is_archive(name: &str) -> bool {
    ["gz", "tar", "zip", "tgz", "xz", "7z"].contains(&ext(name))
}

/// mock 目录项构造器
struct B<'a> {
    path: &'a str,
    owner: &'a str,
}

impl B<'_> {
    fn e(&self, name: &str, kind: Kind, size: u64, mode: &'static str) -> Entry {
        Entry {
            name: name.into(),
            kind,
            size,
            modified: stamp(&join(self.path, name)),
            mode: mode.to_string(),
            owner: self.owner.into(),
            progress: None,
            mount: None,
            link: None,
            local: None,
        }
    }
    fn d(&self, name: &str) -> Entry {
        self.e(name, Kind::Dir, 4096, "drwxr-xr-x")
    }
    fn f(&self, name: &str, size: u64) -> Entry {
        let mode = if name.ends_with(".sh") {
            "-rwxr-xr-x"
        } else if is_secret(name) || name == "authorized_keys" || name == ".env" {
            "-rw-------"
        } else {
            "-rw-r--r--"
        };
        self.e(name, Kind::File, size, mode)
    }
    fn l(&self, name: &str, to: &str) -> Entry {
        let mut e = self.e(name, Kind::Link, 7, "lrwxrwxrwx");
        e.link = Some(to.into());
        e
    }
    fn dirs(&self, names: &[&str]) -> Vec<Entry> {
        names.iter().map(|n| self.d(n)).collect()
    }
}

fn generic(b: &B, p: &str) -> Vec<Entry> {
    let h = hash(p);
    let mut v = vec![b.d("config"), b.d("cache"), b.f("settings.json", 300 + h % 4000), b.f("app.log", 10_000 + h % 3_000_000)];
    if h % 3 == 0 {
        v.push(b.f("README.md", 1200 + h % 900));
    }
    v
}

/// 确定性的 mock 目录内容
fn base(target: &Target, path: &str, node: &Node) -> Vec<Entry> {
    let user = node.user_host.split('@').next().unwrap_or("root").to_string();
    match target {
        Target::Host => {
            let b = B { path, owner: "root" };
            match path {
                "/" => {
                    let mut v = b.dirs(&["boot", "data", "dev", "etc", "home", "opt", "proc", "root", "srv", "tmp", "usr", "var"]);
                    v.push(b.l("bin", "usr/bin"));
                    v.push(b.l("lib", "usr/lib"));
                    v
                }
                "/root" => vec![
                    b.d(".ssh"),
                    b.d(".docker"),
                    b.f(".bashrc", 3 * K + 120),
                    b.f(".profile", 807),
                    b.f("backup.sh", K + 230),
                    b.f("notes.md", 2 * K + 410),
                ],
                "/root/.ssh" => vec![
                    b.f("authorized_keys", 742),
                    b.f("known_hosts", 4 * K + 88),
                    b.f("id_ed25519", 411),
                    b.f("id_ed25519.pub", 98),
                    b.f("config", 356),
                ],
                "/etc" => {
                    let mut v = b.dirs(&["docker", "nginx", "ssh", "systemd", "cron.d"]);
                    v.extend([
                        b.f("fstab", 612),
                        b.f("hostname", 12),
                        b.f("hosts", 221),
                        b.f("os-release", 386),
                        b.f("resolv.conf", 104),
                        b.f("crontab", K + 42),
                    ]);
                    v
                }
                "/etc/nginx" => vec![b.d("conf.d"), b.d("sites-enabled"), b.f("nginx.conf", 2 * K + 412), b.f("mime.types", 5 * K + 231)],
                "/etc/docker" => vec![b.f("daemon.json", 214)],
                "/var" => b.dirs(&["cache", "lib", "log", "tmp"]),
                "/var/log" => vec![
                    b.d("nginx"),
                    b.f("auth.log", 182 * K),
                    b.f("dpkg.log", 41 * K),
                    b.f("kern.log", 96 * K),
                    b.f("syslog", 3 * M + 412 * K),
                    b.f("docker.log", 12 * M + 88 * K),
                ],
                "/var/lib" => b.dirs(&["apt", "docker", "systemd"]),
                "/opt" => b.dirs(&["containerd", "stacks"]),
                "/opt/stacks" => node.stacks.iter().map(|s| b.d(&s.name)).collect(),
                "/data" => b.dirs(&["appdata", "backups", "media"]),
                "/data/media" => b.dirs(&["movies", "music", "tv"]),
                "/data/backups" => vec![
                    b.f("mysql-2026-11-11.sql.gz", 842 * M),
                    b.f("mysql-2026-11-10.sql.gz", 839 * M),
                    b.f("etc-2026-11-01.tar.gz", 12 * M + 300 * K),
                    b.f("stacks-2026-11-01.tar.gz", G + 210 * M),
                ],
                "/home" => vec![B { path, owner: &user }.d(&user)],
                "/tmp" => vec![b.f("install.sh", 18 * K), b.f("core.dump", 210 * M)],
                p => {
                    if let Some(st) = p.strip_prefix("/opt/stacks/").and_then(|n| node.stacks.iter().find(|s| s.name == n)) {
                        let mut v: Vec<Entry> = st.services.iter().map(|s| b.d(&s.name)).collect();
                        v.extend([b.f("compose.yaml", K + 96 * st.services.len() as u64), b.f(".env", 214)]);
                        return v;
                    }
                    generic(&b, p)
                }
            }
        }
        Target::Container { svc, .. } => {
            let b = B { path, owner: "root" };
            let o = B { path, owner: "abc" };
            match path {
                "/" => {
                    let mut v = b.dirs(&["app", "etc", "proc", "tmp", "usr", "var"]);
                    v.push(b.l("bin", "usr/bin"));
                    for (m, host) in target.mounts() {
                        let mut e = o.d(&m[1..]);
                        e.mount = Some(host);
                        v.push(e);
                    }
                    v
                }
                "/app" => vec![b.d("bin"), b.d("lib"), b.f("entrypoint.sh", 2 * K + 12), b.f("VERSION", 9)],
                "/etc" => vec![b.f("hosts", 174), b.f("hostname", 13), b.f("resolv.conf", 92), b.f("os-release", 402), b.f("passwd", K + 20)],
                "/config" => vec![
                    o.d("cache"),
                    o.d("logs"),
                    o.d("plugins"),
                    o.f("config.xml", 3 * K + 640),
                    o.f("system.xml", K + 92),
                    o.f("settings.json", 860),
                ],
                "/config/logs" => vec![
                    o.f(&format!("{svc}-2026-11-11.log"), 486 * K),
                    o.f(&format!("{svc}-2026-11-10.log"), 2 * M + 30 * K),
                    o.f(&format!("{svc}-2026-11-09.log"), M + 870 * K),
                ],
                "/data" => vec![o.d("db"), o.d("uploads"), o.f("cover.jpg", 238 * K)],
                p => generic(&o, p),
            }
        }
    }
}

/// 拖进来的本地文件夹:只算一层的大小,够模拟进度用
fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| rd.flatten().take(2000).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
        .unwrap_or(0)
}

// ---- 状态 --------------------------------------------------------------------
/// 各文件视图(抽屉、文件标签)共享的 mock 状态:同一节点同一目录在哪里看到的都一样。
#[derive(Default)]
pub struct Store {
    /// 新增的条目(上传 / 新建),key = 节点|目标|目录
    extra: HashMap<String, Vec<Entry>>,
    /// 删掉的条目,key = 节点|目标|目录/名字
    deleted: HashSet<String>,
    ticking: bool,
    batch: usize,
    /// 真实 SFTP 的界面刷新循环是否在跑
    polling: bool,
}

/// 一个文件视图的状态(浏览位置、选中、筛选)。每个标签各有一份,抽屉用所在标签的那份。
pub struct Files {
    pub open: bool,
    /// 打开时所属的节点,换节点后重置到本机主目录
    node: NodeId,
    target: Target,
    path: String,
    back: Vec<String>,
    fwd: Vec<String>,
    selected: Option<String>,
    /// 「本地」开关:打开后左边多一栏本机文件
    local: bool,
    /// 左栏的来源:本地,或另一台服务器
    left: LeftSrv,
    lpath: String,
    lsel: Option<String>,
    lback: Vec<String>,
    lfwd: Vec<String>,
    lsearch: Option<Entity<InputState>>,
    /// 地址栏编辑:输入框(本地栏 / 远程栏)、正在编辑哪一栏(0 无,1 本地,2 远程)、本地路径无效
    laddr: Option<Entity<InputState>>,
    addr: Option<Entity<InputState>>,
    addr_edit: u8,
    addr_err: bool,
    /// 筛选框是否展开(平时只显示一个图标)
    lsopen: bool,
    sopen: bool,
    /// 正在播收起动画(播完才真正收起)
    lsclosing: bool,
    sclosing: bool,
    /// 展开的「更多」菜单:0 无,1 本地栏,2 远程栏
    more: u8,
    search: Option<Entity<InputState>>,
    notice: Option<SharedString>,
    /// 真实 SFTP 连接(非 demo 的服务器目标);丢弃即断开
    sftp: Option<Sftp>,
    /// 连上后是否已把路径定位到远程主目录
    path_ready: bool,
    /// 等待确认删除的项(真实服务器上的删除不可恢复)
    confirm_del: Option<String>,
    /// 右键菜单:哪一栏、哪一项(None 是空白处)、弹出位置
    ctx: Option<CtxMenu>,
    /// 表格排序:远程栏、本地栏(本地栏的会记忆)
    sort: Sort,
    lsort: Sort,
}

/// 表格排序列
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SortKey {
    Name,
    Size,
    Time,
    Mode,
    Owner,
}

impl SortKey {
    fn tag(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Time => "time",
            SortKey::Mode => "mode",
            SortKey::Owner => "owner",
        }
    }
    fn from_tag(t: &str) -> Option<Self> {
        Some(match t {
            "name" => SortKey::Name,
            "size" => SortKey::Size,
            "time" => SortKey::Time,
            "mode" => SortKey::Mode,
            "owner" => SortKey::Owner,
            _ => return None,
        })
    }
    /// 第一次点这一列时的方向:大小、时间从大/新到小,其余从小到大
    fn first_asc(self) -> bool {
        !matches!(self, SortKey::Size | SortKey::Time)
    }
}

type Sort = (SortKey, bool);

/// 本地栏的记忆:上次所在目录和排序方式。启动时从本地库读进来,之后每次变化写回库。
static PREFS: std::sync::Mutex<(Option<String>, Option<Sort>)> = std::sync::Mutex::new((None, None));

pub fn restore_prefs(st: &crate::store::Store) {
    let dir = st.get_setting("local_dir").filter(|d| Path::new(d).is_dir());
    let sort = st.get_setting("local_sort").and_then(|v| {
        let (k, d) = v.split_once(':')?;
        Some((SortKey::from_tag(k)?, d == "asc"))
    });
    if let Ok(mut p) = PREFS.lock() {
        *p = (dir, sort);
    }
}

fn sort_dirs_first<T>(v: &mut [T], is_dir: impl Fn(&T) -> bool, name: impl Fn(&T) -> String, cmp: impl Fn(&T, &T) -> std::cmp::Ordering, asc: bool) {
    v.sort_by(|a, b| {
        is_dir(b).cmp(&is_dir(a)).then_with(|| {
            let o = cmp(a, b);
            (if asc { o } else { o.reverse() }).then_with(|| name(a).to_lowercase().cmp(&name(b).to_lowercase()))
        })
    });
}

fn sort_entries(v: &mut [Entry], (key, asc): Sort) {
    sort_dirs_first(
        v,
        |e| e.is_dir(),
        |e| e.name.clone(),
        |a, b| match key {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Time => a.modified.cmp(&b.modified),
            SortKey::Mode => a.mode.cmp(&b.mode),
            SortKey::Owner => a.owner.cmp(&b.owner),
        },
        asc,
    );
}

type LocalRow = (String, bool, u64, String);

fn sort_local(v: &mut [LocalRow], (key, asc): Sort) {
    sort_dirs_first(
        v,
        |e| e.1,
        |e| e.0.clone(),
        |a, b| match key {
            SortKey::Size => a.2.cmp(&b.2),
            SortKey::Time => a.3.cmp(&b.3),
            _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
        },
        asc,
    );
}

/// 拖拽载荷:本地栏的一项(拖到远程栏上传)/ 远程栏的一项(拖到本地栏下载)
#[derive(Clone)]
struct DragLocal {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

#[derive(Clone)]
struct DragRemote {
    name: String,
    is_dir: bool,
    /// 来自左栏(服务器来源)而不是右栏
    left: bool,
}

#[derive(Clone)]
struct CtxMenu {
    which: u8,
    name: Option<String>,
    pos: Point<Pixels>,
}

impl Files {
    pub fn new() -> Self {
        Self {
            open: false,
            node: NodeId(0),
            target: Target::Host,
            path: "/root".into(),
            back: vec![],
            fwd: vec![],
            selected: None,
            local: false,
            left: LeftSrv::default(),
            lpath: PREFS.lock().ok().and_then(|p| p.0.clone()).unwrap_or_else(crate::platform::home_string),
            lsel: None,
            lback: vec![],
            lfwd: vec![],
            lsearch: None,
            laddr: None,
            addr: None,
            addr_edit: 0,
            addr_err: false,
            lsopen: false,
            sopen: false,
            lsclosing: false,
            sclosing: false,
            more: 0,
            search: None,
            notice: None,
            sftp: None,
            path_ready: false,
            confirm_del: None,
            ctx: None,
            sort: (SortKey::Name, true),
            lsort: PREFS.lock().ok().and_then(|p| p.1).unwrap_or((SortKey::Name, true)),
        }
    }

    pub fn node(&self) -> NodeId {
        self.node
    }

    pub fn sftp_ref(&self) -> Option<&Sftp> {
        self.sftp.as_ref()
    }

    /// 丢弃连接(断开),下次渲染会重新连接
    pub fn drop_sftp(&mut self) {
        self.sftp = None;
        self.path_ready = false;
    }

    /// 创建筛选输入框(需要 window 和 cx,所以不放在 `new` 里)
    pub fn init_search(&mut self, window: &mut Window, cx: &mut Context<Home>) {
        if std::env::var_os("OTTER_FILES_SRC").is_some() && self.left.search.is_none() {
            self.left.menu = true;
            self.left.search = Some(cx.new(|cx| InputState::new(window, cx).placeholder("搜索服务器")));
        }
        if self.search.is_none() {
            let s = cx.new(|cx| InputState::new(window, cx).placeholder("筛选当前目录"));
            cx.observe(&s, |_, _, cx| cx.notify()).detach();
            self.search = Some(s);
        }
        if self.lsearch.is_none() {
            let s = cx.new(|cx| InputState::new(window, cx).placeholder("筛选本地目录"));
            cx.observe(&s, |_, _, cx| cx.notify()).detach();
            self.lsearch = Some(s);
        }
        for which in [1u8, 2] {
            let slot = if which == 1 { &mut self.laddr } else { &mut self.addr };
            if slot.is_none() {
                let s = cx.new(|cx| InputState::new(window, cx).placeholder("输入路径,回车跳转"));
                // 回车跳转;失焦放弃编辑;输入时清掉错误标记
                cx.subscribe_in(&s, window, move |this: &mut Home, _, ev: &InputEvent, _window, cx| match ev {
                    InputEvent::PressEnter { .. } => this.commit_addr(which, cx),
                    InputEvent::Blur => {
                        if this.files.addr_edit == which {
                            this.files.addr_edit = 0;
                            this.files.addr_err = false;
                            cx.notify();
                        }
                    }
                    InputEvent::Change => {
                        if this.files.addr_err {
                            this.files.addr_err = false;
                            cx.notify();
                        }
                    }
                    _ => {}
                })
                .detach();
                *slot = Some(s);
            }
        }
    }

    /// 截图调试用:直接跳到某目录 / 选中某项
    pub fn debug_jump(&mut self, cd: Option<String>, sel: Option<String>) {
        if let Some(p) = cd {
            self.path = p;
        }
        self.selected = sel;
        // 调试:OTTER_FILES_LOCAL=0 截图时关闭双栏(默认打开);OTTER_FILES_SEARCH=1 展开两个筛选框
        if std::env::var("OTTER_FILES_LOCAL").as_deref() == Ok("0") {
            self.local = false;
        }
        if std::env::var_os("OTTER_FILES_SEARCH").is_some() {
            self.sopen = true;
            self.lsopen = true;
        }
        // OTTER_FILES_ADDR=1|2 截图时让本地/远程栏进入地址栏编辑,OTTER_FILES_ADDR_ERR=1 同时标红
        self.addr_edit = std::env::var("OTTER_FILES_ADDR").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        self.addr_err = std::env::var_os("OTTER_FILES_ADDR_ERR").is_some();
        // OTTER_FILES_CTX=2:名字 / 2: / 1:名字 截图时弹出右键菜单(位置固定)
        if let Ok(v) = std::env::var("OTTER_FILES_CTX")
            && let Some((w, n)) = v.split_once(':')
        {
            self.ctx = Some(CtxMenu { which: w.parse().unwrap_or(2), name: Some(n.to_string()).filter(|n| !n.is_empty()), pos: point(px(1500.0), px(330.0)) });
        }
        // OTTER_FILES_SRC=1 截图时展开左栏的来源下拉
        self.left.menu = std::env::var_os("OTTER_FILES_SRC").is_some();
        // OTTER_FILES_MORE=1|2 截图时展开本地/远程栏的「更多」菜单
        self.more = std::env::var("OTTER_FILES_MORE").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    }

    /// 截图调试用:切换目标并回到该目标的主目录
    pub fn retarget(&mut self, target: Target) {
        self.path = Self::home_of(&target).into();
        self.target = target;
    }

    fn dir_key(&self) -> String {
        format!("{}|{}|{}", self.node.0, self.target.key(), self.path)
    }

    /// 全尺寸文件标签用的初始状态
    pub fn for_tab(node: NodeId, target: Target) -> Self {
        let mut f = Self::new();
        f.path = Self::home_of(&target).into();
        f.node = node;
        f.target = target;
        f.open = true;
        // SFTP 模式默认双栏(左本地右远程)
        f.local = true;
        f
    }

    fn home_of(target: &Target) -> &'static str {
        match target {
            Target::Host => "/root",
            Target::Container { .. } => "/config",
        }
    }
}


// ---- 交互 --------------------------------------------------------------------
impl Home {
    pub fn open_files(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        self.files.init_search(window, cx);
        let sel = self.sel;
        let fs = &mut self.files;
        if !fs.open || fs.target != target || fs.node != sel {
            fs.path = Files::home_of(&target).into();
            fs.back.clear();
            fs.fwd.clear();
            fs.selected = None;
            fs.sftp = None;
            fs.path_ready = false;
            fs.confirm_del = None;
        }
        fs.target = target;
        fs.node = sel;
        fs.open = true;
        cx.notify();
    }

    fn close_files(&mut self, cx: &mut Context<Self>) {
        self.files.open = false;
        cx.notify();
    }

    fn go(&mut self, path: String, cx: &mut Context<Self>) {
        let fs = &mut self.files;
        if path != fs.path {
            fs.back.push(std::mem::replace(&mut fs.path, path));
            fs.fwd.clear();
            fs.selected = None;
        }
        cx.notify();
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        let fs = &mut self.files;
        if let Some(p) = fs.back.pop() {
            fs.fwd.push(std::mem::replace(&mut fs.path, p));
            fs.selected = None;
            cx.notify();
        }
    }

    fn go_fwd(&mut self, cx: &mut Context<Self>) {
        let fs = &mut self.files;
        if let Some(p) = fs.fwd.pop() {
            fs.back.push(std::mem::replace(&mut fs.path, p));
            fs.selected = None;
            cx.notify();
        }
    }

    /// 抽屉最多盖住内容区,不压节点树和节点菜单
    fn max_w(window: &Window) -> f32 {
        f32::from(window.viewport_size().width) - MENU_W
    }

    fn listing(&self) -> Vec<Entry> {
        let fs = &self.files;
        if self.sftp_real() {
            let mut v: Vec<Entry> = fs.sftp.as_ref().and_then(|s| s.dir(&fs.path)).map(|d| d.entries.iter().map(entry_from_remote).collect()).unwrap_or_default();
            sort_entries(&mut v, fs.sort);
            return v;
        }
        let key = fs.dir_key();
        let mut v = base(&fs.target, &fs.path, self.node());
        if let Some(extra) = self.fstore.extra.get(&key) {
            for e in extra {
                v.retain(|o| o.name != e.name);
                v.push(e.clone());
            }
        }
        v.retain(|e| !self.fstore.deleted.contains(&format!("{key}/{}", e.name)));
        sort_entries(&mut v, fs.sort);
        v
    }

    /// 是否走真实 SFTP:非 demo(有本地库)且目标是服务器;demo 和容器仍是 mock。
    fn sftp_real(&self) -> bool {
        self.store.is_some() && self.files.target == Target::Host
    }

    /// 服务器目标的「主目录」:真实连接用远程 home,否则 mock 的 /root
    fn files_home(&self) -> String {
        if self.sftp_real() {
            if let Some(h) = self.files.sftp.as_ref().map(|s| s.home()).filter(|h| !h.is_empty()) {
                return h;
            }
            return "/".into();
        }
        Files::home_of(&self.files.target).to_string()
    }

    /// 每次渲染前调用:按需建立连接、首次连上后定位到主目录、读取当前目录、收取提示,
    /// 有进行中的事(连接/加载/传输)就起一个定时刷新。
    fn sync_sftp(&mut self, cx: &mut Context<Self>) {
        if !self.sftp_real() {
            return;
        }
        // 这个标签的终端已经连上就复用它的连接(开个 SFTP 通道即可);终端还没连上,等它(连接页显示终端的进度);
        // 没有终端才自己连。终端重连后连接代数变了,旧 SFTP 随旧连接作废,在新连接上重建(路径保留)。
        let term = self
            .workspace
            .active
            .and_then(|a| self.workspace.tabs.get(a))
            .and_then(|t| t.term.as_ref())
            .map(|t| (t.handle.ever_connected(), t.handle.session(), t.handle.generation()));
        match term {
            Some((false, _, _)) => return,
            Some((true, sess, epoch)) => {
                if self.files.sftp.as_ref().and_then(|s| s.epoch()).is_some_and(|e| e != epoch) {
                    self.files.sftp = None;
                }
                if self.files.sftp.is_none() {
                    // 重连中(没有可用连接)就先不建,等连上
                    let Some(sess) = sess else { return };
                    let conn = self.conn_for(self.files.node);
                    self.files.sftp = Some(Sftp::connect(conn, Some((sess, epoch))));
                }
            }
            None => {
                if self.files.sftp.is_none() {
                    let conn = self.conn_for(self.files.node);
                    self.files.sftp = Some(Sftp::connect(conn, None));
                }
            }
        }
        let fs = &mut self.files;
        let Some(s) = fs.sftp.as_ref() else { return };
        let mut home = None;
        if matches!(s.status(), SftpStatus::Ready) {
            if !fs.path_ready {
                fs.path_ready = true;
                home = Some(s.home()).filter(|h| !h.is_empty());
            }
            let want = home.clone().unwrap_or_else(|| fs.path.clone());
            if s.dir(&want).is_none() {
                s.list(&want);
            }
            if let Some(n) = s.take_notice() {
                fs.notice = Some(n.into());
            }
        }
        let busy = s.busy();
        if let Some(h) = home {
            fs.path = h;
        }
        if busy && !self.fstore.polling {
            self.fstore.polling = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_millis(120)).await;
                    let Ok(more) = this.update(cx, |this, cx| {
                        cx.notify();
                        let more = this.files.sftp.as_ref().is_some_and(|s| s.busy());
                        if !more {
                            this.fstore.polling = false;
                        }
                        more
                    }) else {
                        break;
                    };
                    if !more {
                        break;
                    }
                }
            })
            .detach();
        }
    }

    /// 下载目的地:双栏时是本地栏当前目录,否则系统「下载」目录
    fn download_dir(&self) -> PathBuf {
        if self.files.local {
            return PathBuf::from(&self.files.lpath);
        }
        directories::UserDirs::new()
            .and_then(|d| d.download_dir().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| crate::platform::home_dir().join("Downloads"))
    }

    fn download_selected(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.files.selected.clone() else { return };
        let Some(e) = self.listing().into_iter().find(|e| e.name == name) else { return };
        let dest = self.download_dir();
        let _ = std::fs::create_dir_all(&dest);
        let remote = join(&self.files.path, &name);
        if let Some(s) = self.files.sftp.as_ref() {
            s.download(&remote, e.kind == Kind::Dir, dest.clone());
            self.files.notice = Some(format!("开始下载 {name} → {}", dest.display()).into());
        }
        cx.notify();
    }

    /// 可点击的表头:点击切换排序(同一列再点反向),当前排序列旁边显示方向箭头
    fn sort_th(&self, which: u8, label: &'static str, key: SortKey, cx: &mut Context<Self>) -> Stateful<Div> {
        let cur = if which == 1 { self.files.lsort } else { self.files.sort };
        let active = cur.0 == key;
        div()
            .id(("sort-th", which as usize * 10 + key as usize))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(3.0))
            .cursor_pointer()
            .text_size(px(t::TYPE_CAPTION))
            .text_color(if active { c::secondary() } else { c::ghost() })
            .hover(|s| s.text_color(c::secondary()))
            .child(label)
            .when(active, |d| d.child(icon(if cur.1 { IconName::ArrowUp } else { IconName::ArrowDown }, 11.0, c::secondary())))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_sort(which, key, cx)))
    }

    fn toggle_sort(&mut self, which: u8, key: SortKey, cx: &mut Context<Self>) {
        let slot = if which == 1 { &mut self.files.lsort } else { &mut self.files.sort };
        *slot = if slot.0 == key { (key, !slot.1) } else { (key, key.first_asc()) };
        if which == 1 {
            self.remember_local(cx);
        }
        cx.notify();
    }

    /// 把本地栏的目录和排序方式记下来(下次启动、新开文件页都从这里恢复)
    fn remember_local(&mut self, _cx: &mut Context<Self>) {
        let (dir, (k, asc)) = (self.files.lpath.clone(), self.files.lsort);
        if let Ok(mut p) = PREFS.lock() {
            *p = (Some(dir.clone()), Some((k, asc)));
        }
        if let Some(st) = self.store.as_ref() {
            let _ = st.set_setting("local_dir", &dir);
            let _ = st.set_setting("local_sort", &format!("{}:{}", k.tag(), if asc { "asc" } else { "desc" }));
        }
    }

    /// 拖拽下载:把远程当前目录里的一项下载到 `dest`
    fn download_to(&mut self, it: &DragRemote, dest: PathBuf, cx: &mut Context<Self>) {
        if !self.sftp_real() {
            self.files.notice = Some("演示数据不支持下载".into());
        } else if let Some(s) = self.files.sftp.as_ref().filter(|s| matches!(s.status(), SftpStatus::Ready)) {
            let _ = std::fs::create_dir_all(&dest);
            s.download(&join(&self.files.path, &it.name), it.is_dir, dest.clone());
            self.files.notice = Some(format!("开始下载 {} → {}", it.name, dest.display()).into());
        } else {
            self.files.notice = Some("还没连上服务器".into());
        }
        cx.notify();
    }

    /// 拖拽上传到远程的某个目录(拖到目录行上)
    fn upload_into(&mut self, paths: Vec<PathBuf>, dir: String, cx: &mut Context<Self>) {
        if self.sftp_real() {
            if let Some(s) = self.files.sftp.as_ref().filter(|s| matches!(s.status(), SftpStatus::Ready)) {
                let n = paths.len();
                s.upload(paths, &dir);
                self.files.notice = Some(format!("开始上传 {n} 项 → {dir}").into());
            } else {
                self.files.notice = Some("还没连上服务器".into());
            }
            cx.notify();
        } else {
            self.upload(paths, cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.sftp_real() {
            let path = self.files.path.clone();
            if let Some(s) = self.files.sftp.as_ref() {
                s.list(&path);
            }
            self.files.notice = None;
        } else {
            self.files.notice = Some("已刷新".into());
        }
        cx.notify();
    }

    fn upload(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.sftp_real() {
            let dir = self.files.path.clone();
            if let Some(s) = self.files.sftp.as_ref().filter(|s| matches!(s.status(), SftpStatus::Ready)) {
                let n = paths.len();
                s.upload(paths, &dir);
                self.files.notice = Some(format!("开始上传 {n} 项").into());
            } else {
                self.files.notice = Some("还没连上服务器".into());
            }
            cx.notify();
            return;
        }
        let dir = self.files.path.clone();
        let b = B { path: &dir, owner: "root" };
        let mut new = vec![];
        for p in &paths {
            let Some(name) = p.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
            let meta = std::fs::metadata(p).ok();
            let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
            let size = if is_dir { dir_size(p) } else { meta.map(|m| m.len()).unwrap_or(0) };
            let mut e = if is_dir { b.d(&name) } else { b.f(&name, size) };
            e.size = size.max(1);
            e.progress = Some(0.0);
            e.modified = "刚刚".into();
            e.local = (!is_dir).then(|| p.clone());
            new.push(e);
        }
        if new.is_empty() {
            return;
        }
        let fs = &mut self.files;
        let st = &mut self.fstore;
        st.batch += new.len();
        fs.selected = new.last().map(|e| e.name.clone());
        let key = fs.dir_key();
        let list = st.extra.entry(key.clone()).or_default();
        for e in new {
            st.deleted.remove(&format!("{key}/{}", e.name));
            list.retain(|o| o.name != e.name);
            list.push(e);
        }
        fs.notice = None;
        if !st.ticking {
            st.ticking = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_millis(50)).await;
                    let Ok(more) = this.update(cx, |this, cx| {
                        let more = this.step_uploads();
                        cx.notify();
                        more
                    }) else {
                        break;
                    };
                    if !more {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.notify();
    }

    /// 推进一帧上传进度(约 50 MB/s),返回是否还有没传完的
    fn step_uploads(&mut self) -> bool {
        let st = &mut self.fstore;
        let mut more = false;
        for e in st.extra.values_mut().flatten() {
            if let Some(p) = &mut e.progress {
                *p += (2.5 * M as f32 / e.size as f32).clamp(0.012, 0.34);
                if *p >= 1.0 {
                    e.progress = None;
                } else {
                    more = true;
                }
            }
        }
        if !more {
            st.ticking = false;
            let n = std::mem::take(&mut st.batch);
            self.files.notice = Some(format!("已上传 {n} 项").into());
        }
        more
    }

    fn pick_upload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("上传".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let _ = this.update(cx, |this, cx| this.upload(paths, cx));
        })
        .detach();
    }

    fn new_folder(&mut self, cx: &mut Context<Self>) {
        let names: HashSet<String> = self.listing().into_iter().map(|e| e.name).collect();
        let name = (1..)
            .map(|i| if i == 1 { "新建文件夹".to_string() } else { format!("新建文件夹 {i}") })
            .find(|n| !names.contains(n))
            .unwrap();
        let dir = self.files.path.clone();
        if self.sftp_real() {
            if let Some(s) = self.files.sftp.as_ref().filter(|s| matches!(s.status(), SftpStatus::Ready)) {
                s.mkdir(&join(&dir, &name));
                self.files.selected = Some(name);
            }
            cx.notify();
            return;
        }
        let mut e = B { path: &dir, owner: "root" }.d(&name);
        e.modified = "刚刚".into();
        let key = self.files.dir_key();
        self.fstore.extra.entry(key).or_default().push(e);
        self.files.selected = Some(name);
        cx.notify();
    }

    /// 文件区域的右键菜单:在鼠标位置弹出;右键某一项时先选中它,菜单里的操作都作用于选中项
    fn ctx_menu(&self, which: u8, cx: &mut Context<Self>) -> Option<AnyElement> {
        let m = self.files.ctx.clone().filter(|m| m.which == which)?;
        let is_dir = m.name.as_ref().is_some_and(|n| {
            if which == 1 {
                Path::new(&ljoin(&self.files.lpath, n)).is_dir()
            } else {
                self.listing().iter().any(|e| &e.name == n && e.kind == Kind::Dir)
            }
        });
        let mut items: Vec<(MenuItem, u8)> = Vec::new();
        match (which, m.name.is_some()) {
            (1, true) => {
                if is_dir {
                    items.push((MenuItem::new("打开").icon(IconName::FolderOpen), 10));
                }
                items.push((MenuItem::new("上传到服务器").icon(IconName::Upload), 1));
                items.push((MenuItem::new("复制路径").icon(IconName::Copy), 11));
            }
            (1, false) => {
                items.push((MenuItem::new("新建文件夹").icon(IconName::FolderPlus), 0));
                items.push((MenuItem::new("刷新").icon(IconName::RefreshCw), 12));
            }
            (_, true) => {
                if is_dir {
                    items.push((MenuItem::new("打开").icon(IconName::FolderOpen), 10));
                }
                if self.sftp_real() {
                    items.push((MenuItem::new("下载到本地").icon(IconName::Download), 3));
                }
                items.push((MenuItem::new("复制路径").icon(IconName::Copy), 11));
                items.push((MenuItem::new("删除").icon(IconName::Trash), 2));
            }
            (_, false) => {
                items.push((MenuItem::new("新建文件夹").icon(IconName::FolderPlus), 0));
                items.push((MenuItem::new("上传").icon(IconName::Upload), 1));
                items.push((MenuItem::new("刷新").icon(IconName::RefreshCw), 12));
            }
        }
        let (menu_items, acts): (Vec<_>, Vec<_>) = items.into_iter().unzip();
        let (t1, t2) = (cx.entity(), cx.entity());
        let name = m.name.clone();
        Some(
            deferred(
                anchored().position(m.pos).snap_to_window_with_margin(px(8.0)).child(div().relative().w(px(DROPDOWN_COMPACT_W)).h(px(0.0)).child(dropdown_menu_compact(
                    if which == 1 { "l-ctx-menu" } else { "r-ctx-menu" },
                    menu_items,
                    None,
                    MenuSide::Below,
                    0.0,
                    move |i, window, cx| {
                        let (act, name) = (acts[i], name.clone());
                        t1.update(cx, |h, cx| {
                            h.files.ctx = None;
                            h.ctx_action(which, name, act, window, cx)
                        })
                    },
                    move |_, cx| {
                        t2.update(cx, |h, cx| {
                            h.files.ctx = None;
                            cx.notify();
                        })
                    },
                ))),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// 右键菜单的操作:打开 / 复制路径 / 刷新,其余(新建、上传、下载、删除)和「更多」菜单共用
    fn ctx_action(&mut self, which: u8, name: Option<String>, act: u8, window: &mut Window, cx: &mut Context<Self>) {
        match (act, name) {
            (10, Some(n)) => {
                if which == 1 {
                    let to = ljoin(&self.files.lpath, &n);
                    self.local_go(to, cx);
                } else {
                    let to = join(&self.files.path, &n);
                    self.go(to, cx);
                }
            }
            (11, Some(n)) => {
                let full = if which == 1 { ljoin(&self.files.lpath, &n) } else { join(&self.files.path, &n) };
                cx.write_to_clipboard(ClipboardItem::new_string(full));
                self.files.notice = Some("已复制路径".into());
                cx.notify();
            }
            (12, _) => {
                if which == 1 {
                    cx.notify();
                } else {
                    self.refresh(cx);
                }
            }
            (a, _) => self.more_action(which, a, window, cx),
        }
    }

    /// 面包屑右侧的空白:点一下进入地址栏编辑
    fn addr_blank(which: u8, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id(("addr-blank", which as usize))
            .flex_1()
            .min_w(px(16.0))
            .h_full()
            .cursor_text()
            .on_click(cx.listener(move |this, _, window, cx| this.start_addr_edit(which, window, cx)))
    }

    /// 编辑状态的地址栏:和面包屑同尺寸的输入框,聚焦描边,路径无效时描边变红;Esc 放弃
    fn addr_box(&self, which: u8, cx: &mut Context<Self>) -> Div {
        let state = if which == 1 { self.files.laddr.clone() } else { self.files.addr.clone() };
        let err = self.files.addr_err;
        let mut b = div()
            .flex_1()
            .min_w_0()
            .h(px(30.0))
            .flex()
            .flex_row()
            .items_center()
            .pl(px(8.0))
            .pr(px(6.0))
            .gap(px(6.0))
            .rounded(px(t::RADIUS_MD))
            .when(err, |d| d.bg(c::danger().opacity(0.06)).border_1().border_color(c::danger().opacity(0.6)))
            .when(!err, |d| d.bg(c::sunken()).border_1().border_color(c::accent().opacity(0.55)))
            .text_size(px(t::TYPE_CONTROL))
            // 点到地址栏之外(包括不可聚焦的空白处)就放弃编辑:空白处不会抢走焦点,单靠失焦事件不够
            .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                if this.files.addr_edit == which {
                    this.files.addr_edit = 0;
                    this.files.addr_err = false;
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" && this.files.addr_edit != 0 {
                    this.files.addr_edit = 0;
                    this.files.addr_err = false;
                    cx.notify();
                }
            }));
        b = b.child(icon(if err { IconName::TriangleAlert } else { IconName::Pencil }, 13.0, if err { c::danger() } else { c::ghost() }));
        if let Some(st) = state {
            b = b.child(div().flex_1().min_w_0().child(Input::new(&st).appearance(false).with_size(px(12.0 / 0.875))));
        }
        if err {
            b = b.child(div().flex_shrink_0().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child("不是有效的目录"));
        }
        b
    }

    /// 进入地址栏编辑:输入框里是当前完整路径,全选,聚焦
    fn start_addr_edit(&mut self, which: u8, window: &mut Window, cx: &mut Context<Self>) {
        let (state, cur) = if which == 1 { (self.files.laddr.clone(), self.files.lpath.clone()) } else { (self.files.addr.clone(), self.files.path.clone()) };
        let Some(state) = state else { return };
        self.files.addr_edit = which;
        self.files.addr_err = false;
        state.update(cx, |s, cx| {
            s.set_value(cur, window, cx);
            s.select_all(window, cx);
        });
        window.focus(&state.focus_handle(cx), cx);
        cx.notify();
    }

    /// 回车:本地栏检查是不是目录,无效就留在输入框里标红;远程栏交给服务器(读目录失败会在列表区提示)
    fn commit_addr(&mut self, which: u8, cx: &mut Context<Self>) {
        let state = if which == 1 { self.files.laddr.clone() } else { self.files.addr.clone() };
        let Some(state) = state else { return };
        let text = state.read(cx).value().to_string();
        if text.trim().is_empty() {
            self.files.addr_edit = 0;
            cx.notify();
            return;
        }
        if which == 1 {
            let home = crate::platform::home_string();
            let to = local_resolve(&text, &self.files.lpath, &home);
            if Path::new(&to).is_dir() {
                self.files.addr_edit = 0;
                self.local_go(to, cx);
            } else {
                self.files.addr_err = true;
                cx.notify();
            }
        } else {
            let home = self.files_home();
            let to = resolve_path(&text, &self.files.path, &home);
            self.files.addr_edit = 0;
            self.go(to, cx);
        }
    }

    fn local_go(&mut self, path: String, cx: &mut Context<Self>) {
        let fs = &mut self.files;
        if path != fs.lpath {
            fs.lback.push(std::mem::replace(&mut fs.lpath, path));
            fs.lfwd.clear();
            fs.lsel = None;
        }
        self.remember_local(cx);
        cx.notify();
    }

    fn local_back(&mut self, cx: &mut Context<Self>) {
        let fs = &mut self.files;
        if let Some(p) = fs.lback.pop() {
            fs.lfwd.push(std::mem::replace(&mut fs.lpath, p));
            fs.lsel = None;
            self.remember_local(cx);
            cx.notify();
        }
    }

    fn local_fwd(&mut self, cx: &mut Context<Self>) {
        let fs = &mut self.files;
        if let Some(p) = fs.lfwd.pop() {
            fs.lback.push(std::mem::replace(&mut fs.lpath, p));
            fs.lsel = None;
            self.remember_local(cx);
            cx.notify();
        }
    }

    /// 本地栏「新建文件夹」:真的在本机创建
    fn local_mkdir(&mut self, cx: &mut Context<Self>) {
        let dir = self.files.lpath.clone();
        let name = (1..)
            .map(|i| if i == 1 { "新建文件夹".to_string() } else { format!("新建文件夹 {i}") })
            .find(|n| !Path::new(&ljoin(&dir, n)).exists())
            .unwrap();
        match std::fs::create_dir(ljoin(&dir, &name)) {
            Ok(()) => self.files.lsel = Some(name),
            Err(e) => self.files.notice = Some(format!("创建失败:{e}").into()),
        }
        cx.notify();
    }

    /// 「更多」菜单的动作。`which`:1 本地栏,2 远程栏
    fn more_action(&mut self, which: u8, act: u8, window: &mut Window, cx: &mut Context<Self>) {
        self.files.more = 0;
        match (which, act) {
            (1, 0) => self.local_mkdir(cx),
            (1, 1) => {
                if let Some(name) = self.files.lsel.clone() {
                    let p = PathBuf::from(ljoin(&self.files.lpath, &name));
                    if p.is_file() || (p.is_dir() && self.sftp_real()) {
                        self.upload(vec![p], cx);
                    } else {
                        self.files.notice = Some("请先选中要上传的项".into());
                        cx.notify();
                    }
                }
            }
            (2, 0) => self.new_folder(cx),
            (2, 1) => self.pick_upload(window, cx),
            (2, 2) => self.delete_selected(cx),
            (2, 3) => self.download_selected(cx),
            _ => cx.notify(),
        }
    }

    /// 工具栏最右侧的「更多」按钮和它的下拉菜单
    fn more_button(&self, which: u8, items: Vec<(MenuItem, u8)>, cx: &mut Context<Self>) -> Div {
        let open = self.files.more == which;
        let (menu_items, acts): (Vec<_>, Vec<_>) = items.into_iter().unzip();
        let (t1, t2) = (cx.entity(), cx.entity());
        div()
            .relative()
            .flex_shrink_0()
            .child(
                tool_btn(if which == 1 { "l-more" } else { "r-more" }, IconName::Ellipsis, true)
                    .when(open, |d| d.bg(c::hover()))
                    .tooltip(|_, cx| cx.new(|_| Tip("更多".into())).into())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.files.more = if this.files.more == which { 0 } else { which };
                        cx.notify();
                    })),
            )
            .when(open, |d| {
                d.child(div().absolute().top(px(32.0)).right_0().w(px(DROPDOWN_COMPACT_W)).h(px(0.0)).child(dropdown_menu_compact(
                    if which == 1 { "l-more-menu" } else { "r-more-menu" },
                    menu_items,
                    None,
                    MenuSide::Below,
                    0.0,
                    move |i, window, cx| {
                        let act = acts[i];
                        t1.update(cx, |h, cx| h.more_action(which, act, window, cx))
                    },
                    move |_, cx| {
                        t2.update(cx, |h, cx| {
                            h.files.more = 0;
                            cx.notify();
                        })
                    },
                )))
            })
    }

    /// 连接页的「取消/关闭」:文件标签 → 关掉这个标签;抽屉 → 收起抽屉
    pub(crate) fn close_sftp_page(&mut self, tab: bool, cx: &mut Context<Self>) {
        self.files.drop_sftp();
        if tab {
            if let Some(a) = self.workspace.active {
                self.close_tab(a, cx);
            }
        } else {
            self.files.open = false;
        }
        cx.notify();
    }

    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.files.confirm_del.take() else { return };
        let is_dir = self.listing().iter().any(|e| e.name == name && e.kind == Kind::Dir);
        let path = join(&self.files.path, &name);
        if let Some(s) = self.files.sftp.as_ref() {
            s.remove(&path, is_dir);
            self.files.notice = Some(format!("正在删除 {name}…").into());
        }
        self.files.selected = None;
        cx.notify();
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if self.sftp_real() {
            // 真实服务器上的删除不可恢复:先在工具栏下方二次确认
            self.files.confirm_del = self.files.selected.clone();
            cx.notify();
            return;
        }
        let fs = &mut self.files;
        let Some(name) = fs.selected.take() else { return };
        let key = fs.dir_key();
        if let Some(list) = self.fstore.extra.get_mut(&key) {
            list.retain(|e| e.name != name);
        }
        self.fstore.deleted.insert(format!("{key}/{name}"));
        fs.notice = Some(format!("已删除 {name}").into());
        cx.notify();
    }
}

fn entry_from_remote(r: &RemoteEntry) -> Entry {
    let modified = r
        .mtime
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();
    Entry {
        name: r.name.clone(),
        kind: if r.is_dir { Kind::Dir } else if r.is_link { Kind::Link } else { Kind::File },
        size: r.size,
        modified,
        mode: r.mode.clone(),
        owner: r.owner.clone(),
        progress: None,
        mount: None,
        link: r.link.clone(),
        local: None,
    }
}

/// 本机目录的一项:(名字, 是否目录, 大小, 修改时间)
fn read_local(dir: &str) -> Vec<(String, bool, u64, String)> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .take(5000)
                .filter_map(|e| {
                    let m = e.metadata().ok()?;
                    let t = m.modified().ok().map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
                    Some((e.file_name().to_string_lossy().into_owned(), m.is_dir(), m.len(), t))
                })
                .collect()
        })
        .unwrap_or_default();
    v
}

impl Home {
    /// 「本地」栏:本机目录(真实文件系统),只读浏览。
    fn local_pane(&self, pane_w: f32, cx: &mut Context<Self>) -> Stateful<Div> {
        let fs = &self.files;
        let path = fs.lpath.clone();
        let up = lparent(&path);
        let query = fs.lsearch.as_ref().map(|s| s.read(cx).value().trim().to_lowercase()).unwrap_or_default();
        let mut entries: Vec<_> = read_local(&path).into_iter().filter(|e| query.is_empty() || e.0.to_lowercase().contains(&query)).collect();
        sort_local(&mut entries, fs.lsort);
        // 面包屑:和远程栏同样的样式
        let mut crumbs = div()
            .flex_1()
            .min_w_0()
            .h(px(30.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .px(px(3.0))
            .rounded(px(t::RADIUS_MD))
            .bg(c::sunken())
            .border_1()
            .border_color(c::border())
            .overflow_hidden()
            .text_size(px(t::TYPE_CONTROL))
            .font_family(MONO)
            .child(
                div()
                    .id("lcrumb-root")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.0))
                    .h(px(22.0))
                    .w(px(22.0))
                    .justify_center()
                    .rounded(px(t::RADIUS_XS))
                    .cursor_pointer()
                    .hover(|s| s.bg(c::hover()))
                    .child(icon(IconName::House, 13.0, c::muted()))
                    .on_click(cx.listener(|this, _, _, cx| this.local_go(crate::platform::home_string(), cx))),
            );
        let segs = local_crumbs(&path);
        for (i, (sg, to)) in segs.iter().cloned().enumerate() {
            let last = i + 1 == segs.len();
            crumbs = crumbs.child(icon(IconName::ChevronRight, 11.0, c::ghost())).child(
                div()
                    .id(("lcrumb", i))
                    .flex_shrink_0()
                    .h(px(22.0))
                    .px(px(5.0))
                    .flex()
                    .items_center()
                    .rounded(px(t::RADIUS_SM))
                    .cursor_pointer()
                    .text_color(if last { c::text() } else { c::muted() })
                    .hover(|s| s.bg(c::hover()).text_color(c::text()))
                    .child(sg)
                    .on_click(cx.listener(move |this, _, _, cx| this.local_go(to.clone(), cx))),
            );
        }
        crumbs = crumbs.child(Self::addr_blank(1, cx));
        let search = self.filter_box(1, pane_w, cx);
        let more_items = vec![(MenuItem::new("新建文件夹").icon(IconName::FolderPlus), 0u8), (MenuItem::new("上传到服务器").icon(IconName::Upload), 1)];
        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(48.0))
            .flex_shrink_0()
            .px(px(12.0))
            .child(tool_btn("l-back", IconName::ArrowLeft, !fs.lback.is_empty()).on_click(cx.listener(|this, _, _, cx| this.local_back(cx))))
            .child(tool_btn("l-fwd", IconName::ArrowRight, !fs.lfwd.is_empty()).on_click(cx.listener(|this, _, _, cx| this.local_fwd(cx))))
            .child(tool_btn("l-up", IconName::ArrowUp, path != "/").on_click(cx.listener(move |this, _, _, cx| this.local_go(up.clone(), cx))))
            .child(tool_btn("l-refresh", IconName::RefreshCw, true).on_click(cx.listener(|_, _, _, cx| cx.notify())))
            .child(if fs.addr_edit == 1 { self.addr_box(1, cx) } else { crumbs })
            .children(search)
            .child(self.more_button(1, more_items, cx));
        let th = |t: &'static str| div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(t);
        let thead = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .h(px(30.0))
            .flex_shrink_0()
            .px(px(18.0))
            .border_b_1()
            .border_color(c::border())
            .child(self.sort_th(1, "名称", SortKey::Name, cx).flex_1().min_w_0())
            .child(self.sort_th(1, "大小", SortKey::Size, cx).w(px(76.0)).flex_shrink_0().flex().justify_end())
            .child(self.sort_th(1, "修改时间", SortKey::Time, cx).w(px(118.0)).flex_shrink_0());
        let mut rows = div()
            .id("local-rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .px(px(8.0))
            .py(px(4.0))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                this.files.ctx = Some(CtxMenu { which: 1, name: None, pos: ev.position });
                cx.notify();
            }));
        if entries.is_empty() {
            rows = rows.child(super::empty_state(IconName::FolderOpen, "空文件夹", "或没有访问权限"));
        }
        for (i, (name, is_dir, size, time)) in entries.into_iter().enumerate() {
            let on = fs.lsel.as_deref() == Some(name.as_str());
            let (ic, color) = if is_dir { (IconName::Folder, c::accent()) } else { (IconName::File, c::muted()) };
            let enter = is_dir.then(|| ljoin(&path, &name));
            let n = name.clone();
            let name_r = name.clone();
            let row_w = pane_w - 16.0;
            let drag_item = DragLocal { path: PathBuf::from(ljoin(&path, &name)), name: name.clone(), is_dir };
            let drop_name = name.clone();
            rows = rows.child(
                div()
                    .id(("lrow", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .h(px(32.0))
                    .flex_shrink_0()
                    .px(px(10.0))
                    .rounded(px(t::RADIUS_MD))
                    .cursor_pointer()
                    .when(on, |d| d.bg(c::selected()))
                    .when(!on, |d| d.hover(|s| s.bg(c::hover())))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(9.0))
                            .child(icon(ic, 15.0, color))
                            .child(div().min_w_0().truncate().text_size(px(t::TYPE_BODY)).text_color(if on { c::text() } else { c::secondary() }).child(name)),
                    )
                    .child(
                        div()
                            .w(px(76.0))
                            .flex_shrink_0()
                            .flex()
                            .justify_end()
                            .text_size(px(t::TYPE_CAPTION))
                            .font_family(MONO)
                            .text_color(c::muted())
                            .child(if is_dir { "—".to_string() } else { fmt_size(size) }),
                    )
                    .child(div().w(px(118.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::muted()).child(time))
                    .on_drag(drag_item, move |d, _, _, cx| {
                        let (name, is_dir) = (d.name.clone(), d.is_dir);
                        cx.new(|_| DragGhost { name, is_dir, w: row_w })
                    })
                    // 目录行:把远程项拖到它上面,下载到这个目录
                    .when(is_dir, |d| {
                        let lp = path.clone();
                        d.drag_over::<DragRemote>(|s, _, _, _| s.bg(c::accent_soft())).on_drop(cx.listener(move |this, it: &DragRemote, _, cx| {
                            cx.stop_propagation();
                            this.download_to(it, PathBuf::from(ljoin(&lp, &drop_name)), cx);
                        }))
                    })
                    .on_mouse_down(MouseButton::Right, {
                        let n = name_r.clone();
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.files.lsel = Some(n.clone());
                            this.files.ctx = Some(CtxMenu { which: 1, name: Some(n.clone()), pos: ev.position });
                            cx.notify();
                        })
                    })
                    .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                        if ev.click_count() >= 2
                            && let Some(to) = &enter
                        {
                            this.local_go(to.clone(), cx);
                        } else {
                            this.files.lsel = Some(n.clone());
                            cx.notify();
                        }
                    })),
            );
        }
        let lpath_drop = path.clone();
        div()
            .id("local-pane")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(c::border())
            // 把远程项拖到本地栏:下载到当前目录
            .drag_over::<DragRemote>(|s, _, _, _| s.bg(c::accent().opacity(0.05)))
            .on_drop(cx.listener(move |this, it: &DragRemote, _, cx| this.download_to(it, PathBuf::from(&lpath_drop), cx)))
            .child(toolbar)
            .child(thead)
            .child(rows)
            .children(self.ctx_menu(1, cx))
    }
}

// ---- 渲染 --------------------------------------------------------------------
fn file_icon(e: &Entry) -> (IconName, Rgba) {
    match e.kind {
        Kind::Dir if e.mount.is_some() => (IconName::HardDrive, c::accent()),
        Kind::Dir => (IconName::Folder, c::folder()),
        Kind::Link => (IconName::Link, c::muted()),
        Kind::File => {
            let n = e.name.as_str();
            if is_secret(n) {
                (IconName::LockKeyhole, c::warn())
            } else if is_image(n) {
                (IconName::FileImage, c::net())
            } else if is_archive(n) {
                (IconName::FileArchive, c::disk())
            } else if ["sh", "py", "js", "rs"].contains(&ext(n)) {
                (IconName::FileCode, c::accent())
            } else if ["yml", "yaml", "json", "xml", "toml", "conf", "ini", "env"].contains(&ext(n)) {
                (IconName::FileCog, c::mem())
            } else if is_text(n) {
                (IconName::FileText, c::secondary())
            } else {
                (IconName::File, c::muted())
            }
        }
    }
}

fn tool_btn(id: &'static str, ic: IconName, enabled: bool) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(28.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(t::RADIUS_MD))
        .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(c::hover())))
        .child(icon(ic, 15.0, if enabled { c::secondary() } else { c::ghost().opacity(0.6) }))
}

fn progress_bar(p: f32, w: f32) -> Div {
    div()
        .w(px(w))
        .h(px(4.0))
        .rounded_full()
        .bg(c::idle_soft())
        .child(div().h_full().w(px(w * p.clamp(0.0, 1.0))).rounded_full().bg(c::accent()))
}

impl Home {
    /// 右侧文件抽屉(盖在节点标签的内容区上)
    pub fn files_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.files.open {
            return None;
        }
        // 抽屉开着时切了节点:回到新节点本机的主目录
        if self.files.node != self.sel {
            self.files.node = self.sel;
            self.files.target = Target::Host;
            self.files.path = Files::home_of(&Target::Host).into();
            self.files.back.clear();
            self.files.fwd.clear();
            self.files.selected = None;
        }
        let max = Self::max_w(window);
        let want = if self.files.target == Target::Host { HOST_W } else { CT_W };
        let w = want.clamp(MIN_W.min(max), max);
        Some(self.files_panel(window, cx, w, false))
    }

    /// 文件标签页:占满整个标签内容区
    pub fn files_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let w = f32::from(window.viewport_size().width);
        self.files_panel(window, cx, w, true)
    }

    /// 文件管理的完整界面;`tab_mode` 为真时作为标签内容铺满,否则作为抽屉。
    fn files_panel(&mut self, _window: &mut Window, cx: &mut Context<Self>, w: f32, tab_mode: bool) -> AnyElement {
        self.sync_sftp(cx);
        if self.files.local {
            self.restore_left(cx);
            self.sync_left(cx);
        }
        // 真实连接没连上之前,整页显示「连接中」(步骤、指纹确认、失败原因、重试),连上后才是文件列表。
        // 这个标签的终端还没连上时,显示的是终端的连接进度(SFTP 要等它)。
        let early = if !self.sftp_real() {
            None
        } else if let Some((a, t)) = self.workspace.active.and_then(|a| self.workspace.tabs.get(a).and_then(|t| t.term.as_ref()).map(|t| (a, t))).filter(|(_, t)| !t.handle.ever_connected()) {
            Some(self.connecting_page(crate::terminal::connecting::ConnTarget::Terminal(a), t.handle.steps(), t.handle.host_prompt(), crate::terminal::ssh::STEP_LABELS, cx))
        } else if let Some(s) = self.files.sftp.as_ref() {
            let status = s.status();
            // 复用终端连接时只差开个通道(很快),不整页切「连接中」,在列表区显示「正在读取」
            if matches!(status, SftpStatus::Ready) || (matches!(status, SftpStatus::Connecting) && s.epoch().is_some()) {
                None
            } else {
                let prompt = if let SftpStatus::HostPrompt(p) = status { Some(p) } else { None };
                Some(self.connecting_page(crate::terminal::connecting::ConnTarget::Sftp(tab_mode), s.steps(), prompt, crate::terminal::connecting::SFTP_STEP_LABELS, cx))
            }
        } else {
            None
        };
        {
            if let Some(page) = early {
                return if tab_mode {
                    page.into_any_element()
                } else {
                    div()
                        .id("files-drawer")
                        .absolute()
                        .top(px(TITLEBAR_H))
                        .right_0()
                        .bottom_0()
                        .w(px(w))
                        .flex()
                        .flex_col()
                        .bg(c::dialog())
                        .border_l_1()
                        .border_color(c::border_strong())
                        .occlude()
                        .child(page)
                        .into_any_element()
                };
            }
        }
        let node = self.node();
        let fs = &self.files;
        // 打开「本地」后分成左右两栏,每栏各占一半
        let pane_w = if fs.local { w / 2.0 } else { w };
        let query = fs.search.as_ref().map(|s| s.read(cx).value().trim().to_lowercase()).unwrap_or_default();
        let all = self.listing();
        let entries: Vec<Entry> = all.iter().filter(|e| query.is_empty() || e.name.to_lowercase().contains(&query)).cloned().collect();
        let selected = fs.selected.as_ref().and_then(|n| all.iter().find(|e| &e.name == n)).cloned();

        // ---- 头部:标题 + 目标切换 + 宽度 / 关闭 ----
        let is_host = fs.target == Target::Host;
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .h(px(52.0))
            .flex_shrink_0()
            .pl(px(18.0))
            .pr(px(10.0))
            .border_b_1()
            .border_color(c::border())
            .child(
                div()
                    .size(px(30.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(t::RADIUS_MD))
                    .bg(c::accent_soft())
                    .child(icon(IconName::FolderOpen, 15.0, c::accent())),
            )
            .child(div().text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::SEMIBOLD).child("文件"))
            .when(fs.local, |d| d.child(div().ml(px(6.0)).child(self.source_button(cx))))
            .child(div().flex_1())
            .child(
                div()
                    .id("files-local-toggle")
                    .h(px(30.0))
                    .px(px(10.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(t::RADIUS_MD))
                    .cursor_pointer()
                    .hover(|s| s.bg(c::hover()))
                    .child(icon(IconName::Columns2, 14.0, if fs.local { c::accent() } else { c::ghost() }))
                    .child(div().text_size(px(t::TYPE_CONTROL)).text_color(if fs.local { c::text() } else { c::muted() }).child("双栏"))
                    .child(
                        // 开关:轨道 + 滑块
                        div()
                            .w(px(28.0))
                            .h(px(16.0))
                            .px(px(2.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .when(fs.local, |d| d.justify_end())
                            .rounded_full()
                            .bg(if fs.local { c::accent() } else { c::border_strong() })
                            .child(div().size(px(12.0)).rounded_full().bg(if fs.local { c::on_accent() } else { c::muted() })),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.files.local = !this.files.local;
                        cx.notify();
                    })),
            )
            .when(!tab_mode, |d| {
                d.child(tool_btn("files-close", IconName::X, true).on_click(cx.listener(|this, _, _, cx| this.close_files(cx))))
            });

        // ---- 工具栏:前进后退 / 面包屑 / 筛选 / 新建 / 上传 ----
        let path = fs.path.clone();
        let up_path = parent(&path);
        let mount = fs.target.mount_of(&path);
        let mut crumbs = div()
            .flex_1()
            .min_w_0()
            .h(px(30.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .px(px(3.0))
            .rounded(px(t::RADIUS_MD))
            .bg(c::sunken())
            .border_1()
            .border_color(c::border())
            .overflow_hidden()
            .text_size(px(t::TYPE_CONTROL))
            .font_family(MONO)
            .child(
                div()
                    .id("crumb-root")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.0))
                    .h(px(22.0))
                    .w(px(22.0))
                    .justify_center()
                    .rounded(px(t::RADIUS_XS))
                    .cursor_pointer()
                    .hover(|s| s.bg(c::hover()))
                    .child(icon(IconName::House, 13.0, c::muted()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let home = this.files_home();
                        this.go(home, cx)
                    })),
            );
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        for (i, s) in segs.iter().enumerate() {
            let to = format!("/{}", segs[..=i].join("/"));
            let last = i + 1 == segs.len();
            crumbs = crumbs.child(icon(IconName::ChevronRight, 11.0, c::ghost())).child(
                div()
                    .id(("crumb", i))
                    .flex_shrink_0()
                    .h(px(22.0))
                    .px(px(5.0))
                    .flex()
                    .items_center()
                    .rounded(px(t::RADIUS_SM))
                    .cursor_pointer()
                    .text_color(if last { c::text() } else { c::muted() })
                    .hover(|s| s.bg(c::hover()).text_color(c::text()))
                    .child(s.to_string())
                    .on_click(cx.listener(move |this, _, _, cx| this.go(to.clone(), cx))),
            );
        }
        crumbs = crumbs.child(Self::addr_blank(2, cx));
        if let Some((_, host)) = &mount {
            let host = host.clone();
            crumbs = crumbs.child(
                div()
                    .id("mount-badge")
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.0))
                    .h(px(20.0))
                    .px(px(7.0))
                    .rounded(px(t::RADIUS_SM))
                    .bg(c::accent_soft())
                    .text_size(px(t::TYPE_MICRO))
                    .text_color(c::accent())
                    .child(icon(IconName::HardDrive, 11.0, c::accent()))
                    .child("挂载卷")
                    .tooltip(move |_, cx| cx.new(|_| Tip(format!("宿主机路径 {host}"))).into()),
            );
        }
        let search = self.filter_box(2, pane_w, cx);
        let mut more_items = vec![(MenuItem::new("新建文件夹").icon(IconName::FolderPlus), 0u8), (MenuItem::new("上传").icon(IconName::Upload), 1)];
        if selected.is_some() {
            if self.sftp_real() {
                more_items.push((MenuItem::new("下载到本地").icon(IconName::Download), 3));
            }
            more_items.push((MenuItem::new("删除选中项").icon(IconName::Trash), 2));
        }
        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(48.0))
            .flex_shrink_0()
            .px(px(12.0))
            .child(tool_btn("f-back", IconName::ArrowLeft, !fs.back.is_empty()).on_click(cx.listener(|this, _, _, cx| this.go_back(cx))))
            .child(tool_btn("f-fwd", IconName::ArrowRight, !fs.fwd.is_empty()).on_click(cx.listener(|this, _, _, cx| this.go_fwd(cx))))
            .child(tool_btn("f-up", IconName::ArrowUp, path != "/").on_click(cx.listener(move |this, _, _, cx| this.go(up_path.clone(), cx))))
            .child(tool_btn("f-refresh", IconName::RefreshCw, true).on_click(cx.listener(|this, _, _, cx| this.refresh(cx))))
            .child(if fs.addr_edit == 2 { self.addr_box(2, cx) } else { crumbs })
            .children(search)
            .child(self.more_button(2, more_items, cx));

        // 容器层的提醒:不在挂载卷里的改动,重建容器就没了
        let warn = (!is_host && mount.is_none() && path != "/").then(|| {
            let mounts = fs.target.mounts().into_iter().map(|(m, _)| m).collect::<Vec<_>>().join("、");
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .mx(px(12.0))
                .mb(px(8.0))
                .px(px(10.0))
                .py(px(7.0))
                .rounded(px(t::RADIUS_MD))
                .bg(c::warn_soft())
                .text_size(px(t::TYPE_CONTROL))
                .text_color(c::warn())
                .child(icon(IconName::TriangleAlert, 13.0, c::warn()))
                .child(format!("这里是容器层,重建容器后改动会丢失;需要保留的文件请放进挂载卷 {mounts}"))
        });

        // 删除确认条(真实服务器上删除不可恢复)
        let confirm = fs.confirm_del.clone().map(|name| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .mx(px(12.0))
                .mb(px(8.0))
                .pl(px(10.0))
                .pr(px(6.0))
                .h(px(34.0))
                .rounded(px(t::RADIUS_MD))
                .bg(c::danger().opacity(0.14))
                .text_size(px(t::TYPE_CONTROL))
                .text_color(c::danger())
                .child(icon(IconName::Trash, 13.0, c::danger()))
                .child(div().flex_1().min_w_0().truncate().child(format!("确认删除「{name}」?服务器上的删除无法恢复")))
                .child(
                    div()
                        .id("del-cancel")
                        .h(px(24.0))
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .rounded(px(t::RADIUS_SM))
                        .text_color(c::muted())
                        .cursor_pointer()
                        .hover(|s| s.bg(c::hover()))
                        .child("取消")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.files.confirm_del = None;
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id("del-ok")
                        .h(px(24.0))
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .rounded(px(t::RADIUS_SM))
                        .bg(c::danger())
                        .text_color(c::on_accent())
                        .font_weight(FontWeight::SEMIBOLD)
                        .cursor_pointer()
                        .hover(|s| s.bg(c::danger().opacity(0.85)))
                        .child("删除")
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_delete(cx))),
                )
        });

        // ---- 中:文件表格 ----
        let list_w = pane_w;
        let show_mode = list_w >= 520.0;
        let show_owner = list_w >= 600.0;
        let show_time = list_w >= 400.0;
        let cols = |name: AnyElement, size: AnyElement, time: AnyElement, mode: AnyElement, owner: AnyElement| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .child(div().flex_1().min_w_0().child(name))
                .child(div().w(px(76.0)).flex_shrink_0().flex().justify_end().child(size))
                .when(show_time, |d| d.child(div().w(px(118.0)).flex_shrink_0().child(time)))
                .when(show_mode, |d| d.child(div().w(px(82.0)).flex_shrink_0().child(mode)))
                .when(show_owner, |d| d.child(div().w(px(48.0)).flex_shrink_0().child(owner)))
        };
        let (h1, h2, h3, h4, h5) = (
            self.sort_th(2, "名称", SortKey::Name, cx).into_any_element(),
            self.sort_th(2, "大小", SortKey::Size, cx).into_any_element(),
            self.sort_th(2, "修改时间", SortKey::Time, cx).into_any_element(),
            self.sort_th(2, "权限", SortKey::Mode, cx).into_any_element(),
            self.sort_th(2, "所有者", SortKey::Owner, cx).into_any_element(),
        );
        let thead = cols(h1, h2, h3, h4, h5)
            .h(px(30.0))
            .flex_shrink_0()
            .px(px(18.0))
            .border_b_1()
            .border_color(c::border());
        let mut rows = div()
            .id("file-rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .px(px(8.0))
            .py(px(4.0))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                this.files.ctx = Some(CtxMenu { which: 2, name: None, pos: ev.position });
                cx.notify();
            }));
        let real_state = self.sftp_state_view(cx);
        let loading = real_state.is_none() && self.sftp_loading();
        let entries: Vec<Entry> = if real_state.is_some() || loading { vec![] } else { entries };
        if loading {
            // 加载中:用和文件行同样版式的骨架占位,内容出来时版面不跳
            const NAME_W: [f32; 9] = [140.0, 96.0, 180.0, 120.0, 76.0, 160.0, 110.0, 88.0, 132.0];
            let bar = |w: f32| div().h(px(8.0)).w(px(w)).rounded_full().bg(c::hover()).into_any_element();
            let mut sk = div().flex().flex_col();
            for (i, w) in NAME_W.iter().enumerate() {
                let name = div().flex().flex_row().items_center().gap(px(10.0)).child(div().size(px(16.0)).rounded(px(4.0)).bg(c::hover())).child(bar(*w)).into_any_element();
                sk = sk.child(cols(name, bar(34.0), bar(92.0), bar(56.0), bar(28.0)).h(px(32.0)).flex_shrink_0().px(px(10.0)).opacity(1.0 - i as f32 * 0.07));
            }
            rows = rows.child(sk.with_animation("file-skeleton", Animation::new(Duration::from_millis(SKELETON_MS)).repeat().with_easing(bounce(ease_in_out)), |d, t| d.opacity(0.45 + 0.55 * t)));
        } else if let Some(v) = real_state {
            rows = rows.child(v);
        } else if entries.is_empty() {
            let (t, s) = if query.is_empty() { ("空文件夹", "把文件拖到这里即可上传") } else { ("没有匹配的文件", "换个关键词试试") };
            rows = rows.child(super::empty_state(IconName::FolderOpen, t, s));
        }
        for (i, e) in entries.iter().enumerate() {
            let (ic, color) = file_icon(e);
            let on = selected.as_ref().is_some_and(|s| s.name == e.name);
            let name_el = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(9.0))
                .child(icon(ic, 15.0, color))
                .child(div().min_w_0().truncate().text_size(px(t::TYPE_BODY)).text_color(if on { c::text() } else { c::secondary() }).child(e.name.clone()))
                .when_some(e.link.clone(), |d, l| d.child(div().flex_shrink_0().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::ghost()).child(format!("→ {l}"))))
                .when(e.mount.is_some(), |d| {
                    d.child(div().flex_shrink_0().px(px(5.0)).rounded(px(t::RADIUS_XS)).bg(c::accent_soft()).text_size(px(t::TYPE_MICRO)).text_color(c::accent()).child("挂载"))
                })
                .into_any_element();
            let size_el = match e.progress {
                Some(p) => div().flex().flex_row().items_center().gap(px(6.0)).child(progress_bar(p, 38.0)).child(
                    div().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::accent()).child(format!("{:.0}%", p * 100.0)),
                ),
                None => div()
                    .text_size(px(t::TYPE_CAPTION))
                    .font_family(MONO)
                    .text_color(c::muted())
                    .child(if e.is_dir() { "—".to_string() } else { fmt_size(e.size) }),
            }
            .into_any_element();
            let mono = |t: String, color: Rgba| div().truncate().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(color).child(t).into_any_element();
            let name = e.name.clone();
            let enter = e.is_dir().then(|| join(&path, &e.name));
            let name_r = e.name.clone();
            let row_w = list_w - 16.0;
            let drag_item = DragRemote { name: e.name.clone(), is_dir: e.is_dir(), left: false };
            let folder_to = e.is_dir().then(|| join(&path, &e.name));
            rows = rows.child(
                cols(name_el, size_el, mono(e.modified.clone(), c::muted()), mono(e.mode.clone(), c::ghost()), mono(e.owner.clone(), c::ghost()))
                    .id(("frow", i))
                    .h(px(32.0))
                    .flex_shrink_0()
                    .px(px(10.0))
                    .rounded(px(t::RADIUS_MD))
                    .cursor_pointer()
                    .when(on, |d| d.bg(c::selected()))
                    .when(!on, |d| d.hover(|s| s.bg(c::hover())))
                    .on_drag(drag_item, move |d, _, _, cx| {
                        let (name, is_dir) = (d.name.clone(), d.is_dir);
                        cx.new(|_| DragGhost { name, is_dir, w: row_w })
                    })
                    // 目录行:把本地项拖到它上面,上传到这个目录
                    .when_some(folder_to, |d, to| {
                        let to2 = to.clone();
                        d.drag_over::<DragLocal>(|s, _, _, _| s.bg(c::accent_soft()))
                            .on_drop(cx.listener(move |this, it: &DragLocal, _, cx| {
                                cx.stop_propagation();
                                this.upload_into(vec![it.path.clone()], to.clone(), cx);
                            }))
                            .drag_over::<DragRemote>(|s, it, _, _| if it.left { s.bg(c::accent_soft()) } else { s })
                            .on_drop(cx.listener(move |this, it: &DragRemote, _, cx| {
                                cx.stop_propagation();
                                if it.left {
                                    this.relay_drop(it, false, to2.clone(), cx);
                                }
                            }))
                    })
                    .on_mouse_down(MouseButton::Right, {
                        let n = name_r.clone();
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.files.selected = Some(n.clone());
                            this.files.ctx = Some(CtxMenu { which: 2, name: Some(n.clone()), pos: ev.position });
                            cx.notify();
                        })
                    })
                    .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                        if ev.click_count() >= 2
                            && let Some(to) = &enter
                        {
                            this.go(to.clone(), cx);
                        } else {
                            this.files.selected = Some(name.clone());
                            cx.notify();
                        }
                    })),
            );
        }
        let table = div()
            .id("remote-table")
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            // 把本地项拖到远程栏:上传到当前目录
            .drag_over::<DragLocal>(|s, _, _, _| s.bg(c::accent().opacity(0.05)))
            .on_drop(cx.listener(|this, it: &DragLocal, _, cx| this.upload(vec![it.path.clone()], cx)))
            .drag_over::<DragRemote>(|s, it, _, _| if it.left { s.bg(c::accent().opacity(0.05)) } else { s })
            .on_drop(cx.listener(|this, it: &DragRemote, _, cx| {
                if it.left {
                    let dir = this.files.path.clone();
                    this.relay_drop(it, false, dir, cx);
                }
            }))
            .child(thead)
            .child(rows)
            .children(self.ctx_menu(2, cx));

        // ---- 状态栏 ----
        let uploading: Vec<&Entry> = self.fstore.extra.values().flatten().filter(|e| e.progress.is_some()).collect();
        // 真实传输:汇总进行中的上传/下载
        let xfer = fs.sftp.as_ref().filter(|_| self.sftp_real()).map(|s| s.transfers()).unwrap_or_default();
        let running: Vec<_> = xfer.iter().filter(|x| x.state == TransferState::Running).collect();
        let real_up = (!running.is_empty()).then(|| {
            let total: f32 = running.iter().map(|x| x.total as f32).sum();
            let done: f32 = running.iter().map(|x| x.done as f32).sum();
            let p = if total > 0.0 { (done / total).min(1.0) } else { 0.0 };
            let up = running.iter().any(|x| x.dir == Direction::Up);
            let down = running.iter().any(|x| x.dir == Direction::Down);
            let label = match (up, down) {
                (true, true) => "传输",
                (true, false) => "上传",
                _ => "下载",
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .text_color(c::accent())
                .child(icon(if down && !up { IconName::Download } else { IconName::Upload }, 12.0, c::accent()))
                .child(format!("{label} {} 项", running.len()))
                .child(progress_bar(p, 80.0))
                .child(div().font_family(MONO).child(format!("{:.0}%", p * 100.0)))
        });
        let up_info = (!uploading.is_empty()).then(|| {
            let total: f32 = uploading.iter().map(|e| e.size as f32).sum();
            let done: f32 = uploading.iter().map(|e| e.size as f32 * e.progress.unwrap_or(0.0)).sum();
            let p = if total > 0.0 { done / total } else { 0.0 };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .text_color(c::accent())
                .child(icon(IconName::Upload, 12.0, c::accent()))
                .child(format!("上传 {} 项", uploading.len()))
                .child(progress_bar(p, 80.0))
                .child(div().font_family(MONO).child(format!("{:.0}%", p * 100.0)))
        });
        let via = match &fs.target {
            Target::Host => format!("SFTP · {}", node.user_host),
            Target::Container { svc, .. } => format!("docker exec · {svc}"),
        };
        let status = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.0))
            .h(px(30.0))
            .flex_shrink_0()
            .px(px(16.0))
            .border_t_1()
            .border_color(c::border())
            .bg(c::chrome())
            .text_size(px(t::TYPE_CAPTION))
            .text_color(c::ghost())
            .child(if query.is_empty() { format!("{} 项", all.len()) } else { format!("{} / {} 项", entries.len(), all.len()) })
            .when_some(selected.as_ref(), |d, e| d.child(div().truncate().text_color(c::muted()).child(format!("已选 {}", e.name))))
            .child(div().flex_1())
            .children(up_info)
            .children(real_up)
            .when(uploading.is_empty() && running.is_empty(), |d| d.when_some(fs.notice.clone(), |d, n| d.child(div().text_color(c::muted()).child(n))))
            .child(div().font_family(MONO).child(via));

        // ---- 拖入上传的提示层:平时透明,有外部文件拖到抽屉上方时显现 ----
        let drop_path = path.clone();
        let overlay = div()
            .id("drop-zone")
            .absolute()
            .top(px(52.0))
            .left_0()
            .right_0()
            .bottom(px(30.0))
            .p(px(10.0))
            .opacity(0.0)
            .drag_over::<ExternalPaths>(|s, _, _, _| s.opacity(1.0))
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(10.0))
                    .rounded(px(t::RADIUS_XL))
                    .border_2()
                    .border_dashed()
                    .border_color(c::accent().opacity(0.7))
                    .bg(c::scrim_drop())
                    .child(
                        div()
                            .size(px(54.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(c::accent_soft())
                            .child(icon(IconName::CloudUpload, 24.0, c::accent())),
                    )
                    .child(div().text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::SEMIBOLD).child("松开即可上传"))
                    .child(div().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(format!("→ {drop_path}"))),
            );

        let remote = div().flex_1().min_w_0().min_h_0().flex().flex_col().child(toolbar).children(warn).children(confirm).child(table);
        let body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_row()
            .when(fs.local && fs.left.node.is_none(), |d| d.child(self.local_pane(pane_w, cx)))
            .when(fs.local && fs.left.node.is_some(), |d| d.child(self.server_left_pane(pane_w, cx)))
            .child(remote);

        div()
            .id(if tab_mode { "files-tab" } else { "files-drawer" })
            .relative()
            .flex()
            .flex_col()
            .when(!tab_mode, |d| d.bg(c::dialog()))
            .occlude()
            .when(tab_mode, |d| d.flex_1().min_w_0().min_h_0())
            .when(!tab_mode, |d| {
                d.absolute()
                    .top(px(TITLEBAR_H))
                    .right_0()
                    .bottom_0()
                    .w(px(w))
                    .border_l_1()
                    .border_color(c::border_strong())
                    .shadow(vec![BoxShadow {
                        color: hsla(0.0, 0.0, 0.0, 0.45),
                        offset: point(px(-14.0), px(0.0)),
                        blur_radius: px(36.0),
                        spread_radius: px(0.0),
                        inset: false,
                    }])
            })
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.upload(paths.paths().to_vec(), cx)))
            .child(header)
            .child(body)
            .child(status)
            .child(overlay)
            .into_any_element()
    }
}


impl Home {
    /// 筛选框:平时只是一个搜索图标;点击展开成输入框并聚焦,尾部的 × 清空并收起。
    /// `which`:1 本地栏,2 远程栏。
    fn filter_box(&self, which: u8, pane_w: f32, cx: &mut Context<Self>) -> Option<AnyElement> {
        let fs = &self.files;
        let (state, open, closing) = if which == 1 { (fs.lsearch.clone()?, fs.lsopen, fs.lsclosing) } else { (fs.search.clone()?, fs.sopen, fs.sclosing) };
        let set_open = move |this: &mut Home, v: bool| {
            if which == 1 {
                this.files.lsopen = v;
            } else {
                this.files.sopen = v;
            }
        };
        if !open {
            let st = state.clone();
            return Some(
                tool_btn(if which == 1 { "l-search" } else { "r-search" }, IconName::Search, true)
                    .tooltip(|_, cx| cx.new(|_| Tip("筛选".into())).into())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        set_open(this, true);
                        window.focus(&st.focus_handle(cx), cx);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        let st = state.clone();
        let full = if pane_w < 640.0 { 150.0 } else { 200.0 };
        Some(
            div()
                .id((if which == 1 { "l-search-box" } else { "r-search-box" }, closing as usize))
                .w(px(full))
                .overflow_hidden()
                .flex_shrink_0()
                .h(px(30.0))
                .flex()
                .flex_row()
                .items_center()
                .pl(px(9.0))
                .pr(px(3.0))
                .rounded(px(t::RADIUS_MD))
                .bg(c::sunken())
                .border_1()
                .border_color(c::border())
                .text_size(px(t::TYPE_CONTROL))
                .child(icon(IconName::Search, 13.0, c::ghost()))
                .child(div().flex_1().min_w_0().child(Input::new(&state).appearance(false).with_size(px(12.0 / 0.875))))
                .child(
                    div()
                        .id(if which == 1 { "l-search-close" } else { "r-search-close" })
                        .size(px(22.0))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(t::RADIUS_SM))
                        .cursor_pointer()
                        .hover(|s| s.bg(c::hover()))
                        .child(icon(IconName::X, 13.0, c::muted()))
                        .tooltip(|_, cx| cx.new(|_| Tip("清空并关闭".into())).into())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // 先清空(列表立刻恢复),再播收起动画,播完真正收起
                            st.update(cx, |s, cx| s.set_value("", window, cx));
                            if which == 1 {
                                this.files.lsclosing = true;
                            } else {
                                this.files.sclosing = true;
                            }
                            cx.notify();
                            cx.spawn(async move |this, cx| {
                                cx.background_executor().timer(Duration::from_millis(SEARCH_ANIM_MS)).await;
                                let _ = this.update(cx, |this, cx| {
                                    set_open(this, false);
                                    if which == 1 {
                                        this.files.lsclosing = false;
                                    } else {
                                        this.files.sclosing = false;
                                    }
                                    cx.notify();
                                });
                            })
                            .detach();
                        })),
                )
                // 展开:从图标宽度长到完整宽度并淡入;收起:反过来
                .with_animation(
                    (if which == 1 { "l-search-anim" } else { "r-search-anim" }, closing as usize),
                    Animation::new(Duration::from_millis(SEARCH_ANIM_MS)).with_easing(ease_out_quint()),
                    move |d, t| {
                        let t = if closing { 1.0 - t } else { t };
                        d.w(px(28.0 + (full - 28.0) * t)).opacity(0.25 + 0.75 * t)
                    },
                )
                .into_any_element(),
        )
    }

    /// 真实 SFTP 正在加载目录(含复用终端连接时等通道打开):列表区显示骨架占位
    fn sftp_loading(&self) -> bool {
        // 调试:OTTER_FILES_LOADING=1 强制显示加载骨架(截图用)
        if std::env::var_os("OTTER_FILES_LOADING").is_some() {
            return true;
        }
        if !self.sftp_real() {
            return false;
        }
        let Some(s) = self.files.sftp.as_ref() else { return false };
        match s.status() {
            SftpStatus::Connecting => s.epoch().is_some(),
            SftpStatus::Ready => s.dir(&self.files.path).is_none_or(|d| d.error.is_none() && d.loading && d.entries.is_empty()),
            _ => false,
        }
    }

    /// 真实 SFTP 在表格区域的状态:连接中、确认主机指纹、连接失败、加载中、读取出错。
    /// 正常显示文件列表时返回 None。
    fn sftp_state_view(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.sftp_real() {
            return None;
        }
        let card = |ic: IconName, color: Rgba, title: String, sub: String| {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.0))
                .pt(px(48.0))
                .px(px(24.0))
                .child(icon(ic, 26.0, color))
                .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(title))
                .child(div().max_w(px(420.0)).text_center().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(sub))
        };
        let retry = |id: &'static str, cx: &mut Context<Self>, redo_conn: bool| {
            div()
                .id(id)
                .mt(px(8.0))
                .h(px(28.0))
                .px(px(14.0))
                .flex()
                .items_center()
                .rounded(px(t::RADIUS_MD))
                .bg(c::accent_soft())
                .text_size(px(t::TYPE_CONTROL))
                .text_color(c::accent())
                .cursor_pointer()
                .hover(|s| s.bg(c::accent().opacity(0.3)))
                .child("重试")
                .on_click(cx.listener(move |this, _, _, cx| {
                    if redo_conn {
                        this.files.sftp = None;
                        this.files.path_ready = false;
                    } else {
                        let path = this.files.path.clone();
                        if let Some(s) = this.files.sftp.as_ref() {
                            s.list(&path);
                        }
                    }
                    cx.notify();
                }))
        };
        let s = self.files.sftp.as_ref()?;
        match s.status() {
            // 没连上时由整页「连接中」接管(见 files_panel)
            SftpStatus::Connecting | SftpStatus::HostPrompt(_) | SftpStatus::Failed(_) => None,
            SftpStatus::Ready => {
                let d = s.dir(&self.files.path);
                match d {
                    Some(d) if d.error.is_some() => {
                        Some(card(IconName::TriangleAlert, c::warn(), "读取目录失败".into(), d.error.unwrap_or_default()).child(retry("dir-retry", cx, false)).into_any_element())
                    }
                    _ => None,
                }
            }
        }
    }
}

/// 拖拽时跟随鼠标的小卡片:比文件名宽一截的一行(图标 + 名称),和被拖的行区分开,看起来是「拿起了这一项」
pub(crate) struct DragGhost {
    name: String,
    is_dir: bool,
    /// 和被拖的那一行同宽
    w: f32,
}
impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(self.w))
            .h(px(32.0))
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(9.0))
            .rounded(px(t::RADIUS_MD))
            .bg(c::dialog())
            .border_1()
            .border_color(c::accent().opacity(0.6))
            .shadow_lg()
            .child(icon(if self.is_dir { IconName::Folder } else { IconName::File }, 15.0, if self.is_dir { c::accent() } else { c::muted() }))
            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_BODY)).text_color(c::text()).child(self.name.clone()))
    }
}

/// 简单文字 tooltip
pub(crate) struct Tip(pub(crate) String);
impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(t::RADIUS_SM))
            .bg(c::raised_hi())
            .border_1()
            .border_color(c::border_strong())
            .text_size(px(t::TYPE_CAPTION))
            .font_family(MONO)
            .text_color(c::secondary())
            .child(self.0.clone())
    }
}

#[cfg(test)]
mod addr_tests {
    use super::resolve_path as r;
    #[cfg(unix)]
    #[test]
    fn local_path_helpers() {
        assert_eq!(super::lparent("/a/b"), "/a");
        assert_eq!(super::lparent("/a"), "/");
        assert_eq!(super::lparent("/"), "/");
        assert_eq!(super::ljoin("/", "x"), "/x");
        assert_eq!(super::ljoin("/a", "x"), "/a/x");
        assert_eq!(super::local_crumbs("/a/b"), vec![("a".to_string(), "/a".to_string()), ("b".to_string(), "/a/b".to_string())]);
        assert_eq!(super::local_resolve("../c", "/a/b", "/h"), "/a/c");
        assert_eq!(super::local_resolve("~/x", "/a", "/h"), "/h/x");
        assert_eq!(super::local_resolve("/../..", "/a", "/h"), "/");
    }

    #[test]
    fn resolve_path_cases() {
        assert_eq!(r("/etc/nginx", "/root", "/root"), "/etc/nginx");
        assert_eq!(r("~", "/var", "/home/a"), "/home/a");
        assert_eq!(r("~/x/y", "/var", "/home/a"), "/home/a/x/y");
        assert_eq!(r("logs", "/var", "/h"), "/var/logs");
        assert_eq!(r("../etc/", "/var/log", "/h"), "/var/etc");
        assert_eq!(r("/a/./b//c/..", "/", "/h"), "/a/b");
        assert_eq!(r("../../..", "/a", "/h"), "/");
    }
}
