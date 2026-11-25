//! Docker 客户端:经 SSH 在服务器上执行 `docker` 命令,解析输出;服务器上不装任何东西(决策 0001)。
//!
//! 结构照抄 `sftp.rs`:后台 tokio 任务 + 命令通道,界面轮询共享状态。优先复用终端已认证的连接。
//! 输出用 `--format` 的制表符文本,解析是纯函数(见文件前半部分和测试)。
//!
//! 权限:先以当前用户直接跑 `docker`;报没权限就改用 `sudo -n docker`;再不行且节点保存了密码,
//! 用 `sudo -S` 经 stdin 传密码(不进命令行、不写日志)。判定结果缓存在连接里。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use russh::ChannelMsg;
use russh::client::Handle as SshHandle;
use tokio::sync::mpsc;

use crate::logs;
use crate::mock::{Running, SPARK_N, Service, Stack};
pub use crate::sftp::Status;
use crate::terminal::ssh::{Client, Conn, Io, PendingHost, Step, open_authenticated, runtime};

// ---- 数据 --------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Container {
    /// 短 ID(12 位),`ps` 和 `stats` 都用它
    pub id: String,
    pub name: String,
    pub image: String,
    /// `running` / `exited` / `paused` / `restarting` / `created` / `dead`
    pub state: String,
    /// 原样的 `Up 2 hours` / `Exited (3) 5 minutes ago`
    pub status: String,
    pub ports: Vec<String>,
    /// Compose 项目 / 服务(来自标签)
    pub project: Option<String>,
    pub service: Option<String>,
    /// 其他面板(1Panel、宝塔)创建的容器只读,值是面板名
    pub panel: Option<&'static str>,
    /// 挂载的卷名或路径
    pub mounts: Vec<String>,
}

impl Container {
    pub fn running(&self) -> bool {
        self.state == "running"
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub cpu: f32,
    pub mem: u64,
    /// 累计收发字节
    pub net: u64,
    /// 累计块设备读写字节
    pub disk: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Image {
    pub id: String,
    pub repo: String,
    pub tag: String,
    pub size: String,
    pub size_bytes: u64,
    pub created: String,
}

impl Image {
    /// 容器的 `Image` 字段可能写成 `repo`、`repo:tag` 或 ID 前缀
    pub fn matches(&self, image: &str) -> bool {
        image == format!("{}:{}", self.repo, self.tag) || (self.tag == "latest" && image == self.repo) || (!image.is_empty() && self.id.starts_with(image))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Volume {
    pub name: String,
    pub driver: String,
    /// `docker system df -v` 的结果;没取过是 None
    pub size: Option<String>,
}

/// 每个容器最近的采样,画迷你图用
#[derive(Clone, Debug, Default)]
pub struct Series {
    pub cpu: VecDeque<f32>,
    pub mem: VecDeque<f32>,
    /// 网络和磁盘存的是每秒字节数(由累计值相邻两次相减得到)
    pub net: VecDeque<f32>,
    pub disk: VecDeque<f32>,
    last: Option<(Stats, Instant)>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub containers: Vec<Container>,
    pub stats: HashMap<String, Stats>,
    pub series: HashMap<String, Series>,
    pub images: Vec<Image>,
    pub volumes: Vec<Volume>,
    /// 最近一次成功刷新容器列表的时间(毫秒时间戳)
    pub fetched_ms: u64,
}

impl Snapshot {
    pub fn running(&self) -> usize {
        self.containers.iter().filter(|c| c.running()).count()
    }
    /// 有几个容器在用这个镜像
    pub fn image_users(&self, img: &Image) -> usize {
        self.containers.iter().filter(|c| img.matches(&c.image)).count()
    }
    /// 哪些容器挂了这个卷
    pub fn volume_users(&self, v: &Volume) -> Vec<&Container> {
        self.containers.iter().filter(|c| c.mounts.iter().any(|m| m == &v.name)).collect()
    }
    /// 需要关注的容器:非运行状态且不是正常退出(退出码 0),或在反复重启
    pub fn attention(&self) -> Vec<&Container> {
        self.containers.iter().filter(|c| c.state == "restarting" || c.state == "dead" || (c.state == "exited" && !c.status.starts_with("Exited (0)")) || c.status.contains("(unhealthy)")).collect()
    }
}

// ---- 解析(纯函数) -------------------------------------------------------------

pub const PS_FORMAT: &str = r#"{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.State}}\t{{.Status}}\t{{.Ports}}\t{{.Label "com.docker.compose.project"}}\t{{.Label "com.docker.compose.service"}}\t{{.Label "createdBy"}}\t{{.Mounts}}"#;
pub const STATS_FORMAT: &str = r"{{.ID}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.NetIO}}\t{{.BlockIO}}";
pub const IMAGES_FORMAT: &str = r"{{.ID}}\t{{.Repository}}\t{{.Tag}}\t{{.Size}}\t{{.CreatedSince}}";
pub const VOLUMES_FORMAT: &str = r"{{.Name}}\t{{.Driver}}";

fn opt(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty() && s != "<no value>").then(|| s.to_string())
}

/// 把 `0.0.0.0:8080->80/tcp, [::]:8080->80/tcp, 6379/tcp` 整理成不重复的短列表
fn parse_ports(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        // 去掉 IPv4/IPv6 监听地址前缀,v4 和 v6 的重复项合并
        let short = p.rsplit_once(':').map(|(_, r)| r).filter(|r| r.contains("->")).unwrap_or(p);
        if !out.iter().any(|x| x == short) {
            out.push(short.to_string());
        }
    }
    out
}

fn panel_of(created_by: Option<&str>, project: Option<&str>) -> Option<&'static str> {
    match created_by {
        Some(v) if v.eq_ignore_ascii_case("Apps") || v.to_ascii_lowercase().contains("1panel") => Some("1Panel"),
        Some(v) if v.to_ascii_lowercase().contains("bt") || v.contains("宝塔") => Some("宝塔"),
        _ if project.is_some_and(|p| p.starts_with("1panel-")) => Some("1Panel"),
        _ => None,
    }
}

pub fn parse_ps(text: &str) -> Vec<Container> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 5 || f[0].trim().is_empty() {
                return None;
            }
            let get = |i: usize| f.get(i).copied().unwrap_or("");
            let project = opt(get(6));
            let created_by = opt(get(8));
            Some(Container {
                id: f[0].trim().to_string(),
                name: f[1].trim().to_string(),
                image: f[2].trim().to_string(),
                state: f[3].trim().to_lowercase(),
                status: f[4].trim().to_string(),
                ports: parse_ports(get(5)),
                panel: panel_of(created_by.as_deref(), project.as_deref()),
                project,
                service: opt(get(7)),
                mounts: get(9).split(',').map(str::trim).filter(|m| !m.is_empty()).map(String::from).collect(),
            })
        })
        .collect()
}

/// `12.5MiB`、`1.2kB`、`3GB`、`0B` → 字节
pub fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.trim().parse().unwrap_or(0.0);
    let mul: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" => 1e3,
        "kib" => 1024.0,
        "mb" => 1e6,
        "mib" => 1024.0 * 1024.0,
        "gb" => 1e9,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tb" => 1e12,
        "tib" => 1024.0_f64.powi(4),
        _ => 1.0,
    };
    (n * mul).round() as u64
}

/// `1.2kB / 3.4MB` → 两段字节数之和
fn parse_pair_sum(s: &str) -> u64 {
    s.split('/').map(parse_size).sum()
}

pub fn parse_stats(text: &str) -> HashMap<String, Stats> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 5 {
                return None;
            }
            Some((
                f[0].trim().to_string(),
                Stats {
                    cpu: f[1].trim().trim_end_matches('%').parse().unwrap_or(0.0),
                    mem: parse_size(f[2].split('/').next().unwrap_or("")),
                    net: parse_pair_sum(f[3]),
                    disk: parse_pair_sum(f[4]),
                },
            ))
        })
        .collect()
}

pub fn parse_images(text: &str) -> Vec<Image> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 5 || f[0].trim().is_empty() {
                return None;
            }
            Some(Image {
                id: f[0].trim().trim_start_matches("sha256:").chars().take(12).collect(),
                repo: f[1].trim().to_string(),
                tag: f[2].trim().to_string(),
                size: f[3].trim().to_string(),
                size_bytes: parse_size(f[3]),
                created: f[4].trim().to_string(),
            })
        })
        .collect()
}

pub fn parse_volumes(text: &str) -> Vec<Volume> {
    text.lines()
        .filter_map(|line| {
            let (name, driver) = line.split_once('\t')?;
            (!name.trim().is_empty()).then(|| Volume { name: name.trim().to_string(), driver: driver.trim().to_string(), size: None })
        })
        .collect()
}

/// `docker system df -v` 里的 `Local Volumes space usage:` 表:`VOLUME NAME   LINKS   SIZE`
pub fn parse_df_volumes(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut in_table = false;
    for line in text.lines() {
        if line.trim_start().starts_with("VOLUME NAME") {
            in_table = true;
            continue;
        }
        if in_table {
            if line.trim().is_empty() {
                break;
            }
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 3 {
                out.insert(f[0].to_string(), f[f.len() - 1].to_string());
            }
        }
    }
    out
}

// ---- 展示用的格式化 ----------------------------------------------------------

pub fn fmt_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{b:.0} B")
    }
}

pub fn fmt_rate(bps: f32) -> String {
    let b = bps as f64;
    if b >= 1e9 {
        format!("{:.1} GB/s", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB/s", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB/s", b / 1e3)
    } else {
        format!("{b:.0} B/s")
    }
}

/// `Up 2 hours` → `2 小时`;不是运行状态返回「已停止」等
pub fn uptime_text(c: &Container) -> String {
    if !c.running() {
        return match c.state.as_str() {
            "exited" => "已退出".into(),
            "paused" => "已暂停".into(),
            "restarting" => "重启中".into(),
            "created" => "未启动".into(),
            _ => "已停止".into(),
        };
    }
    let s = c.status.trim_start_matches("Up ").trim();
    let s = s.split(" (").next().unwrap_or(s);
    let mut out = s.to_string();
    for (en, zh) in [("Less than a second", "不到 1 秒"), ("About an hour", "约 1 小时"), ("About a minute", "约 1 分钟"), ("seconds", "秒"), ("second", "秒"), ("minutes", "分钟"), ("minute", "分钟"), ("hours", "小时"), ("hour", "小时"), ("days", "天"), ("day", "天"), ("weeks", "周"), ("week", "周"), ("months", "个月"), ("month", "个月"), ("years", "年"), ("year", "年")] {
        out = out.replace(en, zh);
    }
    out.replace("An ", "1 ").replace("a ", "1 ").replace(' ', "").replace("约1", "约 1")
}

/// 采样序列归一到 0..1,左边补 0;取窗口内最大值做分母(至少 `floor`),曲线形状比绝对值重要
fn spark_of(v: &VecDeque<f32>, floor: f32) -> [f32; SPARK_N] {
    let max = v.iter().cloned().fold(floor, f32::max);
    let mut out = [0.0; SPARK_N];
    let skip = v.len().saturating_sub(SPARK_N);
    for (i, x) in v.iter().skip(skip).enumerate() {
        out[SPARK_N - (v.len() - skip) + i] = (x / max).clamp(0.0, 1.0);
    }
    out
}

impl Series {
    fn push(&mut self, s: Stats, at: Instant) {
        let cap = SPARK_N * 2;
        let (net, disk) = match &self.last {
            Some((p, t)) => {
                let dt = at.duration_since(*t).as_secs_f32().max(0.5);
                (s.net.saturating_sub(p.net) as f32 / dt, s.disk.saturating_sub(p.disk) as f32 / dt)
            }
            None => (0.0, 0.0),
        };
        for (q, v) in [(&mut self.cpu, s.cpu), (&mut self.mem, s.mem as f32), (&mut self.net, net), (&mut self.disk, disk)] {
            q.push_back(v);
            while q.len() > cap {
                q.pop_front();
            }
        }
        self.last = Some((s, at));
    }
}

/// 把快照转成堆栈页现有界面用的 `Stack`/`Service`。没有 Compose 标签的容器归到「独立容器」。
/// `expanded`:上一次各堆栈的展开状态,刷新时保留。
pub fn to_stacks(snap: &Snapshot, expanded: &HashMap<String, bool>) -> Vec<Stack> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<Service>> = HashMap::new();
    for c in &snap.containers {
        let key = c.project.clone().unwrap_or_else(|| "独立容器".into());
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        let st = snap.stats.get(&c.id).copied().unwrap_or_default();
        let empty = Series::default();
        let se = snap.series.get(&c.id).unwrap_or(&empty);
        let up = c.running();
        groups.entry(key).or_default().push(Service {
            name: c.name.clone(),
            image: c.image.clone(),
            state: if up { Running::Up } else { Running::Down },
            uptime: uptime_text(c),
            cpu: if up { format!("{:.1}%", st.cpu) } else { "—".into() },
            cpu_v: if up { st.cpu } else { 0.0 },
            mem_mb: st.mem as f32 / 1e6,
            mem: if up { fmt_bytes(st.mem) } else { "—".into() },
            net: if up { fmt_rate(se.net.back().copied().unwrap_or(0.0)) } else { "—".into() },
            disk: if up { fmt_rate(se.disk.back().copied().unwrap_or(0.0)) } else { "—".into() },
            cpu_s: spark_of(&se.cpu, 1.0),
            mem_s: spark_of(&se.mem, 1.0),
            net_s: spark_of(&se.net, 1024.0),
            disk_s: spark_of(&se.disk, 1024.0),
            ports: c.ports.clone(),
        });
    }
    order.sort_by(|a, b| (a == "独立容器").cmp(&(b == "独立容器")).then(a.cmp(b)));
    order
        .into_iter()
        .map(|name| {
            let services = groups.remove(&name).unwrap_or_default();
            let ex = expanded.get(&name).copied().unwrap_or(false);
            Stack { name, services, expanded: ex }
        })
        .collect()
}

// ---- 命令参数校验 --------------------------------------------------------------

/// 容器 ID/名字、镜像名、卷名只接受这些字符,避免被拼进 shell 命令时出事
pub fn safe_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '/' | '@' | '-' | '+'))
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// ---- 运行时:连接、轮询、操作 ----------------------------------------------------

const CANCELLED: &str = "安装已取消";

