//! 虚拟机的建、删、执行。一个 `Vm` 就是从模板机克隆出来的一台临时机器,`Drop` 时删除(用例失败、panic 也删)。

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use super::provision;

/// 所有 orbtest 机器的名字前缀;模板机是 `BASE_PREFIX` 开头,克隆机是 `otter-orbtest-<进程号>-<发行版>-<序号>`
pub const PREFIX: &str = "otter-orbtest-";
pub const BASE_PREFIX: &str = "otter-orbtest-base-";

/// 矩阵里的一个发行版
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Distro {
    /// 短名:模板机叫 `otter-orbtest-base-<key>`
    pub key: &'static str,
    /// `orbctl create` 的发行版参数
    pub image: &'static str,
}

impl Distro {
    pub const UBUNTU: Distro = Distro { key: "ubuntu", image: "ubuntu:noble" };
    pub const ROCKY: Distro = Distro { key: "rocky", image: "rocky:9" };
    pub const DEBIAN: Distro = Distro { key: "debian", image: "debian:bookworm" };
    pub const ALL: [Distro; 3] = [Self::UBUNTU, Self::ROCKY, Self::DEBIAN];

    pub fn base_name(&self) -> String {
        format!("{BASE_PREFIX}{}", self.key)
    }

    pub fn by_key(key: &str) -> Option<Distro> {
        Self::ALL.into_iter().find(|d| d.key == key)
    }
}

/// 没设 `OTTER_ORBTEST=1` 就不动手:用例开头 `if !gate() { return; }`
pub fn gate() -> bool {
    if std::env::var("OTTER_ORBTEST").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("跳过:orbtest 只在要求时运行,请用 scripts/orbtest.sh(它会设 OTTER_ORBTEST=1)");
    false
}

/// 慢用例(要等回滚定时器之类)还要 `OTTER_ORBTEST_SLOW=1`(`scripts/orbtest.sh --slow`)
pub fn gate_slow() -> bool {
    if !gate() {
        return false;
    }
    if std::env::var("OTTER_ORBTEST_SLOW").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("跳过慢用例:加 --slow 才跑");
    false
}

/// 执行宿主机上的命令,返回 (退出码, 合并输出)
pub fn host(program: &str, args: &[&str], stdin: Option<&str>) -> (i32, String) {
    let mut cmd = Command::new(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().unwrap_or_else(|e| panic!("跑不起来 {program}:{e}(装了 OrbStack 吗?)"));
    if let Some(s) = stdin {
        child.stdin.take().unwrap().write_all(s.as_bytes()).ok();
    }
    let o = child.wait_with_output().unwrap();
    (o.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

pub fn orbctl(args: &[&str]) -> (i32, String) {
    host("orbctl", args, None)
}

/// 机器名 → 状态(`running` / `stopped`),没有这台机器返回 None
pub fn machine_state(name: &str) -> Option<String> {
    let (_, out) = orbctl(&["list"]);
    out.lines().filter_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.first() == Some(&name)).then(|| f.get(1).unwrap_or(&"").to_string())
    }).next()
}

/// 所有 orbtest 机器的名字
pub fn machine_names() -> Vec<String> {
    let (_, out) = orbctl(&["list", "-q"]);
    out.lines().map(|l| l.trim().to_string()).filter(|n| n.starts_with(PREFIX)).collect()
}

pub fn delete_machine(name: &str) {
    let _ = orbctl(&["delete", "-f", name]);
}

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub struct Vm {
    pub name: String,
    pub distro: Distro,
    /// 是克隆出来的(Drop 时删);`OTTER_VM` 指定的现成机器不删
    owned: bool,
}

/// 一台共享机器的租约:同一台机器上,同一个 `area`(功能域,如 "firewall")的用例排队用,
/// 不同 `area` 互不影响可以同时用。拿到租约时先执行 `reset` 脚本把该域的状态复位。
pub struct Lease {
    vm: Arc<Vm>,
    _guard: MutexGuard<'static, ()>,
}

impl std::ops::Deref for Lease {
    type Target = Vm;
    fn deref(&self) -> &Vm {
        &self.vm
    }
}

type Shared = Mutex<HashMap<String, Arc<Vm>>>;
type Locks = Mutex<HashMap<String, &'static Mutex<()>>>;
static SHARED: OnceLock<Shared> = OnceLock::new();
static LOCKS: OnceLock<Locks> = OnceLock::new();

