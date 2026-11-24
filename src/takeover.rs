//! agent 接管用户当前的终端会话:授权确认、接管状态、遮罩、输入路由、读屏幕。
//! 接口和规则见 docs/modules/ctl.md,决策见 docs/decisions/0009-agent-interface.md。

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::{Value, json};

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
}

/// 等用户确认的接管申请。
pub struct Prompt {
    tab_id: u64,
    client: Arc<Client>,
    reason: String,
    reply: Reply,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Decision {
    Deny,
    Once,
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
            let err = match self.takeover_revoked.remove(&(client.id, sid)) {
                Some(r) => RpcError::revoked(r),
                None => RpcError::new("no_such_session", "没有这个会话"),
            };
            let _ = reply.send(Err(err));
            return;
        };
        match method {
            "takeover_begin" => self.takeover_begin(client, ix, params, reply, window, cx),
            "takeover_end" => {
                let mine = self.workspace.tabs[ix].takeover.as_ref().is_some_and(|t| t.client_id == client.id);
                if mine {
                    self.end_takeover(sid, None, window, cx);
                    let _ = reply.send(Ok(json!({ "ended": true })));
                } else {
                    let _ = reply.send(Err(RpcError::new("takeover_required", "当前没有在接管这个会话")));
                }
            }
            "send_input" | "send_key" | "read_screen" | "read_scrollback" | "wait_for" => self.takeover_op(client, ix, method, params, reply, cx),
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
            Some(t) if t.client_id == client.id => {
                let _ = reply.send(Ok(json!({ "session": sid, "already": true })));
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
        let granted = self.store.as_ref().is_some_and(|s| s.grant_has(&client.name, &client.parent_exe, node));
        if granted {
            self.start_takeover(ix, client, &reason, window, cx);
            let _ = reply.send(Ok(json!({ "session": sid, "granted": "always" })));
            return;
        }
        alog(format!("{} 申请接管 {}", client.name, self.node_name(Some(node))));
        self.takeover_prompts.push_back(Prompt { tab_id: sid, client: client.clone(), reason, reply });
        // 申请要让用户看见:把窗口带到前台
        cx.activate(true);
        cx.notify();
    }

    fn start_takeover(&mut self, ix: usize, client: &Arc<Client>, reason: &str, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        let node = self.workspace.tabs[ix].node();
        self.workspace.tabs[ix].takeover = Some(Takeover {
            client_id: client.id,
            client_name: client.name.clone(),
            started: now,
            last_call: now,
            last_action: if reason.is_empty() { "已开始接管".into() } else { clip(reason, 60) },
            waiting: false,
        });
        alog(format!("{} 开始接管 {}{}", client.name, self.node_name(node), if reason.is_empty() { String::new() } else { format!("(原因:{reason})") }));
        let fh = self.takeover_focus.get_or_insert_with(|| cx.focus_handle()).clone();
        if self.workspace.active == Some(ix) {
            window.focus(&fh, cx);
        }
        cx.notify();
    }

    /// 结束接管。`revoked` 为 `Some(原因)` 时记下来,agent 下次调用会收到 `takeover_revoked`;
    /// `None` 表示 agent 自己交还。不向终端写任何字节(停止不发 Ctrl+C)。
    pub(crate) fn end_takeover(&mut self, tab_id: u64, revoked: Option<&'static str>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_ix_by_id(tab_id) else { return };
        let Some(t) = self.workspace.tabs[ix].takeover.take() else { return };
        let node = self.node_name(self.workspace.tabs[ix].node());
        match revoked {
            Some(r) => {
                self.takeover_revoked.insert((t.client_id, tab_id), r);
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

    /// 客户端连接断开:释放它持有的接管,撤掉它排队的申请。
    pub(crate) fn takeover_client_closed(&mut self, client_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.takeover_prompts.retain(|p| p.client.id != client_id);
        let ids: Vec<u64> = self.workspace.tabs.iter().filter(|t| t.takeover.as_ref().is_some_and(|k| k.client_id == client_id)).map(|t| t.id).collect();
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
        if let Some(t) = tab.takeover.take() {
            self.takeover_revoked.insert((t.client_id, tab.id), "tab_closed");
            alog(format!("{} 的接管随标签关闭而结束", t.client_name));
        }
        let id = tab.id;
        self.takeover_prompts.retain(|p| p.tab_id != id);
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
                if d == Decision::Always {
                    if let Some(Err(e)) = self.store.as_ref().map(|s| s.grant_add(&p.client.name, &p.client.parent_exe, node)) {
                        logs::push(Level::Error, "本地库", format!("保存接管授权失败: {e}"));
                    } else {
                        alog(format!("用户选择总是允许 {} 接管 {}", p.client.name, node_name));
                    }
                }
                self.start_takeover(ix, &p.client, &p.reason, window, cx);
                let _ = p.reply.send(Ok(json!({ "session": p.tab_id, "granted": if d == Decision::Always { "always" } else { "once" } })));
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
            Some(t) if t.client_id == client.id => t.last_call = Instant::now(),
            Some(_) => {
                let _ = reply.send(Err(RpcError::new("busy", "这个会话已经被另一个 agent 接管")));
                return;
            }
            None => {
                let err = match self.takeover_revoked.remove(&(client.id, sid)) {
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
                let mut bytes = text.as_bytes().to_vec();
                if execute {
                    bytes.push(b'\r');
                }
                let _ = term.handle.inputter().send(bytes);
                let shown = clip(&text.replace(['\n', '\r'], "⏎"), LOG_TEXT_MAX);
                alog(format!("→ {node} 输入:{shown}{}", if execute { " ⏎" } else { "" }));
                set_action(self, format!("{}:{}", if execute { "执行" } else { "输入" }, clip(&text.replace(['\n', '\r'], " "), 40)));
                let _ = reply.send(Ok(json!({ "sent": text.len(), "executed": execute })));
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
                    let out = |key: &str, text: String| Some(Ok(json!({ key: true, "screen": limit_output(text) })));
                    if pattern.as_ref().is_some_and(|r| r.is_match(&text)) {
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
    }

    /// ⌘. :停止当前标签上的接管。返回是否处理了这次按键。
    pub(crate) fn takeover_hotkey(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(a) = self.workspace.active else { return false };
        let Some(id) = self.workspace.tabs[a].takeover.as_ref().map(|_| self.workspace.tabs[a].id) else { return false };
        let m = &ev.keystroke.modifiers;
        if m.platform && !m.control && !m.alt && ev.keystroke.key == "." {
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
        if mode == "prompt" {
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let tab_id = self.workspace.tabs[a].id;
            self.takeover_prompts.push_back(Prompt { tab_id, client, reason: "排查磁盘占用,执行 df -h 和 du".into(), reply });
        } else {
            self.start_takeover(a, &client, "", window, cx);
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
                        .child(div().mt(px(14.0)).child(stop))
                        .child(div().mt(px(8.0)).text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child("⌘. 也可以停止 · 停止不会中断正在运行的命令")),
                )
                .into_any_element(),
        )
    }

    /// 接管申请的确认框:拒绝 / 允许一次 / 总是允许。
    pub(crate) fn takeover_prompt_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let p = self.takeover_prompts.front()?;
        let ix = self.tab_ix_by_id(p.tab_id)?;
        let node_id = self.workspace.tabs[ix].node()?;
        let node = self.all_nodes().find(|n| n.id == node_id)?;
        let hero = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .size(px(68.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(t::RADIUS_2XL))
                    .bg(c::accent().opacity(0.08))
                    .border_1()
                    .border_color(c::accent().opacity(0.18))
                    .child(
                        div()
                            .size(px(48.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(t::RADIUS_XL))
                            .bg(c::accent().opacity(0.16))
                            .border_1()
                            .border_color(c::accent().opacity(0.36))
                            .child(icon(IconName::Bot, 22.0, c::accent())),
                    ),
            )
            .child(div().mt(px(10.0)).max_w(px(340.0)).truncate().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).text_color(c::text()).child(format!("允许 {} 接管终端？", p.client.name)))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("它将直接使用你正在用的这个会话"));
        let target = div()
            .w_full()
            .h(px(crate::NODE_CARD_H))
            .glass(t::RADIUS_LG, GlassStyle::CARD)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .child(Self::os_tile(node.os))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(node.name.clone()))
                    .child(div().truncate().text_size(px(t::TYPE_MICRO)).font_family(crate::MONO).text_color(c::ghost()).child(node.user_host.clone())),
            );
        let mut rows: Vec<(IconName, Rgba, String)> = vec![];
        if !p.reason.is_empty() {
            rows.push((IconName::Sparkles, c::accent(), format!("原因:{}", p.reason)));
        }
        if !p.client.parent_exe.is_empty() {
            rows.push((IconName::Terminal, c::muted(), format!("来自 {}", p.client.parent_exe)));
        }
        rows.push((IconName::Keyboard, c::warn(), "能在终端里输入命令、读取屏幕".into()));
        rows.push((IconName::ScanEye, c::warn(), "能看到终端里已有的历史内容".into()));
        rows.push((IconName::OctagonX, c::online(), "随时点「停止接管」或按 ⌘. 中止".into()));
        let n = rows.len();
        let points = div().w_full().flex().flex_col().rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).bg(c::field()).children(rows.into_iter().enumerate().map(|(i, (ic, color, text))| {
            div()
                .min_h(px(38.0))
                .px(px(12.0))
                .py(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .when(i + 1 < n, |d| d.border_b_1().border_color(c::line()))
                .child(icon(ic, 14.0, color))
                .child(div().flex_1().min_w_0().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(text))
        }));
        let deny = crate::node_form::ctl_btn("tp-deny", None, "拒绝", 38.0).flex_1().justify_center().on_click(cx.listener(|this, _, window, cx| this.resolve_takeover_prompt(Decision::Deny, window, cx)));
        let once = crate::node_form::ctl_btn("tp-once", None, "允许一次", 38.0).flex_1().justify_center().on_click(cx.listener(|this, _, window, cx| this.resolve_takeover_prompt(Decision::Once, window, cx)));
        let always = div()
            .id("tp-always")
            .flex_1()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .h(px(38.0))
            .rounded(px(t::RADIUS_MD))
            .bg(c::accent())
            .text_color(c::on_accent())
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .cursor_pointer()
            .hover(|s| s.opacity(0.88))
            .child("总是允许")
            .on_click(cx.listener(|this, _, window, cx| this.resolve_takeover_prompt(Decision::Always, window, cx)));
        let buttons = div().w_full().flex().flex_row().gap(px(10.0)).child(deny).child(once).child(always);
        let body = div().flex().flex_col().items_center().gap(px(18.0)).px(px(28.0)).pt(px(28.0)).pb(px(24.0)).child(hero).child(target).child(points).child(buttons);
        Some(crate::ui::modal_bg(
            "takeover-prompt",
            crate::nodes::CONFIRM_W,
            c::dialog_glass(),
            cx.listener(|this, e: &KeyDownEvent, window, cx| {
                if e.keystroke.key == "escape" {
                    this.resolve_takeover_prompt(Decision::Deny, window, cx);
                    cx.stop_propagation();
                }
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
    use super::{INPUT_MAX, OUTPUT_MAX, key_bytes, limit_output, trim_trailing_blank};

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
}
