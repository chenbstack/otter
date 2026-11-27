//! 应用商店的真机用例(以内置目录里的 Redis 为例):渲染 → 安装(compose 起容器、密码只在 `.env`)→ 读回记录 →
//! 升级(数据保留)→ 卸载(保留数据)→ 卸载并删除数据。覆盖「安装 → 运行 → 升级 → 卸载」整条链路。

use super::harness::{Distro, Vm};
use crate::appstore::*;

const RESET: &str = r#"
docker rm -f otter-orbtest-redis >/dev/null 2>&1
rm -rf /opt/otter/docker/compose/otter-orbtest-redis /opt/otter/jobs/app /opt/otter/trash
true
"#;

const NAME: &str = "otter-orbtest-redis";
const PORT: &str = "16379";

/// 跑一个后台任务到结束。Docker Hub 偶尔超时(`compose pull` 即使镜像在本地也要问一次仓库):
/// 日志里是网络错误就重试,最多 3 次;别的失败直接报
fn run_job(vm: &Vm, what: &str, body: &str) -> String {
    for attempt in 1..=3 {
        let o = vm.exec(&format!("启动任务:{what}"), &job_start_script(body));
        assert!(o.contains("OK"), "{o}");
        let t = std::time::Instant::now();
        let p = loop {
            let p = crate::web::parse_job_poll(&vm.sh(&job_poll_script(0)));
            if p.status.is_some() {
                break p;
            }
            assert!(t.elapsed().as_secs() < 300, "{what} 超时:\n{}", p.log);
            std::thread::sleep(std::time::Duration::from_secs(2));
        };
        vm.exec("清任务", &job_clear_script());
        if p.status == Some(0) {
            return p.log;
        }
        let net = ["timeout", "TLS handshake", "resolve reference", "EOF", "connection reset"].iter().any(|k| p.log.contains(k));
        assert!(net && attempt < 3, "{what} 失败:\n{}", p.log);
        println!("{what}:网络错误,重试(第 {attempt} 次)");
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    unreachable!()
}

fn redis(vm: &Vm, pw: &str, args: &str) -> String {
    vm.sh(&format!("docker exec {NAME} redis-cli -a '{pw}' --no-auth-warning {args}")).trim().to_string()
}

fn lifecycle(d: Distro) {
    let vm = Vm::shared(d, "otter", RESET);
    let cat = embedded();
    let app = cat.apps.iter().find(|a| a.id == "redis").unwrap();

    // 渲染:填默认值,改名、端口、版本
    let mut v = default_values(app, &[], &[]);
    v.project = NAME.into();
    v.container = NAME.into();
    v.version = "7.2-alpine".into();
    v.params.insert("port".into(), PORT.into());
    let pw = v.params["password"].clone();
    let plan = render(app, &v).unwrap();
    assert!(plan.env.contains(&pw) && !plan.compose.contains(&pw), "密码只应出现在 .env,不能写进 compose");
    let rec = Record::new(&plan, &v);

    // 先把两个版本的镜像拉好(compose 自己拉遇到网络抖动不会重试)
    vm.pull("redis:7.2-alpine");
    vm.pull("redis:7.4-alpine");

    // 安装
    let log = run_job(&vm, "安装", &install_script(app, &plan, &rec, false));
    assert!(log.contains("已启动"), "{log}");
    assert_eq!(vm.sh(&format!("docker inspect -f '{{{{.State.Running}}}}' {NAME}")).trim(), "true");
    assert_eq!(redis(&vm, &pw, "ping"), "PONG");
    assert!(vm.sh(&format!("docker exec {NAME} redis-cli ping 2>&1")).contains("NOAUTH"), "没带密码不应能访问");
    let listen = vm.sh(&format!("ss -ltnH 'sport = :{PORT}'"));
    assert!(listen.contains(&format!("127.0.0.1:{PORT}")), "默认应只绑定 127.0.0.1:{listen}");
    assert_eq!(vm.sh(&format!("stat -c %a {}/.env", plan.dir)).trim(), "600", ".env 权限应为 600");

    // 读回记录和 compose
    let (r, compose) = parse_record_output(&vm.sh(&read_record_script(&plan.dir))).expect("读不回记录");
    assert_eq!((r.app.as_str(), r.version.as_str(), r.container.as_str()), ("redis", "7.2-alpine", NAME));
    assert_eq!(compose.trim(), plan.compose.trim());

    redis(&vm, &pw, "set k v1");
    // 重复安装同名容器:被拒绝,现有的不受影响
    let o = vm.exec("启动重复安装", &job_start_script(&install_script(app, &plan, &rec, false)));
    assert!(o.contains("OK"));
    assert_ne!(wait_job(&vm), 0, "同名容器应拒绝安装");
    vm.exec("清任务", &job_clear_script());
    assert_eq!(redis(&vm, &pw, "get k"), "v1", "重复安装失败不应影响现有容器");

    // 升级:换版本,数据还在,记录更新
    check_upgrade(app, "7.2-alpine", "7.4-alpine").unwrap();
    let v2 = r.values_for(app, "7.4-alpine");
    let plan2 = render(app, &v2).unwrap();
    let rec2 = Record { version: "7.4-alpine".into(), ..r.clone() };
    let log = run_job(&vm, "升级", &upgrade_script(app, &plan2, &rec2));
    assert!(log.contains("已升级"), "{log}");
    assert!(vm.sh(&format!("docker inspect -f '{{{{.Config.Image}}}}' {NAME}")).contains("7.4-alpine"));
    assert_eq!(redis(&vm, &pw, "get k"), "v1", "升级后数据应保留");
    assert_eq!(redis(&vm, &pw, "ping"), "PONG", "升级后密码应沿用");
    let (r, _) = parse_record_output(&vm.sh(&read_record_script(&plan.dir))).unwrap();
    assert_eq!(r.version, "7.4-alpine");
    assert!(check_upgrade(app, "7.4-alpine", "7.4-alpine").is_err());

    // 升级到一个不存在的版本:失败并还原,旧版本继续跑
    let bad = render(app, &r.values_for(app, "0.0.1-nonexistent")).unwrap();
    let o = vm.exec("启动失败升级", &job_start_script(&upgrade_script(app, &bad, &r)));
    assert!(o.contains("OK"));
    let st = wait_job(&vm);
    assert_ne!(st, 0, "不存在的版本应当失败");
    assert!(vm.sh(&format!("docker inspect -f '{{{{.Config.Image}}}}' {NAME}")).contains("7.4-alpine"), "失败后应还原到旧版本");
    assert_eq!(redis(&vm, &pw, "get k"), "v1");
    vm.exec("清任务", &job_clear_script());

    // 卸载(保留数据):容器没了,目录和数据还在
    let o = vm.exec("卸载", &run_script(&uninstall_script(&plan.dir, NAME, false).unwrap()));
    let _ = o;
    assert!(!vm.sh("docker ps -a --format '{{.Names}}'").contains(NAME));
    assert!(vm.sh(&format!("ls {}/data", plan.dir)).contains("appendonlydir") || vm.sh(&format!("ls -A {}/data", plan.dir)).trim().len() > 0, "数据目录应保留");

    // 卸载并删除数据
    vm.exec("卸载并删数据", &run_script(&uninstall_script(&plan.dir, NAME, true).unwrap()));
    assert_eq!(vm.sh(&format!("[ -e {} ] && echo yes || echo no", plan.dir)).trim(), "no", "目录应被删除");
    // 路径不在 Otter 目录下的卸载被拒绝
    assert!(uninstall_script("/etc", NAME, true).is_err() && uninstall_script("/opt/otter/docker/compose/../x", NAME, true).is_err());
}
per_distro!(redis_lifecycle, lifecycle);

fn wait_job(vm: &Vm) -> i32 {
    let t = std::time::Instant::now();
    loop {
        let p = crate::web::parse_job_poll(&vm.sh(&job_poll_script(0)));
        if let Some(st) = p.status {
            return st;
        }
        assert!(t.elapsed().as_secs() < 300, "任务超时");
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// 卸载脚本用 `set -e` 并且最后输出提示;直接当脚本跑
fn run_script(s: &str) -> String {
    s.to_string()
}
