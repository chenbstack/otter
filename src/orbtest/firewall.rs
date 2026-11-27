//! 防火墙的真机用例:三种前端(ufw / firewalld / iptables / nftables)的增删和互切、定时回滚、容器端口防护(含从宿主机真实访问)。
//! 同一发行版的用例共用一台虚拟机(`fw()` 取租约,排队用,拿到时先复位防火墙状态);不同发行版并行。

use super::harness::{curl, gate, gate_slow, host_ip_to, Distro, Lease, Vm};
use crate::firewall::*;

const SSH: [u16; 1] = [22];

/// 把防火墙相关状态恢复成"刚装好"的样子:各前端关掉、规则链和表删掉、容器删掉、回滚定时器取消
const RESET: &str = r#"
systemctl stop otter-fw-rollback.timer otter-fw-rollback.service 2>/dev/null
rm -f /run/otter-fw-rollback.sh
command -v ufw >/dev/null && { ufw --force disable; ufw --force reset; } >/dev/null 2>&1
systemctl disable --now firewalld >/dev/null 2>&1
for _i in 1 2 3; do iptables -w -D INPUT -j OTTER-INPUT 2>/dev/null; ip6tables -w -D INPUT -j OTTER-INPUT 2>/dev/null; done
for t in iptables ip6tables; do $t -w -F OTTER-INPUT 2>/dev/null; $t -w -X OTTER-INPUT 2>/dev/null; done
for _i in 1 2 3; do iptables -w -D DOCKER-USER -m comment --comment otter:if -j OTTER-DOCKER 2>/dev/null; done
iptables -w -F OTTER-DOCKER 2>/dev/null; iptables -w -X OTTER-DOCKER 2>/dev/null
nft delete table inet otter 2>/dev/null; nft delete table inet otter_docker 2>/dev/null
rm -rf /opt/otter/firewall
docker rm -f otter-orbtest-ct >/dev/null 2>&1
true
"#;

fn fw(d: Distro) -> Lease {
    Vm::shared(d, "firewall", RESET)
}

struct F<'a> {
    vm: &'a Vm,
}

impl F<'_> {
    fn info(&self) -> FwInfo {
        parse(&self.vm.run(READ_SCRIPT).1)
    }
    /// 像 App 一样带进度包装地执行,要求成功
    fn exec(&self, label: &str, script: &str) -> String {
        self.vm.exec(label, &with_progress(label, script))
    }
    fn show(&self, tag: &str) -> FwInfo {
        let i = self.info();
        println!("[{tag}] manage={:?} ufw={} firewalld={} ipt_live={} nft_live={} rules={:?}", i.manage(), i.ufw_active, i.firewalld_active, i.ipt_live, i.nft_live, i.rules.iter().map(|r| format!("{}|{:?}|{}", r.to, r.action, r.from)).collect::<Vec<_>>());
        i
    }
    /// 切换前端并取消回滚(模拟 App 连上了),返回切换后的状态
    fn switch(&self, from: Front, to: Front) -> FwInfo {
        let s = switch_script(Some(from), to, &self.info().rules, &SSH).unwrap();
        self.exec(&format!("切换 {from:?} → {to:?}"), &s);
        self.vm.run(&guard_cancel());
        self.show(&format!("切到 {to:?} 后"))
    }
}

fn nr(a: Action, port: &str, p: Proto, from: &str) -> NewRule {
    NewRule { action: a, port: port.into(), proto: p, from: from.into() }
}

// ---- iptables / nftables 前端(Ubuntu、Debian、Rocky 都有) ----

