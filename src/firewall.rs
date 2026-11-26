//! 防火墙管理的纯逻辑:读取脚本、输出解析、各种写入脚本。页面在 `pages/firewallpage.rs`;设计见 `docs/modules/firewall.md`。
//!
//! 支持 ufw 和 firewalld(完整读写),nftables/iptables 只读显示。全程 SSH + root 命令,服务器上不装东西;
//! 规则持久化交给 ufw/firewalld 自己。所有写入脚本里的值先过校验(端口、IP/网段、区域名),不拼未校验的文本。
//! 防锁死:启用、改默认策略前先放行 SSH 端口;删除放行 SSH 端口的规则由调用方挡住(`is_ssh_rule`)。

use std::net::IpAddr;

use crate::sysops::quote;

/// 读取脚本(root):装了什么、哪个在生效、SSH 端口、规则
pub const READ_SCRIPT: &str = r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH
has=; for c in ufw firewall-cmd nft iptables; do command -v $c >/dev/null 2>&1 && has="$has $c"; done
echo "@@HAS$has"
on=
command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q '^Status: active' && on="$on ufw"
command -v firewall-cmd >/dev/null 2>&1 && systemctl is-active --quiet firewalld 2>/dev/null && on="$on firewalld"
echo "@@ON$on"
p=$( (sshd -T 2>/dev/null | awk '$1=="port"{print $2}'; ss -tlnp 2>/dev/null | awk '/sshd/{n=split($4,a,":"); print a[n]}') | sort -u | tr '\n' ' ')
echo "@@SSH $p"
if command -v ufw >/dev/null 2>&1; then echo "@@UFW"; ufw status verbose 2>&1; echo "@@UFWN"; ufw status numbered 2>&1; fi
case " $on " in *" firewalld "*)
  echo "@@FWD $(firewall-cmd --get-default-zone 2>/dev/null)"
  for z in $(firewall-cmd --get-active-zones 2>/dev/null | grep -v '^[[:space:]]'); do echo "@@ZONE $z"; firewall-cmd --permanent --zone="$z" --list-all 2>&1; done;;
esac
case "$on" in *ufw*|*firewalld*) ;; *)
  echo "@@RAW"; if command -v nft >/dev/null 2>&1; then nft list ruleset 2>&1 | head -n 300; elif command -v iptables >/dev/null 2>&1; then iptables -S 2>&1 | head -n 300; fi;;
esac
echo "@@IPT"; cat /opt/otter/firewall/iptables.conf 2>/dev/null
echo "@@POL $(iptables -w -S INPUT 2>/dev/null | sed -n 's/^-P INPUT //p')"
iptables -w -C INPUT -j OTTER-INPUT >/dev/null 2>&1 && echo "@@IPTON"
echo "@@DU"; cat /opt/otter/firewall/docker-ports.conf 2>/dev/null
echo "@@DUL"; command -v iptables >/dev/null 2>&1 && iptables -w -S DOCKER-USER 2>/dev/null | grep 'otter:dp'
exit 0
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Front {
    Ufw,
    Firewalld,
    /// 没有 ufw / firewalld 时,Otter 在 iptables 里自己建的 `OTTER-INPUT` 链
    Iptables,
}

impl Front {
    pub fn label(self) -> &'static str {
        match self {
            Front::Ufw => "ufw",
            Front::Firewalld => "firewalld",
            Front::Iptables => "iptables",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Allow,
    Deny,
    Reject,
    Limit,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Allow => "放行",
            Action::Deny => "丢弃",
            Action::Reject => "拒绝",
            Action::Limit => "限速",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleKind {
    /// ufw 的编号
    Ufw(u32),
    Port,
    Service,
    Source,
    Rich,
    /// Otter 的 iptables 规则(key 是 conf 里的整行)
    Ipt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub kind: RuleKind,
    /// firewalld 的区域;ufw 为空
    pub zone: String,
    /// 目标:`22/tcp`、`80`、服务名
    pub to: String,
    pub action: Action,
    /// 来源:`Anywhere` 或地址/网段
    pub from: String,
    pub v6: bool,
    /// 删除用:ufw 是 `to|action|from`(删前核对),firewalld 是端口/服务名/网段/富规则原文
    pub key: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FwInfo {
    pub ufw_installed: bool,
    pub firewalld_installed: bool,
    pub ufw_active: bool,
    pub firewalld_active: bool,
    pub ssh_ports: Vec<u16>,
    /// ufw 的默认入站策略(`deny`/`allow`/`reject`),firewalld 为空
    pub default_in: String,
    pub default_zone: String,
    pub rules: Vec<Rule>,
    /// 没有 ufw/firewalld 在生效时,nftables/iptables 的原始规则(只读)
    pub raw: String,
    /// 有 nftables 或 iptables
    pub low_level: bool,
    /// 有 iptables 命令(Docker 端口控制要用)
    pub iptables: bool,
    /// Otter 写的 Docker 发布端口限制
    pub du: Vec<PortLimit>,
    /// iptables 的 INPUT 链默认策略(`ACCEPT`/`DROP`)
    pub input_policy: String,
    /// `OTTER-INPUT` 链在 iptables 里并被 INPUT 链调用
    pub ipt_live: bool,
}

impl FwInfo {
    /// 正在生效的前端;两个都在生效是冲突,返回 None 并由 `conflict` 提示
    pub fn active(&self) -> Option<Front> {
        match (self.ufw_active, self.firewalld_active) {
            (true, false) => Some(Front::Ufw),
            (false, true) => Some(Front::Firewalld),
            _ => None,
        }
    }

    pub fn conflict(&self) -> bool {
        self.ufw_active && self.firewalld_active
    }

    /// 页面管理哪个:生效的那个;都没生效就取装了的(ufw 优先)
    pub fn manage(&self) -> Option<Front> {
        self.active().or(if self.conflict() {
            None
        } else if self.ufw_installed {
            Some(Front::Ufw)
        } else if self.firewalld_installed {
            Some(Front::Firewalld)
        } else if self.iptables {
            Some(Front::Iptables)
        } else {
            None
        })
    }
}

pub fn parse(out: &str) -> FwInfo {
    let mut info = FwInfo::default();
    let (mut sec, mut zone) = ("", String::new());
    let mut ufw_verbose = String::new();
    let mut ufw_num = String::new();
    let mut zone_text: Vec<(String, String)> = Vec::new();
    let (mut du_conf, mut du_live, mut ipt_conf) = (String::new(), String::new(), String::new());
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            let (tag, arg) = rest.split_once(' ').unwrap_or((rest, ""));
            match tag {
                "HAS" => {
                    let names: Vec<&str> = arg.split_whitespace().collect();
                    info.ufw_installed = names.contains(&"ufw");
                    info.firewalld_installed = names.contains(&"firewall-cmd");
                    info.low_level = names.contains(&"nft") || names.contains(&"iptables");
                    info.iptables = names.contains(&"iptables");
                }
                "ON" => {
                    info.ufw_active = arg.split_whitespace().any(|x| x == "ufw");
                    info.firewalld_active = arg.split_whitespace().any(|x| x == "firewalld");
                }
                "SSH" => {
                    for p in arg.split_whitespace().filter_map(|p| p.parse::<u16>().ok()) {
                        if !info.ssh_ports.contains(&p) {
                            info.ssh_ports.push(p);
                        }
                    }
                }
                "POL" => info.input_policy = arg.trim().to_string(),
                "IPTON" => info.ipt_live = true,
                "FWD" => info.default_zone = arg.trim().to_string(),
                "ZONE" => {
                    zone = arg.trim().to_string();
                    zone_text.push((zone.clone(), String::new()));
                }
                _ => {}
            }
            sec = tag;
            continue;
        }
        match sec {
            "UFW" => {
                ufw_verbose.push_str(line);
                ufw_verbose.push('\n');
            }
            "UFWN" => {
                ufw_num.push_str(line);
                ufw_num.push('\n');
            }
            "ZONE" => {
                if let Some(z) = zone_text.last_mut() {
                    z.1.push_str(line);
                    z.1.push('\n');
                }
            }
            "RAW" => {
                info.raw.push_str(line);
                info.raw.push('\n');
            }
            "IPT" => {
                ipt_conf.push_str(line);
                ipt_conf.push('\n');
            }
            "DU" => {
                du_conf.push_str(line);
                du_conf.push('\n');
            }
            "DUL" => {
                du_live.push_str(line);
                du_live.push('\n');
            }
            _ => {}
        }
    }
    if let Some(l) = ufw_verbose.lines().find_map(|l| l.strip_prefix("Default:")) {
        info.default_in = l.trim().split_whitespace().next().unwrap_or("").to_string();
    }
    info.du = parse_du(&du_conf, &du_live);
    if info.manage() == Some(Front::Iptables) {
        info.rules = parse_ipt(&ipt_conf);
    }
    info.rules.extend(parse_ufw_numbered(&ufw_num));
    for (z, t) in zone_text {
        info.rules.extend(parse_zone(&z, &t));
    }
    info
}

/// `ufw status numbered`:`[ 1] 22/tcp   ALLOW IN   Anywhere   # 备注`
pub fn parse_ufw_numbered(text: &str) -> Vec<Rule> {
    let mut v = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix('[') else { continue };
        let Some((n, body)) = rest.split_once(']') else { continue };
        let Ok(n) = n.trim().parse::<u32>() else { continue };
        let body = body.trim();
        let body = body.split_once(" # ").map(|(a, _)| a).unwrap_or(body).trim_end();
        let parts: Vec<&str> = split_cols(body);
        if parts.len() < 3 {
            continue;
        }
        let (mut to, act, mut from) = (parts[0].to_string(), parts[1], parts[2].to_string());
        let v6 = to.ends_with("(v6)") || from.ends_with("(v6)");
        to = to.trim_end_matches("(v6)").trim().to_string();
        from = from.trim_end_matches("(v6)").trim().to_string();
        let action = match act.split_whitespace().next().unwrap_or("") {
            "ALLOW" => Action::Allow,
            "DENY" => Action::Deny,
            "REJECT" => Action::Reject,
            "LIMIT" => Action::Limit,
            _ => continue,
        };
        let key = format!("{}|{}|{}", parts[0], act, parts[2]);
        v.push(Rule { kind: RuleKind::Ufw(n), zone: String::new(), to, action, from, v6, key });
    }
    v
}

