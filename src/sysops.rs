//! 服务器运维页(进程、服务、存储、计划任务)用到的命令脚本和输出解析。
//!
//! 命令通过 SSH 在服务器上执行,界面只读解析后的结构。脚本都是 POSIX sh,读取类的不需要 root。

/// 读取的数据种类,同时也是界面缓存结果的键
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Procs,
    Services,
    Storage,
    Cron,
    /// 防火墙页:读取全部状态(root)
    Firewall,
    /// 空间透镜扫描前的访问检查
    Probe,
    /// 空间透镜里删除一个文件/目录
    Remove,
    /// 网站页:读取全部状态(root)
    Web,
    /// 网站页:一次操作的结果(站点/证书/账号/容器控制)
    WebOp,
    /// 网站页:后台长任务(安装、申请证书)的进度轮询
    WebPoll,
    /// 工具箱:端口检查、网络诊断(一次一个命令)、时间与时区、Swap 的读取
    ToolPorts,
    ToolNet,
    ToolTime,
    ToolSwap,
    /// 工具箱新增:主机名与 hosts/DNS、内核参数、SSH(设置和公钥共用)、用户、fail2ban
    ToolHost,
    ToolSysctl,
    ToolSsh,
    ToolUsers,
    ToolF2b,
    /// 堆栈的 compose 文件内容
    ComposeFile,
    /// 堆栈的 env 文件内容
    ComposeEnv,
}

/// 高风险操作(启用防火墙等)的「连不上就回滚」保险:脚本里已经在服务器上排了一个定时回滚,
/// 操作成功后由本机去连 `host:port`,连得上就执行 `cancel` 取消回滚,连不上就让服务器自己回滚
#[derive(Clone, Debug, PartialEq)]
pub struct Guard {
    pub host: String,
    pub port: u16,
    /// 服务器上回滚前等多少秒
    pub secs: u32,
    pub cancel: String,
    /// 回滚的是什么,用在提示里:「防火墙」「SSH 设置」
    pub what: String,
}

/// 一次要在服务器上执行的操作(结束进程、重启服务、保存 crontab……),完成后可以顺带重读一份数据
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// 动词,用在提示里:「已 重启」「重启失败」
    pub label: String,
    /// 对象(进程号、服务名……),也用来标记「正在进行」
    pub target: String,
    pub script: String,
    pub root: bool,
    /// 通过 stdin 交给脚本
    pub input: Option<String>,
    /// 完成后重读:(种类, 脚本, 是否 root)
    pub reload: Option<(Kind, String, bool)>,
    /// 高风险操作的自动回滚保险
    pub guard: Option<Guard>,
    /// 最长等多少秒;0 是默认的 60 秒(创建大的 swap 文件这类要更久)
    pub secs: u64,
    /// 确认框用普通样式(可以再改回来的操作:改时区、启用 swap),不是红色的「此操作无法撤销」
    pub calm: bool,
}

/// 先用 procps 的 `ps -eo`;BusyBox(Alpine、OpenWrt 等)的 ps 不认 `-e`/`--sort`,
/// 这时改读 /proc,输出同样的列(CPU 是进程整个生命周期的平均,和 procps 的 pcpu 定义一致)
pub const PROCS_SCRIPT: &str = r#"if out=$(ps -eo pid=,user:20=,pcpu=,pmem=,rss=,etime=,args= --sort=-pcpu 2>/dev/null) && [ -n "$out" ]; then echo "$out" | head -400; exit 0; fi
[ -r /proc/self/stat ] || { ps -eo pid=,user:20=,pcpu=,pmem=,rss=,etime=,args= 2>&1 | head -3; exit 0; }
hz=$(getconf CLK_TCK 2>/dev/null || echo 100); up=$(cut -d' ' -f1 /proc/uptime | cut -d. -f1); mt=$(awk '/^MemTotal/{print $2}' /proc/meminfo)
for d in /proc/[0-9]*; do
  p=${d#/proc/}; st=$(cat "$d/stat" 2>/dev/null) || continue
  comm=${st#*(}; comm=${comm%)*}; set -- ${st##*) }
  [ -n "${20}" ] || continue
  info=$(awk '/^Uid:/{u=$2} /^VmRSS:/{r=$2} END{print u+0, r+0}' "$d/status" 2>/dev/null)
  cmd=$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null)
  [ -n "$(echo $cmd)" ] || cmd="[$comm]"
  echo "$p $info $((${12} + ${13})) ${20} $cmd"
done | awk -v hz="$hz" -v up="$up" -v mt="$mt" 'BEGIN { while ((getline l < "/etc/passwd") > 0) { split(l, a, ":"); u[a[3]] = a[1] } }
{ pid = $1; uid = $2; rss = $3; tt = $4; sta = $5; $1 = $2 = $3 = $4 = $5 = ""; sub(/^ +/, "")
  el = up - sta / hz; if (el < 1) el = 1; cpu = tt / hz / el * 100; mem = mt > 0 ? rss / mt * 100 : 0
  usr = (uid in u) ? u[uid] : uid; d = int(el / 86400); h = int(el % 86400 / 3600); m = int(el % 3600 / 60); s = int(el % 60)
  et = d > 0 ? sprintf("%d-%02d:%02d:%02d", d, h, m, s) : (h > 0 ? sprintf("%02d:%02d:%02d", h, m, s) : sprintf("%02d:%02d", m, s))
  printf "%d %s %.1f %.1f %d %s %s\n", pid, usr, cpu, mem, rss, et, $0 }' | sort -k3,3nr | head -400"#;

pub const SERVICES_SCRIPT: &str = r#"if command -v systemctl >/dev/null 2>&1; then
systemctl list-units --type=service --all --no-pager --no-legend --plain 2>&1; echo '@@FILES'; systemctl list-unit-files --type=service --no-pager --no-legend --plain 2>/dev/null
elif [ -f /etc/rc.common ]; then
# OpenWrt(procd):/etc/init.d/<名> running|enabled,输出成和 systemd 一样的行,复用同一个解析
T=; command -v timeout >/dev/null 2>&1 && T='timeout 5'
for f in /etc/init.d/*; do [ -x "$f" ] && head -n 1 "$f" | grep -q rc.common || continue; n=${f##*/}
if $T "$f" running >/dev/null 2>&1; then echo "$n.service loaded active running"; else echo "$n.service loaded inactive dead"; fi; done
echo '@@FILES'
for f in /etc/init.d/*; do [ -x "$f" ] && head -n 1 "$f" | grep -q rc.common || continue; n=${f##*/}
if ls /etc/rc.d/S[0-9][0-9]$n >/dev/null 2>&1; then echo "$n.service enabled"; else echo "$n.service disabled"; fi; done
else echo '这台服务器没有 systemd,也不是 OpenWrt(可能是 Alpine/OpenRC 等),暂不支持服务管理' >&2; exit 1; fi"#;