fn iptables_front(vm: &Vm) {
    let f = F { vm };
    let i = f.info();
    f.exec("iptables 加规则", &ipt_add_many(&[nr(Action::Allow, "80", Proto::Tcp, ""), nr(Action::Deny, "3306", Proto::Tcp, "1.2.3.4"), nr(Action::Allow, "8000-8100", Proto::Udp, "")], &i.rules).unwrap());
    let i = f.show("加规则后");
    assert!(i.ipt_live && i.rules.len() >= 3);
    let out = vm.sh("iptables -S OTTER-INPUT");
    assert!(out.contains("--dport 80 -j ACCEPT") && out.contains("-s 1.2.3.4/32") && out.contains("--dport 8000:8100"), "{out}");
    let r = i.rules.iter().find(|r| r.to == "80/tcp").cloned().unwrap();
    f.exec("iptables 删规则", &ipt_delete(&r, &i.rules).unwrap());
    assert_eq!(f.show("删规则后").rules.len(), i.rules.len() - 1);
    f.exec("iptables 解绑", &ipt_disable());
    let i = f.show("解绑后");
    assert!(!i.ipt_live);
    assert!(!vm.sh("iptables -S INPUT").contains("OTTER-INPUT"));
}

fn nftables_front(vm: &Vm) {
    let f = F { vm };
    let i = f.info();
    f.exec("nftables 加规则", &nft_add_many(&[nr(Action::Allow, "443", Proto::Tcp, ""), nr(Action::Deny, "3306", Proto::Tcp, "1.2.3.4"), nr(Action::Allow, "9000", Proto::Tcp, "2001:db8::/32")], &i.rules).unwrap());
    let i = f.show("加规则后");
    assert!(i.nft_live && i.rules.len() == 3);
    let out = vm.sh("nft list table inet otter");
    assert!(out.contains("tcp dport 443 accept") && out.contains("ip saddr 1.2.3.4 drop") && out.contains("ip6 saddr 2001:db8::/32 accept"), "{out}");
    let r = i.rules.iter().find(|r| r.to == "443/tcp").cloned().unwrap();
    f.exec("nftables 删规则", &nft_delete(&r, &i.rules).unwrap());
    assert_eq!(f.show("删规则后").rules.len(), 2);
    f.exec("nftables 解绑", &nft_disable());
    assert!(!f.info().nft_live);
    assert!(!vm.sh("nft list tables").contains("inet otter"));
}

// ---- 前端互切 ----

/// 目标没装时拒绝,且不动现状
fn refuse_missing(f: &F, from: Front, missing: Front) {
    let s = switch_script(Some(from), missing, &f.info().rules, &SSH).unwrap();
    let (c, o) = f.vm.run(&with_progress("switch", &s));
    println!("== 切到没装的 {missing:?}: exit {c}\n{o}");
    assert_ne!(c, 0);
    assert_eq!(f.info().manage(), Some(from), "现状被动了");
}

/// 定时回滚:切换后不取消,等 `GUARD_SECS` 秒,应该恢复到切换前的前端
fn rollback(vm: &Vm, from: Front, to: Front) {
    let f = F { vm };
    let before = f.info();
    let s = switch_script(Some(from), to, &before.rules, &SSH).unwrap();
    f.exec("切换(不取消回滚)", &s);
    assert_eq!(f.show("切换后").manage(), Some(to));
    std::thread::sleep(std::time::Duration::from_secs(GUARD_SECS as u64 + 8));
    let after = f.show("等回滚后");
    assert_eq!(after.manage(), Some(from), "回滚没生效");
}

// ---- 容器端口防护 ----

const CONTAINER: &str = "otter-orbtest-ct";

fn start_container(vm: &Vm, ports: &[u16]) {
    let p: String = ports.iter().map(|p| format!("-p {p}:80 ")).collect();
    let o = vm.sh(&format!("docker rm -f {CONTAINER} >/dev/null 2>&1; docker run -d --name {CONTAINER} {p}openresty/openresty:alpine 2>&1 | tail -1"));
    assert!(o.trim().len() >= 12, "起不来容器:{o}");
    std::thread::sleep(std::time::Duration::from_secs(2));
}

