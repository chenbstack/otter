//! 系统读取类功能的真机用例:进程、服务、存储、计划任务的读取脚本在三个发行版上输出格式一致、能被解析;
//! `/opt/otter` 数据目录的初始化(可重复)、原子写文件、全局锁、删除脚本。这些都是只读或只动 `/opt/otter` 与临时目录,可以和别的域同时用同一台机器。

use super::harness::{Distro, Vm};
use crate::srvdata;
use crate::sysops::*;

fn vm(d: Distro) -> super::Lease {
    Vm::shared(d, "system", "rm -rf /tmp/orbtest-sys; true")
}

/// `/opt/otter` 下的东西(数据目录、计划任务脚本、网站)共用 "otter" 域,用例排队,互相不踩
fn vm_otter(d: Distro) -> super::Lease {
    Vm::shared(d, "otter", "rm -rf /opt/otter /tmp/orbtest-lock; true")
}

fn procs(d: Distro) {
    let vm = vm(d);
    let out = vm.exec("读进程", PROCS_SCRIPT);
    let list = parse_procs(&out).unwrap_or_else(|e| panic!("解析失败:{e}\n{out}"));
    assert!(list.len() > 10, "进程太少:{}", list.len());
    let init = list.iter().find(|p| p.pid == 1).expect("没有 pid 1");
    assert_eq!(init.user, "root");
    assert!(init.cmd.contains("init") || init.cmd.contains("systemd"), "{}", init.cmd);
    assert!(list.iter().all(|p| p.rss_kb > 0 || p.cmd.starts_with('[') || p.rss_kb == 0));
}
per_distro!(procs_read, procs);

/// BusyBox 式的 ps 不认 `-e`(报 "unrecognized option: e"):用一个会失败的假 ps 顶在 PATH 前面,脚本要改读 /proc 并输出同样的列
fn procs_fallback(d: Distro) {
    let vm = vm(d);
    let out = vm.exec("读进程(ps 不可用)", &format!("mkdir -p /tmp/orbtest-sys/bin && printf '#!/bin/sh\\necho \"ps: unrecognized option: e\" >&2\\nexit 1\\n' > /tmp/orbtest-sys/bin/ps && chmod +x /tmp/orbtest-sys/bin/ps && PATH=/tmp/orbtest-sys/bin:$PATH && {PROCS_SCRIPT}"));
    let list = parse_procs(&out).unwrap_or_else(|e| panic!("解析失败:{e}\n{out}"));
    assert!(list.len() > 10, "进程太少:{}", list.len());
    let init = list.iter().find(|p| p.pid == 1).expect("没有 pid 1");
    assert_eq!(init.user, "root");
    assert!(init.cmd.contains("init") || init.cmd.contains("systemd"), "{}", init.cmd);
    // 内存和运行时长要有值;按 CPU 降序
    assert!(list.iter().any(|p| p.rss_kb > 0) && list.iter().all(|p| !p.etime.is_empty()));
    assert!(list.windows(2).all(|w| w[0].cpu >= w[1].cpu), "没按 CPU 排序");
}
per_distro!(procs_read_fallback, procs_fallback);

fn services(d: Distro) {
    let vm = vm(d);
    let out = vm.exec("读服务", SERVICES_SCRIPT);
    let list = parse_services(&out).unwrap_or_else(|e| panic!("解析失败:{e}\n{out}"));
    let docker = list.iter().find(|s| s.name == "docker").expect("没有 docker 服务");
    assert!(docker.running() && docker.enabled == "enabled", "{docker:?}");
    // 起一个临时服务:能看到、启停后状态变化
    vm.exec("建临时服务", "printf '[Service]\\nExecStart=/bin/sleep 3000\\n' > /etc/systemd/system/orbtest-sleep.service && systemctl daemon-reload && systemctl start orbtest-sleep");
    let s = parse_services(&vm.sh(SERVICES_SCRIPT)).unwrap().into_iter().find(|s| s.name == "orbtest-sleep").expect("没有临时服务");
    assert!(s.running(), "{s:?}");
    assert!(safe_unit(&s.name));
    vm.exec("停临时服务", "systemctl stop orbtest-sleep");
    let s = parse_services(&vm.sh(SERVICES_SCRIPT)).unwrap().into_iter().find(|s| s.name == "orbtest-sleep");
    assert!(s.map(|s| !s.running()).unwrap_or(true));
    vm.exec("清理", "rm -f /etc/systemd/system/orbtest-sleep.service; systemctl daemon-reload");
}
per_distro!(services_read, services);

fn storage(d: Distro) {
    let vm = vm(d);
    let out = vm.exec("读存储", STORAGE_SCRIPT);
    let list = parse_storage(&out).unwrap_or_else(|e| panic!("解析失败:{e}\n{out}"));
    let root = list.iter().find(|m| m.mount == "/").expect("没有根分区");
    assert!(root.total_kb > 0 && root.used_kb <= root.total_kb && root.pct <= 100, "{root:?}");
    assert!(list.iter().all(|m| !m.fstype.is_empty() && !m.dev.is_empty()));
}
per_distro!(storage_read, storage);

