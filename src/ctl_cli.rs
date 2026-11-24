//! `otter …` 和 `otter mcp`:连本地 socket 的薄客户端,不启动界面。
//! MCP 是主入口(stdio,按行分隔的 JSON-RPC),CLI 只给人和脚本查询用。

use std::io::{BufRead, BufReader, Write};
use crate::ipc::SyncStream;

use serde_json::{Value, json};

/// agent 自报的名称(必须提供):`--agent <名字>` > 环境变量 `OTTER_AGENT` > MCP 客户端自报的 `clientInfo.name`。界面上的确认框、遮罩、标签提示和日志
/// 都显示它。名称只是给用户看的,不是身份。
static AGENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn agent_name(default: Option<&str>) -> Result<String, CallError> {
    AGENT
        .get()
        .cloned()
        .or_else(|| std::env::var("OTTER_AGENT").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
        .or_else(|| default.map(String::from))
        .ok_or_else(|| CallError { kind: "bad_request".into(), message: "必须说明你是谁:加 --agent <名字>(或设环境变量 OTTER_AGENT),它会显示在用户的确认框和日志里".into() })
}

/// 连接 Otter:身份必须提供。
fn open_conn(default: Option<&str>) -> Result<Conn, CallError> {
    agent_name(default).and_then(|n| Conn::open(&n))
}

/// 从参数里取出 `--agent <名字>`(任何位置),返回剩下的参数。
fn take_agent(args: &[String]) -> Vec<String> {
    let mut rest = Vec::with_capacity(args.len());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--agent" {
            if let Some(n) = it.next().map(|n| n.trim()).filter(|n| !n.is_empty()) {
                let _ = AGENT.set(n.to_string());
            }
        } else {
            rest.push(a.clone());
        }
    }
    rest
}

/// 一条到运行中 Otter 的连接。
pub struct Conn {
    rd: BufReader<SyncStream>,
    wr: SyncStream,
    next: u64,
}

pub struct CallError {
    pub kind: String,
    pub message: String,
}

impl Conn {
    pub fn open(client_name: &str) -> Result<Self, CallError> {
        let wr = crate::ipc::connect().map_err(|_| CallError { kind: "not_running".into(), message: "Otter is not running; open Otter first".into() })?;
        let rd = BufReader::new(wr.try_clone().map_err(|e| CallError { kind: "internal".into(), message: e.to_string() })?);
        let mut c = Self { rd, wr, next: 1 };
        c.call("hello", json!({ "client_name": client_name, "parent_exe": crate::ipc::parent_exe() }))?;
        Ok(c)
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let io = |e: std::io::Error| CallError { kind: "not_running".into(), message: format!("lost the connection to Otter: {e}") };
        let id = self.next;
        self.next += 1;
        let mut line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        line.push('\n');
        self.wr.write_all(line.as_bytes()).map_err(io)?;
        let mut buf = String::new();
        if self.rd.read_line(&mut buf).map_err(io)? == 0 {
            return Err(CallError { kind: "not_running".into(), message: "Otter has quit".into() });
        }
        let v: Value = serde_json::from_str(&buf).map_err(|e| CallError { kind: "internal".into(), message: format!("response is not valid JSON: {e}") })?;
        if let Some(e) = v.get("error") {
            let kind = e.pointer("/data/kind").and_then(Value::as_str).unwrap_or("internal").to_string();
            let mut message = e.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(r) = e.pointer("/data/reason").and_then(Value::as_str) {
                message = format!("{message}(reason: {r})");
            }
            return Err(CallError { kind, message });
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// `otter nodes [--group <名字或id>]`
fn run_nodes(args: &[String]) -> i32 {
    let mut params = Value::Null;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--group" => match it.next() {
                // 纯数字按分组 id,否则按名字
                Some(g) => params = json!({ "group": g.parse::<i64>().map_or_else(|_| json!(g), |n| json!(n)) }),
                None => {
                    eprintln!("--group needs a group name or id");
                    return 2;
                }
            },
            other => {
                eprintln!("unknown argument {other}\nusage: otter nodes [--group <name|id>]");
                return 2;
            }
        }
    }
    match open_conn(None).and_then(|mut c| c.call("list_nodes", params)) {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("{}: {}", e.kind, e.message);
            if e.kind == "not_running" { 3 } else { 1 }
        }
    }
}

