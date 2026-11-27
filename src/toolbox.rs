//! 工具箱:端口检查、网络诊断、时间与时区、Swap 用到的服务器端脚本、输出解析和判断逻辑。
//!
//! 全部经 SSH 在服务器上用现成的命令完成,服务器上不装东西([0001](../docs/decisions/0001-agentless.md));
//! 缺命令时给出提示,不代装。纯逻辑,有单测;真机行为见 `src/orbtest/toolbox.rs`。

use crate::docker::Container;
use crate::firewall::{self, Action as FwAction, FwInfo};

const PATH_FIX: &str = "PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH\n";

/// 脚本里按需加超时:有 `timeout` 就用,没有就直接跑
const T_FN: &str = "T() { s=$1; shift; if command -v timeout >/dev/null 2>&1; then timeout \"$s\" \"$@\"; else \"$@\"; fi; }\n";

// ================= 端口检查 =================

/// 读监听的端口(需要 root 才能看到别的用户的进程名)
pub const PORTS_SCRIPT: &str = "PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH\ncommand -v ss >/dev/null 2>&1 || { echo '服务器上没有 ss 命令(iproute2),没法读取监听端口' >&2; exit 127; }\nss -tulnpH 2>&1\n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listen {
    /// `tcp` / `udp`
    pub proto: String,
    pub addr: String,
    pub port: u16,
    pub procs: Vec<(String, u32)>,
}

impl Listen {
    pub fn wildcard(&self) -> bool {
        matches!(self.addr.as_str(), "0.0.0.0" | "::" | "*")
    }
    pub fn loopback(&self) -> bool {
        self.addr.starts_with("127.") || self.addr == "::1"
    }
}

/// `users:(("sshd",pid=744,fd=3),("nginx",pid=9,fd=6))` → [(sshd,744),(nginx,9)]
fn parse_users(s: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("(\"") {
        rest = &rest[i + 2..];
        let Some(j) = rest.find('"') else { break };
        let name = rest[..j].to_string();
        rest = &rest[j + 1..];
        let pid = rest.strip_prefix(",pid=").map(|r| r.chars().take_while(char::is_ascii_digit).collect::<String>()).and_then(|d| d.parse().ok()).unwrap_or(0);
        out.push((name, pid));
    }
    out
}

/// `ss -tulnpH` 的输出。每行:`netid state recv-q send-q local peer [users:(...)]`
pub fn parse_listens(text: &str) -> Vec<Listen> {
    let mut v = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 5 || !matches!(f[0], "tcp" | "udp") {
            continue;
        }
        let Some((host, port)) = f[4].rsplit_once(':') else { continue };
        let Ok(port) = port.parse::<u16>() else { continue };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let host = host.split('%').next().unwrap_or(host);
        let addr = if host == "*" { "0.0.0.0" } else { host };
        let procs = f.get(6..).map(|u| parse_users(&u.join(" "))).unwrap_or_default();
        v.push(Listen { proto: f[0].to_string(), addr: addr.to_string(), port, procs });
    }
    v
}

/// 同协议同端口(v4/v6、多个地址)合并成一行
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenGroup {
    pub proto: String,
    pub port: u16,
    pub addrs: Vec<String>,
    pub procs: Vec<(String, u32)>,
}

impl ListenGroup {
    /// 只有本机能连:所有地址都是回环
    pub fn local_only(&self) -> bool {
        !self.addrs.is_empty() && self.addrs.iter().all(|a| a.starts_with("127.") || a == "::1")
    }
    /// 地址的简短显示:`0.0.0.0`、`127.0.0.1`、`0.0.0.0 + ::`
    pub fn addr_text(&self) -> String {
        self.addrs.join(" + ")
    }
}

pub fn group_listens(v: &[Listen]) -> Vec<ListenGroup> {
    let mut out: Vec<ListenGroup> = Vec::new();
    for l in v {
        let i = match out.iter().position(|g| g.proto == l.proto && g.port == l.port) {
            Some(i) => i,
            None => {
                out.push(ListenGroup { proto: l.proto.clone(), port: l.port, addrs: Vec::new(), procs: Vec::new() });
                out.len() - 1
            }
        };
        let g = &mut out[i];
        if !g.addrs.contains(&l.addr) {
            g.addrs.push(l.addr.clone());
        }
        for p in &l.procs {
            if !g.procs.contains(p) {
                g.procs.push(p.clone());
            }
        }
    }
    out.sort_by(|a, b| a.port.cmp(&b.port).then(a.proto.cmp(&b.proto)));
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Good,
    Info,
    Warn,
    Bad,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub text: String,
}

fn f(level: Level, text: impl Into<String>) -> Finding {
    Finding { level, text: text.into() }
}

/// 防火墙规则的目标(`22/tcp`、`22`、`8000-8100/udp`、`8000:8100/tcp`)是否覆盖这个端口和协议;服务名规则不匹配
fn rule_covers(to: &str, port: u16, proto: &str) -> bool {
    let (p, pr) = match to.split_once('/') {
        Some((p, pr)) => (p, Some(pr)),
        None => (to, None),
    };
    if pr.is_some_and(|x| x != proto && x != "tcp+udp") {
        return false;
    }
    firewall::parse_port(p).is_some_and(|(a, b)| (a..=b).contains(&port))
}

/// 主机防火墙对「外部访问这个端口」的结论
fn host_firewall(info: &FwInfo, port: u16, proto: &str) -> Finding {
    let front = info.manage();
    let (on, policy_open, label) = if info.ufw_active {
        (true, !matches!(info.default_in.as_str(), "deny" | "reject"), "ufw")
    } else if info.firewalld_active {
        // firewalld 的区域默认拒绝没列出的端口
        (true, false, "firewalld")
    } else if info.ipt_live {
        (true, !info.input_policy.eq_ignore_ascii_case("DROP"), "iptables(Otter 规则链)")
    } else {
        (false, true, "")
    };
    if !on {
        // 面板等写的 iptables 规则
        let hit = info.ext_rules.iter().find(|r| (r.proto == proto || r.proto.is_empty() || r.proto == "all") && rule_covers(&r.port.replace('-', ":"), port, proto));
        if let Some(r) = hit {
            return match r.action {
                FwAction::Allow => f(Level::Good, format!("iptables({})里有放行这个端口的规则", r.chain)),
                _ => f(Level::Bad, format!("iptables({})里有{}这个端口的规则", r.chain, r.action.label())),
            };
        }
        if info.input_policy.eq_ignore_ascii_case("DROP") {
            return f(Level::Warn, "iptables 的 INPUT 链默认策略是 DROP,没有找到放行这个端口的规则,外部可能连不上");
        }
        let _ = front;
        return f(Level::Info, "没有启用主机防火墙(ufw / firewalld),不会拦截这个端口");
    }
    let mut allow: Option<&firewall::Rule> = None;
    for r in info.rules.iter().filter(|r| !r.v6 && rule_covers(&r.to, port, proto)) {
        match r.action {
            FwAction::Deny | FwAction::Reject if r.from == "Anywhere" || r.from.is_empty() => {
                return f(Level::Bad, format!("{label} 有{}这个端口的规则({}),外部连不上", r.action.label(), r.to));
            }
            FwAction::Allow | FwAction::Limit => {
                if allow.is_none() || (r.from == "Anywhere" || r.from.is_empty()) {
                    allow = Some(r);
                }
            }
            _ => {}
        }
    }
    match allow {
        Some(r) if r.from == "Anywhere" || r.from.is_empty() => f(Level::Good, format!("{label} 放行了这个端口({})", r.to)),
        Some(r) => f(Level::Warn, format!("{label} 只允许来源 {} 访问这个端口({})", r.from, r.to)),
        None if policy_open => f(Level::Good, format!("{label} 没有针对这个端口的规则,默认策略是放行")),
        None => f(Level::Bad, format!("{label} 没有放行这个端口的规则,默认会拒绝外部访问;到「防火墙」页新增一条放行规则")),
    }
}

