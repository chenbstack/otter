//! agent 接管用户当前的终端会话:授权确认、接管状态、遮罩、输入路由、读屏幕。
//! 接口和规则见 docs/modules/ctl.md,决策见 docs/decisions/0009-agent-interface.md。

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::input::InputState;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::ctl::{Client, Reply, RpcError};
use crate::logs::{self, Level};
use crate::mock::NodeId;
use crate::terminal::ssh::Status;
use crate::workspace::{NodeMode, TabKind};
use crate::ui::*;
use crate::{Home, icon};

/// 超过这么久没有任何调用,自动释放(`wait_for` 等待期间不算)。目前与 `MAX_DURATION` 相同
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// 单次接管的最长时长
pub const MAX_DURATION: Duration = Duration::from_secs(30 * 60);
/// `send_input` 单次最多字节
pub const INPUT_MAX: usize = 8 * 1024;
/// 单次返回给 agent 的文本最多字节
pub const OUTPUT_MAX: usize = 64 * 1024;
/// `read_scrollback` 每次最多行数
pub const SCROLLBACK_MAX: usize = 500;
const WAIT_DEFAULT_MS: u64 = 10_000;
const WAIT_MAX_MS: u64 = 120_000;
const WAIT_POLL_MS: u64 = 150;
const LOG_TEXT_MAX: usize = 200;

/// 一个标签上正在进行的接管。
pub struct Takeover {
    pub client_id: u64,
    pub client_name: String,
    pub started: Instant,
    pub last_call: Instant,
    /// 遮罩上显示的最近一次动作,如「输入:df -h」
    pub last_action: String,
    /// 正在 `wait_for` 里等待,此时不算空闲
    pub waiting: bool,
    /// 持久接管的令牌:接管不随连接断开而结束,凭令牌可以从别的连接继续操作(命令行一条命令一个连接)
    pub lease: Option<String>,
    /// 用户同意过的提权授权:到期前,屏幕上出现 sudo 密码提示时 agent 可以不再询问直接请 Otter 代填
    pub sudo: Option<SudoGrant>,
}

/// 一次「N 分钟内同意」的提权授权。随接管结束而消失,不落盘。
pub struct SudoGrant {
    pub until: Instant,
    /// 节点没保存密码时,用户在确认框里输入的密码(只在内存里)
    password: Option<Zeroizing<String>>,
}

/// 提权授权的时长选项。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SudoScope {
    Once,
    Min10,
    Min30,
}

impl SudoScope {
    fn window(self) -> Option<Duration> {
        match self {
            Self::Once => None,
            Self::Min10 => Some(Duration::from_secs(10 * 60)),
            Self::Min30 => Some(Duration::from_secs(30 * 60)),
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Min10 => "10m",
            Self::Min30 => "30m",
        }
    }
}

/// 等用户确认的提权请求(同一时间只有一个)。
pub struct SudoPrompt {
    pub(crate) tab_id: u64,
    client: Arc<Client>,
    reason: String,
    /// 屏幕上的 sudo 提示行,给用户看
    line: String,
    /// 节点没保存密码时,让用户输入
    input: Option<Entity<InputState>>,
    reply: Reply,
}

/// 这一行像不像密码提示:含 password / 密码,以冒号结尾(`[sudo] password for chen:`、`[sudo: authenticate] Password:`、`[sudo] chen 的密码:`)。
/// 不去枚举 sudo 各版本、各语言的写法;最终要不要填由用户在确认框里看着这一行决定。
pub fn is_sudo_prompt(line: &str) -> bool {
    let l = line.trim().to_lowercase();
    (l.ends_with(':') || l.ends_with('：')) && (l.contains("password") || l.contains("密码")) && l.len() <= 200
}

impl Takeover {
    /// 这次调用是不是接管的持有者:同一个连接,或者带着正确的令牌。
    fn owned_by(&self, client: &Client, params: &Value) -> bool {
        self.client_id == client.id || self.lease.as_deref().is_some_and(|l| params.get("token").and_then(Value::as_str) == Some(l))
    }
}

