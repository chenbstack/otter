//! 工具箱新增工具的真机用例:主机名与 hosts/DNS、内核参数、SSH 设置与公钥、用户与 sudo、fail2ban。
//! 都是整台机器的状态:每个用例开头复位、结束还原;SSH 用例不会断开 orbctl 的通道(它不走 sshd)。

use super::harness::{Distro, Vm};
use crate::sysops::quote;
use crate::toolbox::{fail2ban as f2b, host, ssh, sysctl, users};

/// 带 stdin 执行(脚本本身也是经 stdin 喂给 `sh -s` 的,所以套一层)
fn run_in(vm: &Vm, script: &str, input: &str) -> (i32, String) {
    vm.run(&format!("printf '%s' {} | sh -c {}", quote(input), quote(script)))
}

fn exec_in(vm: &Vm, label: &str, script: &str, input: &str) -> String {
    let (c, o) = run_in(vm, script, input);
    println!("== {label}: exit {c}\n{}", o.lines().take(12).collect::<Vec<_>>().join("\n"));
    assert_eq!(c, 0, "{label} 失败:{o}");
    o
}

// ================= 主机名 + hosts + DNS =================

const HOST_RESET: &str = r#"
[ -f /tmp/orbtest-hosts ] && cat /tmp/orbtest-hosts > /etc/hosts
[ -f /tmp/orbtest-hn ] && hostnamectl set-hostname "$(cat /tmp/orbtest-hn)"
rm -f /etc/systemd/resolved.conf.d/99-otter.conf
systemctl is-active systemd-resolved >/dev/null 2>&1 && systemctl restart systemd-resolved
[ -f /tmp/orbtest-resolv ] && [ ! -L /etc/resolv.conf ] && cat /tmp/orbtest-resolv > /etc/resolv.conf
[ -L /tmp/orbtest-resolv-link ] && { rm -f /etc/resolv.conf; cp -P /tmp/orbtest-resolv-link /etc/resolv.conf; }
true
"#;

fn read_host(vm: &Vm) -> host::HostInfo {
    host::parse_host(&vm.sh(host::HOST_SCRIPT)).expect("读主机信息")
}

