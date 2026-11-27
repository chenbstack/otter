//! 服务器资源监控:经 SSH 每 2 秒执行一条只读的 shell,读 `/proc` 和 `df`、算出 CPU、内存、磁盘、网络、进程。
//! 服务器上不装任何东西。目前只支持 Linux(没有 `/proc/stat` 就报「不支持」)。
//!
//! 采样脚本一次输出多段(`##名字` 开头),解析是纯函数(见 `parse_sample`、`compute`,有测试)。
//! CPU、网络、磁盘 IO 在 `/proc` 里都是累计值,本地用前后两次的差值除以时间得到速率。

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

/// 一条折线保留的点数(2 秒一个点,约 5 分钟)
pub const HISTORY: usize = 150;
pub const INTERVAL: Duration = Duration::from_secs(2);

/// 采样命令。用 `awk` 把每个进程的 `/proc/<pid>/stat` 压成「pid、名字、cpu 时间、rss 页数」。
pub const SAMPLE_CMD: &str = r#"export LC_ALL=C
[ -r /proc/stat ] || { echo '##unsupported'; exit 0; }
echo '##host'; hostname 2>/dev/null; uname -sr 2>/dev/null; (. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME"); grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2
echo '##stat'; grep '^cpu' /proc/stat
echo '##mem'; grep -E '^(MemTotal|MemFree|MemAvailable|Buffers|Cached|SwapTotal|SwapFree):' /proc/meminfo
echo '##net'; cat /proc/net/dev
echo '##disk'; cat /proc/diskstats
echo '##load'; cat /proc/loadavg /proc/uptime
echo '##df'; (df -P -B1 -x tmpfs -x devtmpfs -x squashfs -x overlay -x efivarfs 2>/dev/null || df -P 2>/dev/null)
echo '##temp'; cat /sys/class/thermal/thermal_zone0/temp 2>/dev/null
echo '##pinfo'; ps -eo pid=,user:20=,nlwp=,args= 2>/dev/null | cut -c1-260
echo '##ps'; cat /proc/[0-9]*/stat 2>/dev/null | awk '{i=index($0,"(");j=0;for(k=length($0);k>0;k--)if(substr($0,k,1)==")"){j=k;break};if(j==0)next;n=split(substr($0,j+2),a," ");print substr($0,1,i-2)"\t"substr($0,i+1,j-i-1)"\t"(a[12]+a[13])"\t"a[22]}'
"#;

/// `/proc/stat` 一行 cpu 的各项计数
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct CpuTimes {
    pub user: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub steal: u64,
    pub total: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiskUse {
    pub mount: String,
    /// 设备名,如 /dev/sda1
    pub dev: String,
    pub total: u64,
    pub used: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Host {
    pub name: String,
    pub kernel: String,
    pub os: String,
    pub cpu_model: String,
}

/// 一次采样的原始数据(累计值)
#[derive(Clone, Debug, Default)]
pub struct Raw {
    pub host: Host,
    /// 第 0 项是总计,后面是每个核
    pub cpu: Vec<CpuTimes>,
    pub mem_total: u64,
    pub mem_free: u64,
    pub mem_avail: u64,
    pub mem_cache: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub disk_r: u64,
    pub disk_w: u64,
    pub disk_rio: u64,
    pub disk_wio: u64,
    /// 各整盘「忙」的毫秒数之和,和整盘个数
    pub disk_busy_ms: u64,
    pub disk_n: u64,
    pub load: [f32; 3],
    pub uptime: f64,
    pub disks: Vec<DiskUse>,
    pub temp_c: Option<f32>,
    /// pid → (名字, cpu 时钟数, rss 字节)
    pub procs: HashMap<u32, (String, u64, u64)>,
    /// pid → (用户, 线程数, 完整命令行);`ps` 不可用时是空的
    pub pinfo: HashMap<u32, (String, u32, String)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
    pub user: String,
    pub threads: u32,
    pub cmd: String,
    /// 最近几次的 CPU 占用(旧 → 新),画行尾的小图
    pub hist: Vec<f32>,
}

/// 算好的当前读数
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub host: Host,
    /// 总 CPU 占用 0..100,以及 user / system / iowait 三段
    pub cpu: f32,
    pub cpu_user: f32,
    pub cpu_sys: f32,
    pub cpu_wait: f32,
    pub cores: Vec<f32>,
    pub mem_total: u64,
    pub mem_used: u64,
    /// 真正没被用的内存(`MemFree`),和「可用」(含可回收缓存)不同
    pub mem_free: u64,
    pub mem_cache: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub net_rx: f32,
    pub net_tx: f32,
    pub disk_r: f32,
    pub disk_w: f32,
    /// 每秒读/写次数
    pub disk_riops: f32,
    pub disk_wiops: f32,
    /// 磁盘忙碌程度 0~100
    pub disk_util: f32,
    pub load: [f32; 3],
    pub uptime: f64,
    pub disks: Vec<DiskUse>,
    pub temp_c: Option<f32>,
    /// 按 CPU 从高到低,最多 `PROC_KEEP` 个
    pub procs: Vec<Proc>,
}

const PROC_KEEP: usize = 60;

/// 界面读的快照:当前读数 + 历史折线
#[derive(Clone, Debug, Default)]
pub struct MonSnapshot {
    pub now: Metrics,
    pub cpu_h: Vec<f32>,
    pub mem_h: Vec<f32>,
    /// 每个核心各自的占用历史
    pub cores_h: Vec<Vec<f32>>,
    /// 内存历史:每点 [已用, 缓存, 空闲] 字节数,三项相加等于总量
    pub mem_s: Vec<[f32; 3]>,
    pub rx_h: Vec<f32>,
    pub tx_h: Vec<f32>,
    pub rd_h: Vec<f32>,
    pub wr_h: Vec<f32>,
}

/// 内部状态:上一次的原始值 + 历史
#[derive(Default)]
pub struct State {
    prev: Option<(Raw, std::time::Instant)>,
    cpu_h: VecDeque<f32>,
    mem_h: VecDeque<f32>,
    cores_h: Vec<VecDeque<f32>>,
    mem_s: VecDeque<[f32; 3]>,
    rx_h: VecDeque<f32>,
    tx_h: VecDeque<f32>,
    rd_h: VecDeque<f32>,
    wr_h: VecDeque<f32>,
    proc_h: HashMap<u32, VecDeque<f32>>,
}

const PROC_HIST: usize = 32;

fn push<T>(q: &mut VecDeque<T>, v: T) {
    q.push_back(v);
    while q.len() > HISTORY {
        q.pop_front();
    }
}

impl State {
    /// 喂入一次新采样;第一次只记基线,返回 None
    pub fn feed(&mut self, raw: Raw, at: std::time::Instant) -> Option<MonSnapshot> {
        let out = self.prev.as_ref().and_then(|(p, t)| {
            let dt = at.duration_since(*t).as_secs_f32();
            (dt > 0.2).then(|| compute(p, &raw, dt))
        });
        self.prev = Some((raw, at));
        let mut m = out?;
        // 进程的 CPU 小图:每个进程记最近几次,不在列表里的清掉
        let live: std::collections::HashSet<u32> = m.procs.iter().map(|p| p.pid).collect();
        self.proc_h.retain(|pid, _| live.contains(pid));
        for p in &mut m.procs {
            let h = self.proc_h.entry(p.pid).or_default();
            h.push_back(p.cpu);
            while h.len() > PROC_HIST {
                h.pop_front();
            }
            p.hist = h.iter().copied().collect();
        }
        push(&mut self.cpu_h, m.cpu);
        if self.cores_h.len() != m.cores.len() {
            self.cores_h = vec![VecDeque::new(); m.cores.len()];
        }
        for (q, v) in self.cores_h.iter_mut().zip(&m.cores) {
            push(q, *v);
        }
        push(&mut self.mem_h, if m.mem_total > 0 { m.mem_used as f32 / m.mem_total as f32 * 100.0 } else { 0.0 });
        push(&mut self.mem_s, [m.mem_used as f32, m.mem_cache as f32, m.mem_free as f32]);
        push(&mut self.rx_h, m.net_rx);
        push(&mut self.tx_h, m.net_tx);
        push(&mut self.rd_h, m.disk_r);
        push(&mut self.wr_h, m.disk_w);
        Some(MonSnapshot { now: m, cpu_h: self.cpu_h.iter().copied().collect(), mem_h: self.mem_h.iter().copied().collect(), cores_h: self.cores_h.iter().map(|q| q.iter().copied().collect()).collect(), mem_s: self.mem_s.iter().copied().collect(), rx_h: self.rx_h.iter().copied().collect(), tx_h: self.tx_h.iter().copied().collect(), rd_h: self.rd_h.iter().copied().collect(), wr_h: self.wr_h.iter().copied().collect() })
    }
}

/// 解析一段采样输出。不是 Linux(没有 `/proc/stat`)返回 `Err("unsupported")`。
pub fn parse_sample(text: &str) -> Result<Raw, String> {
    let mut sections: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut cur = "";
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("##") {
            cur = name.trim();
            sections.entry(cur).or_default();
        } else if !cur.is_empty() {
            sections.entry(cur).or_default().push(line);
        }
    }
    if sections.contains_key("unsupported") {
        return Err("unsupported".into());
    }
    let get = |k: &str| sections.get(k).map(Vec::as_slice).unwrap_or(&[]);
    let mut raw = Raw::default();

    let h = get("host");
    raw.host = Host { name: h.first().unwrap_or(&"").trim().into(), kernel: h.get(1).unwrap_or(&"").trim().into(), os: h.get(2).unwrap_or(&"").trim().into(), cpu_model: h.get(3).unwrap_or(&"").trim().into() };

    for line in get("stat") {
        let mut it = line.split_whitespace();
        let Some(name) = it.next() else { continue };
        if !name.starts_with("cpu") {
            continue;
        }
        let v: Vec<u64> = it.filter_map(|x| x.parse().ok()).collect();
        if v.len() < 5 {
            continue;
        }
        // user nice system idle iowait irq softirq steal
        let g = |i: usize| v.get(i).copied().unwrap_or(0);
        let total: u64 = v.iter().take(8).sum();
        raw.cpu.push(CpuTimes { user: g(0) + g(1), system: g(2) + g(5) + g(6), idle: g(3), iowait: g(4), steal: g(7), total });
    }
    if raw.cpu.is_empty() {
        return Err("读不到 /proc/stat".into());
    }

    for line in get("mem") {
        let Some((k, rest)) = line.split_once(':') else { continue };
        let kb: u64 = rest.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0) * 1024;
        match k {
            "MemTotal" => raw.mem_total = kb,
            "MemFree" => raw.mem_free = kb,
            "MemAvailable" => raw.mem_avail = kb,
            "Buffers" | "Cached" => raw.mem_cache += kb,
            "SwapTotal" => raw.swap_total = kb,
            "SwapFree" => raw.swap_free = kb,
            _ => {}
        }
    }
    if raw.mem_avail == 0 {
        raw.mem_avail = raw.mem_free + raw.mem_cache;
    }

    for line in get("net") {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();
        if name == "lo" || ["veth", "docker", "br-", "virbr", "cni", "flannel", "cali", "tun", "tap"].iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if v.len() >= 9 {
            raw.net_rx += v[0];
            raw.net_tx += v[8];
        }
    }

    for line in get("disk") {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || !is_whole_disk(f[2]) {
            continue;
        }
        raw.disk_r += f[5].parse::<u64>().unwrap_or(0) * 512;
        raw.disk_w += f[9].parse::<u64>().unwrap_or(0) * 512;
        raw.disk_rio += f[3].parse::<u64>().unwrap_or(0);
        raw.disk_wio += f[7].parse::<u64>().unwrap_or(0);
        raw.disk_busy_ms += f.get(12).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        raw.disk_n += 1;
    }

    let l = get("load");
    if let Some(first) = l.first() {
        let v: Vec<f32> = first.split_whitespace().take(3).filter_map(|x| x.parse().ok()).collect();
        if v.len() == 3 {
            raw.load = [v[0], v[1], v[2]];
        }
    }
    raw.uptime = l.get(1).and_then(|s| s.split_whitespace().next()).and_then(|x| x.parse().ok()).unwrap_or(0.0);

    let mut seen: Vec<String> = Vec::new();
    for line in get("df").iter().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        // 文件系统 总量 已用 可用 容量% 挂载点(挂载点可能含空格,取第 6 列起)
        if f.len() < 6 {
            continue;
        }
        let (Ok(total), Ok(used)) = (f[1].parse::<u64>(), f[2].parse::<u64>()) else { continue };
        let mount = f[5..].join(" ");
        if total == 0 || skip_mount(&mount) || seen.iter().any(|d| d == f[0]) {
            continue;
        }
        seen.push(f[0].to_string());
        raw.disks.push(DiskUse { mount, dev: f[0].to_string(), total, used });
    }

    for line in get("pinfo") {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(user), Some(thr)) = (it.next(), it.next(), it.next()) else { continue };
        let (Ok(pid), Ok(thr)) = (pid.parse::<u32>(), thr.parse::<u32>()) else { continue };
        raw.pinfo.insert(pid, (user.to_string(), thr, it.collect::<Vec<_>>().join(" ")));
    }

    raw.temp_c = get("temp").first().and_then(|s| s.trim().parse::<f32>().ok()).map(|m| m / 1000.0).filter(|t| *t > 0.0 && *t < 150.0);

    for line in get("ps") {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 4 {
            continue;
        }
        let (Ok(pid), Ok(ticks), Ok(pages)) = (f[0].parse::<u32>(), f[2].parse::<u64>(), f[3].parse::<u64>()) else { continue };
        raw.procs.insert(pid, (f[1].to_string(), ticks, pages * 4096));
    }
    Ok(raw)
}

