//! orbtest:在 OrbStack 虚拟机里真机测试服务器端脚本。只在 `#[cfg(all(test, feature = "orbtest"))]` 里,不进构建和打包,普通 `cargo test` 连编译都不编译;
//! 用例全部 `#[ignore]`,并且要环境变量 `OTTER_ORBTEST=1`(`scripts/orbtest.sh` 会设)才真的动手。
//! 设计和用法见 `docs/modules/orbtest.md`、`docs/plans/active/2026-11-27-orbtest.md`。

/// 同一个用例函数在一组发行版上各生成一个测试:`distros!(模块名, 函数, gate 或 gate_slow, 发行版组)`,
/// 每个发行版是模块里的一个测试(`模块名::ubuntu`、`模块名::rocky` ……),函数签名 `fn(Distro)`;不同发行版并行跑。
macro_rules! distros {
    ($name:ident, $f:ident, $gate:ident, $set:ident) => {
        mod $name {
            distros!(@gen $f, $gate, $set);
        }
    };
    (@gen $f:ident, $gate:ident, ALL) => {
        distros!(@t $f, $gate, [ubuntu = UBUNTU, debian = DEBIAN, rocky = ROCKY]);
    };
    // 防火墙前端不同的三族
    (@gen $f:ident, $gate:ident, UFW) => {
        distros!(@t $f, $gate, [ubuntu = UBUNTU]);
    };
    (@gen $f:ident, $gate:ident, FWD) => {
        distros!(@t $f, $gate, [rocky = ROCKY]);
    };
    (@gen $f:ident, $gate:ident, PLAIN) => {
        distros!(@t $f, $gate, [debian = DEBIAN]);
    };
    (@t $f:ident, $gate:ident, [$($t:ident = $d:ident),*]) => {
        $(
            #[test]
            #[ignore]
            pub fn $t() {
                if crate::orbtest::$gate() {
                    super::$f(crate::orbtest::Distro::$d)
                }
            }
        )*
    };
}

macro_rules! per_distro {
    ($name:ident, $f:ident) => {
        distros!($name, $f, gate, ALL);
    };
}

mod appstore;
mod cron;
mod database;
mod docker;
mod firewall;
mod harness;
mod mounts;
mod provision;
mod system;
mod toolbox;
mod web;

pub use harness::{gate, gate_slow, Distro, Lease, Vm};
