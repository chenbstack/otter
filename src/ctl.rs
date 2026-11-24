//! 本地接口:App 监听一个 Unix socket,MCP / CLI 经它访问运行中的 Otter。
//! 协议是按行分隔的 JSON-RPC;连接后第一条必须是 `hello`。方法分发和只读方法在这里,
//! 接管(授权、遮罩、输入)在 `takeover.rs`。设计见 docs/modules/ctl.md。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::mock::NodeId;
use crate::terminal::ssh::Status;
use crate::workspace::{NodeMode, TabKind};
use gpui_kit::{Context, Window};

use crate::{Home, logs};

pub const PROTOCOL: u32 = 1;

/// 一个已握手的客户端连接。
pub struct Client {
    pub id: u64,
    /// 客户端自报的名称(MCP 取 `clientInfo.name`),只用来给用户看,不能当身份
    pub name: String,
    /// 客户端进程的父进程路径,同样只用来展示
    pub parent_exe: String,
}

/// 返回给客户端的错误。`kind` 是稳定的机器可读码(见 docs/modules/ctl.md 的错误码表)。
#[derive(Debug, Clone)]
pub struct RpcError {
    pub kind: &'static str,
    pub message: String,
    pub reason: Option<&'static str>,
}

impl RpcError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), reason: None }
    }
    pub fn revoked(reason: &'static str) -> Self {
        let message = match reason {
            "user_stopped" => "用户停止了接管",
            "idle_timeout" => "空闲太久,接管已自动结束",
            "max_duration" => "接管时间到上限,已结束",
            "tab_closed" => "标签已关闭",
            "disconnected" => "连接已断开",
            _ => "接管已结束",
        };
        Self { kind: "takeover_revoked", message: message.into(), reason: Some(reason) }
    }
    fn to_json(&self) -> Value {
        let mut data = json!({ "kind": self.kind });
        if let Some(r) = self.reason {
            data["reason"] = json!(r);
        }
        json!({ "code": -32000, "message": self.message, "data": data })
    }
}

pub type Reply = oneshot::Sender<Result<Value, RpcError>>;

/// 从连接线程发往界面线程的消息。
pub enum UiMsg {
    Call { client: Arc<Client>, method: String, params: Value, reply: Reply },
    /// 客户端连接断开:它持有的接管要释放
    Closed { client_id: u64 },
}

/// 启动本地接口服务。已有另一个 Otter 在监听、或绑定失败时返回 `None`(日志里写明原因)。
pub fn start() -> Option<mpsc::UnboundedReceiver<UiMsg>> {
    let _guard = crate::terminal::ssh::runtime().enter();
    let mut conns = match crate::ipc_server::bind() {
        Ok(c) => c,
        Err(crate::ipc_server::BindError::InUse) => {
            logs::push(logs::Level::Warn, "Agent", "已有另一个 Otter 在监听本地接口,本进程不启用");
            return None;
        }
        Err(crate::ipc_server::BindError::Other(e)) => {
            logs::push(logs::Level::Error, "Agent", format!("本地接口启动失败: {e}"));
            return None;
        }
    };
    let (tx, rx) = mpsc::unbounded_channel();
    crate::terminal::ssh::runtime().spawn(async move {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        while let Some(acc) = conns.recv().await {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(serve(acc.conn, acc.pid, id, tx.clone()));
        }
    });
    Some(rx)
}