fn query_help(cmd: &str) -> &'static str {
    match cmd {
        "nodes" => "usage: otter nodes [--group <name|id>]\nList nodes as JSON: id, name, host/user/port, group, tags, OS, created_at, last_connected_at, connected. --group takes a group id when numeric, otherwise a group name.",
        "groups" => "usage: otter groups\nList groups as JSON: id, name, node_count.",
        "sessions" => "usage: otter sessions\nList the sessions open in Otter as JSON: session id, node, state, focused, taken over.",
        _ => "usage: otter current\nThe session the user is looking at, as JSON; null if there is none. Read-only, never switches tabs.",
    }
}

/// `otter <nodes|groups|sessions|current|takeover|release|run|type|key|screen|scroll>`:`args[0]` 是命令名。返回退出码。
pub fn run_ctl(args: &[String]) -> i32 {
    let args = &take_agent(args)[..];
    if args.len() > 1 && args[1..].iter().any(|a| a == "-h" || a == "--help") && matches!(args[0].as_str(), "nodes" | "groups" | "sessions" | "current") {
        println!("{}", query_help(&args[0]));
        return 0;
    }
    let (method, usage) = match args.first().map(String::as_str) {
        Some("nodes") => return run_nodes(&args[1..]),
        Some("groups") => ("list_groups", ""),
        Some("sessions") => ("list_sessions", ""),
        Some("current") => ("get_focus", ""),
        Some(c @ ("takeover" | "release" | "run" | "type" | "key" | "screen" | "scroll" | "sudo")) => return run_leased(c, &args[1..]),
        _ => ("", "usage: otter <nodes|groups|sessions|current|takeover|release|run|type|key|screen|scroll> (see: otter --help)"),
    };
    if method.is_empty() {
        eprintln!("{usage}");
        return 2;
    }
    match open_conn(None).and_then(|mut c| c.call(method, Value::Null)) {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("{}: {}", e.kind, e.message);
            if e.kind == "not_running" { 3 } else { 1 }
        }
    }
}

fn tool(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
    json!({ "name": name, "description": desc, "inputSchema": { "type": "object", "properties": props, "required": required } })
}