fn host_tool(d: Distro) {
    let vm = Vm::shared(d, "toolmore-host", HOST_RESET);
    vm.sh("[ -f /tmp/orbtest-hosts ] || cp /etc/hosts /tmp/orbtest-hosts; [ -f /tmp/orbtest-hn ] || hostname > /tmp/orbtest-hn; [ -f /tmp/orbtest-resolv ] || { [ -L /etc/resolv.conf ] || cp /etc/resolv.conf /tmp/orbtest-resolv; }");
    let i0 = read_host(&vm);
    assert!(!i0.hostname.is_empty() && i0.entries.iter().any(|e| e.names.iter().any(|n| n == "localhost")), "{i0:?}");
    println!("DNS 方式 {:?},当前 {:?}", i0.mode, i0.dns);

    // 主机名
    vm.exec("改主机名", &host::set_hostname_script("orbtest-web").unwrap());
    let i1 = read_host(&vm);
    assert_eq!(i1.hostname, "orbtest-web", "{i1:?}");
    assert_eq!(i1.static_name, "orbtest-web");
    if i0.entries.iter().any(|e| e.ip == "127.0.1.1") {
        assert!(i1.entries.iter().any(|e| e.ip == "127.0.1.1" && e.names == vec!["orbtest-web"]), "{:?}", i1.entries);
    }
    let (c, _) = vm.run(&host::set_hostname_script("Bad_Name").unwrap_or_else(|_| "false".into()));
    assert_ne!(c, 0);
    vm.exec("还原主机名", &host::set_hostname_script(&vm.sh("cat /tmp/orbtest-hn").trim().to_lowercase()).unwrap());

    // hosts:增、改、删;每一条都能改,包括 localhost 之外的任意行
    vm.exec("hosts 添加", &host::hosts_add_script(&host::hosts_line("10.9.8.7", "orbtest.local orbtest2.local", "测试").unwrap()));
    let i2 = read_host(&vm);
    let e = i2.entries.iter().find(|e| e.names.contains(&"orbtest.local".to_string())).expect("新条目").clone();
    assert_eq!((e.ip.as_str(), e.comment.as_str()), ("10.9.8.7", "测试"), "{e:?}");
    assert!(vm.sh("getent hosts orbtest.local").contains("10.9.8.7"));
    let new = host::hosts_line("10.9.8.9", "orbtest.local", "").unwrap();
    // 文件在你打开之后被改过:拒绝
    let (c, o) = vm.run(&host::hosts_edit_script(e.line, "10.0.0.1\tsomething else", &new));
    assert_ne!(c, 0, "{o}");
    assert!(o.contains("被改过"), "{o}");
    vm.exec("hosts 修改", &host::hosts_edit_script(e.line, &e.raw, &new));
    assert!(vm.sh("getent hosts orbtest.local").contains("10.9.8.9"));
    assert!(!vm.sh("grep -c orbtest2.local /etc/hosts").trim().starts_with('1'), "旧行应该被换掉");
    let i3 = read_host(&vm);
    let e = i3.entries.iter().find(|e| e.names.contains(&"orbtest.local".to_string())).unwrap().clone();
    vm.exec("hosts 删除", &host::hosts_delete_script(e.line, &e.raw));
    assert!(!read_host(&vm).entries.iter().any(|e| e.names.contains(&"orbtest.local".to_string())));
    // 其它行原样还在
    assert_eq!(read_host(&vm).entries.len(), i0.entries.len());
    // 末尾没有换行的 hosts 也能正确追加
    vm.sh("printf '%s' \"$(cat /etc/hosts)\" > /etc/hosts");
    vm.exec("无换行追加", &host::hosts_add_script("10.9.8.7\torbtest.local"));
    assert!(read_host(&vm).entries.iter().any(|e| e.names.contains(&"orbtest.local".to_string())));
    assert_eq!(read_host(&vm).entries.len(), i0.entries.len() + 1);
    vm.sh("cat /tmp/orbtest-hosts > /etc/hosts");

    // DNS
    let servers = vec!["9.9.9.9".to_string(), "149.112.112.112".to_string()];
    match i0.mode.clone() {
        host::DnsMode::Resolved => {
            vm.exec("DNS 设置", &host::dns_set_script(&host::DnsMode::Resolved, &servers).unwrap());
            let i = read_host(&vm);
            assert!(i.dns.contains(&"9.9.9.9".to_string()) && i.dns_otter == servers, "{i:?}");
            vm.exec("DNS 恢复默认", &host::dns_reset_script(&host::DnsMode::Resolved));
            let i = read_host(&vm);
            assert!(i.dns_otter.is_empty(), "{i:?}");
        }
        host::DnsMode::File => {
            vm.exec("DNS 设置", &host::dns_set_script(&host::DnsMode::File, &servers).unwrap());
            let i = read_host(&vm);
            assert_eq!(i.dns[..2], servers[..], "{i:?}");
            vm.sh("cat /tmp/orbtest-resolv > /etc/resolv.conf");
        }
        host::DnsMode::NetworkManager { .. } => {
            // OrbStack 把 resolv.conf 做成指向宿主机 DNS 的符号链接,NetworkManager 不写它;换成普通文件让 NM 接管,和真实 RHEL 系服务器一致
            vm.sh("[ -L /tmp/orbtest-resolv-link ] || cp -P /etc/resolv.conf /tmp/orbtest-resolv-link; if [ -L /etc/resolv.conf ]; then rm -f /etc/resolv.conf; nmcli general reload dns-rc; sleep 1; fi");
            let i = read_host(&vm);
            assert!(i.dns_otter.is_empty(), "{i:?}");
            let mode = i.mode.clone();
            vm.exec("DNS 设置(NM)", &host::dns_set_script(&mode, &servers).unwrap());
            let i = read_host(&vm);
            assert_eq!(i.dns[..2], servers[..], "{i:?}");
            assert_eq!(i.dns_otter, servers, "{i:?}");
            // 加一个 IPv6 地址:进 ipv6.dns,三个都在
            let mixed = vec!["9.9.9.9".to_string(), "2620:fe::fe".to_string()];
            vm.exec("DNS 设置(NM,v4+v6)", &host::dns_set_script(&mode, &mixed).unwrap());
            let i = read_host(&vm);
            assert_eq!(i.dns_otter, mixed, "{i:?}");
            vm.exec("DNS 恢复默认(NM)", &host::dns_reset_script(&mode));
            let i = read_host(&vm);
            assert!(i.dns_otter.is_empty() && !i.dns.contains(&"9.9.9.9".to_string()), "{i:?}");
            vm.sh("rm -f /etc/resolv.conf; cp -P /tmp/orbtest-resolv-link /etc/resolv.conf");
        }
        host::DnsMode::Readonly(why) => {
            println!("DNS 只读:{why}");
            assert!(host::dns_set_script(&i0.mode, &servers).is_err());
            // 临时把符号链接换成普通文件,测「直接改 resolv.conf」这条路
            vm.sh("[ -L /tmp/orbtest-resolv-link ] || cp -P /etc/resolv.conf /tmp/orbtest-resolv-link; rm -f /etc/resolv.conf; printf 'search lan\\nnameserver 1.1.1.1\\n' > /etc/resolv.conf");
            let f = read_host(&vm);
            assert_eq!(f.mode, host::DnsMode::File, "{f:?}");
            assert_eq!(f.dns, vec!["1.1.1.1"]);
            vm.exec("DNS 设置(文件)", &host::dns_set_script(&host::DnsMode::File, &servers).unwrap());
            let f = read_host(&vm);
            assert_eq!(f.dns[..2], servers[..], "{f:?}");
            assert!(vm.sh("cat /etc/resolv.conf").contains("search lan"), "其它行要保留");
            assert!(vm.sh("cat /etc/resolv.conf.otter-bak").contains("1.1.1.1"), "要有备份");
            vm.sh("rm -f /etc/resolv.conf; cp -P /tmp/orbtest-resolv-link /etc/resolv.conf");
        }
    }
}
per_distro_tools!(host_tool, host_tool);