fn container_matches(c: &Container, port: u16, proto: &str) -> Vec<crate::docker::Publish> {
    c.publish.iter().filter(|p| (p.proto.is_empty() || p.proto == proto) && firewall::parse_port(&p.host).is_some_and(|(a, b)| (a..=b).contains(&port))).cloned().collect()
}

/// 诊断一个端口(TCP):有没有程序监听、监听在哪、是不是容器发布的、防火墙放不放行。
/// 第一条是总结,后面是每一层的判断。`fw` 是 None 表示没读到防火墙状态。
pub fn diagnose(port: u16, proto: &str, listens: &[Listen], containers: &[Container], fw: Option<&FwInfo>) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    let groups: Vec<ListenGroup> = group_listens(&listens.iter().filter(|l| l.port == port && l.proto == proto).cloned().collect::<Vec<_>>());
    let pubs: Vec<(&Container, crate::docker::Publish)> = containers.iter().filter(|c| c.running()).flat_map(|c| container_matches(c, port, proto).into_iter().map(move |p| (c, p))).collect();

    let mut reachable = true;
    // 1. 监听
    match groups.first() {
        None if pubs.is_empty() => {
            out.push(f(Level::Bad, format!("没有程序在监听 {port}/{proto}:服务没启动,或者它监听的是别的端口")));
            reachable = false;
        }
        None => {}
        Some(g) => {
            let who = if g.procs.is_empty() { "(进程名需要 root 才能读到)".to_string() } else { g.procs.iter().map(|(n, p)| format!("{n}(PID {p})")).collect::<Vec<_>>().join("、") };
            if g.local_only() {
                out.push(f(Level::Bad, format!("{who} 只监听在 {},只有服务器本机能连;要让外部访问,把它的监听地址改成 0.0.0.0", g.addr_text())));
                reachable = false;
            } else {
                out.push(f(Level::Good, format!("{who} 监听在 {}", g.addr_text())));
            }
        }
    }
    // 2. 容器
    for (c, p) in &pubs {
        let to = format!("{}:{} → 容器 {} 的 {}/{}", if p.ip.is_empty() { "0.0.0.0" } else { &p.ip }, p.host, c.name, p.container, p.proto);
        if p.ip.starts_with("127.") || p.ip == "::1" {
            out.push(f(Level::Bad, format!("容器发布的端口只绑定在本机:{to}")));
            reachable = false;
        } else {
            out.push(f(Level::Info, format!("Docker 发布:{to}")));
        }
    }
    // 3. 防火墙
    match fw {
        None => out.push(f(Level::Info, "没读到防火墙状态,跳过这一步(到「防火墙」页查看)")),
        Some(info) => {
            let docker_published = !pubs.is_empty();
            if docker_published {
                // 容器发布的端口走转发链,ufw / firewalld 的主机规则管不到
                match info.du.iter().find(|l| l.port == port && l.proto == proto) {
                    Some(l) => match &l.mode {
                        firewall::PortMode::Deny => {
                            out.push(f(Level::Bad, "容器端口防护:已禁止外部访问这个端口(本机和 Docker 内部网络不受影响)"));
                            reachable = false;
                        }
                        firewall::PortMode::Only(src) => out.push(f(Level::Warn, format!("容器端口防护:只允许来源 {} 访问这个端口", src.join("、")))),
                    },
                    None => out.push(f(Level::Info, "容器发布的端口走转发链,不受 ufw / firewalld 的主机规则管;要限制来源,到「防火墙 → 容器端口防护」设置")),
                }
            } else {
                let v = host_firewall(info, port, proto);
                if v.level == Level::Bad {
                    reachable = false;
                }
                out.push(v);
            }
        }
    }
    // 总结放最前
    let sum = if groups.is_empty() && pubs.is_empty() {
        f(Level::Bad, format!("{port}/{proto}:没有程序监听,连不上"))
    } else if reachable {
        f(Level::Good, format!("{port}/{proto}:有程序在监听,没有发现拦截;如果外部仍然连不上,检查云厂商的安全组"))
    } else {
        f(Level::Bad, format!("{port}/{proto}:外部很可能连不上,原因见下"))
    };
    out.insert(0, sum);
    out
}

// ================= 网络诊断 =================