fn tools() -> Value {
    let sid = json!({ "type": "integer", "description": "会话 id,来自 list_sessions" });
    json!([
        tool("list_groups", "列出节点分组:id、名字、节点数", json!({}), &[]),
        tool("list_nodes", "列出 Otter 里的服务器节点(不含密码、私钥等秘密)。可用 group 只看某个分组", json!({ "group": { "description": "分组 id(数字)或分组名;不给则列出全部节点", "type": ["integer", "string"] } }), &[]),
        tool("list_sessions", "列出 Otter 里当前打开的会话(标签),含是否被接管", json!({}), &[]),
        tool("get_focus", "用户当前看着的会话;没有则返回 null", json!({}), &[]),
        tool("takeover_begin", "接管一个已连上的终端会话。每次都会弹窗请用户确认;用户随时能中止。之后才能输入、读屏幕", json!({ "session": sid, "reason": { "type": "string", "description": "必填:告诉用户你要做什么" } }), &["session", "reason"]),
        tool("takeover_end", "交还接管。不会向终端发送任何按键", json!({ "session": sid }), &["session"]),
        tool("send_input", "向被接管的终端输入文本。单次最多 8KB。execute 为 true 时在末尾自动按回车执行,不要在 text 里自己带换行,并返回 mark(命令开始的位置,交给 wait_for 的 since)", json!({ "session": sid, "text": { "type": "string" }, "execute": { "type": "boolean", "description": "输入后自动回车执行,默认 false" } }), &["session", "text"]),
        tool("request_sudo", "屏幕上出现 sudo 的密码提示(「[sudo] password for …」「[sudo: authenticate] Password:」之类)后调用。命令行里 `otter run --sudo '<命令>'` 会自动完成这一步:请用户同意由 Otter 代为输入 sudo 密码,你拿不到密码。用户可选「仅这一次 / 10 分钟 / 30 分钟」;授权期内再遇到 sudo 提示,调用即直接代填不再弹窗。屏幕上不是 sudo 提示会被拒绝(no_sudo_prompt);用户拒绝返回 sudo_denied。成功后用 wait_for 等命令继续", json!({ "session": sid, "reason": { "type": "string", "description": "必填:告诉用户为什么需要提权、要执行什么" } }), &["session", "reason"]),
        tool("send_key", "向被接管的终端发送一个按键:enter、tab、esc、up、down、left、right、home、end、pageup、pagedown、backspace、delete、space、ctrl+<字母>", json!({ "session": sid, "key": { "type": "string" } }), &["session", "key"]),
        tool("read_screen", "读取被接管终端当前屏幕的文本和光标位置", json!({ "session": sid }), &["session"]),
        tool("read_scrollback", "读取被接管终端最近的 N 行(含已滚出屏幕的),最多 500", json!({ "session": sid, "lines": { "type": "integer" } }), &["session"]),
        tool("wait_for", "等到屏幕出现匹配 pattern(正则)、或静默 idle_ms 毫秒、或超时,然后返回屏幕。执行命令的推荐用法:send_input(execute:true) 拿到 mark,再 wait_for 带上 since=mark,读返回的 output", json!({ "session": sid, "pattern": { "type": "string" }, "idle_ms": { "type": "integer" }, "timeout_ms": { "type": "integer", "description": "默认 10000,最大 120000" }, "since": { "type": "object", "description": "send_input(execute:true) 返回的 mark。带上它,返回里多一个 output:这条命令之后的新输出(含命令回显和结束时的提示符),pattern 也只在新输出里匹配" } }), &["session"]),
    ])
}

/// `otter mcp`:stdio 上的 MCP 服务器。返回退出码。
pub fn run_mcp(args: &[String]) -> i32 {
    take_agent(args);
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut client_name: Option<String> = None;
    let mut conn: Option<Conn> = None;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => {
                if let Some(n) = params.pointer("/clientInfo/name").and_then(Value::as_str) {
                    client_name = Some(n.to_string());
                }
                let ver = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2024-11-05");
                json!({ "protocolVersion": ver, "capabilities": { "tools": {} }, "serverInfo": { "name": "otter", "version": env!("CARGO_PKG_VERSION") } })
            }
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tools() }),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let known = tools().as_array().is_some_and(|a| a.iter().any(|t| t["name"] == name));
                let r = if !known {
                    Err(CallError { kind: "bad_request".into(), message: format!("未知工具 {name}") })
                } else {
                    if conn.is_none() {
                        match open_conn(client_name.as_deref()) {
                            Ok(c) => conn = Some(c),
                            Err(e) => {
                                reply(&mut out, id, tool_result(Err(e)));
                                continue;
                            }
                        }
                    }
                    let r = conn.as_mut().map(|c| c.call(name, args)).unwrap_or(Ok(Value::Null));
                    if r.as_ref().is_err_and(|e| e.kind == "not_running") {
                        conn = None;
                    }
                    r
                };
                tool_result(r)
            }
            // 通知(没有 id)不回
            _ if id.is_none() => continue,
            _ => {
                let _ = writeln!(out, "{}", json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "method not found" } }));
                let _ = out.flush();
                continue;
            }
        };
        reply(&mut out, id, result);
    }
    0
}