/// 一键安装 Docker 走到哪一步
#[derive(Clone, Debug, Default, PartialEq)]
pub enum InstallPhase {
    #[default]
    Idle,
    /// 正在探测系统、测速
    Probing,
    /// 探测完,等用户确认
    Ready,
    Running,
    /// 与服务器的连接断了,安装还在服务器上继续;重新检测会重连并接着看
    Lost,
    /// 用户取消了安装
    Cancelled,
    Failed(String),
    Done,
}

#[derive(Clone, Debug, Default)]
pub struct InstallView {
    pub phase: InstallPhase,
    pub probe: Option<crate::docker_install::Probe>,
    pub log: Vec<String>,
    /// 最近一次收到新输出的时间(界面据此提示「很久没动静」)
    pub last_output: Option<std::time::Instant>,
    /// 已经请求取消,等服务器上的进程退出
    pub cancelling: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    /// 服务器上没有 docker 命令
    NoDocker,
    /// daemon 没在运行或连不上
    DaemonDown(String),
    /// 没权限且 sudo 也不行(需要密码但节点没保存)
    NoPermission(String),
    Other(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Start(String),
    Stop(String),
    Restart(String),
    Remove(String),
    Pull(String),
    RemoveImage(String),
    RemoveVolume(String),
    /// 清理所有没被容器用到的镜像(`docker image prune -a`)
    PruneImages,
    /// 清理所有没被容器用到的存储卷(`docker volume prune`)
    PruneVolumes,
    /// 把内容写进 `/etc/docker/daemon.json`(先备份,校验通过才替换)
    SaveDaemon(String),
    /// 同上,写完重启 Docker 让配置生效
    SaveRestartDaemon(String),
    /// 退出某个镜像仓库的登录(`docker logout`),参数是仓库地址
    Logout(String),
    /// 确认把 HTTP 仓库加进 insecure-registries、重启 Docker,然后用暂存的凭据登录
    ConfirmInsecure(String),
    /// 运维页的操作(结束进程、重启服务、保存 crontab……),在服务器上执行一段脚本
    Sys(crate::sysops::Job),
    /// 按 compose 文件重新部署堆栈(`docker compose up -d`)
    /// 新建堆栈:建目录、写 docker-compose.yml,再 `docker compose up -d`
    ComposeCreate { project: String, dir: String, text: String, env: String },
    ComposeUp { project: String, dir: String, files: Vec<String>, envs: Vec<String>, pull: bool },
    /// 在没装 Docker 的服务器上一键安装(只在「没有安装 Docker」页出现):安装源,以及装完是否配置 Docker Hub 镜像加速
    InstallDocker { source: crate::docker_install::Source, accel: bool },
}

impl Action {
    fn args(&self) -> Vec<String> {
        let v = |a: &[&str], x: &str| a.iter().map(|s| s.to_string()).chain([x.to_string()]).collect();
        match self {
            Action::Start(n) => v(&["start"], n),
            Action::Stop(n) => v(&["stop"], n),
            Action::Restart(n) => v(&["restart"], n),
            Action::Remove(n) => v(&["rm", "-f"], n),
            Action::Pull(n) => v(&["pull"], n),
            Action::RemoveImage(n) => v(&["rmi"], n),
            Action::RemoveVolume(n) => v(&["volume", "rm"], n),
            Action::PruneImages => ["image", "prune", "-a", "-f"].iter().map(|s| s.to_string()).collect(),
            Action::PruneVolumes => ["volume", "prune", "-f"].iter().map(|s| s.to_string()).collect(),
            // 走 `run_daemon`,不经过 docker 命令
            Action::SaveDaemon(_) | Action::SaveRestartDaemon(_) => Vec::new(),
            Action::Logout(h) => v(&["logout"], h),
            Action::ConfirmInsecure(_) | Action::Sys(_) | Action::ComposeCreate { .. } | Action::InstallDocker { .. } => Vec::new(),
            Action::ComposeUp { project, dir, files, envs, pull } => {
                let mut v: Vec<String> = ["compose", "-p", project, "--project-directory", dir].map(String::from).to_vec();
                for f in files {
                    v.extend(["-f".to_string(), f.clone()]);
                }
                for e in envs {
                    v.extend(["--env-file".to_string(), e.clone()]);
                }
                v.extend(["up", "-d"].map(String::from));
                if *pull {
                    v.extend(["--pull", "always"].map(String::from));
                }
                v
            }
        }
    }
    pub fn target(&self) -> &str {
        match self {
            Action::Start(n) | Action::Stop(n) | Action::Restart(n) | Action::Remove(n) | Action::Pull(n) | Action::RemoveImage(n) | Action::RemoveVolume(n) | Action::Logout(n) | Action::ConfirmInsecure(n) => n,
            Action::Sys(j) => &j.target,
            Action::ComposeUp { project, .. } | Action::ComposeCreate { project, .. } => project,
            Action::PruneImages => "未使用的镜像",
            Action::PruneVolumes => "未使用的存储卷",
            Action::SaveDaemon(_) | Action::SaveRestartDaemon(_) => "daemon.json",
            Action::InstallDocker { .. } => "docker",
        }
    }
    pub(crate) fn verb(&self) -> &'static str {
        match self {
            Action::Start(_) => "启动",
            Action::Stop(_) => "停止",
            Action::Restart(_) => "重启",
            Action::Remove(_) => "删除",
            Action::Pull(_) => "拉取",
            Action::RemoveImage(_) => "删除镜像",
            Action::RemoveVolume(_) => "删除存储卷",
            Action::PruneImages => "清理镜像",
            Action::PruneVolumes => "清理存储卷",
            Action::SaveDaemon(_) => "保存配置",
            Action::SaveRestartDaemon(_) => "保存并重启 Docker",
            Action::Logout(_) => "退出登录",
            Action::ConfirmInsecure(_) => "启用 HTTP 仓库",
            Action::Sys(_) => "执行",
            Action::ComposeUp { .. } => "应用",
            Action::ComposeCreate { .. } => "创建",
            Action::InstallDocker { .. } => "安装",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Sudo {
    No,
    /// `sudo -n`
    Passwordless,
    /// `sudo -S`,密码走 stdin
    WithPassword,
}

enum Cmd {
    Refresh,
    /// 「没有安装 Docker」页:探测系统并测速,结果用 `install_view` 取
    InstallProbe,
    /// 取消服务器上进行中的安装
    InstallCancel,
    Act(Action),
    /// 读服务器上的 daemon.json
    LoadDaemon,
    /// 读已登录的镜像仓库
    LoadRegistries,
    /// 登录镜像仓库;密码只走 stdin,不进命令行和日志
    Login { registry: String, user: String, pass: zeroize::Zeroizing<String> },
    /// 登录 HTTP 仓库:daemon.json 里没有它时先向用户确认
    LoginHttp { registry: String, user: String, pass: zeroize::Zeroizing<String> },
    /// 运维页读一份数据(进程、服务……),结果用 `sys_take` 取
    Sys { kind: crate::sysops::Kind, script: String, root: bool },
    /// 容器抽屉读一份数据(日志、目录列表、文件内容),结果用 `ct_take` 取;`tag` 原样带回,界面靠它认出这是哪次请求
    Ct { kind: crate::ctfiles::CtKind, tag: String, args: Vec<String>, merge: bool },
    /// 数据库页:在数据库容器里 `docker exec` 一段脚本,SQL 等从 stdin 传入(密码只放在这里,不进命令行);结果用 `db_take` 按 `tag` 取
    Db { tag: String, args: Vec<String>, input: Option<zeroize::Zeroizing<String>> },
    /// 在服务器上跑一段无需 root 的只读脚本(如查镜像 tag),结果存进 `Shared.db[tag]`
    Shell { tag: String, script: String },
    /// 数据库备份/恢复:在服务器上以 root(docker 要 sudo 时)或登录用户跑一段脚本,长任务由脚本自己脱离运行,这里只等它「启动」;结果存进 `Shared.db[tag]`
    HostSh { tag: String, script: String, secs: u64 },
    /// 把服务器上的一个文件下载到本地(整个读进内存,上限 `FETCH_MAX`);结果是本地路径
    Fetch { tag: String, remote: String, local: std::path::PathBuf },
    /// 容器里的文件操作:界面是 SFTP 那套文件管理,命令在这里换成 docker exec / docker cp
    CtFs { name: String, sh: Arc<crate::sftp::Shared>, cmd: crate::sftp::Cmd },
}

struct Shared {
    status: Mutex<Status>,
    host_prompt: PendingHost,
    steps: Arc<Mutex<Vec<Step>>>,
    snapshot: Mutex<Option<Arc<Snapshot>>>,
    problem: Mutex<Option<Problem>>,
    /// 服务器资源监控的最新读数(`monitor.rs`)和不能监控的原因
    monitor: Mutex<Option<Arc<crate::monitor::MonSnapshot>>>,
    monitor_err: Mutex<Option<String>>,
    /// 正在进行的操作,键是 `动作:目标`
    busy: Mutex<HashSet<String>>,
    notice: Mutex<Option<String>>,
    /// 读到的 daemon.json:(文件是否存在, 内容) 或失败原因;界面取走后清空
    daemon: Mutex<Option<Result<(bool, String), String>>>,
    /// 读到的已登录仓库;界面取走后清空
    registries: Mutex<Option<Result<Vec<RegAuth>, String>>>,
    /// 运维页读到的原始输出,按种类存放;界面取走后清空
    sys: Mutex<HashMap<crate::sysops::Kind, Result<String, String>>>,
    /// 容器抽屉读到的内容:(请求的标签, 输出),按种类存放;界面取走后清空
    ct: Mutex<HashMap<crate::ctfiles::CtKind, (String, Result<String, String>)>>,
    /// 数据库页执行的结果,键是请求的 `tag`;界面取走后清空
    db: Mutex<HashMap<String, Result<String, String>>>,
    /// 正在下载的文件进度:tag → (已下载, 总大小)
    fetch: Mutex<HashMap<String, (u64, u64)>>,
    /// 等用户确认的 HTTP 仓库登录:(仓库, 用户名, 密码),确认或取消后清掉
    pending_login: Mutex<Option<(String, String, zeroize::Zeroizing<String>)>>,
    /// 需要向用户确认的 HTTP 仓库地址;界面取走后清空
    insecure_ask: Mutex<Option<String>>,
    /// 一键安装 Docker 的状态和日志
    install: Mutex<InstallView>,
    /// 已经有一个循环在盯着服务器上的安装(避免重复)
    install_following: AtomicBool,
    /// 界面每次需要重绘时递增
    version: AtomicU64,
    /// 界面最后一次表示「我还在看」的时间(毫秒时间戳);太久没人看就暂停轮询
    touched: AtomicU64,
    want_volume_sizes: AtomicBool,
    closed: AtomicBool,
    /// 探测出的 docker 调用方式(容器终端照它拼命令);探测完成前是 None
    sudo: Mutex<Option<Sudo>>,
}

impl Shared {
    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// 界面持有的一端;丢弃它会停止轮询并断开连接(复用终端连接时不影响终端)。
pub struct Docker {
    cmd: mpsc::UnboundedSender<Cmd>,
    sh: Arc<Shared>,
    abort: tokio::task::AbortHandle,
    /// 复用终端连接时,对应的终端连接代数;终端重连后代数变了,这个连接就作废
    epoch: Option<u32>,
}

impl Drop for Docker {
    fn drop(&mut self) {
        self.sh.closed.store(true, Ordering::Relaxed);
        self.abort.abort();
    }
}

const POLL_PS: Duration = Duration::from_secs(5);
const POLL_STATS: Duration = Duration::from_secs(10);
const POLL_DF: Duration = Duration::from_secs(30);
const WATCH_TTL_MS: u64 = 4000;

impl Docker {
    pub fn connect(conn: Conn, shared: Option<(Arc<SshHandle<Client>>, u32)>) -> Docker {
        let epoch = shared.as_ref().map(|s| s.1);
        let shared = shared.map(|s| s.0);
        let (tx, rx) = mpsc::unbounded_channel();
        let sh = Arc::new(Shared {
            status: Mutex::new(Status::Connecting),
            host_prompt: Arc::new(Mutex::new(None)),
            steps: Arc::new(Mutex::new(Vec::new())),
            snapshot: Mutex::new(None),
            problem: Mutex::new(None),
            monitor: Mutex::new(None),
            monitor_err: Mutex::new(None),
            busy: Mutex::new(HashSet::new()),
            install: Mutex::new(InstallView::default()),
            install_following: AtomicBool::new(false),
            notice: Mutex::new(None),
            daemon: Mutex::new(None),
            registries: Mutex::new(None),
            sys: Mutex::new(HashMap::new()),
            ct: Mutex::new(HashMap::new()),
            db: Mutex::new(HashMap::new()),
            fetch: Mutex::new(HashMap::new()),
            pending_login: Mutex::new(None),
            insecure_ask: Mutex::new(None),
            version: AtomicU64::new(1),
            touched: AtomicU64::new(now_ms()),
            want_volume_sizes: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            sudo: Mutex::new(None),
        });
        let sh2 = sh.clone();
        let join = runtime().spawn(async move {
            let label = format!("{}@{}:{}", conn.user, conn.host, conn.port);
            let io = Io::headless(sh2.host_prompt.clone(), sh2.steps.clone());
            if let Err(e) = session_task(&conn, &label, &io, sh2.clone(), rx, shared).await {
                logs::push(logs::Level::Error, &label, format!("Docker:{e}"));
                if io.steps_empty() {
                    io.begin_step("连接服务器");
                }
                io.fail_step(&e);
                *sh2.status.lock().unwrap() = Status::Failed(e);
                sh2.bump();
            }
        });
        Docker { cmd: tx, sh, abort: join.abort_handle(), epoch }
    }

    pub fn epoch(&self) -> Option<u32> {
        self.epoch
    }

    /// 这个容器里的文件管理:用 SFTP 同一套界面状态,命令通过本连接的 docker exec / cp 执行
    pub fn container_fs(&self, name: &str) -> crate::sftp::Sftp {
        let (tx, name) = (self.cmd.clone(), name.to_string());
        crate::sftp::Sftp::via(self.epoch, move |cmd, sh| {
            let _ = tx.send(Cmd::CtFs { name: name.clone(), sh, cmd });
        })
    }

    /// 在容器里开交互 shell 的终端连接:`docker exec -it`(必要时 sudo),照 Docker 页探测出的方式拼。
    /// `conn` 是节点的连接信息(新开一条 SSH,不占 Docker 页的连接);Docker 还没探测完或容器名不合法时返回 None。
    pub fn exec_conn(&self, mut conn: Conn, name: &str) -> Option<Conn> {
        if !safe_name(name) {
            return None;
        }
        let sudo = (*self.sh.sudo.lock().unwrap())?;
        let q = shell_quote(name);
        // 优先 bash,没有再 sh
        let run = |d: &str| format!("{d} exec -it {q} sh -c 'command -v bash >/dev/null 2>&1 && exec bash || exec sh'");
        let (command, stdin_first) = match sudo {
            Sudo::No => (run("docker"), None),
            Sudo::Passwordless => (run("sudo -n docker"), None),
            Sudo::WithPassword => (format!("stty -echo; {}", run("sudo -S -p '' docker")), conn.auth.password.clone()),
        };
        conn.exec = Some(crate::terminal::ssh::Exec { command, stdin_first });
        Some(conn)
    }

    pub fn status(&self) -> Status {
        if let Some(p) = self.sh.host_prompt.lock().ok().and_then(|g| g.as_ref().map(|(p, _)| p.clone())) {
            return Status::HostPrompt(p);
        }
        self.sh.status.lock().map(|s| s.clone()).unwrap_or(Status::Failed("内部错误".into()))
    }

    pub fn steps(&self) -> Vec<Step> {
        self.sh.steps.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn answer_host(&self, yes: bool) {
        if let Ok(mut g) = self.sh.host_prompt.lock() {
            if let Some(tx) = g.as_mut().and_then(|(_, tx)| tx.take()) {
                let _ = tx.send(yes);
            }
        }
    }

    /// 最近一次刷新的数据;还没取到过是 None
    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.sh.snapshot.lock().ok().and_then(|s| s.clone())
    }

    /// 服务器资源监控的最新读数;还没有(采样需要两次才能算出速率)是 None
    pub fn monitor(&self) -> Option<Arc<crate::monitor::MonSnapshot>> {
        self.sh.monitor.lock().ok().and_then(|s| s.clone())
    }

    /// 不能监控的原因(不是 Linux 等)
    pub fn monitor_error(&self) -> Option<String> {
        self.sh.monitor_err.lock().ok().and_then(|s| s.clone())
    }

    pub fn problem(&self) -> Option<Problem> {
        self.sh.problem.lock().ok().and_then(|p| p.clone())
    }

    /// 界面重绘用的版本号:变了就说明有新数据、新状态或新提示
    pub fn version(&self) -> u64 {
        self.sh.version.load(Ordering::Relaxed)
    }

    /// 界面在看这个页面;太久没调用会暂停轮询
    pub fn touch(&self) {
        self.sh.touched.store(now_ms(), Ordering::Relaxed);
    }

    /// 存储卷页在前台时才取体积(`docker system df -v` 比较慢)
    pub fn want_volume_sizes(&self, on: bool) {
        self.sh.want_volume_sizes.store(on, Ordering::Relaxed);
    }

    pub fn refresh(&self) {
        let _ = self.cmd.send(Cmd::Refresh);
    }

    /// 请求读取 daemon.json;结果用 `take_daemon` 取
    pub fn load_daemon(&self) {
        let _ = self.cmd.send(Cmd::LoadDaemon);
    }

    pub fn take_daemon(&self) -> Option<Result<(bool, String), String>> {
        self.sh.daemon.lock().ok().and_then(|mut d| d.take())
    }

    /// 请求读容器抽屉的数据(日志、目录、文件);结果用 `ct_take` 取
    pub fn ct_load(&self, kind: crate::ctfiles::CtKind, tag: &str, args: Vec<String>, merge: bool) {
        let _ = self.cmd.send(Cmd::Ct { kind, tag: tag.to_string(), args, merge });
    }

    pub fn ct_take(&self, kind: crate::ctfiles::CtKind) -> Option<(String, Result<String, String>)> {
        self.sh.ct.lock().ok().and_then(|mut m| m.remove(&kind))
    }

    /// 请求读一份运维数据;结果用 `sys_take` 取
    pub fn sys_load(&self, kind: crate::sysops::Kind, script: &str, root: bool) {
        let _ = self.cmd.send(Cmd::Sys { kind, script: script.to_string(), root });
    }

    /// 在数据库容器里执行(`args` 是 `docker` 的参数,如 `exec -i <容器> sh -c <脚本>`),`input` 走 stdin;结果用 `db_take(tag)` 取
    pub fn db_run(&self, tag: &str, args: Vec<String>, input: Option<String>) {
        let _ = self.cmd.send(Cmd::Db { tag: tag.to_string(), args, input: input.map(zeroize::Zeroizing::new) });
    }

    /// 在服务器上跑一段脚本(普通用户),结果用 `db_take(tag)` 取
    pub fn shell_run(&self, tag: &str, script: &str) {
        let _ = self.cmd.send(Cmd::Shell { tag: tag.to_string(), script: script.to_string() });
    }

    /// 备份/恢复脚本(见 `Cmd::HostSh`)
    pub fn host_run(&self, tag: &str, script: String, secs: u64) {
        let _ = self.cmd.send(Cmd::HostSh { tag: tag.to_string(), script, secs });
    }

    /// 下载进度 (已下载, 总大小);没有在下载返回 None
    pub fn fetch_progress(&self, tag: &str) -> Option<(u64, u64)> {
        self.sh.fetch.lock().ok().and_then(|m| m.get(tag).copied())
    }

    /// 下载服务器上的文件到本地路径 `local`
    pub fn fetch(&self, tag: &str, remote: &str, local: std::path::PathBuf) {
        let _ = self.cmd.send(Cmd::Fetch { tag: tag.to_string(), remote: remote.to_string(), local });
    }

    pub fn db_take(&self, tag: &str) -> Option<Result<String, String>> {
        self.sh.db.lock().ok().and_then(|mut m| m.remove(tag))
    }

    pub fn sys_take(&self, kind: crate::sysops::Kind) -> Option<Result<String, String>> {
        self.sh.sys.lock().ok().and_then(|mut m| m.remove(&kind))
    }

    /// 请求读取已登录的仓库;结果用 `take_registries` 取
    pub fn load_registries(&self) {
        let _ = self.cmd.send(Cmd::LoadRegistries);
    }

    pub fn take_registries(&self) -> Option<Result<Vec<RegAuth>, String>> {
        self.sh.registries.lock().ok().and_then(|mut d| d.take())
    }

    /// 登录镜像仓库(`registry` 留空是 Docker Hub)
    pub fn login(&self, registry: String, user: String, pass: String) {
        let _ = self.cmd.send(Cmd::Login { registry, user, pass: zeroize::Zeroizing::new(pass) });
    }

    /// 登录 HTTP 仓库。daemon.json 里还没有它时不会直接登录,而是让 `take_insecure_ask` 返回它,等用户确认
    pub fn login_http(&self, registry: String, user: String, pass: String) {
        let _ = self.cmd.send(Cmd::LoginHttp { registry, user, pass: zeroize::Zeroizing::new(pass) });
    }

    pub fn take_insecure_ask(&self) -> Option<String> {
        self.sh.insecure_ask.lock().ok().and_then(|mut d| d.take())
    }

    /// 用户取消了 HTTP 仓库的确认:丢掉暂存的凭据
    pub fn drop_pending_login(&self) {
        if let Ok(mut p) = self.sh.pending_login.lock() {
            *p = None;
        }
    }

    pub fn act(&self, a: Action) {
        let _ = self.cmd.send(Cmd::Act(a));
    }

    /// 这个目标上是否有操作在进行
    pub fn is_busy(&self, target: &str) -> bool {
        self.sh.busy.lock().map(|b| b.iter().any(|k| k.ends_with(&format!(":{target}")))).unwrap_or(false)
    }

    /// 探测系统并测速(只在还没探测过时发出);之后用 `install_view` 看进度
    pub fn install_probe(&self) {
        {
            let mut v = self.sh.install.lock().unwrap();
            if v.phase != InstallPhase::Idle {
                return;
            }
            v.phase = InstallPhase::Probing;
        }
        let _ = self.cmd.send(Cmd::InstallProbe);
        self.sh.bump();
    }

    /// 取消进行中的安装(结束服务器上的安装进程)
    pub fn install_cancel(&self) {
        {
            let mut v = self.sh.install.lock().unwrap();
            if v.phase != InstallPhase::Running || v.cancelling {
                return;
            }
            v.cancelling = true;
            v.log.push("==> 正在取消安装…".into());
        }
        let _ = self.cmd.send(Cmd::InstallCancel);
        self.sh.bump();
    }

    pub fn install_view(&self) -> InstallView {
        self.sh.install.lock().unwrap().clone()
    }

    /// 这个目标上正在进行的操作的动词(没有则 None)
    pub fn busy_label(&self, target: &str) -> Option<String> {
        let suffix = format!(":{target}");
        self.sh.busy.lock().ok().and_then(|b| b.iter().find_map(|k| k.strip_suffix(&suffix).map(str::to_string)))
    }

    /// 是否有任何目标在执行指定动词的操作(拉取、清理……)
    pub fn any_busy_verb(&self, verb: &str) -> bool {
        let prefix = format!("{verb}:");
        self.sh.busy.lock().map(|b| b.iter().any(|k| k.starts_with(&prefix))).unwrap_or(false)
    }

    /// 这个目标上是否正在执行指定动词的操作(「停止」「重启」……)
    pub fn is_busy_verb(&self, verb: &str, target: &str) -> bool {
        self.sh.busy.lock().map(|b| b.contains(&format!("{verb}:{target}"))).unwrap_or(false)
    }

    pub fn take_notice(&self) -> Option<String> {
        self.sh.notice.lock().ok().and_then(|mut n| n.take())
    }
}

/// 一次 exec 的结果
pub(crate) struct Output {
    pub(crate) code: u32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// 字节形式的 exec 结果(`docker cp` 的 tar 流是二进制)
pub(crate) struct RawOutput {
    pub(crate) code: u32,
    pub(crate) out: Vec<u8>,
    pub(crate) err: Vec<u8>,
}

pub(crate) async fn exec(session: &SshHandle<Client>, cmd: &str, stdin: Option<&[u8]>, timeout: Duration) -> Result<Output, String> {
    let r = exec_raw(session, cmd, stdin, timeout).await?;
    Ok(Output { code: r.code, stdout: String::from_utf8_lossy(&r.out).into_owned(), stderr: String::from_utf8_lossy(&r.err).into_owned() })
}

pub(crate) async fn exec_raw(session: &SshHandle<Client>, cmd: &str, stdin: Option<&[u8]>, timeout: Duration) -> Result<RawOutput, String> {
    let fut = async {
        let mut ch = session.channel_open_session().await.map_err(|e| format!("打开通道失败:{e}"))?;
        ch.exec(true, cmd).await.map_err(|e| format!("执行失败:{e}"))?;
        if let Some(data) = stdin {
            ch.data(data).await.map_err(|e| format!("写入失败:{e}"))?;
            let _ = ch.eof().await;
        }
        let (mut out, mut err, mut code) = (Vec::new(), Vec::new(), None);
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, .. } => err.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok::<_, String>(RawOutput { code: code.unwrap_or(255), out, err })
    };
    tokio::time::timeout(timeout, fut).await.map_err(|_| format!("命令超时({} 秒)", timeout.as_secs()))?
}

struct Runner {
    session: Arc<SshHandle<Client>>,
    sudo: Sudo,
    /// 仅 `Sudo::WithPassword` 用,用完随 Runner 释放
    password: Option<zeroize::Zeroizing<String>>,
}

impl Runner {
    async fn docker(&self, args: &[String], timeout: Duration) -> Result<Output, String> {
        self.docker_in(args, None, timeout).await
    }

    /// 跑 docker 命令,`input` 通过 stdin 交给它(`docker login --password-stdin` 用)。走 sudo 且要密码时,sudo 先读走第一行密码,剩下的才是命令的输入。
    async fn docker_in(&self, args: &[String], input: Option<&str>, timeout: Duration) -> Result<Output, String> {
        let quoted = args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
        match self.sudo {
            Sudo::No => exec(&self.session, &format!("docker {quoted}"), input.map(str::as_bytes), timeout).await,
            Sudo::Passwordless => exec(&self.session, &format!("sudo -n docker {quoted}"), input.map(str::as_bytes), timeout).await,
            Sudo::WithPassword => {
                let pw = self.password.as_deref().map(String::as_str).unwrap_or("");
                let data = zeroize::Zeroizing::new(format!("{pw}\n{}", input.unwrap_or("")));
                exec(&self.session, &format!("sudo -k; sudo -S -p '' docker {quoted}"), Some(data.as_bytes()), timeout).await
            }
        }
    }
}

impl Runner {
    /// 跑 docker 命令并拿字节形式的输出;`merge` 把 stderr 并进 stdout(`docker logs` 要按时间顺序看两路输出)。`input` 通过 stdin 交给命令。
    async fn docker_raw(&self, args: &[String], input: Option<&[u8]>, merge: bool, timeout: Duration) -> Result<RawOutput, String> {
        let quoted = args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
        let tail = if merge { " 2>&1" } else { "" };
        match self.sudo {
            Sudo::No => exec_raw(&self.session, &format!("docker {quoted}{tail}"), input, timeout).await,
            Sudo::Passwordless => exec_raw(&self.session, &format!("sudo -n docker {quoted}{tail}"), input, timeout).await,
            Sudo::WithPassword => {
                let pw = self.password.as_deref().map(String::as_str).unwrap_or("");
                let mut data = zeroize::Zeroizing::new(format!("{pw}\n").into_bytes());
                data.extend_from_slice(input.unwrap_or(&[]));
                exec_raw(&self.session, &format!("sudo -k; sudo -S -p '' docker {quoted}{tail}"), Some(&data), timeout).await
            }
        }
    }

    /// 以 root 身份跑一段 shell:已经是 root 就直接跑,否则走 sudo(免密,或用节点保存的密码)。`input` 通过 stdin 传给脚本。
    async fn root(&self, script: &str, input: Option<&str>, timeout: Duration) -> Result<Output, String> {
        let q = shell_quote(script);
        match self.sudo {
            Sudo::WithPassword => {
                let pw = self.password.as_deref().map(String::as_str).unwrap_or("");
                let data = format!("{pw}\n{}", input.unwrap_or(""));
                exec(&self.session, &format!("sudo -k; sudo -S -p '' sh -c {q}"), Some(data.as_bytes()), timeout).await
            }
            _ => exec(&self.session, &format!("if [ \"$(id -u)\" = 0 ]; then sh -c {q}; else sudo -n sh -c {q}; fi"), input.map(str::as_bytes), timeout).await,
        }
    }
}

/// 探测:系统、架构、权限、各安装源的速度。只读,不需要 root。
async fn run_install_probe(r: &Runner, sh: &Shared, has_password: bool) {
    let cmd = format!("sh -c {}", shell_quote(crate::docker_install::PROBE_SCRIPT));
    let res = exec(&r.session, &cmd, None, Duration::from_secs(40)).await;
    let mut attach = false;
    {
        let mut v = sh.install.lock().unwrap();
        match res {
            Ok(o) => {
                let probe = crate::docker_install::parse_probe(&o.stdout, has_password);
                attach = probe.inst_running;
                v.probe = Some(probe);
                v.phase = InstallPhase::Ready;
            }
            Err(e) => v.phase = InstallPhase::Failed(format!("探测失败:{e}")),
        }
    }
    sh.bump();
    // 服务器上有一次安装还在跑(上次连接断了、Otter 重开过):接着看
    if attach {
        install_follow(r, sh, true).await;
    }
}

/// 安装 Docker:把安装脚本交给服务器,脱离 SSH 连接在后台跑,再轮询日志。
/// 断网、休眠、关掉 Otter 都不会中断安装;重新连上后会接着看。
async fn run_install(r: &Runner, sh: &Shared, src: crate::docker_install::Source, accel: bool) {
    {
        let mut v = sh.install.lock().unwrap();
        v.phase = InstallPhase::Running;
        v.log.clear();
        v.cancelling = false;
        v.last_output = Some(std::time::Instant::now());
    }
    sh.bump();
    logs::push(logs::Level::Info, "Docker", format!("开始安装 Docker(源:{})", src.label()));
    // 选中的源排第一,其余能连通的按速度排后面:一个源失败(镜像同步不全等)脚本会自动换下一个
    let order = sh.install.lock().unwrap().probe.as_ref().map_or_else(|| vec![src], |p| p.ranked(src));
    let script = crate::docker_install::install_script(&order);
    let fail = |e: String| {
        sh.install.lock().unwrap().phase = InstallPhase::Failed(e);
        sh.bump();
    };
    match r.root(&crate::docker_install::start_script(accel), Some(&script), Duration::from_secs(60)).await {
        Ok(o) if o.code == 0 => {}
        Ok(o) => {
            let text = format!("{}{}", o.stdout, o.stderr);
            let hint = crate::docker_install::explain(&text).map(|h| format!(",{h}")).unwrap_or_default();
            return fail(format!("没能在服务器上启动安装{hint}:{}", text.trim().lines().last().unwrap_or("")));
        }
        Err(e) => return fail(format!("没能在服务器上启动安装:{e}")),
    }
    install_follow(r, sh, false).await;
}

/// 盯着服务器上的安装:每 2 秒取一次新日志,直到结束或连接断掉。`attach` 表示是重连后接上的(日志从头读)。
async fn install_follow(r: &Runner, sh: &Shared, attach: bool) {
    use crate::docker_install::{explain, parse_poll, poll_script};
    if sh.install_following.swap(true, Ordering::SeqCst) {
        return;
    }
    if attach {
        let mut v = sh.install.lock().unwrap();
        v.phase = InstallPhase::Running;
        v.log.clear();
        v.cancelling = false;
        v.last_output = Some(std::time::Instant::now());
        v.log.push("==> 接上了服务器上正在进行的安装".into());
    }
    let key = "安装:docker".to_string();
    sh.busy.lock().unwrap().insert(key.clone());
    sh.bump();

    let mut offset = 0usize;
    let mut pending: Vec<u8> = Vec::new();
    let mut fails = 0;
    let outcome: Option<Result<(), String>> = loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if sh.closed.load(Ordering::Relaxed) {
            break None;
        }
        let cmd = format!("sh -c {}", shell_quote(&poll_script(offset)));
        let raw = match exec_raw(&r.session, &cmd, None, Duration::from_secs(20)).await {
            Ok(o) => {
                fails = 0;
                o
            }
            Err(_) => {
                fails += 1;
                if fails >= 3 {
                    break None;
                }
                continue;
            }
        };
        let poll = parse_poll(&raw.out);
        if !poll.log.is_empty() {
            offset += poll.log.len();
            pending.extend_from_slice(&poll.log);
            let mut v = sh.install.lock().unwrap();
            v.last_output = Some(std::time::Instant::now());
            while let Some(i) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending.drain(..=i).collect();
                let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                // 进度条用 \r 原地刷新,只留最后一段
                v.log.push(text.rsplit('\r').next().unwrap_or("").to_string());
            }
            if v.log.len() > 2000 {
                v.log.drain(..500);
            }
            drop(v);
            sh.bump();
        }
        let ended = poll.status.is_some() || !poll.alive;
        if !ended {
            continue;
        }
        let (cancelled, log_text) = {
            let v = sh.install.lock().unwrap();
            (v.cancelling, v.log.iter().rev().take(25).rev().cloned().collect::<Vec<_>>().join("\n"))
        };
        let last = log_text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
        let hint = explain(&log_text).map(|h| format!(",{h}")).unwrap_or_default();
        let res = match poll.status {
            Some(0) => {
                if poll.accel && !poll.accel_done {
                    install_accel(r, sh).await;
                }
                Ok(())
            }
            Some(code) => Err(format!("安装失败(退出码 {code}){hint}:{last}")),
            None if cancelled => Err(CANCELLED.into()),
            None => Err(format!("安装进程中断了(服务器可能重启过,或进程被结束){hint}:{last}")),
        };
        // 结果已经读完,日志也在界面里了:清掉服务器上的脚本、日志和状态文件。失败只记一行
        if let Err(e) = r.root(&crate::docker_install::cleanup_script(res.is_err()), None, Duration::from_secs(15)).await {
            logs::push(logs::Level::Warn, "Docker", format!("没能清理服务器上的安装临时文件:{e}"));
        }
        break Some(res);
    };
    sh.busy.lock().unwrap().remove(&key);
    sh.install_following.store(false, Ordering::SeqCst);
    match outcome {
        None => {
            logs::push(logs::Level::Warn, "Docker", "与服务器的连接断了,安装仍在服务器上进行");
            let mut v = sh.install.lock().unwrap();
            if v.phase == InstallPhase::Running {
                v.phase = InstallPhase::Lost;
            }
        }
        Some(res) => {
            logs::push(if res.is_ok() { logs::Level::Info } else { logs::Level::Warn }, "Docker", match &res {
                Ok(()) => "Docker 安装完成".to_string(),
                Err(e) => e.clone(),
            });
            sh.install.lock().unwrap().phase = match res {
                Ok(()) => InstallPhase::Done,
                Err(e) if e == CANCELLED => InstallPhase::Cancelled,
                Err(e) => InstallPhase::Failed(e),
            };
        }
    }
    sh.bump();
}

/// 装完后配置 Docker Hub 镜像加速(合并进 daemon.json);服务器上的标记文件保证只做一次。失败只记一行,不算安装失败。
async fn install_accel(r: &Runner, sh: &Shared) {
    let push = |line: String| {
        sh.install.lock().unwrap().log.push(line);
        sh.bump();
    };
    push("==> 配置 Docker Hub 镜像加速".into());
    let done = match read_daemon_text(r).await.and_then(|t| crate::daemon_conf::add_mirrors(&t, &crate::docker_install::REGISTRY_MIRRORS)) {
        Ok(text) => match write_daemon(r, &text).await {
            Ok(()) => restart_docker(r).await,
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    };
    let _ = r.root(&crate::docker_install::accel_done_script(), None, Duration::from_secs(15)).await;
    match done {
        Ok(()) => push("镜像加速已配置".into()),
        Err(e) => push(format!("镜像加速没有配置成功:{e}")),
    }
}

/// 一个已登录的镜像仓库。只保留地址和用户名,凭据本身不会读进界面。
#[derive(Clone, Debug, PartialEq)]
pub struct RegAuth {
    pub host: String,
    /// 用户名;凭据由系统钥匙串之类的 helper 保存时拿不到
    pub user: Option<String>,
}

/// 把 `auth` 字段(base64 的 `用户名:密码`)里的用户名取出来,密码部分直接丢弃
fn auth_user(auth: &str) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for ch in auth.bytes() {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            buf.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    let colon = buf.iter().position(|b| *b == b':')?;
    String::from_utf8(buf[..colon].to_vec()).ok().filter(|u| !u.is_empty())
}

/// 解析 `~/.docker/config.json` 里登录过的仓库(`auths` 里的每一项,外加 credHelpers 里单独指定的)。内容为空表示没登录过。
pub fn parse_registries(text: &str) -> Result<Vec<RegAuth>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("config.json 不是合法的 JSON:{e}"))?;
    let mut out: Vec<RegAuth> = Vec::new();
    if let Some(auths) = v.get("auths").and_then(|a| a.as_object()) {
        for (host, e) in auths {
            let user = e.get("auth").and_then(|a| a.as_str()).and_then(auth_user).or_else(|| e.get("username").and_then(|u| u.as_str()).filter(|u| !u.is_empty()).map(str::to_string));
            out.push(RegAuth { host: host.clone(), user });
        }
    }
    if let Some(h) = v.get("credHelpers").and_then(|a| a.as_object()) {
        for host in h.keys() {
            if !out.iter().any(|r| &r.host == host) {
                out.push(RegAuth { host: host.clone(), user: None });
            }
        }
    }
    out.sort_by(|a, b| a.host.cmp(&b.host));
    Ok(out)
}

/// 跑一段 shell:`root` 时走 sudo,否则以登录用户的身份(包一层 `sh -c`,不依赖用户的登录 shell)
async fn sys_exec(r: &Runner, script: &str, root: bool, input: Option<&str>, t: Duration) -> Result<Output, String> {
    if root { r.root(script, input, t).await } else { exec(&r.session, &format!("sh -c {}", shell_quote(script)), input.map(str::as_bytes), t).await }
}

/// 下载一个文件:先查大小,再 `cat` 边收边写到本地(先写临时文件,完整了才改名),进度放进 `Shared.fetch`
async fn fetch_file(r: &Runner, sh: &Shared, tag: &str, remote: &str, local: &std::path::Path) -> Result<String, String> {
    use std::io::Write as _;
    let q = shell_quote(remote);
    let o = exec(&r.session, &format!("stat -c %s {q}"), None, Duration::from_secs(15)).await?;
    let size: u64 = o.stdout.trim().parse().map_err(|_| "找不到这个备份文件".to_string())?;
    sh.fetch.lock().unwrap().insert(tag.to_string(), (0, size));
    sh.bump();
    let tmp = local.with_extension("otter-part");
    let mut f = std::fs::File::create(&tmp).map_err(|e| format!("保存失败:{e}"))?;
    let mut ch = r.session.channel_open_session().await.map_err(|e| format!("打开通道失败:{e}"))?;
    ch.exec(true, format!("cat {q}")).await.map_err(|e| format!("执行失败:{e}"))?;
    let (mut done, mut code, mut last) = (0u64, None, Instant::now());
    let res: Result<(), String> = async {
        loop {
            // 一直没数据(连接卡住)超过 60 秒就放弃
            let Some(msg) = tokio::time::timeout(Duration::from_secs(60), ch.wait()).await.map_err(|_| "下载超时,请重试".to_string())? else { break };
            match msg {
                ChannelMsg::Data { data } => {
                    f.write_all(&data).map_err(|e| format!("保存失败:{e}"))?;
                    done += data.len() as u64;
                    if last.elapsed() >= Duration::from_millis(150) {
                        last = Instant::now();
                        sh.fetch.lock().unwrap().insert(tag.to_string(), (done, size));
                        sh.bump();
                    }
                }
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok(())
    }
    .await;
    drop(f);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if code != Some(0) || done != size {
        let _ = std::fs::remove_file(&tmp);
        return Err("下载中断,请重试".into());
    }
    std::fs::rename(&tmp, local).map_err(|e| format!("保存失败:{e}"))?;
    Ok(local.display().to_string())
}

fn last_line(o: &Output) -> String {
    let t = if o.stderr.trim().is_empty() { &o.stdout } else { &o.stderr };
    t.trim().lines().last().unwrap_or("未知错误").trim().to_string()
}

/// 容器抽屉读数据:输出原样存起来(带上请求的标签),解析在界面一侧
async fn ct_load(r: &Runner, sh: &Shared, kind: crate::ctfiles::CtKind, tag: String, args: Vec<String>, merge: bool) {
    let res = match r.docker_raw(&args, None, merge, Duration::from_secs(30)).await {
        Ok(o) if o.code == 0 || (merge && !o.out.is_empty()) => Ok(String::from_utf8_lossy(&o.out).into_owned()),
        Ok(o) => {
            let t = if o.err.is_empty() { &o.out } else { &o.err };
            Err(String::from_utf8_lossy(t).trim().lines().last().unwrap_or("未知错误").to_string())
        }
        Err(e) => Err(e),
    };
    sh.ct.lock().unwrap().insert(kind, (tag, res));
    sh.bump();
}

/// 数据库页:在容器里跑一段脚本。成功返回 stdout;失败返回 stderr(没有就 stdout)的最后一行。
/// `input` 里可能有密码,用完即清零;结果和日志里都不带它。
async fn db_exec(r: &Runner, sh: &Shared, tag: String, args: Vec<String>, input: Option<zeroize::Zeroizing<String>>) {
    let res = match r.docker_raw(&args, input.as_ref().map(|i| i.as_bytes()), false, Duration::from_secs(60)).await {
        Ok(o) if o.code == 0 => Ok(String::from_utf8_lossy(&o.out).into_owned()),
        Ok(o) => {
            let t = if o.err.iter().any(|b| !b.is_ascii_whitespace()) { &o.err } else { &o.out };
            Err(String::from_utf8_lossy(t).trim().lines().last().unwrap_or("未知错误").to_string())
        }
        Err(e) => Err(e),
    };
    sh.db.lock().unwrap().insert(tag, res);
    sh.bump();
}

/// 把 docker 报的错换成人看得懂的话(容器文件管理用)
fn ct_friendly(msg: &str) -> String {
    let m = msg.to_lowercase();
    if m.contains("executable file not found") || m.contains("no such file or directory: unknown") {
        "这个容器的镜像里没有 ls、sh 这类基础命令(常见于只装了一个程序的精简镜像),所以不能浏览和修改里面的目录。上传和下载仍然可以用。".into()
    } else if m.contains("is not running") || m.contains("not running") {
        "容器没有运行,请先启动容器".into()
    } else if m.contains("no such container") {
        "找不到这个容器,可能已经被删除".into()
    } else if m.contains("permission denied") {
        "没有权限访问这个位置".into()
    } else if m.contains("no such file or directory") {
        "这个路径不存在".into()
    } else if m.contains("not a directory") {
        "这不是一个文件夹".into()
    } else {
        msg.to_string()
    }
}

fn raw_last_line(o: &RawOutput) -> String {
    let t = if o.err.is_empty() { &o.out } else { &o.err };
    String::from_utf8_lossy(t).trim().lines().last().unwrap_or("未知错误").trim().to_string()
}

/// 容器里的文件操作。结果写进和 SFTP 同一套共享状态,界面不用区分。
async fn ctfs_run(r: &Runner, name: &str, sh: &crate::sftp::Shared, cmd: crate::sftp::Cmd) {
    use crate::sftp::{self, Cmd as F, Direction};
    let arg = |v: &[&str]| -> Vec<String> { ["exec", name].iter().chain(v).map(|x| x.to_string()).collect() };
    match cmd {
        F::List(path) => ctfs_list(r, name, sh, &path).await,
        F::Mkdir(path) => {
            if !crate::ctfiles::safe_path(&path) {
                sftp::notice(sh, "新建文件夹失败:路径不合法".into());
            } else {
                match r.docker_raw(&arg(&["mkdir", "-p", "--", &path]), None, false, Duration::from_secs(30)).await {
                    Ok(o) if o.code == 0 => {}
                    Ok(o) => sftp::notice(sh, format!("新建文件夹失败:{}", ct_friendly(&raw_last_line(&o)))),
                    Err(e) => sftp::notice(sh, format!("新建文件夹失败:{e}")),
                }
            }
            ctfs_list(r, name, sh, &sftp::parent_of(&path)).await;
        }
        F::Remove { path, .. } => {
            let short = path.rsplit('/').next().unwrap_or(&path).to_string();
            if path.trim_matches('/').is_empty() || !crate::ctfiles::safe_path(&path) {
                sftp::notice(sh, "不能删除这个路径".into());
            } else {
                match r.docker_raw(&arg(&["rm", "-rf", "--", &path]), None, false, Duration::from_secs(120)).await {
                    Ok(o) if o.code == 0 => sftp::notice(sh, format!("已删除 {short}")),
                    Ok(o) => sftp::notice(sh, format!("删除 {short} 失败:{}", ct_friendly(&raw_last_line(&o)))),
                    Err(e) => sftp::notice(sh, format!("删除 {short} 失败:{e}")),
                }
            }
            ctfs_list(r, name, sh, &sftp::parent_of(&path)).await;
        }
        F::Upload { locals, remote_dir } => {
            let names: Vec<String> = locals.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
            let id = sftp::new_transfer(sh, sftp::transfer_name(&names), Direction::Up, 1);
            let res: Result<(), String> = async {
                if !crate::ctfiles::safe_path(&remote_dir) {
                    return Err("路径不合法".into());
                }
                let tar = tokio::task::spawn_blocking(move || crate::ctfiles::build_tar(&locals)).await.map_err(|e| e.to_string())??;
                if let Ok(mut t) = sh.transfers.lock() {
                    if let Some(x) = t.iter_mut().find(|x| x.id == id) {
                        x.total = (tar.len() as u64).max(1);
                    }
                }
                let args: Vec<String> = ["cp".to_string(), "-".to_string(), format!("{name}:{remote_dir}")].into();
                let o = r.docker_raw(&args, Some(&tar), false, Duration::from_secs(600)).await?;
                if o.code != 0 {
                    return Err(ct_friendly(&raw_last_line(&o)));
                }
                Ok(())
            }
            .await;
            sftp::finish(sh, id, res);
            ctfs_list(r, name, sh, &remote_dir).await;
        }
        F::Download { remote, is_dir, local_dir } => {
            let short = remote.trim_end_matches('/').rsplit('/').next().unwrap_or("文件").to_string();
            let id = sftp::new_transfer(sh, short.clone(), Direction::Down, 1);
            let res: Result<(), String> = async {
                if !crate::ctfiles::safe_path(&remote) || !crate::ctfiles::safe_filename(&short) {
                    return Err("路径不合法".into());
                }
                let args: Vec<String> = ["cp".to_string(), format!("{name}:{remote}"), "-".to_string()].into();
                let o = r.docker_raw(&args, None, false, Duration::from_secs(600)).await?;
                if o.code != 0 {
                    return Err(ct_friendly(&raw_last_line(&o)));
                }
                if o.out.len() as u64 > crate::ctfiles::XFER_MAX {
                    return Err(format!("内容太大(上限 {} MB)", crate::ctfiles::XFER_MAX / 1024 / 1024));
                }
                if let Ok(mut t) = sh.transfers.lock() {
                    if let Some(x) = t.iter_mut().find(|x| x.id == id) {
                        x.total = (o.out.len() as u64).max(1);
                    }
                }
                let _ = std::fs::create_dir_all(&local_dir);
                let data = o.out;
                tokio::task::spawn_blocking(move || {
                    if is_dir {
                        crate::ctfiles::extract_dir(&data, &local_dir)
                    } else {
                        crate::ctfiles::extract_file(&data, &sftp::unique_local(&local_dir, &short))
                    }
                })
                .await
                .map_err(|e| e.to_string())?
            }
            .await;
            sftp::finish(sh, id, res);
        }
    }
}

/// 读目录:`ls -la`,符号链接再补一次「指向的是不是目录」
async fn ctfs_list(r: &Runner, name: &str, sh: &crate::sftp::Shared, path: &str) {
    use crate::sftp::{DirState, RemoteEntry};
    let fail = |e: String| DirState { entries: vec![], loading: false, error: Some(e) };
    let state = async {
        if !crate::ctfiles::safe_path(path) {
            return fail("路径不合法".into());
        }
        let dir = if path.ends_with('/') { path.to_string() } else { format!("{path}/") };
        let args: Vec<String> = ["exec", name, "ls", "-la", &dir].iter().map(|x| x.to_string()).collect();
        let o = match r.docker_raw(&args, None, false, Duration::from_secs(30)).await {
            Ok(o) => o,
            Err(e) => return fail(ct_friendly(&e)),
        };
        if o.code != 0 {
            return fail(ct_friendly(&raw_last_line(&o)));
        }
        let list = match crate::ctfiles::parse_ls(&String::from_utf8_lossy(&o.out)) {
            Ok(l) => l,
            Err(e) => return fail(e),
        };
        let links: Vec<String> = list.iter().filter(|e| e.kind == crate::ctfiles::EKind::Link).map(|e| e.name.clone()).collect();
        let mut dirs: Vec<String> = Vec::new();
        if !links.is_empty() {
            let mut a: Vec<String> = ["exec", "-w", &dir, name, "sh", "-c", r#"for f; do [ -d "$f" ] && printf '%s\n' "$f"; done; true"#, "_"].iter().map(|x| x.to_string()).collect();
            a.extend(links);
            if let Ok(o) = r.docker_raw(&a, None, false, Duration::from_secs(30)).await {
                dirs = String::from_utf8_lossy(&o.out).lines().map(String::from).collect();
            }
        }
        let entries = list
            .into_iter()
            .map(|e| {
                let is_link = e.kind == crate::ctfiles::EKind::Link;
                RemoteEntry {
                    is_dir: e.kind == crate::ctfiles::EKind::Dir || (is_link && dirs.contains(&e.name)),
                    is_link,
                    size: e.size,
                    mtime: crate::ctfiles::mtime_secs(&e.mtime),
                    mode: e.mode,
                    owner: e.owner,
                    link: e.link,
                    name: e.name,
                }
            })
            .collect();
        DirState { entries, loading: false, error: None }
    }
    .await;
    crate::sftp::set_dir(sh, path, state);
}

/// 运维页读数据:输出原样存起来,解析在界面一侧
async fn sys_load(r: &Runner, sh: &Shared, kind: crate::sysops::Kind, script: &str, root: bool) {
    let res = match sys_exec(r, script, root, None, Duration::from_secs(30)).await {
        Ok(o) if o.code == 0 || !o.stdout.trim().is_empty() => Ok(o.stdout),
        Ok(o) => Err(last_line(&o)),
        Err(e) => Err(e),
    };
    sh.sys.lock().unwrap().insert(kind, res);
    sh.bump();
}

/// 运维页的操作;完成后提示结果,并重读相关数据
async fn sys_job(r: &Runner, sh: &Shared, job: crate::sysops::Job) {
    let key = format!("{}:{}", job.label, job.target);
    sh.busy.lock().unwrap().insert(key.clone());
    sh.bump();
    let res = sys_exec(r, &job.script, job.root, job.input.as_deref(), Duration::from_secs(60)).await;
    sh.busy.lock().unwrap().remove(&key);
    let msg = match res {
        Ok(o) if o.code == 0 => format!("已{}:{}", job.label, job.target),
        Ok(o) => format!("{}失败:{}", job.label, last_line(&o)),
        Err(e) => format!("{}失败:{e}", job.label),
    };
    logs::push(logs::Level::Info, "服务器", msg.clone());
    *sh.notice.lock().unwrap() = Some(msg);
    sh.bump();
    if let Some((kind, script, root)) = job.reload {
        tokio::time::sleep(Duration::from_millis(500)).await;
        sys_load(r, sh, kind, &script, root).await;
    }
}

/// 读已登录的仓库:和 docker 命令用同一个身份(走 sudo 时是 root 的配置)
async fn load_registries(r: &Runner, sh: &Shared) {
    let script = r#"f="${DOCKER_CONFIG:-$HOME/.docker}/config.json"; [ -f "$f" ] && cat "$f"; true"#;
    let out = match r.sudo {
        Sudo::No => exec(&r.session, script, None, Duration::from_secs(15)).await,
        _ => r.root(r#"f=/root/.docker/config.json; [ -f "$f" ] && cat "$f"; true"#, None, Duration::from_secs(15)).await,
    };
    let res = match out {
        Ok(o) if o.code == 0 => parse_registries(&o.stdout),
        Ok(o) => Err(format!("读取失败:{}", o.stderr.trim().lines().last().unwrap_or("未知错误"))),
        Err(e) => Err(e),
    };
    *sh.registries.lock().unwrap() = Some(res);
    sh.bump();
}

/// 登录仓库:密码走 stdin(`--password-stdin`),不出现在命令行、进程列表和日志里
async fn run_login(r: &Runner, sh: &Shared, registry: &str, user: &str, pass: &str) {
    let label = if registry.is_empty() { "Docker Hub" } else { registry };
    let key = format!("登录:{label}");
    if (!registry.is_empty() && !safe_name(registry)) || user.is_empty() || user.chars().any(|c| c.is_control()) || pass.is_empty() || pass.contains(['\n', '\r']) {
        *sh.notice.lock().unwrap() = Some("登录失败:仓库地址、用户名或密码里有不支持的字符".into());
        sh.bump();
        return;
    }
    sh.busy.lock().unwrap().insert(key.clone());
    sh.bump();
    let mut args: Vec<String> = ["login", "-u", user, "--password-stdin"].iter().map(|s| s.to_string()).collect();
    if !registry.is_empty() {
        args.push(registry.to_string());
    }
    let input = zeroize::Zeroizing::new(format!("{pass}\n"));
    let res = r.docker_in(&args, Some(&input), Duration::from_secs(60)).await;
    sh.busy.lock().unwrap().remove(&key);
    let msg = match res {
        Ok(o) if o.code == 0 => format!("已登录 {label}"),
        Ok(o) => {
            let err = o.stderr.lines().find(|l| l.to_lowercase().contains("error")).or_else(|| o.stderr.trim().lines().last()).unwrap_or("未知错误");
            format!("登录 {label} 失败:{}", err.trim())
        }
        Err(e) => format!("登录 {label} 失败:{e}"),
    };
    logs::push(logs::Level::Info, "Docker", msg.clone());
    *sh.notice.lock().unwrap() = Some(msg);
    load_registries(r, sh).await;
}

/// 读 daemon.json(文件不存在算「还没有配置」,不是错误)
async fn load_daemon(r: &Runner, sh: &Shared) {
    let script = format!("if [ -f {p} ]; then cat {p}; else echo __OTTER_NOFILE__; fi", p = crate::daemon_conf::PATH);
    let res = match r.root(&script, None, Duration::from_secs(15)).await {
        Ok(o) if o.code == 0 => {
            if o.stdout.trim() == "__OTTER_NOFILE__" { Ok((false, String::new())) } else { Ok((true, o.stdout)) }
        }
        Ok(o) => Err(format!("读取失败:{}", o.stderr.trim().lines().last().unwrap_or("需要 root 权限(免密 sudo,或在节点里保存密码)"))),
        Err(e) => Err(e),
    };
    *sh.daemon.lock().unwrap() = Some(res);
    sh.bump();
}

/// 写 daemon.json:先备份成 `.bak`,新内容写到临时文件,dockerd 支持 `--validate` 时先校验,通过才替换;`restart` 时再重启 Docker
async fn run_daemon(r: &Runner, sh: &Shared, text: &str, restart: bool, verb: &'static str) {
    let key = format!("{verb}:daemon.json");
    sh.busy.lock().unwrap().insert(key.clone());
    sh.bump();
    let mut res = write_daemon(r, text).await;
    if res.is_ok() && restart {
        res = restart_docker(r).await.map_err(|e| format!("配置已保存,但重启 Docker 失败:{e}"));
    }
    sh.busy.lock().unwrap().remove(&key);
    let msg = match res {
        Ok(()) => format!("已{verb}"),
        Err(e) => format!("{verb}失败:{e}"),
    };
    logs::push(logs::Level::Info, "Docker", msg.clone());
    *sh.notice.lock().unwrap() = Some(msg);
    load_daemon(r, sh).await;
}

/// 写 daemon.json:先备份成 `.bak`,新内容写到临时文件,dockerd 支持 `--validate` 时先校验,通过才替换
async fn write_daemon(r: &Runner, text: &str) -> Result<(), String> {
    let p = crate::daemon_conf::PATH;
    let script = format!(
        "set -e; mkdir -p /etc/docker; [ -f {p} ] && cp -p {p} {p}.bak || true; cat > {p}.otter-new; \
         if command -v dockerd >/dev/null 2>&1 && dockerd --help 2>&1 | grep -q -- --validate; then \
           dockerd --validate --config-file={p}.otter-new >/dev/null 2>&1 || {{ rm -f {p}.otter-new; echo '配置没通过 dockerd 的校验,没有改动' >&2; exit 1; }}; fi; \
         mv {p}.otter-new {p}"
    );
    match r.root(&script, Some(text), Duration::from_secs(30)).await {
        Ok(o) if o.code == 0 => Ok(()),
        Ok(o) => Err(o.stderr.trim().lines().last().unwrap_or("需要 root 权限(免密 sudo,或在节点里保存密码)").to_string()),
        Err(e) => Err(e),
    }
}

async fn restart_docker(r: &Runner) -> Result<(), String> {
    match r.root("systemctl restart docker || service docker restart", None, Duration::from_secs(120)).await {
        Ok(o) if o.code == 0 => Ok(()),
        Ok(o) => Err(o.stderr.trim().lines().last().unwrap_or("未知错误").to_string()),
        Err(e) => Err(e),
    }
}

/// 读 daemon.json 的原文;文件不存在是空串
async fn read_daemon_text(r: &Runner) -> Result<String, String> {
    let script = format!("if [ -f {p} ]; then cat {p}; fi", p = crate::daemon_conf::PATH);
    match r.root(&script, None, Duration::from_secs(15)).await {
        Ok(o) if o.code == 0 => Ok(o.stdout),
        Ok(o) => Err(format!("读取 daemon.json 失败:{}", o.stderr.trim().lines().last().unwrap_or("需要 root 权限(免密 sudo,或在节点里保存密码)"))),
        Err(e) => Err(e),
    }
}

/// 登录 HTTP 仓库:先看 daemon.json 的 insecure-registries 里有没有它。有就直接登录;没有就把凭据暂存,让界面去向用户确认「加入并重启 Docker」。
async fn run_login_http(r: &Runner, sh: &Shared, registry: &str, user: &str, pass: zeroize::Zeroizing<String>) {
    if registry.is_empty() || !safe_name(registry) || registry.contains("://") {
        *sh.notice.lock().unwrap() = Some("登录失败:HTTP 仓库要填 主机:端口,不带 http://".into());
        sh.bump();
        return;
    }
    match read_daemon_text(r).await.and_then(|t| crate::daemon_conf::has_insecure(&t, registry)) {
        Ok(true) => run_login(r, sh, registry, user, &pass).await,
        Ok(false) => {
            *sh.pending_login.lock().unwrap() = Some((registry.to_string(), user.to_string(), pass));
            *sh.insecure_ask.lock().unwrap() = Some(registry.to_string());
            sh.bump();
        }
        Err(e) => {
            *sh.notice.lock().unwrap() = Some(format!("登录 {registry} 失败:{e}"));
            sh.bump();
        }
    }
}

/// 用户确认后:把仓库加进 insecure-registries → 重启 Docker → 等它起来 → 登录
async fn run_insecure(r: &Runner, sh: &Shared, host: &str) {
    let pending = sh.pending_login.lock().unwrap().take();
    let Some((registry, user, pass)) = pending.filter(|p| p.0 == host) else {
        *sh.notice.lock().unwrap() = Some("登录请求已过期,请重新添加仓库".into());
        sh.bump();
        return;
    };
    let key = format!("启用 HTTP 仓库:{host}");
    sh.busy.lock().unwrap().insert(key.clone());
    sh.bump();
    let say = |m: String| {
        *sh.notice.lock().unwrap() = Some(m);
        sh.bump();
    };
    let step = async {
        let text = read_daemon_text(r).await?;
        let new = crate::daemon_conf::add_insecure(&text, host)?;
        write_daemon(r, &new).await?;
        say("已写入 daemon.json,正在重启 Docker…".into());
        restart_docker(r).await.map_err(|e| format!("配置已保存,但重启 Docker 失败:{e}"))?;
        // 等 Docker 重新可用
        let ver: Vec<String> = ["version", "--format", "{{.Server.Version}}"].iter().map(|s| s.to_string()).collect();
        for _ in 0..30 {
            if matches!(r.docker(&ver, Duration::from_secs(10)).await, Ok(o) if o.code == 0) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err("Docker 重启后一直没有恢复".to_string())
    };
    let res: Result<(), String> = step.await;
    sh.busy.lock().unwrap().remove(&key);
    match res {
        Ok(()) => {
            logs::push(logs::Level::Info, "Docker", format!("已把 {host} 加入 insecure-registries 并重启 Docker"));
            run_login(r, sh, &registry, &user, &pass).await;
        }
        Err(e) => {
            let msg = format!("启用 HTTP 仓库失败:{e}");
            logs::push(logs::Level::Info, "Docker", msg.clone());
            say(msg);
        }
    }
    load_daemon(r, sh).await;
}

/// 把 docker 的错误输出归类成界面能解释的问题
fn classify(stderr: &str) -> Problem {
    let low = stderr.to_lowercase();
    let msg = stderr.trim().lines().last().unwrap_or("").to_string();
    // 服务器可能是中文等其他语言环境:「未找到命令」「找不到命令」「No such file」也算没装
    let not_found = low.contains("command not found") || low.contains("未找到命令") || low.contains("找不到命令") || low.contains("未找到") && low.contains("docker") || low.contains("not found") && low.contains("docker");
    if not_found {
        Problem::NoDocker
    } else if low.contains("permission denied") {
        Problem::NoPermission(msg)
    } else if low.contains("cannot connect to the docker daemon") || low.contains("is the docker daemon running") {
        Problem::DaemonDown(msg)
    } else {
        Problem::Other(msg)
    }
}

/// 探测怎么才能在这台服务器上跑 docker:直接 → sudo -n → sudo -S(有保存的密码)
async fn probe(session: &Arc<SshHandle<Client>>, conn: &Conn) -> Result<Runner, Problem> {
    let ver: Vec<String> = ["version", "--format", "{{.Server.Version}}"].iter().map(|s| s.to_string()).collect();
    let t = Duration::from_secs(15);
    let mut r = Runner { session: session.clone(), sudo: Sudo::No, password: None };
    let first = r.docker(&ver, t).await.map_err(Problem::Other)?;
    if first.code == 0 {
        return Ok(r);
    }
    let p = classify(&first.stderr);
    if !matches!(p, Problem::NoPermission(_)) {
        return Err(p);
    }
    r.sudo = Sudo::Passwordless;
    let mut last = String::new();
    if let Ok(o) = r.docker(&ver, t).await {
        if o.code == 0 {
            logs::push(logs::Level::Info, &conn.host, "Docker:当前用户没有权限,改用 sudo");
            return Ok(r);
        }
        last = o.stderr;
    }
    let saved = conn.auth.password.is_some();
    if let Some(pw) = &conn.auth.password {
        r.sudo = Sudo::WithPassword;
        r.password = Some(pw.clone());
        if let Ok(o) = r.docker(&ver, t).await {
            if o.code == 0 {
                logs::push(logs::Level::Info, &conn.host, "Docker:当前用户没有权限,改用 sudo(密码)");
                return Ok(r);
            }
            last = o.stderr;
        }
    }
    let msg = sudo_failure(&last, saved, &conn.user);
    logs::push(logs::Level::Info, &conn.host, format!("Docker:{msg}"));
    Err(Problem::NoPermission(msg))
}

/// 把 sudo 失败的输出翻成能照着做的原因。`saved` 是节点里有没有保存密码
fn sudo_failure(stderr: &str, saved: bool, user: &str) -> String {
    let low = stderr.to_lowercase();
    let fix = format!("在服务器上用 root 执行 usermod -aG docker {user} 并重新登录,即可不用 sudo");
    if low.contains("command not found") || low.contains("未找到命令") || low.contains("not found") {
        format!("当前用户不能运行 docker,且服务器没有安装 sudo。{fix}")
    } else if low.contains("not in the sudoers") || low.contains("不在 sudoers") || low.contains("may not run sudo") {
        format!("当前用户不能运行 docker,且不在 sudoers 里,不能 sudo。{fix}")
    } else if saved && (low.contains("incorrect password") || low.contains("try again") || low.contains("sorry") || low.contains("密码")) {
        "当前用户不能运行 docker;sudo 拒绝了节点里保存的密码,请在节点里更新为正确的 sudo 密码".into()
    } else if !saved {
        format!("当前用户不能运行 docker,sudo 需要密码但节点里没有保存。可在节点里保存密码,或配置免密 sudo。{fix}")
    } else {
        let tail = stderr.trim().lines().last().unwrap_or("").trim();
        if tail.is_empty() {
            format!("当前用户不能运行 docker,sudo 也不可用。{fix}")
        } else {
            format!("当前用户不能运行 docker,sudo 也失败了:{tail}。{fix}")
        }
    }
}

struct Pollers {
    last_ps: Option<Instant>,
    last_stats: Option<Instant>,
    last_df: Option<Instant>,
}

async fn refresh_ps(r: &Runner, sh: &Shared) -> Result<(), String> {
    let t = Duration::from_secs(20);
    let ps = r.docker(&["ps".into(), "-a".into(), "--format".into(), PS_FORMAT.into()], t).await?;
    if ps.code != 0 {
        return Err(ps.stderr.trim().to_string());
    }
    let imgs = r.docker(&["images".into(), "--format".into(), IMAGES_FORMAT.into()], t).await?;
    let vols = r.docker(&["volume".into(), "ls".into(), "--format".into(), VOLUMES_FORMAT.into()], t).await?;
    let mut snap = sh.snapshot.lock().unwrap().as_deref().cloned().unwrap_or_default();
    snap.containers = parse_ps(&ps.stdout);
    if imgs.code == 0 {
        snap.images = parse_images(&imgs.stdout);
    }
    if vols.code == 0 {
        let old: HashMap<String, Option<String>> = snap.volumes.iter().map(|v| (v.name.clone(), v.size.clone())).collect();
        snap.volumes = parse_volumes(&vols.stdout);
        for v in &mut snap.volumes {
            v.size = old.get(&v.name).cloned().flatten();
        }
    }
    let alive: HashSet<&String> = snap.containers.iter().map(|c| &c.id).collect();
    snap.stats.retain(|id, _| alive.contains(id));
    snap.series.retain(|id, _| alive.contains(id));
    snap.fetched_ms = now_ms();
    *sh.snapshot.lock().unwrap() = Some(Arc::new(snap));
    *sh.problem.lock().unwrap() = None;
    sh.bump();
    Ok(())
}

async fn refresh_stats(r: &Runner, sh: &Shared) -> Result<(), String> {
    let o = r.docker(&["stats".into(), "--no-stream".into(), "--format".into(), STATS_FORMAT.into()], Duration::from_secs(40)).await?;
    if o.code != 0 {
        return Err(o.stderr.trim().to_string());
    }
    let stats = parse_stats(&o.stdout);
    let at = Instant::now();
    let mut snap = sh.snapshot.lock().unwrap().as_deref().cloned().unwrap_or_default();
    for (id, s) in &stats {
        snap.series.entry(id.clone()).or_default().push(*s, at);
    }
    snap.stats = stats;
    *sh.snapshot.lock().unwrap() = Some(Arc::new(snap));
    sh.bump();
    Ok(())
}

async fn refresh_df(r: &Runner, sh: &Shared) -> Result<(), String> {
    let o = r.docker(&["system".into(), "df".into(), "-v".into()], Duration::from_secs(60)).await?;
    if o.code != 0 {
        return Err(o.stderr.trim().to_string());
    }
    let sizes = parse_df_volumes(&o.stdout);
    let mut snap = sh.snapshot.lock().unwrap().as_deref().cloned().unwrap_or_default();
    for v in &mut snap.volumes {
        if let Some(s) = sizes.get(&v.name) {
            v.size = Some(s.clone());
        }
    }
    *sh.snapshot.lock().unwrap() = Some(Arc::new(snap));
    sh.bump();
    Ok(())
}

async fn session_task(conn: &Conn, label: &str, io: &Io, sh: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Cmd>, shared: Option<Arc<SshHandle<Client>>>) -> Result<(), String> {
    if let Some(e) = &conn.auth_error {
        return Err(e.clone());
    }
    if !conn.has_saved_auth() && shared.is_none() {
        return Err("这个节点还没有保存密码或私钥,请先在节点的「编辑」里填写".into());
    }
    let session = match shared {
        Some(s) => {
            logs::push(logs::Level::Debug, label, "Docker:复用终端的连接");
            s
        }
        None => {
            let s = Arc::new(open_authenticated(conn, io).await?);
            logs::push(logs::Level::Debug, label, "Docker:认证成功");
            s
        }
    };
    // 资源监控和 Docker 互不依赖:没装 Docker 的服务器也能看到 CPU、内存、磁盘
    tokio::spawn(monitor_task(session.clone(), sh.clone()));
    // 运维页(进程、服务、存储、计划任务)和 Docker 也互不依赖;要 root 的操作走 sudo(免密,或用节点保存的密码)
    let sys = Arc::new(Runner { session: session.clone(), sudo: if conn.auth.password.is_some() { Sudo::WithPassword } else { Sudo::Passwordless }, password: conn.auth.password.clone() });
    io.begin_step("检测 Docker");
    let runner = match probe(&session, conn).await {
        Ok(r) => r,
        Err(p) => {
            // 能连上但跑不了 docker:不是连接失败,留在页面里按问题类型解释
            io.done_step();
            *sh.problem.lock().unwrap() = Some(p);
            *sh.status.lock().unwrap() = Status::Ready;
            sh.bump();
            // 保持连接,等界面点「重试」
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    Cmd::Refresh => return Err("请重新进入页面重试".into()),
                    Cmd::InstallProbe => {
                        let (r, sh2, pw) = (sys.clone(), sh.clone(), conn.auth.password.is_some());
                        tokio::spawn(async move { run_install_probe(&r, &sh2, pw).await });
                    }
                    Cmd::InstallCancel => {
                        let r = sys.clone();
                        tokio::spawn(async move {
                            let _ = r.root(&crate::docker_install::cancel_script(), None, Duration::from_secs(20)).await;
                        });
                    }
                    Cmd::Act(Action::InstallDocker { source, accel }) => {
                        let (r, sh2) = (sys.clone(), sh.clone());
                        tokio::spawn(async move { run_install(&r, &sh2, source, accel).await });
                    }
                    Cmd::Sys { kind, script, root } => {
                        let (r, sh2) = (sys.clone(), sh.clone());
                        tokio::spawn(async move { sys_load(&r, &sh2, kind, &script, root).await });
                    }
                    Cmd::Act(Action::Sys(job)) => {
                        let (r, sh2) = (sys.clone(), sh.clone());
                        tokio::spawn(async move { sys_job(&r, &sh2, job).await });
                    }
                    _ => {}
                }
            }
            return Ok(());
        }
    };
    io.done_step();
    *sh.status.lock().unwrap() = Status::Ready;
    sh.bump();
    logs::push(logs::Level::Debug, label, "Docker 已就绪");

    *sh.sudo.lock().unwrap() = Some(runner.sudo);
    let runner = Arc::new(runner);
    let mut pol = Pollers { last_ps: None, last_stats: None, last_df: None };
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    Cmd::Refresh => {
                        pol.last_ps = None;
                        pol.last_stats = None;
                    }
                    // 只在「没有安装 Docker」页用;已经能跑 docker 时不会发出
                    Cmd::InstallProbe | Cmd::InstallCancel => continue,
                    Cmd::LoadDaemon => {
                        let (r, sh2) = (runner.clone(), sh.clone());
                        tokio::spawn(async move { load_daemon(&r, &sh2).await });
                        continue;
                    }
                    Cmd::LoadRegistries => {
                        let (r, sh2) = (runner.clone(), sh.clone());
                        tokio::spawn(async move { load_registries(&r, &sh2).await });
                        continue;
                    }
                    Cmd::Login { registry, user, pass } => {
                        let (r, sh2) = (runner.clone(), sh.clone());
                        tokio::spawn(async move { run_login(&r, &sh2, &registry, &user, &pass).await });
                        continue;
                    }
                    Cmd::LoginHttp { registry, user, pass } => {
                        let (r, sh2) = (runner.clone(), sh.clone());
                        tokio::spawn(async move { run_login_http(&r, &sh2, &registry, &user, pass).await });
                        continue;
                    }
                    Cmd::Sys { kind, script, root } => {
                        let (r, sh2) = (sys.clone(), sh.clone());
                        tokio::spawn(async move { sys_load(&r, &sh2, kind, &script, root).await });
                        continue;
                    }
                    Cmd::Act(Action::Sys(job)) => {
                        let (r, sh2) = (sys.clone(), sh.clone());
                        tokio::spawn(async move { sys_job(&r, &sh2, job).await });
                        continue;
                    }
                    Cmd::Ct { kind, tag, args, merge } => {
                        let (r, sh2) = (runner.clone(), sh.clone());
                        tokio::spawn(async move { ct_load(&r, &sh2, kind, tag, args, merge).await });
                        continue;
                    }
                    Cmd::Db { tag, args, input } => {
                        let (r, sh2) = (runner.clone(), sh.clone());
                        tokio::spawn(async move { db_exec(&r, &sh2, tag, args, input).await });
                        continue;
                    }
                    Cmd::Shell { tag, script } => {
                        let (r, sh2) = (runner.clone(), sh.clone());
                        tokio::spawn(async move {
                            let res = match sys_exec(&r, &script, false, None, Duration::from_secs(20)).await {
                                Ok(o) if o.code == 0 => Ok(o.stdout),
                                Ok(o) => Err(last_line(&o)),
                                Err(e) => Err(e),
                            };
                            sh2.db.lock().unwrap().insert(tag, res);
                            sh2.bump();
                        });
                        continue;
                    }
                    Cmd::HostSh { tag, script, secs } => {
                        let (r, sh2) = (runner.clone(), sh.clone());
                        tokio::spawn(async move {
                            let t = Duration::from_secs(secs);
                            let res = if matches!(r.sudo, Sudo::No) { sys_exec(&r, &script, false, None, t).await } else { r.root(&script, None, t).await };
                            let res = match res {
                                Ok(o) if o.code == 0 => Ok(o.stdout),
                                Ok(o) => Err(last_line(&o)),
                                Err(e) => Err(e),
                            };
                            sh2.db.lock().unwrap().insert(tag, res);
                            sh2.bump();
                        });
                        continue;
                    }
                    Cmd::Fetch { tag, remote, local } => {
                        let (r, sh2) = (sys.clone(), sh.clone());
                        tokio::spawn(async move {
                            let res = fetch_file(&r, &sh2, &tag, &remote, &local).await;
                            sh2.fetch.lock().unwrap().remove(&tag);
                            sh2.db.lock().unwrap().insert(tag, res);
                            sh2.bump();
                        });
                        continue;
                    }
                    Cmd::CtFs { name, sh: fsh, cmd } => {
                        let r = runner.clone();
                        tokio::spawn(async move { ctfs_run(&r, &name, &fsh, cmd).await });
                        continue;
                    }
                    Cmd::Act(a) => {
                        let (r, sh2) = (runner.clone(), sh.clone());
                        tokio::spawn(async move { run_action(&r, &sh2, a).await });
                        // 操作后很快刷新一次
                        pol.last_ps = None;
                        pol.last_df = None;
                    }
                }
            }
            _ = tick.tick() => {}
        }
        if sh.closed.load(Ordering::Relaxed) {
            break;
        }
        let watched = now_ms().saturating_sub(sh.touched.load(Ordering::Relaxed)) < WATCH_TTL_MS;
        if !watched {
            continue;
        }
        let due = |t: &Option<Instant>, every: Duration| t.map(|t| t.elapsed() >= every).unwrap_or(true);
        if due(&pol.last_ps, POLL_PS) {
            pol.last_ps = Some(Instant::now());
            if let Err(e) = refresh_ps(&runner, &sh).await {
                *sh.problem.lock().unwrap() = Some(classify(&e));
                logs::push(logs::Level::Warn, label, format!("Docker 刷新失败:{e}"));
                sh.bump();
            }
        }
        if due(&pol.last_stats, POLL_STATS) {
            pol.last_stats = Some(Instant::now());
            if let Err(e) = refresh_stats(&runner, &sh).await {
                logs::push(logs::Level::Warn, label, format!("Docker 资源统计失败:{e}"));
            }
        }
        if sh.want_volume_sizes.load(Ordering::Relaxed) && due(&pol.last_df, POLL_DF) {
            pol.last_df = Some(Instant::now());
            if let Err(e) = refresh_df(&runner, &sh).await {
                logs::push(logs::Level::Warn, label, format!("Docker 磁盘统计失败:{e}"));
            }
        }
    }
    drop(session);
    Ok(())
}