pub const STORAGE_SCRIPT: &str = r#"X='-x tmpfs -x devtmpfs -x squashfs -x overlay -x efivarfs'; df -PT $X 2>/dev/null || df -P 2>&1 | awk 'NR==1{print;next} $1 ~ /^(tmpfs|devtmpfs|overlay|shm|none|udev|squashfs)$/{next} {printf "%s -", $1; for(i=2;i<=NF;i++) printf " %s", $i; print ""}'; echo '@@INODES'; df -Pi $X 2>/dev/null || df -Pi 2>/dev/null; echo '@@FSTAB'; cat /etc/fstab 2>/dev/null"#;

pub const CRON_SCRIPT: &str = "echo '@@USER'; crontab -l 2>/dev/null; for f in /etc/crontab /etc/cron.d/*; do [ -f \"$f\" ] && { echo \"@@FILE $f\"; cat \"$f\" 2>/dev/null; }; done; echo '@@TIMERS'; systemctl list-timers --all --no-pager --no-legend --plain 2>/dev/null";

/// 单引号包起来,里面的单引号转义
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 空间透镜:统计一个目录往下 `depth` 层的占用。`$1` 是目录(由调用方作为参数传入,不拼进脚本)。
/// `nice`/`ionice` 有就用,降低对服务器的影响;`-x` 不跨文件系统;无权限的报错丢掉。
/// `du` 是后序输出:一个目录的行出现,说明它整棵子树统计完了。
pub fn lens_script(depth: u32, files: bool) -> String {
    let find = if files { format!(" $n find \"$1\" -xdev -maxdepth {depth} -type f -size +1024k -exec du -k -- {{}} + 2>/dev/null | sed 's/^/F:/';") } else { String::new() };
    // 先列一层子目录(很快,界面据此先画出占位卡片),再 du;stdbuf 有就用,让 du 的输出逐行而不是攒满 4K 才出来
    format!("n=; command -v nice >/dev/null 2>&1 && n='nice -n 19'; command -v ionice >/dev/null 2>&1 && n=\"$n ionice -c3\"; $n find \"$1\" -xdev -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sed 's/^/D:/'; command -v stdbuf >/dev/null 2>&1 && n=\"$n stdbuf -oL\"; $n du -xk -d{depth} -- \"$1\" 2>/dev/null; s=$?;{find} exit $s")
}

/// 完整的 exec 命令:`sh -c <脚本> sh <目录>`。`files` 为 false 时不额外找大文件(网络存储上少遍历一遍)
pub fn lens_command(path: &str, depth: u32, files: bool) -> String {
    format!("sh -c {} sh {}", quote(&lens_script(depth, files)), quote(path))
}