/// 限制的设置、读回、两种后端互换、取消;不看流量,只看规则
fn docker_ports(vm: &Vm) {
    let f = F { vm };
    start_container(vm, &[18080, 18081]);
    let i = f.info();
    assert!(i.du_ok() && !i.docker_nft && !i.du_nft, "du_ok={} docker_nft={} du_nft={}", i.du_ok(), i.docker_nft, i.du_nft);
    f.exec("禁止外部 18080", &du_set("tcp", 18080, Some(&PortMode::Deny), &i.du, &SSH, false).unwrap());
    let i = f.info();
    f.exec("18081 只允许来源", &du_set("tcp", 18081, Some(&PortMode::Only(vec!["10.1.2.3".into(), "192.168.0.0/16".into()])), &i.du, &SSH, false).unwrap());
    let i = f.info();
    assert!(i.du.len() == 2 && i.du.iter().all(|l| l.live), "{:?}", i.du);
    let chain = vm.sh("iptables -S OTTER-DOCKER");
    assert!(chain.contains("-i docker0") && chain.contains("-i br-+") && chain.contains("--ctorigdstport 18080") && chain.contains("-s 10.1.2.3/32"), "{chain}");
    assert!(vm.sh("iptables -S DOCKER-USER").contains("-j OTTER-DOCKER"));
    assert!(vm.sh("systemctl is-enabled otter-docker-user.service").contains("enabled"));
    // 换到 nftables 后端:限制搬过去,iptables 那边清干净
    f.exec("后端换 nftables", &du_backend(true, &i.du).unwrap());
    let i = f.info();
    assert!(i.du_nft && i.du.len() == 2 && i.du.iter().all(|l| l.live), "{:?}", i.du);
    let t = vm.sh("nft list table inet otter_docker");
    assert!(t.contains("ct original proto-dst 18080") && t.contains("ip saddr 10.1.2.3 return comment \"otter:dp\""), "{t}");
    assert!(!vm.sh("iptables -S DOCKER-USER").contains("OTTER-DOCKER"), "iptables 那边没清干净");
    f.exec("nftables 后端取消 18080", &du_set("tcp", 18080, None, &i.du, &SSH, true).unwrap());
    let i = f.info();
    assert_eq!(i.du.len(), 1);
    // 换回 iptables
    f.exec("后端换回 iptables", &du_backend(false, &i.du).unwrap());
    let i = f.info();
    assert!(!i.du_nft && i.du.len() == 1 && i.du[0].live);
    assert!(!vm.sh("nft list tables").contains("otter_docker"), "nft 那边没清干净");
    f.exec("取消 18081", &du_set("tcp", 18081, None, &i.du, &SSH, false).unwrap());
    assert!(f.info().du.is_empty());
    assert!(!vm.sh("iptables -S").contains("OTTER-DOCKER"));
    vm.sh(&format!("docker rm -f {CONTAINER}"));
}