/// 资源监控:界面在看时每 2 秒采一次样
async fn monitor_task(session: Arc<SshHandle<Client>>, sh: Arc<Shared>) {
    use crate::monitor::{INTERVAL, SAMPLE_CMD, State, parse_sample};
    let mut st = State::default();
    let mut tick = tokio::time::interval(INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if sh.closed.load(Ordering::Relaxed) {
            break;
        }
        if now_ms().saturating_sub(sh.touched.load(Ordering::Relaxed)) >= WATCH_TTL_MS {
            continue;
        }
        let out = match exec(&session, SAMPLE_CMD, None, Duration::from_secs(15)).await {
            Ok(o) => o,
            Err(_) => continue,
        };
        match parse_sample(&out.stdout) {
            Ok(raw) => {
                if let Some(snap) = st.feed(raw, Instant::now()) {
                    *sh.monitor.lock().unwrap() = Some(Arc::new(snap));
                    sh.bump();
                }
            }
            Err(e) => {
                let msg = if e == "unsupported" { "这台服务器不是 Linux,暂时不能监控系统资源".to_string() } else { e };
                *sh.monitor_err.lock().unwrap() = Some(msg);
                sh.bump();
                break;
            }
        }
    }
}

/// compose 项目名:小写字母、数字、`-`、`_`,以字母或数字开头
pub fn valid_project(s: &str) -> bool {
    let mut ch = s.chars();
    ch.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) && s.len() <= 63 && ch.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