fn new_lease() -> String {
    use rand::RngExt as _;
    let b: [u8; 16] = rand::rng().random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 等用户确认的接管申请。
pub struct Prompt {
    pub(crate) tab_id: u64,
    client: Arc<Client>,
    reason: String,
    lease: bool,
    reply: Reply,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Decision {
    Deny,
    Once,
    /// 24 小时内不再询问
    Day,
    Always,
}

fn alog(msg: impl Into<String>) {
    logs::push(Level::Info, "Agent", msg);
}

fn clip(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

/// 截到不超过 `OUTPUT_MAX` 字节(按字符边界),超出时在末尾标注。
pub fn limit_output(mut s: String) -> String {
    if s.len() <= OUTPUT_MAX {
        return s;
    }
    let mut cut = OUTPUT_MAX;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str("\n…(输出过长,已截断)");
    s
}

/// `send_key` 的键名 → 要写进终端的字节。
pub fn key_bytes(key: &str) -> Option<Vec<u8>> {
    let k = key.trim().to_lowercase();
    let b: &[u8] = match k.as_str() {
        "enter" | "return" => b"\r",
        "tab" => b"\t",
        "esc" | "escape" => b"\x1b",
        "backspace" => b"\x7f",
        "delete" => b"\x1b[3~",
        "space" => b" ",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        _ => {
            let letter = k.strip_prefix("ctrl+")?;
            let mut it = letter.chars();
            let (ch, rest) = (it.next()?, it.next());
            if rest.is_some() || !ch.is_ascii_alphabetic() {
                return None;
            }
            return Some(vec![(ch as u8) & 0x1f]);
        }
    };
    Some(b.to_vec())
}

/// 「命令从这里开始」的标记:光标所在行的绝对行号,加上这一行当时的内容(用来校验行号没有因为回滚被挤掉而错位)。
/// `send_input` 带 `execute` 时返回它,`wait_for` 的 `since` 收回它,返回这条命令之后的新输出。
fn output_mark(snap: &gpui_xterm::TextSnapshot) -> Value {
    let anchor = snap.lines.get(snap.screen_start + snap.cursor_line).cloned().unwrap_or_default();
    json!({ "row": snap.history_total + snap.cursor_line, "anchor": anchor })
}

/// 标记之后的新输出(含命令回显和结束时的提示符)。行号对不上(回滚被挤掉、屏幕被清了)时返回 `None`,调用方退回整屏。
fn output_since(snap: &gpui_xterm::TextSnapshot, mark: &Value) -> Option<String> {
    let row = mark.get("row")?.as_u64()? as i64;
    let anchor = mark.get("anchor").and_then(Value::as_str).unwrap_or("");
    // 窗口第一行的绝对行号
    let window_start = snap.history_total as i64 - snap.screen_start as i64;
    let idx = usize::try_from(row - window_start).ok().filter(|i| *i < snap.lines.len())?;
    if !snap.lines[idx].starts_with(anchor) {
        return None;
    }
    Some(trim_trailing_blank(snap.lines[idx..].to_vec()).join("\n"))
}

/// 屏幕文本:去掉末尾的空行。
fn trim_trailing_blank(mut lines: Vec<String>) -> Vec<String> {
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

impl Home {
    fn tab_ix_by_id(&self, id: u64) -> Option<usize> {
        self.workspace.tabs.iter().position(|t| t.id == id)
    }

    pub(crate) fn node_name(&self, id: Option<NodeId>) -> String {
        id.and_then(|i| self.all_nodes().find(|n| n.id == i)).map_or_else(|| "?".into(), |n| n.name.clone())
    }

    /// 处理所有非只读方法(接管相关)。`reply` 一律在这里或它派生的任务里回掉。
    pub(crate) fn takeover_call(&mut self, client: &Arc<Client>, method: &str, params: &Value, reply: Reply, window: &mut Window, cx: &mut Context<Self>) {
        let sid = params.get("session").and_then(Value::as_u64);
        let Some(sid) = sid else {
            let _ = reply.send(Err(RpcError::new("bad_request", "缺少 session 参数")));
            return;
        };
        let Some(ix) = self.tab_ix_by_id(sid) else {
            // 标签已经没了:如果这个客户端刚被中止过,告诉它原因
            let err = match self.take_revoked(client.id, sid, params) {
                Some(r) => RpcError::revoked(r),
                None => RpcError::new("no_such_session", "没有这个会话"),
            };
            let _ = reply.send(Err(err));
            return;
        };
        match method {
            "takeover_begin" => self.takeover_begin(client, ix, params, reply, window, cx),
            "takeover_end" => {
                let mine = self.workspace.tabs[ix].takeover.as_ref().is_some_and(|t| t.owned_by(client, params));
                if mine {
                    self.end_takeover(sid, None, window, cx);
                    let _ = reply.send(Ok(json!({ "ended": true })));
                } else {
                    let _ = reply.send(Err(RpcError::new("takeover_required", "当前没有在接管这个会话")));
                }
            }
            "send_input" | "send_key" | "read_screen" | "read_scrollback" | "wait_for" => self.takeover_op(client, ix, method, params, reply, cx),
            "request_sudo" => self.sudo_request(client, ix, params, reply, window, cx),
            _ => {
                let _ = reply.send(Err(RpcError::new("bad_request", format!("未知方法 {method}"))));
            }
        }
    }

    fn takeover_begin(&mut self, client: &Arc<Client>, ix: usize, params: &Value, reply: Reply, window: &mut Window, cx: &mut Context<Self>) {
        let tab = &self.workspace.tabs[ix];
        let sid = tab.id;
        let TabKind::Node(node) = tab.kind else {
            let _ = reply.send(Err(RpcError::new("bad_request", "只能接管节点的终端标签")));
            return;
        };
        if tab.mode != NodeMode::Terminal || tab.term.is_none() {
            let _ = reply.send(Err(RpcError::new("bad_request", "这个标签不在终端模式")));
            return;
        }
        if tab.term.as_ref().is_some_and(|t| t.handle.status() != Status::Connected) {
            let _ = reply.send(Err(RpcError::new("not_connected", "终端还没有连上")));
            return;
        }
        match &tab.takeover {
            Some(t) if t.owned_by(client, params) => {
                let mut r = json!({ "session": sid, "already": true });
                if let Some(l) = &t.lease {
                    r["lease"] = json!(l);
                }
                let _ = reply.send(Ok(r));
                return;
            }
            Some(_) => {
                let _ = reply.send(Err(RpcError::new("busy", "这个会话已经被另一个 agent 接管")));
                return;
            }
            None => {}
        }
        if self.takeover_prompts.iter().any(|p| p.tab_id == sid) {
            let _ = reply.send(Err(RpcError::new("busy", "这个会话正在等用户确认另一个接管申请")));
            return;
        }
        let reason = clip(params.get("reason").and_then(Value::as_str).unwrap_or("").trim(), 200);
        if reason.is_empty() {
            let _ = reply.send(Err(RpcError::new("bad_request", "必须提供 reason:告诉用户你要做什么")));
            return;
        }
        let lease = params.get("lease").and_then(Value::as_bool).unwrap_or(false);
        let granted = self.store.as_ref().is_some_and(|s| s.grant_has(&client.name, &client.parent_exe, node));
        if granted {
            let l = self.start_takeover(ix, client, &reason, lease, window, cx);
            let mut r = json!({ "session": sid, "granted": "always" });
            if let Some(l) = l {
                r["lease"] = json!(l);
            }
            let _ = reply.send(Ok(r));
            return;
        }
        alog(format!("{} 申请接管 {}", client.name, self.node_name(Some(node))));
        self.takeover_prompts.push_back(Prompt { tab_id: sid, client: client.clone(), reason, lease, reply });
        // 申请要让用户看见:把窗口带到前台,并切到被申请的那个标签
        cx.activate(true);
        self.activate(ix);
        cx.notify();
    }

    /// 开始接管;`lease` 为真时返回令牌。
    fn start_takeover(&mut self, ix: usize, client: &Arc<Client>, reason: &str, lease: bool, window: &mut Window, cx: &mut Context<Self>) -> Option<String> {
        let now = Instant::now();
        let token = lease.then(new_lease);
        let node = self.workspace.tabs[ix].node();
        self.workspace.tabs[ix].takeover = Some(Takeover {
            client_id: client.id,
            client_name: client.name.clone(),
            started: now,
            last_call: now,
            last_action: if reason.is_empty() { "已开始接管".into() } else { clip(reason, 60) },
            waiting: false,
            lease: token.clone(),
            sudo: None,
        });
        alog(format!("{} 开始接管 {}{}", client.name, self.node_name(node), if reason.is_empty() { String::new() } else { format!("(原因:{reason})") }));
        let fh = self.takeover_focus.get_or_insert_with(|| cx.focus_handle()).clone();
        if self.workspace.active == Some(ix) {
            window.focus(&fh, cx);
        }
        cx.notify();
        token
    }

    /// 取出并清掉「接管被结束」的记录:先按连接找,再按令牌找。
    fn take_revoked(&mut self, client_id: u64, sid: u64, params: &Value) -> Option<&'static str> {
        self.takeover_revoked.remove(&(client_id, sid)).or_else(|| {
            let token = params.get("token").and_then(Value::as_str)?;
            self.lease_revoked.remove(token)
        })
    }

    /// 记下一次接管被结束的原因,让 agent 下次调用时知道。
    fn record_revoked(&mut self, t: &Takeover, tab_id: u64, reason: &'static str) {
        self.takeover_revoked.insert((t.client_id, tab_id), reason);
        if let Some(l) = &t.lease {
            if self.lease_revoked.len() >= 64 {
                self.lease_revoked.clear();
            }
            self.lease_revoked.insert(l.clone(), reason);
        }
    }

    /// 结束接管。`revoked` 为 `Some(原因)` 时记下来,agent 下次调用会收到 `takeover_revoked`;
    /// `None` 表示 agent 自己交还。不向终端写任何字节(停止不发 Ctrl+C)。
    pub(crate) fn end_takeover(&mut self, tab_id: u64, revoked: Option<&'static str>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_ix_by_id(tab_id) else { return };
        let Some(t) = self.workspace.tabs[ix].takeover.take() else { return };
        self.drop_sudo_prompt(tab_id);
        let node = self.node_name(self.workspace.tabs[ix].node());
        match revoked {
            Some(r) => {
                self.record_revoked(&t, tab_id, r);
                alog(format!("{} 对 {} 的接管被结束({})", t.client_name, node, revoke_label(r)));
            }
            None => alog(format!("{} 交还了对 {} 的接管", t.client_name, node)),
        }
        self.refocus_terminal(ix, window, cx);
        cx.notify();
    }

    fn refocus_terminal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace.active != Some(ix) {
            return;
        }
        let tab = &self.workspace.tabs[ix];
        if let (NodeMode::Terminal, Some(term)) = (tab.mode, tab.term.as_ref()) {
            let fh = term.view.read(cx).focus_handle().clone();
            window.focus(&fh, cx);
        }
    }

    /// 客户端连接断开:释放它持有的接管(持久接管不释放,凭令牌可以继续),撤掉它排队的申请。
    pub(crate) fn takeover_client_closed(&mut self, client_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.takeover_prompts.retain(|p| p.client.id != client_id);
        if self.sudo_prompt.as_ref().is_some_and(|p| p.client.id == client_id) {
            self.sudo_prompt = None;
        }
        let ids: Vec<u64> = self.workspace.tabs.iter().filter(|t| t.takeover.as_ref().is_some_and(|k| k.client_id == client_id && k.lease.is_none())).map(|t| t.id).collect();
        for id in ids {
            // 客户端已经断开,不需要记「被中止」
            let ix = self.tab_ix_by_id(id);
            if let Some(ix) = ix {
                if let Some(t) = self.workspace.tabs[ix].takeover.take() {
                    alog(format!("{} 断开了连接,接管已结束", t.client_name));
                }
                self.refocus_terminal(ix, window, cx);
            }
        }
        self.takeover_revoked.retain(|(cid, _), _| *cid != client_id);
        cx.notify();
    }

    /// 标签要被关掉了:它上面的接管记成 `tab_closed`,agent 下次调用能知道。
    pub(crate) fn takeover_tab_closing(&mut self, ix: usize) {
        let Some(tab) = self.workspace.tabs.get_mut(ix) else { return };
        let id = tab.id;
        if let Some(t) = tab.takeover.take() {
            alog(format!("{} 的接管随标签关闭而结束", t.client_name));
            self.record_revoked(&t, id, "tab_closed");
        }
        self.takeover_prompts.retain(|p| p.tab_id != id);
        self.drop_sudo_prompt(id);
    }

    /// 用户对确认框的选择。
    pub(crate) fn resolve_takeover_prompt(&mut self, d: Decision, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.takeover_prompts.pop_front() else { return };
        let ix = self.tab_ix_by_id(p.tab_id);
        let node = ix.and_then(|i| self.workspace.tabs[i].node());
        let node_name = self.node_name(node);
        match (d, ix, node) {
            (Decision::Deny, ..) => {
                alog(format!("用户拒绝了 {} 接管 {}", p.client.name, node_name));
                let _ = p.reply.send(Err(RpcError::new("takeover_denied", "用户拒绝了接管")));
            }
            (_, Some(ix), Some(node)) if self.workspace.tabs[ix].takeover.is_none() => {
                if d != Decision::Once {
                    let (expires, what) = if d == Decision::Day { (crate::store::now_secs() + 24 * 3600, "24 小时内允许") } else { (0, "总是允许") };
                    if let Some(Err(e)) = self.store.as_ref().map(|s| s.grant_add(&p.client.name, &p.client.parent_exe, node, expires)) {
                        logs::push(Level::Error, "本地库", format!("保存接管授权失败: {e}"));
                    } else {
                        alog(format!("用户选择{what} {} 接管 {}", p.client.name, node_name));
                    }
                }
                let l = self.start_takeover(ix, &p.client, &p.reason, p.lease, window, cx);
                let mut r = json!({ "session": p.tab_id, "granted": match d { Decision::Always => "always", Decision::Day => "day", _ => "once" } });
                if let Some(l) = l {
                    r["lease"] = json!(l);
                }
                let _ = p.reply.send(Ok(r));
            }
            _ => {
                let _ = p.reply.send(Err(RpcError::new("no_such_session", "会话已经不存在或已被接管")));
            }
        }
        cx.notify();
    }

    fn takeover_op(&mut self, client: &Arc<Client>, ix: usize, method: &str, params: &Value, reply: Reply, cx: &mut Context<Self>) {
        let sid = self.workspace.tabs[ix].id;
        match self.workspace.tabs[ix].takeover.as_mut() {
            Some(t) if t.owned_by(client, params) => {
                t.last_call = Instant::now();
                // 持久接管换了连接:此后这个连接就是持有者
                t.client_id = client.id;
            }
            Some(_) => {
                let _ = reply.send(Err(RpcError::new("busy", "这个会话已经被另一个 agent 接管")));
                return;
            }
            None => {
                let err = match self.take_revoked(client.id, sid, params) {
                    Some(r) => RpcError::revoked(r),
                    None => RpcError::new("takeover_required", "先调用 takeover_begin 接管这个会话"),
                };
                let _ = reply.send(Err(err));
                return;
            }
        }
        let node = self.node_name(self.workspace.tabs[ix].node());
        let Some(term) = self.workspace.tabs[ix].term.as_ref() else {
            let _ = reply.send(Err(RpcError::new("not_connected", "终端还没有连上")));
            return;
        };
        let set_action = |this: &mut Self, s: String| {
            if let Some(t) = this.workspace.tabs[ix].takeover.as_mut() {
                t.last_action = s;
            }
        };
        match method {
            "send_input" => {
                let text = params.get("text").and_then(Value::as_str).unwrap_or("");
                let execute = params.get("execute").and_then(Value::as_bool).unwrap_or(false);
                if text.len() > INPUT_MAX {
                    let _ = reply.send(Err(RpcError::new("bad_request", format!("单次输入最多 {INPUT_MAX} 字节"))));
                    return;
                }
                if term.handle.status() != Status::Connected {
                    let _ = reply.send(Err(RpcError::new("not_connected", "终端已断开")));
                    return;
                }
                // 先记下光标位置,命令的输出从这里开始算
                let mark = execute.then(|| output_mark(&term.view.read(cx).text_snapshot(0)));
                let mut bytes = text.as_bytes().to_vec();
                if execute {
                    bytes.push(b'\r');
                }
                let _ = term.handle.inputter().send(bytes);
                let shown = clip(&text.replace(['\n', '\r'], "⏎"), LOG_TEXT_MAX);
                alog(format!("→ {node} 输入:{shown}{}", if execute { " ⏎" } else { "" }));
                set_action(self, format!("{}:{}", if execute { "执行" } else { "输入" }, clip(&text.replace(['\n', '\r'], " "), 40)));
                let mut r = json!({ "sent": text.len(), "executed": execute });
                if let Some(m) = mark {
                    r["mark"] = m;
                }
                let _ = reply.send(Ok(r));
            }
            "send_key" => {
                let key = params.get("key").and_then(Value::as_str).unwrap_or("");
                let Some(bytes) = key_bytes(key) else {
                    let _ = reply.send(Err(RpcError::new("bad_request", format!("不认识的按键 {key:?}"))));
                    return;
                };
                if term.handle.status() != Status::Connected {
                    let _ = reply.send(Err(RpcError::new("not_connected", "终端已断开")));
                    return;
                }
                let _ = term.handle.inputter().send(bytes);
                alog(format!("→ {node} 按键:{key}"));
                set_action(self, format!("按键:{key}"));
                let _ = reply.send(Ok(json!({ "sent": key })));
            }
            "read_screen" => {
                let snap = term.view.read(cx).text_snapshot(0);
                let (cols, rows) = term.view.read(cx).dimensions();
                let lines = trim_trailing_blank(snap.lines[snap.screen_start..].to_vec());
                set_action(self, "读取屏幕".into());
                let _ = reply.send(Ok(json!({
                    "text": limit_output(lines.join("\n")),
                    "cursor": { "line": snap.cursor_line, "col": snap.cursor_col },
                    "cols": cols, "rows": rows,
                })));
            }
            "read_scrollback" => {
                let n = params.get("lines").and_then(Value::as_u64).unwrap_or(100).clamp(1, SCROLLBACK_MAX as u64) as usize;
                let snap = term.view.read(cx).text_snapshot(SCROLLBACK_MAX);
                let all = trim_trailing_blank(snap.lines);
                let from = all.len().saturating_sub(n);
                set_action(self, format!("读取最近 {n} 行"));
                let _ = reply.send(Ok(json!({ "text": limit_output(all[from..].join("\n")), "lines": all.len() - from })));
            }
            "wait_for" => self.takeover_wait(client.id, ix, params, reply, cx),
            _ => unreachable!(),
        }
        cx.notify();
    }

    /// `wait_for`:每 150ms 看一次屏幕,出现 `pattern`、或静默 `idle_ms`、或超时就回。
    fn takeover_wait(&mut self, client_id: u64, ix: usize, params: &Value, reply: Reply, cx: &mut Context<Self>) {
        let sid = self.workspace.tabs[ix].id;
        let pattern = match params.get("pattern").and_then(Value::as_str) {
            Some(p) => match regex::Regex::new(p) {
                Ok(r) => Some(r),
                Err(e) => {
                    let _ = reply.send(Err(RpcError::new("bad_request", format!("pattern 不是合法的正则: {e}"))));
                    return;
                }
            },
            None => None,
        };
        let idle = params.get("idle_ms").and_then(Value::as_u64).map(Duration::from_millis);
        if pattern.is_none() && idle.is_none() {
            let _ = reply.send(Err(RpcError::new("bad_request", "pattern 和 idle_ms 至少给一个")));
            return;
        }
        let since = params.get("since").cloned().filter(|v| v.is_object());
        let timeout = Duration::from_millis(params.get("timeout_ms").and_then(Value::as_u64).unwrap_or(WAIT_DEFAULT_MS).min(WAIT_MAX_MS));
        if let Some(t) = self.workspace.tabs[ix].takeover.as_mut() {
            t.waiting = true;
            t.last_action = "等待输出…".into();
        }
        cx.spawn(async move |this, cx| {
            let start = Instant::now();
            let mut last_text = String::new();
            let mut last_change = Instant::now();
            let result = loop {
                cx.background_executor().timer(Duration::from_millis(WAIT_POLL_MS)).await;
                let step = this.update(cx, |this, cx| -> Option<Result<Value, RpcError>> {
                    let Some(ix) = this.tab_ix_by_id(sid) else {
                        return Some(Err(RpcError::revoked(this.takeover_revoked.remove(&(client_id, sid)).unwrap_or("tab_closed"))));
                    };
                    if !this.workspace.tabs[ix].takeover.as_ref().is_some_and(|t| t.client_id == client_id) {
                        return Some(Err(RpcError::revoked(this.takeover_revoked.remove(&(client_id, sid)).unwrap_or("user_stopped"))));
                    }
                    let term = this.workspace.tabs[ix].term.as_ref()?;
                    let snap = term.view.read(cx).text_snapshot(200);
                    let text = trim_trailing_blank(snap.lines).join("\n");
                    if text != last_text {
                        last_text = text.clone();
                        last_change = Instant::now();
                    }
                    // 给了 since 就只看这条命令之后的新输出:正则也只在新输出里匹配,免得被屏幕上旧的提示符骗过去
                    let new_out = since.as_ref().and_then(|m| output_since(&term.view.read(cx).text_snapshot(SCROLLBACK_MAX), m));
                    let out = |key: &str, text: String| {
                        let mut r = json!({ key: true, "screen": limit_output(text) });
                        if let Some(o) = &new_out {
                            r["output"] = json!(limit_output(o.clone()));
                        }
                        Some(Ok(r))
                    };
                    if pattern.as_ref().is_some_and(|r| r.is_match(new_out.as_deref().unwrap_or(&text))) {
                        out("matched", text)
                    } else if idle.is_some_and(|d| last_change.elapsed() >= d) {
                        out("idle", text)
                    } else if start.elapsed() >= timeout {
                        out("timeout", text)
                    } else {
                        None
                    }
                });
                match step {
                    Ok(None) => continue,
                    Ok(Some(r)) => break r,
                    Err(_) => return,
                }
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(t) = this.workspace.tabs.iter_mut().find(|t| t.id == sid).and_then(|t| t.takeover.as_mut()) {
                    t.waiting = false;
                    t.last_call = Instant::now();
                }
                cx.notify();
            });
            let _ = reply.send(result);
        })
        .detach();
    }

    /// 每秒一次:空闲超时、最长时长、终端断线都在这里收。
    pub(crate) fn takeover_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut ending: Vec<(u64, &'static str)> = vec![];
        for t in &self.workspace.tabs {
            let Some(k) = &t.takeover else { continue };
            if t.term.as_ref().is_some_and(|s| matches!(s.handle.status(), Status::Closed(_))) {
                ending.push((t.id, "tab_closed"));
            } else if k.started.elapsed() >= MAX_DURATION {
                ending.push((t.id, "max_duration"));
            } else if !k.waiting && k.last_call.elapsed() >= IDLE_TIMEOUT {
                ending.push((t.id, "idle_timeout"));
            }
        }
        for (id, why) in ending {
            self.end_takeover(id, Some(why), window, cx);
        }
        let now = Instant::now();
        for k in self.workspace.tabs.iter_mut().filter_map(|t| t.takeover.as_mut()) {
            if k.sudo.as_ref().is_some_and(|g| g.until <= now) {
                k.sudo = None;
            }
        }
    }

    /// ⌘. :停止当前标签上的接管。返回是否处理了这次按键。
    pub(crate) fn takeover_hotkey(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(a) = self.workspace.active else { return false };
        let Some(id) = self.workspace.tabs[a].takeover.as_ref().map(|_| self.workspace.tabs[a].id) else { return false };
        let m = &ev.keystroke.modifiers;
        if crate::platform::accel(m) && ev.keystroke.key == "." {
            self.end_takeover(id, Some("user_stopped"), window, cx);
            return true;
        }
        // 接管期间,用户在这个标签上的其它按键一律吞掉
        true
    }

    /// 调试:截图时直接给活动标签挂上接管(`OTTER_TAKEOVER=1`)或弹出确认框(`OTTER_TAKEOVER=prompt`)。
    pub(crate) fn debug_takeover(&mut self, mode: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(a) = self.workspace.active else { return };
        let client = Arc::new(Client { id: 0, name: "Claude Code".into(), parent_exe: "/usr/local/bin/claude".into() });
        if mode == "sudo" || mode == "sudo-saved" {
            self.start_takeover(a, &client, "", false, window, cx);
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let tab_id = self.workspace.tabs[a].id;
            let input = (mode == "sudo").then(|| cx.new(|cx| InputState::new(window, cx).placeholder("sudo 密码").masked(true)));
            self.sudo_prompt = Some(SudoPrompt { tab_id, client, reason: "清理 /var/log 下的旧日志".into(), line: "[sudo] password for chen:".into(), input, reply });
        } else if mode == "prompt" {
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let tab_id = self.workspace.tabs[a].id;
            self.takeover_prompts.push_back(Prompt { tab_id, client, reason: "排查磁盘占用,执行 df -h 和 du".into(), lease: false, reply });
        } else {
            self.start_takeover(a, &client, "", false, window, cx);
            self.workspace.tabs[a].takeover.as_mut().map(|t| {
                t.last_action = "输入:df -h".into();
                t.waiting = mode == "wait";
            });
        }
    }

    /// 当前活动标签被接管时盖在终端区上的遮罩。
    pub(crate) fn takeover_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let a = self.workspace.active?;
        let tab = &self.workspace.tabs[a];
        let k = tab.takeover.as_ref()?;
        let id = tab.id;
        let fh = self.takeover_focus.clone()?;
        let stop = div()
            .id("takeover-stop")
            .h(px(38.0))
            .px(px(18.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .rounded(px(t::RADIUS_MD))
            .bg(c::danger())
            .text_color(c::on_accent())
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .cursor_pointer()
            .hover(|s| s.opacity(0.88))
            .child(icon(IconName::OctagonX, 14.0, c::on_accent()))
            .child("停止接管")
            .on_click(cx.listener(move |this, _, window, cx| this.end_takeover(id, Some("user_stopped"), window, cx)));
        let glyph = div()
            .size(px(68.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(t::RADIUS_2XL))
            .bg(c::accent().opacity(0.10))
            .border_1()
            .border_color(c::accent().opacity(0.24))
            .child(
                div()
                    .size(px(48.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(t::RADIUS_XL))
                    .bg(c::accent().opacity(0.18))
                    .border_1()
                    .border_color(c::accent().opacity(0.4))
                    .child(icon(IconName::Bot, 24.0, c::accent())),
            )
            .with_animation(
                "takeover-breathe",
                Animation::new(Duration::from_millis(2200)).repeat().with_easing(ease_in_out),
                |d, t| d.opacity(0.62 + 0.38 * (1.0 - (2.0 * t - 1.0).abs())),
            );
        Some(
            div()
                .id("takeover-overlay")
                .absolute()
                .inset_0()
                .occlude()
                .track_focus(&fh)
                .bg(c::scrim_takeover())
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                    this.takeover_hotkey(e, window, cx);
                    cx.stop_propagation();
                }))
                .on_mouse_down(MouseButton::Left, {
                    let fh = fh.clone();
                    move |_, window, cx| window.focus(&fh, cx)
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(6.0))
                        .child(glyph)
                        .child(div().mt(px(10.0)).text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).text_color(c::text()).child("Agent 接管中"))
                        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(k.client_name.clone()))
                        .child(div().max_w(px(360.0)).truncate().text_size(px(t::TYPE_MICRO)).font_family(crate::MONO).text_color(c::ghost()).child(k.last_action.clone()))
                        .children(k.sudo.as_ref().map(|g| {
                            let mins = g.until.saturating_duration_since(Instant::now()).as_secs().div_ceil(60);
                            div()
                                .id("takeover-sudo")
                                .mt(px(10.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .text_size(px(t::TYPE_MICRO))
                                .text_color(c::warn())
                                .child(icon(IconName::KeyRound, 12.0, c::warn()))
                                .child(format!("已授权提权,剩余约 {mins} 分钟"))
                                .child(div().id("takeover-sudo-revoke").cursor_pointer().text_color(c::muted()).hover(|s| s.text_color(c::text())).child("撤销").on_click(cx.listener(move |this, _, _, cx| this.revoke_sudo(id, cx))))
                        }))
                        .child(div().mt(px(14.0)).child(stop))
                        .child(div().mt(px(8.0)).text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(format!("{}. 也可以停止 · 停止不会中断正在运行的命令", crate::platform::accel_name()))),
                )
                .into_any_element(),
        )
    }

    /// 接管申请的确认框:拒绝 / 允许一次(默认) / 总是允许,三个按钮等宽。
    pub(crate) fn takeover_prompt_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let p = self.takeover_prompts.front()?;
        let ix = self.tab_ix_by_id(p.tab_id)?;
        let node_id = self.workspace.tabs[ix].node()?;
        let node = self.all_nodes().find(|n| n.id == node_id)?;
        let hero = crate::ui::alert_hero(IconName::Bot, c::accent(), format!("允许 {} 接管终端？", p.client.name), "它将直接使用你正在用的这个会话");
        let target = Self::node_target_card(node);
        let mut rows: Vec<(IconName, Rgba, String)> = vec![];
        if !p.reason.is_empty() {
            rows.push((IconName::Sparkles, c::accent(), format!("原因:{}", p.reason)));
        }
        if !p.client.parent_exe.is_empty() {
            rows.push((IconName::Terminal, c::muted(), format!("来自 {}", p.client.parent_exe)));
        }
        rows.push((IconName::Keyboard, c::warn(), "能在终端里输入命令、读取屏幕".into()));
        rows.push((IconName::ScanEye, c::warn(), "能看到终端里已有的历史内容".into()));
        rows.push((IconName::OctagonX, c::online(), format!("随时点「停止接管」或按 {}. 中止", crate::platform::accel_name())));
        let points = crate::ui::alert_rows(rows);
        // 两排:上面三个允许选项等宽(「允许一次」是默认项,主色,回车也触发;「24 小时」「总是允许」是更重的决定,不抢眼),下面整行宽的「拒绝」
        let btn_w = crate::ui::alert_btn_w(3);
        let choice = |id: &'static str, label: &'static str, d: Decision, cx: &mut Context<Self>| {
            crate::node_form::ctl_btn(id, None, label, 38.0).w(px(btn_w)).flex_none().justify_center().on_click(cx.listener(move |this, _, window, cx| this.resolve_takeover_prompt(d, window, cx)))
        };
        let day = choice("tp-day", "24 小时", Decision::Day, cx);
        let always = choice("tp-always", "总是允许", Decision::Always, cx);
        let deny = crate::node_form::ctl_btn("tp-deny", None, "拒绝", 38.0).w_full().flex_none().justify_center().on_click(cx.listener(|this, _, window, cx| this.resolve_takeover_prompt(Decision::Deny, window, cx)));
        let once = crate::ui::alert_primary_btn("tp-once", None, "允许一次", Some(btn_w)).on_click(cx.listener(|this, _, window, cx| this.resolve_takeover_prompt(Decision::Once, window, cx)));
        let hint = div().w_full().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).text_center().child(format!("「24 小时」「总是允许」只对 {} 生效,可在「信任」页撤销", node.name));
        let buttons = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(div().w_full().flex().flex_row().gap(px(10.0)).child(always).child(day).child(once))
            .child(deny)
            .child(hint);
        let body = crate::ui::alert_body(hero, Some(target), points, buttons);
        Some(crate::ui::modal_bg(
            "takeover-prompt",
            crate::confirm::CONFIRM_W,
            c::dialog_glass(),
            cx.listener(|this, e: &KeyDownEvent, window, cx| {
                match e.keystroke.key.as_str() {
                    "escape" => this.resolve_takeover_prompt(Decision::Deny, window, cx),
                    "enter" => this.resolve_takeover_prompt(Decision::Once, window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }),
            [body.into_any_element()],
        ))
    }
}

impl Home {
    /// 屏幕上(光标所在的最后一行)现在是不是 sudo 密码提示;是就返回这一行。
    fn sudo_prompt_line(&self, ix: usize, cx: &App) -> Option<String> {
        let term = self.workspace.tabs[ix].term.as_ref()?;
        let snap = term.view.read(cx).text_snapshot(0);
        let lines = trim_trailing_blank(snap.lines[snap.screen_start..].to_vec());
        lines.last().filter(|l| is_sudo_prompt(l)).map(|l| l.trim().to_string())
    }

    /// `request_sudo`:agent 看到 sudo 的密码提示后,请 Otter 代为输入密码。agent 始终拿不到密码。
    fn sudo_request(&mut self, client: &Arc<Client>, ix: usize, params: &Value, reply: Reply, window: &mut Window, cx: &mut Context<Self>) {
        let sid = self.workspace.tabs[ix].id;
        match self.workspace.tabs[ix].takeover.as_mut() {
            Some(t) if t.owned_by(client, params) => {
                t.last_call = Instant::now();
                t.client_id = client.id;
            }
            Some(_) => {
                let _ = reply.send(Err(RpcError::new("busy", "这个会话已经被另一个 agent 接管")));
                return;
            }
            None => {
                let _ = reply.send(Err(RpcError::new("takeover_required", "先调用 takeover_begin 接管这个会话")));
                return;
            }
        }
        if params.get("reason").and_then(Value::as_str).is_none_or(|r| r.trim().is_empty()) {
            let _ = reply.send(Err(RpcError::new("bad_request", "必须提供 reason:告诉用户为什么需要提权、要执行什么")));
            return;
        }
        let Some(line) = self.sudo_prompt_line(ix, cx) else {
            let _ = reply.send(Err(RpcError::new("no_sudo_prompt", "屏幕上没有 sudo 密码提示。先 send_input 执行带 sudo 的命令,等屏幕出现「[sudo] password for …」再调用")));
            return;
        };
        if self.sudo_prompt.is_some() {
            let _ = reply.send(Err(RpcError::new("busy", "正在等用户确认另一个提权请求")));
            return;
        }
        let Some(node) = self.workspace.tabs[ix].node() else {
            let _ = reply.send(Err(RpcError::new("bad_request", "只能给节点的终端提权")));
            return;
        };
        // 授权窗口内:屏幕确实是 sudo 提示,直接代填
        let remaining = self.workspace.tabs[ix].takeover.as_ref().and_then(|t| t.sudo.as_ref()).map(|g| g.until.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|r| !r.is_zero()) {
            let typed = self.workspace.tabs[ix].takeover.as_ref().and_then(|t| t.sudo.as_ref()).and_then(|g| g.password.clone());
            match self.sudo_fill(ix, typed, cx) {
                Ok(()) => {
                    let _ = reply.send(Ok(json!({ "filled": true, "granted": "window", "remaining_s": remaining.map_or(0, |r| r.as_secs()) })));
                }
                Err(e) => {
                    let _ = reply.send(Err(RpcError::new("sudo_failed", e)));
                }
            }
            cx.notify();
            return;
        }
        // 节点没保存密码:确认框里让用户输入
        let saved = self.conn_for(node).auth.password.is_some();
        let input = (!saved).then(|| cx.new(|cx| InputState::new(window, cx).placeholder("sudo 密码").masked(true)));
        if let Some(i) = &input {
            let fh = i.focus_handle(cx);
            window.focus(&fh, cx);
        }
        let reason = clip(params.get("reason").and_then(Value::as_str).unwrap_or("").trim(), 200);
        alog(format!("{} 申请在 {} 提权(sudo)", client.name, self.node_name(Some(node))));
        self.sudo_prompt = Some(SudoPrompt { tab_id: sid, client: client.clone(), reason, line, input, reply });
        cx.activate(true);
        self.activate(ix);
        cx.notify();
    }

    /// 把密码写进终端并回车。优先级:用户刚输入的 > 授权里记着的(同样是用户输入的) > 节点保存的。
    /// 写之前再确认一次屏幕还是 sudo 提示,免得用户等确认的这段时间里屏幕变了,密码被敲进别处。
    fn sudo_fill(&mut self, ix: usize, typed: Option<Zeroizing<String>>, cx: &mut Context<Self>) -> Result<(), String> {
        if self.sudo_prompt_line(ix, cx).is_none() {
            return Err("屏幕上的 sudo 密码提示已经消失".into());
        }
        let node = self.workspace.tabs[ix].node().ok_or("只能给节点的终端提权")?;
        let pw = match typed {
            Some(p) => p,
            None => self.conn_for(node).auth.password.ok_or("这个节点没有保存密码")?,
        };
        let term = self.workspace.tabs[ix].term.as_ref().ok_or("终端还没有连上")?;
        if term.handle.status() != Status::Connected {
            return Err("终端已断开".into());
        }
        let mut bytes = Zeroizing::new(pw.as_bytes().to_vec());
        bytes.push(b'\r');
        let _ = term.handle.inputter().send(bytes.to_vec());
        let name = self.node_name(Some(node));
        if let Some(t) = self.workspace.tabs[ix].takeover.as_mut() {
            t.last_action = "提权:已代为输入 sudo 密码".into();
        }
        alog(format!("→ {name} 已代为输入 sudo 密码"));
        Ok(())
    }

    /// 用户对提权请求的选择;`None` 表示拒绝。
    pub(crate) fn resolve_sudo_prompt(&mut self, scope: Option<SudoScope>, cx: &mut Context<Self>) {
        let Some(p) = self.sudo_prompt.as_ref() else { return };
        // 要输入密码却没输:不处理,等用户输入
        let typed = p.input.as_ref().map(|i| Zeroizing::new(i.read(cx).value().to_string()));
        if scope.is_some() && typed.as_ref().is_some_and(|t| t.is_empty()) {
            return;
        }
        let Some(p) = self.sudo_prompt.take() else { return };
        let ix = self.tab_ix_by_id(p.tab_id);
        let node_name = self.node_name(ix.and_then(|i| self.workspace.tabs[i].node()));
        let Some(scope) = scope else {
            alog(format!("用户拒绝了 {} 在 {} 提权", p.client.name, node_name));
            let _ = p.reply.send(Err(RpcError::new("sudo_denied", "用户拒绝了提权")));
            cx.notify();
            return;
        };
        let still_mine = ix.is_some_and(|i| self.workspace.tabs[i].takeover.as_ref().is_some_and(|t| t.client_id == p.client.id));
        let (Some(ix), true) = (ix, still_mine) else {
            let _ = p.reply.send(Err(RpcError::new("takeover_required", "接管已经结束")));
            cx.notify();
            return;
        };
        match self.sudo_fill(ix, typed.clone(), cx) {
            Ok(()) => {
                if let Some(d) = scope.window() {
                    if let Some(t) = self.workspace.tabs[ix].takeover.as_mut() {
                        t.sudo = Some(SudoGrant { until: Instant::now() + d, password: typed });
                    }
                }
                alog(format!("用户允许 {} 在 {} 提权({})", p.client.name, node_name, scope.label()));
                let _ = p.reply.send(Ok(json!({ "filled": true, "granted": scope.label() })));
            }
            Err(e) => {
                let _ = p.reply.send(Err(RpcError::new("sudo_failed", e)));
            }
        }
        cx.notify();
    }

    /// 撤销这个标签上的提权授权。
    pub(crate) fn revoke_sudo(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(ix) = self.tab_ix_by_id(tab_id) {
            if let Some(t) = self.workspace.tabs[ix].takeover.as_mut() {
                if t.sudo.take().is_some() {
                    alog(format!("用户撤销了 {} 的提权授权", t.client_name));
                }
            }
        }
        cx.notify();
    }

    /// 接管结束、标签关闭时,这个标签上等确认的提权请求一并作废。
    fn drop_sudo_prompt(&mut self, tab_id: u64) {
        if self.sudo_prompt.as_ref().is_some_and(|p| p.tab_id == tab_id) {
            if let Some(p) = self.sudo_prompt.take() {
                let _ = p.reply.send(Err(RpcError::new("takeover_required", "接管已经结束")));
            }
        }
    }

    /// 提权请求的确认框:拒绝 / 仅这一次(默认) / 10 分钟 / 30 分钟。节点没保存密码时多一个密码输入框。
    pub(crate) fn sudo_prompt_dialog(&self, window: &Window, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let p = self.sudo_prompt.as_ref()?;
        let ix = self.tab_ix_by_id(p.tab_id)?;
        let node_id = self.workspace.tabs[ix].node()?;
        let node = self.all_nodes().find(|n| n.id == node_id)?;
        let hero = crate::ui::alert_hero(IconName::KeyRound, c::warn(), format!("允许 {} 提权？", p.client.name), "由 Otter 代为输入 sudo 密码,agent 看不到密码");
        let target = Self::node_target_card(node);
        let mut rows: Vec<(IconName, Rgba, String)> = vec![];
        if !p.reason.is_empty() {
            rows.push((IconName::Sparkles, c::accent(), format!("原因:{}", p.reason)));
        }
        rows.push((IconName::Terminal, c::muted(), p.line.clone()));
        rows.push((IconName::KeyRound, c::warn(), if p.input.is_some() { "这个节点没有保存密码,请在下面输入".into() } else { "将使用节点里保存的密码".into() }));
        rows.push((IconName::OctagonX, c::online(), "随时可以在遮罩上撤销授权或停止接管".into()));
        let points = crate::ui::alert_rows(rows);
        let needs_pw = p.input.is_some();
        let empty = p.input.as_ref().is_some_and(|i| i.read(cx).value().is_empty());
        let off = needs_pw && empty;
        let pw_field = p.input.as_ref().map(|i| {
            let focused = i.focus_handle(cx).is_focused(window);
            div().w_full().child(crate::ui::field(IconName::Lock, focused, false, crate::node_form::input_cell(i, false)))
        });
        let btn_w = crate::ui::alert_btn_w(3);
        let choice = |id: &'static str, label: &'static str, s: SudoScope, cx: &mut Context<Self>| {
            crate::node_form::ctl_btn(id, None, label, 38.0)
                .w(px(btn_w))
                .flex_none()
                .justify_center()
                .when(off, |d| d.opacity(0.4))
                .on_click(cx.listener(move |this, _, _, cx| this.resolve_sudo_prompt(Some(s), cx)))
        };
        let m30 = choice("sp-30", "30 分钟", SudoScope::Min30, cx);
        let m10 = choice("sp-10", "10 分钟", SudoScope::Min10, cx);
        let once = crate::ui::alert_primary_btn("sp-once", None, "仅这一次", Some(btn_w)).when(off, |d| d.opacity(0.4)).on_click(cx.listener(|this, _, _, cx| this.resolve_sudo_prompt(Some(SudoScope::Once), cx)));
        let deny = crate::node_form::ctl_btn("sp-deny", None, "拒绝", 38.0).w_full().flex_none().justify_center().on_click(cx.listener(|this, _, _, cx| this.resolve_sudo_prompt(None, cx)));
        let hint = div().w_full().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).text_center().child("授权只对当前这次接管有效,接管结束即失效;密码不会保存");
        let buttons = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .children(pw_field)
            .child(div().w_full().flex().flex_row().gap(px(10.0)).child(m30).child(m10).child(once))
            .child(deny)
            .child(hint);
        let body = crate::ui::alert_body(hero, Some(target), points, buttons);
        Some(crate::ui::modal_bg(
            "sudo-prompt",
            crate::confirm::CONFIRM_W,
            c::dialog_glass(),
            cx.listener(|this, e: &KeyDownEvent, _, cx| {
                match e.keystroke.key.as_str() {
                    "escape" => this.resolve_sudo_prompt(None, cx),
                    "enter" => this.resolve_sudo_prompt(Some(SudoScope::Once), cx),
                    _ => return,
                }
                cx.stop_propagation();
            }),
            [body.into_any_element()],
        ))
    }
}

