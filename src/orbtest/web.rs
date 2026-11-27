//! 网站(OpenResty)的真机用例:安装长任务、读状态、静态站点和反向代理站点保存后真的能从宿主机访问、
//! 自签证书的 HTTPS、停用/启用、保存出错时事务回滚、卸载。端口用 18090/18443,不占 80/443。

use super::harness::{curl, Distro, Vm};
use crate::web::*;

const HTTP: u16 = 18090;
const HTTPS: u16 = 18443;

const RESET: &str = r#"
docker rm -f otter-openresty >/dev/null 2>&1
rm -rf /opt/otter /etc/cron.d/otter-websites
true
"#;

fn spec() -> InstallSpec {
    InstallSpec { http: HTTP, https: HTTPS, ..Default::default() }
}

fn listener(domain: &str, port: u16, cert: Option<&str>) -> Listener {
    Listener { domains: vec![domain.into()], port, cert: cert.map(String::from), force_https: false }
}

fn site(name: &str, kind: SiteKind, listeners: Vec<Listener>) -> SiteSpec {
    SiteSpec { name: name.into(), kind, listeners, ipv6: false, hsts: false, adv: Advanced::default() }
}

/// 带 Host 头访问(返回 "状态码\n正文");刚 reload 完连不上就重试几次
fn get(vm: &Vm, scheme: &str, port: u16, host: &str, path: &str) -> String {
    let mut r = String::new();
    for _ in 0..8 {
        r = vm.sh(&format!("rm -f /tmp/orbtest-body; curl -sk -m 5 -o /tmp/orbtest-body -w '%{{http_code}}' --resolve {host}:{port}:127.0.0.1 {scheme}://{host}:{port}{path}; echo; cat /tmp/orbtest-body 2>/dev/null"));
        if !r.starts_with("000") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    r
}

/// reload 是异步的(旧 worker 还会服务一小会儿):最多等 8 秒,直到访问结果满足条件
fn eventually(vm: &Vm, host: &str, want_backend: bool, why: &str) {
    for _ in 0..8 {
        if get(vm, "http", HTTP, host, "/").contains("Welcome to OpenResty") == want_backend {
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    panic!("{why}");
}

fn install(vm: &Vm) {
    let body = install_job_script(&spec());
    let o = vm.exec("启动安装任务", &job_start_script(&body));
    assert!(parse_op(&o).is_ok(), "{o}");
    let t = std::time::Instant::now();
    loop {
        let p = parse_job_poll(&vm.sh(&job_poll_script(0)));
        if let Some(st) = p.status {
            assert_eq!(st, 0, "安装失败:\n{}", p.log);
            assert!(p.log.contains("OpenResty 已启动"), "{}", p.log);
            break;
        }
        assert!(t.elapsed().as_secs() < 240, "安装超时:\n{}", p.log);
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    vm.exec("清任务", &job_clear_script());
}

fn web(d: Distro) {
    let vm = Vm::shared(d, "otter", RESET);
    let t = spec().target();

    // 安装前:状态是未安装
    let st = parse_state(&vm.sh(&state_script()));
    assert!(!st.installed() && st.docker == "ok", "{:?}", st.docker);

    install(&vm);
    let st = parse_state(&vm.sh(&state_script()));
    assert!(st.running() && st.layout.is_some(), "安装后状态不对:{:?} {:?}", st.container, st.layout);
    assert_eq!(st.target.ports, (HTTP, HTTPS));
    assert!(st.default_conf.is_some() && st.nginx_conf.is_some());
    // 默认站点:没有匹配的域名时返回 404/444,而不是别的站点的内容
    let dflt = vm.sh(&format!("curl -s -m 5 -o /dev/null -w '%{{http_code}}' http://127.0.0.1:{HTTP}/"));
    assert!(matches!(dflt.trim(), "404" | "000" | "403"), "默认站点响应 {dflt}");

    // 静态站点:保存 → 宿主机真的访问到
    let s1 = site("a.test", SiteKind::Static { spa: false, index: false }, vec![listener("a.test", HTTP, None)]);
    let o = vm.exec("保存静态站点", &site_save_script(&s1, true, &t).unwrap());
    assert!(parse_op(&o).is_ok(), "{o}");
    let r = get(&vm, "http", HTTP, "a.test", "/");
    assert!(r.starts_with("200") && r.contains("<h1>a.test</h1>"), "{r}");
    let ip = vm.ip();
    let c = curl(&ip, HTTP);
    assert!(matches!(c.as_str(), "000" | "404" | "403"), "从宿主机直接按 IP 访问应落到默认站点(拒绝),实际 {c}");

    // 反向代理站点:后端是另一个容器
    vm.exec("起后端", "docker rm -f orbtest-backend >/dev/null 2>&1; docker run -d --name orbtest-backend -p 127.0.0.1:18095:80 openresty/openresty:alpine >/dev/null; sleep 2");
    let s2 = site("b.test", SiteKind::Proxy { locations: vec![Location::new("/", Backend::Url("http://127.0.0.1:18095".into()))], upstreams: vec![] }, vec![listener("b.test", HTTP, None)]);
    let o = vm.exec("保存代理站点", &site_save_script(&s2, true, &t).unwrap());
    assert!(parse_op(&o).is_ok(), "{o}");
    let r = get(&vm, "http", HTTP, "b.test", "/");
    assert!(r.starts_with("200") && r.contains("Welcome to OpenResty"), "代理没通:{r}");

    // 读回:两个站点都在,表单数据能还原
    let st = parse_state(&vm.sh(&state_script()));
    for (n, want) in [("a.test", &s1), ("b.test", &s2)] {
        let e = st.sites.iter().find(|e| e.file == format!("{n}.conf")).unwrap_or_else(|| panic!("读不到 {n}"));
        let raw = vm.sh(&format!("cat /opt/otter/websites/conf/conf.d/{n}.conf"));
        println!("[{n}] 直接 cat 读回 modified={:?};状态里 modified={}", parse_site(&raw).map(|p| p.1), e.modified);
        assert!(e.enabled && !e.modified, "{n}: enabled={} modified={} lost={:?}\n{}", e.enabled, e.modified, e.lost, e.text);
        assert_eq!(e.spec.as_ref().unwrap().kind, want.kind, "{n} 读回的类型不一致");
    }

    // 自签证书 + HTTPS
    let o = vm.exec("自签证书", &cert_self_script("a.test", &["a.test".to_string()], 30).unwrap());
    assert!(parse_op(&o).is_ok(), "{o}");
    let st = parse_state(&vm.sh(&state_script()));
    let c = st.certs.iter().find(|c| c.name == "a.test").expect("没读到证书");
    assert!(c.covers("a.test") && c.days_left(now()).unwrap_or(0) >= 29, "{c:?}");
    let s1s = site("a.test", SiteKind::Static { spa: false, index: false }, vec![listener("a.test", HTTP, None), listener("a.test", HTTPS, Some("a.test"))]);
    let o = vm.exec("保存 HTTPS", &site_save_script(&s1s, true, &t).unwrap());
    assert!(parse_op(&o).is_ok(), "{o}");
    let r = get(&vm, "https", HTTPS, "a.test", "/");
    assert!(r.starts_with("200") && r.contains("a.test"), "HTTPS 不通:{r}");

    // 停用 → 访问不到;启用 → 恢复
    let o = vm.exec("停用", &site_toggle_script("b.test.conf", false).unwrap());
    assert!(parse_op(&o).is_ok(), "{o}");
    eventually(&vm, "b.test", false, "停用后不该再到后端");
    let o = vm.exec("启用", &site_toggle_script("b.test.conf.off", true).unwrap());
    assert!(parse_op(&o).is_ok(), "{o}");
    eventually(&vm, "b.test", true, "启用后应恢复到后端");

    // 事务:写进坏配置时 nginx -t 失败,整个回滚,原站点还能访问
    let (c, o) = vm.run(&site_raw_script("a.test.conf", "server { this is not valid }\n").unwrap());
    assert_eq!(c, 0);
    assert!(parse_op(&o).is_err(), "坏配置居然成功了:{o}");
    assert!(get(&vm, "http", HTTP, "a.test", "/").starts_with("200"), "回滚后站点应仍可访问");

    // 删除站点
    let o = vm.exec("删站点", &site_delete_script("b.test.conf").unwrap());
    assert!(parse_op(&o).is_ok(), "{o}");
    eventually(&vm, "b.test", false, "删除后不该再到后端");

    // 续期 cron
    vm.exec("装续期", &cron_script(9, 4));
    assert_eq!(parse_state(&vm.sh(&state_script())).cron.as_deref(), Some("9 4 * * *"));
    let o = vm.exec("立即续期", &renew_now_script());
    assert!(parse_op(&o).is_ok(), "{o}");

    // 控制:重启、卸载(配置和站点保留)
    for a in ["reload", "restart"] {
        let o = vm.exec(a, &ctl_script(a, &t).unwrap());
        assert!(parse_op(&o).is_ok(), "{o}");
    }
    let o = vm.exec("卸载", &ctl_script("remove", &t).unwrap());
    assert!(parse_op(&o).is_ok(), "{o}");
    let st = parse_state(&vm.sh(&state_script()));
    assert!(!st.running() && st.sites.iter().any(|e| e.file == "a.test.conf"), "卸载后站点配置应保留");
    vm.exec("清理后端", "docker rm -f orbtest-backend >/dev/null");
}
per_distro!(lifecycle, web);

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}