/// 新建堆栈:先用当前用户建目录写文件,没权限再用管理员权限;写好后 `docker compose up -d`
async fn run_create(r: &Runner, sh: &Shared, project: &str, dir: &str, text: &str, env: &str) {
    let key = format!("创建:{project}");
    let fail = |msg: String| {
        logs::push(logs::Level::Info, "Docker", msg.clone());
        *sh.notice.lock().unwrap() = Some(msg);
        sh.bump();
    };
    if !valid_project(project) {
        return fail("创建失败:堆栈名只能用小写字母、数字、- 和 _".into());
    }
    if !dir.starts_with('/') || dir.chars().any(|c| c.is_control()) {
        return fail("创建失败:存储地址要是以 / 开头的绝对路径".into());
    }
    let dir = dir.trim_end_matches('/');
    let file = format!("{dir}/docker-compose.yml");
    let q = shell_quote(&file);
    let script = format!(
        "f={q}; if [ -e \"$f\" ]; then echo '这个目录里已经有 docker-compose.yml' >&2; exit 1; fi; mkdir -p -- {} && cat > \"$f\"",
        shell_quote(dir)
    );
    sh.busy.lock().unwrap().insert(key.clone());
    sh.bump();
    let mut root = false;
    let mut res = sys_exec(r, &script, false, Some(text), Duration::from_secs(30)).await;
    if matches!(&res, Ok(o) if o.code != 0 && o.stderr.to_lowercase().contains("ermission")) {
        root = true;
        res = sys_exec(r, &script, true, Some(text), Duration::from_secs(30)).await;
    }
    // 有 .env 内容就一起写(和 compose 文件同一个身份,这样两个文件属主一致)
    if !env.is_empty() && matches!(&res, Ok(o) if o.code == 0) {
        let envq = shell_quote(&format!("{dir}/.env"));
        // .env 里常放密码:只有属主能读写
        res = sys_exec(r, &format!("umask 077; cat > {envq}"), root, Some(env), Duration::from_secs(30)).await;
    }
    let msg = match res {
        Ok(o) if o.code == 0 => {
            let a = Action::ComposeUp { project: project.to_string(), dir: dir.to_string(), files: vec![file.clone()], envs: Vec::new(), pull: false };
            match r.docker(&a.args(), Duration::from_secs(600)).await {
                Ok(o) if o.code == 0 => format!("已创建:{project}"),
                Ok(o) => format!("创建失败:文件已写入 {file},但启动失败:{}", o.stderr.trim().lines().last().unwrap_or("未知错误")),
                Err(e) => format!("创建失败:文件已写入 {file},但启动失败:{e}"),
            }
        }
        Ok(o) => format!("创建失败:{}", last_line(&o)),
        Err(e) => format!("创建失败:{e}"),
    };
    sh.busy.lock().unwrap().remove(&key);
    fail(msg);
}

