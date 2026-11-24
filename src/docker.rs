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
            Action::ConfirmInsecure(_) | Action::Sys(_) => Vec::new(),
        }
    }
    pub fn target(&self) -> &str {
        match self {
            Action::Start(n) | Action::Stop(n) | Action::Restart(n) | Action::Remove(n) | Action::Pull(n) | Action::RemoveImage(n) | Action::RemoveVolume(n) | Action::Logout(n) | Action::ConfirmInsecure(n) => n,
            Action::Sys(j) => &j.target,
            Action::PruneImages => "未使用的镜像",
            Action::PruneVolumes => "未使用的存储卷",
            Action::SaveDaemon(_) | Action::SaveRestartDaemon(_) => "daemon.json",
        }
    }
    fn verb(&self) -> &'static str {
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
    /// 等用户确认的 HTTP 仓库登录:(仓库, 用户名, 密码),确认或取消后清掉
    pending_login: Mutex<Option<(String, String, zeroize::Zeroizing<String>)>>,
    /// 需要向用户确认的 HTTP 仓库地址;界面取走后清空
    insecure_ask: Mutex<Option<String>>,
    /// 界面每次需要重绘时递增
    version: AtomicU64,
    /// 界面最后一次表示「我还在看」的时间(毫秒时间戳);太久没人看就暂停轮询
    touched: AtomicU64,
    want_volume_sizes: AtomicBool,
    closed: AtomicBool,
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
            notice: Mutex::new(None),
            daemon: Mutex::new(None),
            registries: Mutex::new(None),
            sys: Mutex::new(HashMap::new()),
            pending_login: Mutex::new(None),
            insecure_ask: Mutex::new(None),
            version: AtomicU64::new(1),
            touched: AtomicU64::new(now_ms()),
            want_volume_sizes: AtomicBool::new(false),
            closed: AtomicBool::new(false),
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

    /// 请求读一份运维数据;结果用 `sys_take` 取
    pub fn sys_load(&self, kind: crate::sysops::Kind, script: &str, root: bool) {
        let _ = self.cmd.send(Cmd::Sys { kind, script: script.to_string(), root });
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

pub(crate) async fn exec(session: &SshHandle<Client>, cmd: &str, stdin: Option<&[u8]>, timeout: Duration) -> Result<Output, String> {
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
        Ok::<_, String>(Output { code: code.unwrap_or(255), stdout: String::from_utf8_lossy(&out).into_owned(), stderr: String::from_utf8_lossy(&err).into_owned() })
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
                exec(&self.session, &format!("sudo -S -p '' docker {quoted}"), Some(data.as_bytes()), timeout).await
            }
        }
    }
}

impl Runner {
    /// 以 root 身份跑一段 shell:已经是 root 就直接跑,否则走 sudo(免密,或用节点保存的密码)。`input` 通过 stdin 传给脚本。
    async fn root(&self, script: &str, input: Option<&str>, timeout: Duration) -> Result<Output, String> {
        let q = shell_quote(script);
        match self.sudo {
            Sudo::WithPassword => {
                let pw = self.password.as_deref().map(String::as_str).unwrap_or("");
                let data = format!("{pw}\n{}", input.unwrap_or(""));
                exec(&self.session, &format!("sudo -S -p '' sh -c {q}"), Some(data.as_bytes()), timeout).await
            }
            _ => exec(&self.session, &format!("if [ \"$(id -u)\" = 0 ]; then sh -c {q}; else sudo -n sh -c {q}; fi"), input.map(str::as_bytes), timeout).await,
        }
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

fn last_line(o: &Output) -> String {
    let t = if o.stderr.trim().is_empty() { &o.stdout } else { &o.stderr };
    t.trim().lines().last().unwrap_or("未知错误").trim().to_string()
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
    if low.contains("command not found") || low.contains("not found") && low.contains("docker") {
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
    if let Ok(o) = r.docker(&ver, t).await {
        if o.code == 0 {
            logs::push(logs::Level::Info, &conn.host, "Docker:当前用户没有权限,改用 sudo");
            return Ok(r);
        }
    }
    if let Some(pw) = &conn.auth.password {
        r.sudo = Sudo::WithPassword;
        r.password = Some(pw.clone());
        if let Ok(o) = r.docker(&ver, t).await {
            if o.code == 0 {
                logs::push(logs::Level::Info, &conn.host, "Docker:当前用户没有权限,改用 sudo(密码)");
                return Ok(r);
            }
        }
    }
    Err(Problem::NoPermission("当前用户不能运行 docker,sudo 也不可用(需要免密 sudo,或在节点里保存密码)".into()))
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
            logs::push(logs::Level::Info, label, "Docker:复用终端的连接");
            s
        }
        None => {
            let s = Arc::new(open_authenticated(conn, io).await?);
            logs::push(logs::Level::Info, label, "Docker:认证成功");
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
    logs::push(logs::Level::Info, label, "Docker 已就绪");

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

async fn run_action(r: &Runner, sh: &Shared, a: Action) {
    if let Action::ConfirmInsecure(host) = &a {
        run_insecure(r, sh, host).await;
        return;
    }
    if let Action::SaveDaemon(text) | Action::SaveRestartDaemon(text) = &a {
        run_daemon(r, sh, text, matches!(a, Action::SaveRestartDaemon(_)), a.verb()).await;
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
    let timeout = if matches!(a, Action::Pull(_) | Action::PruneImages) { Duration::from_secs(600) } else { Duration::from_secs(60) };
    let res = r.docker(&a.args(), timeout).await;
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
    *sh.notice.lock().unwrap() = Some(msg);
    sh.bump();
    if matches!(a, Action::Logout(_)) {
        load_registries(r, sh).await;
    }
}

// ---- 演示数据(--demo / --shot):和真实快照同一个结构,界面只读 Snapshot ----------------

/// 用 mock 的堆栈拼一份快照,外加几条镜像和存储卷,让各页在演示和截图里有内容
pub fn demo_snapshot(stacks: &[Stack]) -> Snapshot {
    let mut snap = Snapshot::default();
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