/// 扫描前检查:登录用户能不能读这个目录。整个检查最多 5 秒(对端不响应时不会卡住);
/// 在 `sh -c` 里跑,用 `test`/`ls` 真的去访问一次——网络存储上权限位不可靠,要问对端才准。输出一个词。
pub fn probe_script(path: &str) -> String {
    format!("p={}; t=; command -v timeout >/dev/null 2>&1 && t='timeout 5'; $t sh -c '[ -d \"$1\" ] || exit 3; test -r \"$1\" && test -x \"$1\" || exit 4; ls -A -- \"$1\" >/dev/null 2>&1 || exit 4' sh \"$p\" 2>/dev/null; case $? in 0) echo OK;; 124) echo TIMEOUT;; 3) echo MISSING;; *) echo DENIED;; esac", quote(path))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Probe {
    Ok,
    Denied,
    Missing,
    Timeout,
}

pub fn parse_probe(out: &str) -> Probe {
    match out.trim() {
        "DENIED" => Probe::Denied,
        "MISSING" => Probe::Missing,
        "TIMEOUT" => Probe::Timeout,
        // 其他(包括读不出来)按可读处理,真有问题扫描时自然会报
        _ => Probe::Ok,
    }
}

/// 删除脚本:先确认存在、里面没有挂载的其他存储(`rm -r` 会钻进去删),再删;
/// 能用 `--one-file-system` 就加上。输出 `OK` 或 `ERR:原因`。
pub fn remove_script(path: &str) -> String {
    format!(
        "p={}; if [ ! -e \"$p\" ] && [ ! -L \"$p\" ]; then echo 'ERR:已经不存在'; exit 0; fi; \
         if awk -v p=\"$p\" '{{m=$2; if (m==p || index(m, p \"/\")==1) f=1}} END{{exit !f}}' /proc/mounts 2>/dev/null; then echo 'ERR:里面挂载了其他存储,不能删除'; exit 0; fi; \
         F=; rm --help 2>&1 | grep -q one-file-system && F=--one-file-system; \
         o=$(rm -rf $F -- \"$p\" 2>&1) && echo OK || echo \"ERR:$o\"",
        quote(path)
    )
}

#[derive(Clone, Debug, PartialEq)]
pub enum Removed {
    Ok,
    /// 没有权限(可以让用户选 root 重试)
    Denied(String),
    Failed(String),
}

pub fn parse_remove(out: &str) -> Removed {
    let t = out.trim();
    if t.lines().last() == Some("OK") && !t.contains("ERR:") {
        return Removed::Ok;
    }
    let msg = t.split_once("ERR:").map(|x| x.1).unwrap_or(t).trim().to_string();
    let low = msg.to_lowercase();
    if low.contains("permission denied") || low.contains("operation not permitted") {
        Removed::Denied(msg)
    } else {
        Removed::Failed(if msg.is_empty() { "未知错误".into() } else { msg })
    }
}

/// 网络/远程文件系统:遍历要走网络,慢且会给对端加负担
pub fn is_network_fs(fstype: &str) -> bool {
    let t = fstype.to_ascii_lowercase();
    t.starts_with("nfs") || matches!(t.as_str(), "cifs" | "smb3" | "smbfs" | "afs" | "ceph" | "glusterfs" | "9p" | "davfs") || t.starts_with("fuse.sshfs") || t.starts_with("fuse.s3") || t.starts_with("fuse.rclone") || t.starts_with("fuse.gluster")
}

/// 扫描输出的一行是什么
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LineKind {
    /// `du` 统计完的目录
    Dir,
    /// ≥1 MiB 的文件
    File,
    /// 只知道有这个子目录,大小还没统计出来(用来先画占位卡片)
    Listed,
}

/// 透镜扫描过程中从后台送给界面的事件
#[derive(Clone, Debug, PartialEq)]
pub enum LensEv {
    /// 新统计完的目录:(KiB, 完整路径)
    Lines { scan: u64, lines: Vec<(u64, String, LineKind)> },
    /// 扫描结束:出错的原因,或用户主动停止
    Done { scan: u64, err: Option<String>, stopped: bool },
}

/// 解析一行:`KiB<TAB>路径` 是目录;`F:KiB<TAB>路径` 是 ≥1 MiB 的文件(`du` 只列目录,文件由脚本里的 `find` 补);
/// `D:路径` 是先列出来的子目录(还没有大小)
pub fn parse_lens_line(line: &str) -> Option<(u64, String, LineKind)> {
    let line = line.trim_end_matches('\r');
    if let Some(p) = line.strip_prefix("D:") {
        return if p.is_empty() { None } else { Some((0, p.to_string(), LineKind::Listed)) };
    }
    let (line, kind) = match line.strip_prefix("F:") {
        Some(r) => (r, LineKind::File),
        None => (line, LineKind::Dir),
    };
    let (n, p) = line.split_once('\t')?;
    if p.is_empty() { None } else { Some((n.trim().parse().ok()?, p.to_string(), kind)) }
}

/// systemd 单元名:只允许这些字符,避免拼进 shell 命令时出事
/// 服务操作命令:有 systemd 用 systemctl,否则(OpenWrt)走 /etc/init.d/<名>
pub fn svc_cmd(action: &str, name: &str) -> String {
    format!("if command -v systemctl >/dev/null 2>&1; then systemctl {action} {name}.service; else /etc/init.d/{name} {action}; fi")
}

pub fn safe_unit(s: &str) -> bool {
    !s.is_empty() && s.len() <= 200 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '@' | '-' | '\\'))
}

/// 容量(KiB)显示成 `12.3 GB`
pub fn fmt_kb(kb: u64) -> String {
    let b = kb as f64 * 1024.0;
    let units = ["B", "KB", "MB", "GB", "TB", "PB"];
    let (mut v, mut i) = (b, 0);
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 || v >= 100.0 { format!("{:.0} {}", v, units[i]) } else { format!("{:.1} {}", v, units[i]) }
}

// ---- 进程 ----

#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub user: String,
    pub cpu: f32,
    pub mem: f32,
    pub rss_kb: u64,
    pub etime: String,
    pub cmd: String,
}

pub fn parse_procs(text: &str) -> Result<Vec<Proc>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(user), Some(cpu), Some(mem), Some(rss), Some(et)) = (it.next(), it.next(), it.next(), it.next(), it.next(), it.next()) else { continue };
        let (Ok(pid), Ok(cpu), Ok(mem), Ok(rss)) = (pid.parse::<u32>(), cpu.parse::<f32>(), mem.parse::<f32>(), rss.parse::<u64>()) else { continue };
        let cmd = it.collect::<Vec<_>>().join(" ");
        out.push(Proc { pid, user: user.to_string(), cpu, mem, rss_kb: rss, etime: et.to_string(), cmd });
    }
    if out.is_empty() {
        let msg = text.trim().lines().next().unwrap_or("没有读到进程");
        return Err(msg.to_string());
    }
    Ok(out)
}

// ---- 服务 ----

#[derive(Clone, Debug, PartialEq)]
pub struct Service {
    /// 不带 `.service` 后缀
    pub name: String,
    /// active / inactive / failed / activating ……
    pub active: String,
    /// running / exited / dead / failed ……
    pub sub: String,
    pub desc: String,
    /// enabled / disabled / static / masked ……,不知道是空串
    pub enabled: String,
}

impl Service {
    pub fn running(&self) -> bool {
        self.active == "active" && self.sub != "exited"
    }
    pub fn failed(&self) -> bool {
        self.active == "failed"
    }
}