async fn run_action(r: &Runner, sh: &Shared, a: Action) {
    if let Action::ConfirmInsecure(host) = &a {
        run_insecure(r, sh, host).await;
        return;
    }
    if matches!(a, Action::InstallDocker { .. }) {
        return;
    }
    if let Action::SaveDaemon(text) | Action::SaveRestartDaemon(text) = &a {
        run_daemon(r, sh, text, matches!(a, Action::SaveRestartDaemon(_)), a.verb()).await;
        return;
    }
    if let Action::ComposeCreate { project, dir, text, env } = &a {
        run_create(r, sh, project, dir, text, env).await;
        return;
    }
    let prune = matches!(a, Action::PruneImages | Action::PruneVolumes);
    if !prune && !safe_name(a.target()) {
        *sh.notice.lock().unwrap() = Some(format!("{}失败:名称「{}」含有不支持的字符", a.verb(), a.target()));
        sh.bump();
        return;
    }
    let key = format!("{}:{}", a.verb(), a.target());
    sh.busy.lock().unwrap().insert(key.clone());
    sh.bump();
    let timeout = if matches!(a, Action::Pull(_) | Action::PruneImages | Action::ComposeUp { .. } | Action::ComposeCreate { .. }) { Duration::from_secs(600) } else { Duration::from_secs(60) };
    let res = r.docker(&a.args(), timeout).await;
    // 先把列表刷新到最新再撤掉"进行中",否则动画结束后要等下一轮轮询才看到变化
    let _ = refresh_ps(r, sh).await;
    sh.busy.lock().unwrap().remove(&key);
    let msg = match res {
        Ok(o) if o.code == 0 && prune => {
            // docker 会在最后一行报「Total reclaimed space: 1.2GB」,带上这个数
            let freed = o.stdout.lines().rev().find_map(|l| l.split_once("reclaimed space:").map(|(_, v)| v.trim().to_string()));
            match freed {
                Some(f) => format!("已{},释放 {f}", a.verb()),
                None => format!("已{}", a.verb()),
            }
        }
        Ok(o) if o.code == 0 => format!("已{}:{}", a.verb(), a.target()),
        Ok(o) => format!("{}失败:{}", a.verb(), o.stderr.trim().lines().last().unwrap_or("未知错误")),
        Err(e) => format!("{}失败:{e}", a.verb()),
    };
    logs::push(logs::Level::Info, "Docker", msg.clone());
    // 容器的启动/停止/重启/删除成功时,按钮上的动画和列表变化已经说明了结果,不再弹提示;失败仍提示
    let quiet_ok = matches!(a, Action::Start(_) | Action::Stop(_) | Action::Restart(_) | Action::Remove(_)) && msg.starts_with('已');
    if !quiet_ok {
        *sh.notice.lock().unwrap() = Some(msg);
    }
    sh.bump();
    if matches!(a, Action::Logout(_)) {
        load_registries(r, sh).await;
    }
}