impl Vm {
    /// 取这个发行版的共享机器(整个测试进程里只克隆一次,进程结束后由 `orbtest.sh` 的收尾清理删除)。
    /// 用例之间不要依赖上一个用例留下的状态:`reset` 在每次拿到租约时执行。
    pub fn shared(d: Distro, area: &str, reset: &str) -> Lease {
        let lock: &'static Mutex<()> = *LOCKS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .entry(format!("{}/{area}", d.key))
            .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))));
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let vm = {
            let mut m = SHARED.get_or_init(Default::default).lock().unwrap();
            m.entry(d.key.to_string()).or_insert_with(|| Arc::new(Vm::acquire(d))).clone()
        };
        let (c, o) = vm.run(reset);
        assert_eq!(c, 0, "复位 {area} 失败:{o}");
        Lease { vm, _guard: guard }
    }

    /// 取一台干净的机器:`OTTER_VM=<名字>` 直接用现成的;否则确保模板机最新,再克隆一台并启动
    pub fn acquire(d: Distro) -> Vm {
        if let Ok(name) = std::env::var("OTTER_VM") {
            let vm = Vm { name, distro: d, owned: false };
            vm.wait_ready();
            return vm;
        }
        provision::ensure(d);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("{PREFIX}{}-{}-{n}", std::process::id(), d.key);
        let (c, o) = orbctl(&["clone", &d.base_name(), &name]);
        assert_eq!(c, 0, "克隆 {} 失败:{o}", d.base_name());
        let vm = Vm { name, distro: d, owned: true };
        let (c, o) = orbctl(&["start", &vm.name]);
        assert_eq!(c, 0, "启动 {} 失败:{o}", vm.name);
        vm.wait_ready();
        vm
    }

    /// 接上一台现成的机器(模板机的装机用,不会删它)
    pub(super) fn attach(name: &str, d: Distro) -> Vm {
        Vm { name: name.to_string(), distro: d, owned: false }
    }

    /// 等 systemd 起来(running / degraded 都算)
    pub fn wait_ready(&self) {
        let t = Instant::now();
        loop {
            let (_, o) = self.run("systemctl is-system-running 2>&1");
            let s = o.trim();
            if s == "running" || s == "degraded" {
                return;
            }
            assert!(t.elapsed() < Duration::from_secs(90), "{} 一直没就绪:{s}", self.name);
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// 以 root 执行脚本(从标准输入喂给 `sh -s`),返回 (退出码, 合并输出)
    pub fn run(&self, script: &str) -> (i32, String) {
        host("orb", &["-m", &self.name, "sudo", "sh", "-s"], Some(script))
    }

    /// 执行并要求退出码为 0;输出前几行打印出来方便看
    pub fn exec(&self, label: &str, script: &str) -> String {
        let (c, o) = self.run(script);
        println!("== {label}: exit {c}\n{}", o.lines().take(12).collect::<Vec<_>>().join("\n"));
        assert_eq!(c, 0, "{label} 失败:{o}");
        o
    }

    /// 执行,只要输出
    pub fn sh(&self, cmd: &str) -> String {
        self.run(cmd).1
    }

    /// 虚拟机的 IPv4 地址(宿主机能直接访问)
    pub fn ip(&self) -> String {
        self.sh("ip -4 -o addr show scope global | awk '{print $4}' | cut -d/ -f1 | head -1").trim().to_string()
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }
        if std::env::var("OTTER_VM_KEEP").as_deref() == Ok("1") && std::thread::panicking() {
            eprintln!("保留现场:{}(用 orb -m {} 进去看,看完 scripts/orbtest.sh clean)", self.name, self.name);
            return;
        }
        delete_machine(&self.name);
    }
}

/// 宿主机(Mac)访问虚拟机时用的源地址:虚拟机能看到的是这个 IP
pub fn host_ip_to(vm_ip: &str) -> String {
    let (_, o) = host("sh", &["-c", &format!("ifconfig $(route -n get {vm_ip} | awk '/interface:/{{print $2}}') | awk '/inet /{{print $2}}' | head -1")], None);
    o.trim().to_string()
}

/// 从宿主机访问虚拟机上的 HTTP 端口,返回状态码(连不上是 `000`)
pub fn curl(ip: &str, port: u16) -> String {
    let (_, o) = host("curl", &["-s", "-m", "3", "-o", "/dev/null", "-w", "%{http_code}", &format!("http://{ip}:{port}/")], None);
    o.trim().to_string()
}
