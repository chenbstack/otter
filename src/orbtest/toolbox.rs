//! 工具箱的真机用例:端口检查(监听、容器发布、诊断结论)、网络诊断(TCP、DNS、ping、追踪、出口 IP)、
//! 时间与时区(读取、改时区、NTP 开关)、Swap(创建、调整、停用/启用、删除、swappiness、回环设备当分区)。
//! 时区和 swap 是整台机器的状态:用例结束一定还原;共用的机器上和其他域的用例互不依赖这些。

use super::harness::{Distro, Vm};
use crate::toolbox::*;

const RESET: &str = r#"
pkill -f 'http.server 1807'; pkill -f 'nc -lk' >/dev/null 2>&1
docker rm -f orbtest-tb >/dev/null 2>&1
for p in /orbtest-swap /orbtest-swap2; do swapoff $p >/dev/null 2>&1; rm -f $p $p.otter-new; sed -i "\#^$p #d;\#otter-off $p #d" /etc/fstab; done
umount /orbtest-mnt /orbtest-tmpfs >/dev/null 2>&1; rmdir /orbtest-mnt /orbtest-tmpfs >/dev/null 2>&1
for l in $(losetup -j /orbtest-loop.img 2>/dev/null | cut -d: -f1); do swapoff $l 2>/dev/null; losetup -d $l 2>/dev/null; done
rm -f /orbtest-loop.img /etc/sysctl.d/99-otter-swap.conf
sed -i '/orbtest-loop/d;/otter-off \/dev\/loop/d' /etc/fstab
[ -f /tmp/orbtest-tz ] && timedatectl set-timezone "$(cat /tmp/orbtest-tz)" >/dev/null 2>&1
true
"#;

fn tb(d: Distro) -> super::harness::Lease {
    Vm::shared(d, "toolbox", RESET)
}

/// 起一个后台监听:有 python3 用它(进程名清楚),没有(Debian 最小系统)用 nc
fn listen_cmd(addr: &str, port: u16) -> String {
    format!("if command -v python3 >/dev/null 2>&1; then (cd /tmp; setsid nohup python3 -m http.server {port} --bind {addr} >/dev/null 2>&1 < /dev/null &); else (setsid nohup nc -lk {addr} {port} >/dev/null 2>&1 < /dev/null &); fi; ")
}

fn listens(vm: &Vm) -> Vec<Listen> {
    parse_listens(&vm.sh(PORTS_SCRIPT))
}

fn net(vm: &Vm, t: &NetTool, host: &str) -> NetResult {
    parse_net(&vm.sh(&net_script(t, host).unwrap()))
}

fn ports(d: Distro) {
    let vm = tb(d);
    vm.pull("openresty/openresty:alpine");
    vm.exec("起监听", &format!("{}{}sleep 1; docker run -d --name orbtest-tb -p 18073:80 openresty/openresty:alpine >/dev/null; sleep 2", listen_cmd("127.0.0.1", 18071), listen_cmd("0.0.0.0", 18072)));
    let l = listens(&vm);
    let g = group_listens(&l);
    let local = g.iter().find(|x| x.port == 18071).expect("18071");
    assert!(local.local_only() && local.procs.iter().any(|(n, _)| n.starts_with("python") || n.contains("nc")), "{local:?}");
    let any = g.iter().find(|x| x.port == 18072).expect("18072");
    assert!(!any.local_only(), "{any:?}");

    let fw = crate::firewall::parse(&vm.sh(crate::firewall::READ_SCRIPT));
    let cs = crate::docker::parse_ps(&vm.sh(&format!("docker ps -a --format '{}'", crate::docker::PS_FORMAT)));
    let d71 = diagnose(18071, "tcp", &l, &cs, Some(&fw));
    assert_eq!(d71[0].level, Level::Bad, "{d71:?}");
    assert!(d71.iter().any(|x| x.text.contains("只监听在 127.0.0.1")), "{d71:?}");
    let d72 = diagnose(18072, "tcp", &l, &cs, Some(&fw));
    assert_eq!(d72[0].level, Level::Good, "{d72:?}");
    let d73 = diagnose(18073, "tcp", &l, &cs, Some(&fw));
    assert!(d73.iter().any(|x| x.text.contains("容器 orbtest-tb")), "{d73:?}");
    let d9 = diagnose(18079, "tcp", &l, &cs, Some(&fw));
    assert!(d9[0].text.contains("没有程序监听"), "{d9:?}");
    vm.sh("pkill -f 'http.server 1807'; pkill -f 'nc -lk'; docker rm -f orbtest-tb");
}
per_distro!(ports, ports);

