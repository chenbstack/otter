//! 模板机:每个发行版一台长期保留的 `otter-orbtest-base-<key>`,装好 Docker、防火墙软件和测试要用的镜像。
//! 装机脚本改了(哈希对不上)下次用的时候自动重建;失败不留半成品。

use std::sync::Mutex;

use super::harness::{self, Distro, Vm, BASE_PREFIX, PREFIX};

fn script_of(d: Distro) -> &'static str {
    match d.key {
        "ubuntu" => include_str!("provision/ubuntu.sh"),
        "rocky" => include_str!("provision/rocky.sh"),
        "debian" => include_str!("provision/debian.sh"),
        "ubuntu22" => include_str!("provision/ubuntu22.sh"),
        "rocky8" => include_str!("provision/rocky8.sh"),
        "debian11" => include_str!("provision/debian11.sh"),
        _ => unreachable!(),
    }
}

/// 装机脚本按 `## 说明` 分步,每步单独执行,出错时能说清是哪一步
fn steps(d: Distro) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = Vec::new();
    for line in script_of(d).lines() {
        if let Some(label) = line.strip_prefix("## ") {
            v.push((label.trim().to_string(), String::new()));
        } else if let Some(last) = v.last_mut() {
            last.1.push_str(line);
            last.1.push('\n');
        }
    }
    v
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// 装机脚本和镜像参数的哈希:任何一个变了,模板机就过期
pub fn hash(d: Distro) -> String {
    format!("{:016x}", fnv(&format!("{}\n{}", d.image, script_of(d))))
}

fn stamp_path(d: Distro) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join("Library/Caches/otter-orbtest").join(format!("{}.stamp", d.key))
}

/// 模板机存在且是最新的
pub fn is_fresh(d: Distro) -> bool {
    harness::machine_state(&d.base_name()).is_some() && std::fs::read_to_string(stamp_path(d)).map(|s| s.trim() == hash(d)).unwrap_or(false)
}

static LOCK: Mutex<()> = Mutex::new(());

/// 确保模板机存在且最新;没有或过期就(重)建
pub fn ensure(d: Distro) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if is_fresh(d) {
        return;
    }
    build(d);
}

/// 删掉旧的重建
pub fn build(d: Distro) {
    let name = d.base_name();
    if harness::machine_state(&name).is_some() {
        println!("[{}] 模板机过期或要求重建,先删掉旧的", d.key);
        harness::delete_machine(&name);
    }
    let _ = std::fs::remove_file(stamp_path(d));
    println!("[{}] 创建模板机 {name}({})", d.key, d.image);
    // 镜像源偶尔 503:创建和每个装机步骤都重试几次
    let mut last = String::new();
    let mut created = false;
    for attempt in 1..=3 {
        let (c, o) = harness::orbctl(&["create", d.image, &name]);
        if c == 0 {
            created = true;
            break;
        }
        println!("[{}] 创建失败(第 {attempt} 次),重试:{}", d.key, o.lines().next().unwrap_or(""));
        last = o;
        harness::delete_machine(&name);
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    if !created {
        panic!("创建 {name} 失败:{last}");
    }
    let vm = Vm::attach(&name, d);
    vm.wait_ready();
    for (i, (label, body)) in steps(d).iter().enumerate() {
        println!("[{}] 第 {} 步:{label}", d.key, i + 1);
        let (mut c, mut out) = (0, String::new());
        for attempt in 1..=3 {
            (c, out) = vm.run(&format!("set -e\n{body}"));
            if c == 0 {
                break;
            }
            println!("[{}] 第 {} 步失败(第 {attempt} 次),重试", d.key, i + 1);
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        if c != 0 {
            let tail: Vec<&str> = out.lines().rev().take(25).collect();
            harness::delete_machine(&name);
            panic!("[{}] 装机第 {} 步「{label}」失败(退出码 {c}),已删除半成品模板机。最后的输出:\n{}", d.key, i + 1, tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
        }
    }
    // 停机:克隆停着的机器是一致的快照
    let _ = harness::orbctl(&["stop", &name]);
    let path = stamp_path(d);
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    std::fs::write(&path, hash(d)).unwrap();
    println!("[{}] 模板机就绪", d.key);
}

/// 命令行里指定的发行版(`OTTER_ORBTEST_DISTROS=ubuntu,rocky`),没指定是全部
fn wanted() -> Vec<Distro> {
    match std::env::var("OTTER_ORBTEST_DISTROS") {
        Ok(s) if !s.trim().is_empty() => s.split(',').map(|k| Distro::by_key(k.trim()).unwrap_or_else(|| panic!("没有这个发行版:{k}(可选:ubuntu、rocky、debian、ubuntu22、rocky8、debian11)"))).collect(),
        _ => Distro::ALL.to_vec(),
    }
}

// ---- scripts/orbtest.sh 的子命令 ----

#[test]
#[ignore]
fn cmd_init() {
    if !harness::gate() {
        return;
    }
    for d in wanted() {
        if is_fresh(d) {
            println!("[{}] 模板机已是最新,跳过", d.key);
        } else {
            ensure(d);
        }
    }
}

#[test]
#[ignore]
fn cmd_rebuild() {
    if !harness::gate() {
        return;
    }
    for d in wanted() {
        build(d);
    }
}

#[test]
#[ignore]
fn cmd_status() {
    if !harness::gate() {
        return;
    }
    println!();
    let (_, list) = harness::orbctl(&["list"]);
    for d in Distro::ALL {
        let name = d.base_name();
        let line = list.lines().find(|l| l.split_whitespace().next() == Some(name.as_str()));
        match line {
            None => println!("{:<8} 没有模板机", d.key),
            Some(l) => {
                let f: Vec<&str> = l.split_whitespace().collect();
                println!("{:<8} {}  {}  磁盘 {}", d.key, f.get(1).unwrap_or(&""), if is_fresh(d) { "最新" } else { "过期(下次使用时自动重建)" }, f.get(5).map(|s| format!("{s} {}", f.get(6).unwrap_or(&""))).unwrap_or_default());
            }
        }
    }
    let temps: Vec<String> = harness::machine_names().into_iter().filter(|n| !n.starts_with(BASE_PREFIX)).collect();
    if temps.is_empty() {
        println!("没有残留的临时机器");
    } else {
        println!("残留的临时机器(scripts/orbtest.sh clean 会删):{}", temps.join(" "));
    }
}

/// 删掉所有 `otter-orbtest-*`(含模板机)
#[test]
#[ignore]
fn cmd_clean() {
    if !harness::gate() {
        return;
    }
    for n in harness::machine_names() {
        println!("删除 {n}");
        harness::delete_machine(&n);
    }
    for d in Distro::ALL {
        let _ = std::fs::remove_file(stamp_path(d));
    }
    assert!(harness::machine_names().is_empty(), "还有残留:{:?}", harness::machine_names());
}

/// 只删残留的临时机器,保留模板机(`scripts/orbtest.sh` 开头结尾用)
#[test]
#[ignore]
fn cmd_sweep() {
    if !harness::gate() {
        return;
    }
    for n in harness::machine_names().into_iter().filter(|n| !n.starts_with(BASE_PREFIX)) {
        println!("清理残留 {n}");
        harness::delete_machine(&n);
    }
    let _ = PREFIX;
}