/// 从宿主机真实访问容器端口,验证限制确实生效(两种后端)
fn docker_traffic(vm: &Vm) {
    let f = F { vm };
    let ip = vm.ip();
    let me = host_ip_to(&ip);
    assert!(!me.is_empty(), "取不到宿主机地址");
    start_container(vm, &[18080, 18081, 18082]);
    assert_eq!((curl(&ip, 18080).as_str(), curl(&ip, 18081).as_str(), curl(&ip, 18082).as_str()), ("200", "200", "200"), "基线不通");
    let mut cur = f.info().du;
    for (port, mode) in [(18080u16, PortMode::Deny), (18081, PortMode::Only(vec![me.clone()])), (18082, PortMode::Only(vec!["1.2.3.4".into()]))] {
        f.exec("设置限制", &du_set("tcp", port, Some(&mode), &cur, &SSH, false).unwrap());
        cur = f.info().du;
    }
    let got = |_: &str| (curl(&ip, 18080), curl(&ip, 18081), curl(&ip, 18082));
    assert_eq!(got("iptables"), ("000".into(), "200".into(), "000".into()), "iptables 后端:禁止 / 只允许本机 / 只允许别人");
    // 本机访问不受影响:被禁止的 18080,在服务器上用它自己的 IP 和 127.0.0.1 访问仍然通
    let local = |what: &str| {
        for h in [ip.as_str(), "127.0.0.1"] {
            let c = vm.sh(&format!("curl -s -m 5 -o /dev/null -w '%{{http_code}}' http://{h}:18080/"));
            assert_eq!(c.trim(), "200", "{what}:本机用 {h} 访问被禁止的端口应该通");
        }
    };
    local("iptables 后端");
    f.exec("后端换 nftables", &du_backend(true, &cur).unwrap());
    local("nftables 后端");
    assert_eq!(got("nft"), ("000".into(), "200".into(), "000".into()), "nftables 后端");
    f.exec("后端换回 iptables", &du_backend(false, &cur).unwrap());
    for l in f.info().du {
        f.exec("取消", &du_set(&l.proto, l.port, None, &f.info().du, &SSH, false).unwrap());
    }
    assert_eq!(got("cleared"), ("200".into(), "200".into(), "200".into()), "取消后应该全通");
    vm.sh(&format!("docker rm -f {CONTAINER}"));
}

// ---- Ubuntu:ufw / iptables / nftables ----

fn ufw_iptables_rules(d: Distro) {
    iptables_front(&fw(d));
}

fn ufw_nftables_rules(d: Distro) {
    nftables_front(&fw(d));
}

fn ufw_ufw_rules(d: Distro) {
    let vm = fw(d);
    let f = F { vm: &vm };
    f.exec("ufw 加规则", &ufw_add(&nr(Action::Allow, "8080", Proto::Tcp, "")).unwrap());
    f.exec("ufw 启用", &ufw_enable(&SSH).unwrap());
    let i = f.show("启用后");
    assert!(i.ufw_active && i.manage() == Some(Front::Ufw));
    assert!(i.rules.iter().any(|r| r.to == "22/tcp") && i.rules.iter().any(|r| r.to == "8080/tcp"));
    f.exec("ufw 加带来源的规则", &ufw_add(&nr(Action::Allow, "7000", Proto::Both, "10.0.0.0/8")).unwrap());
    let i = f.info();
    let r = i.rules.iter().find(|r| r.to.contains("7000") && !r.v6).cloned().unwrap();
    let RuleKind::Ufw(n) = r.kind else { panic!() };
    // 回归:ufw 输出行尾有空格,删除前的核对曾经一直失败
    f.exec("ufw 删规则", &ufw_delete(n, &r.key).unwrap());
    assert!(!f.info().rules.iter().any(|r| r.to.contains("7000")));
    f.exec("ufw 停用", &ufw_disable());
    assert!(!f.info().ufw_active);
}

fn ufw_switch(d: Distro) {
    let vm = fw(d);
    let f = F { vm: &vm };
    f.exec("ufw 加规则", &ufw_add(&nr(Action::Allow, "8080", Proto::Tcp, "")).unwrap());
    f.exec("ufw 启用", &ufw_enable(&SSH).unwrap());
    vm.run(&guard_cancel());
    assert_eq!(f.info().manage(), Some(Front::Ufw));
    let n = f.switch(Front::Ufw, Front::Iptables);
    assert!(n.manage() == Some(Front::Iptables) && !n.ufw_active && n.rules.iter().any(|r| r.to == "8080/tcp") && n.rules.iter().any(|r| r.to == "22/tcp"));
    let n = f.switch(Front::Iptables, Front::Nftables);
    assert!(n.manage() == Some(Front::Nftables) && !n.ipt_live && n.rules.iter().any(|r| r.to == "8080/tcp"));
    let n = f.switch(Front::Nftables, Front::Ufw);
    assert!(n.manage() == Some(Front::Ufw) && n.ufw_active && !n.nft_live);
    refuse_missing(&f, Front::Ufw, Front::Firewalld);
}