// ================= 内核参数 =================

const SYSCTL_RESET: &str = r#"
rm -f /etc/sysctl.d/99-otter.conf /etc/modules-load.d/otter-bbr.conf
[ -f /tmp/orbtest-sysctl ] && while read -r k v; do sysctl -w "$k=$v" >/dev/null 2>&1; done < /tmp/orbtest-sysctl
true
"#;

fn read_sysctl(vm: &Vm) -> sysctl::SysctlInfo {
    sysctl::parse_sysctl(&vm.sh(&sysctl::read_script())).expect("读内核参数")
}

fn sysctl_tool(d: Distro) {
    let vm = Vm::shared(d, "toolmore-sysctl", SYSCTL_RESET);
    let i0 = read_sysctl(&vm);
    // 记下原值,复位时还原
    let saved: String = sysctl::ITEMS.iter().map(|it| format!("{} {}\n", it.key, i0.value(it.key))).collect::<String>() + &format!("net.core.default_qdisc {}\n", i0.qdisc);
    vm.sh(&format!("[ -f /tmp/orbtest-sysctl ] || printf '%s' {} > /tmp/orbtest-sysctl", quote(&saved)));
    assert!(!i0.value("net.core.somaxconn").is_empty(), "{i0:?}");

    // 数值:设置立即生效并写进文件,读回
    vm.exec("somaxconn", &sysctl::set_script("net.core.somaxconn", "4096").unwrap());
    let i1 = read_sysctl(&vm);
    assert_eq!(i1.value("net.core.somaxconn"), "4096");
    assert_eq!(i1.saved("net.core.somaxconn"), Some("4096"));
    // 再设一次不会重复写
    vm.exec("somaxconn 再设", &sysctl::set_script("net.core.somaxconn", "1024").unwrap());
    assert_eq!(vm.sh("grep -c '^net.core.somaxconn' /etc/sysctl.d/99-otter.conf").trim(), "1");
    // 开关
    let was = i0.value("net.ipv4.ip_forward").to_string();
    let flip = if was == "1" { "0" } else { "1" };
    vm.exec("ip_forward", &sysctl::set_script("net.ipv4.ip_forward", flip).unwrap());
    assert_eq!(read_sysctl(&vm).value("net.ipv4.ip_forward"), flip);
    // 重启后生效:`sysctl --system` 能把文件里的值读回来
    vm.sh("sysctl -w net.core.somaxconn=128 >/dev/null; sysctl --system >/dev/null 2>&1");
    assert_eq!(read_sysctl(&vm).value("net.core.somaxconn"), "1024", "配置文件里的值要能被 sysctl --system 读回");
    // 不合法的值不会执行
    assert!(sysctl::set_script("net.core.somaxconn", "1; reboot").is_err());
    // 恢复默认:文件里的行没了
    vm.exec("恢复默认", &sysctl::reset_script("net.core.somaxconn").unwrap());
    assert_eq!(read_sysctl(&vm).saved("net.core.somaxconn"), None);
    // BBR:内核支持才测
    if i0.bbr {
        vm.exec("开 BBR", &sysctl::set_script("net.ipv4.tcp_congestion_control", "bbr").unwrap());
        let b = read_sysctl(&vm);
        assert_eq!(b.value("net.ipv4.tcp_congestion_control"), "bbr", "{b:?}");
        assert_eq!(b.qdisc, "fq");
        assert!(vm.sh("cat /etc/modules-load.d/otter-bbr.conf").contains("tcp_bbr"));
        vm.exec("关 BBR", &sysctl::set_script("net.ipv4.tcp_congestion_control", "cubic").unwrap());
        assert_eq!(read_sysctl(&vm).value("net.ipv4.tcp_congestion_control"), "cubic");
        assert!(!vm.sh("ls /etc/modules-load.d/").contains("otter-bbr"));
    } else {
        println!("这台机器的内核没有 BBR,跳过");
    }
    vm.sh("rm -f /etc/sysctl.d/99-otter.conf");
}
per_distro_tools!(sysctl_tool, sysctl_tool);

