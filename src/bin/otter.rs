//! `otter`:命令行程序,不带界面。只连运行中的 Otter App 的本地接口(见 docs/modules/ctl.md)。
//! 界面程序是 `otter-gui`(App 包里的可执行文件),这里不链接任何界面代码,启动快、体积小。

#[path = "../ctl_cli.rs"]
mod ctl_cli;
#[path = "../ipc.rs"]
mod ipc;

const USAGE: &str = "otter — 通过本地接口访问运行中的 Otter App

用法:
  otter ctl nodes [--group <名字或id>]   列出节点
  otter ctl groups                        列出分组
  otter ctl sessions                      列出已连接的终端会话
  otter ctl focus                         当前聚焦的会话
  otter ctl shell [会话id]                交互模式(接管 → 操作 → 交还)
  otter ctl run [选项] <命令>             一次性:接管、执行、交还
  otter ctl screen [选项]                 一次性:接管、读屏幕、交还
  otter mcp                               以 MCP 服务器方式运行(stdio),给 Claude Code 等 agent 用
  otter --version | --help

需要先打开 Otter App。接管会在 App 里弹出确认框。";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("ctl") => ctl_cli::run_ctl(&args[2..]),
        Some("mcp") => ctl_cli::run_mcp(),
        Some("--version" | "-V" | "version") => {
            println!("otter {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("--help" | "-h" | "help") | None => {
            println!("{USAGE}");
            0
        }
        Some(other) => {
            eprintln!("不认识的命令: {other}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}
