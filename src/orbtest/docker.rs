//! 容器页的真机用例:用 App 真正用的 `Action::args()` 和列表解析,在真实 Docker 上跑
//! 启停重启删除、拉取/删除镜像、存储卷、清理、compose 堆栈部署和删除。
//! 这个域单独一台机器:`prune -a` 会清掉所有没用的镜像和卷,不能和别的用例混用。

use super::harness::{Distro, Vm};
use crate::docker::*;

const RESET: &str = r#"
docker ps -aq --filter label=orbtest=1 | xargs -r docker rm -f >/dev/null 2>&1
docker compose -p orbtest-stack down --remove-orphans >/dev/null 2>&1
docker volume ls -q --filter label=orbtest=1 | xargs -r docker volume rm -f >/dev/null 2>&1
docker rmi -f busybox:1.36 >/dev/null 2>&1
rm -rf /tmp/orbtest-stack
true
"#;

/// 像 App 那样执行一个动作,返回 (退出码, 输出)
fn act(vm: &Vm, a: &Action) -> (i32, String) {
    let args = a.args().iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(" ");
    vm.run(&format!("docker {args} 2>&1"))
}

fn ok(vm: &Vm, a: &Action) -> String {
    let (c, o) = act(vm, a);
    assert_eq!(c, 0, "{} 失败:{o}", a.verb());
    o
}

fn containers(vm: &Vm) -> Vec<Container> {
    parse_ps(&vm.sh(&format!("docker ps -a --format '{PS_FORMAT}'")))
}

fn find(vm: &Vm, name: &str) -> Option<Container> {
    containers(vm).into_iter().find(|c| c.name == name)
}

