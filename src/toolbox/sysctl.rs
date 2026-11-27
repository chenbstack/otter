//! 内核参数(sysctl)的几个常用开关。
//!
//! 只管一份白名单;写进 `/etc/sysctl.d/99-otter.conf`(每个键一行,先 `sysctl -w` 成功才写文件),
//! 「恢复默认」是删掉那一行(当前运行值不变,重启后恢复系统默认)。BBR 要加载 tcp_bbr 模块,
//! 为了重启后还在,另写 `/etc/modules-load.d/otter-bbr.conf`。

use super::common::sections;
use super::PATH_FIX;

pub const CONF: &str = "/etc/sysctl.d/99-otter.conf";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// 开/关两个值
    Switch { on: &'static str, off: &'static str },
    /// 几个常用值
    Presets(&'static [&'static str]),
    /// BBR 拥塞控制(同时设 fq 队列)
    Bbr,
}

#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub key: &'static str,
    pub title: &'static str,
    pub desc: &'static str,
    pub control: Control,
    /// 改之前要提醒的事(关闭 IP 转发会影响 Docker 等)
    pub warn: &'static str,
}

pub const ITEMS: [Item; 8] = [
    Item { key: "net.ipv4.ip_forward", title: "IP 转发", desc: "把收到的包转给别的网络。Docker、VPN、做网关的机器需要打开", control: Control::Switch { on: "1", off: "0" }, warn: "关闭后 Docker 容器的网络、VPN、NAT 转发会不通" },
    Item { key: "net.ipv4.tcp_congestion_control", title: "BBR 拥塞控制", desc: "Google 的 TCP 拥塞算法,高延迟、有丢包的线路上通常更快", control: Control::Bbr, warn: "" },
    Item { key: "net.ipv4.tcp_syncookies", title: "SYN Cookies", desc: "半连接队列被打满时的防护,抵御 SYN Flood,建议开着", control: Control::Switch { on: "1", off: "0" }, warn: "关闭后遇到 SYN Flood 攻击服务会更容易被打垮" },
    Item { key: "net.ipv4.icmp_echo_ignore_all", title: "禁 ping", desc: "开启后服务器不再回应 ping(其它端口不受影响)", control: Control::Switch { on: "1", off: "0" }, warn: "" },
    Item { key: "net.core.somaxconn", title: "监听队列长度", desc: "单个端口等待被接受的连接数上限,高并发的 Web、Redis 常调大", control: Control::Presets(&["128", "1024", "4096", "65535"]), warn: "" },
    Item { key: "fs.file-max", title: "系统最多打开的文件数", desc: "整个系统的文件句柄上限;单个进程的上限另由 ulimit 或服务的 LimitNOFILE 决定", control: Control::Presets(&["65536", "524288", "1048576"]), warn: "" },
    Item { key: "vm.overcommit_memory", title: "内存过量分配", desc: "0 启发式(默认),1 总是允许(Redis 推荐),2 严格限制", control: Control::Presets(&["0", "1", "2"]), warn: "" },
    Item { key: "vm.max_map_count", title: "内存映射区数量", desc: "Elasticsearch、部分 Java 服务要求 262144 以上", control: Control::Presets(&["65530", "262144", "1048576"]), warn: "" },
];

pub fn item(key: &str) -> Option<&'static Item> {
    ITEMS.iter().find(|i| i.key == key)
}