/// 主机名或 IP(含 IPv6):只允许字母数字和 `.-:_`,不以 `-` 开头
pub fn valid_host(s: &str) -> bool {
    !s.is_empty() && s.len() <= 253 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetTool {
    Ping,
    Trace,
    /// 记录类型,解析服务器(空是用系统默认的)
    Dns(String, String),
    Tcp(u16),
    PublicIp,
    /// 本机网卡上的地址(不联网)
    LocalIps,
}

pub const DNS_TYPES: [&str; 6] = ["A", "AAAA", "MX", "TXT", "CNAME", "NS"];

/// 一次诊断的脚本:输出最后固定跟着 `@@MISSING=命令`(缺命令时)和 `@@EXIT=退出码`
pub fn net_script(tool: &NetTool, host: &str) -> Result<String, String> {
    let mut s = String::from(PATH_FIX);
    s.push_str(T_FN);
    let need = |cmds: &str, alt: &str| format!("have=\nfor c in {cmds}; do command -v $c >/dev/null 2>&1 && {{ have=$c; break; }}; done\n[ -n \"$have\" ] || {{ echo \"@@MISSING={alt}\"; echo '@@EXIT=127'; exit 0; }}\n");
    match tool {
        NetTool::PublicIp => {
            // 不需要主机:服务器去问第三方查询服务
            s.push_str(&need("curl wget", "curl 或 wget"));
            // 只认看起来像 IP 的一行;失败的原因不输出,界面只说「没查到」
            s.push_str("for u in https://api.ipify.org https://ifconfig.me/ip https://icanhazip.com; do\n  if [ \"$have\" = curl ]; then r=$(T 10 curl -fsS -m 8 \"$u\" 2>/dev/null); else r=$(T 10 wget -qO- -T 8 \"$u\" 2>/dev/null); fi\n  c=$?\n  r=$(echo \"$r\" | head -1 | tr -d ' \\r')\n  case \"$r\" in ''|*[!0-9a-fA-F:.]*) c=1;; esac\n  if [ $c = 0 ]; then echo \"$r\"; echo \"@@EXIT=0\"; exit 0; fi\ndone\necho '@@EXIT=1'\n");
            return Ok(s);
        }
        NetTool::LocalIps => {
            s.push_str("ip -o addr show scope global 2>/dev/null | awk '{print $2, $4}'\necho '@@EXIT=0'\n");
            return Ok(s);
        }
        _ if !valid_host(host) => return Err("主机名只能包含字母、数字和 . - : _".into()),
        _ => {}
    }
    let h = crate::sysops::quote(host);
    match tool {
        NetTool::Ping => {
            s.push_str(&need("ping", "ping"));
            s.push_str(&format!("T 20 ping -c 4 -W 2 {h} 2>&1; echo \"@@EXIT=$?\"\n"));
        }
        NetTool::Trace => {
            s.push_str(&need("traceroute tracepath", "traceroute 或 tracepath"));
            s.push_str(&format!("if [ \"$have\" = traceroute ]; then T 45 traceroute -n -w 2 -q 1 -m 20 {h} 2>&1; else T 45 tracepath -n -m 20 {h} 2>&1; fi; echo \"@@EXIT=$?\"\n"));
        }
        NetTool::Dns(ty, server) => {
            if !DNS_TYPES.contains(&ty.as_str()) {
                return Err("不支持这种记录类型".into());
            }
            if !server.is_empty() && !valid_host(server) {
                return Err("解析服务器地址不对".into());
            }
            s.push_str("echo \"系统解析服务器:$(grep -E '^nameserver' /etc/resolv.conf 2>/dev/null | awk '{print $2}' | tr '\\n' ' ')\"\n");
            s.push_str(&need("dig nslookup getent", "dig、nslookup 或 getent"));
            let srv = if server.is_empty() { String::new() } else { format!("@{}", crate::sysops::quote(server)) };
            let ns_srv = if server.is_empty() { String::new() } else { crate::sysops::quote(server) };
            s.push_str(&format!(
                "case $have in\n  dig) T 15 dig +time=3 +tries=1 +noall +answer +stats {srv} {h} {ty} 2>&1; c=$?;;\n  nslookup) T 15 nslookup -type={ty} {h} {ns_srv} 2>&1; c=$?;;\n  *) T 15 getent ahosts {h} 2>&1; c=$?; [ $c = 2 ] && echo '解析不出这个域名';;\nesac\necho \"@@EXIT=$c\"\n"
            ));
        }
        NetTool::Tcp(port) => {
            if *port == 0 {
                return Err("端口要在 1-65535 之间".into());
            }
            s.push_str(&format!(
                "t0=$(date +%s%N 2>/dev/null)\nif command -v bash >/dev/null 2>&1; then T 8 bash -c 'exec 3<>/dev/tcp/\"$0\"/\"$1\"' {h} {port} 2>/dev/null; c=$?\nelif command -v nc >/dev/null 2>&1; then T 8 nc -z -w 5 {h} {port} >/dev/null 2>&1; c=$?\nelse echo '@@MISSING=bash 或 nc'; echo '@@EXIT=127'; exit 0; fi\nt1=$(date +%s%N 2>/dev/null)\nif [ $c = 0 ]; then echo \"{host}:{port} 已连通\"; else echo \"{host}:{port} 连不上(超时、被拒绝或被防火墙挡住)\"; fi\n[ -n \"$t0\" ] && [ -n \"$t1\" ] && echo \"用时 $(( (t1 - t0) / 1000000 )) ms\"\necho \"@@EXIT=$c\"\n"
            ));
        }
        NetTool::PublicIp | NetTool::LocalIps => unreachable!(),
    }
    Ok(s)
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct NetResult {
    pub text: String,
    pub code: i32,
    /// 服务器上缺的命令
    pub missing: Option<String>,
}

/// 本机网卡地址:(网卡, 地址),去掉掩码和 Docker/虚拟网卡
pub fn parse_nics(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (n, a) = (f.next()?, f.next()?);
            if n.starts_with("docker") || n.starts_with("br-") || n.starts_with("veth") || n.starts_with("cni") || n.starts_with("flannel") || n.starts_with("lo") {
                return None;
            }
            Some((n.to_string(), a.split('/').next()?.to_string()))
        })
        .collect()
}

pub fn parse_net(out: &str) -> NetResult {
    let mut r = NetResult { code: -1, ..Default::default() };
    let mut lines: Vec<&str> = Vec::new();
    for l in out.lines() {
        if let Some(v) = l.strip_prefix("@@EXIT=") {
            r.code = v.trim().parse().unwrap_or(-1);
        } else if let Some(v) = l.strip_prefix("@@MISSING=") {
            r.missing = Some(v.trim().to_string());
        } else {
            lines.push(l);
        }
    }
    r.text = lines.join("\n").trim_end().to_string();
    r
}

// ================= 时间与时区 =================

pub const TIME_SCRIPT: &str = r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH
echo "@@EPOCH=$(date +%s)"
echo "@@DATE=$(date '+%Y-%m-%d %H:%M:%S %z')"
if command -v timedatectl >/dev/null 2>&1; then
  echo '@@TDC=1'
  timedatectl show 2>/dev/null
else
  echo '@@TDC=0'
  echo "Timezone=$(cat /etc/timezone 2>/dev/null || readlink /etc/localtime 2>/dev/null | sed 's|.*/zoneinfo/||')"
fi
for s in systemd-timesyncd chronyd chrony ntpd ntp; do
  if systemctl cat $s.service >/dev/null 2>&1; then echo "@@SVC=$s:$(systemctl is-active $s.service 2>/dev/null)"; fi
done
b=
for s in systemd-timesyncd chronyd chrony; do systemctl is-active -q $s.service 2>/dev/null && { b=$s; break; }; done
if [ -z "$b" ]; then for s in systemd-timesyncd chronyd chrony; do systemctl cat $s.service >/dev/null 2>&1 && { b=$s; break; }; done; fi
echo "@@NTPB=$b"
case $b in
  systemd-timesyncd)
    echo "@@NTPSRV=$(grep -hE '^NTP=' /etc/systemd/timesyncd.conf /etc/systemd/timesyncd.conf.d/*.conf 2>/dev/null | tail -1 | cut -d= -f2)"
    [ -f /etc/systemd/timesyncd.conf.d/99-otter.conf ] && echo '@@NTPOTTER=1';;
  chronyd|chrony)
    f=/etc/chrony/chrony.conf; [ -f $f ] || f=/etc/chrony.conf
    echo "@@NTPSRV=$(grep -hE '^[[:space:]]*(server|pool)[[:space:]]' $f 2>/dev/null | awk '{print $2}' | tr '\n' ' ')"
    grep -q '# otter$' $f 2>/dev/null && echo '@@NTPOTTER=1';;
esac
echo '@@ZONES'
timedatectl list-timezones 2>/dev/null
exit 0
"#;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TimeInfo {
    /// 服务器当前的 Unix 时间(读取那一刻)
    pub epoch: i64,
    /// `2026-11-26 14:03:12 +0800`
    pub date: String,
    pub tz: String,
    pub has_timedatectl: bool,
    /// NTP 服务是否启用(timedatectl 的 `NTP=`)
    pub ntp: Option<bool>,
    /// 系统时钟是否已经和时间服务器同步
    pub synced: Option<bool>,
    pub can_ntp: Option<bool>,
    /// 机器上有的 NTP 服务和是否在运行
    pub services: Vec<(String, bool)>,
    pub zones: Vec<String>,
    /// 在用(或装着)的时间服务:`systemd-timesyncd` / `chronyd` / `chrony`;空是没有
    pub ntp_backend: String,
    /// 配置里的 NTP 服务器(空是用系统默认)
    pub ntp_servers: Vec<String>,
    /// 服务器是 Otter 设的(drop-in 文件或带 `# otter` 的行)
    pub ntp_by_otter: bool,
}