/// 按两个以上的空格切列
fn split_cols(s: &str) -> Vec<&str> {
    let mut v = Vec::new();
    let b = s.as_bytes();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        if b[i] == b' ' && i + 1 < b.len() && b[i + 1] == b' ' {
            if i > start {
                v.push(&s[start..i]);
            }
            while i < b.len() && b[i] == b' ' {
                i += 1;
            }
            start = i;
        } else {
            i += 1;
        }
    }
    if start < b.len() {
        v.push(&s[start..]);
    }
    v
}

/// `firewall-cmd --permanent --zone=X --list-all` 的输出
pub fn parse_zone(zone: &str, text: &str) -> Vec<Rule> {
    let mut v = Vec::new();
    let mut in_rich = false;
    let mk = |kind: RuleKind, to: String, action: Action, from: &str, key: String| Rule { kind, zone: zone.to_string(), to, action, from: from.to_string(), v6: false, key };
    for line in text.lines() {
        let t = line.trim();
        if in_rich {
            if line.starts_with(['\t', ' ']) && t.starts_with("rule ") {
                v.push(parse_rich(zone, t));
                continue;
            }
            in_rich = false;
        }
        let Some((k, val)) = t.split_once(':') else { continue };
        let val = val.trim();
        match k {
            "services" => v.extend(val.split_whitespace().map(|s| mk(RuleKind::Service, s.to_string(), Action::Allow, "Anywhere", s.to_string()))),
            "ports" => v.extend(val.split_whitespace().map(|p| mk(RuleKind::Port, p.to_string(), Action::Allow, "Anywhere", p.to_string()))),
            "sources" => v.extend(val.split_whitespace().map(|s| mk(RuleKind::Source, "全部端口".into(), Action::Allow, s, s.to_string()))),
            "rich rules" => in_rich = true,
            _ => {}
        }
    }
    v
}

/// `rule family="ipv4" source address="1.2.3.4" port port="22" protocol="tcp" accept` 拆出主要字段;`key` 保留原文
fn parse_rich(zone: &str, text: &str) -> Rule {
    let attr = |name: &str| -> Option<String> {
        let pat = format!("{name}=\"");
        let i = text.find(&pat)? + pat.len();
        Some(text[i..].split('"').next()?.to_string())
    };
    let port = attr("port port");
    let proto = attr("protocol");
    let to = match (&port, proto) {
        (Some(p), Some(pr)) => format!("{p}/{pr}"),
        (Some(p), None) => p.clone(),
        _ => attr("service name").unwrap_or_else(|| "(高级规则)".into()),
    };
    let from = attr("source address").unwrap_or_else(|| "Anywhere".into());
    let action = if text.ends_with("accept") {
        Action::Allow
    } else if text.ends_with("reject") {
        Action::Reject
    } else if text.ends_with("drop") {
        Action::Deny
    } else {
        Action::Allow
    };
    Rule { kind: RuleKind::Rich, zone: zone.to_string(), to, action, from, v6: text.contains("family=\"ipv6\""), key: text.to_string() }
}

// ---- 新规则 ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
    Both,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRule {
    pub action: Action,
    /// `80` 或 `8000-8100`
    pub port: String,
    pub proto: Proto,
    /// 空是任意来源
    pub from: String,
}

/// 端口或端口段:`80`、`8000-8100`(也接受 `8000:8100`),返回 (起, 止)
pub fn parse_port(s: &str) -> Option<(u16, u16)> {
    let s = s.trim();
    let p = |x: &str| x.trim().parse::<u16>().ok().filter(|n| *n >= 1);
    match s.split_once(['-', ':']) {
        Some((a, b)) => {
            let (a, b) = (p(a)?, p(b)?);
            (a <= b).then_some((a, b))
        }
        None => p(s).map(|n| (n, n)),
    }
}

/// 来源:IPv4/IPv6 地址,可带 `/掩码`
pub fn valid_source(s: &str) -> bool {
    let (addr, mask) = match s.split_once('/') {
        Some((a, m)) => (a, Some(m)),
        None => (s, None),
    };
    let Ok(ip) = addr.parse::<IpAddr>() else { return false };
    match mask {
        None => true,
        Some(m) => m.parse::<u8>().is_ok_and(|n| n <= if ip.is_ipv4() { 32 } else { 128 }),
    }
}

impl NewRule {
    pub fn validate(&self) -> Result<(), String> {
        parse_port(&self.port).ok_or("端口要写成 80 或 8000-8100(1–65535)")?;
        if !self.from.is_empty() && !valid_source(&self.from) {
            return Err("来源要写成 IP 或网段,如 203.0.113.5、10.0.0.0/8".into());
        }
        if self.action == Action::Limit {
            return Err("暂不支持限速规则".into());
        }
        Ok(())
    }