fn network(d: Distro) {
    let vm = tb(d);
    vm.sh(&format!("{}sleep 1", listen_cmd("0.0.0.0", 18072)));
    // TCP:通和不通
    let r = net(&vm, &NetTool::Tcp(18072), "127.0.0.1");
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.text.contains("已连通"), "{r:?}");
    let r = net(&vm, &NetTool::Tcp(18079), "127.0.0.1");
    assert_ne!(r.code, 0);
    assert!(r.text.contains("连不上"), "{r:?}");
    // ping / 路由追踪:有命令就要成功,没有命令要说清楚缺什么
    let r = net(&vm, &NetTool::Ping, "127.0.0.1");
    assert!(r.code == 0 || r.missing.is_some(), "{r:?}");
    let r = net(&vm, &NetTool::Trace, "127.0.0.1");
    assert!(r.code == 0 || r.missing.is_some(), "{r:?}");
    // DNS(要外网)
    let r = net(&vm, &NetTool::Dns("A".into(), String::new()), "example.com");
    assert!(r.code == 0 && !r.text.is_empty() || r.missing.is_some(), "{r:?}");
    let r = net(&vm, &NetTool::Dns("A".into(), "1.1.1.1".into()), "example.com");
    assert!(r.code == 0 || r.missing.is_some(), "{r:?}");
    // 出口 IP
    let r = net(&vm, &NetTool::PublicIp, "");
    assert!(r.code == 0 && r.text.trim().chars().any(|c| c == '.' || c == ':'), "{r:?}");
    vm.sh("pkill -f 'http.server 1807'; pkill -f 'nc -lk'");
}
per_distro!(network, network);

fn time(d: Distro) {
    let vm = tb(d);
    let read = || parse_time(&vm.sh(TIME_SCRIPT)).expect("读时间");
    let t = read();
    assert!(t.has_timedatectl && !t.tz.is_empty() && t.zones.iter().any(|z| z == "Asia/Shanghai"), "{t:?}");
    assert!(t.epoch > 1_700_000_000, "{t:?}");
    // 页面上的时钟按读取时的时间往后推:推 0 秒就是原样
    assert_eq!(shift_date(&t.date, 0), t.date);
    vm.sh(&format!("echo {} > /tmp/orbtest-tz", t.tz));
    // 改时区再读回,日期的偏移也跟着变
    let other = if t.tz == "Asia/Tokyo" { "Asia/Shanghai" } else { "Asia/Tokyo" };
    vm.exec("改时区", &set_tz_script(other).unwrap());
    let t2 = read();
    assert_eq!(t2.tz, other, "{t2:?}");
    assert!(t2.date.ends_with(if other == "Asia/Tokyo" { "+0900" } else { "+0800" }), "{}", t2.date);
    assert!(set_tz_script("No/Such_Zone").is_ok());
    let (c, _) = vm.run(&set_tz_script("No/Such_Zone").unwrap());
    assert_ne!(c, 0, "不存在的时区应该失败");
    assert_eq!(read().tz, other, "失败后时区不该变");
    vm.exec("改回", &set_tz_script(&t.tz).unwrap());
    assert_eq!(read().tz, t.tz);
    // NTP 开关:机器上有时间服务才能开关;没有时要给出错误,不是悄悄成功
    // (机器上一个时间服务都没有时 `set-ntp` 也返回成功,但状态不变:页面只在有服务时提供这个开关)
    let (c, o) = vm.run(&set_ntp_script(!t.ntp.unwrap_or(false)));
    println!("set-ntp: exit {c} {o};时间服务 {:?}", t.services);
    if c == 0 {
        assert_eq!(read().ntp, Some(!t.ntp.unwrap_or(false)));
        vm.exec("NTP 还原", &set_ntp_script(t.ntp.unwrap_or(false)));
    } else {
        // 没生效就必须报错,而不是悄悄成功
        assert!(o.contains("没有生效") || o.contains("没有 timedatectl") || o.to_lowercase().contains("fail") || o.contains("not"), "{o}");
        assert_eq!(read().ntp, t.ntp, "失败后 NTP 状态不该变");
    }
    // NTP 服务器:机器上有时间服务才能改(容器里常常没有,跳过)
    if !t.ntp_backend.is_empty() {
        let b = t.ntp_backend.clone();
        let orig = t.ntp_servers.clone();
        let (c, o) = vm.run(&set_ntp_server_script(&b, Some("ntp.aliyun.com")).unwrap());
        println!("改 NTP 服务器({b}): exit {c} {o}");
        if c == 0 {
            let t3 = read();
            assert!(t3.ntp_servers.iter().any(|s| s == "ntp.aliyun.com") && t3.ntp_by_otter, "{t3:?}");
            // 改回默认:恢复原来的配置
            vm.exec("NTP 服务器改回默认", &set_ntp_server_script(&b, None).unwrap());
            let t4 = read();
            assert!(!t4.ntp_by_otter && t4.ntp_servers == orig, "{t4:?} 原来 {orig:?}");
        }
    }
}
per_distro!(time, time);