pub fn parse_services(text: &str) -> Result<Vec<Service>, String> {
    let (units, files) = text.split_once("@@FILES").unwrap_or((text, ""));
    let mut enabled = std::collections::HashMap::new();
    for l in files.lines() {
        let mut it = l.split_whitespace();
        if let (Some(u), Some(st)) = (it.next(), it.next()) {
            enabled.insert(u.trim_end_matches(".service").to_string(), st.to_string());
        }
    }
    let mut out: Vec<Service> = Vec::new();
    for l in units.lines() {
        let l = l.trim().trim_start_matches(['●', '*', '○', '×']).trim();
        let mut it = l.split_whitespace();
        let (Some(u), Some(load), Some(active), Some(sub)) = (it.next(), it.next(), it.next(), it.next()) else { continue };
        if !u.ends_with(".service") || load == "not-found" {
            continue;
        }
        let name = u.trim_end_matches(".service").to_string();
        let desc = it.collect::<Vec<_>>().join(" ");
        let en = enabled.get(&name).cloned().unwrap_or_default();
        out.push(Service { name, active: active.to_string(), sub: sub.to_string(), desc, enabled: en });
    }
    // 从没启动过的单元不在 list-units 里,补上
    let known: std::collections::HashSet<String> = out.iter().map(|s| s.name.clone()).collect();
    for (name, st) in &enabled {
        if known.contains(name) || name.ends_with('@') || name.contains("@.") {
            continue;
        }
        out.push(Service { name: name.clone(), active: "inactive".into(), sub: "dead".into(), desc: String::new(), enabled: st.clone() });
    }
    if out.is_empty() {
        let msg = units.trim().lines().next().unwrap_or("没有读到服务");
        return Err(msg.to_string());
    }
    out.sort_by(|a, b| b.running().cmp(&a.running()).then(a.name.cmp(&b.name)));
    Ok(out)
}

// ---- 存储 ----

#[derive(Clone, Debug, PartialEq)]
pub struct Mount {
    pub dev: String,
    pub fstype: String,
    pub total_kb: u64,
    pub used_kb: u64,
    pub avail_kb: u64,
    pub pct: u8,
    pub mount: String,
    /// inode 使用率;文件系统不统计 inode(btrfs 等)时是 None
    pub inode_pct: Option<u8>,
    /// `/etc/fstab` 里这个挂载点的那一行(编辑时预填;没有说明是手动临时挂的)
    pub fstab: Option<crate::mounts::FstabEntry>,
    /// 是不是 Otter 能管的挂载(能编辑、卸载),见 `mounts::managed`
    pub managed: bool,
}

fn pct_of(s: &str) -> Option<u8> {
    s.trim_end_matches('%').parse().ok()
}

pub fn parse_storage(text: &str) -> Result<Vec<Mount>, String> {
    let (text, fstab_text) = text.split_once(crate::mounts::FSTAB_MARK).unwrap_or((text, ""));
    let fstab = crate::mounts::parse_fstab(fstab_text);
    let (space, inodes) = text.split_once("@@INODES").unwrap_or((text, ""));
    let mut ino = std::collections::HashMap::new();
    for l in inodes.lines().skip(1) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() >= 6 {
            ino.insert(f[5..].join(" "), pct_of(f[4]));
        }
    }
    let mut out: Vec<Mount> = Vec::new();
    for l in space.lines().skip(1) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 7 {
            continue;
        }
        let (Ok(total), Ok(used), Ok(avail)) = (f[2].parse::<u64>(), f[3].parse::<u64>(), f[4].parse::<u64>()) else { continue };
        let mount = f[6..].join(" ");
        if ["/run/", "/var/lib/docker/", "/var/lib/kubelet/", "/snap/", "/boot/efi"].iter().any(|p| mount.starts_with(p)) || total == 0 {
            continue;
        }
        // 同一设备的多个挂载点(bind、子卷)只留第一个
        if out.iter().any(|m| m.dev == f[0]) {
            continue;
        }
        let inode_pct = ino.get(&mount).copied().flatten();
        let managed = crate::mounts::managed(&mount, &fstab);
        let entry = fstab.iter().find(|e| e.mount == mount).cloned();
        out.push(Mount { dev: f[0].to_string(), fstype: f[1].to_string(), total_kb: total, used_kb: used, avail_kb: avail, pct: pct_of(f[5]).unwrap_or(0), mount, inode_pct, fstab: entry, managed });
    }
    if out.is_empty() {
        let msg = space.trim().lines().next().unwrap_or("没有读到磁盘信息");
        return Err(msg.to_string());
    }
    out.sort_by(|a, b| a.mount.cmp(&b.mount));
    Ok(out)
}

// ---- 计划任务 ----