    fn protos(&self) -> Vec<&'static str> {
        match self.proto {
            Proto::Tcp => vec!["tcp"],
            Proto::Udp => vec!["udp"],
            Proto::Both => vec!["tcp", "udp"],
        }
    }

    /// 一句话:「放行 80/tcp,来自任意地址」
    pub fn describe(&self) -> String {
        let p = match self.proto {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
            Proto::Both => "tcp+udp",
        };
        format!("{} {}/{p},来自{}", self.action.label(), self.port, if self.from.is_empty() { "任意地址" } else { &self.from })
    }
}

// ---- 防锁死 ----

/// 这条规则放行了 SSH 端口吗(删它可能把自己关在外面)。ufw 的 `22/tcp`、`22`、`OpenSSH`,firewalld 的 `ssh` 服务和 `22/tcp`
pub fn is_ssh_rule(r: &Rule, ssh: &[u16]) -> bool {
    if r.action != Action::Allow {
        return false;
    }
    let to = r.to.as_str();
    if matches!(to, "OpenSSH" | "SSH" | "ssh") && ssh.contains(&22) {
        return true;
    }
    let port = to.split('/').next().unwrap_or("");
    match parse_port(port) {
        Some((a, b)) => ssh.iter().any(|p| (a..=b).contains(p)) && !to.ends_with("/udp"),
        None => false,
    }
}

/// 新规则会不会把 SSH 端口挡掉:拒绝/丢弃规则、不限来源、端口覆盖 SSH 端口
pub fn blocks_ssh(n: &NewRule, ssh: &[u16]) -> bool {
    if n.action == Action::Allow || !n.from.is_empty() || n.proto == Proto::Udp {
        return false;
    }
    parse_port(&n.port).is_some_and(|(a, b)| ssh.iter().any(|p| (a..=b).contains(p)))
}

// ---- 防锁死:启用前检查 + 服务器上的定时回滚 ----

/// 脚本里带这个标记的操作,执行器会在成功后检查能不能新建 SSH 连接,连不上就让服务器自己回滚
pub const GUARD_MARK: &str = "# otter-guard";
/// 服务器等多久还没人取消就回滚
pub const GUARD_SECS: u32 = 120;
const GUARD_UNIT: &str = "otter-fw-rollback";

/// 取消定时回滚(能连上之后执行)
pub fn guard_cancel() -> String {
    format!("systemctl stop {GUARD_UNIT}.timer {GUARD_UNIT}.service >/dev/null 2>&1; systemctl reset-failed {GUARD_UNIT}.service >/dev/null 2>&1; true")
}

/// 排一个 `GUARD_SECS` 秒后执行 `rollback` 的临时 systemd 定时器;没有 systemd-run 就不排(操作照常做)
fn guard_arm(rollback: &str) -> String {
    format!(
        "{GUARD_MARK}\nsystemctl stop {GUARD_UNIT}.timer {GUARD_UNIT}.service >/dev/null 2>&1 || true\ncommand -v systemd-run >/dev/null 2>&1 && systemd-run --quiet --unit={GUARD_UNIT} --on-active={GUARD_SECS} /bin/sh -c {} || true\n",
        quote(rollback)
    )
}

/// 已有的拒绝规则会不会挡住这些 SSH 端口:有就报出那一条并退出。`@P@` 换成端口,`@LIST@` 换成列规则的命令
const DENY_AWK: &str = r#"for p in @PORTS@; do
bad=$(@LIST@ | awk -v p="$p" '@AWK@')
if [ -n "$bad" ]; then echo "已有规则会挡住 SSH 端口 ${p}:${bad}。请先删掉这条规则,再启用防火墙,否则会把自己锁在外面" >&2; exit 1; fi
done
"#;

const UFW_AWK: &str = r#"/^ufw (deny|reject) / { if ($0 ~ / from / && $0 !~ / from any( |$)/) next; if ($0 ~ /proto udp|\/udp/) next; n=split($0,w," "); for(i=1;i<=n;i++){ t=w[i]; sub(/\/tcp$/,"",t); if (t ~ /^[0-9]+(:[0-9]+)?$/) { split(t,r,":"); lo=r[1]+0; hi=(r[2]==""?lo:r[2]+0); if (p+0>=lo && p+0<=hi) { print; exit } } } }"#;

const FWD_AWK: &str = r#"/<rule/ && !/<source/ && /<(drop|reject)/ && /protocol="tcp"/ { if (match($0, /port="[0-9]+(-[0-9]+)?"/)) { t=substr($0,RSTART+6,RLENGTH-7); split(t,r,"-"); lo=r[1]+0; hi=(r[2]==""?lo:r[2]+0); if (p+0>=lo && p+0<=hi) { print t "/tcp 拒绝"; exit } } }"#;

fn deny_check(ssh: &[u16], list: &str, awk: &str) -> String {
    let ports = ssh.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(" ");
    DENY_AWK.replace("@PORTS@", &ports).replace("@LIST@", list).replace("@AWK@", awk)
}

/// ufw 里已有的「拒绝」规则(不限来源)是否覆盖 SSH 端口
fn ufw_preflight(ssh: &[u16]) -> String {
    deny_check(ssh, "ufw show added 2>/dev/null", UFW_AWK)
}

/// firewalld 的永久配置里不限来源的拒绝富规则是否覆盖 SSH 端口
fn fwd_preflight(ssh: &[u16]) -> String {
    deny_check(ssh, "{ cat /etc/firewalld/zones/*.xml 2>/dev/null | tr '\\n' ' ' | sed 's#</rule>#&\\n#g'; }", FWD_AWK)
}

fn ssh_ports_ok(ssh: &[u16]) -> Result<(), String> {
    if ssh.is_empty() { Err("读不到 SSH 端口,为避免把自己锁在外面,不能启用防火墙".into()) } else { Ok(()) }
}

// ---- ufw 写入 ----

fn ufw_word(a: Action) -> &'static str {
    match a {
        Action::Allow => "allow",
        Action::Deny => "deny",
        Action::Reject => "reject",
        Action::Limit => "limit",
    }
}

pub fn ufw_add(n: &NewRule) -> Result<String, String> {
    n.validate()?;
    let (a, b) = parse_port(&n.port).ok_or("端口不合法")?;
    let port = if a == b { a.to_string() } else { format!("{a}:{b}") };
    let word = ufw_word(n.action);
    // 端口段必须带协议;单端口选「全部」可以不写协议
    let protos: Vec<Option<&str>> = if n.proto == Proto::Both && a == b { vec![None] } else { n.protos().into_iter().map(Some).collect() };
    let mut s = String::from("PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH\nset -e\n");
    for pr in protos {
        let cmd = if n.from.is_empty() {
            match pr {
                Some(p) => format!("ufw {word} {port}/{p}"),
                None => format!("ufw {word} {port}"),
            }
        } else {
            match pr {
                Some(p) => format!("ufw {word} proto {p} from {} to any port {port}", n.from),
                None => format!("ufw {word} from {} to any port {port}", n.from),
            }
        };
        s.push_str(&cmd);
        s.push('\n');
    }
    Ok(s)
}

