//! orbtest:在 OrbStack 虚拟机里真机测试服务器端脚本。只在 `#[cfg(all(test, feature = "orbtest"))]` 里,不进构建和打包,普通 `cargo test` 连编译都不编译;
//! 用例全部 `#[ignore]`,并且要环境变量 `OTTER_ORBTEST=1`(`scripts/orbtest.sh` 会设)才真的动手。
//! 设计和用法见 `docs/modules/orbtest.md`、`docs/plans/active/2026-11-27-orbtest.md`。

mod firewall;
mod harness;
mod provision;

pub use harness::{gate, gate_slow, Distro, Lease, Vm};