async fn serve(stream: crate::ipc_server::Conn, pid: Option<u32>, id: u64, tx: mpsc::UnboundedSender<UiMsg>) {
    let (rd, mut wr) = tokio::io::split(stream);
    let mut lines = BufReader::new(rd).lines();
    let mut client: Option<Arc<Client>> = None;
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = write(&mut wr, &error_response(Value::Null, &RpcError::new("bad_request", format!("不是合法的 JSON: {e}")))).await;
                continue;
            }
        };
        let rid = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        let resp = if method == "hello" {
            let s = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or("").chars().take(200).collect::<String>();
            let name = s("client_name").trim().to_string();
            if name.is_empty() {
                error_response(rid, &RpcError::new("bad_request", "hello 必须带 client_name:说明你是哪个 agent"))
            } else {
                // 调用方的来源只信系统给的对端进程号查出来的父进程路径,客户端自己报的 parent_exe 一律忽略。查不到就是空串 = 来源未验证
                let parent_exe = match pid {
                    Some(p) => tokio::task::spawn_blocking(move || crate::ipc::peer_source(p)).await.unwrap_or_default(),
                    None => String::new(),
                };
                client = Some(Arc::new(Client { id, name, parent_exe }));
                ok_response(rid, json!({ "protocol": PROTOCOL, "app": "otter" }))
            }
        } else if let Some(c) = &client {
            let (rtx, rrx) = oneshot::channel();
            if tx.send(UiMsg::Call { client: c.clone(), method, params, reply: rtx }).is_err() {
                return;
            }
            match rrx.await {
                Ok(Ok(v)) => ok_response(rid, v),
                Ok(Err(e)) => error_response(rid, &e),
                Err(_) => error_response(rid, &RpcError::new("internal", "App 没有响应")),
            }
        } else {
            error_response(rid, &RpcError::new("bad_request", "第一条消息必须是 hello"))
        };
        if write(&mut wr, &resp).await.is_err() {
            break;
        }
    }
    if client.is_some() {
        let _ = tx.send(UiMsg::Closed { client_id: id });
    }
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, e: &RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": e.to_json() })
}

async fn write(wr: &mut (impl tokio::io::AsyncWrite + Unpin), v: &Value) -> std::io::Result<()> {
    let mut s = v.to_string();
    s.push('\n');
    wr.write_all(s.as_bytes()).await
}

/// `user@host[:port]` 拆成 (用户, 主机, 端口)
pub fn split_user_host(uh: &str) -> (String, String, u16) {
    let (user, rest) = uh.split_once('@').unwrap_or(("", uh));
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse().unwrap_or(22)),
        _ => (rest, 22),
    };
    (user.to_string(), host.to_string(), port)
}

fn state_name(s: &Status) -> &'static str {
    match s {
        Status::Connecting | Status::Waiting => "connecting",
        Status::Connected => "connected",
        Status::Reconnecting(_) => "reconnecting",
        Status::Closed(_) => "failed",
    }
}

impl Home {
    /// 在界面线程里处理一条来自客户端的消息。
    pub(crate) fn ctl_msg(&mut self, msg: UiMsg, window: &mut Window, cx: &mut Context<Self>) {
        match msg {
            UiMsg::Closed { client_id } => self.takeover_client_closed(client_id, window, cx),
            UiMsg::Call { client, method, params, reply } => match method.as_str() {
                "list_nodes" => {
                    let _ = reply.send(self.ctl_list_nodes(&params));
                }
                "list_groups" => {
                    let _ = reply.send(Ok(self.ctl_list_groups()));
                }
                "list_sessions" => {
                    let _ = reply.send(Ok(self.ctl_list_sessions()));
                }
                "get_focus" => {
                    let _ = reply.send(Ok(self.ctl_get_focus()));
                }
                // 其余方法(接管相关)由 takeover.rs 接手,reply 也交给它回
                _ => self.takeover_call(&client, &method, &params, reply, window, cx),
            },
        }
    }

    fn ctl_node_json(&self, id: NodeId) -> Value {
        let g = self.groups.iter().find(|g| g.nodes.iter().any(|n| n.id == id));
        let n = g.and_then(|g| g.nodes.iter().find(|n| n.id == id));
        match (g, n) {
            (Some(g), Some(n)) => {
                let (user, host, port) = split_user_host(&n.user_host);
                json!({
                    "id": n.id.0, "name": n.name, "host": host, "user": user, "port": port,
                    "group_id": g.id, "group": g.name, "created_at": n.created_at, "last_connected_at": n.last_connected, "tags": n.tags, "os": n.os.key(), "connected": self.node_connected(n.id),
                })
            }
            _ => Value::Null,
        }
    }

    fn node_connected(&self, id: NodeId) -> bool {
        self.workspace.tabs.iter().any(|t| t.node() == Some(id) && t.term.as_ref().is_some_and(|s| s.handle.status() == Status::Connected))
    }