fn revoke_label(r: &str) -> &'static str {
    match r {
        "user_stopped" => "用户停止",
        "idle_timeout" => "空闲超时",
        "max_duration" => "时长到上限",
        "tab_closed" => "标签关闭或终端断开",
        _ => "其它原因",
    }
}

#[cfg(test)]
mod tests {
    use super::{INPUT_MAX, is_sudo_prompt, OUTPUT_MAX, Takeover, key_bytes, limit_output, new_lease, trim_trailing_blank};
    use crate::ctl::Client;
    use serde_json::json;
    use std::time::Instant;

    fn client(id: u64) -> Client {
        Client { id, name: "c".into(), parent_exe: String::new() }
    }

    fn takeover(client_id: u64, lease: Option<&str>) -> Takeover {
        let now = Instant::now();
        Takeover { client_id, client_name: "c".into(), started: now, last_call: now, last_action: String::new(), waiting: false, lease: lease.map(String::from), sudo: None }
    }

    fn snap(lines: &[&str], screen_start: usize, cursor_line: usize, history_total: usize) -> gpui_xterm::TextSnapshot {
        gpui_xterm::TextSnapshot { lines: lines.iter().map(|s| s.to_string()).collect(), screen_start, cursor_line, cursor_col: 0, history_total }
    }