pub fn parse_time(out: &str) -> Result<TimeInfo, String> {
    let mut t = TimeInfo::default();
    let mut in_zones = false;
    for l in out.lines() {
        let l = l.trim_end();
        if in_zones {
            if !l.trim().is_empty() {
                t.zones.push(l.trim().to_string());
            }
            continue;
        }
        if l == "@@ZONES" {
            in_zones = true;
        } else if let Some(v) = l.strip_prefix("@@EPOCH=") {
            t.epoch = v.trim().parse().map_err(|_| "读不到服务器时间".to_string())?;
        } else if let Some(v) = l.strip_prefix("@@DATE=") {
            t.date = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("@@TDC=") {
            t.has_timedatectl = v.trim() == "1";
        } else if let Some(v) = l.strip_prefix("@@NTPB=") {
            t.ntp_backend = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("@@NTPSRV=") {
            t.ntp_servers = v.split_whitespace().map(str::to_string).collect();
        } else if l.starts_with("@@NTPOTTER=") {
            t.ntp_by_otter = true;
        } else if let Some(v) = l.strip_prefix("@@SVC=") {
            if let Some((n, st)) = v.split_once(':') {
                t.services.push((n.to_string(), st.trim() == "active"));
            }
        } else if let Some((k, v)) = l.split_once('=') {
            let yes = |v: &str| Some(v.trim() == "yes");
            match k {
                "Timezone" => t.tz = v.trim().to_string(),
                "NTP" => t.ntp = yes(v),
                "NTPSynchronized" => t.synced = yes(v),
                "CanNTP" => t.can_ntp = yes(v),
                _ => {}
            }
        }
    }
    if t.epoch == 0 {
        return Err("读不到服务器时间".into());
    }
    // Debian 上 chrony.service 是 chronyd.service 的别名,只留一个
    if t.services.iter().any(|(n, _)| n == "chronyd") {
        t.services.retain(|(n, _)| n != "chrony");
    }
    Ok(t)
}

/// 把 `2026-11-27 14:11:52 +0800` 往后推 `secs` 秒(页面上让服务器时间自己走,不用每秒去读);格式不对原样返回
pub fn shift_date(date: &str, secs: i64) -> String {
    let (day, rest) = match date.split_once(' ') {
        Some(x) => x,
        None => return date.to_string(),
    };
    let (clock, zone) = rest.split_once(' ').unwrap_or((rest, ""));
    let d: Vec<i64> = day.split('-').filter_map(|x| x.parse().ok()).collect();
    let c: Vec<i64> = clock.split(':').filter_map(|x| x.parse().ok()).collect();
    if d.len() != 3 || c.len() != 3 {
        return date.to_string();
    }
    // 天数 <-> 公历日期(Howard Hinnant 的算法)
    let (y, m, dd) = (d[0], d[1], d[2]);
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + dd - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let total = days * 86400 + c[0] * 3600 + c[1] * 60 + c[2] + secs;
    let (days, sod) = (total.div_euclid(86400), total.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day_n = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day_n:02} {:02}:{:02}:{:02} {zone}", sod / 3600, sod % 3600 / 60, sod % 60).trim_end().to_string()
}

/// 时区名:`Asia/Shanghai`、`UTC`、`America/Argentina/Buenos_Aires`、`Etc/GMT+8`
pub fn valid_tz(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && !s.contains("..") && !s.starts_with('/') && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '+' | '-'))
}

pub fn set_tz_script(tz: &str) -> Result<String, String> {
    if !valid_tz(tz) {
        return Err("时区名不对".into());
    }
    Ok(format!("PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH\ncommand -v timedatectl >/dev/null 2>&1 || {{ echo '服务器上没有 timedatectl' >&2; exit 1; }}\ntimedatectl set-timezone {} 2>&1\n", crate::sysops::quote(tz)))
}

pub fn set_ntp_script(on: bool) -> String {
    let want = if on { "yes" } else { "no" };
    format!(
        "PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH\ncommand -v timedatectl >/dev/null 2>&1 || {{ echo '服务器上没有 timedatectl' >&2; exit 1; }}\ntimedatectl set-ntp {} 2>&1 || exit 1\n# set-ntp 在没有可用的时间服务(容器、没装 chrony/timesyncd)时也返回成功,状态却不变:回读确认\nsleep 1\n[ \"$(timedatectl show -p NTP --value 2>/dev/null)\" = {want} ] || {{ echo '设置没有生效:服务器上没有能运行的时间同步服务(chrony、systemd-timesyncd),或它在这个环境里起不来' >&2; exit 1; }}\n",
        if on { "true" } else { "false" }
    )
}

/// 常用 NTP 服务器:(地址, 说明)
pub const NTP_PRESETS: [(&str, &str); 6] = [
    ("ntp.aliyun.com", "阿里云"),
    ("ntp.tencent.com", "腾讯云"),
    ("time.cloudflare.com", "Cloudflare"),
    ("time.google.com", "Google"),
    ("pool.ntp.org", "NTP Pool"),
    ("cn.pool.ntp.org", "NTP Pool 中国"),
];

/// 改 NTP 服务器(`None` 是改回系统默认)。timesyncd 写 drop-in 文件;chrony 把原有的 server/pool 行注释成
/// `# otter-off …` 再追加 `server X iburst # otter`,改回默认时恢复。重启后服务没起来就还原。
pub fn set_ntp_server_script(backend: &str, server: Option<&str>) -> Result<String, String> {
    if let Some(h) = server {
        if !valid_host(h) {
            return Err("NTP 服务器地址不对".into());
        }
    }
    let head = "PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH\n";
    match backend {
        "systemd-timesyncd" => {
            let d = "/etc/systemd/timesyncd.conf.d";
            let put = match server {
                Some(h) => format!("mkdir -p {d} && printf '[Time]\\nNTP=%s\\n' {} > {d}/99-otter.conf", crate::sysops::quote(h)),
                None => format!("rm -f {d}/99-otter.conf"),
            };
            Ok(format!("{head}{put} || exit 1\nsystemctl restart systemd-timesyncd 2>&1\nsleep 1\nsystemctl is-active -q systemd-timesyncd || {{ rm -f {d}/99-otter.conf; systemctl restart systemd-timesyncd 2>/dev/null; echo '时间服务重启失败,已改回原来的设置' >&2; exit 1; }}\n"))
        }
        "chronyd" | "chrony" => {
            let edit = match server {
                Some(h) => format!("sed -i -E 's/^([[:space:]]*(server|pool)[[:space:]])/# otter-off \\1/' \"$f\"\nprintf 'server %s iburst # otter\\n' {} >> \"$f\"\n", crate::sysops::quote(h)),
                None => String::new(),
            };
            Ok(format!("{head}f=/etc/chrony/chrony.conf; [ -f \"$f\" ] || f=/etc/chrony.conf\n[ -f \"$f\" ] || {{ echo '找不到 chrony 的配置文件' >&2; exit 1; }}\ncp -n \"$f\" \"$f.otter-bak\"; cp \"$f\" \"$f.otter-prev\" || exit 1\nsed -i -e '/# otter$/d' -e 's/^# otter-off //' \"$f\"\n{edit}systemctl restart {backend} 2>&1\nsleep 1\nsystemctl is-active -q {backend} || {{ cp \"$f.otter-prev\" \"$f\"; systemctl restart {backend} 2>/dev/null; echo '时间服务重启失败,已改回原来的设置' >&2; exit 1; }}\n"))
        }
        _ => Err("服务器上没有可改的时间服务(chrony、systemd-timesyncd)".into()),
    }
}