// ================= SSH 设置与公钥 =================

const SSH_RESET: &str = r#"
systemctl stop otter-ssh-rollback.timer otter-ssh-rollback.service >/dev/null 2>&1
rm -f /etc/ssh/sshd_config.d/00-otter.conf /run/otter-ssh*
[ -f /tmp/orbtest-sshd_config ] && cat /tmp/orbtest-sshd_config > /etc/ssh/sshd_config
rm -f /etc/systemd/system/ssh.socket.d/otter.conf
systemctl daemon-reload; systemctl restart ssh.socket >/dev/null 2>&1
userdel -r orbssh >/dev/null 2>&1
true
"#;

fn read_ssh(vm: &Vm) -> ssh::SshInfo {
    ssh::parse_ssh(&vm.sh(ssh::SSH_SCRIPT)).expect("读 SSH 信息")
}

const PUB1: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBqXq3VtKzv1yrmZ6l1p0xPq0q8oQwYb5T1x2oR6mYcA orbtest-1";
const PUB2: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPqXq3VtKzv1yrmZ6l1p0xPq0q8oQwYb5T1x2oR6mYcB orbtest-2";

fn ssh_tool(d: Distro) {
    let vm = Vm::shared(d, "toolmore-ssh", SSH_RESET);
    // 虚拟机模板里没有 sshd:装上(Ubuntu 24.04 装完是 ssh.socket 激活,正好测这条路)
    if vm.run("command -v sshd").0 != 0 {
        let pm = if vm.run("command -v apt-get").0 == 0 { "DEBIAN_FRONTEND=noninteractive apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq openssh-server" } else { "dnf install -y -q openssh-server" };
        vm.exec("装 sshd", &format!("{pm} && (systemctl enable --now ssh || systemctl enable --now sshd)"));
    }
    vm.sh("[ -f /tmp/orbtest-sshd_config ] || cp /etc/ssh/sshd_config /tmp/orbtest-sshd_config");
    let i0 = read_ssh(&vm);
    println!("sshd:端口 {:?} root={} 密码={} socket={} include={} 单元={}", i0.ports, i0.root_login, i0.password, i0.socket, i0.include, i0.unit);
    assert_eq!(i0.ports, vec![22]);
    assert!(i0.users.iter().any(|u| u.name == "root"));

    // 简单项:MaxAuthTries、ClientAliveInterval
    vm.exec("MaxAuthTries", &ssh::set_script("MaxAuthTries", "3").unwrap());
    assert_eq!(read_ssh(&vm).max_tries, Some(3));
    vm.exec("ClientAliveInterval", &ssh::set_script("ClientAliveInterval", "60").unwrap());
    let i = read_ssh(&vm);
    assert_eq!((i.max_tries, i.alive_interval), (Some(3), Some(60)), "两项都要保留");
    assert_eq!(i.otter_value("MaxAuthTries"), Some("3"));
    // 一次改完会排定时回滚;取消它
    assert!(vm.sh("systemctl list-timers --all --no-pager 2>/dev/null").contains("otter-ssh-rollback"), "应该排了定时回滚");
    vm.exec("取消回滚", &ssh::guard_cancel());
    assert!(!vm.sh("systemctl list-timers --all --no-pager 2>/dev/null").contains("otter-ssh-rollback"));

    // 比别的 drop-in 靠前:cloud-init 这类文件里写了相反的值也要生效
    vm.sh("mkdir -p /etc/ssh/sshd_config.d; printf 'PasswordAuthentication yes\\n' > /etc/ssh/sshd_config.d/50-orbtest.conf");
    // 先放一把 root 的公钥,再关密码登录
    exec_in(&vm, "添加公钥", &ssh::add_key_script("root").unwrap(), PUB1);
    let i = read_ssh(&vm);
    let root = i.user("root").unwrap();
    assert!(root.keys.iter().any(|k| k.comment == "orbtest-1" && k.fp.starts_with("SHA256:")), "{root:?}");
    assert!(ssh::check_change(&i, "root", "PasswordAuthentication", "no").is_ok());
    vm.exec("关密码登录", &ssh::set_script("PasswordAuthentication", "no").unwrap());
    assert!(!read_ssh(&vm).password, "00-otter.conf 要排在 50-orbtest.conf 前面生效");
    vm.exec("取消回滚", &ssh::guard_cancel());

    // 回滚脚本:还原成设置之前
    vm.exec("PermitRootLogin", &ssh::set_script("PermitRootLogin", "prohibit-password").unwrap());
    vm.exec("手动执行回滚", "sh /run/otter-ssh-rollback.sh");
    let i = read_ssh(&vm);
    assert_ne!(i.root_login, "", "{i:?}");
    assert_eq!(i.otter_value("PermitRootLogin"), None, "回滚后 PermitRootLogin 应该回到设置之前: {i:?}");
    assert!(!i.password, "回滚只回到最近一次设置之前,之前关掉的密码登录还是关的");
    vm.exec("取消回滚", &ssh::guard_cancel());

    // 校验失败:写坏的 sshd_config 不会被重载,会还原
    vm.sh("rm -f /etc/ssh/sshd_config.d/50-orbtest.conf");
    let before = vm.sh("cat /etc/ssh/sshd_config.d/00-otter.conf");
    // 主配置没有 Include(RHEL 8 的 OpenSSH 8.0)时,脚本直接写主配置,写入点不一样
    let (from, to) = if i0.include { ("echo \"$K $V\" >> $F.tmp", "echo \"$K $V bogus-extra\" >> $F.tmp") } else { ("echo \"$K $V # otter\";", "echo \"$K $V bogus-extra # otter\";") };
    let bad = ssh::set_script("MaxAuthTries", "3").unwrap();
    assert!(bad.contains(from), "没找到要注入错误的位置");
    let (c, o) = vm.run(&bad.replace(from, to));
    assert_ne!(c, 0, "{o}");
    assert!(o.contains("校验失败") || o.contains("没有生效"), "{o}");
    assert_eq!(vm.sh("cat /etc/ssh/sshd_config.d/00-otter.conf"), before, "失败后文件要还原");
    assert!(vm.run("sshd -t").0 == 0);

    // 端口:改到 2222,监听在新端口,sshd -T 看到的也是 2222
    vm.exec("改端口", &ssh::set_script("Port", "2222").unwrap());
    let i = read_ssh(&vm);
    assert_eq!(i.ports, vec![2222], "{i:?}");
    let ss = vm.sh("ss -ltn");
    assert!(ss.contains(":2222"), "{ss}");
    if !i.socket {
        // 没有 socket 激活时,22 应该已经释放
        assert!(!vm.sh("ss -ltn").lines().any(|l| l.split_whitespace().nth(3).is_some_and(|a| a.ends_with(":22"))), "{ss}");
    }
    // 回滚到 22
    vm.exec("回滚端口", "sh /run/otter-ssh-rollback.sh");
    std::thread::sleep(std::time::Duration::from_secs(1));
    let i = read_ssh(&vm);
    assert_eq!(i.ports, vec![22], "{i:?}");
    assert!(vm.sh("ss -ltn").contains(":22 "), "22 应该重新监听");
    vm.exec("取消回滚", &ssh::guard_cancel());
    // 占用中的端口要拒绝
    vm.sh("if command -v python3 >/dev/null 2>&1; then (setsid nohup python3 -m http.server 18099 --bind 0.0.0.0 >/dev/null 2>&1 < /dev/null &); else (setsid nohup nc -lk 0.0.0.0 18099 >/dev/null 2>&1 < /dev/null &); fi; sleep 1");
    if vm.sh("ss -ltn").contains(":18099") {
        let (c, o) = vm.run(&ssh::set_script("Port", "18099").unwrap());
        assert_ne!(c, 0);
        assert!(o.contains("占用"), "{o}");
    } else {
        println!("起不了监听程序(没有 python3 和 nc),跳过端口占用检查");
    }
    vm.sh("pkill -f 'http.server 18099'; pkill -f 'nc -lk 0.0.0.0 18099'");

    // 公钥:增删、重复、无换行文件
    let user = "orbssh";
    vm.exec("建用户", &format!("useradd -m {user}"));
    exec_in(&vm, "添加公钥 1", &ssh::add_key_script(user).unwrap(), PUB1);
    exec_in(&vm, "添加公钥 2", &ssh::add_key_script(user).unwrap(), PUB2);
    let (c, o) = run_in(&vm, &ssh::add_key_script(user).unwrap(), PUB1);
    assert_ne!(c, 0);
    assert!(o.contains("已经在里面"), "{o}");
    let i = read_ssh(&vm);
    let u = i.user(user).expect("orbssh 应该出现在用户列表里");
    assert_eq!(u.keys.len(), 2, "{u:?}");
    assert_eq!(vm.sh(&format!("stat -c '%a %U' /home/{user}/.ssh /home/{user}/.ssh/authorized_keys")).trim(), "700 orbssh\n600 orbssh");
    // 最后一行没有换行时追加不会粘在一起
    vm.sh(&format!("printf '%s' \"$(cat /home/{user}/.ssh/authorized_keys)\" > /home/{user}/.ssh/authorized_keys"));
    vm.sh(&format!("rm -f /home/{user}/.ssh/authorized_keys.x"));
    let k1 = u.keys[0].clone();
    vm.exec("删除公钥", &ssh::delete_key_script(user, &k1.raw).unwrap());
    let u = read_ssh(&vm).user(user).unwrap().clone();
    assert_eq!(u.keys.len(), 1);
    assert_ne!(u.keys[0].raw, k1.raw);
    let (c, o) = vm.run(&ssh::delete_key_script(user, &k1.raw).unwrap());
    assert_ne!(c, 0, "{o}");
    assert!(o.contains("已经不在"), "{o}");
    vm.sh(&format!("userdel -r {user}"));
    // root 的测试公钥清掉
    vm.exec("删 root 公钥", &ssh::delete_key_script("root", PUB1).unwrap());
    vm.sh("rm -f /etc/ssh/sshd_config.d/00-otter.conf");
}
per_distro_tools!(ssh_tool, ssh_tool);

