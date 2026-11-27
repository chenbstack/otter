//! 计划任务的真机用例:Docker 清理任务装到 `/etc/cron.d`、脚本真的清理、读取脚本能认出并解析回来、删除干净;
//! 证书续期任务的安装/改时间/删除;cron 守护进程真的在跑(慢用例:等一分钟看它真的触发)。

use super::harness::{Distro, Vm};
use crate::crontasks::*;
use crate::sysops::{parse_cron, CRON_SCRIPT};

const RESET: &str = r#"
rm -f /etc/cron.d/otter-* /tmp/orbtest-cron-*
rm -rf /opt/otter/docker/bin /opt/otter/docker/logs /opt/otter/websites/bin /opt/otter/websites/logs
docker rm -f orbtest-dead orbtest-keep >/dev/null 2>&1; docker network rm orbtest-net >/dev/null 2>&1; docker volume rm orbtest-vol >/dev/null 2>&1
true
"#;

fn vm(d: Distro) -> super::Lease {
    Vm::shared(d, "otter", RESET)
}

fn prune(d: Distro) {
    let vm = vm(d);
    let id = "ab12cd";
    let o = PruneOpts { dangling: true, containers: true, networks: true, builder: false, all_images: false, days: 0 };
    let sched = "0 4 * * 0";
    let body = prune_script(&o).unwrap();
    let ins = install(&format!("prune-{id}.sh"), &body, &prune_crond(id), sched, &prune_cmd("@@BIN@@", id, &o)).unwrap();
    vm.exec("安装清理任务", &ins);
    assert_eq!(vm.sh(&format!("stat -c %a /etc/cron.d/otter-docker-prune-{id} {BIN}/prune-{id}.sh")).trim().replace('\n', ","), "644,700");

    // 读回:列表能认出这条 root 任务,标记能还原成同样的选项
    let info = parse_cron(&vm.sh(CRON_SCRIPT));
    let job = info.jobs.iter().find(|j| j.source == prune_crond(id)).unwrap_or_else(|| panic!("计划任务读取里没有它:{:?}", info.jobs));
    assert_eq!((job.schedule.as_str(), job.user.as_str()), (sched, "root"));
    let t = parse_prune(&job.cmd).expect("标记解析失败");
    assert_eq!((t.id.as_str(), t.opts), (id, o));
    assert_eq!(t.script, format!("{BIN}/prune-{id}.sh"));

    // 真的执行一次:造一个已停止的容器 + 一个没用的网络,脚本跑完它们都没了,日志有记录
    vm.exec("造垃圾", "docker create --name orbtest-dead openresty/openresty:alpine >/dev/null && docker network create orbtest-net >/dev/null && docker create --name orbtest-keep openresty/openresty:alpine >/dev/null && docker rm orbtest-keep >/dev/null");
    assert!(vm.sh("docker ps -a --format '{{.Names}}'").contains("orbtest-dead"));
    vm.exec("执行清理脚本", &t.script);
    let left = vm.sh("docker ps -a --format '{{.Names}}'; docker network ls --format '{{.Name}}'");
    assert!(!left.contains("orbtest-dead") && !left.contains("orbtest-net"), "没清掉:{left}");
    let log = vm.sh(&format!("cat {LOGS}/prune.log"));
    assert!(log.contains("已停止的容器") && log.contains("未使用的网络") && !log.contains("失败"), "{log}");
    // 存储卷不清理
    vm.exec("造存储卷", "docker volume create orbtest-vol >/dev/null");
    vm.exec("再执行", &t.script);
    assert!(vm.sh("docker volume ls --format '{{.Name}}'").contains("orbtest-vol"), "存储卷不应被清理");
    vm.exec("清理存储卷", "docker volume rm orbtest-vol");

    // 日志只留最后 200 行
    vm.exec("灌日志", &format!("for i in $(seq 1 400); do echo line$i; done >> {LOGS}/prune.log"));
    vm.exec("再执行", &t.script);
    assert!(vm.sh(&format!("wc -l < {LOGS}/prune.log")).trim().parse::<u32>().unwrap() <= 205);

    // 删除:cron 文件没了,读取里也没了
    vm.exec("删除任务", &format!("rm -f {} {BIN}/prune-{id}.sh", prune_crond(id)));
    assert!(!parse_cron(&vm.sh(CRON_SCRIPT)).jobs.iter().any(|j| j.source == prune_crond(id)));
}
per_distro!(docker_prune, prune);

fn cert_renew(d: Distro) {
    let vm = vm(d);
    vm.exec("装续期任务", &crate::web::cron_script(17, 3));
    let info = parse_cron(&vm.sh(CRON_SCRIPT));
    let job = info.jobs.iter().find(|j| is_cert_renew(&j.source, &j.cmd)).unwrap_or_else(|| panic!("认不出续期任务:{:?}", info.jobs));
    assert_eq!((job.schedule.as_str(), job.user.as_str()), ("17 3 * * *", "root"));
    assert_eq!(vm.sh(&format!("stat -c %a {}", crate::web::RENEW_SH)).trim(), "755");
    vm.exec("改时间", &crate::web::cron_script(5, 22));
    let job = parse_cron(&vm.sh(CRON_SCRIPT)).jobs.into_iter().find(|j| is_cert_renew(&j.source, &j.cmd)).unwrap();
    assert_eq!(job.schedule, "5 22 * * *");
    vm.exec("删除", &crate::web::cron_remove_script());
    assert!(!parse_cron(&vm.sh(CRON_SCRIPT)).jobs.iter().any(|j| is_cert_renew(&j.source, &j.cmd)));
}
per_distro!(cert_renew_task, cert_renew);

/// cron 守护进程真的在跑、真的会执行 /etc/cron.d 里的文件(等一分钟,慢用例)
fn cron_fires(d: Distro) {
    let vm = vm(d);
    let ins = install("fire.sh", "#!/bin/sh\ndate +%s > /tmp/orbtest-cron-fired\n", "/etc/cron.d/otter-orbtest-fire", "* * * * *", "@@BIN@@/fire.sh").unwrap();
    vm.exec("装每分钟任务", &ins);
    let svc = vm.sh("systemctl is-active cron crond 2>/dev/null | grep -c '^active$'");
    assert_eq!(svc.trim(), "1", "cron 守护进程没在跑");
    let t = std::time::Instant::now();
    loop {
        if vm.sh("cat /tmp/orbtest-cron-fired 2>/dev/null").trim().parse::<u64>().is_ok() {
            break;
        }
        assert!(t.elapsed().as_secs() < 100, "100 秒内 cron 没有触发");
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    vm.exec("清理", "rm -f /etc/cron.d/otter-orbtest-fire");
}

distros!(fires, cron_fires, gate_slow, ALL);