// ================= Swap =================

pub const SWAP_SCRIPT: &str = r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH
echo '@@MEM'
grep -E '^(MemTotal|MemAvailable|SwapTotal|SwapFree):' /proc/meminfo
echo '@@SWAPS'
cat /proc/swaps 2>/dev/null
echo '@@FSTAB'
grep -E '^[[:space:]]*(#[[:space:]]*otter-off[[:space:]]+)?[^#[:space:]][^[:space:]]*[[:space:]]+[^[:space:]]+[[:space:]]+swap[[:space:]]' /etc/fstab 2>/dev/null
echo '@@SWAPPINESS'
cat /proc/sys/vm/swappiness 2>/dev/null
echo '@@ROOT'
df -PkT / 2>/dev/null | tail -n 1
echo '@@MANAGED'
cat /etc/sysctl.d/99-otter-swap.conf 2>/dev/null
exit 0
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapKind {
    File,
    Partition,
    Zram,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapDev {
    /// 文件路径或设备
    pub path: String,
    pub kind: SwapKind,
    pub size_kb: u64,
    pub used_kb: u64,
    pub prio: i32,
    /// `/etc/fstab` 里有它(重启后还在)
    pub in_fstab: bool,
    /// fstab 那一行带 `# otter`(Otter 创建)
    pub otter: bool,
}

/// fstab 里有、但现在没启用的 swap:停用后保留了这一行(带 `# otter-off` 标记),或者本来就没启用
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapOff {
    pub spec: String,
    /// 是 Otter 停用的(行首有 `# otter-off`),启用时恢复
    pub by_otter: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SwapInfo {
    pub devs: Vec<SwapDev>,
    pub off: Vec<SwapOff>,
    pub mem_total_kb: u64,
    pub mem_avail_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
    pub swappiness: Option<u32>,
    pub root_fs: String,
    pub root_avail_kb: u64,
    /// Otter 设过 swappiness(`/etc/sysctl.d/99-otter-swap.conf` 存在)
    pub swappiness_set: bool,
}

fn kv(line: &str) -> Option<u64> {
    line.split_whitespace().nth(1)?.parse().ok()
}

pub fn parse_swap(out: &str) -> Result<SwapInfo, String> {
    let mut info = SwapInfo::default();
    let mut sec = "";
    let mut fstab: Vec<(String, bool, bool)> = Vec::new(); // (第一列, 是 otter-off, 带 # otter)
    let mut seen_mem = false;
    for l in out.lines() {
        if let Some(s) = l.strip_prefix("@@") {
            sec = match s.trim() {
                "MEM" => "mem",
                "SWAPS" => "swaps",
                "FSTAB" => "fstab",
                "SWAPPINESS" => "sw",
                "ROOT" => "root",
                "MANAGED" => "managed",
                _ => "",
            };
            continue;
        }
        match sec {
            "mem" => {
                seen_mem = true;
                match l.split(':').next().unwrap_or("") {
                    "MemTotal" => info.mem_total_kb = kv(l).unwrap_or(0),
                    "MemAvailable" => info.mem_avail_kb = kv(l).unwrap_or(0),
                    "SwapTotal" => info.swap_total_kb = kv(l).unwrap_or(0),
                    "SwapFree" => info.swap_free_kb = kv(l).unwrap_or(0),
                    _ => {}
                }
            }
            "swaps" => {
                let fl: Vec<&str> = l.split_whitespace().collect();
                if fl.len() < 5 || fl[0] == "Filename" {
                    continue;
                }
                let kind = if fl[0].starts_with("/dev/zram") {
                    SwapKind::Zram
                } else if fl[1] == "file" {
                    SwapKind::File
                } else {
                    SwapKind::Partition
                };
                info.devs.push(SwapDev { path: fl[0].to_string(), kind, size_kb: fl[2].parse().unwrap_or(0), used_kb: fl[3].parse().unwrap_or(0), prio: fl[4].parse().unwrap_or(0), in_fstab: false, otter: false });
            }
            "fstab" => {
                let off = l.trim_start().starts_with('#');
                let body = l.trim_start().trim_start_matches('#').trim_start();
                let body = body.strip_prefix("otter-off").map(str::trim_start).unwrap_or(body);
                let first = body.split_whitespace().next().unwrap_or("").to_string();
                if !first.is_empty() {
                    fstab.push((first, off, l.contains("# otter") && !off || l.contains("#otter")));
                }
            }
            "sw" => info.swappiness = l.trim().parse().ok(),
            "root" => {
                let fl: Vec<&str> = l.split_whitespace().collect();
                if fl.len() >= 6 {
                    info.root_fs = fl[1].to_string();
                    info.root_avail_kb = fl[4].parse().unwrap_or(0);
                }
            }
            "managed" => {
                if l.contains("vm.swappiness") {
                    info.swappiness_set = true;
                }
            }
            _ => {}
        }
    }
    if !seen_mem {
        return Err("读不到内存信息".into());
    }
    for d in &mut info.devs {
        if let Some((_, _, otter)) = fstab.iter().find(|(p, off, _)| p == &d.path && !*off) {
            d.in_fstab = true;
            d.otter = *otter;
        }
    }
    for (spec, off, _) in &fstab {
        if *off || !info.devs.iter().any(|d| &d.path == spec) {
            // 没启用的:UUID= 之类的 spec 对不上 /proc/swaps,只在确实没启用的情况下列出
            if *off || !spec.starts_with("UUID=") && !spec.starts_with("LABEL=") && !spec.starts_with("PARTUUID=") {
                info.off.push(SwapOff { spec: spec.clone(), by_otter: *off });
            }
        }
    }
    Ok(info)
}

/// 路径:绝对路径,只含 `[A-Za-z0-9._/-]`,不含 `..`,不在 /proc /sys /dev 下
pub fn valid_swap_path(p: &str) -> bool {
    p.starts_with('/')
        && p.len() <= 200
        && !p.contains("..")
        && !p.ends_with('/')
        && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
        && !["/proc", "/sys", "/dev", "/run", "/tmp"].iter().any(|d| p == *d || p.starts_with(&format!("{d}/")))
}

pub const SWAP_MIN_MB: u64 = 64;
pub const SWAP_MAX_MB: u64 = 131_072;

/// 建 swap 文件的公共片段:检查文件系统和空间 → 分配 → mkswap。
/// 变量:`$P`(目标文件)、`$MB`(大小);`extra` 是同一磁盘上还要额外留的空间(调整大小时旧文件还在)
fn alloc_snippet() -> &'static str {
    r#"D=$(dirname "$P")
[ -d "$D" ] || { echo "目录 $D 不存在" >&2; exit 1; }
FS=$(stat -f -c %T "$D" 2>/dev/null)
case "$FS" in
  tmpfs|overlay|overlayfs|zfs|nfs|nfs4|cifs|smb*|fuse*|ramfs) echo "这个位置的文件系统($FS)不能放 swap 文件,换到 ext4/xfs/btrfs 的目录" >&2; exit 1;;
esac
AVAIL=$(df -Pk "$D" | awk 'NR==2{print $4}')
NEED=$((MB * 1024 + 1048576 + ${EXTRA_KB:-0}))
[ "$AVAIL" -ge "$NEED" ] || { echo "磁盘空间不够:剩 $((AVAIL / 1024)) MB,需要 $((NEED / 1024)) MB(含 1 GB 余量)" >&2; exit 1; }
alloc() {
  rm -f "$1"
  if [ "$FS" = btrfs ]; then : > "$1" && chattr +C "$1" 2>/dev/null; dd if=/dev/zero of="$1" bs=1M count=$MB status=none || return 1
  else fallocate -l ${MB}M "$1" 2>/dev/null || dd if=/dev/zero of="$1" bs=1M count=$MB status=none || return 1
  fi
  chmod 600 "$1" && mkswap "$1" >/dev/null
}
"#
}