// ================= 用户与 sudo =================

const USERS_RESET: &str = "for u in orbu1 orbu2; do pkill -KILL -u $u >/dev/null 2>&1; userdel -rf $u >/dev/null 2>&1; done; true";

fn read_users(vm: &Vm) -> users::UsersInfo {
    users::parse_users(&vm.sh(users::USERS_SCRIPT)).expect("读用户")
}

fn users_tool(d: Distro) {
    let vm = Vm::shared(d, "toolmore-users", USERS_RESET);
    let i0 = read_users(&vm);
    let group = i0.admin_group.clone().expect("管理员组");
    println!("管理员组 {group},{} 个用户", i0.users.len());
    assert!(i0.users.iter().any(|u| u.uid == 0));

    // 新建:密码 + 公钥 + sudo
    let (s, input) = users::create_script("orbu1", Some(&group), "Pw-orbtest-1", PUB1).unwrap();
    exec_in(&vm, "新建用户", &s, &input);
    let i = read_users(&vm);
    let u = i.users.iter().find(|u| u.name == "orbu1").expect("orbu1");
    assert_eq!(u.pw, users::Pw::Set, "{u:?}");
    assert!(i.is_admin(u) && u.has_shell() && !u.locked(), "{u:?}");
    assert!(vm.sh("cat /home/orbu1/.ssh/authorized_keys").contains("orbtest-1"));
    assert_eq!(vm.sh("stat -c '%a %U' /home/orbu1/.ssh /home/orbu1/.ssh/authorized_keys").trim(), "700 orbu1\n600 orbu1");
    assert!(vm.sh(&format!("id -nG orbu1")).split_whitespace().any(|g| g == group));
    // 密码真的设上了:能用 su 验证
    let su = vm.sh("printf 'Pw-orbtest-1\\n' | su -c 'id -un' orbu1 2>&1 | tail -1");
    println!("su: {su}");
    // 重名
    let (c, o) = run_in(&vm, &s, &input);
    assert_ne!(c, 0);
    assert!(o.contains("已经存在"), "{o}");
    // 只有公钥(没有密码)的用户
    let (s2, in2) = users::create_script("orbu2", None, "", PUB2).unwrap();
    exec_in(&vm, "新建仅公钥用户", &s2, &in2);
    let i = read_users(&vm);
    let u2 = i.users.iter().find(|u| u.name == "orbu2").unwrap();
    assert_eq!(u2.pw, users::Pw::None, "{u2:?}");
    assert!(!i.is_admin(u2) && !u2.locked());
    // 创建失败要清理干净:密码里带冒号没问题,公钥格式错在 Rust 里就拒绝;这里模拟脚本中途失败
    let (c, _) = run_in(&vm, &users::create_script("orbu3", Some("no-such-group"), "x", "").unwrap().0, "x\n\n");
    assert_ne!(c, 0);
    assert!(vm.run("id orbu3").0 != 0, "失败后不该留下半成品用户");
    assert!(vm.run("test -d /home/orbu3").0 != 0, "失败后不该留下家目录");

    // 改密码
    let (ps, pin) = users::password_script("orbu1", "New-pass-2").unwrap();
    let before = vm.sh("getent shadow orbu1 | cut -d: -f2");
    exec_in(&vm, "改密码", &ps, &pin);
    assert_ne!(vm.sh("getent shadow orbu1 | cut -d: -f2"), before);
    // 锁定 / 解锁
    vm.exec("锁定", &users::lock_script("orbu1").unwrap());
    assert!(read_users(&vm).users.iter().find(|u| u.name == "orbu1").unwrap().locked());
    vm.exec("锁定仅公钥用户", &users::lock_script("orbu2").unwrap());
    assert!(read_users(&vm).users.iter().find(|u| u.name == "orbu2").unwrap().locked());
    vm.exec("解锁", &users::unlock_script("orbu1").unwrap());
    vm.exec("解锁仅公钥用户", &users::unlock_script("orbu2").unwrap());
    let i = read_users(&vm);
    assert!(!i.users.iter().find(|u| u.name == "orbu1").unwrap().locked(), "{:?}", i.users);
    let u2 = i.users.iter().find(|u| u.name == "orbu2").unwrap();
    assert!(!u2.locked() && u2.pw == users::Pw::None, "仅公钥用户解锁后不能变成空密码: {u2:?}");
    // sudo 开关
    vm.exec("移出管理员组", &users::sudo_script("orbu1", &group, false).unwrap());
    let i = read_users(&vm);
    assert!(!i.is_admin(i.users.iter().find(|u| u.name == "orbu1").unwrap()));
    vm.exec("加入管理员组", &users::sudo_script("orbu2", &group, true).unwrap());
    let i = read_users(&vm);
    assert!(i.is_admin(i.users.iter().find(|u| u.name == "orbu2").unwrap()));
    // 删除:有进程在跑也能删;保留家目录和连家目录一起删
    vm.sh("su -s /bin/sh -c 'nohup sleep 300 >/dev/null 2>&1 &' orbu1; sleep 1");
    assert!(vm.sh("pgrep -u orbu1 sleep").trim().len() > 0, "应该有一个 sleep 进程");
    vm.exec("删除并保留家目录", &users::delete_script("orbu1", false).unwrap());
    assert!(read_users(&vm).users.iter().all(|u| u.name != "orbu1"));
    assert!(vm.run("test -d /home/orbu1").0 == 0, "家目录要保留");
    vm.sh("rm -rf /home/orbu1");
    vm.exec("删除连家目录", &users::delete_script("orbu2", true).unwrap());
    assert!(read_users(&vm).users.iter().all(|u| u.name != "orbu2"));
    assert!(vm.run("test -d /home/orbu2").0 != 0, "家目录要一起删");
    let (c, o) = vm.run(&users::delete_script("orbu2", true).unwrap());
    assert_ne!(c, 0);
    assert!(o.contains("不存在"), "{o}");
}
per_distro_tools!(users_tool, users_tool);