    #[test]
    fn output_since_returns_only_new_lines() {
        use super::{output_mark, output_since};
        // 命令前:屏幕 3 行,光标在第 2 行(提示符)
        let before = snap(&["Welcome", "old output", "root@h:~#", ""], 0, 2, 0);
        let mark = output_mark(&before);
        assert_eq!(mark["row"], 2);
        // 命令后:多了输出和新提示符,没有滚动
        let after = snap(&["Welcome", "old output", "root@h:~# ls", "a b c", "root@h:~#"], 0, 4, 0);
        assert_eq!(output_since(&after, &mark).unwrap(), "root@h:~# ls\na b c\nroot@h:~#");
        // 滚动了:回滚多了 2 行,窗口起点后移,行号靠 history_total 对齐
        let scrolled = snap(&["root@h:~# ls", "a b c", "root@h:~#"], 1, 1, 3);
        assert_eq!(output_since(&scrolled, &mark).unwrap(), "root@h:~# ls\na b c\nroot@h:~#");
        // 回滚被挤掉导致错位、或屏幕被清空:校验行对不上,返回 None 让调用方退回整屏
        let wrong = snap(&["foo", "bar", "baz"], 0, 2, 0);
        assert!(output_since(&wrong, &mark).is_none());
    }

    #[test]
    fn lease_lets_another_connection_continue() {
        let t = takeover(1, Some("tok"));
        assert!(t.owned_by(&client(1), &json!({})), "同一连接");
        assert!(t.owned_by(&client(2), &json!({ "token": "tok" })), "新连接带令牌");
        assert!(!t.owned_by(&client(2), &json!({ "token": "bad" })));
        assert!(!t.owned_by(&client(2), &json!({})));
        // 普通接管不认令牌
        let t = takeover(1, None);
        assert!(!t.owned_by(&client(2), &json!({ "token": "tok" })));
    }

