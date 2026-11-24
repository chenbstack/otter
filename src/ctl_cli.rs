//! `otter ctl …` 和 `otter mcp`:连本地 socket 的薄客户端,不启动界面。
//! MCP 是主入口(stdio,按行分隔的 JSON-RPC),CLI 只给人和脚本查询用。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use serde_json::{Value, json};

/// 一条到运行中 Otter 的连接。
pub struct Conn {
    rd: BufReader<UnixStream>,
    wr: UnixStream,
    next: u64,
}

pub struct CallError {
    pub kind: String,
    pub message: String,
}

impl Conn {
    pub fn open(client_name: &str) -> Result<Self, CallError> {
        let path = crate::ctl::sock_path().ok_or_else(|| CallError { kind: "not_running".into(), message: "找不到数据目录".into() })?;
        let wr = UnixStream::connect(&path).map_err(|_| CallError { kind: "not_running".into(), message: "Otter 没有在运行,请先打开 Otter".into() })?;
        let rd = BufReader::new(wr.try_clone().map_err(|e| CallError { kind: "internal".into(), message: e.to_string() })?);
        let mut c = Self { rd, wr, next: 1 };
        c.call("hello", json!({ "client_name": client_name, "parent_exe": parent_exe() }))?;
        Ok(c)
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let io = |e: std::io::Error| CallError { kind: "not_running".into(), message: format!("和 Otter 的连接断了: {e}") };
        let id = self.next;
        self.next += 1;
        let mut line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        line.push('\n');
        self.wr.write_all(line.as_bytes()).map_err(io)?;
        let mut buf = String::new();
        if self.rd.read_line(&mut buf).map_err(io)? == 0 {
            return Err(CallError { kind: "not_running".into(), message: "Otter 已退出".into() });
        }
        let v: Value = serde_json::from_str(&buf).map_err(|e| CallError { kind: "internal".into(), message: format!("响应不是合法 JSON: {e}") })?;
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

/// 父进程的可执行路径,只用来在确认框里给用户看。
fn parent_exe() -> String {
    let ppid = std::os::unix::process::parent_id();
    std::process::Command::new("ps").args(["-o", "comm=", "-p", &ppid.to_string()]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

/// `otter ctl <nodes|sessions|focus>`。返回退出码。
pub fn run_ctl(args: &[String]) -> i32 {
    let (method, usage) = match args.first().map(String::as_str) {
        Some("nodes") => ("list_nodes", ""),
        Some("sessions") => ("list_sessions", ""),
        Some("focus") => ("get_focus", ""),
        Some("shell") => return run_shell(&args[1..]),
        Some("run") => return run_once(&args[1..]),
        Some("screen") => return run_once(&args[..]),
        _ => ("", "用法: otter ctl <nodes|sessions|focus|shell [会话id]|run [选项] <命令>|screen [选项]>"),
    };
    if method.is_empty() {
        eprintln!("{usage}");
        return 2;
    }
    match Conn::open("otter-cli").and_then(|mut c| c.call(method, Value::Null)) {
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
        tool("list_nodes", "列出 Otter 里的服务器节点(不含密码、私钥等秘密)", json!({}), &[]),
        tool("list_sessions", "列出 Otter 里当前打开的会话(标签),含是否被接管", json!({}), &[]),
        tool("get_focus", "用户当前看着的会话;没有则返回 null", json!({}), &[]),
        tool("takeover_begin", "接管一个已连上的终端会话。每次都会弹窗请用户确认;用户随时能中止。之后才能输入、读屏幕", json!({ "session": sid, "reason": { "type": "string", "description": "告诉用户你要做什么" } }), &["session"]),
        tool("takeover_end", "交还接管。不会向终端发送任何按键", json!({ "session": sid }), &["session"]),
        tool("send_input", "向被接管的终端输入文本。单次最多 8KB。execute 为 true 时在末尾自动按回车执行,不要在 text 里自己带换行", json!({ "session": sid, "text": { "type": "string" }, "execute": { "type": "boolean", "description": "输入后自动回车执行,默认 false" } }), &["session", "text"]),
        tool("send_key", "向被接管的终端发送一个按键:enter、tab、esc、up、down、left、right、home、end、pageup、pagedown、backspace、delete、space、ctrl+<字母>", json!({ "session": sid, "key": { "type": "string" } }), &["session", "key"]),
        tool("read_screen", "读取被接管终端当前屏幕的文本和光标位置", json!({ "session": sid }), &["session"]),
        tool("read_scrollback", "读取被接管终端最近的 N 行(含已滚出屏幕的),最多 500", json!({ "session": sid, "lines": { "type": "integer" } }), &["session"]),
        tool("wait_for", "等到屏幕出现匹配 pattern(正则)、或静默 idle_ms 毫秒、或超时,然后返回屏幕", json!({ "session": sid, "pattern": { "type": "string" }, "idle_ms": { "type": "integer" }, "timeout_ms": { "type": "integer", "description": "默认 10000,最大 120000" } }), &["session"]),
    ])
}

/// `otter mcp`:stdio 上的 MCP 服务器。返回退出码。
pub fn run_mcp() -> i32 {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut client_name = String::from("mcp-client");
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
                    client_name = n.to_string();
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
                        match Conn::open(&client_name) {
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

const SHELL_HELP: &str = "命令:
  begin [原因]        申请接管(会在 Otter 里弹确认)
  run <命令>          输入并自动回车执行
  type <文本>         只输入,不回车
  key <按键>          enter / tab / esc / up / ctrl+c ...
  screen              读当前屏幕
  scroll [行数]       读最近的行,默认 100
  wait <正则> [毫秒]   等屏幕出现匹配,默认 10000
  idle <毫秒> [超时]   等屏幕静默
  stop                交还接管(不发 Ctrl+C)
  help / quit         帮助 / 退出(退出也会交还)";

fn print_ok(v: &Value) {
    match v.get("text").and_then(Value::as_str).or_else(|| v.get("screen").and_then(Value::as_str)) {
        Some(t) => println!("{t}"),
        None => println!("{}", serde_json::to_string(v).unwrap_or_default()),
    }
}

/// `otter ctl shell [会话id]`:交互模式。一个进程里保持连接,所以接管能跨多条命令。
fn run_shell(args: &[String]) -> i32 {
    let mut c = match Conn::open("otter-cli") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}: {}", e.kind, e.message);
            return 3;
        }
    };
    let sid = match args.first().and_then(|a| a.parse::<u64>().ok()) {
        Some(s) => s,
        None => match c.call("get_focus", Value::Null) {
            Ok(v) => match v.get("session").and_then(Value::as_u64) {
                Some(s) => {
                    println!("当前焦点会话: {s} ({})", v.get("node_name").and_then(Value::as_str).unwrap_or("?"));
                    s
                }
                None => {
                    eprintln!("Otter 里当前没有可用的会话,请指定会话 id(otter ctl sessions)");
                    return 1;
                }
            },
            Err(e) => {
                eprintln!("{}: {}", e.kind, e.message);
                return 1;
            }
        },
    };
    println!("会话 {sid}。输入 help 查看命令。");
    let stdin = std::io::stdin();
    let mut held = false;
    loop {
        print!("otter[{sid}]{}> ", if held { "*" } else { "" });
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim_end_matches(['\n', '\r']);
        let (cmd, rest) = line.trim_start().split_once(' ').unwrap_or((line.trim(), ""));
        let rest = rest.trim_start();
        let (method, params) = match cmd {
            "" => continue,
            "help" | "?" => {
                println!("{SHELL_HELP}");
                continue;
            }
            "quit" | "exit" | "q" => break,
            "begin" => ("takeover_begin", json!({ "session": sid, "reason": rest })),
            "stop" => ("takeover_end", json!({ "session": sid })),
            "run" => ("send_input", json!({ "session": sid, "text": rest, "execute": true })),
            "type" => ("send_input", json!({ "session": sid, "text": rest })),
            "key" => ("send_key", json!({ "session": sid, "key": rest })),
            "screen" => ("read_screen", json!({ "session": sid })),
            "scroll" => ("read_scrollback", json!({ "session": sid, "lines": rest.parse::<u64>().unwrap_or(100) })),
            "wait" => {
                let (pat, ms) = rest.rsplit_once(' ').filter(|(_, m)| m.parse::<u64>().is_ok()).map_or((rest, 10_000), |(p, m)| (p, m.parse().unwrap_or(10_000)));
                ("wait_for", json!({ "session": sid, "pattern": pat, "timeout_ms": ms }))
            }
            "idle" => {
                let mut it = rest.split_whitespace().filter_map(|x| x.parse::<u64>().ok());
                ("wait_for", json!({ "session": sid, "idle_ms": it.next().unwrap_or(1000), "timeout_ms": it.next().unwrap_or(10_000) }))
            }
            _ => {
                println!("不认识的命令 {cmd},输入 help 查看");
                continue;
            }
        };
        match c.call(method, params) {
            Ok(v) => {
                held = match method {
                    "takeover_begin" => true,
                    "takeover_end" => false,
                    _ => held,
                };
                print_ok(&v);
            }
            Err(e) => {
                if e.kind == "takeover_revoked" || e.kind == "takeover_denied" {
                    held = false;
                }
                eprintln!("{}: {}", e.kind, e.message);
                if e.kind == "not_running" {
                    return 3;
                }
            }
        }
    }
    if held {
        let _ = c.call("takeover_end", json!({ "session": sid }));
    }
    0
}

const ONCE_HELP: &str = "用法:
  otter ctl run [选项] <命令>    接管、输入并回车执行、等输出、打印屏幕、交还
  otter ctl screen [选项]        接管、打印当前屏幕、交还
选项:
  --session <id>   会话 id,默认用户当前焦点的会话
  --reason <文字>  显示在确认框里的原因
  --wait <正则>    等屏幕出现匹配(默认改为等静默)
  --idle <毫秒>    等屏幕静默这么久,默认 1000
  --timeout <毫秒> 最长等待,默认 10000
每次都会在 Otter 里弹确认;命令结束时交还接管,不发 Ctrl+C。";

/// `otter ctl run|screen`:一次性命令。一条命令内完成接管 → 操作 → 交还。
fn run_once(args: &[String]) -> i32 {
    let screen_only = args.first().is_some_and(|a| a == "screen");
    let args = if screen_only { &args[1..] } else { args };
    let (mut sid, mut reason, mut pattern, mut idle, mut timeout) = (None::<u64>, String::new(), None::<String>, 1000u64, 10_000u64);
    let mut cmd: Vec<&str> = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" => sid = it.next().and_then(|v| v.parse().ok()),
            "--reason" => reason = it.next().cloned().unwrap_or_default(),
            "--wait" => pattern = it.next().cloned(),
            "--idle" => idle = it.next().and_then(|v| v.parse().ok()).unwrap_or(idle),
            "--timeout" => timeout = it.next().and_then(|v| v.parse().ok()).unwrap_or(timeout),
            "-h" | "--help" => {
                println!("{ONCE_HELP}");
                return 0;
            }
            other => cmd.push(other),
        }
    }
    let cmd = cmd.join(" ");
    if !screen_only && cmd.is_empty() {
        eprintln!("{ONCE_HELP}");
        return 2;
    }
    let fail = |e: CallError| {
        eprintln!("{}: {}", e.kind, e.message);
        match e.kind.as_str() {
            "not_running" => 3,
            "takeover_denied" => 4,
            _ => 1,
        }
    };
    let mut c = match Conn::open("otter-cli") {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let sid = match sid {
        Some(s) => s,
        None => match c.call("get_focus", Value::Null).map(|v| v.get("session").and_then(Value::as_u64)) {
            Ok(Some(s)) => s,
            Ok(None) => {
                eprintln!("Otter 里当前没有可用的会话,用 --session 指定(otter ctl sessions)");
                return 1;
            }
            Err(e) => return fail(e),
        },
    };
    if let Err(e) = c.call("takeover_begin", json!({ "session": sid, "reason": if reason.is_empty() { cmd.clone() } else { reason } })) {
        return fail(e);
    }
    let result = (|| {
        if screen_only {
            return c.call("read_screen", json!({ "session": sid }));
        }
        c.call("send_input", json!({ "session": sid, "text": cmd, "execute": true }))?;
        let mut w = json!({ "session": sid, "timeout_ms": timeout });
        match &pattern {
            Some(p) => w["pattern"] = json!(p),
            None => w["idle_ms"] = json!(idle),
        }
        c.call("wait_for", w)
    })();
    let _ = c.call("takeover_end", json!({ "session": sid }));
    match result {
        Ok(v) => {
            print_ok(&v);
            if v.get("timeout").and_then(Value::as_bool) == Some(true) {
                eprintln!("(等待超时,以上是当时的屏幕)");
            }
            0
        }
        Err(e) => fail(e),
    }
}