fn docker(d: Distro) {
    let vm = Vm::shared(d, "docker", RESET);
    vm.pull("openresty/openresty:alpine");

    // 起一个带端口、卷、标签的容器,列表解析得出来
    vm.exec(
        "起容器",
        "docker volume create --label orbtest=1 orbtest-vol >/dev/null; docker run -d --name orbtest-a --label orbtest=1 -p 18081:80 -v orbtest-vol:/data openresty/openresty:alpine >/dev/null; sleep 1",
    );
    let c = find(&vm, "orbtest-a").expect("列表里没有容器");
    assert!(c.running() && c.image == "openresty/openresty:alpine", "{c:?}");
    assert!(c.ports.iter().any(|p| p == "18081->80/tcp"), "端口:{:?}", c.ports);
    assert!(c.mounts.iter().any(|m| m == "orbtest-vol"), "挂载:{:?}", c.mounts);

    // 状态数据:CPU/内存解析
    let st = parse_stats(&vm.sh(&format!("docker stats --no-stream --format '{STATS_FORMAT}'")));
    assert!(st.keys().any(|k| c.id.starts_with(k.as_str()) || k.starts_with(&c.id)), "stats 里没有这个容器:{st:?}");

    // 停止 → 启动 → 重启 → 删除
    ok(&vm, &Action::Stop("orbtest-a".into()));
    assert!(!find(&vm, "orbtest-a").unwrap().running());
    ok(&vm, &Action::Start("orbtest-a".into()));
    assert!(find(&vm, "orbtest-a").unwrap().running());
    ok(&vm, &Action::Restart("orbtest-a".into()));
    assert!(find(&vm, "orbtest-a").unwrap().running());
    // 删不存在的容器报错(App 会提示失败)
    let (code, _) = act(&vm, &Action::Start("orbtest-nope".into()));
    assert_ne!(code, 0);
    ok(&vm, &Action::Remove("orbtest-a".into()));
    assert!(find(&vm, "orbtest-a").is_none());

    // 存储卷:列表、用量、删除
    let vols = parse_volumes(&vm.sh(&format!("docker volume ls --format '{VOLUMES_FORMAT}'")));
    assert!(vols.iter().any(|v| v.name == "orbtest-vol" && v.driver == "local"), "{vols:?}");
    let df = parse_df_volumes(&vm.sh("docker system df -v 2>/dev/null"));
    assert!(df.contains_key("orbtest-vol"), "df 里没有卷:{df:?}");
    ok(&vm, &Action::RemoveVolume("orbtest-vol".into()));
    assert!(!parse_volumes(&vm.sh(&format!("docker volume ls --format '{VOLUMES_FORMAT}'"))).iter().any(|v| v.name == "orbtest-vol"));
    // 清理未使用的卷:匿名卷和没被容器用的命名卷都要清(确认框列的就是这些),在用的卷不动
    vm.exec("造匿名卷、命名卷、在用的卷", "docker volume create --label orbtest=1 orbtest-named >/dev/null; docker volume create --label orbtest=1 orbtest-used >/dev/null; docker run -d --name orbtest-keep --label orbtest=1 -v orbtest-used:/data openresty/openresty:alpine >/dev/null; docker run --name orbtest-anon --label orbtest=1 -v /data openresty/openresty:alpine true; docker rm orbtest-anon >/dev/null");
    ok(&vm, &Action::PruneVolumes);
    let after: Vec<String> = parse_volumes(&vm.sh(&format!("docker volume ls --format '{VOLUMES_FORMAT}'"))).into_iter().map(|v| v.name).collect();
    assert_eq!(after, vec!["orbtest-used".to_string()], "只该剩在用的卷");
    ok(&vm, &Action::Remove("orbtest-keep".into()));

    // 镜像:拉取 → 出现在列表 → 删除;被容器占用的不能删;清理未使用镜像
    ok(&vm, &Action::Pull("busybox:1.36".into()));
    let imgs = || parse_images(&vm.sh(&format!("docker images --format '{IMAGES_FORMAT}'")));
    assert!(imgs().iter().any(|i| i.repo == "busybox" && i.tag == "1.36"), "{:?}", imgs());
    vm.exec("占用镜像", "docker run -d --name orbtest-b --label orbtest=1 busybox:1.36 sleep 600 >/dev/null");
    let (code, _) = act(&vm, &Action::RemoveImage("busybox:1.36".into()));
    assert_ne!(code, 0, "有容器在用的镜像不该删掉");
    ok(&vm, &Action::Remove("orbtest-b".into()));
    ok(&vm, &Action::RemoveImage("busybox:1.36".into()));
    assert!(!imgs().iter().any(|i| i.repo == "busybox"));
    ok(&vm, &Action::Pull("busybox:1.36".into()));
    let o = ok(&vm, &Action::PruneImages);
    assert!(o.contains("reclaimed space"), "{o}");
    assert!(!imgs().iter().any(|i| i.repo == "busybox"), "未使用的镜像应被清掉");
    vm.pull("openresty/openresty:alpine");

    // compose 堆栈:写文件 → up → 按项目分组 → 重新部署 → down
    let dir = "/tmp/orbtest-stack";
    let text = "services:\n  web:\n    image: openresty/openresty:alpine\n    labels: [orbtest=1]\n    ports: ['18082:80']\n  cache:\n    image: openresty/openresty:alpine\n    labels: [orbtest=1]\n";
    vm.exec("写 compose", &format!("mkdir -p {dir} && cat > {dir}/docker-compose.yml <<'EOF'\n{text}EOF"));
    let up = Action::ComposeUp { project: "orbtest-stack".into(), dir: dir.into(), files: vec![format!("{dir}/docker-compose.yml")], envs: vec![], pull: false };
    ok(&vm, &up);
    let cs = containers(&vm);
    let mine: Vec<_> = cs.iter().filter(|c| c.project.as_deref() == Some("orbtest-stack")).collect();
    assert_eq!(mine.len(), 2, "{cs:?}");
    let snap = Snapshot { containers: cs.clone(), ..Default::default() };
    let stacks = to_stacks(&snap, &Default::default());
    let s = stacks.iter().find(|s| s.name == "orbtest-stack").expect("堆栈没分出来");
    assert_eq!(s.services.len(), 2);
    assert!(s.services.iter().all(|x| matches!(x.state, crate::Running::Up)));
    // 再次 up 幂等
    ok(&vm, &up);
    assert_eq!(containers(&vm).iter().filter(|c| c.project.as_deref() == Some("orbtest-stack")).count(), 2);
    ok(&vm, &Action::ComposeDown("orbtest-stack".into()));
    assert!(containers(&vm).iter().all(|c| c.project.as_deref() != Some("orbtest-stack")));
    // compose 文件保留
    assert!(vm.sh(&format!("test -f {dir}/docker-compose.yml && echo yes")).contains("yes"));

    // 名称校验:不安全的名字 App 不会执行
    assert!(!safe_name("a;rm -rf /") && !safe_name("-f") && safe_name("redis:7.4-alpine"));
}
per_distro!(containers, docker);