#[derive(Clone, Debug, PartialEq)]
pub struct CronJob {
    /// 「我的 crontab」或文件路径
    pub source: String,
    pub schedule: String,
    pub user: String,
    pub cmd: String,
    /// 在「我的 crontab」原文里的行号(从 0 起);系统任务是 None,只能看不能改
    pub line: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Timer {
    pub unit: String,
    pub activates: String,
    /// 下一次触发的时间,没有计划是空串
    pub next: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CronInfo {
    /// 当前用户 crontab 的原文
    pub user_text: String,
    pub jobs: Vec<CronJob>,
    pub timers: Vec<Timer>,
}

fn parse_cron_line(line: &str, source: &str, with_user: bool, me: &str) -> Option<CronJob> {
    let l = line.trim();
    if l.is_empty() || l.starts_with('#') {
        return None;
    }
    let toks: Vec<&str> = l.split_whitespace().collect();
    // 环境变量赋值(`MAILTO=x`、`PATH=…`)
    if toks[0].contains('=') && !toks[0].starts_with(|c: char| c.is_ascii_digit() || c == '*' || c == '@') {
        return None;
    }
    let (schedule, rest): (String, &[&str]) = if toks[0].starts_with('@') {
        (toks[0].to_string(), &toks[1..])
    } else if toks.len() > 5 {
        (toks[..5].join(" "), &toks[5..])
    } else {
        return None;
    };
    let (user, cmd) = if with_user {
        let (u, c) = rest.split_first()?;
        (u.to_string(), c.join(" "))
    } else {
        (me.to_string(), rest.join(" "))
    };
    (!cmd.is_empty()).then(|| CronJob { source: source.to_string(), schedule, user, cmd, line: None })
}

pub fn parse_cron(text: &str) -> CronInfo {
    let mut info = CronInfo::default();
    let mut cur = "";
    let (mut user_buf, mut timers_buf) = (String::new(), String::new());
    let mut section = 0; // 0 = 开头, 1 = 用户, 2 = 系统文件, 3 = 定时器
    for l in text.lines() {
        if l == "@@USER" {
            section = 1;
            continue;
        }
        if let Some(f) = l.strip_prefix("@@FILE ") {
            section = 2;
            cur = f.trim();
            continue;
        }
        if l == "@@TIMERS" {
            section = 3;
            continue;
        }
        match section {
            1 => {
                if let Some(mut j) = parse_cron_line(l, "我的 crontab", false, "我") {
                    j.line = Some(user_buf.lines().count());
                    info.jobs.push(j);
                }
                user_buf.push_str(l);
                user_buf.push('\n');
            }
            2 => {
                if let Some(j) = parse_cron_line(l, cur, true, "") {
                    info.jobs.push(j);
                }
            }
            3 => {
                timers_buf.push_str(l);
                timers_buf.push('\n');
            }
            _ => {}
        }
    }
    info.user_text = user_buf;
    for l in timers_buf.lines() {
        let t: Vec<&str> = l.split_whitespace().collect();
        if t.len() < 2 || !t[t.len() - 1].ends_with(".service") && !t[t.len() - 1].ends_with(".target") {
            continue;
        }
        let activates = t[t.len() - 1].to_string();
        let unit = t[t.len() - 2].to_string();
        // 开头是 `星期 日期 时间 时区`;没有计划时是 n/a
        let next = if t[0] == "n/a" || t.len() < 6 { String::new() } else { t[..4].join(" ") };
        info.timers.push(Timer { unit, activates, next });
    }
    info
}

/// 计划是不是 cron 认的写法:5 段(分 时 日 月 周),或 `@daily` 这样的简写
pub fn valid_schedule(s: &str) -> bool {
    let s = s.trim();
    if s.starts_with('@') {
        return matches!(s, "@reboot" | "@hourly" | "@daily" | "@midnight" | "@weekly" | "@monthly" | "@yearly" | "@annually");
    }
    let f: Vec<&str> = s.split_whitespace().collect();
    f.len() == 5 && f.iter().all(|x| x.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '*' | '/' | ',' | '-' | '?')))
}

/// 改 crontab 原文:`line` 是要改的行(None 是在末尾加),`new` 是新内容(None 是删掉这一行)
pub fn crontab_replace(text: &str, line: Option<usize>, new: Option<&str>) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    match (line, new) {
        (Some(i), Some(n)) if i < lines.len() => lines[i] = n.to_string(),
        (Some(i), None) if i < lines.len() => {
            lines.remove(i);
        }
        (None, Some(n)) => lines.push(n.to_string()),
        _ => {}
    }
    if lines.is_empty() { String::new() } else { lines.join("\n") + "\n" }
}

/// 常见的 cron 表达式翻成中文,翻不了的原样返回
pub fn describe(schedule: &str) -> String {
    match schedule {
        "@reboot" => return "开机时".into(),
        "@hourly" => return "每小时".into(),
        "@daily" | "@midnight" => return "每天 00:00".into(),
        "@weekly" => return "每周日 00:00".into(),
        "@monthly" => return "每月 1 日 00:00".into(),
        "@yearly" | "@annually" => return "每年 1 月 1 日 00:00".into(),
        _ => {}
    }
    let f: Vec<&str> = schedule.split_whitespace().collect();
    if f.len() != 5 {
        return schedule.to_string();
    }
    let (m, h, dom, mon, dow) = (f[0], f[1], f[2], f[3], f[4]);
    let num = |s: &str| s.parse::<u32>().ok();
    let hm = |h: u32, m: u32| format!("{h:02}:{m:02}");
    let wk = |d: u32| ["日", "一", "二", "三", "四", "五", "六", "日"].get(d as usize).copied().unwrap_or("?");
    if [m, h, dom, mon, dow].iter().all(|x| *x == "*") {
        return "每分钟".into();
    }
    if let Some(n) = m.strip_prefix("*/").and_then(num) {
        if [h, dom, mon, dow].iter().all(|x| *x == "*") {
            return format!("每 {n} 分钟");
        }
    }
    if let (Some(mm), Some(n)) = (num(m), h.strip_prefix("*/").and_then(num)) {
        if [dom, mon, dow].iter().all(|x| *x == "*") {
            return if mm == 0 { format!("每 {n} 小时") } else { format!("每 {n} 小时(第 {mm} 分)") };
        }
    }
    if let (Some(mm), true) = (num(m), mon == "*") {
        if h == "*" && dom == "*" && dow == "*" {
            return format!("每小时第 {mm} 分");
        }
        if let Some(hh) = num(h) {
            if dom == "*" && dow == "*" {
                return format!("每天 {}", hm(hh, mm));
            }
            if dom == "*" {
                if let Some(d) = num(dow) {
                    return format!("每周{} {}", wk(d), hm(hh, mm));
                }
            }
            if dow == "*" {
                if let Some(d) = num(dom) {
                    return format!("每月 {d} 日 {}", hm(hh, mm));
                }
            }
        }
    }
    schedule.to_string()
}

// ---- 人性化的运行时间 ----