/// OrbStack 的机器共用一个内核:swap(/proc/swaps、SwapTotal)和 vm.swappiness 是整个内核的,
/// 不同发行版的 swap 用例不能同时跑,否则互相占着路径、改着总量
static SWAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn swap(d: Distro) {
    let _k = SWAP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let vm = tb(d);
    let sw0 = vm.sh("cat /proc/sys/vm/swappiness").trim().to_string();
    let read = || parse_swap(&vm.sh(SWAP_SCRIPT)).expect("读 swap");
    let dev = |i: &SwapInfo, p: &str| i.devs.iter().find(|x| x.path == p).cloned();
    let base = read();
    assert!(base.mem_total_kb > 0 && base.root_avail_kb > 0 && !base.root_fs.is_empty(), "{base:?}");

    // 创建:启用、600、记进 fstab(带 # otter)
    vm.exec("创建", &swap_create_script("/orbtest-swap", 256).unwrap());
    let i = read();
    let s = dev(&i, "/orbtest-swap").expect("创建后没出现在 swap 列表");
    assert_eq!(s.kind, SwapKind::File);
    assert!((s.size_kb as i64 - 262_140).abs() < 64, "{s:?}");
    assert!(s.in_fstab && s.otter, "{s:?}");
    assert_eq!(vm.sh("stat -c %a /orbtest-swap").trim(), "600");
    assert_eq!(i.swap_total_kb, base.swap_total_kb + s.size_kb);
    // 已存在时拒绝,不破坏原来的
    let (c, o) = vm.run(&swap_create_script("/orbtest-swap", 256).unwrap());
    assert_ne!(c, 0);
    assert!(o.contains("已经存在"), "{o}");
    assert!(dev(&read(), "/orbtest-swap").is_some());
    // 空间不够(小文件系统上,含 1 GB 余量)和不支持的文件系统(tmpfs):拒绝,不留半成品
    let tm = vm.run("mkdir -p /orbtest-tmpfs && mount -t tmpfs -o size=64m tmpfs /orbtest-tmpfs");
    assert_eq!(tm.0, 0, "{}", tm.1);
    let (c, o) = vm.run(&swap_create_script("/orbtest-tmpfs/swap", 64).unwrap());
    assert_ne!(c, 0);
    assert!(o.contains("不能放 swap 文件"), "{o}");
    vm.sh("umount /orbtest-tmpfs; rmdir /orbtest-tmpfs");
    // 小的 ext4/xfs 文件系统(机器上有 mkfs 才测):空间不够,拒绝,不留半成品
    let small = vm.run("command -v mkfs.ext4 >/dev/null 2>&1 && M='mkfs.ext4 -q -F' || { command -v mkfs.xfs >/dev/null 2>&1 && M='mkfs.xfs -q -f'; } || exit 9; set -e; dd if=/dev/zero of=/orbtest-loop.img bs=1M count=256 status=none; $M /orbtest-loop.img; mkdir -p /orbtest-mnt; mount -o loop /orbtest-loop.img /orbtest-mnt");
    if small.0 == 0 {
        let (c, o) = vm.run(&swap_create_script("/orbtest-mnt/swap", 64).unwrap());
        assert_ne!(c, 0);
        assert!(o.contains("磁盘空间不够"), "{o}");
        assert!(vm.sh("ls /orbtest-mnt/swap 2>&1").contains("No such file"), "留下了半成品");
        assert!(!vm.sh("cat /etc/fstab").contains("/orbtest-mnt"));
        vm.sh("umount /orbtest-mnt; rmdir /orbtest-mnt; rm -f /orbtest-loop.img");
    } else {
        println!("没有 mkfs,跳过「空间不够」的用例:{}", small.1);
        vm.sh("rm -f /orbtest-loop.img");
    }

    // 调整大小
    vm.exec("调整", &swap_resize_script("/orbtest-swap", 384).unwrap());
    let s = dev(&read(), "/orbtest-swap").expect("调整后没了");
    assert!((s.size_kb as i64 - 393_212).abs() < 64, "{s:?}");
    assert!(s.in_fstab, "调整后 fstab 里的行不该没了");
    assert_eq!(vm.sh("ls /orbtest-swap.otter-new 2>&1").contains("No such file"), true);

    // 停用:不再启用、文件还在、fstab 行被标记;再启用
    vm.exec("停用", &swap_off_script("/orbtest-swap").unwrap());
    let i = read();
    assert!(dev(&i, "/orbtest-swap").is_none());
    assert!(i.off.iter().any(|o| o.spec == "/orbtest-swap" && o.by_otter), "{:?}", i.off);
    assert!(vm.sh("test -f /orbtest-swap && echo yes").contains("yes"));
    vm.exec("启用", &swap_on_script());
    assert!(dev(&read(), "/orbtest-swap").is_some_and(|s| s.in_fstab));
    // 「重启后还在」:关掉再按 fstab 全部启用
    vm.sh("swapoff /orbtest-swap; swapon -a");
    assert!(dev(&read(), "/orbtest-swap").is_some(), "fstab 里的 swap 重启后应该还在");

    // swappiness
    vm.exec("swappiness", &set_swappiness_script(15).unwrap());
    let i = read();
    assert_eq!(i.swappiness, Some(15));
    assert!(i.swappiness_set);
    assert!(vm.sh("cat /etc/sysctl.d/99-otter-swap.conf").contains("vm.swappiness = 15"));

    // 回环设备当「分区」:停用 / 启用,不动设备
    let loopdev = vm.sh("dd if=/dev/zero of=/orbtest-loop.img bs=1M count=64 status=none && chmod 600 /orbtest-loop.img && losetup -f --show /orbtest-loop.img 2>/dev/null").trim().to_string();
    if loopdev.starts_with("/dev/loop") {
        vm.exec("回环 swap", &format!("mkswap {loopdev} >/dev/null && swapon {loopdev} && echo '{loopdev} none swap sw 0 0 # orbtest-loop' >> /etc/fstab"));
        let i = read();
        let p = dev(&i, &loopdev).expect("回环设备没出现");
        assert_eq!(p.kind, SwapKind::Partition);
        assert!(p.in_fstab && !p.otter, "{p:?}");
        let (c, o) = vm.run(&swap_delete_script(&loopdev).unwrap_or_default());
        assert!(swap_delete_script(&loopdev).is_err(), "分区不允许删除:{c} {o}");
        vm.exec("停用分区", &swap_off_script(&loopdev).unwrap());
        let i = read();
        assert!(dev(&i, &loopdev).is_none());
        assert!(i.off.iter().any(|o| o.spec == loopdev && o.by_otter), "{:?}", i.off);
        assert!(vm.sh("ls /dev/loop* | head -50").contains(&loopdev), "设备不该被动");
        vm.exec("启用分区", &swap_on_script());
        assert!(dev(&read(), &loopdev).is_some());
    } else {
        println!("这台机器没有回环设备,跳过「分区」的部分");
    }

    // 删除:swapoff、fstab 行、文件都没了
    vm.exec("删除", &swap_delete_script("/orbtest-swap").unwrap());
    let i = read();
    assert!(dev(&i, "/orbtest-swap").is_none());
    assert!(!vm.sh("cat /etc/fstab").contains("/orbtest-swap "), "fstab 里还有");
    assert!(vm.sh("ls /orbtest-swap 2>&1").contains("No such file"));
    // 清理
    vm.run(RESET);
    vm.sh(&format!("sysctl -w vm.swappiness={sw0}"));
    assert_eq!(read().swap_total_kb, base.swap_total_kb, "还原后 swap 总量应回到原来");
}
per_distro!(swap, swap);