fn ufw_docker_ports(d: Distro) {
    docker_ports(&fw(d));
}

fn ufw_docker_traffic(d: Distro) {
    docker_traffic(&fw(d));
}

/// 慢:等 120 秒的回滚
fn ufw_rollback(d: Distro) {
    let vm = fw(d);
    let f = F { vm: &vm };
    f.exec("ufw 启用", &ufw_enable(&SSH).unwrap());
    vm.run(&guard_cancel());
    rollback(&vm, Front::Ufw, Front::Iptables);
    assert!(f.info().ufw_active);
}

// ---- Rocky:firewalld(systemd 252) ----

fn fwd_firewalld_rules(d: Distro) {
    let vm = fw(d);
    let f = F { vm: &vm };
    let i = f.info();
    assert!(i.firewalld_installed && !i.firewalld_active && i.manage() == Some(Front::Firewalld));
    f.exec("firewalld 启用", &fwd_enable(&SSH).unwrap());
    let i = f.show("启用后");
    assert!(i.firewalld_active && i.rules.iter().any(|r| r.to == "22/tcp"));
    let z = i.default_zone.clone();
    assert!(!z.is_empty(), "没有默认区域");
    for n in [nr(Action::Allow, "80", Proto::Tcp, ""), nr(Action::Allow, "8000-8100", Proto::Udp, ""), nr(Action::Deny, "3306", Proto::Tcp, "1.2.3.4"), nr(Action::Allow, "9000", Proto::Both, "10.0.0.0/8"), nr(Action::Reject, "5432", Proto::Tcp, "")] {
        f.exec(&format!("加 {}", n.port), &fwd_add(&z, &n).unwrap());
    }
    let i = f.show("加规则后");
    let all = vm.sh("firewall-cmd --permanent --list-all");
    assert!(all.contains("80/tcp") && all.contains("8000-8100/udp") && all.contains("source address=\"1.2.3.4\" port port=\"3306\" protocol=\"tcp\" drop") && all.contains("port port=\"5432\" protocol=\"tcp\" reject"), "{all}");
    // Both + 来源会拆成 tcp 和 udp 两条
    assert_eq!(i.rules.iter().filter(|r| r.to.starts_with("9000/")).count(), 2);
    for key in ["80/tcp", "8000-8100/udp", "3306/tcp", "9000/tcp", "9000/udp", "5432/tcp"] {
        let r = f.info().rules.iter().find(|r| r.to == key).cloned().unwrap_or_else(|| panic!("找不到 {key}"));
        f.exec(&format!("删 {key}"), &fwd_delete(&r).unwrap());
    }
    let left: Vec<String> = f.info().rules.iter().map(|r| r.to.clone()).collect();
    assert!(left.iter().all(|t| !t.starts_with("80/") && !t.starts_with("9000/") && !t.starts_with("5432/")), "{left:?}");
    f.exec("firewalld 停用", &fwd_disable());
    assert!(!f.info().firewalld_active);
}

fn fwd_switch(d: Distro) {
    let vm = fw(d);
    let f = F { vm: &vm };
    f.exec("firewalld 启用", &fwd_enable(&SSH).unwrap());
    vm.run(&guard_cancel());
    let z = f.info().default_zone;
    f.exec("加 443", &fwd_add(&z, &nr(Action::Allow, "443", Proto::Tcp, "")).unwrap());
    let n = f.switch(Front::Firewalld, Front::Iptables);
    assert!(n.manage() == Some(Front::Iptables) && !n.firewalld_active && n.rules.iter().any(|r| r.to == "443/tcp"));
    let n = f.switch(Front::Iptables, Front::Nftables);
    assert!(n.manage() == Some(Front::Nftables) && !n.ipt_live);
    let n = f.switch(Front::Nftables, Front::Firewalld);
    assert!(n.manage() == Some(Front::Firewalld) && n.firewalld_active && !n.nft_live && n.rules.iter().any(|r| r.to == "443/tcp"));
    refuse_missing(&f, Front::Firewalld, Front::Ufw);
}