/// 整块磁盘(不算分区、loop、ram、dm)
fn is_whole_disk(name: &str) -> bool {
    let digits_end = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    for p in ["sd", "vd", "xvd", "hd"] {
        if let Some(rest) = name.strip_prefix(p) {
            return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_lowercase());
        }
    }
    if let Some(rest) = name.strip_prefix("nvme") {
        // nvme0n1(整盘),nvme0n1p1(分区)
        return rest.split_once('n').is_some_and(|(a, b)| digits_end(a) && digits_end(b));
    }
    if let Some(rest) = name.strip_prefix("mmcblk") {
        return digits_end(rest);
    }
    false
}

fn skip_mount(m: &str) -> bool {
    ["/run", "/snap", "/var/lib/docker", "/var/lib/containers", "/sys", "/proc", "/dev", "/boot/efi"].iter().any(|p| m == *p || m.starts_with(&format!("{p}/")))
}

/// 用前后两次采样算出速率和占用
pub fn compute(prev: &Raw, cur: &Raw, dt: f32) -> Metrics {
    let pct = |a: &CpuTimes, b: &CpuTimes| -> (f32, f32, f32, f32) {
        let total = b.total.saturating_sub(a.total) as f32;
        if total <= 0.0 {
            return (0.0, 0.0, 0.0, 0.0);
        }
        let idle = (b.idle + b.iowait).saturating_sub(a.idle + a.iowait) as f32;
        let f = |x: u64, y: u64| y.saturating_sub(x) as f32 / total * 100.0;
        (((total - idle) / total * 100.0).clamp(0.0, 100.0), f(a.user, b.user), f(a.system, b.system), f(a.iowait, b.iowait))
    };
    let (cpu, cpu_user, cpu_sys, cpu_wait) = match (prev.cpu.first(), cur.cpu.first()) {
        (Some(a), Some(b)) => pct(a, b),
        _ => (0.0, 0.0, 0.0, 0.0),
    };
    let cores: Vec<f32> = prev.cpu.iter().zip(&cur.cpu).skip(1).map(|(a, b)| pct(a, b).0).collect();
    let ncores = cores.len().max(1) as f32;
    let total_delta = cur.cpu.first().zip(prev.cpu.first()).map(|(b, a)| b.total.saturating_sub(a.total)).unwrap_or(0) as f32;

    let rate = |a: u64, b: u64| b.saturating_sub(a) as f32 / dt;

    let mut procs: Vec<Proc> = cur
        .procs
        .iter()
        .map(|(pid, (name, ticks, rss))| {
            let before = prev.procs.get(pid).map(|p| p.1).unwrap_or(*ticks);
            let cpu = if total_delta > 0.0 { ticks.saturating_sub(before) as f32 / total_delta * ncores * 100.0 } else { 0.0 };
            let (user, threads, cmd) = cur.pinfo.get(pid).cloned().unwrap_or_default();
            Proc { pid: *pid, name: name.clone(), cpu, mem: *rss, user, threads, cmd, hist: Vec::new() }
        })
        .collect();
    procs.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal).then(b.mem.cmp(&a.mem)));
    procs.truncate(PROC_KEEP);

    Metrics {
        host: cur.host.clone(),
        cpu,
        cpu_user,
        cpu_sys,
        cpu_wait,
        cores,
        mem_total: cur.mem_total,
        mem_used: cur.mem_total.saturating_sub(cur.mem_avail),
        mem_free: cur.mem_free,
        mem_cache: cur.mem_cache,
        swap_total: cur.swap_total,
        swap_used: cur.swap_total.saturating_sub(cur.swap_free),
        net_rx: rate(prev.net_rx, cur.net_rx),
        net_tx: rate(prev.net_tx, cur.net_tx),
        disk_r: rate(prev.disk_r, cur.disk_r),
        disk_w: rate(prev.disk_w, cur.disk_w),
        disk_riops: rate(prev.disk_rio, cur.disk_rio),
        disk_wiops: rate(prev.disk_wio, cur.disk_wio),
        disk_util: (cur.disk_busy_ms.saturating_sub(prev.disk_busy_ms) as f32 / (dt * 1000.0 * cur.disk_n.max(1) as f32) * 100.0).clamp(0.0, 100.0),
        load: cur.load,
        uptime: cur.uptime,
        disks: cur.disks.clone(),
        temp_c: cur.temp_c,
        procs,
    }
}