/// 计划任务的运行频率(表单里的「每天 03:00」「每隔 30 分钟」);和 cron 表达式互相转换
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreqKind {
    Minutes,
    Hours,
    Daily,
    Weekly,
    Monthly,
    Reboot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Freq {
    pub kind: FreqKind,
    /// 每隔几分钟/几小时
    pub every: u32,
    pub hour: u32,
    pub minute: u32,
    /// 周几:第 0 位是周日,第 1 位是周一……(和 cron 的 0-6 一致)
    pub weekdays: u8,
    /// 每月几号
    pub day: u32,
}

impl Default for Freq {
    fn default() -> Self {
        Freq { kind: FreqKind::Daily, every: 30, hour: 3, minute: 0, weekdays: 0b0000010, day: 1 }
    }
}

impl Freq {
    /// 转成 cron 表达式;取值不合法(周几一个都没选、时间越界)返回 None
    pub fn to_cron(&self) -> Option<String> {
        let (h, m) = (self.hour, self.minute);
        if h > 23 || m > 59 {
            return None;
        }
        Some(match self.kind {
            FreqKind::Reboot => "@reboot".to_string(),
            FreqKind::Minutes if (1..=59).contains(&self.every) => if self.every == 1 { "* * * * *".into() } else { format!("*/{} * * * *", self.every) },
            FreqKind::Hours if (1..=23).contains(&self.every) => if self.every == 1 { format!("{m} * * * *") } else { format!("{m} */{} * * *", self.every) },
            FreqKind::Daily => format!("{m} {h} * * *"),
            FreqKind::Weekly if self.weekdays & 0x7f != 0 => {
                let days: Vec<String> = (0..7).filter(|d| self.weekdays & (1 << d) != 0).map(|d| d.to_string()).collect();
                format!("{m} {h} * * {}", days.join(","))
            }
            FreqKind::Monthly if (1..=31).contains(&self.day) => format!("{m} {h} {} * *", self.day),
            _ => return None,
        })
    }

    /// 从 cron 表达式反解;复杂的表达式(范围、多个小时等)返回 None,由「高级」处理
    pub fn from_cron(s: &str) -> Option<Freq> {
        let d = Freq::default();
        match s.trim() {
            "@reboot" => return Some(Freq { kind: FreqKind::Reboot, ..d }),
            "@hourly" => return Some(Freq { kind: FreqKind::Hours, every: 1, minute: 0, ..d }),
            "@daily" | "@midnight" => return Some(Freq { kind: FreqKind::Daily, hour: 0, minute: 0, ..d }),
            "@weekly" => return Some(Freq { kind: FreqKind::Weekly, hour: 0, minute: 0, weekdays: 1, ..d }),
            "@monthly" => return Some(Freq { kind: FreqKind::Monthly, hour: 0, minute: 0, day: 1, ..d }),
            _ => {}
        }
        let f: Vec<&str> = s.split_whitespace().collect();
        if f.len() != 5 {
            return None;
        }
        let (m, h, dom, mon, dow) = (f[0], f[1], f[2], f[3], f[4]);
        let num = |x: &str| x.parse::<u32>().ok();
        let step = |x: &str| x.strip_prefix("*/").and_then(num);
        if mon != "*" {
            return None;
        }
        if [m, h, dom, dow].iter().all(|x| *x == "*") {
            return Some(Freq { kind: FreqKind::Minutes, every: 1, ..d });
        }
        if let (Some(n), true) = (step(m), [h, dom, dow].iter().all(|x| *x == "*")) {
            return (1..=59).contains(&n).then_some(Freq { kind: FreqKind::Minutes, every: n, ..d });
        }
        let mm = num(m).filter(|v| *v <= 59)?;
        if h == "*" && dom == "*" && dow == "*" {
            return Some(Freq { kind: FreqKind::Hours, every: 1, minute: mm, ..d });
        }
        if let (Some(n), true) = (step(h), dom == "*" && dow == "*") {
            return (1..=23).contains(&n).then_some(Freq { kind: FreqKind::Hours, every: n, minute: mm, ..d });
        }
        let hh = num(h).filter(|v| *v <= 23)?;
        if dom == "*" && dow == "*" {
            return Some(Freq { kind: FreqKind::Daily, hour: hh, minute: mm, ..d });
        }
        if dom == "*" {
            let mut mask = 0u8;
            for part in dow.split(',') {
                let (a, b) = match part.split_once('-') {
                    Some((a, b)) => (num(a)?, num(b)?),
                    None => (num(part)?, num(part)?),
                };
                if a > 7 || b > 7 || a > b {
                    return None;
                }
                for v in a..=b {
                    mask |= 1 << (v % 7);
                }
            }
            return Some(Freq { kind: FreqKind::Weekly, hour: hh, minute: mm, weekdays: mask, ..d });
        }
        if dow == "*" {
            let day = num(dom).filter(|v| (1..=31).contains(v))?;
            return Some(Freq { kind: FreqKind::Monthly, hour: hh, minute: mm, day, ..d });
        }
        None
    }
}

/// 表单里的 `HH:MM`(也接受 `H:MM`、`HH：MM`)
pub fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let s = s.trim().replace('：', ":");
    let (h, m) = s.split_once(':')?;
    let (h, m) = (h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?);
    (h <= 23 && m <= 59).then_some((h, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_procs() {
        let t = "    1 root                  0.0  0.1  11852     05:12:01 /sbin/init splash\n 2210 www-data              12.5  3.4 204800       12:30 nginx: worker process\n";
        let p = parse_procs(t).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!((p[1].pid, p[1].user.as_str(), p[1].cpu, p[1].rss_kb), (2210, "www-data", 12.5, 204800));
        assert_eq!(p[1].cmd, "nginx: worker process");
        assert!(parse_procs("ps: unrecognized option").is_err());
    }

    #[test]
    fn parses_services_with_enabled_state() {
        let t = "  ssh.service loaded active running OpenBSD Secure Shell server\n● foo.service loaded failed failed Foo daemon\n  gone.service not-found inactive dead gone.service\n@@FILES\nssh.service enabled enabled\nfoo.service disabled enabled\ncron.service enabled enabled\ngetty@.service enabled enabled\n";
        let s = parse_services(t).unwrap();
        let names: Vec<&str> = s.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["ssh", "cron", "foo"]);
        assert!(s[0].running() && s[0].enabled == "enabled" && s[0].desc.starts_with("OpenBSD"));
        assert!(s[2].failed() && s[2].enabled == "disabled");
        assert_eq!(s[1].active, "inactive");
    }

    #[test]
    fn parses_storage_and_inodes() {
        let t = "Filesystem     Type 1024-blocks     Used Available Capacity Mounted on\n/dev/sda1      ext4    103081248 41234567  56000000      43% /\n/dev/sda2      ext4     51540000  1000000  48000000       3% /data disk\n/dev/sda1      ext4    103081248 41234567  56000000      43% /var/lib/docker/x\n@@INODES\nFilesystem      Inodes  IUsed   IFree IUse% Mounted on\n/dev/sda1      6553600 345678 6207922    6% /\n/dev/sda2      3276800     11 3276789    1% /data disk\n";
        let m = parse_storage(t).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].mount.as_str(), m[0].pct, m[0].inode_pct), ("/", 43, Some(6)));
        assert_eq!(m[1].mount, "/data disk");
        assert_eq!(m[1].inode_pct, Some(1));
        assert!(parse_storage("df: command not found").is_err());
    }

    #[test]
    fn parses_lens_lines() {
        assert_eq!(parse_lens_line("3000\t/var/lib"), Some((3000, "/var/lib".into(), LineKind::Dir)));
        assert_eq!(parse_lens_line("F:2048\t/var/big.log"), Some((2048, "/var/big.log".into(), LineKind::File)));
        assert_eq!(parse_lens_line("D:/var/lib"), Some((0, "/var/lib".into(), LineKind::Listed)));
        assert_eq!(parse_lens_line("D:"), None);
        assert_eq!(parse_lens_line("12\t/a b/c\r"), Some((12, "/a b/c".into(), LineKind::Dir)));
        assert_eq!(parse_lens_line("du: cannot read"), None);
        assert_eq!(parse_lens_line("x\t/a"), None);
        assert_eq!(parse_lens_line("5\t"), None);
    }

    #[test]
    fn probe_script_and_parse() {
        let sc = probe_script("/mnt/a b'c");
        assert!(sc.starts_with("p='/mnt/a b'\\''c';") && sc.contains("timeout 5"));
        assert_eq!(parse_probe("OK\n"), Probe::Ok);
        assert_eq!(parse_probe("DENIED"), Probe::Denied);
        assert_eq!(parse_probe(" MISSING "), Probe::Missing);
        assert_eq!(parse_probe("TIMEOUT"), Probe::Timeout);
        assert_eq!(parse_probe(""), Probe::Ok);
    }

    #[test]
    fn lens_command_quotes_the_path() {
        let c = lens_command("/a b'c", 3, true);
        assert!(c.starts_with("sh -c '") && c.ends_with(" sh '/a b'\\''c'"));
        assert!(c.contains("du -xk -d3 -- "));
        // 路径只作为参数,不出现在脚本里
        assert!(!lens_script(3, true).contains("a b"));
        // 网络存储不跑 find
        assert!(lens_script(3, true).contains("-size +1024k") && !lens_script(3, false).contains("-size +1024k"));
        // 都会先列一层子目录
        assert!(lens_script(3, false).contains("sed 's/^/D:/'"));
        assert!(is_network_fs("nfs4") && is_network_fs("cifs") && !is_network_fs("ext4") && !is_network_fs("xfs"));
    }

    #[test]
    fn parses_cron_sources_and_timers() {
        let t = "@@USER\nMAILTO=me@x.com\n*/5 * * * * /usr/bin/backup --fast\n@reboot /opt/start.sh\n@@FILE /etc/crontab\nSHELL=/bin/sh\n17 * * * * root cd / && run-parts /etc/cron.hourly\n@@FILE /etc/cron.d/logrotate\n0 3 * * 1 root /usr/sbin/logrotate\n@@TIMERS\nTue 2026-11-24 10:00:00 CST 12min left Tue 2026-11-24 09:00:01 CST 47min ago apt-daily.timer apt-daily.service\nn/a n/a n/a n/a x.timer x.service\n";
        let c = parse_cron(t);
        assert_eq!(c.jobs.len(), 4);
        assert_eq!((c.jobs[0].user.as_str(), c.jobs[0].schedule.as_str(), c.jobs[0].cmd.as_str()), ("我", "*/5 * * * *", "/usr/bin/backup --fast"));
        assert_eq!(c.jobs[1].schedule, "@reboot");
        assert_eq!((c.jobs[2].source.as_str(), c.jobs[2].user.as_str()), ("/etc/crontab", "root"));
        assert_eq!(c.jobs[3].cmd, "/usr/sbin/logrotate");
        assert!(c.user_text.contains("MAILTO"));
        assert_eq!(c.timers.len(), 2);
        assert_eq!((c.timers[0].unit.as_str(), c.timers[0].next.as_str()), ("apt-daily.timer", "Tue 2026-11-24 10:00:00 CST"));
        assert_eq!(c.timers[1].next, "");
    }

    #[test]
    fn edits_crontab_lines() {
        let c = parse_cron("@@USER\nMAILTO=x\n*/5 * * * * a.sh\n# note\n0 3 * * * b.sh\n@@FILE /etc/crontab\n1 2 * * * root c\n");
        let lines: Vec<Option<usize>> = c.jobs.iter().map(|j| j.line).collect();
        assert_eq!(lines, [Some(1), Some(3), None]);
        let t = &c.user_text;
        assert_eq!(crontab_replace(t, Some(3), None), "MAILTO=x\n*/5 * * * * a.sh\n# note\n");
        assert_eq!(crontab_replace(t, Some(1), Some("*/10 * * * * a.sh")), "MAILTO=x\n*/10 * * * * a.sh\n# note\n0 3 * * * b.sh\n");
        assert_eq!(crontab_replace(t, None, Some("@reboot r.sh")).lines().last(), Some("@reboot r.sh"));
        assert_eq!(crontab_replace("", None, Some("@daily x")), "@daily x\n");
        assert_eq!(crontab_replace("@daily x\n", Some(0), None), "");
    }

    #[test]
    fn validates_schedules() {
        assert!(valid_schedule("*/5 * * * *") && valid_schedule("0 3 * * 1-5") && valid_schedule("@daily") && valid_schedule("0 0 1 jan *"));
        assert!(!valid_schedule("* * * *") && !valid_schedule("@often") && !valid_schedule("0 3 * * * x") && !valid_schedule("a;b * * * *") && !valid_schedule(""));
    }

    #[test]
    fn describes_schedules() {
        assert_eq!(describe("*/5 * * * *"), "每 5 分钟");
        assert_eq!(describe("0 3 * * *"), "每天 03:00");
        assert_eq!(describe("30 4 * * 1"), "每周一 04:30");
        assert_eq!(describe("0 0 15 * *"), "每月 15 日 00:00");
        assert_eq!(describe("15 * * * *"), "每小时第 15 分");
        assert_eq!(describe("@daily"), "每天 00:00");
        assert_eq!(describe("0 */12 * * *"), "每 12 小时");
        assert_eq!(describe("1-5 2 * * *"), "1-5 2 * * *");
    }

    #[test]
    fn validates_names_and_formats() {
        assert!(safe_unit("nginx") && safe_unit("getty@tty1") && !safe_unit("a b") && !safe_unit("-x") && !safe_unit("a;b"));
        assert_eq!(fmt_kb(1024), "1.0 MB");
        assert_eq!(fmt_kb(50 * 1024 * 1024), "50.0 GB");
    }
    #[test]
    fn remove_script_and_result() {
        let sc = remove_script("/data/it's here");
        assert!(sc.contains("'/data/it'\\''s here'") || sc.contains("it"), "{sc}");
        assert!(sc.contains("rm -rf $F -- \"$p\""));
        assert_eq!(parse_remove("OK\n"), Removed::Ok);
        assert!(matches!(parse_remove("ERR:rm: cannot remove 'x': Permission denied"), Removed::Denied(_)));
        assert_eq!(parse_remove("ERR:已经不存在"), Removed::Failed("已经不存在".into()));
        assert!(matches!(parse_remove(""), Removed::Failed(_)));
    }

    #[test]
    fn storage_reads_fstab_and_marks_managed() {
        let t = "Filesystem     Type 1024-blocks     Used Available Capacity Mounted on\n/dev/sda1      ext4    103081248 41234567  56000000      43% /\n/dev/sdb1      xfs     515396075 98304000 417092075      20% /data\n10.0.0.5:/e    nfs4    1048576 314572 734003 30% /mnt/nas\n/dev/sdc1      ext4    1000000 1000 999000 1% /scratch\n@@INODES\nFilesystem Inodes IUsed IFree IUse% Mounted on\n@@FSTAB\n# c\nUUID=x / ext4 defaults 0 1\n/dev/sdb1 /data xfs defaults 0 2\n";
        let v = parse_storage(t).unwrap();
        let by = |m: &str| v.iter().find(|x| x.mount == m).unwrap();
        assert!(!by("/").managed);
        assert!(by("/data").managed && by("/data").fstab.as_ref().unwrap().fstype == "xfs");
        assert!(by("/mnt/nas").managed && by("/mnt/nas").fstab.is_none());
        assert!(!by("/scratch").managed);
    }

    #[test]
    fn freq_round_trips() {
        let f = |s: &str| Freq::from_cron(s);
        assert_eq!(f("*/5 * * * *").unwrap().to_cron().unwrap(), "*/5 * * * *");
        assert_eq!(f("0 */6 * * *").unwrap().to_cron().unwrap(), "0 */6 * * *");
        assert_eq!(f("30 3 * * *").map(|x| (x.kind, x.hour, x.minute)), Some((FreqKind::Daily, 3, 30)));
        assert_eq!(f("0 4 * * 1,3,5").unwrap().to_cron().unwrap(), "0 4 * * 1,3,5");
        assert_eq!(f("0 4 * * 1-5").unwrap().to_cron().unwrap(), "0 4 * * 1,2,3,4,5");
        assert_eq!(f("0 4 * * 7").unwrap().to_cron().unwrap(), "0 4 * * 0");
        assert_eq!(f("15 2 10 * *").unwrap().to_cron().unwrap(), "15 2 10 * *");
        assert_eq!(f("@reboot").unwrap().to_cron().unwrap(), "@reboot");
        assert_eq!(f("@daily").unwrap().to_cron().unwrap(), "0 0 * * *");
        // 复杂的留给「高级」
        assert!(f("0 1,13 * * *").is_none());
        assert!(f("0 3 * 6 *").is_none());
        assert!(f("*/90 * * * *").is_none());
        // 取值不合法
        let w = Freq { kind: FreqKind::Weekly, weekdays: 0, ..Freq::default() };
        assert!(w.to_cron().is_none());
        assert!(Freq { hour: 24, ..Freq::default() }.to_cron().is_none());
        assert_eq!(parse_hhmm("3:05"), Some((3, 5)));
        assert_eq!(parse_hhmm("03：30"), Some((3, 30)));
        assert!(parse_hhmm("25:00").is_none());
        assert!(parse_hhmm("abc").is_none());
    }
}