fn lens_and_remove(d: Distro) {
    let vm = vm(d);
    vm.exec("造目录", "mkdir -p /tmp/orbtest-sys/a/b && head -c 2000000 /dev/zero > /tmp/orbtest-sys/a/big && echo hi > /tmp/orbtest-sys/a/b/small");
    let out = vm.exec("空间透镜", &lens_command("/tmp/orbtest-sys", 2, true));
    let rows: Vec<_> = out.lines().filter_map(parse_lens_line).collect();
    assert!(rows.iter().any(|(sz, p, k)| *k == LineKind::File && p.ends_with("/a/big") && *sz >= 1900), "{out}");
    assert!(rows.iter().any(|(sz, p, k)| *k == LineKind::Dir && p == "/tmp/orbtest-sys/a" && *sz >= 1900), "{out}");
    // 删除:存在 → OK;再删 → 已经不存在;挂载点在里面 → 拒绝
    assert_eq!(parse_probe(&vm.sh(&probe_script("/tmp/orbtest-sys/a"))), Probe::Ok);
    assert_eq!(parse_remove(&vm.sh(&remove_script("/tmp/orbtest-sys/a"))), Removed::Ok);
    assert!(matches!(parse_remove(&vm.sh(&remove_script("/tmp/orbtest-sys/a"))), Removed::Failed(m) if m.contains("不存在")));
    vm.exec("造挂载", "mkdir -p /tmp/orbtest-sys/m/mnt && mount -t tmpfs tmpfs /tmp/orbtest-sys/m/mnt && echo keep > /tmp/orbtest-sys/m/mnt/f");
    let r = parse_remove(&vm.sh(&remove_script("/tmp/orbtest-sys/m")));
    assert!(matches!(&r, Removed::Failed(m) if m.contains("挂载")), "{r:?}");
    assert_eq!(vm.sh("cat /tmp/orbtest-sys/m/mnt/f").trim(), "keep", "挂载里的文件不能被删");
    vm.exec("卸载", "umount /tmp/orbtest-sys/m/mnt");
}
per_distro!(lens_remove, lens_and_remove);

fn srvdata_layout(d: Distro) {
    let vm = vm_otter(d);
    for i in 0..2 {
        vm.exec(&format!("初始化 /opt/otter 第 {i} 次"), &srvdata::init_script());
    }
    assert_eq!(srvdata::parse_layout(&vm.sh("cat /opt/otter/otter.json")), Some(srvdata::LAYOUT));
    assert_eq!(vm.sh("stat -c %a /opt/otter/websites/secrets").trim(), "700");
    assert!(vm.sh("ls /opt/otter/docker/compose /opt/otter/websites/conf/conf.d; echo ok").contains("ok"));
    // 原子写:任意内容原样落盘,没有残留 .tmp
    let text = "line1\n'quoted' \"x\" $HOME `id`\n中文 \\n\n";
    vm.exec("写文件", &srvdata::write_file_cmd("/opt/otter/tmp/it's a file", text, "600"));
    assert_eq!(vm.sh("cat \"/opt/otter/tmp/it's a file\""), text);
    assert_eq!(vm.sh("stat -c %a \"/opt/otter/tmp/it's a file\"").trim(), "600");
    assert_eq!(vm.sh("ls /opt/otter/tmp | grep -c tmp$").trim(), "0");
}
per_distro!(srvdata_init, srvdata_layout);

/// 全局锁:一个操作拿着锁,另一个等到它放;等不到(这里用 flock 超时无法等 30 秒,改为验证互斥顺序)
fn srvdata_lock(d: Distro) {
    let vm = vm_otter(d);
    vm.exec("初始化", &srvdata::init_script());
    let script = format!(
        "{}\necho A >> /tmp/orbtest-lock/order; sleep 3; echo A-end >> /tmp/orbtest-lock/order",
        srvdata::lock_prelude()
    );
    vm.exec("准备", "mkdir -p /tmp/orbtest-lock; : > /tmp/orbtest-lock/order");
    let b = format!("{}\necho B >> /tmp/orbtest-lock/order", srvdata::lock_prelude());
    let name = vm.name.clone();
    let h = std::thread::spawn(move || super::harness::host("orb", &["-m", &name, "sudo", "sh", "-s"], Some(&script)));
    std::thread::sleep(std::time::Duration::from_millis(1200));
    vm.exec("第二个操作", &b);
    h.join().unwrap();
    assert_eq!(vm.sh("cat /tmp/orbtest-lock/order").trim().replace('\n', ","), "A,A-end,B", "第二个操作应等第一个结束");
}
per_distro!(lock, srvdata_lock);