    /// 分组参数:数字按分组 id,字符串按名字(名字重复时要求改用 id)。
    fn resolve_group(&self, v: &Value) -> Result<usize, RpcError> {
        let by_id = |id: i64| self.groups.iter().position(|g| g.id == id);
        let ix = match v {
            Value::Number(n) => n.as_i64().and_then(by_id),
            Value::String(name) => {
                let hits: Vec<usize> = (0..self.groups.len()).filter(|&i| self.groups[i].name == *name).collect();
                match hits.as_slice() {
                    [i] => Some(*i),
                    [] => name.parse::<i64>().ok().and_then(by_id),
                    _ => {
                        let ids: Vec<String> = hits.iter().map(|&i| self.groups[i].id.to_string()).collect();
                        return Err(RpcError::new("bad_request", format!("有多个叫「{name}」的分组,请改用分组 id: {}", ids.join(", "))));
                    }
                }
            }
            _ => return Err(RpcError::new("bad_request", "group 要么是分组 id(数字),要么是分组名")),
        };
        ix.ok_or_else(|| RpcError::new("no_such_group", "没有这个分组"))
    }

    /// `group` 参数可选:给了只列该分组的节点,没给列全部。
    pub(crate) fn ctl_list_nodes(&self, params: &Value) -> Result<Value, RpcError> {
        let groups: Vec<usize> = match params.get("group").filter(|v| !v.is_null()) {
            Some(v) => vec![self.resolve_group(v)?],
            None => (0..self.groups.len()).collect(),
        };
        Ok(Value::Array(groups.into_iter().flat_map(|i| self.groups[i].nodes.iter()).map(|n| self.ctl_node_json(n.id)).collect()))
    }

    pub(crate) fn ctl_list_groups(&self) -> Value {
        Value::Array(self.groups.iter().map(|g| json!({ "id": g.id, "name": g.name, "node_count": g.nodes.len() })).collect())
    }

    fn ctl_session_json(&self, ix: usize) -> Value {
        let t = &self.workspace.tabs[ix];
        let (kind, mode) = match &t.kind {
            TabKind::Nodes => ("nodes", ""),
            TabKind::Node(_) => ("node", match t.mode {
                NodeMode::Terminal => "terminal",
                NodeMode::Files => "sftp",
                NodeMode::Containers => "docker",
            }),
            TabKind::Terminal { .. } => ("container_terminal", "terminal"),
            TabKind::Logs { .. } => ("logs", ""),
        };
        let state = t.term.as_ref().map_or("none", |s| state_name(&s.handle.status()));
        json!({
            "session": t.id,
            "node": t.node().map(|n| n.0),
            "node_name": t.node().and_then(|n| self.all_nodes().find(|x| x.id == n)).map(|n| n.name.clone()),
            "kind": kind, "mode": mode, "state": state,
            "focused": self.workspace.active == Some(ix),
            "takeover": t.takeover.is_some(),
        })
    }

    pub(crate) fn ctl_list_sessions(&self) -> Value {
        Value::Array((0..self.workspace.tabs.len()).filter(|&i| !matches!(self.workspace.tabs[i].kind, TabKind::Nodes)).map(|i| self.ctl_session_json(i)).collect())
    }

    pub(crate) fn ctl_get_focus(&self) -> Value {
        match self.workspace.active {
            Some(a) if !matches!(self.workspace.tabs[a].kind, TabKind::Nodes) => self.ctl_session_json(a),
            _ => Value::Null,
        }
    }

    /// 每 150ms 看一次各标签是不是在忙,变了才重绘(后台标签的输出不会触发 `Home` 重绘)。
    pub(crate) fn start_activity_watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let mut last: Vec<u64> = vec![];
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(150)).await;
                let ok = this.update_in(cx, |this, _, cx| {
                    let now: Vec<u64> = this.workspace.tabs.iter().filter(|t| t.busy()).map(|t| t.id).collect();
                    if now != last {
                        last = now;
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// 启动本地接口并开始处理消息。演示模式和截图不启用,免得抢走真实 App 的 socket。
    pub(crate) fn start_ctl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut rx) = start() else { return };
        cx.spawn_in(window, async move |this, cx| {
            while let Some(msg) = rx.recv().await {
                if this.update_in(cx, |this, window, cx| this.ctl_msg(msg, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                if this.update_in(cx, |this, window, cx| {
                    this.takeover_tick(window, cx);
                    this.persist_new_passwords();
                }).is_err() {
                    break;
                }
            }
        })
        .detach();
    }
}