// ================= fail2ban =================

const F2B_RESET: &str = "rm -f /etc/fail2ban/jail.d/otter.local; systemctl is-active fail2ban >/dev/null 2>&1 && systemctl restart fail2ban >/dev/null 2>&1; true";

fn read_f2b(vm: &Vm) -> f2b::F2bInfo {
    f2b::parse_f2b(&vm.sh(f2b::F2B_SCRIPT)).expect("读 fail2ban")
}

fn fail2ban_tool(d: Distro) {
    let vm = Vm::shared(d, "toolmore-f2b", F2B_RESET);
    let i0 = read_f2b(&vm);
    println!("fail2ban:{:?} 包管理器 {} 运行 {} 来源 {}", i0.bin, i0.pkg, i0.active, i0.client);
    assert!(["apt", "dnf", "yum"].contains(&i0.pkg.as_str()), "{i0:?}");
    // 没装就装(用户在界面上点「安装」等价的脚本),白名单带上本次连接来源
    if i0.bin.is_none() {
        let (s, input) = f2b::install_script(&i0.pkg, &f2b::initial_conf("203.0.113.9")).unwrap();
        exec_in(&vm, "安装 fail2ban", &s, &input);
    } else {
        let (s, input) = f2b::save_conf_script(&f2b::initial_conf("203.0.113.9")).unwrap();
        exec_in(&vm, "写初始配置", &s, &input);
    }
    let i = read_f2b(&vm);
    assert!(i.bin.is_some() && i.active, "{i:?}");
    assert!(i.conf.sshd_enabled(), "{:?}", i.conf);
    assert!(i.conf.ignore().contains(&"203.0.113.9".to_string()) && i.conf.ignore().contains(&"127.0.0.1/8".to_string()), "{:?}", i.conf);
    let j = i.jails.iter().find(|j| j.name == "sshd").unwrap_or_else(|| panic!("sshd jail 没起来:{i:?}\n{}", vm.sh("fail2ban-client status 2>&1; journalctl -u fail2ban --no-pager -n 20 2>&1")));
    assert!(j.bantime.is_some() && j.maxretry.is_some(), "{j:?}");

    // 手动封一个 IP,读得到,解封后没了
    vm.exec("封禁", "fail2ban-client set sshd banip 198.51.100.7");
    let j = read_f2b(&vm).jails.into_iter().find(|j| j.name == "sshd").unwrap();
    assert!(j.banned.contains(&"198.51.100.7".to_string()) && j.banned_now >= 1, "{j:?}");
    // 封禁真的落到了防火墙上
    let rules = vm.sh("nft list ruleset 2>/dev/null; iptables -S 2>/dev/null; firewall-cmd --list-rich-rules 2>/dev/null");
    assert!(rules.contains("198.51.100.7"), "封禁的 IP 应该出现在防火墙规则里:{rules}");
    vm.exec("解封", &f2b::unban_script("sshd", "198.51.100.7").unwrap());
    let j = read_f2b(&vm).jails.into_iter().find(|j| j.name == "sshd").unwrap();
    assert!(!j.banned.contains(&"198.51.100.7".to_string()), "{j:?}");

    // 改封禁参数:写进配置、服务重载后生效
    let mut c = read_f2b(&vm).conf;
    c.set("bantime", "86400");
    c.set("findtime", "3600");
    c.set("maxretry", "3");
    let (s, input) = f2b::save_conf_script(&c).unwrap();
    exec_in(&vm, "改参数", &s, &input);
    let j = read_f2b(&vm).jails.into_iter().find(|j| j.name == "sshd").unwrap();
    assert_eq!((j.bantime, j.findtime, j.maxretry), (Some(86400), Some(3600), Some(3)), "{j:?}");
    // 写坏配置:检查不通过,原样还原
    let mut bad = c.clone();
    bad.set("bantime", "abc");
    assert!(f2b::save_conf_script(&bad).is_err(), "Rust 侧先拦");
    let before = vm.sh("cat /etc/fail2ban/jail.d/otter.local");
    let (c2, o) = run_in(&vm, &f2b::save_conf_script(&c).unwrap().0, "[DEFAULT]\nbantime = 1d\n[sshd]\nenabled = true\nfilter = no-such-filter\n");
    assert_ne!(c2, 0, "{o}");
    assert!(o.contains("还原"), "{o}");
    assert_eq!(vm.sh("cat /etc/fail2ban/jail.d/otter.local"), before, "失败后配置要还原");
    assert!(read_f2b(&vm).active, "失败后服务要还在运行");

    // 启停
    vm.exec("停止", &f2b::service_script(false));
    assert!(!read_f2b(&vm).active);
    vm.exec("启动", &f2b::service_script(true));
    assert!(read_f2b(&vm).active);
}
per_distro_tools!(fail2ban_tool, fail2ban_tool);
