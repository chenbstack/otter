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

use gpui_kit::assets::IconName;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Home, MENU_W, MONO, TITLEBAR_H, c, icon, t};
use crate::mock::{Node, NodeId};

/// 服务器文件:三栏,通常正好盖满内容区
const HOST_W: f32 = 1100.0;
/// 容器文件:两栏,容器里通常只关心挂载卷和少数配置
const CT_W: f32 = 760.0;
const MIN_W: f32 = 480.0;
/// 抽屉宽度 >= 这个才显示左侧"常用位置"
const PLACES_MIN: f32 = 880.0;
/// 抽屉宽度 >= 这个才显示右侧预览
const PREVIEW_MIN: f32 = 700.0;
const PLACES_W: f32 = 168.0;
const PREVIEW_W: f32 = 290.0;

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
    mode: &'static str,
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

fn parent(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].into(),
    }
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
            mode,
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

/// 文本文件的 mock 内容;上传的文件直接读本地源文件。
fn text_for(e: &Entry, target: &Target, path: &str, node: &Node) -> String {
    if let Some(p) = &e.local {
        let bytes = std::fs::read(p).unwrap_or_default();
        return String::from_utf8_lossy(&bytes[..bytes.len().min(8 * 1024)]).into_owned();
    }
    let host_ip = node.user_host.split('@').nth(1).unwrap_or("127.0.0.1").split(':').next().unwrap_or("");
    let me = match target {
        Target::Host => node.name.clone(),
        Target::Container { svc, .. } => svc.clone(),
    };
    let name = e.name.as_str();
    match name {
        "compose.yaml" | "docker-compose.yml" => {
            let stack = path.rsplit('/').next().unwrap_or("");
            let mut s = String::from("services:\n");
            for svc in node.stacks.iter().filter(|s| s.name == stack).flat_map(|s| s.services.iter()) {
                s += &format!("  {0}:\n    image: {1}\n    container_name: {0}\n    restart: unless-stopped\n", svc.name, svc.image);
                if !svc.ports.is_empty() {
                    s += "    ports:\n";
                    for p in &svc.ports {
                        s += &format!("      - \"{}\"\n", p.trim_end_matches("/tcp"));
                    }
                }
                s += &format!("    volumes:\n      - ./{}/config:/config\n      - /data/{stack}:/data\n    env_file: .env\n\n", svc.name);
            }
            s
        }
        ".env" => "TZ=Asia/Shanghai\nPUID=1000\nPGID=1000\nUMASK=022\nDATA_DIR=/data\nLOG_LEVEL=info\n".into(),
        "hosts" => format!(
            "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n{host_ip}\t{me}\n\n# docker\n172.17.0.1\thost.docker.internal\n"
        ),
        "hostname" => format!("{me}\n"),
        "resolv.conf" => "# Generated by systemd-resolved\nnameserver 223.5.5.5\nnameserver 119.29.29.29\noptions edns0 trust-ad\n".into(),
        "os-release" => "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nNAME=\"Debian GNU/Linux\"\nVERSION_ID=\"12\"\nVERSION=\"12 (bookworm)\"\nID=debian\nHOME_URL=\"https://www.debian.org/\"\n".into(),
        "fstab" => "# <file system>  <mount point>  <type>  <options>  <dump> <pass>\nUUID=7c1e-4f2a  /      ext4  errors=remount-ro  0 1\nUUID=19ab-77c0  /data  xfs   defaults,noatime   0 2\n/swapfile       none   swap  sw                 0 0\n".into(),
        "crontab" => "SHELL=/bin/sh\nPATH=/usr/local/sbin:/usr/local/bin:/sbin:/bin:/usr/sbin:/usr/bin\n\n# m h dom mon dow user  command\n17 *  * * *  root  cd / && run-parts --report /etc/cron.hourly\n30 3  * * *  root  /root/backup.sh >> /var/log/backup.log 2>&1\n".into(),
        "passwd" => "root:x:0:0:root:/root:/bin/bash\ndaemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\nabc:x:1000:1000::/config:/bin/false\nnobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n".into(),
        "daemon.json" => "{\n  \"log-driver\": \"json-file\",\n  \"log-opts\": { \"max-size\": \"20m\", \"max-file\": \"3\" },\n  \"data-root\": \"/var/lib/docker\",\n  \"registry-mirrors\": [\"https://mirror.ccs.tencentyun.com\"]\n}\n".into(),
        "settings.json" => format!("{{\n  \"name\": \"{me}\",\n  \"listen\": \"0.0.0.0:8096\",\n  \"logLevel\": \"info\",\n  \"cacheDir\": \"/config/cache\",\n  \"autoUpdate\": false\n}}\n"),
        "config.xml" | "system.xml" => format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<ServerConfiguration>\n  <ServerName>{me}</ServerName>\n  <HttpServerPortNumber>8096</HttpServerPortNumber>\n  <HttpsPortNumber>8920</HttpsPortNumber>\n  <EnableUPnP>false</EnableUPnP>\n  <UICulture>zh-CN</UICulture>\n  <CachePath>/config/cache</CachePath>\n  <LogFileRetentionDays>7</LogFileRetentionDays>\n</ServerConfiguration>\n"),
        "nginx.conf" => "user www-data;\nworker_processes auto;\npid /run/nginx.pid;\n\nevents {\n    worker_connections 1024;\n}\n\nhttp {\n    sendfile on;\n    tcp_nopush on;\n    keepalive_timeout 65;\n    include /etc/nginx/mime.types;\n    access_log /var/log/nginx/access.log;\n    error_log /var/log/nginx/error.log;\n    gzip on;\n    include /etc/nginx/conf.d/*.conf;\n}\n".into(),
        ".bashrc" => "# ~/.bashrc\n[ -z \"$PS1\" ] && return\n\nHISTSIZE=5000\nHISTCONTROL=ignoredups\n\nalias ll='ls -alF'\nalias dc='docker compose'\nalias dps='docker ps --format \"table {{.Names}}\\t{{.Status}}\"'\n\nexport EDITOR=vim\n".into(),
        ".profile" => "# ~/.profile\nif [ -n \"$BASH_VERSION\" ] && [ -f \"$HOME/.bashrc\" ]; then\n    . \"$HOME/.bashrc\"\nfi\nmesg n 2> /dev/null || true\n".into(),
        "backup.sh" => "#!/usr/bin/env bash\nset -euo pipefail\n\nDAY=$(date +%F)\nOUT=/data/backups\n\ndocker exec mysql mysqldump -uroot --all-databases | gzip > \"$OUT/mysql-$DAY.sql.gz\"\ntar czf \"$OUT/stacks-$DAY.tar.gz\" /opt/stacks\n\n# 保留 14 天\nfind \"$OUT\" -mtime +14 -delete\n".into(),
        "entrypoint.sh" => "#!/bin/sh\nset -e\n\nchown -R abc:abc /config\nexec s6-setuidgid abc /app/bin/server \\\n    --datadir /config \\\n    --ffmpeg /app/bin/ffmpeg\n".into(),
        "VERSION" => "4.8.11.0\n".into(),
        "authorized_keys" => "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGq3kP0bX… chen@macbook\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIM8fW2… deploy@ci\n".into(),
        "id_ed25519.pub" => "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH1x9Qe2… root@server\n".into(),
        "known_hosts" => "|1|q2Fz…=|Xk9…= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI…\n|1|Hn3e…=|p0Q…= ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAA…\n".into(),
        "config" => "Host github.com\n    User git\n    IdentityFile ~/.ssh/id_ed25519\n\nHost nas\n    HostName 192.168.1.10\n    User admin\n".into(),
        _ if ext(name) == "md" => format!("# {}\n\n- 每天 03:30 自动备份到 /data/backups\n- 证书由 openresty 自动续期\n- 改 compose 后记得 `dc up -d`\n", name.trim_end_matches(".md")),
        _ if ext(name) == "log" || name == "syslog" => {
            let h = hash(name);
            (0..60)
                .map(|i| {
                    let x = h.wrapping_mul(i + 7) >> 11;
                    let (lv, msg) = match x % 11 {
                        0 => ("WARN ", "slow response from upstream (1.8s)"),
                        1 => ("ERROR", "connection reset by peer"),
                        2 | 3 => ("INFO ", "GET /health 200 2ms"),
                        4 => ("INFO ", "scheduled task finished"),
                        5 => ("DEBUG", "cache hit ratio 0.93"),
                        _ => ("INFO ", "request completed"),
                    };
                    format!("2026-11-11 08:{:02}:{:02} {lv} [{me}] {msg}", i / 2, (x >> 5) % 60)
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => format!("# {name}\n"),
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
    picker: bool,
    search: Option<Entity<InputState>>,
    notice: Option<SharedString>,
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
            picker: false,
            search: None,
            notice: None,
        }
    }

    /// 创建筛选输入框(需要 window 和 cx,所以不放在 `new` 里)
    pub fn init_search(&mut self, window: &mut Window, cx: &mut Context<Home>) {
        if self.search.is_none() {
            let s = cx.new(|cx| InputState::new(window, cx).placeholder("筛选当前目录"));
            cx.observe(&s, |_, _, cx| cx.notify()).detach();
            self.search = Some(s);
        }
    }

    pub fn is_host(&self) -> bool {
        self.target == Target::Host
    }

    /// 截图调试用:直接跳到某目录 / 选中某项
    pub fn debug_jump(&mut self, cd: Option<String>, sel: Option<String>) {
        if let Some(p) = cd {
            self.path = p;
        }
        self.selected = sel;
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
        }
        fs.target = target;
        fs.node = sel;
        fs.open = true;
        fs.picker = false;
        cx.notify();
    }

    pub fn toggle_host_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.files.open && self.files.target == Target::Host {
            self.close_files(cx);
        } else {
            self.open_files(Target::Host, window, cx);
        }
    }

    fn close_files(&mut self, cx: &mut Context<Self>) {
        self.files.open = false;
        self.files.picker = false;
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
        let key = fs.dir_key();
        let mut v = base(&fs.target, &fs.path, self.node());
        if let Some(extra) = self.fstore.extra.get(&key) {
            for e in extra {
                v.retain(|o| o.name != e.name);
                v.push(e.clone());
            }
        }
        v.retain(|e| !self.fstore.deleted.contains(&format!("{key}/{}", e.name)));
        v.sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        v
    }

    fn upload(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
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
        let mut e = B { path: &dir, owner: "root" }.d(&name);
        e.modified = "刚刚".into();
        let key = self.files.dir_key();
        self.fstore.extra.entry(key).or_default().push(e);
        self.files.selected = Some(name);
        cx.notify();
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
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

// ---- 渲染 --------------------------------------------------------------------
fn file_icon(e: &Entry) -> (IconName, Rgba) {
    match e.kind {
        Kind::Dir if e.mount.is_some() => (IconName::HardDrive, c::online()),
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
                (IconName::FileCode, c::online())
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

fn text_btn(id: &'static str, ic: IconName, label: &'static str, accent: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(30.0))
        .px(px(if label.is_empty() { 8.0 } else { 10.0 }))
        .rounded(px(t::RADIUS_MD))
        .text_size(px(t::TYPE_CONTROL))
        .cursor_pointer()
        .when(accent, |d| {
            d.bg(c::online())
                .text_color(c::on_accent())
                .font_weight(FontWeight::SEMIBOLD)
                .hover(|s| s.bg(c::online().opacity(0.88)))
                .child(icon(ic, 13.0, c::on_accent()))
        })
        .when(!accent, |d| {
            d.border_1()
                .border_color(c::border_strong())
                .bg(c::raised())
                .text_color(c::secondary())
                .hover(|s| s.bg(c::raised_hi()).text_color(c::text()))
                .child(icon(ic, 13.0, c::muted()))
        })
        .child(label)
}

fn meta_row(k: &'static str, v: impl Into<SharedString>, mono: bool) -> Div {
    div()
        .flex()
        .flex_row()
        .gap(px(10.0))
        .text_size(px(t::TYPE_CAPTION))
        .child(div().w(px(52.0)).flex_shrink_0().text_color(c::ghost()).child(k))
        .child(div().flex_1().min_w_0().text_color(c::secondary()).when(mono, |d| d.font_family(MONO)).child(v.into()))
}

fn progress_bar(p: f32, w: f32) -> Div {
    div()
        .w(px(w))
        .h(px(4.0))
        .rounded_full()
        .bg(c::idle_soft())
        .child(div().h_full().w(px(w * p.clamp(0.0, 1.0))).rounded_full().bg(c::online()))
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
        let show_places = w >= PLACES_MIN;
        let show_preview = w >= PREVIEW_MIN;

        let node = self.node();
        let fs = &self.files;
        let query = fs.search.as_ref().map(|s| s.read(cx).value().trim().to_lowercase()).unwrap_or_default();
        let all = self.listing();
        let entries: Vec<Entry> = all.iter().filter(|e| query.is_empty() || e.name.to_lowercase().contains(&query)).cloned().collect();
        let selected = fs.selected.as_ref().and_then(|n| all.iter().find(|e| &e.name == n)).cloned();

        // ---- 头部:标题 + 目标切换 + 宽度 / 关闭 ----
        let is_host = fs.target == Target::Host;
        let ct_name = match &fs.target {
            Target::Container { svc, .. } => Some(svc.clone()),
            Target::Host => None,
        };
        let seg = |id: &'static str, ic: IconName, label: String, on: bool| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .h(px(26.0))
                .px(px(10.0))
                .rounded(px(t::RADIUS_SM))
                .text_size(px(t::TYPE_CONTROL))
                .cursor_pointer()
                .when(on, |d| d.bg(c::selected()).text_color(c::text()).font_weight(FontWeight::MEDIUM))
                .when(!on, |d| d.text_color(c::muted()).hover(|s| s.text_color(c::secondary())))
                .child(icon(ic, 13.0, if on { c::online() } else { c::ghost() }))
                .child(label)
        };
        let picker = fs.picker.then(|| {
            let mut list = div()
                .id("ct-picker")
                .absolute()
                .top(px(34.0))
                .left_0()
                .w(px(300.0))
                .max_h(px(380.0))
                .overflow_y_scroll()
                .p(px(6.0))
                .rounded(px(t::RADIUS_LG))
                .border_1()
                .border_color(c::border_strong())
                .bg(c::raised_hi())
                .shadow(vec![BoxShadow {
                    color: hsla(0.0, 0.0, 0.0, 0.5),
                    offset: point(px(0.0), px(10.0)),
                    blur_radius: px(28.0),
                    spread_radius: px(0.0),
                    inset: false,
                }])
                .occlude();
            if node.stacks.is_empty() {
                list = list.child(div().p(px(10.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child("这个节点上没有容器"));
            }
            for (si, st) in node.stacks.iter().enumerate() {
                list = list.child(
                    div().px(px(8.0)).pt(px(if si == 0 { 4.0 } else { 10.0 })).pb(px(4.0)).text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(st.name.clone()),
                );
                for (vi, svc) in st.services.iter().enumerate() {
                    let up = svc.state == crate::mock::Running::Up;
                    let on = ct_name.as_deref() == Some(svc.name.as_str());
                    let target = Target::Container { stack: st.name.clone(), svc: svc.name.clone() };
                    list = list.child(
                        div()
                            .id(("ct-opt", si * 100 + vi))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .h(px(30.0))
                            .px(px(8.0))
                            .rounded(px(t::RADIUS_SM))
                            .cursor_pointer()
                            .when(on, |d| d.bg(c::selected()))
                            .hover(|s| s.bg(c::hover()))
                            .child(div().size(px(6.0)).flex_shrink_0().rounded_full().bg(if up { c::online() } else { c::ghost() }))
                            .child(div().text_size(px(t::TYPE_BODY)).text_color(if on { c::text() } else { c::secondary() }).child(svc.name.clone()))
                            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::ghost()).child(svc.image.clone()))
                            .on_click(cx.listener(move |this, _, window, cx| this.open_files(target.clone(), window, cx))),
                    );
                }
            }
            deferred(list).with_priority(1)
        });
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
                    .bg(c::online_soft())
                    .child(icon(IconName::FolderOpen, 15.0, c::online())),
            )
            .child(div().text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::SEMIBOLD).child("文件"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(2.0))
                    .p(px(2.0))
                    .rounded(px(t::RADIUS_MD))
                    .bg(c::sunken())
                    .border_1()
                    .border_color(c::border())
                    .child(seg("seg-host", IconName::Server, "服务器".into(), is_host).on_click(cx.listener(|this, _, window, cx| {
                        this.open_files(Target::Host, window, cx)
                    })))
                    .child(
                        div()
                            .relative()
                            .child(
                                seg("seg-ct", IconName::Box, ct_name.clone().unwrap_or_else(|| "容器".into()), !is_host)
                                    .child(icon(IconName::ChevronDown, 12.0, c::ghost()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.files.picker = !this.files.picker;
                                        cx.notify();
                                    })),
                            )
                            .children(picker),
                    ),
            )
            .child(div().flex_1())
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
            .px(px(6.0))
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
                    .px(px(6.0))
                    .rounded(px(t::RADIUS_SM))
                    .cursor_pointer()
                    .hover(|s| s.bg(c::hover()))
                    .child(icon(if is_host { IconName::Server } else { IconName::Box }, 12.0, c::muted()))
                    .child(div().text_color(c::muted()).child(ct_name.clone().unwrap_or_else(|| node.name.clone())))
                    .on_click(cx.listener(|this, _, _, cx| this.go("/".into(), cx))),
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
        if let Some((_, host)) = &mount {
            let host = host.clone();
            crumbs = crumbs.child(div().flex_1()).child(
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
                    .bg(c::online_soft())
                    .text_size(px(t::TYPE_MICRO))
                    .text_color(c::online())
                    .child(icon(IconName::HardDrive, 11.0, c::online()))
                    .child("挂载卷")
                    .tooltip(move |_, cx| cx.new(|_| Tip(format!("宿主机路径 {host}"))).into()),
            );
        }
        let search = fs.search.clone().map(|s| {
            div()
                .w(px(if w < 640.0 { 130.0 } else { 180.0 }))
                .flex_shrink_0()
                .h(px(30.0))
                .flex()
                .flex_row()
                .items_center()
                .pl(px(9.0))
                .rounded(px(t::RADIUS_MD))
                .bg(c::sunken())
                .border_1()
                .border_color(c::border())
                .text_size(px(t::TYPE_CONTROL))
                .child(icon(IconName::Search, 13.0, c::ghost()))
                .child(div().flex_1().child(Input::new(&s).appearance(false).with_size(px(12.0 / 0.875))))
        });
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
            .child(tool_btn("f-refresh", IconName::RefreshCw, true).on_click(cx.listener(|this, _, _, cx| {
                this.files.notice = Some("已刷新".into());
                cx.notify();
            })))
            .child(div().w(px(4.0)))
            .child(crumbs)
            .children(search)
            .child(
                text_btn("f-mkdir", IconName::FolderPlus, if w < 640.0 { "" } else { "新建文件夹" }, false)
                    .on_click(cx.listener(|this, _, _, cx| this.new_folder(cx))),
            )
            .child(text_btn("f-upload", IconName::Upload, "上传", true).on_click(cx.listener(|this, _, window, cx| this.pick_upload(window, cx))));

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

        // ---- 左:常用位置 ----
        let places = show_places.then(|| {
            let item = |i: usize, ic: IconName, label: String, sub: Option<String>, to: &str| {
                let active = path == to;
                let to = to.to_string();
                div()
                    .id(("place", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(9.0))
                    .min_h(px(30.0))
                    .py(px(4.0))
                    .px(px(9.0))
                    .rounded(px(t::RADIUS_MD))
                    .cursor_pointer()
                    .when(active, |d| d.bg(c::selected()))
                    .when(!active, |d| d.hover(|s| s.bg(c::hover())))
                    .child(icon(ic, 14.0, if active { c::online() } else { c::ghost() }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().text_size(px(t::TYPE_BODY)).text_color(if active { c::text() } else { c::secondary() }).child(label))
                            .when_some(sub, |d, s| d.child(div().truncate().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::ghost()).child(s))),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.go(to.clone(), cx)))
            };
            let title = |t: &'static str| div().px(px(9.0)).pt(px(10.0)).pb(px(4.0)).text_size(px(t::TYPE_MICRO)).font_weight(FontWeight::SEMIBOLD).text_color(c::ghost()).child(t);
            let mut col = div().w(px(PLACES_W)).flex_shrink_0().flex().flex_col().gap(px(1.0)).px(px(8.0)).border_r_1().border_color(c::border());
            if is_host {
                col = col
                    .child(title("常用位置"))
                    .child(item(0, IconName::House, "主目录".into(), None, "/root"))
                    .child(item(1, IconName::HardDrive, "根目录".into(), None, "/"))
                    .child(item(2, IconName::Layers, "堆栈".into(), Some("/opt/stacks".into()), "/opt/stacks"))
                    .child(item(3, IconName::Database, "数据".into(), Some("/data".into()), "/data"))
                    .child(item(4, IconName::FileCog, "配置".into(), Some("/etc".into()), "/etc"))
                    .child(item(5, IconName::ScrollText, "日志".into(), Some("/var/log".into()), "/var/log"));
            } else {
                col = col
                    .child(title("挂载卷"))
                    .children(fs.target.mounts().into_iter().enumerate().map(|(i, (m, host))| item(10 + i, IconName::HardDrive, m.clone(), Some(host), &m)))
                    .child(title("容器内"))
                    .child(item(20, IconName::Box, "根目录".into(), None, "/"))
                    .child(item(21, IconName::FileCode, "应用".into(), Some("/app".into()), "/app"))
                    .child(item(22, IconName::FileCog, "配置".into(), Some("/etc".into()), "/etc"));
            }
            col
        });

        // ---- 中:文件表格 ----
        let list_w = w - if show_places { PLACES_W } else { 0.0 } - if show_preview { PREVIEW_W } else { 0.0 };
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
        let th = |t: &'static str| div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(t).into_any_element();
        let thead = cols(th("名称"), th("大小"), th("修改时间"), th("权限"), th("所有者"))
            .h(px(30.0))
            .flex_shrink_0()
            .px(px(18.0))
            .border_b_1()
            .border_color(c::border());
        let mut rows = div().id("file-rows").flex_1().min_h_0().overflow_y_scroll().flex().flex_col().px(px(8.0)).py(px(4.0));
        if entries.is_empty() {
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
                    d.child(div().flex_shrink_0().px(px(5.0)).rounded(px(t::RADIUS_XS)).bg(c::online_soft()).text_size(px(t::TYPE_MICRO)).text_color(c::online()).child("挂载"))
                })
                .into_any_element();
            let size_el = match e.progress {
                Some(p) => div().flex().flex_row().items_center().gap(px(6.0)).child(progress_bar(p, 38.0)).child(
                    div().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::online()).child(format!("{:.0}%", p * 100.0)),
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
            rows = rows.child(
                cols(name_el, size_el, mono(e.modified.clone(), c::muted()), mono(e.mode.into(), c::ghost()), mono(e.owner.clone(), c::ghost()))
                    .id(("frow", i))
                    .h(px(32.0))
                    .flex_shrink_0()
                    .px(px(10.0))
                    .rounded(px(t::RADIUS_MD))
                    .cursor_pointer()
                    .when(on, |d| d.bg(c::selected()))
                    .when(!on, |d| d.hover(|s| s.bg(c::hover())))
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
        let table = div().flex_1().min_w_0().flex().flex_col().child(thead).child(rows);

        // ---- 右:预览 / 详情 ----
        let preview = show_preview.then(|| {
            let mut col = div()
                .w(px(PREVIEW_W))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .p(px(16.0))
                .border_l_1()
                .border_color(c::border());
            match &selected {
                None => {
                    let dirs = all.iter().filter(|e| e.is_dir()).count();
                    let files = all.len() - dirs;
                    let total: u64 = all.iter().filter(|e| !e.is_dir()).map(|e| e.size).sum();
                    col = col
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("当前目录"))
                                .child(div().text_size(px(t::TYPE_BODY)).font_family(MONO).text_color(c::text()).child(path.clone())),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(7.0))
                                .child(meta_row("文件夹", dirs.to_string(), true))
                                .child(meta_row("文件", files.to_string(), true))
                                .child(meta_row("合计", fmt_size(total), true))
                                .when_some(mount.clone(), |d, (_, h)| d.child(meta_row("宿主机", h, true))),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap(px(8.0))
                                .py(px(26.0))
                                .px(px(12.0))
                                .rounded(px(t::RADIUS_LG))
                                .border_1()
                                .border_dashed()
                                .border_color(c::border_strong())
                                .child(icon(IconName::CloudUpload, 22.0, c::ghost()))
                                .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child("拖文件到抽屉里上传"))
                                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("支持多个文件和文件夹")),
                        );
                }
                Some(e) => {
                    let (ic, color) = file_icon(e);
                    let kind = match e.kind {
                        Kind::Dir if e.mount.is_some() => "挂载卷".to_string(),
                        Kind::Dir => "文件夹".into(),
                        Kind::Link => "符号链接".into(),
                        Kind::File => fmt_size(e.size),
                    };
                    col = col.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(11.0))
                            .child(
                                div()
                                    .size(px(40.0))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(t::RADIUS_LG))
                                    .bg(color.opacity(0.12))
                                    .child(icon(ic, 19.0, color)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .child(div().truncate().text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::SEMIBOLD).child(e.name.clone()))
                                    .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(kind)),
                            ),
                    );
                    // 预览区
                    let pv = div()
                        .id("preview")
                        .flex_1()
                        .min_h(px(120.0))
                        .rounded(px(t::RADIUS_LG))
                        .border_1()
                        .border_color(c::border())
                        .bg(c::sunken())
                        .overflow_y_scroll();
                    let center = |ic: IconName, t: String| {
                        div()
                            .size_full()
                            .min_h(px(140.0))
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap(px(8.0))
                            .child(icon(ic, 22.0, c::ghost()))
                            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(t))
                    };
                    let pv = if let Some(p) = e.progress {
                        pv.child(
                            center(IconName::CloudUpload, format!("上传中 {:.0}% · {}", p * 100.0, fmt_size(e.size)))
                                .child(progress_bar(p, 160.0)),
                        )
                    } else if e.is_dir() {
                        let sub = base(&fs.target, &join(&path, &e.name), node).len();
                        pv.child(center(IconName::FolderOpen, format!("{sub} 项 · 双击打开")))
                    } else if is_secret(&e.name) {
                        pv.child(center(IconName::LockKeyhole, "私钥文件,不提供预览".into()))
                    } else if is_image(&e.name) {
                        pv.child(center(IconName::FileImage, "图片预览(原型里未接)".into()))
                    } else if is_archive(&e.name) {
                        pv.child(center(IconName::FileArchive, "压缩包 · 可下载后查看".into()))
                    } else if is_text(&e.name) || e.local.is_some() {
                        let text = text_for(e, &fs.target, &path, node);
                        let lines: Vec<&str> = text.lines().take(200).collect();
                        let n_w = 14.0 + lines.len().to_string().len() as f32 * 7.0;
                        pv.child(div().flex().flex_col().py(px(8.0)).font_family(MONO).text_size(px(t::TYPE_CAPTION)).children(lines.iter().enumerate().map(
                            |(i, l)| {
                                let color = if l.trim_start().starts_with('#') || l.trim_start().starts_with("<?") {
                                    c::ghost()
                                } else if l.contains(" ERROR ") {
                                    c::danger()
                                } else if l.contains(" WARN ") {
                                    c::warn()
                                } else {
                                    c::secondary()
                                };
                                div()
                                    .flex()
                                    .flex_row()
                                    .min_h(px(17.0))
                                    .child(div().w(px(n_w)).flex_shrink_0().pr(px(8.0)).flex().justify_end().text_color(c::ghost().opacity(0.7)).child((i + 1).to_string()))
                                    .child(div().flex_1().min_w_0().pr(px(8.0)).whitespace_nowrap().overflow_hidden().text_color(color).child(l.to_string()))
                            },
                        )))
                    } else {
                        pv.child(center(IconName::File, "二进制文件,不支持预览".into()))
                    };
                    col = col
                        .child(pv)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(7.0))
                                .child(meta_row("路径", join(&path, &e.name), true))
                                .child(meta_row("修改", e.modified.clone(), true))
                                .child(meta_row("权限", e.mode, true))
                                .child(meta_row("所有者", e.owner.clone(), true))
                                .when_some(e.mount.clone(), |d, h| d.child(meta_row("宿主机", h, true)))
                                .when_some(e.link.clone(), |d, l| d.child(meta_row("指向", l, true))),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.0))
                                .child(text_btn("pv-dl", IconName::Download, "下载", false))
                                .when(!e.is_dir() && is_text(&e.name) && !is_secret(&e.name), |d| d.child(text_btn("pv-edit", IconName::Pencil, "编辑", false)))
                                .child(text_btn("pv-rename", IconName::TextCursorInput, "重命名", false))
                                .child(div().flex_1())
                                .child(
                                    div()
                                        .id("pv-del")
                                        .size(px(30.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(t::RADIUS_MD))
                                        .cursor_pointer()
                                        .hover(|s| s.bg(c::danger().opacity(0.12)))
                                        .child(icon(IconName::Trash, 14.0, c::danger().opacity(0.85)))
                                        .tooltip(|_, cx| cx.new(|_| Tip("删除".into())).into())
                                        .on_click(cx.listener(|this, _, _, cx| this.delete_selected(cx))),
                                ),
                        );
                }
            }
            col
        });

        // ---- 状态栏 ----
        let uploading: Vec<&Entry> = self.fstore.extra.values().flatten().filter(|e| e.progress.is_some()).collect();
        let up_info = (!uploading.is_empty()).then(|| {
            let total: f32 = uploading.iter().map(|e| e.size as f32).sum();
            let done: f32 = uploading.iter().map(|e| e.size as f32 * e.progress.unwrap_or(0.0)).sum();
            let p = if total > 0.0 { done / total } else { 0.0 };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .text_color(c::online())
                .child(icon(IconName::Upload, 12.0, c::online()))
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
            .when(uploading.is_empty(), |d| d.when_some(fs.notice.clone(), |d, n| d.child(div().text_color(c::muted()).child(n))))
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
                    .border_color(c::online().opacity(0.7))
                    .bg(c::scrim_drop())
                    .child(
                        div()
                            .size(px(54.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(c::online_soft())
                            .child(icon(IconName::CloudUpload, 24.0, c::online())),
                    )
                    .child(div().text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::SEMIBOLD).child("松开即可上传"))
                    .child(div().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(format!("→ {drop_path}"))),
            );

        let picker_open = fs.picker;
        let body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_row()
            .children(places)
            .child(table)
            .children(preview);

        div()
            .id(if tab_mode { "files-tab" } else { "files-drawer" })
            .relative()
            .flex()
            .flex_col()
            .bg(c::bg())
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
            .child(toolbar)
            .children(warn)
            .child(body)
            .child(status)
            .child(overlay)
            .when(picker_open, |d| {
                // 点下拉以外任何地方都收起
                d.child(div().id("picker-backdrop").absolute().inset_0().occlude().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.files.picker = false;
                        cx.notify();
                    }),
                ))
            })
            .into_any_element()
    }
}


/// 简单文字 tooltip
struct Tip(String);
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