fn reply(out: &mut std::io::Stdout, id: Option<Value>, result: Value) {
    if let Some(id) = id {
        let _ = writeln!(out, "{}", json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        let _ = out.flush();
    }
}

fn tool_result(r: Result<Value, CallError>) -> Value {
    match r {
        Ok(v) => {
            // 文本类结果直接给文本,其余给 JSON
            let text = match v.get("text").and_then(Value::as_str) {
                Some(t) if v.as_object().is_some_and(|o| o.len() == 1) => t.to_string(),
                _ => serde_json::to_string_pretty(&v).unwrap_or_default(),
            };
            json!({ "content": [{ "type": "text", "text": text }] })
        }
        Err(e) => json!({ "isError": true, "content": [{ "type": "text", "text": format!("{}: {}", e.kind, e.message) }] }),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn with_sudo_prepends_only_when_missing() {
        assert_eq!(super::with_sudo("systemctl restart nginx"), "sudo systemctl restart nginx");
        assert_eq!(super::with_sudo("sudo ls /root"), "sudo ls /root");
        assert_eq!(super::with_sudo("cd /x && sudo ls"), "cd /x && sudo ls");
        assert_eq!(super::with_sudo("echo pseudo-sudoku"), "sudo echo pseudo-sudoku");
    }

    use super::{CallError, tool_result, tools};

    #[test]
    fn tool_list_has_no_exec_and_all_require_valid_schema() {
        let t = tools();
        let names: Vec<&str> = t.as_array().unwrap().iter().filter_map(|x| x["name"].as_str()).collect();
        assert!(names.contains(&"takeover_begin") && names.contains(&"read_screen"));
        assert!(!names.contains(&"exec"));
        for x in t.as_array().unwrap() {
            assert_eq!(x["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn errors_become_is_error_results() {
        let r = tool_result(Err(CallError { kind: "takeover_denied".into(), message: "x".into() }));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().starts_with("takeover_denied"));
    }
}

fn print_ok(v: &Value) {
    match v.get("text").and_then(Value::as_str).or_else(|| v.get("output").and_then(Value::as_str)).or_else(|| v.get("screen").and_then(Value::as_str)) {
        Some(t) => println!("{t}"),
        None => println!("{}", serde_json::to_string(v).unwrap_or_default()),
    }
}

const ONCE_HELP: &str = "Taking over and releasing are separate commands: take over once, run as many commands as you like, release at the end.
The takeover is not tied to the command that created it (a token is kept under leases/ in Otter's data directory), so a failing command does not drop it.

Usage:
  otter takeover [options]          ask to take over a session (Otter shows a confirmation); stays held on success
  otter release [options]           hand the session back (sends no Ctrl+C)
  otter run [options] <command>     type a command, press Enter, wait for output, print it (needs a takeover)
  otter type [options] <text>       type text without pressing Enter
  otter key [options] <key>         press a key: enter tab esc up down ctrl+c ...
  otter screen [options]            print the current screen
  otter scroll [options] [lines]    print recent scrollback, 100 lines by default
Options:
  --session <id>     session id; default is the session the user is looking at (see: otter sessions)
  --reason <text>    required for takeover, run --once, run --sudo and sudo: what you are doing; shown to the user in the confirmation
  --wait <regex>     run: wait until the new output matches (default: wait until output goes quiet)
  --idle <ms>        run: how long output must stay quiet, default 1000
  --timeout <ms>     run: longest wait, default 10000
  --agent <name>     required (every command): which agent you are, shown in the confirmation, overlay, tab tooltip and log; or set OTTER_AGENT
  --sudo             run: the command needs root; Otter prepends `sudo` if the command has none, and Otter asks the user once to approve typing the sudo password (you never see it)
  --once             no persistent takeover: take over, act and release within this one command (asks every time)
A takeover ends after 30 minutes, or after 30 idle minutes; the user can stop it in Otter at any time.";

// ---- 持久接管的令牌:一条命令一个连接,令牌记在本机,下一条命令凭它继续 ----

fn lease_path(sid: u64) -> Option<std::path::PathBuf> {
    crate::ipc::data_dir().map(|d| d.join("leases").join(sid.to_string()))
}

fn lease_read(sid: u64) -> Option<String> {
    std::fs::read_to_string(lease_path(sid)?).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn lease_write(sid: u64, token: &str) {
    let Some(p) = lease_path(sid) else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&p, token).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
    }
}

fn lease_clear(sid: u64) {
    if let Some(p) = lease_path(sid) {
        let _ = std::fs::remove_file(p);
    }
}

/// `--sudo` 时命令里没有 sudo 就在最前面补一个;已经带了就原样用(用户/agent 自己写的位置优先)。
fn with_sudo(cmd: &str) -> String {
    let has = cmd.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')).any(|w| w == "sudo");
    if has { cmd.to_string() } else { format!("sudo {}", cmd.trim_start()) }
}

/// `wait_for` 返回的屏幕最后一行像不像密码提示(含 password / 密码,冒号结尾)。服务端收到 `request_sudo` 时还会再看一遍。
fn at_password_prompt(v: &Value) -> bool {
    let text = v.get("screen").and_then(Value::as_str).or_else(|| v.get("output").and_then(Value::as_str)).unwrap_or("");
    text.lines().rev().find(|l| !l.trim().is_empty()).is_some_and(|l| { let l = l.trim().to_lowercase(); (l.ends_with(':') || l.ends_with('：')) && (l.contains("password") || l.contains("密码")) })
}

fn fail(e: &CallError) -> i32 {
    eprintln!("{}: {}", e.kind, e.message);
    match e.kind.as_str() {
        "not_running" => 3,
        "takeover_denied" => 4,
        _ => 1,
    }
}

/// `otter takeover|release|run|type|key|screen|scroll`。
fn run_leased(cmd_name: &str, args: &[String]) -> i32 {
    let (mut sid, mut reason, mut pattern, mut idle, mut timeout, mut once, mut sudo) = (None::<u64>, String::new(), None::<String>, 1000u64, 10_000u64, false, false);
    let mut rest: Vec<&str> = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => sid = it.next().and_then(|v| v.parse().ok()),
            "--reason" => reason = it.next().cloned().unwrap_or_default(),
            "--wait" => pattern = it.next().cloned(),
            "--idle" => idle = it.next().and_then(|v| v.parse().ok()).unwrap_or(idle),
            "--timeout" => timeout = it.next().and_then(|v| v.parse().ok()).unwrap_or(timeout),
            "--once" => once = true,
            "--sudo" => sudo = true,
            "-h" | "--help" => {
                println!("{ONCE_HELP}");
                return 0;
            }
            other => rest.push(other),
        }
    }
    let text = rest.join(" ");
    if matches!(cmd_name, "run" | "type" | "key") && text.is_empty() {
        eprintln!("{ONCE_HELP}");
        return 2;
    }
    // 接管和提权都必须说明要做什么,这句话会显示在用户的确认框里
    let needs_reason = cmd_name == "takeover" || cmd_name == "sudo" || (cmd_name == "run" && (once || sudo));
    if needs_reason && reason.trim().is_empty() {
        eprintln!("bad_request: 必须用 --reason <说明> 告诉用户你要做什么(显示在确认框里)");
        return 2;
    }
    let mut c = match open_conn(None) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let sid = match sid {
        Some(s) => s,
        None => match c.call("get_focus", Value::Null).map(|v| v.get("session").and_then(Value::as_u64)) {
            Ok(Some(s)) => s,
            Ok(None) => {
                eprintln!("no session is open in Otter; pick one with --session (see: otter sessions)");
                return 1;
            }
            Err(e) => return fail(&e),
        },
    };

    // 申请接管:成功后把令牌记下来,命令结束也不放手
    if cmd_name == "takeover" {
        let token = lease_read(sid);
        let mut p = json!({ "session": sid, "lease": true, "reason": reason });
        if let Some(t) = &token {
            p["token"] = json!(t);
        }
        return match c.call("takeover_begin", p) {
            Ok(v) => {
                if let Some(l) = v.get("lease").and_then(Value::as_str) {
                    lease_write(sid, l);
                }
                println!("{}", json!({ "session": sid, "taken": true, "granted": v.get("granted"), "already": v.get("already") }));
                0
            }
            Err(e) => fail(&e),
        };
    }
    if cmd_name == "release" {
        let token = lease_read(sid);
        let r = c.call("takeover_end", json!({ "session": sid, "token": token }));
        lease_clear(sid);
        return match r {
            Ok(_) => {
                println!("{}", json!({ "session": sid, "released": true }));
                0
            }
            // 已经不在接管了(被用户停止、超时、App 重启)也算交还完成,只是提示一下
            Err(e) if matches!(e.kind.as_str(), "takeover_required" | "takeover_revoked" | "no_such_session") => {
                println!("{}", json!({ "session": sid, "released": false, "note": e.message }));
                0
            }
            Err(e) => fail(&e),
        };
    }

    // 其余命令:凭令牌操作;--once 时就地接管、结束时交还
    let token = if once {
        let begin = json!({ "session": sid, "reason": reason });
        if let Err(e) = c.call("takeover_begin", begin) {
            return fail(&e);
        }
        None
    } else {
        match lease_read(sid) {
            Some(t) => Some(t),
            None => {
                eprintln!("takeover_required: session {sid} is not taken over; run: otter takeover --session {sid}");
                return 1;
            }
        }
    };
    let with = |mut v: Value| {
        v["session"] = json!(sid);
        if let Some(t) = &token {
            v["token"] = json!(t);
        }
        v
    };
    let result = (|| match cmd_name {
        "screen" => c.call("read_screen", with(json!({}))),
        "scroll" => c.call("read_scrollback", with(json!({ "lines": rest.first().and_then(|n| n.parse::<u64>().ok()).unwrap_or(100) }))),
        "type" => c.call("send_input", with(json!({ "text": text, "execute": false }))),
        "key" => c.call("send_key", with(json!({ "key": text }))),
        "sudo" => c.call("request_sudo", with(json!({ "reason": reason }))),
        _ => {
            let text = if sudo { with_sudo(&text) } else { text.clone() };
            let sent = c.call("send_input", with(json!({ "text": text, "execute": true })))?;
            let wait = || {
                let mut w = with(json!({ "timeout_ms": timeout }));
                // 带上命令开始的位置,返回的 output 就只有这条命令之后的新内容
                if let Some(m) = sent.get("mark") {
                    w["since"] = m.clone();
                }
                match &pattern {
                    Some(p) => w["pattern"] = json!(p),
                    None => w["idle_ms"] = json!(idle),
                }
                w
            };
            let mut r = c.call("wait_for", wait())?;
            // sudo 停在固定提示语上:请用户同意,Otter 代填密码,再接着等命令跑完
            for _ in 0..3 {
                if !sudo || !at_password_prompt(&r) {
                    break;
                }
                c.call("request_sudo", with(json!({ "reason": reason })))?;
                r = c.call("wait_for", wait())?;
            }
            Ok(r)
        }
    })();
    if once {
        let _ = c.call("takeover_end", json!({ "session": sid }));
    }
    match result {
        Ok(v) => {
            print_ok(&v);
            if v.get("timeout").and_then(Value::as_bool) == Some(true) {
                eprintln!("(timed out waiting; the output above is what was on screen)");
            }
            0
        }
        Err(e) => {
            // 接管已经没了:令牌作废,告诉 agent 重新接管
            if matches!(e.kind.as_str(), "takeover_required" | "takeover_revoked" | "no_such_session") {
                lease_clear(sid);
                eprintln!("{}: {} (token discarded; to continue run: otter takeover --session {sid})", e.kind, e.message);
                return 1;
            }
            fail(&e)
        }
    }
}