pub fn swap_create_script(path: &str, mb: u64) -> Result<String, String> {
    if !valid_swap_path(path) {
        return Err("路径只能是绝对路径,只含字母、数字和 . _ / -,不能放在 /tmp、/dev 等位置".into());
    }
    if !(SWAP_MIN_MB..=SWAP_MAX_MB).contains(&mb) {
        return Err(format!("大小要在 {SWAP_MIN_MB} MB 到 {} GB 之间", SWAP_MAX_MB / 1024));
    }
    Ok(format!(
        "{PATH_FIX}P={p}\nMB={mb}\nset -e\n[ -e \"$P\" ] && {{ echo \"$P 已经存在\" >&2; exit 1; }}\n{alloc}done_ok=0\ncleanup() {{ [ \"$done_ok\" = 1 ] || {{ swapoff \"$P\" 2>/dev/null; rm -f \"$P\"; }}; }}\ntrap cleanup EXIT\nalloc \"$P\" || {{ echo '创建 swap 文件失败(磁盘写满或不支持)' >&2; exit 1; }}\n# 有的文件系统 fallocate 出来的文件带空洞,swapon 会拒绝:换成 dd 写满再试一次\nif ! swapon \"$P\" 2>/dev/null; then\n  rm -f \"$P\"; fb=1\n  : > \"$P\"; [ \"$FS\" = btrfs ] && chattr +C \"$P\" 2>/dev/null\n  dd if=/dev/zero of=\"$P\" bs=1M count=$MB status=none && chmod 600 \"$P\" && mkswap \"$P\" >/dev/null && swapon \"$P\"\nfi\ncp -p /etc/fstab /etc/fstab.otter-bak 2>/dev/null || true\nprintf '%s none swap sw 0 0 # otter\\n' \"$P\" >> /etc/fstab\ndone_ok=1\nsystemctl daemon-reload >/dev/null 2>&1 || true\necho \"已创建并启用 $P({mb} MB)\"\n",
        p = crate::sysops::quote(path),
        alloc = alloc_snippet(),
    ))
}

/// 把已用的 swap 装回物理内存会不会撑爆:要求可用内存 ≥ 这个 swap 的已用量 + 100 MB
const MEM_CHECK: &str = "used=$(awk -v p=\"$P\" '$1==p{print $4}' /proc/swaps)\n[ -n \"$used\" ] || { echo \"$P 现在没有启用\" >&2; exit 1; }\navail=$(awk '/^MemAvailable:/{print $2}' /proc/meminfo)\n[ \"$avail\" -ge $((used + 102400)) ] || { echo \"可用内存只有 $((avail / 1024)) MB,装不下 swap 里已用的 $((used / 1024)) MB,现在关掉会被系统强杀进程\" >&2; exit 1; }\n";

/// 从 fstab 里去掉指向这个设备的 swap 行(写前备份)
const FSTAB_DROP: &str = "cp -p /etc/fstab /etc/fstab.otter-bak 2>/dev/null || true\nawk -v p=\"$P\" '{ f=$1; if ($1==\"#\") f=$2; if (f==\"otter-off\") f=$3; if (!(f==p && ($3==\"swap\" || $4==\"swap\" || $5==\"swap\"))) print }' /etc/fstab.otter-bak > /etc/fstab.otter-new && cat /etc/fstab.otter-new > /etc/fstab; rm -f /etc/fstab.otter-new\n";

/// 把指向这个设备的 swap 行注释掉并打上 `# otter-off` 标记,以后能原样恢复
const FSTAB_OFF: &str = "cp -p /etc/fstab /etc/fstab.otter-bak 2>/dev/null || true\nawk -v p=\"$P\" '{ if ($1==p && $3==\"swap\") print \"# otter-off \" $0; else print }' /etc/fstab.otter-bak > /etc/fstab.otter-new && cat /etc/fstab.otter-new > /etc/fstab; rm -f /etc/fstab.otter-new\n";

/// 停用:`swapoff`,fstab 里的行注释掉(保留,可以再启用);文件不删
pub fn swap_off_script(path: &str) -> Result<String, String> {
    if !(valid_swap_path(path) || path.starts_with("/dev/") && path.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'))) || path.contains("..") {
        return Err("路径不对".into());
    }
    Ok(format!("{PATH_FIX}P={}\nset -e\n{MEM_CHECK}swapoff \"$P\"\n{FSTAB_OFF}systemctl daemon-reload >/dev/null 2>&1 || true\necho \"已停用 $P\"\n", crate::sysops::quote(path)))
}

/// 启用 fstab 里被 Otter 停用的 swap:去掉标记行首的 `# otter-off `,再 `swapon -a`
pub fn swap_on_script() -> String {
    format!("{PATH_FIX}set -e\ncp -p /etc/fstab /etc/fstab.otter-bak 2>/dev/null || true\nsed 's/^# otter-off //' /etc/fstab.otter-bak > /etc/fstab.otter-new && cat /etc/fstab.otter-new > /etc/fstab; rm -f /etc/fstab.otter-new\nsystemctl daemon-reload >/dev/null 2>&1 || true\nswapon -a 2>&1\necho '已启用 fstab 里的 swap'\n")
}

/// 启用某个没有记在 fstab 里的设备(临时,重启后不在)
pub fn swap_on_one_script(path: &str) -> Result<String, String> {
    if !valid_swap_path(path) {
        return Err("路径不对".into());
    }
    Ok(format!("{PATH_FIX}swapon {} 2>&1\n", crate::sysops::quote(path)))
}

/// 删除 swap 文件:`swapoff`、删 fstab 里的行、删文件。分区和 zram 不能删
pub fn swap_delete_script(path: &str) -> Result<String, String> {
    if !valid_swap_path(path) {
        return Err("只能删除 swap 文件,分区和 zram 只能停用".into());
    }
    Ok(format!("{PATH_FIX}P={}\nset -e\n[ -f \"$P\" ] || {{ echo \"$P 不是文件\" >&2; exit 1; }}\n{MEM_CHECK}swapoff \"$P\"\n{FSTAB_DROP}rm -f \"$P\"\nsystemctl daemon-reload >/dev/null 2>&1 || true\necho \"已删除 $P\"\n", crate::sysops::quote(path)))
}