    #[test]
    fn leases_are_unique_hex() {
        let (a, b) = (new_lease(), new_lease());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn keys_map_to_bytes() {
        assert_eq!(key_bytes("enter"), Some(b"\r".to_vec()));
        assert_eq!(key_bytes("Ctrl+C"), Some(vec![3]));
        assert_eq!(key_bytes("ctrl+d"), Some(vec![4]));
        assert_eq!(key_bytes("up"), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes("ctrl+1"), None);
        assert_eq!(key_bytes("ctrl+ab"), None);
        assert_eq!(key_bytes("f13"), None);
    }

    #[test]
    fn output_is_limited_on_char_boundary() {
        let s = "好".repeat(OUTPUT_MAX); // 每个 3 字节
        let out = limit_output(s);
        assert!(out.len() <= OUTPUT_MAX + 64);
        assert!(out.ends_with("已截断)"));
        assert_eq!(limit_output("abc".into()), "abc");
    }

    #[test]
    fn trailing_blank_lines_are_trimmed() {
        let v = trim_trailing_blank(vec!["a".into(), "".into(), "b".into(), "".into(), "".into()]);
        assert_eq!(v, vec!["a".to_string(), "".into(), "b".into()]);
    }

    #[test]
    fn password_prompts_are_recognised() {
        assert!(is_sudo_prompt("[sudo] password for chen:"));
        assert!(is_sudo_prompt("  [sudo] password for chen: "));
        assert!(is_sudo_prompt("[sudo] chen 的密码:"));
        assert!(is_sudo_prompt("[sudo: authenticate] Password:"));
        assert!(is_sudo_prompt("Password:"));
        assert!(!is_sudo_prompt("$ sudo ls"));
        assert!(!is_sudo_prompt("Last password change: Mon"));
        assert!(!is_sudo_prompt("password policy updated"));
    }
}