/// 字节数 → 「1.2 MB」
pub fn fmt_bytes(b: f64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (b.max(0.0), 0);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{v:.0} B") } else if v >= 100.0 { format!("{v:.0} {}", U[i]) } else { format!("{v:.1} {}", U[i]) }
}

pub fn fmt_rate(b: f32) -> String {
    format!("{}/s", fmt_bytes(b as f64))
}

/// 运行时长 → 「23 天 4 小时」
pub fn fmt_uptime(s: f64) -> String {
    let s = s as u64;
    let (d, h, m) = (s / 86400, s % 86400 / 3600, s % 3600 / 60);
    if d > 0 { format!("{d} 天 {h} 小时") } else if h > 0 { format!("{h} 小时 {m} 分") } else { format!("{m} 分钟") }
}

/// `--demo` 用的假数据:带起伏的历史曲线
pub fn demo_snapshot() -> MonSnapshot {
    // 调试:OTTER_PHASE=<0..30> 让曲线滚动一圈(首尾衔接),数字取曲线最新点,用于录官网动画
    let anim: Option<f32> = std::env::var("OTTER_PHASE").ok().and_then(|v| v.parse::<f32>().ok()).map(|k| k * std::f32::consts::TAU / 30.0);
    let ph0 = anim.unwrap_or(0.0);
    let wave = |base: f32, amp: f32, phase: f32| -> Vec<f32> { (0..HISTORY).map(|i| (base + amp * ((i as f32 * 0.21 + phase + ph0).sin() * 0.6 + (i as f32 * 0.07 + phase + ph0).cos() * 0.4)).max(0.0)).collect() };
    let cpu_h = wave(38.0, 22.0, 0.0);
    let mem_h = wave(62.0, 4.0, 1.0);
    let now = Metrics {
        host: Host { name: "dev-mac".into(), kernel: "Linux 6.1.0".into(), os: "Debian GNU/Linux 12".into(), cpu_model: "Intel Xeon Platinum 8369B".into() },
        cpu: *cpu_h.last().unwrap(),
        cpu_user: 24.0,
        cpu_sys: 9.0,
        cpu_wait: 3.0,
        cores: (0..std::env::var("OTTER_DEMO_CORES").ok().and_then(|v| v.parse().ok()).unwrap_or(16)).map(|i| (20.0 + 60.0 * ((i as f32 * 1.7).sin().abs())).min(98.0)).collect(),
        mem_total: 16 << 30,
        mem_used: (10.2 * (1u64 << 30) as f64) as u64,
        mem_free: (1.8 * (1u64 << 30) as f64) as u64,
        mem_cache: 3 << 30,
        swap_total: 2 << 30,
        swap_used: 200 << 20,
        net_rx: 1_300_000.0,
        net_tx: 240_000.0,
        disk_r: 3_200_000.0,
        disk_w: 900_000.0,
        disk_riops: 420.0,
        disk_wiops: 96.0,
        disk_util: 37.0,
        load: [1.42, 1.18, 0.96],
        uptime: 86400.0 * 23.0 + 3600.0 * 4.0,
        disks: vec![DiskUse { mount: "/".into(), dev: "/dev/nvme0n1p2".into(), total: 100 << 30, used: 58 << 30 }, DiskUse { mount: "/data".into(), dev: "/dev/nvme1n1".into(), total: 500 << 30, used: 462 << 30 }],
        temp_c: Some(52.0),
        procs: ["java", "mysqld", "nginx", "dockerd", "node", "redis-server", "sshd", "containerd", "systemd-journal", "python3"].iter().enumerate().map(|(i, n)| {
            let cpu = 42.0 / (i as f32 + 1.0);
            let cmds = ["java -Xmx2g -jar /opt/app/server.jar --spring.profiles.active=prod", "/usr/sbin/mysqld --defaults-file=/etc/mysql/my.cnf", "nginx: worker process", "/usr/bin/dockerd -H fd:// --containerd=/run/containerd/containerd.sock", "node /srv/api/dist/main.js", "redis-server *:6379", "sshd: root@pts/0", "/usr/bin/containerd", "/lib/systemd/systemd-journald", "python3 /opt/jobs/sync.py --daemon"];
            let users = ["app", "mysql", "www-data", "root", "app", "redis", "root", "root", "root", "app"];
            Proc { pid: 100 + i as u32 * 137, name: n.to_string(), cpu, mem: (1500 >> (i / 2)) * 1_048_576, user: users[i].into(), threads: [64, 38, 4, 21, 12, 5, 1, 17, 1, 3][i], cmd: cmds[i].into(), hist: (0..12).map(|k| (cpu * (0.6 + 0.4 * ((k as f32 * 0.9 + i as f32).sin() * 0.5 + 0.5))).max(0.0)).collect() }
        }).collect(),
    };
    let (total, cache) = (now.mem_total as f32, now.mem_cache as f32);
    let mem_s = mem_h.iter().map(|p| { let used = total * p / 100.0; [used, cache, (total - used - cache).max(0.0)] }).collect();
    let cores_h: Vec<Vec<f32>> = now.cores.iter().enumerate().map(|(i, last)| { let mut v = wave(*last, 18.0, i as f32 * 0.9); if anim.is_none() { *v.last_mut().unwrap() = *last; } v.iter().map(|x| x.clamp(0.0, 100.0)).collect() }).collect();
    let (rx_h, tx_h, rd_h, wr_h) = (wave(1_000_000.0, 700_000.0, 2.0), wave(200_000.0, 150_000.0, 3.0), wave(2_000_000.0, 1_800_000.0, 4.0), wave(700_000.0, 600_000.0, 5.0));
    let mut now = now;
    if anim.is_some() {
        let cpu = now.cpu;
        now.cpu_user = cpu * 0.63;
        now.cpu_sys = cpu * 0.24;
        now.cpu_wait = cpu * 0.08;
        now.cores = cores_h.iter().map(|v| *v.last().unwrap()).collect();
        now.mem_used = (total * mem_h.last().unwrap() / 100.0) as u64;
        now.mem_free = (total - now.mem_used as f32 - cache).max(0.0) as u64;
        now.net_rx = *rx_h.last().unwrap();
        now.net_tx = *tx_h.last().unwrap();
        now.disk_r = *rd_h.last().unwrap();
        now.disk_w = *wr_h.last().unwrap();
        now.disk_riops = now.disk_r / 7500.0;
        now.disk_wiops = now.disk_w / 9000.0;
        now.disk_util = cpu * 0.9;
        now.load = [1.0 + cpu / 90.0, 0.9 + cpu / 120.0, 0.8 + cpu / 160.0];
        now.temp_c = Some(40.0 + cpu * 0.3);
        for (i, pr) in now.procs.iter_mut().enumerate() {
            let base = 42.0 / (i as f32 + 1.0);
            pr.cpu = base * (0.55 + 0.9 * cpu / 60.0) * (1.0 + 0.1 * (ph0 * 1.0 + i as f32).sin());
            pr.mem = (pr.mem as f32 * (1.0 + 0.03 * (ph0 + i as f32 * 1.3).sin())) as u64;
            pr.hist = (0..12).map(|k| (pr.cpu * (0.6 + 0.4 * (((k as f32) * 0.9 + i as f32 + ph0).sin() * 0.5 + 0.5))).max(0.0)).collect();
        }
    }
    MonSnapshot { now, cpu_h, mem_h, cores_h, mem_s, rx_h, tx_h, rd_h, wr_h }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "##host\nweb-1\nLinux 6.1.0\nDebian GNU/Linux 12\n Intel Xeon\n##stat\ncpu  100 0 50 800 50 0 0 0 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\ncpu1 50 0 25 400 25 0 0 0 0 0\n##mem\nMemTotal: 8000000 kB\nMemFree: 1000000 kB\nMemAvailable: 4000000 kB\nBuffers: 100000 kB\nCached: 2000000 kB\nSwapTotal: 1000000 kB\nSwapFree: 900000 kB\n##net\nInter-|   Receive\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets\n    lo: 999 1 0 0 0 0 0 0 999 1 0 0 0 0 0 0\n  eth0: 1000 10 0 0 0 0 0 0 500 5 0 0 0 0 0 0\n##disk\n   8 0 sda 1 0 100 0 1 0 200 0 0 0 0\n   8 1 sda1 1 0 999 0 1 0 999 0 0 0 0\n##load\n0.50 0.40 0.30 1/200 999\n86400.5 1000.0\n##df\nFilesystem 1-blocks Used Available Capacity Mounted on\n/dev/sda1 1000 600 400 60% /\n/dev/sda1 1000 600 400 60% /var/lib/docker\n##temp\n45000\n##pinfo\n   42 www-data       4 nginx: worker process\n##ps\n1\tsystemd\t10\t100\n42\tweb (x)\t20\t200\n";

    fn b(extra_cpu: u64) -> String {
        A.replace("cpu  100 0 50 800 50 0 0 0 0 0", &format!("cpu  {} 0 50 {} 50 0 0 0 0 0", 100 + extra_cpu, 800 + 100 - extra_cpu))
            .replace("1000 10 0 0 0 0 0 0 500 5", "3048 10 0 0 0 0 0 0 1524 5")
            .replace("sda 1 0 100 0 1 0 200", "sda 1 0 300 0 1 0 600")
            .replace("web (x)\t20", "web (x)\t120")
    }

    #[test]
    fn parses_sections() {
        let r = parse_sample(A).unwrap();
        assert_eq!(r.host.name, "web-1");
        assert_eq!(r.host.cpu_model, "Intel Xeon");
        assert_eq!(r.cpu.len(), 3);
        assert_eq!(r.mem_total, 8_000_000 * 1024);
        assert_eq!(r.mem_avail, 4_000_000 * 1024);
        assert_eq!((r.net_rx, r.net_tx), (1000, 500), "lo 不算");
        assert_eq!((r.disk_r, r.disk_w), (100 * 512, 200 * 512), "分区不算");
        assert_eq!(r.load, [0.5, 0.4, 0.3]);
        assert_eq!(r.disks, vec![DiskUse { mount: "/".into(), dev: "/dev/sda1".into(), total: 1000, used: 600 }], "docker 目录和同一设备重复挂载不算");
        assert_eq!(r.temp_c, Some(45.0));
        assert_eq!(r.procs[&42], ("web (x)".to_string(), 20, 200 * 4096), "名字里带括号和空格也能解析");
        assert_eq!(r.pinfo[&42], ("www-data".to_string(), 4, "nginx: worker process".to_string()));
    }

    #[test]
    fn computes_rates() {
        let p = parse_sample(A).unwrap();
        let c = parse_sample(&b(100)).unwrap();
        let m = compute(&p, &c, 2.0);
        // 总计:增量 total=100+100+... 用户 +100,idle +0 → 忙
        assert!(m.cpu > 90.0, "cpu={}", m.cpu);
        assert_eq!(m.net_rx, 1024.0);
        assert_eq!(m.net_tx, 512.0);
        assert_eq!(m.disk_r, 200.0 * 512.0 / 2.0);
        assert_eq!(m.mem_used, 4_000_000 * 1024);
        assert_eq!(m.swap_used, 100_000 * 1024);
        assert_eq!(m.cores.len(), 2);
        assert_eq!(m.procs[0].pid, 42, "增量最多的排第一");
    }

    #[test]
    fn unsupported_system() {
        assert_eq!(parse_sample("##unsupported\n").unwrap_err(), "unsupported");
    }

    #[test]
    fn whole_disks() {
        for n in ["sda", "vdb", "xvda", "nvme0n1", "mmcblk0"] {
            assert!(is_whole_disk(n), "{n}");
        }
        for n in ["sda1", "nvme0n1p1", "mmcblk0p1", "loop0", "dm-0", "ram0"] {
            assert!(!is_whole_disk(n), "{n}");
        }
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_bytes(1536.0), "1.5 KB");
        assert_eq!(fmt_uptime(86400.0 * 23.0 + 3600.0 * 4.0), "23 天 4 小时");
    }
}