/// 删 ufw 规则:删前核对这个编号现在还是显示的那一条(列表可能已经变了),对不上就不删
pub fn ufw_delete(n: u32, key: &str) -> Result<String, String> {
    if key.contains(['\'', '\n', '\\']) {
        return Err("规则里有不支持的字符,请到终端里处理".into());
    }
    Ok(format!(
        "PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH\ncur=$(ufw status numbered | grep -E '^\\[ *{n}\\]' | sed -E 's/^\\[ *[0-9]+\\] *//; s/ +# .*$//; s/ {{2,}}/|/g')\n[ \"$cur\" = {} ] || {{ echo '规则列表已经变了,请刷新后再删' >&2; exit 1; }}\nufw --force delete {n}\n",
        quote(&key.split('|').map(|p| p.trim()).collect::<Vec<_>>().join("|"))
    ))
}

fn ufw_ssh_lines(ssh: &[u16]) -> String {
    ssh.iter().map(|p| format!("ufw allow {p}/tcp >/dev/null\n")).collect()
}

pub fn ufw_enable(ssh: &[u16]) -> Result<String, String> {
    ssh_ports_ok(ssh)?;
    Ok(format!("PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH\nset -e\n{}{}{}ufw --force enable\n", ufw_preflight(ssh), guard_arm("ufw disable"), ufw_ssh_lines(ssh)))
}

pub fn ufw_disable() -> String {
    "PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH\nufw disable\n".into()
}

/// 改默认入站策略(`allow` / `deny`);改成 deny 前先放行 SSH 端口
pub fn ufw_default(allow: bool, ssh: &[u16]) -> Result<String, String> {
    if !allow {
        ssh_ports_ok(ssh)?;
    }
    let pre = if allow { String::new() } else { format!("{}{}{}", ufw_preflight(ssh), guard_arm("ufw default allow incoming"), ufw_ssh_lines(ssh)) };
    Ok(format!("PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH\nset -e\n{pre}ufw default {} incoming\n", if allow { "allow" } else { "deny" }))
}

// ---- firewalld 写入 ----

pub fn valid_zone(z: &str) -> bool {
    !z.is_empty() && z.len() <= 64 && z.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

const FWD_PATH: &str = "PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH\nset -e\n";

pub fn fwd_add(zone: &str, n: &NewRule) -> Result<String, String> {
    n.validate()?;
    if !valid_zone(zone) {
        return Err("区域名不合法".into());
    }
    let (a, b) = parse_port(&n.port).ok_or("端口不合法")?;
    let port = if a == b { a.to_string() } else { format!("{a}-{b}") };
    let mut s = String::from(FWD_PATH);
    for pr in n.protos() {
        // 任意来源的放行用普通端口规则,其余(限来源、拒绝、丢弃)用富规则
        if n.from.is_empty() && n.action == Action::Allow {
            s.push_str(&format!("firewall-cmd --permanent --zone={zone} --add-port={port}/{pr}\n"));
        } else {
            let fam = if n.from.is_empty() { String::new() } else { format!("family=\"{}\" source address=\"{}\" ", if n.from.contains(':') { "ipv6" } else { "ipv4" }, n.from) };
            let verb = match n.action {
                Action::Allow => "accept",
                Action::Deny => "drop",
                Action::Reject => "reject",
                Action::Limit => return Err("暂不支持限速规则".into()),
            };
            let rule = format!("rule {fam}port port=\"{port}\" protocol=\"{pr}\" {verb}");
            s.push_str(&format!("firewall-cmd --permanent --zone={zone} --add-rich-rule={}\n", quote(&rule)));
        }
    }
    s.push_str("firewall-cmd --reload\n");
    Ok(s)
}

pub fn fwd_delete(r: &Rule) -> Result<String, String> {
    if !valid_zone(&r.zone) || r.key.contains(['\n', '\r']) {
        return Err("规则里有不支持的字符,请到终端里处理".into());
    }
    let z = &r.zone;
    let opt = match r.kind {
        RuleKind::Port => format!("--remove-port={}", quote(&r.key)),
        RuleKind::Service => format!("--remove-service={}", quote(&r.key)),
        RuleKind::Source => format!("--remove-source={}", quote(&r.key)),
        RuleKind::Rich => format!("--remove-rich-rule={}", quote(&r.key)),
        RuleKind::Ufw(_) | RuleKind::Ipt => return Err("不是 firewalld 的规则".into()),
    };
    Ok(format!("{FWD_PATH}firewall-cmd --permanent --zone={z} {opt}\nfirewall-cmd --reload\n"))
}

pub fn fwd_enable(ssh: &[u16]) -> Result<String, String> {
    ssh_ports_ok(ssh)?;
    let adds: String = ssh.iter().map(|p| format!("firewall-cmd --permanent --add-port={p}/tcp >/dev/null\n")).collect();
    let offline: String = ssh.iter().map(|p| format!("firewall-offline-cmd --add-port={p}/tcp >/dev/null 2>&1 || true\n")).collect();
    let arm = guard_arm("systemctl disable --now firewalld");
    Ok(format!("{FWD_PATH}{}if systemctl is-active --quiet firewalld; then\n{adds}firewall-cmd --reload\nelse\n{arm}{offline}systemctl enable --now firewalld\nfi\n", fwd_preflight(ssh)))
}

pub fn fwd_disable() -> String {
    format!("{FWD_PATH}systemctl disable --now firewalld\n")
}

// ---- iptables(没有 ufw / firewalld 时)------------------------------------------------
//
// 不碰已有的规则:Otter 只管自己的 `OTTER-INPUT` 链(INPUT 链第一条跳进去),规则存在
// `/opt/otter/firewall/iptables.conf`,由 `otter-iptables.service` 在开机时套用(先建新链再换名,没有空窗)。
// 每条规则都带 `! -i lo`,不影响本机访问本机。只支持 IPv4。

const IPT_CONF: &str = "/opt/otter/firewall/iptables.conf";
const IPT_SH: &str = "/opt/otter/firewall/iptables.sh";
const IPT_UNIT: &str = "/etc/systemd/system/otter-iptables.service";

fn ipt_word(a: Action) -> Option<&'static str> {
    match a {
        Action::Allow => Some("allow"),
        Action::Deny => Some("drop"),
        Action::Reject => Some("reject"),
        Action::Limit => None,
    }
}

/// conf 的一行:`allow tcp 80 -` / `drop tcp 8000:8100 203.0.113.0/24`
pub fn parse_ipt(conf: &str) -> Vec<Rule> {
    let mut v = Vec::new();
    for line in conf.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 4 {
            continue;
        }
        let action = match f[0] {
            "allow" => Action::Allow,
            "drop" => Action::Deny,
            "reject" => Action::Reject,
            _ => continue,
        };
        if !matches!(f[1], "tcp" | "udp") || parse_port(f[2]).is_none() {
            continue;
        }
        let from = if f[3] == "-" { "Anywhere".to_string() } else { f[3].to_string() };
        v.push(Rule { kind: RuleKind::Ipt, zone: String::new(), to: format!("{}/{}", f[2], f[1]), action, from, v6: false, key: f[..4].join(" ") });
    }
    v
}

fn ipt_apply_script() -> String {
    format!(
        r#"#!/bin/sh
# Otter 生成:把 {IPT_CONF} 里的规则写进 OTTER-INPUT 链。开机时由 otter-iptables.service 套用;请在 Otter 里修改,手改会被覆盖
PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin; export PATH
CONF={IPT_CONF}
iptables -w -F OTTER-NEW >/dev/null 2>&1; iptables -w -X OTTER-NEW >/dev/null 2>&1
iptables -w -N OTTER-NEW || exit 1
if [ -f "$CONF" ]; then
  while read -r act proto port src; do
    case "$act" in allow) j=ACCEPT;; drop) j=DROP;; reject) j=REJECT;; *) continue;; esac
    case "$proto" in tcp|udp) ;; *) continue;; esac
    case "$port" in ''|*[!0-9:]*) continue;; esac
    s=; [ -n "$src" ] && [ "$src" != "-" ] && s="-s $src"
    iptables -w -A OTTER-NEW ! -i lo -p "$proto" --dport "$port" $s -j "$j" || {{ iptables -w -F OTTER-NEW; iptables -w -X OTTER-NEW; exit 1; }}
  done <"$CONF"