/// 调整 swap 文件大小:先在旁边建好新文件,再 `swapoff` 旧的、换上新的、`swapon`
pub fn swap_resize_script(path: &str, mb: u64) -> Result<String, String> {
    if !valid_swap_path(path) {
        return Err("只能调整 swap 文件".into());
    }
    if !(SWAP_MIN_MB..=SWAP_MAX_MB).contains(&mb) {
        return Err(format!("大小要在 {SWAP_MIN_MB} MB 到 {} GB 之间", SWAP_MAX_MB / 1024));
    }
    Ok(format!(
        "{PATH_FIX}P={p}\nMB={mb}\nset -e\n[ -f \"$P\" ] || {{ echo \"$P 不是文件\" >&2; exit 1; }}\nEXTRA_KB=0\n{alloc}{MEM_CHECK}N=\"$P.otter-new\"\ntrap 'rm -f \"$N\"' EXIT\nalloc \"$N\" || {{ echo '创建新的 swap 文件失败' >&2; exit 1; }}\nswapoff \"$P\"\nmv -f \"$N\" \"$P\"\nswapon \"$P\" 2>&1 || {{ echo '新文件启用失败,请到服务器上检查' >&2; exit 1; }}\ntrap - EXIT\necho \"已把 $P 调整为 {mb} MB\"\n",
        p = crate::sysops::quote(path),
        alloc = alloc_snippet().replace("${EXTRA_KB:-0}", "$((OLD_KB))").replace("D=$(dirname", "OLD_KB=$(du -k \"$P\" | awk '{print $1}')\nD=$(dirname"),
    ))
}

pub fn set_swappiness_script(n: u32) -> Result<String, String> {
    if n > 100 {
        return Err("swappiness 要在 0 到 100 之间".into());
    }
    Ok(format!("{PATH_FIX}set -e\nmkdir -p /etc/sysctl.d\nprintf '# otter\\nvm.swappiness = {n}\\n' > /etc/sysctl.d/99-otter-swap.conf\nsysctl -w vm.swappiness={n} 2>&1\n"))
}