fn fwd_iptables_nftables_rules(d: Distro) {
    let vm = fw(d);
    iptables_front(&vm);
    nftables_front(&vm);
}

fn fwd_docker_ports(d: Distro) {
    docker_ports(&fw(d));
}

fn fwd_docker_traffic(d: Distro) {
    docker_traffic(&fw(d));
}

/// 慢:systemd 252 下的回滚(命令行带分号的临时单元会被重载杀掉,这个用例专门防它)
fn fwd_rollback(d: Distro) {
    let vm = fw(d);
    let f = F { vm: &vm };
    f.exec("firewalld 启用", &fwd_enable(&SSH).unwrap());
    vm.run(&guard_cancel());
    rollback(&vm, Front::Firewalld, Front::Iptables);
    assert!(f.info().firewalld_active && !f.info().ipt_live);
}

// ---- Debian:没有 ufw / firewalld,默认走 iptables / nftables ----

fn plain_iptables_nftables(d: Distro) {
    let vm = fw(d);
    let i = F { vm: &vm }.info();
    assert!(!i.ufw_installed && !i.firewalld_installed && i.iptables && i.nft, "debian 上应只有 iptables / nft");
    iptables_front(&vm);
    nftables_front(&vm);
}

fn plain_switch(d: Distro) {
    let vm = fw(d);
    let f = F { vm: &vm };
    f.exec("iptables 加规则", &ipt_add_many(&[nr(Action::Allow, "22", Proto::Tcp, ""), nr(Action::Allow, "8080", Proto::Tcp, "")], &[]).unwrap());
    f.exec("iptables 绑定", &ipt_enable(&f.info().rules));
    assert_eq!(f.info().manage(), Some(Front::Iptables));
    let n = f.switch(Front::Iptables, Front::Nftables);
    assert!(n.manage() == Some(Front::Nftables) && !n.ipt_live && n.rules.iter().any(|r| r.to == "8080/tcp"));
    let n = f.switch(Front::Nftables, Front::Iptables);
    assert!(n.manage() == Some(Front::Iptables) && !n.nft_live);
    refuse_missing(&f, Front::Iptables, Front::Ufw);
}

fn plain_docker_ports(d: Distro) {
    docker_ports(&fw(d));
}

// ---- 注册:同一族的用例在这一族的所有发行版上各跑一遍 ----

distros!(ufw_iptables_rules, ufw_iptables_rules, gate, UFW);
distros!(ufw_nftables_rules, ufw_nftables_rules, gate, UFW);
distros!(ufw_ufw_rules, ufw_ufw_rules, gate, UFW);
distros!(ufw_switch, ufw_switch, gate, UFW);
distros!(ufw_docker_ports, ufw_docker_ports, gate, UFW);
distros!(ufw_docker_traffic, ufw_docker_traffic, gate, UFW);
distros!(ufw_rollback, ufw_rollback, gate_slow, UFW);
distros!(fwd_firewalld_rules, fwd_firewalld_rules, gate, FWD);
distros!(fwd_switch, fwd_switch, gate, FWD);
distros!(fwd_iptables_nftables_rules, fwd_iptables_nftables_rules, gate, FWD);
distros!(fwd_docker_ports, fwd_docker_ports, gate, FWD);
distros!(fwd_docker_traffic, fwd_docker_traffic, gate, FWD);
distros!(fwd_rollback, fwd_rollback, gate_slow, FWD);
distros!(plain_iptables_nftables, plain_iptables_nftables, gate, PLAIN);
distros!(plain_switch, plain_switch, gate, PLAIN);
distros!(plain_docker_ports, plain_docker_ports, gate, PLAIN);