fi
iptables -w -I INPUT 1 -j OTTER-NEW
while iptables -w -D INPUT -j OTTER-INPUT 2>/dev/null; do :; done
iptables -w -F OTTER-INPUT 2>/dev/null; iptables -w -X OTTER-INPUT 2>/dev/null
iptables -w -E OTTER-NEW OTTER-INPUT
exit 0
"#
    )
}

/// 把 `rules`(conf 行)写成 conf、装脚本和单元、立即套用
fn ipt_install(lines: &[String]) -> String {
    let mut conf = String::from("# Otter 管理的 iptables 入站规则:动作 协议 端口 来源(- 表示任意)\n");
    for l in lines {
        conf.push_str(l);
        conf.push('\n');
    }
    let apply = ipt_apply_script();
    format!(
        r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH
set -e
command -v iptables >/dev/null 2>&1 || {{ echo '服务器上没有 iptables 命令' >&2; exit 1; }}
command -v systemctl >/dev/null 2>&1 || {{ echo '服务器没有 systemd,没法让规则在重启后保留,所以没有改动' >&2; exit 1; }}
mkdir -p {DU_DIR}
cat >{IPT_CONF} <<'OTTER_EOF'
{conf}OTTER_EOF
cat >{IPT_SH} <<'OTTER_EOF'
{apply}OTTER_EOF
chmod 700 {IPT_SH}
cat >{IPT_UNIT} <<'OTTER_EOF'
[Unit]
Description=Otter: iptables inbound rules
Wants=network-pre.target
Before=network-pre.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart={IPT_SH}

[Install]
WantedBy=multi-user.target
OTTER_EOF
systemctl daemon-reload
systemctl enable otter-iptables.service >/dev/null 2>&1
{IPT_SH}
"#
    )
}

/// 打开开关:按现有规则重新写 conf、装单元并套用
pub fn ipt_enable(current: &[Rule]) -> String {
    let lines: Vec<String> = current.iter().filter(|r| r.kind == RuleKind::Ipt).map(|r| r.key.clone()).collect();
    ipt_install(&lines)
}

/// 关掉开关:停开机套用,把 OTTER-INPUT 从 INPUT 链摘掉(规则和 conf 保留)
pub fn ipt_disable() -> String {
    "PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH\ncommand -v systemctl >/dev/null 2>&1 && systemctl disable otter-iptables.service >/dev/null 2>&1\nwhile iptables -w -D INPUT -j OTTER-INPUT 2>/dev/null; do :; done\nexit 0\n".to_string()
}

pub fn ipt_add(n: &NewRule, current: &[Rule]) -> Result<String, String> {
    ipt_add_many(std::slice::from_ref(n), current)
}

/// 一次加多条(端口列表拆出来的),合在一起写一次
pub fn ipt_add_many(ns: &[NewRule], current: &[Rule]) -> Result<String, String> {
    let mut lines: Vec<String> = current.iter().filter(|r| r.kind == RuleKind::Ipt).map(|r| r.key.clone()).collect();
    for n in ns {
        n.validate()?;
        if n.action == Action::Limit {
            return Err("暂不支持限速规则".into());
        }
        if !n.from.is_empty() && !valid_du_source(&n.from) {
            return Err("来源要写成 IPv4 地址或网段,如 203.0.113.5、10.0.0.0/8".into());
        }
        let (a, b) = parse_port(&n.port).ok_or("端口不合法")?;
        let port = if a == b { a.to_string() } else { format!("{a}:{b}") };
        let word = ipt_word(n.action).ok_or("动作不支持")?;
        for pr in n.protos() {
            let l = format!("{word} {pr} {port} {}", if n.from.is_empty() { "-" } else { &n.from });
            if !lines.contains(&l) {
                lines.push(l);
            }
        }
    }
    Ok(ipt_install(&lines))
}

pub fn ipt_delete(r: &Rule, current: &[Rule]) -> Result<String, String> {
    if r.kind != RuleKind::Ipt {
        return Err("不是 Otter 的 iptables 规则".into());
    }
    let lines: Vec<String> = current.iter().filter(|x| x.kind == RuleKind::Ipt && x.key != r.key).map(|x| x.key.clone()).collect();
    Ok(ipt_install(&lines))
}

// ---- Docker 发布端口(DOCKER-USER 链)------------------------------------------------
//
// Docker 发布的端口先 DNAT 再走 FORWARD 链,ufw / firewalld 管 INPUT 链的规则对它们不起作用。
// 这里往 iptables 的 `DOCKER-USER` 链写规则(它在 Docker 自己的规则之前,Docker 重启不会清空),用
// `--ctorigdstport` 按「宿主机端口」匹配。规则的真相源是服务器上的 `/opt/otter/firewall/docker-ports.conf`,
// 由 `otter-docker-user.service`(开机、Docker 重启后)调 `docker-user.sh` 重新套用,与 ufw / firewalld 无关。

pub const DU_DIR: &str = "/opt/otter/firewall";
pub const DU_CONF: &str = "/opt/otter/firewall/docker-ports.conf";
const DU_SH: &str = "/opt/otter/firewall/docker-user.sh";
const DU_UNIT: &str = "/etc/systemd/system/otter-docker-user.service";
const DU_TAG: &str = "otter:dp";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortMode {
    /// 禁止外部访问(本机和 Docker 内部网络不受影响)
    Deny,
    /// 只允许这些来源(IPv4 地址或网段)
    Only(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortLimit {
    pub proto: String,
    pub port: u16,
    pub mode: PortMode,
    /// 这条规则现在确实在 iptables 里(没有就是还没套用,比如 Docker 没启动)
    pub live: bool,
}

/// 读 conf 和 `iptables -S DOCKER-USER` 里带 Otter 标记的行
pub fn parse_du(conf: &str, live: &str) -> Vec<PortLimit> {
    let mut v = Vec::new();
    for line in conf.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 || !matches!(f[0], "tcp" | "udp") {
            continue;
        }
        let Ok(port) = f[1].parse::<u16>() else { continue };
        let mode = match f[2] {
            "deny" => PortMode::Deny,
            "only" => PortMode::Only(f.get(3).map(|x| x.split(',').filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default()),
            _ => continue,
        };
        let proto = f[0].to_string();
        let has = live.lines().any(|l| l.contains(&format!("-p {proto} ")) && l.contains(&format!("--ctorigdstport {port} ")) && l.contains("-j DROP"));
        v.push(PortLimit { proto, port, mode, live: has });
    }
    v
}

/// 限制来源只支持 IPv4(Docker 的 IPv6 端口发布不走这条链)
pub fn valid_du_source(s: &str) -> bool {
    valid_source(s) && !s.contains(':')
}

fn du_match(proto: &str, port: u16) -> String {
    format!("-p {proto} -m conntrack --ctorigdstport {port} --ctdir ORIGINAL ! -i docker0 ! -i br-+ -m comment --comment {DU_TAG}")
}

/// 给用户看的预览:最终会写进 DOCKER-USER 的 iptables 规则
pub fn du_preview(proto: &str, port: u16, mode: &PortMode) -> Vec<String> {
    let m = du_match(proto, port);
    let mut v = Vec::new();
    if let PortMode::Only(src) = mode {
        for s in src {
            v.push(format!("iptables -I DOCKER-USER {m} -s {s} -j RETURN"));
        }
    }
    v.push(format!("iptables -I DOCKER-USER {m} -j DROP"));
    v
}

fn du_apply_script(conf: &str) -> String {
    format!(
        r#"#!/bin/sh
# Otter 生成:把 {conf} 里的规则写进 DOCKER-USER 链。开机和 Docker 重启后由 otter-docker-user.service 重新套用;请在 Otter 里修改,手改会被覆盖
PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin; export PATH
CONF={conf}
iptables -w -n -L DOCKER-USER >/dev/null 2>&1 || iptables -w -N DOCKER-USER || exit 1
iptables -w -S DOCKER-USER | grep -- 'comment {DU_TAG}' | sed 's/^-A /-D /' | while read -r l; do iptables -w $l; done
[ -f "$CONF" ] || exit 0
while read -r proto port mode srcs; do
  case "$proto" in tcp|udp) ;; *) continue;; esac
  case "$port" in ''|*[!0-9]*) continue;; esac
  m="-p $proto -m conntrack --ctorigdstport $port --ctdir ORIGINAL ! -i docker0 ! -i br-+ -m comment --comment {DU_TAG}"
  case "$mode" in
    deny) iptables -w -I DOCKER-USER $m -j DROP;;
    only) iptables -w -I DOCKER-USER $m -j DROP
          for s in $(echo "$srcs" | tr ',' ' '); do iptables -w -I DOCKER-USER $m -s "$s" -j RETURN; done;;
  esac