pub fn fmt_kb(kb: u64) -> String {
    let b = kb as f64 * 1024.0;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", b / 1073741824.0)
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.0} MB", b / 1048576.0)
    } else {
        format!("{kb} KB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SS: &str = "tcp   LISTEN 0 4096 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=744,fd=3))\ntcp   LISTEN 0 4096 [::]:22 [::]:* users:((\"sshd\",pid=744,fd=4))\ntcp   LISTEN 0 4096 127.0.0.1:5432 0.0.0.0:* users:((\"postgres\",pid=900,fd=5))\nudp   UNCONN 0 0 127.0.0.53%lo:53 0.0.0.0:*\ntcp   LISTEN 0 511 *:8080 *:* users:((\"docker-proxy\",pid=1,fd=4),(\"x\",pid=2,fd=1))\n";

    #[test]
    fn listens_parse_and_group() {
        let v = parse_listens(SS);
        assert_eq!(v.len(), 5);
        assert_eq!(v[3].addr, "127.0.0.53");
        assert_eq!(v[4].addr, "0.0.0.0");
        assert_eq!(v[4].procs, vec![("docker-proxy".to_string(), 1), ("x".to_string(), 2)]);
        let g = group_listens(&v);
        let ssh = g.iter().find(|x| x.port == 22).unwrap();
        assert_eq!(ssh.addrs, vec!["0.0.0.0", "::"]);
        assert_eq!(ssh.procs.len(), 1);
        assert!(g.iter().find(|x| x.port == 5432).unwrap().local_only());
        assert!(!ssh.local_only());
    }

    fn info_ufw(rules: Vec<firewall::Rule>, policy: &str) -> FwInfo {
        FwInfo { ufw_installed: true, ufw_active: true, default_in: policy.into(), rules, ..Default::default() }
    }

    fn rule(to: &str, a: FwAction, from: &str) -> firewall::Rule {
        firewall::Rule { kind: firewall::RuleKind::Port, zone: String::new(), to: to.into(), action: a, from: from.into(), v6: false, key: String::new() }
    }

    #[test]
    fn diagnose_cases() {
        let l = parse_listens(SS);
        // 监听在 0.0.0.0、ufw 放行
        let fw = info_ufw(vec![rule("22/tcp", FwAction::Allow, "Anywhere")], "deny");
        let d = diagnose(22, "tcp", &l, &[], Some(&fw));
        assert_eq!(d[0].level, Level::Good, "{d:?}");
        // 监听在 0.0.0.0、ufw 默认拒绝且没规则
        let d = diagnose(8080, "tcp", &l, &[], Some(&fw));
        assert_eq!(d[0].level, Level::Bad);
        assert!(d.iter().any(|x| x.text.contains("没有放行")), "{d:?}");
        // 只监听 127.0.0.1
        let d = diagnose(5432, "tcp", &l, &[], Some(&fw));
        assert_eq!(d[0].level, Level::Bad);
        assert!(d.iter().any(|x| x.text.contains("只监听在 127.0.0.1")));
        // 没有程序监听
        let d = diagnose(9999, "tcp", &l, &[], Some(&fw));
        assert!(d[0].text.contains("没有程序监听"));
        // 拒绝规则在前
        let fw2 = info_ufw(vec![rule("8080/tcp", FwAction::Deny, "Anywhere"), rule("8080/tcp", FwAction::Allow, "Anywhere")], "allow");
        assert_eq!(diagnose(8080, "tcp", &l, &[], Some(&fw2))[0].level, Level::Bad);
        // 只允许某个来源
        let fw3 = info_ufw(vec![rule("8080/tcp", FwAction::Allow, "10.0.0.0/8")], "deny");
        let d = diagnose(8080, "tcp", &l, &[], Some(&fw3));
        assert!(d.iter().any(|x| x.level == Level::Warn && x.text.contains("10.0.0.0/8")), "{d:?}");
        // 防火墙没启用
        let d = diagnose(22, "tcp", &l, &[], Some(&FwInfo::default()));
        assert_eq!(d[0].level, Level::Good);
    }

    #[test]
    fn rule_range_and_proto() {
        assert!(rule_covers("8000:8100/tcp", 8050, "tcp"));
        assert!(rule_covers("8000-8100/udp", 8050, "udp"));
        assert!(!rule_covers("8000-8100/udp", 8050, "tcp"));
        assert!(rule_covers("443", 443, "tcp"));
        assert!(!rule_covers("OpenSSH", 22, "tcp"));
    }

    #[test]
    fn docker_published_port() {
        let c = crate::docker::parse_ps("abc\tweb\tnginx\trunning\tUp 1h\t0.0.0.0:8080->80/tcp\t\t\t\t\t\n");
        let l = parse_listens(SS);
        let fw = info_ufw(vec![], "deny");
        let d = diagnose(8080, "tcp", &l, &c, Some(&fw));
        assert!(d.iter().any(|x| x.text.contains("容器 web")), "{d:?}");
        // 容器端口走转发链,ufw 默认拒绝不影响:不该报「没有放行」
        assert!(!d.iter().any(|x| x.text.contains("没有放行")), "{d:?}");
        let mut fw = fw;
        fw.du = vec![firewall::PortLimit { proto: "tcp".into(), port: 8080, mode: firewall::PortMode::Deny, live: true }];
        let d = diagnose(8080, "tcp", &l, &c, Some(&fw));
        assert_eq!(d[0].level, Level::Bad);
    }

    #[test]
    fn host_validation_and_scripts() {
        assert!(valid_host("example.com") && valid_host("2001:db8::1") && valid_host("10.0.0.1"));
        assert!(!valid_host("a;b") && !valid_host("-x") && !valid_host("") && !valid_host("a b"));
        assert!(net_script(&NetTool::Ping, "a;rm -rf /").is_err());
        assert!(net_script(&NetTool::Tcp(0), "a.com").is_err());
        assert!(net_script(&NetTool::Dns("ANY".into(), String::new()), "a.com").is_err());
        for t in [NetTool::Ping, NetTool::Trace, NetTool::Dns("A".into(), "1.1.1.1".into()), NetTool::Tcp(443), NetTool::PublicIp, NetTool::LocalIps] {
            let s = net_script(&t, "example.com").unwrap();
            let o = std::process::Command::new("sh").args(["-n", "-c", &s]).output().unwrap();
            assert!(o.status.success(), "{t:?}: {}", String::from_utf8_lossy(&o.stderr));
        }
    }

    #[test]
    fn nics() {
        let v = parse_nics("lo 127.0.0.1/8\neth0 10.0.4.12/24\ndocker0 172.17.0.1/16\nveth1a2 fe80::1/64\n");
        assert_eq!(v, vec![("eth0".to_string(), "10.0.4.12".to_string())]);
    }

    #[test]
    fn net_output() {
        let r = parse_net("PING a (1.1.1.1)\n4 packets\n@@EXIT=0\n");
        assert_eq!((r.code, r.missing.as_deref()), (0, None));
        assert!(!r.text.contains("@@"));
        let r = parse_net("@@MISSING=traceroute 或 tracepath\n@@EXIT=127\n");
        assert_eq!((r.code, r.missing.as_deref()), (127, Some("traceroute 或 tracepath")));
    }

    #[test]
    fn time_parse() {
        let out = "@@EPOCH=1790000000\n@@DATE=2026-11-26 14:03:12 +0800\n@@TDC=1\nTimezone=Asia/Shanghai\nLocalRTC=no\nCanNTP=yes\nNTP=yes\nNTPSynchronized=yes\n@@SVC=systemd-timesyncd:active\n@@SVC=chrony:inactive\n@@ZONES\nAsia/Shanghai\nUTC\n";
        let t = parse_time(out).unwrap();
        assert_eq!((t.epoch, t.tz.as_str(), t.ntp, t.synced, t.can_ntp), (1790000000, "Asia/Shanghai", Some(true), Some(true), Some(true)));
        assert_eq!(t.services, vec![("systemd-timesyncd".to_string(), true), ("chrony".to_string(), false)]);
        assert_eq!(t.zones, vec!["Asia/Shanghai", "UTC"]);
        assert!(parse_time("garbage").is_err());
        assert_eq!(shift_date("2026-11-27 14:11:52 +0800", 8), "2026-11-27 14:12:00 +0800");
        assert_eq!(shift_date("2026-12-31 23:59:59 +0000", 1), "2027-01-01 00:00:00 +0000");
        assert_eq!(shift_date("2028-02-28 23:59:59 -0500", 86400 + 1), "2028-03-01 00:00:00 -0500");
        assert_eq!(shift_date("乱码", 5), "乱码");
        let t = parse_time("@@EPOCH=1\n@@NTPB=chronyd\n@@NTPSRV=ntp.aliyun.com pool.ntp.org \n@@NTPOTTER=1\n@@SVC=chronyd:active\n@@SVC=chrony:active\n").unwrap();
        assert_eq!((t.ntp_backend.as_str(), t.ntp_servers.len(), t.ntp_by_otter, t.services.len()), ("chronyd", 2, true, 1));
        for b in ["systemd-timesyncd", "chronyd", "chrony"] {
            for srv in [Some("ntp.aliyun.com"), None] {
                let sc = set_ntp_server_script(b, srv).unwrap();
                let o = std::process::Command::new("sh").args(["-n", "-c", &sc]).output().unwrap();
                assert!(o.status.success(), "{b}: {}", String::from_utf8_lossy(&o.stderr));
            }
        }
        assert!(set_ntp_server_script("chronyd", Some("a;b")).is_err() && set_ntp_server_script("", None).is_err());
        assert!(valid_tz("Asia/Shanghai") && valid_tz("Etc/GMT+8") && !valid_tz("../etc/passwd") && !valid_tz("a;b") && !valid_tz("/abs"));
        assert!(set_tz_script("Asia/Shanghai;reboot").is_err());
    }

    const SWAP_OUT: &str = "@@MEM\nMemTotal:        2000000 kB\nMemAvailable:    900000 kB\nSwapTotal:       2097148 kB\nSwapFree:        2000000 kB\n@@SWAPS\nFilename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n/swapfile                               file\t\t1048572\t\t1000\t\t-2\n/dev/sda2                               partition\t1048576\t\t0\t\t-3\n/dev/zram0                              partition\t500000\t\t10\t\t100\n@@FSTAB\n/swapfile none swap sw 0 0 # otter\n# otter-off /dev/sdb1 none swap sw 0 0\nUUID=abcd none swap sw 0 0\n@@SWAPPINESS\n60\n@@ROOT\n/dev/sda1      ext4  30000000  10000000  20000000  34% /\n@@MANAGED\n# otter\nvm.swappiness = 10\n";

    #[test]
    fn swap_parse() {
        let i = parse_swap(SWAP_OUT).unwrap();
        assert_eq!(i.devs.len(), 3);
        assert_eq!((i.devs[0].kind, i.devs[0].in_fstab, i.devs[0].otter), (SwapKind::File, true, true));
        assert_eq!((i.devs[1].kind, i.devs[1].in_fstab), (SwapKind::Partition, false));
        assert_eq!(i.devs[2].kind, SwapKind::Zram);
        assert_eq!(i.off, vec![SwapOff { spec: "/dev/sdb1".into(), by_otter: true }]);
        assert_eq!((i.mem_total_kb, i.swappiness, i.root_fs.as_str(), i.root_avail_kb, i.swappiness_set), (2000000, Some(60), "ext4", 20000000, true));
        assert!(parse_swap("").is_err());
    }

    #[test]
    fn swap_scripts_valid() {
        assert!(swap_create_script("/swapfile", 1024).is_ok());
        for bad in ["swapfile", "/a b", "/tmp/x", "/dev/sda", "/a/../b", "/x;rm", "/dir/"] {
            assert!(swap_create_script(bad, 1024).is_err(), "{bad}");
        }
        assert!(swap_create_script("/swapfile", 1).is_err());
        assert!(swap_delete_script("/dev/sda2").is_err());
        assert!(swap_off_script("/dev/sda2").is_ok() && swap_off_script("/dev/../etc").is_err());
        assert!(set_swappiness_script(101).is_err());
        let all = [swap_create_script("/swapfile", 512).unwrap(), swap_resize_script("/swapfile", 2048).unwrap(), swap_delete_script("/swapfile").unwrap(), swap_off_script("/swapfile").unwrap(), swap_on_script(), swap_on_one_script("/swapfile").unwrap(), set_swappiness_script(10).unwrap(), set_tz_script("UTC").unwrap(), set_ntp_script(true), SWAP_SCRIPT.to_string(), TIME_SCRIPT.to_string(), PORTS_SCRIPT.to_string()];
        for s in all {
            let o = std::process::Command::new("sh").args(["-n", "-c", &s]).output().unwrap();
            assert!(o.status.success(), "{s}\n{}", String::from_utf8_lossy(&o.stderr));
        }
    }
}
