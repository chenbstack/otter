//! `otter`:命令行程序,不带界面。只连运行中的 Otter App 的本地接口(见 docs/modules/ctl.md)。
//! 界面程序是 `otter-gui`(App 包里的可执行文件),这里不链接任何界面代码,启动快、体积小。

#[path = "../ctl_cli.rs"]
mod ctl_cli;
#[path = "../ipc.rs"]
mod ipc;

const USAGE: &str = "otter - talk to a running Otter app through its local interface (open Otter first)

Query (read-only, prints JSON):
  otter nodes [--group <name|id>]   server nodes
  otter groups                      node groups
  otter sessions                    open terminal sessions
  otter current                     the session the user is looking at (never switches tabs)

Takeover (taking over and releasing are separate commands; a failing command does not drop it):
  otter takeover [options]          ask to take over a session; Otter shows a confirmation, then it stays held
  otter release [options]           hand the session back (sends no Ctrl+C)

Act (needs an active takeover):
  otter run [options] <command>     type a command and press Enter, print only its new output
  otter type [options] <text>       type text without pressing Enter
  otter key [options] <key>         press a key: enter tab esc up down ctrl+c ...
  otter screen [options]            print the current screen
  otter scroll [options] [lines]    print recent scrollback, 100 lines by default

For agents:
  otter mcp [--agent <name>]        run as an MCP server over stdio

Other:
  otter --version | --help
  otter <command> --help            options of one command

Every command accepts --agent <name> (or the OTTER_AGENT environment variable) so the user can see
in Otter which agent is acting; without it MCP uses the client's own name and the CLI shows otter-cli.
--session <id> picks the session; the default is the one the user is looking at.";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("nodes" | "groups" | "sessions" | "current" | "takeover" | "release" | "run" | "type" | "key" | "screen" | "scroll") => ctl_cli::run_ctl(&args[1..]),
        // 旧写法 `otter ctl <命令>` 继续能用
        Some("ctl") => ctl_cli::run_ctl(&args[2..]),
        Some("mcp") => ctl_cli::run_mcp(&args[2..]),
        Some("--version" | "-V" | "version") => {
            println!("otter {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("--help" | "-h" | "help") | None => {
            println!("{USAGE}");
            0
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}