// ---- 演示数据(--demo / --shot):和真实快照同一个结构,界面只读 Snapshot ----------------

/// 用 mock 的堆栈拼一份快照,外加几条镜像和存储卷,让各页在演示和截图里有内容
pub fn demo_snapshot(stacks: &[Stack]) -> Snapshot {
    let mut snap = Snapshot::default();
    // 调试:OTTER_DBM 截图时补一个 PostgreSQL 容器,数据库页才有实例可看
    if std::env::var_os("OTTER_DBM").is_some() {
        snap.containers.push(Container { id: "demo-pg".into(), name: "demo-postgres".into(), image: "postgres:16".into(), state: "running".into(), status: "Up 3 days".into(), ports: vec!["127.0.0.1:5432->5432".into()], project: None, service: None, panel: None, mounts: vec![] });
    }
    for (si, s) in stacks.iter().enumerate() {
        for (i, svc) in s.services.iter().enumerate() {
            let id = format!("{:012x}", (si * 100 + i + 1) * 7919);
            snap.containers.push(Container {
                id: id.clone(),
                name: svc.name.clone(),
                image: svc.image.clone(),
                state: if svc.state == Running::Up { "running".into() } else { "exited".into() },
                status: if svc.state == Running::Up { format!("Up {}", svc.uptime.replace("天", " days").replace("小时", " hours")) } else { "Exited (137) 2 hours ago".into() },
                ports: svc.ports.clone(),
                project: Some(s.name.clone()),
                service: Some(svc.name.clone()),
                panel: None,
                mounts: vec![format!("{}-data", svc.name)],
            });
            snap.stats.insert(id.clone(), Stats { cpu: svc.cpu_v, mem: (svc.mem_mb * 1e6) as u64, net: 0, disk: 0 });
            let mut se = Series::default();
            for k in 0..SPARK_N {
                se.cpu.push_back(svc.cpu_s[k]);
                se.mem.push_back(svc.mem_s[k]);
                se.net.push_back(svc.net_s[k] * 2e6);
                se.disk.push_back(svc.disk_s[k] * 5e5);
            }
            snap.series.insert(id, se);
        }
    }
    for (i, (repo, tag, size)) in [("nginx", "1.27", "192MB"), ("redis", "7", "117MB"), ("postgres", "16", "431MB"), ("grafana/grafana", "11", "466MB"), ("traefik", "v3", "181MB"), ("busybox", "latest", "4.3MB")].iter().enumerate() {
        snap.images.push(Image { id: format!("{:012x}", (i + 1) * 104729), repo: repo.to_string(), tag: tag.to_string(), size: size.to_string(), size_bytes: parse_size(size), created: format!("{} 周前", i + 1) });
    }
    for c in &snap.containers {
        for m in &c.mounts {
            if !snap.volumes.iter().any(|v| &v.name == m) {
                snap.volumes.push(Volume { name: m.clone(), driver: "local".into(), size: Some(format!("{} MB", 12 + snap.volumes.len() * 7)) });
            }
        }
    }
    snap.fetched_ms = now_ms();
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_up_passes_all_files_and_envs() {
        let a = Action::ComposeUp { project: "p".into(), dir: "/d".into(), files: vec!["/d/a.yml".into(), "/d/b.yml".into()], envs: vec!["/d/.env.prod".into()], pull: true };
        assert_eq!(
            a.args().join(" "),
            "compose -p p --project-directory /d -f /d/a.yml -f /d/b.yml --env-file /d/.env.prod up -d --pull always"
        );
    }

    const PS: &str = "abc123def456\tdemo-web\tnginx:alpine\trunning\tUp 2 hours\t0.0.0.0:8080->80/tcp, [::]:8080->80/tcp, 443/tcp\tdemo\tweb\t\tdemo-data\n\
                      111111111111\tdemo-exited\tbusybox\texited\tExited (3) 5 minutes ago\t\t\t\t\t\n\
                      222222222222\tapp\tgrafana\trunning\tUp About an hour (healthy)\t\t1panel-app\t\tApps\t/data/a,vol1\n";

    #[::core::prelude::v1::test]
    fn parses_ps_with_ports_labels_and_panel() {
        let v = parse_ps(PS);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].id, "abc123def456");
        assert_eq!(v[0].ports, vec!["8080->80/tcp", "443/tcp"]);
        assert_eq!(v[0].project.as_deref(), Some("demo"));
        assert_eq!(v[0].service.as_deref(), Some("web"));
        assert_eq!(v[0].mounts, vec!["demo-data"]);
        assert!(v[0].running() && v[0].panel.is_none());
        assert!(!v[1].running() && v[1].project.is_none() && v[1].ports.is_empty());
        assert_eq!(v[2].panel, Some("1Panel"));
        assert_eq!(v[2].mounts, vec!["/data/a", "vol1"]);
        assert!(parse_ps("").is_empty());
        assert!(parse_ps("garbage line").is_empty());
    }

    #[::core::prelude::v1::test]
    fn parses_sizes_and_stats() {
        assert_eq!(parse_size("12.5MiB"), 13_107_200);
        assert_eq!(parse_size("1.2kB"), 1200);
        assert_eq!(parse_size("3GB"), 3_000_000_000);
        assert_eq!(parse_size("0B"), 0);
        assert_eq!(parse_size("--"), 0);
        let s = parse_stats("abc123def456\t0.05%\t12.5MiB / 7.6GiB\t1.2kB / 3.4kB\t0B / 8.19kB\n");
        let st = s["abc123def456"];
        assert!((st.cpu - 0.05).abs() < 1e-6);
        assert_eq!(st.mem, 13_107_200);
        assert_eq!(st.net, 4600);
        assert_eq!(st.disk, 8190);
        assert!(parse_stats("a\tb").is_empty());
    }

    #[::core::prelude::v1::test]
    fn parses_registry_logins() {
        // "alice:secret" → YWxpY2U6c2VjcmV0
        let cfg = r#"{"auths":{"https://index.docker.io/v1/":{"auth":"YWxpY2U6c2VjcmV0"},"ghcr.io":{},"reg.example.com:5000":{"username":"bob"}},"credHelpers":{"gcr.io":"gcloud"}}"#;
        let r = parse_registries(cfg).unwrap();
        let get = |h: &str| r.iter().find(|x| x.host == h).unwrap().user.clone();
        assert_eq!(get("https://index.docker.io/v1/").as_deref(), Some("alice"));
        assert_eq!(get("ghcr.io"), None);
        assert_eq!(get("reg.example.com:5000").as_deref(), Some("bob"));
        assert_eq!(get("gcr.io"), None);
        assert_eq!(r.len(), 4);
        assert!(parse_registries("").unwrap().is_empty());
        assert!(parse_registries("{").is_err());
        assert_eq!(auth_user("!!!"), None);
    }

    #[::core::prelude::v1::test]
    fn parses_images_volumes_and_df() {
        let imgs = parse_images("sha256:0123456789abcdef\tnginx\talpine\t73.2MB\t2 weeks ago\nffff\t<none>\t<none>\t1.2GB\t3 months ago\n");
        assert_eq!(imgs[0].id, "0123456789ab");
        assert_eq!(imgs[0].size_bytes, 73_200_000);
        assert!(imgs[0].matches("nginx:alpine") && !imgs[0].matches("nginx"));
        assert!(imgs[1].matches("ffff"));
        let vols = parse_volumes("demo-data\tlocal\n\tlocal\n");
        assert_eq!(vols.len(), 1);
        let df = "Images space usage:\n\nREPOSITORY  TAG\n\nLocal Volumes space usage:\n\nVOLUME NAME   LINKS   SIZE\ndemo-data     1       1.5MB\nother         0       0B\n\nBuild cache usage: 0B\n";
        let m = parse_df_volumes(df);
        assert_eq!(m["demo-data"], "1.5MB");
        assert_eq!(m["other"], "0B");
    }

    #[::core::prelude::v1::test]
    fn classifies_errors() {
        assert_eq!(classify("sh: 1: docker: not found"), Problem::NoDocker);
        assert_eq!(classify("bash: 行 1: docker: 未找到命令"), Problem::NoDocker);
        assert_eq!(classify("bash: docker: 找不到命令"), Problem::NoDocker);
        assert!(matches!(classify("permission denied while trying to connect to the Docker daemon socket"), Problem::NoPermission(_)));
        assert!(matches!(classify("Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?"), Problem::DaemonDown(_)));
        assert!(matches!(classify("weird"), Problem::Other(_)));
    }

    #[::core::prelude::v1::test]
    fn validates_names_and_quotes() {
        assert!(safe_name("nginx:1.27") && safe_name("grafana/grafana:11") && safe_name("my_app-1"));
        assert!(!safe_name("") && !safe_name("-rf") && !safe_name("a b") && !safe_name("a;b") && !safe_name("$(x)") && !safe_name("a'b"));
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }

    #[::core::prelude::v1::test]
    fn groups_stacks_and_keeps_expanded() {
        let mut snap = Snapshot { containers: parse_ps(PS), ..Default::default() };
        snap.stats.insert("abc123def456".into(), Stats { cpu: 2.5, mem: 50_000_000, net: 0, disk: 0 });
        let mut ex = HashMap::new();
        ex.insert("demo".to_string(), true);
        let stacks = to_stacks(&snap, &ex);
        let names: Vec<_> = stacks.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["1panel-app", "demo", "独立容器"]);
        assert!(stacks[1].expanded && !stacks[0].expanded);
        assert_eq!(stacks[1].services[0].cpu, "2.5%");
        assert_eq!(stacks[2].services[0].cpu, "—");
    }

    #[::core::prelude::v1::test]
    fn series_rate_and_normalization() {
        let mut s = Series::default();
        let t0 = Instant::now();
        s.push(Stats { cpu: 1.0, mem: 100, net: 0, disk: 0 }, t0);
        s.push(Stats { cpu: 2.0, mem: 100, net: 10_000, disk: 2_000 }, t0 + Duration::from_secs(10));
        assert_eq!(s.net.back().copied(), Some(1000.0));
        assert_eq!(s.disk.back().copied(), Some(200.0));
        let a = spark_of(&s.cpu, 1.0);
        assert_eq!(a[SPARK_N - 1], 1.0);
        assert_eq!(a[SPARK_N - 2], 0.5);
        assert_eq!(a[0], 0.0);
    }

    #[::core::prelude::v1::test]
    fn formats_values() {
        assert_eq!(fmt_bytes(188_000_000), "188 MB");
        assert_eq!(fmt_bytes(1_300_000_000), "1.3 GB");
        assert_eq!(fmt_rate(22_700_000.0), "22.7 MB/s");
        let c = |state: &str, status: &str| Container { state: state.into(), status: status.into(), ..Default::default() };
        assert_eq!(uptime_text(&c("running", "Up 2 hours")), "2小时");
        assert_eq!(uptime_text(&c("running", "Up About an hour (healthy)")), "约 1小时");
        assert_eq!(uptime_text(&c("exited", "Exited (0) 1 day ago")), "已退出");
    }

    #[::core::prelude::v1::test]
    fn sudo_failure_reasons() {
        assert!(sudo_failure("-bash: sudo: 未找到命令", true, "scm").contains("没有安装 sudo"));
        assert!(sudo_failure("scm is not in the sudoers file.", true, "scm").contains("sudoers"));
        assert!(sudo_failure("Sorry, try again.", true, "scm").contains("拒绝了节点里保存的密码"));
        assert!(sudo_failure("sudo: a password is required", false, "scm").contains("没有保存"));
    }

    #[::core::prelude::v1::test]
    fn attention_list() {
        let snap = Snapshot { containers: parse_ps(PS), ..Default::default() };
        let names: Vec<_> = snap.attention().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["demo-exited"]);
    }

    /// 真机端到端:OTTER_TEST_DOCKER=user:password@host(可选 OTTER_TEST_DOCKER_SKIP_ACTIONS=1)
    #[::core::prelude::v1::test]
    fn end_to_end_against_real_docker() {
        let Ok(spec) = std::env::var("OTTER_TEST_DOCKER") else { return };
        let (cred, host) = spec.rsplit_once('@').unwrap();
        let (user, pw) = cred.split_once(':').unwrap();
        let mut conn = Conn::parse(&format!("{user}@{host}"));
        conn.auth.agent = false;
        conn.auth.password = Some(zeroize::Zeroizing::new(pw.to_string()));
        let d = Docker::connect(conn, None);
        let wait = |cond: &dyn Fn() -> bool, what: &str| {
            for _ in 0..400 {
                if cond() {
                    return;
                }
                if let Status::HostPrompt(_) = d.status() {
                    d.answer_host(true);
                }
                if let Status::Failed(e) = d.status() {
                    panic!("连接失败:{e}");
                }
                d.touch();
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!("等待超时:{what};状态 {:?} 问题 {:?}", d.status(), d.problem());
        };
        wait(&|| matches!(d.status(), Status::Ready) && (d.snapshot().is_some() || d.problem().is_some()), "就绪");
        if let Some(p) = d.problem() {
            println!("PROBLEM: {p:?}");
            return;
        }
        wait(&|| d.snapshot().is_some_and(|s| !s.stats.is_empty()), "stats");
        let s = d.snapshot().unwrap();
        println!("containers={} running={} images={} volumes={} stats={}", s.containers.len(), s.running(), s.images.len(), s.volumes.len(), s.stats.len());
        for c in &s.containers {
            println!("  {} {} {} {:?} panel={:?} mounts={:?}", c.name, c.state, c.status, c.ports, c.panel, c.mounts);
        }
        assert!(!s.containers.is_empty());
        if std::env::var("OTTER_TEST_DOCKER_SKIP_ACTIONS").is_ok() {
            return;
        }
        let name = s.containers.iter().find(|c| c.running() && c.panel.is_none()).map(|c| c.name.clone()).expect("一个运行中的容器");
        d.act(Action::Stop(name.clone()));
        wait(&|| d.snapshot().is_some_and(|s| s.containers.iter().any(|c| c.name == name && !c.running())), "停止");
        d.act(Action::Start(name.clone()));
        wait(&|| d.snapshot().is_some_and(|s| s.containers.iter().any(|c| c.name == name && c.running())), "启动");
        d.act(Action::Restart(name.clone()));
        std::thread::sleep(Duration::from_secs(3));
        wait(&|| d.snapshot().is_some_and(|s| s.containers.iter().any(|c| c.name == name && c.running())), "重启后运行");
        d.act(Action::Pull("busybox:1.36".into()));
        wait(&|| d.snapshot().is_some_and(|s| s.images.iter().any(|i| i.repo == "busybox" && i.tag == "1.36")), "拉取");
        d.act(Action::RemoveImage("busybox:1.36".into()));
        wait(&|| d.snapshot().is_some_and(|s| !s.images.iter().any(|i| i.repo == "busybox" && i.tag == "1.36")), "删除镜像");
        d.want_volume_sizes(true);
        wait(&|| d.snapshot().is_some_and(|s| s.volumes.iter().any(|v| v.size.is_some())), "卷体积");
        println!("{}", d.take_notice().unwrap_or_default());
    }
}