/// 读取:每个键的当前值、Otter 配置文件里写了什么、内核有没有 bbr
pub fn read_script() -> String {
    let mut s = String::from(PATH_FIX);
    s.push_str("echo '@@VALUES'\n");
    for i in ITEMS {
        s.push_str(&format!("echo \"{} $(sysctl -n {} 2>/dev/null)\"\n", i.key, i.key));
    }
    s.push_str("echo '@@QDISC'; sysctl -n net.core.default_qdisc 2>/dev/null\n");
    s.push_str("echo '@@BBR'\nif grep -qw bbr /proc/sys/net/ipv4/tcp_available_congestion_control 2>/dev/null; then echo yes; elif modprobe -n tcp_bbr >/dev/null 2>&1; then echo module; else echo no; fi\n");
    s.push_str(&format!("echo '@@CONF'; cat {CONF} 2>/dev/null\ntrue\n"));
    s
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SysctlInfo {
    /// (键, 当前值);读不到的键值为空串
    pub values: Vec<(String, String)>,
    /// 配置文件里 Otter 写过的键值
    pub saved: Vec<(String, String)>,
    pub qdisc: String,
    /// 内核能用 bbr(已内置或模块可加载)
    pub bbr: bool,
}

impl SysctlInfo {
    pub fn value(&self, key: &str) -> &str {
        self.values.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or("")
    }

    pub fn saved(&self, key: &str) -> Option<&str> {
        self.saved.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

pub fn parse_sysctl(out: &str) -> Result<SysctlInfo, String> {
    let m = sections(out);
    let Some(vals) = m.get("VALUES") else { return Err("读不到内核参数".into()) };
    let values = vals.iter().filter_map(|l| l.trim().split_once(' ').or_else(|| (!l.trim().is_empty()).then(|| (l.trim(), "")))).map(|(k, v)| (k.to_string(), v.trim().to_string())).collect();
    let saved = m
        .get("CONF")
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with('#') {
                return None;
            }
            l.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .filter(|(k, _)| item(k).is_some() || k == "net.core.default_qdisc")
        .collect();
    Ok(SysctlInfo { values, saved, qdisc: super::common::first(&m, "QDISC"), bbr: matches!(super::common::first(&m, "BBR").as_str(), "yes" | "module") })
}

fn value_ok(it: &Item, v: &str) -> bool {
    match it.control {
        Control::Switch { on, off } => v == on || v == off,
        Control::Presets(p) => p.contains(&v),
        Control::Bbr => matches!(v, "bbr" | "cubic"),
    }
}

/// 写入文件里的一个键:去掉旧行,追加新行
fn upsert(key: &str, val: &str) -> String {
    format!("touch {CONF}; grep -v '^{k}[[:space:]]*=' {CONF} > {CONF}.tmp || true; echo '{key} = {val}' >> {CONF}.tmp; cat {CONF}.tmp > {CONF}; rm -f {CONF}.tmp\n", k = key.replace('.', "\\."))
}

fn drop_line(key: &str) -> String {
    format!("[ -f {CONF} ] && {{ grep -v '^{k}[[:space:]]*=' {CONF} > {CONF}.tmp || true; cat {CONF}.tmp > {CONF}; rm -f {CONF}.tmp; }}\n", k = key.replace('.', "\\."))
}

/// 设一个键的值(立即生效并写进配置文件)
pub fn set_script(key: &str, val: &str) -> Result<String, String> {
    let it = item(key).ok_or_else(|| format!("不支持的内核参数 {key}"))?;
    if !value_ok(it, val) {
        return Err(format!("{key} 不能设成「{val}」"));
    }
    let mut s = format!("{PATH_FIX}set -e\nmkdir -p /etc/sysctl.d\n");
    if it.control == Control::Bbr {
        if val == "bbr" {
            s.push_str("modprobe tcp_bbr 2>/dev/null || true\ngrep -qw bbr /proc/sys/net/ipv4/tcp_available_congestion_control || { echo '这台服务器的内核不支持 BBR(需要 4.9 以上并带 tcp_bbr 模块)' >&2; exit 1; }\nsysctl -w net.core.default_qdisc=fq >/dev/null\nsysctl -w net.ipv4.tcp_congestion_control=bbr >/dev/null\n");
            s.push_str(&upsert("net.core.default_qdisc", "fq"));
            s.push_str(&upsert("net.ipv4.tcp_congestion_control", "bbr"));
            s.push_str("mkdir -p /etc/modules-load.d; echo tcp_bbr > /etc/modules-load.d/otter-bbr.conf\n");
        } else {
            s.push_str("sysctl -w net.ipv4.tcp_congestion_control=cubic >/dev/null\n");
            s.push_str(&drop_line("net.core.default_qdisc"));
            s.push_str(&drop_line("net.ipv4.tcp_congestion_control"));
            s.push_str("rm -f /etc/modules-load.d/otter-bbr.conf\n");
        }
        s.push_str("[ \"$(sysctl -n net.ipv4.tcp_congestion_control)\" = \"");
        s.push_str(val);
        s.push_str("\" ] || { echo '设置没有生效' >&2; exit 1; }\n");
        return Ok(s);
    }
    s.push_str(&format!("sysctl -w {key}={val} >/dev/null\n"));
    s.push_str(&upsert(key, val));
    s.push_str(&format!("[ \"$(sysctl -n {key})\" = \"{val}\" ] || {{ echo '设置没有生效' >&2; exit 1; }}\n"));
    Ok(s)
}

/// 恢复默认:去掉配置文件里的行(运行值不变,重启后恢复系统默认)
pub fn reset_script(key: &str) -> Result<String, String> {
    let it = item(key).ok_or_else(|| format!("不支持的内核参数 {key}"))?;
    let mut s = format!("{PATH_FIX}set -e\n");
    if it.control == Control::Bbr {
        s.push_str(&drop_line("net.core.default_qdisc"));
        s.push_str(&drop_line("net.ipv4.tcp_congestion_control"));
        s.push_str("rm -f /etc/modules-load.d/otter-bbr.conf\n");
    } else {
        s.push_str(&drop_line(key));
    }
    s.push_str("sysctl --system >/dev/null 2>&1 || true\n");
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let out = "@@VALUES\nnet.ipv4.ip_forward 1\nnet.core.somaxconn 4096\nvm.max_map_count \n@@QDISC\nfq_codel\n@@BBR\nmodule\n@@CONF\n# x\nnet.core.somaxconn = 4096\nfoo.bar = 1\nnet.ipv4.ip_forward=1\n";
        let i = parse_sysctl(out).unwrap();
        assert_eq!(i.value("net.ipv4.ip_forward"), "1");
        assert_eq!(i.value("vm.max_map_count"), "");
        assert_eq!(i.saved("net.core.somaxconn"), Some("4096"));
        assert_eq!(i.saved("net.ipv4.ip_forward"), Some("1"));
        assert_eq!(i.saved("foo.bar"), None);
        assert!(i.bbr);
        assert!(parse_sysctl("").is_err());
    }

    #[test]
    fn scripts() {
        let s = set_script("net.core.somaxconn", "4096").unwrap();
        assert!(s.contains("sysctl -w net.core.somaxconn=4096") && s.contains("99-otter.conf"));
        assert!(set_script("net.core.somaxconn", "4096; reboot").is_err());
        assert!(set_script("kernel.panic", "1").is_err());
        assert!(set_script("net.ipv4.ip_forward", "2").is_err());
        let b = set_script("net.ipv4.tcp_congestion_control", "bbr").unwrap();
        assert!(b.contains("default_qdisc=fq") && b.contains("modules-load.d"));
        assert!(reset_script("net.ipv4.tcp_congestion_control").unwrap().contains("rm -f /etc/modules-load.d/otter-bbr.conf"));
        assert!(reset_script("x").is_err());
    }
}