done <"$CONF"
exit 0
"#
    )
}

fn du_conf_text(list: &[PortLimit]) -> String {
    let mut s = String::from("# Otter 管理的 Docker 发布端口限制:协议 端口 deny|only 来源列表\n");
    for l in list {
        match &l.mode {
            PortMode::Deny => s.push_str(&format!("{} {} deny\n", l.proto, l.port)),
            PortMode::Only(src) => s.push_str(&format!("{} {} only {}\n", l.proto, l.port, src.join(","))),
        }
    }
    s
}

/// 设置(`Some`)或取消(`None`)一个端口的限制,返回 root 脚本:写 conf、装脚本和 systemd 单元、立即套用。
/// `current` 是现有的限制;`ssh` 里的端口不允许限制
pub fn du_set(proto: &str, port: u16, mode: Option<&PortMode>, current: &[PortLimit], ssh: &[u16]) -> Result<String, String> {
    if !matches!(proto, "tcp" | "udp") || port == 0 {
        return Err("端口或协议不合法".into());
    }
    if ssh.contains(&port) && proto == "tcp" {
        return Err("这是 SSH 端口,限制它可能把自己锁在服务器外面".into());
    }
    if let Some(PortMode::Only(src)) = mode {
        if src.is_empty() {
            return Err("至少填一个允许访问的来源".into());
        }
        if let Some(bad) = src.iter().find(|s| !valid_du_source(s)) {
            return Err(format!("来源「{bad}」不对:要写成 IPv4 地址或网段,如 203.0.113.5、10.0.0.0/8"));
        }
    }
    let mut list: Vec<PortLimit> = current.iter().filter(|l| !(l.proto == proto && l.port == port)).cloned().collect();
    if let Some(m) = mode {
        list.push(PortLimit { proto: proto.into(), port, mode: m.clone(), live: false });
    }
    list.sort_by(|a, b| (a.port, &a.proto).cmp(&(b.port, &b.proto)));
    let conf = du_conf_text(&list);
    let apply = du_apply_script(DU_CONF);
    Ok(format!(
        r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin:$PATH; export PATH
set -e
command -v iptables >/dev/null 2>&1 || {{ echo '服务器上没有 iptables 命令' >&2; exit 1; }}
command -v systemctl >/dev/null 2>&1 || {{ echo '服务器没有 systemd,没法让规则在重启后保留,所以没有改动' >&2; exit 1; }}
mkdir -p {DU_DIR}
cat >{DU_CONF} <<'OTTER_EOF'
{conf}OTTER_EOF
cat >{DU_SH} <<'OTTER_EOF'
{apply}OTTER_EOF
chmod 700 {DU_SH}
cat >{DU_UNIT} <<'OTTER_EOF'
[Unit]
Description=Otter: Docker published port rules
After=docker.service network.target
PartOf=docker.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart={DU_SH}

[Install]
WantedBy=multi-user.target
OTTER_EOF
systemctl daemon-reload
systemctl enable otter-docker-user.service >/dev/null 2>&1
{DU_SH}
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UFW_NUM: &str = "Status: active\n\n     To                         Action      From\n     --                         ------      ----\n[ 1] 22/tcp                     ALLOW IN    Anywhere\n[ 2] 80/tcp                     ALLOW IN    203.0.113.0/24             # web\n[ 3] 22/tcp (v6)                ALLOW IN    Anywhere (v6)\n[ 4] 3306                       DENY IN     Anywhere\n";

    #[test]
    fn ufw_rules() {
        let v = parse_ufw_numbered(UFW_NUM);
        assert_eq!(v.len(), 4);
        assert_eq!((v[0].to.as_str(), v[0].action, v[0].from.as_str(), v[0].v6), ("22/tcp", Action::Allow, "Anywhere", false));
        assert_eq!(v[1].from, "203.0.113.0/24");
        assert_eq!(v[1].key, "80/tcp|ALLOW IN|203.0.113.0/24");
        assert!(v[2].v6);
        assert_eq!(v[3].action, Action::Deny);
    }

    #[test]
    fn full_parse() {
        let out = format!("@@HAS ufw nft\n@@ON ufw\n@@SSH 22 2222 \n@@UFW\nStatus: active\nLogging: on (low)\nDefault: deny (incoming), allow (outgoing), disabled (routed)\n@@UFWN\n{UFW_NUM}");
        let i = parse(&out);
        assert!(i.ufw_installed && i.ufw_active && !i.firewalld_active && i.low_level);
        assert_eq!(i.ssh_ports, [22, 2222]);
        assert_eq!(i.default_in, "deny");
        assert_eq!(i.rules.len(), 4);
        assert_eq!(i.manage(), Some(Front::Ufw));
    }

    #[test]
    fn firewalld_zone() {
        let t = "public (active)\n  target: default\n  interfaces: eth0\n  sources: 10.0.0.0/8\n  services: cockpit ssh\n  ports: 80/tcp 8000-8100/udp\n  rich rules: \n\trule family=\"ipv4\" source address=\"1.2.3.4\" port port=\"22\" protocol=\"tcp\" accept\n\trule port port=\"3306\" protocol=\"tcp\" drop\n";
        let v = parse_zone("public", t);
        assert_eq!(v.len(), 7);
        assert_eq!(v[2].kind, RuleKind::Service);
        assert_eq!(v[5].to, "22/tcp");
        assert_eq!((v[5].from.as_str(), v[5].action), ("1.2.3.4", Action::Allow));
        assert_eq!((v[6].to.as_str(), v[6].action), ("3306/tcp", Action::Deny));
        let out = format!("@@HAS firewall-cmd\n@@ON firewalld\n@@SSH 22\n@@FWD public\n@@ZONE public\n{t}");
        let i = parse(&out);
        assert_eq!((i.default_zone.as_str(), i.rules.len(), i.manage()), ("public", 7, Some(Front::Firewalld)));
    }

    #[test]
    fn nothing_active() {
        let i = parse("@@HAS ufw firewall-cmd\n@@ON\n@@SSH 22\n@@UFW\nStatus: inactive\n@@UFWN\nStatus: inactive\n@@RAW\ntable inet filter {}\n");
        assert_eq!(i.active(), None);
        assert_eq!(i.manage(), Some(Front::Ufw));
        assert!(i.raw.contains("table inet"));
        let both = FwInfo { ufw_active: true, firewalld_active: true, ..Default::default() };
        assert!(both.conflict() && both.manage().is_none());
    }

    #[test]
    fn validation() {
        assert_eq!(parse_port("80"), Some((80, 80)));
        assert_eq!(parse_port("8000-8100"), Some((8000, 8100)));
        assert_eq!(parse_port("8000:8100"), Some((8000, 8100)));
        assert_eq!(parse_port("0"), None);
        assert_eq!(parse_port("9000-8000"), None);
        assert_eq!(parse_port("80; rm"), None);
        assert!(valid_source("203.0.113.5") && valid_source("10.0.0.0/8") && valid_source("2001:db8::/32"));
        assert!(!valid_source("10.0.0.0/33") && !valid_source("a; b") && !valid_source("1.2.3"));
    }

    fn nr(a: Action, port: &str, proto: Proto, from: &str) -> NewRule {
        NewRule { action: a, port: port.into(), proto, from: from.into() }
    }

    #[test]
    fn ssh_guard() {
        let v = parse_ufw_numbered(UFW_NUM);
        assert!(is_ssh_rule(&v[0], &[22]) && is_ssh_rule(&v[2], &[22]));
        assert!(!is_ssh_rule(&v[1], &[22]) && !is_ssh_rule(&v[3], &[22]));
        assert!(blocks_ssh(&nr(Action::Deny, "20-30", Proto::Tcp, ""), &[22]));
        assert!(!blocks_ssh(&nr(Action::Deny, "22", Proto::Tcp, "1.2.3.4"), &[22]));
        assert!(!blocks_ssh(&nr(Action::Allow, "22", Proto::Tcp, ""), &[22]));
        assert!(ufw_enable(&[]).is_err() && fwd_enable(&[]).is_err() && ufw_default(false, &[]).is_err());
        assert!(ufw_default(true, &[]).is_ok());
    }

    fn sh_ok(script: &str) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("otter-fw-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.sh");
        std::fs::write(&f, script).unwrap();
        let out = std::process::Command::new("sh").arg("-n").arg(&f).output().unwrap();
        assert!(out.status.success(), "{script}\n{}", String::from_utf8_lossy(&out.stderr));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 在临时目录里放假的 ufw,跑预检脚本,返回 (退出码, stderr)
    fn run_pre(script: &str, fake: &str) -> (i32, String) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("otter-fwp-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ufw"), fake).unwrap();
        std::process::Command::new("chmod").arg("+x").arg(dir.join("ufw")).status().unwrap();
        std::fs::write(dir.join("t.sh"), script).unwrap();
        let out = std::process::Command::new("sh").arg(dir.join("t.sh")).env("PATH", format!("{}:/usr/bin:/bin", dir.display())).output().unwrap();
        std::fs::remove_dir_all(&dir).ok();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).to_string())
    }

    #[test]
    fn preflight_refuses_deny_on_ssh() {
        let pre = ufw_preflight(&[22, 2222]);
        let fake = |rules: &str| format!("#!/bin/sh\nif [ \"$1 $2\" = \"show added\" ]; then printf 'Added user rules (see runtime for all):\\n{rules}'; fi\n");
        // 不限来源的拒绝 22:拒绝
        let (c, e) = run_pre(&pre, &fake("ufw deny 22/tcp\\n"));
        assert_eq!(c, 1, "{e}");
        assert!(e.contains("22") && e.contains("ufw deny 22/tcp"), "{e}");
        // 端口段覆盖
        assert_eq!(run_pre(&pre, &fake("ufw deny 2000:3000/tcp\\n")).0, 1);
        // 只挡某个来源、或者 udp、或者别的端口、或者放行:通过
        assert_eq!(run_pre(&pre, &fake("ufw deny from 1.2.3.4 to any port 22\\n")).0, 0);
        assert_eq!(run_pre(&pre, &fake("ufw deny 22/udp\\nufw deny 80/tcp\\nufw allow 22/tcp\\n")).0, 0);
        // from any 等同不限来源
        assert_eq!(run_pre(&pre, &fake("ufw deny from any to any port 2222\\n")).0, 1);
    }

    #[test]
    fn guard_scripts() {
        let u = ufw_enable(&[22]).unwrap();
        assert!(u.contains(GUARD_MARK) && u.contains("systemd-run") && u.contains("ufw disable"));
        sh_ok(&u);
        let d = ufw_default(false, &[22]).unwrap();
        assert!(d.contains(GUARD_MARK) && d.contains("ufw default allow incoming"));
        sh_ok(&d);
        assert!(!ufw_default(true, &[22]).unwrap().contains(GUARD_MARK));
        let f = fwd_enable(&[22]).unwrap();
        assert!(f.contains(GUARD_MARK) && f.contains("disable --now firewalld"));
        sh_ok(&f);
        sh_ok(&guard_cancel());
        assert!(!ufw_disable().contains(GUARD_MARK));
    }

    #[test]
    fn scripts() {
        sh_ok(READ_SCRIPT);
        let u = ufw_add(&nr(Action::Allow, "80", Proto::Both, "")).unwrap();
        assert!(u.contains("ufw allow 80\n"));
        let u = ufw_add(&nr(Action::Deny, "8000-8100", Proto::Both, "10.0.0.0/8")).unwrap();
        assert!(u.contains("ufw deny proto tcp from 10.0.0.0/8 to any port 8000:8100") && u.contains("proto udp"));
        sh_ok(&u);
        sh_ok(&ufw_delete(3, "22/tcp|ALLOW IN|Anywhere").unwrap());
        assert!(ufw_delete(3, "x'y").is_err());
        sh_ok(&ufw_enable(&[22, 2222]).unwrap());
        sh_ok(&ufw_disable());
        sh_ok(&ufw_default(false, &[22]).unwrap());
        let f = fwd_add("public", &nr(Action::Allow, "80", Proto::Tcp, "")).unwrap();
        assert!(f.contains("--add-port=80/tcp"));
        let f = fwd_add("public", &nr(Action::Deny, "3306", Proto::Tcp, "")).unwrap();
        assert!(f.contains("rule port port=\"3306\" protocol=\"tcp\" drop"));
        sh_ok(&f);
        let f = fwd_add("public", &nr(Action::Allow, "22", Proto::Tcp, "2001:db8::/32")).unwrap();
        assert!(f.contains("family=\"ipv6\""));
        sh_ok(&f);
        assert!(fwd_add("pub lic", &nr(Action::Allow, "80", Proto::Tcp, "")).is_err());
        assert!(fwd_add("public", &nr(Action::Allow, "80; id", Proto::Tcp, "")).is_err());
        for r in parse_zone("public", "  ports: 80/tcp\n  services: ssh\n  sources: 10.0.0.0/8\n  rich rules: \n\trule port port=\"1\" protocol=\"tcp\" drop\n") {
            sh_ok(&fwd_delete(&r).unwrap());
        }
        sh_ok(&fwd_enable(&[22]).unwrap());
        sh_ok(&fwd_disable());
    }

    #[test]
    fn docker_port_limits() {
        let conf = "# x\ntcp 8080 deny\ntcp 3306 only 203.0.113.5,10.0.0.0/8\nbad line\n";
        let live = "-A DOCKER-USER -p tcp -m conntrack --ctorigdstport 8080 --ctdir ORIGINAL ! -i docker0 ! -i br-+ -m comment --comment otter:dp -j DROP\n";
        let v = parse_du(conf, live);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].port, &v[0].mode, v[0].live), (8080, &PortMode::Deny, true));
        assert_eq!(v[1].mode, PortMode::Only(vec!["203.0.113.5".into(), "10.0.0.0/8".into()]));
        assert!(!v[1].live);
        let i = parse(&format!("@@HAS iptables\n@@ON\n@@SSH 22\n@@DU\n{conf}@@DUL\n{live}"));
        assert!(i.iptables && i.du.len() == 2);

        let only = PortMode::Only(vec!["203.0.113.5".into()]);
        let s = du_set("tcp", 5432, Some(&only), &v, &[22]).unwrap();
        sh_ok(&s);
        assert!(s.contains("tcp 5432 only 203.0.113.5") && s.contains("tcp 8080 deny"));
        // 取消限制:conf 里不再有这个端口
        let s = du_set("tcp", 8080, None, &v, &[22]).unwrap();
        assert!(!s.contains("tcp 8080 deny") && s.contains("tcp 3306 only"));
        // 拒绝:SSH 端口、IPv6 来源、空来源、坏来源
        assert!(du_set("tcp", 22, Some(&PortMode::Deny), &[], &[22]).is_err());
        assert!(du_set("tcp", 80, Some(&PortMode::Only(vec!["2001:db8::1".into()])), &[], &[22]).is_err());
        assert!(du_set("tcp", 80, Some(&PortMode::Only(vec![])), &[], &[22]).is_err());
        assert!(du_set("tcp", 80, Some(&PortMode::Only(vec!["1.2.3.4; id".into()])), &[], &[22]).is_err());
        assert_eq!(du_preview("tcp", 80, &only).len(), 2);
    }

    /// 用假的 iptables 跑一遍套用脚本:先清掉旧的 Otter 规则,再按 conf 写,允许来源在 DROP 之前
    #[test]
    fn docker_apply_script_with_fake_iptables() {
        let dir = std::env::temp_dir().join(format!("otter-fwdu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (log, conf) = (dir.join("log"), dir.join("ports.conf"));
        std::fs::write(&conf, "tcp 8080 deny\ntcp 3306 only 1.2.3.4,10.0.0.0/8\nxx 1 deny\n").unwrap();
        let fake = dir.join("iptables");
        std::fs::write(&fake, format!("#!/bin/sh\necho \"$@\" >>{}\ncase \"$*\" in *-S*) echo '-A DOCKER-USER -p tcp -m conntrack --ctorigdstport 9 --ctdir ORIGINAL ! -i docker0 ! -i br-+ -m comment --comment otter:dp -j DROP';; esac\nexit 0\n", log.display())).unwrap();
        std::process::Command::new("chmod").arg("755").arg(&fake).status().unwrap();
        let sh = dir.join("apply.sh");
        std::fs::write(&sh, du_apply_script(conf.to_str().unwrap()).replace("PATH=/usr/local/sbin", &format!("PATH={}:/usr/local/sbin", dir.display()))).unwrap();
        let out = std::process::Command::new("sh").arg(&sh).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let log = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert!(lines.iter().any(|l| l.starts_with("-w -D DOCKER-USER") && l.contains("--ctorigdstport 9 ")), "{log}");
        let at = |needle: &str| lines.iter().position(|l| l.starts_with("-w -I DOCKER-USER") && l.contains(needle)).unwrap_or_else(|| panic!("{needle}\n{log}"));
        let (drop, ret1, ret2) = (at("--ctorigdstport 3306 --ctdir ORIGINAL ! -i docker0 ! -i br-+ -m comment --comment otter:dp -j DROP"), at("-s 1.2.3.4 -j RETURN"), at("-s 10.0.0.0/8 -j RETURN"));
        assert!(drop < ret1 && drop < ret2);
        assert!(lines.iter().any(|l| l.contains("--ctorigdstport 8080") && l.ends_with("-j DROP")));
        assert!(!log.contains("xx"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn iptables_front() {
        let i = parse("@@HAS iptables nft\n@@ON\n@@SSH 22\n@@RAW\n-P INPUT ACCEPT\n@@IPT\n# c\nallow tcp 80 -\ndrop tcp 8000:8100 203.0.113.0/24\nbad\n@@POL ACCEPT\n@@IPTON\n@@DU\n@@DUL\n");
        assert_eq!(i.manage(), Some(Front::Iptables));
        assert!(i.ipt_live && i.input_policy == "ACCEPT");
        assert_eq!(i.rules.len(), 2);
        assert_eq!((i.rules[1].to.as_str(), i.rules[1].action, i.rules[1].from.as_str()), ("8000:8100/tcp", Action::Deny, "203.0.113.0/24"));
        let s = ipt_add(&nr(Action::Allow, "443", Proto::Both, ""), &i.rules).unwrap();
        sh_ok(&s);
        assert!(s.contains("allow tcp 443 -\nallow udp 443 -") && s.contains("allow tcp 80 -"));
        sh_ok(&ipt_disable());
        sh_ok(&ipt_enable(&i.rules));
        let s = ipt_delete(&i.rules[0], &i.rules).unwrap();
        assert!(!s.contains("allow tcp 80 -") && s.contains("drop tcp 8000:8100 203.0.113.0/24"));
        assert!(ipt_add(&nr(Action::Allow, "80", Proto::Tcp, "2001:db8::1"), &[]).is_err());
        assert!(ipt_add(&nr(Action::Allow, "80; id", Proto::Tcp, ""), &[]).is_err());
        // ufw 装了就不用 iptables 前端
        let u = parse("@@HAS ufw iptables\n@@ON\n@@SSH 22\n");
        assert_eq!(u.manage(), Some(Front::Ufw));
    }

    /// 套用脚本先建新链、填规则、换名,每条规则都排除 lo
    #[test]
    fn iptables_apply_with_fake() {
        let dir = std::env::temp_dir().join(format!("otter-fwipt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (log, conf) = (dir.join("log"), dir.join("c.conf"));
        std::fs::write(&conf, "allow tcp 80 -\ndrop tcp 8000:8100 1.2.3.4\nreject udp 53 -\n").unwrap();
        let fake = dir.join("iptables");
        std::fs::write(&fake, format!("#!/bin/sh\necho \"$@\" >>{}\ncase \"$*\" in *-D*) exit 1;; esac\nexit 0\n", log.display())).unwrap();
        std::process::Command::new("chmod").arg("755").arg(&fake).status().unwrap();
        let sh = dir.join("a.sh");
        std::fs::write(&sh, ipt_apply_script().replace(IPT_CONF, conf.to_str().unwrap()).replace("PATH=/usr/local/sbin", &format!("PATH={}:/usr/local/sbin", dir.display()))).unwrap();
        let out = std::process::Command::new("sh").arg(&sh).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let log = std::fs::read_to_string(&log).unwrap();
        assert!(log.contains("-w -A OTTER-NEW ! -i lo -p tcp --dport 80 -j ACCEPT"), "{log}");
        assert!(log.contains("--dport 8000:8100 -s 1.2.3.4 -j DROP") && log.contains("-p udp --dport 53 -j REJECT"), "{log}");
        assert!(log.trim_end().ends_with("-w -E OTTER-NEW OTTER-INPUT"), "{log}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
