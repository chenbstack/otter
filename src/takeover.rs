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

/// 空闲超时和单次最长时长(分钟,设置页「AI 接管」可改)
static IDLE_MIN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(30);
static MAX_MIN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(30);
pub const KEY_IDLE_MIN: &str = "takeover_idle_min";
pub const KEY_MAX_MIN: &str = "takeover_max_min";

pub fn idle_min() -> u64 {
    IDLE_MIN.load(std::sync::atomic::Ordering::Relaxed)
}
pub fn set_idle_min(v: u64) {
    IDLE_MIN.store(v.clamp(1, 24 * 60), std::sync::atomic::Ordering::Relaxed);
}
pub fn max_min() -> u64 {
    MAX_MIN.load(std::sync::atomic::Ordering::Relaxed)
}
pub fn set_max_min(v: u64) {
    MAX_MIN.store(v.clamp(1, 24 * 60), std::sync::atomic::Ordering::Relaxed);
}
/// 超过这么久没有任何调用,自动释放(`wait_for` 等待期间不算)
pub fn idle_timeout() -> Duration {
    Duration::from_secs(idle_min() * 60)
}
/// 单次接管的最长时长
pub fn max_duration() -> Duration {
    Duration::from_secs(max_min() * 60)
}

/// 启动时按记住的设置恢复
pub fn restore_prefs(st: &crate::store::Store) {
    if let Some(v) = st.get_setting(KEY_IDLE_MIN).and_then(|v| v.parse().ok()) {
        set_idle_min(v);
    }
    if let Some(v) = st.get_setting(KEY_MAX_MIN).and_then(|v| v.parse().ok()) {
        set_max_min(v);
    }
}
/// `send_input` 单次最多字节
/// 接管遮罩中间内容的最大宽度(状态行超出截断),token `layout.takeover.w`
const TAKEOVER_CARD_W: f32 = 360.0;

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
    /// Otter 代为输入过的密码(只在内存里):返回给 agent 的屏幕内容里要把它们抹掉
    typed: Vec<Zeroizing<String>>,
    /// agent 最近一次执行(send_input 带 execute)的命令。提权确认框里给用户看,免得屏幕上的「密码提示」是 agent 自己造的
    last_cmd: String,
    /// 终端前台现在是不是 root shell(定时在服务器上查,遮罩上提示)
    root_shell: bool,
    rs_checked: Instant,
    rs_busy: bool,
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
    /// agent 最近执行的命令,给用户看
    cmd: String,
    /// 是 sudo 还是 su:su 要的是 root 密码,Otter 没有,必须让用户输入,而且只代填这一次
    kind: Fg,
    /// 节点没保存密码、或者是 su 时,让用户输入
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

/// 在远端(经同一条 SSH 连接另开一个 exec 通道)列出进程,供 `sudo_at_prompt` 判断终端前台是不是真的 sudo。
/// 第一行 `ME=` 是这条 exec 的父进程(sshd 为这条连接开的会话进程),交互终端里的 shell 也是它的子进程。
pub const SUDO_FG_SCRIPT: &str = "echo \"ME=$PPID\"; ps -eo pid=,ppid=,pgid=,tpgid=,tty=,user=,comm= 2>/dev/null || ps -axo pid=,ppid=,pgid=,tpgid=,tty=,user=,comm= 2>/dev/null";

/// 终端的前台进程组里是不是有一个 root 身份、还没起子进程的 `sudo` 或 `su`(也就是真的停在它们的密码提示上),返回是哪一个。
/// 屏幕上的「密码提示」可以被 agent 的脚本伪造,这是不看屏幕、从服务器进程表里验证的办法:
/// - 伪造提示的脚本以普通用户身份运行,不是 root;
/// - sudo 已经认证通过(缓存、免密)并起了命令时有子进程,这时输入会被交给那个命令而不是 sudo。
/// 看不懂输出(没有 ps、格式不同)一律当作验证失败,不填。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fg {
    Sudo,
    Su,
}

impl Fg {
    fn name(self) -> &'static str {
        match self {
            Fg::Sudo => "sudo",
            Fg::Su => "su",
        }
    }
}

struct Proc {
    pid: String,
    ppid: String,
    pgid: String,
    tpgid: String,
    tty: String,
    user: String,
    comm: String,
}

/// 解析 `SUDO_FG_SCRIPT` 的输出:(这条连接 sshd 会话进程的 pid, 所有进程, 这条连接上带终端的 shell 所在终端的 (终端, 前台进程组, shell 的用户))
fn parse_ps(out: &str) -> Result<(Vec<Proc>, Vec<(String, String, String)>), String> {
    let mut me = None;
    let mut rows: Vec<Proc> = Vec::new();
    for l in out.lines() {
        if let Some(v) = l.strip_prefix("ME=") {
            me = Some(v.trim().to_string());
            continue;
        }
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 7 {
            continue;
        }
        rows.push(Proc { pid: f[0].into(), ppid: f[1].into(), pgid: f[2].into(), tpgid: f[3].into(), tty: f[4].into(), user: f[5].into(), comm: f[6..].join(" ") });
    }
    let me = me.filter(|m| !m.is_empty()).ok_or("没能读到远端进程信息")?;
    if rows.is_empty() {
        return Err("远端没有可用的 ps,没法确认终端前台是 sudo".into());
    }
    let has_tty = |t: &str| !matches!(t, "?" | "??" | "-" | "");
    let ttys: Vec<(String, String, String)> = rows.iter().filter(|r| r.ppid == me && has_tty(&r.tty)).map(|r| (r.tty.clone(), r.tpgid.clone(), r.user.clone())).collect();
    if ttys.is_empty() {
        return Err("没找到这条连接上的终端".into());
    }
    Ok((rows, ttys))
}

pub fn sudo_at_prompt(out: &str) -> Result<Fg, String> {
    let (rows, ttys) = parse_ps(out)?;
    for (tty, fg, _) in ttys {
        for r in rows.iter().filter(|r| r.tty == tty && r.pgid == fg) {
            let kind = match r.comm.as_str() {
                "sudo" => Fg::Sudo,
                "su" => Fg::Su,
                _ => continue,
            };
            if r.user == "root" && !rows.iter().any(|c| c.ppid == r.pid) {
                return Ok(kind);
            }
        }
    }
    Err("终端前台不是正在等密码的 sudo 或 su".into())
}

/// 终端前台现在是不是一个 root 的 shell,而登录用户不是 root(也就是 `su` / `sudo -i` 之后 agent 的命令都以 root 运行)。
/// 读不到进程表按「不是」处理,这只是给用户看的提示,不影响放行。
pub fn root_shell_active(out: &str) -> bool {
    const SHELLS: [&str; 9] = ["bash", "sh", "zsh", "dash", "fish", "ksh", "tcsh", "csh", "ash"];
    let Ok((rows, ttys)) = parse_ps(out) else { return false };
    ttys.iter().any(|(tty, fg, login)| login != "root" && rows.iter().any(|r| &r.tty == tty && &r.pgid == fg && r.user == "root" && SHELLS.contains(&r.comm.as_str())))
}

/// agent 最近的命令会不会打开一个 root shell(之后的命令都以 root 运行、不再询问):
/// `su`、`su -`、`su root`(没有 -c);`sudo -i`、`sudo -s`、`sudo su`、`sudo bash` 等。`su -c '…'`、`sudo <命令>` 只提权这一条,不算。
pub fn opens_root_shell(cmd: &str) -> bool {
    const SHELLS: [&str; 9] = ["bash", "sh", "zsh", "dash", "fish", "ksh", "tcsh", "csh", "ash"];
    // 带参数值的 sudo 选项:值要跳过,免得把用户名当成命令
    const SUDO_ARG: [&str; 9] = ["-u", "-g", "-h", "-p", "-C", "-r", "-t", "-U", "-D"];
    fn su_opens(args: &[&str]) -> bool {
        !args.iter().any(|a| *a == "--command" || a.starts_with("--command=") || (a.starts_with('-') && !a.starts_with("--") && a.contains('c')))
    }
    cmd.split(['&', ';', '|', '\n']).any(|seg| {
        let toks: Vec<&str> = seg.split_whitespace().collect();
        let Some(i) = toks.iter().position(|t| *t == "su" || *t == "sudo") else { return false };
        if toks[i] == "su" {
            return su_opens(&toks[i + 1..]);
        }
        let mut k = i + 1;
        while k < toks.len() {
            let t = toks[k];
            if t == "--login" || t == "--shell" || (t.starts_with('-') && !t.starts_with("--") && (t.contains('i') || t.contains('s'))) {
                return true;
            }
            if SUDO_ARG.contains(&t) {
                k += 2;
                continue;
            }
            if t.starts_with('-') {
                k += 1;
                continue;
            }
            // 第一个不是选项的词就是要执行的命令
            return if t == "su" { su_opens(&toks[k + 1..]) } else { SHELLS.contains(&t) };
        }
        false
    })
}

/// 把 Otter 代为输入过的密码从要交给 agent 的文本里抹掉(换成 `••••`)。
/// 密码可能因为终端换行被拆成两行,所以字符之间允许夹一个换行。agent 无论如何都不该看到密码。
pub fn redact_secrets(secrets: &[Zeroizing<String>], text: String) -> String {
    let mut out = text;
    for sec in secrets.iter().filter(|s| !s.is_empty()) {
        let pat = sec.chars().map(|c| regex::escape(&c.to_string())).collect::<Vec<_>>().join("\n?");
        if let Ok(re) = regex::Regex::new(&pat) {
            out = re.replace_all(&out, "••••").into_owned();
        }
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
        // 来源没验证出来(parent_exe 为空)的客户端,不能用长期授权
        let granted = !client.parent_exe.is_empty() && self.store.as_ref().is_some_and(|s| s.grant_has(&client.name, &client.parent_exe, node));
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
        // 节点保存的登录密码:agent 不该看到,先记下来供脱敏用(没有节点或没保存就空)
        let saved_pw: Vec<Zeroizing<String>> = self.workspace.tabs[ix].node().and_then(|n| self.conn_for(n).auth.password).into_iter().collect();
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
            typed: saved_pw,
            last_cmd: String::new(),
            root_shell: false,
            rs_checked: now,
            rs_busy: false,
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
        // 来源没验证出来的客户端只能「允许一次」,即使界面被绕过也不落长期授权
        let d = if p.client.parent_exe.is_empty() && matches!(d, Decision::Day | Decision::Always) { Decision::Once } else { d };
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
                if execute {
                    if let Some(t) = self.workspace.tabs[ix].takeover.as_mut() {
                        t.last_cmd = clip(&text.replace(['\n', '\r'], " "), 200);
                    }
                }
                let shown = clip(&text.replace(['\n', '\r'], "⏎"), LOG_TEXT_MAX);
                alog(format!("→ {node} 输入:{shown}{}", if execute { " ⏎" } else { "" }));
                set_action(self, format!("{}:{}", if execute { "执行" } else { "输入" }, clip(&text.replace(['\n', '\r'], " "), 40)));
                let mut r = json!({ "sent": text.len(), "executed": execute });
                if let Some(mut m) = mark {
                    // 锚点是光标行的原文,可能含刚输入的密码:返回给 agent 前先抹掉
                    if let Some(a) = m.get("anchor").and_then(Value::as_str).map(String::from) {
                        m["anchor"] = json!(self.redact_for(ix, a));
                    }
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
                // 整份(含滚动历史)先抹密码再取屏幕那一段,密码被换行拆在屏幕上沿时也抹得掉
                let redacted: Vec<String> = self.redact_for(ix, snap.lines.join("\n")).split('\n').map(String::from).collect();
                let screen_start = snap.screen_start.saturating_sub(snap.lines.len().saturating_sub(redacted.len()));
                let lines = trim_trailing_blank(redacted[screen_start.min(redacted.len())..].to_vec());
                let text = lines.join("\n");
                set_action(self, "读取屏幕".into());
                let _ = reply.send(Ok(json!({
                    "text": limit_output(text),
                    "cursor": { "line": snap.cursor_line, "col": snap.cursor_col },
                    "cols": cols, "rows": rows,
                })));
            }
            "read_scrollback" => {
                let n = params.get("lines").and_then(Value::as_u64).unwrap_or(100).clamp(1, SCROLLBACK_MAX as u64) as usize;
                let snap = term.view.read(cx).text_snapshot(SCROLLBACK_MAX);
                let all = trim_trailing_blank(snap.lines);
                // 先对整段历史抹密码再截取最近 n 行:密码被换行拆在截取边界两侧时才抹得掉
                let all: Vec<String> = self.redact_for(ix, all.join("\n")).split('\n').map(String::from).collect();
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
                    let text = this.redact_for(ix, trim_trailing_blank(snap.lines).join("\n"));
                    if text != last_text {
                        last_text = text.clone();
                        last_change = Instant::now();
                    }
                    // 给了 since 就只看这条命令之后的新输出:正则也只在新输出里匹配,免得被屏幕上旧的提示符骗过去
                    let new_out = since.as_ref().and_then(|m| output_since(&term.view.read(cx).text_snapshot(SCROLLBACK_MAX), m)).map(|o| this.redact_for(ix, o));
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
            } else if k.started.elapsed() >= max_duration() {
                ending.push((t.id, "max_duration"));
            } else if !k.waiting && k.last_call.elapsed() >= idle_timeout() {
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
        // 每 5 秒在服务器上看一眼终端前台是不是 root shell
        let due: Vec<u64> = self.workspace.tabs.iter().filter(|t| t.takeover.as_ref().is_some_and(|k| !k.rs_busy && k.rs_checked.elapsed() >= Duration::from_secs(5))).map(|t| t.id).collect();
        for id in due {
            self.check_root_shell(id, cx);
        }
    }

    /// 经同一条 SSH 连接的 exec 通道查前台进程,结果记在 `Takeover.root_shell`
    fn check_root_shell(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_ix_by_id(tab_id) else { return };
        if let Some(k) = self.workspace.tabs[ix].takeover.as_mut() {
            k.rs_checked = Instant::now();
        }
        let Some(session) = self.workspace.tabs[ix].term.as_ref().and_then(|t| t.handle.session()) else { return };
        if let Some(k) = self.workspace.tabs[ix].takeover.as_mut() {
            k.rs_busy = true;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        crate::terminal::ssh::runtime().spawn(async move {
            let r = crate::docker::exec(&session, SUDO_FG_SCRIPT, None, Duration::from_secs(8)).await;
            let _ = tx.send(r.map(|o| o.stdout));
        });
        cx.spawn(async move |this, cx| {
            let out = rx.await.unwrap_or_else(|_| Err("中断".into()));
            let _ = this.update(cx, |this, cx| {
                if let Some(ix) = this.tab_ix_by_id(tab_id) {
                    if let Some(k) = this.workspace.tabs[ix].takeover.as_mut() {
                        k.rs_busy = false;
                        let now = out.map(|o| root_shell_active(&o)).unwrap_or(false);
                        if k.root_shell != now {
                            k.root_shell = now;
                            alog(if now { "终端进入了 root shell".to_string() } else { "终端退出了 root shell".to_string() });
                            cx.notify();
                        }
                    }
                }
            });
        })
        .detach();
    }

    /// ⌘. :停止当前标签上的接管。返回是否处理了这次按键。
    pub(crate) fn takeover_hotkey(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(a) = self.workspace.active else { return false };
        let Some(id) = self.workspace.tabs[a].takeover.as_ref().map(|_| self.workspace.tabs[a].id) else { return false };
        let m = &ev.keystroke.modifiers;
        if ev.keystroke.key == "escape" || (crate::platform::accel(m) && ev.keystroke.key == ".") {
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
            self.sudo_prompt = Some(SudoPrompt { tab_id, client, reason: "清理 /var/log 下的旧日志".into(), line: "[sudo] password for chen:".into(), cmd: "sudo rm -rf /var/log/old".into(), kind: Fg::Sudo, input, reply });
        } else if mode.starts_with("prompt") {
            let client = if mode == "prompt-unverified" { Arc::new(Client { id: 0, name: "Claude Code".into(), parent_exe: String::new() }) } else { client };
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
        // 停止按钮:危险操作不做红底,红色描边 + 红字,悬停才出浅红底
        let stop = div()
            .id("takeover-stop")
            .h(px(32.0))
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded(px(t::RADIUS_XS))
            .border_1()
            .border_color(c::danger().opacity(0.45))
            .text_color(c::danger())
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .cursor_pointer()
            .hover(|s| s.bg(c::danger().opacity(0.14)).border_color(c::danger()))
            .child(icon(IconName::OctagonX, 14.0, c::danger()))
            .child("停止接管")
            .on_click(cx.listener(move |this, _, window, cx| this.end_takeover(id, Some("user_stopped"), window, cx)));
        // HUD 风格的卡片,全部静态:不做循环动画(接管可能持续很久,逐帧动画会让整窗每帧重绘,白占 CPU)
        // 中间内容不加底板,保持透明,靠柔和暗影托住可读性。全部静态,不做循环动画(接管可能持续很久,逐帧动画会让整窗每帧重绘,白占 CPU)
        let hue = gpui::Hsla::from(c::accent());
        let mono = |size: f32| div().text_size(px(size)).font_family(crate::MONO).font_weight(FontWeight::BOLD);
        let mark = div()
            .size(px(52.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(t::RADIUS_LG))
            .bg(c::accent().opacity(0.14))
            .border_1()
            .border_color(c::accent().opacity(0.45))
            .shadow(vec![gpui::BoxShadow { color: gpui::hsla(hue.h, 0.9, 0.65, 0.28), offset: point(px(0.0), px(0.0)), blur_radius: px(22.0), spread_radius: px(0.0), inset: false }])
            .child(icon(IconName::Bot, 26.0, c::accent()));
        // 状态行:等宽小字,像终端提示符
        let state = div().max_w(px(TAKEOVER_CARD_W)).truncate().text_size(px(t::TYPE_CAPTION)).font_family(crate::MONO).text_color(c::secondary()).child(format!("› {}", k.last_action));
        // 提醒条:黄色软底
        let warn_bar = |ic: IconName, kids: Div| div().flex().flex_row().items_center().gap(px(8.0)).px(px(10.0)).py(px(7.0)).bg(c::warn_soft()).text_size(px(t::TYPE_MICRO)).text_color(c::warn()).child(icon(ic, 12.0, c::warn())).child(kids);
        let hint = div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child("Esc 也可以停止 · 停止不会中断正在运行的命令");
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
                        .px(px(24.0))
                        .py(px(8.0))
                        .max_w(relative(0.92))
                        .shadow(crate::ui::shadow_overlay_content())
                        .child(mark)
                        // 身份:标题和 agent 名字紧挨着,是一组
                        .child(div().mt(px(14.0)).text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).text_color(c::text()).child("Agent 接管中"))
                        .child(mono(t::TYPE_CONTROL).mt(px(4.0)).text_color(gpui::hsla(hue.h, 0.85, 0.78, 1.0)).child(k.client_name.clone()))
                        // 它正在做什么:和身份隔开一段
                        .child(div().mt(px(12.0)).child(state))
                        .children(k.root_shell.then(|| div().id("takeover-root-shell").mt(px(14.0)).child(warn_bar(IconName::TriangleAlert, div().child("终端当前是 root shell,agent 的命令以 root 运行")))))
                        .children(k.sudo.as_ref().map(|g| {
                            let mins = g.until.saturating_duration_since(Instant::now()).as_secs().div_ceil(60);
                            div().id("takeover-sudo").mt(px(14.0)).child(warn_bar(
                                IconName::KeyRound,
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap(px(8.0))
                                    .child(format!("已授权提权,剩余约 {mins} 分钟"))
                                    .child(div().id("takeover-sudo-revoke").cursor_pointer().text_color(c::muted()).hover(|s| s.text_color(c::text())).child("撤销").on_click(cx.listener(move |this, _, _, cx| this.revoke_sudo(id, cx)))),
                            ))
                        }))
                        // 操作:按钮和提示是一组,离上面的信息再远一点
                        .child(div().mt(px(20.0)).child(stop))
                        .child(div().mt(px(10.0)).child(hint)),
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
        let verified = !p.client.parent_exe.is_empty();
        if verified {
            rows.push((IconName::Terminal, c::muted(), format!("来自 {}", p.client.parent_exe)));
        } else {
            rows.push((IconName::TriangleAlert, c::warn(), "无法确认调用方来自哪个程序,只能允许这一次".into()));
        }
        rows.push((IconName::Keyboard, c::warn(), "能在终端里输入命令、读取屏幕".into()));
        rows.push((IconName::ScanEye, c::warn(), "能看到终端里已有的历史内容".into()));
        rows.push((IconName::OctagonX, c::online(), "随时点「停止接管」或按 Esc 中止".to_string()));
        let points = crate::ui::alert_rows(rows);
        // 两排:上面三个允许选项等宽(「允许一次」是默认项,主色,回车也触发;「24 小时」「总是允许」是更重的决定,不抢眼),下面整行宽的「拒绝」
        let btn_w = crate::ui::alert_btn_w(3);
        let choice = |id: &'static str, label: &'static str, d: Decision, cx: &mut Context<Self>| {
            crate::node_form::ctl_btn(id, None, label, 38.0).w(px(btn_w)).flex_none().justify_center().on_click(cx.listener(move |this, _, window, cx| this.resolve_takeover_prompt(d, window, cx)))
        };
        let deny = crate::node_form::ctl_btn("tp-deny", None, "拒绝", 38.0).w_full().flex_none().justify_center().on_click(cx.listener(|this, _, window, cx| this.resolve_takeover_prompt(Decision::Deny, window, cx)));
        let once_w = if verified { btn_w } else { crate::ui::alert_btn_w(1) };
        let once = crate::ui::alert_primary_btn("tp-once", None, "允许一次", Some(once_w)).on_click(cx.listener(|this, _, window, cx| this.resolve_takeover_prompt(Decision::Once, window, cx)));
        let buttons = if verified {
            let day = choice("tp-day", "24 小时", Decision::Day, cx);
            let always = choice("tp-always", "总是允许", Decision::Always, cx);
            let hint = div().w_full().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).text_center().child(format!("「24 小时」「总是允许」只对 {} 生效,可在「信任」页撤销", node.name));
            div().w_full().flex().flex_col().gap(px(10.0)).child(div().w_full().flex().flex_row().gap(px(10.0)).child(always).child(day).child(once)).child(deny).child(hint)
        } else {
            div().w_full().flex().flex_col().gap(px(10.0)).child(once).child(deny)
        };
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
    /// 这个标签上不能让 agent 看到的密码:接管开始时记下的节点登录密码,加上 Otter 代为输入过的。
    fn agent_secrets(&self, ix: usize) -> Vec<Zeroizing<String>> {
        self.workspace.tabs[ix].takeover.as_ref().map(|t| t.typed.clone()).unwrap_or_default()
    }

    /// 交给 agent 的文本一律先过这里。
    fn redact_for(&self, ix: usize, text: String) -> String {
        redact_secrets(&self.agent_secrets(ix), text)
    }

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
            let _ = reply.send(Err(RpcError::new("no_sudo_prompt", "屏幕上没有 sudo/su 密码提示。先 send_input 执行带 sudo 或 su 的命令,等屏幕出现「[sudo] password for …」或「Password:」再调用")));
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
        // 免确认代填只在 agent 最近执行的命令里确实有 sudo 时才用:屏幕上的「密码提示」可以被 agent 的脚本伪造,
        // 命令里没有 sudo 就还是弹确认框,让用户看着命令和提示行决定
        let cmd = self.workspace.tabs[ix].takeover.as_ref().map(|t| t.last_cmd.clone()).unwrap_or_default();
        let has_sudo = regex::Regex::new(r"\bsudo\b").is_ok_and(|r| r.is_match(&cmd));
        // 命令里是 su(没有 sudo)就按 su 处理:要 root 密码,只能让用户输入,不走授权窗口;弄错了的话填写前的远端校验会拒绝
        let kind = if !has_sudo && regex::Regex::new(r"\bsu\b").is_ok_and(|r| r.is_match(&cmd)) { Fg::Su } else { Fg::Sudo };
        if kind == Fg::Sudo && remaining.is_some_and(|r| !r.is_zero()) && has_sudo {
            let typed = self.workspace.tabs[ix].takeover.as_ref().and_then(|t| t.sudo.as_ref()).and_then(|g| g.password.clone());
            let left = remaining.map_or(0, |r| r.as_secs());
            self.sudo_fill_verified(ix, Fg::Sudo, typed, cx, move |_, res, _| match res {
                Ok(()) => {
                    let _ = reply.send(Ok(json!({ "filled": true, "granted": "window", "remaining_s": left })));
                }
                Err(e) => {
                    let _ = reply.send(Err(RpcError::new("sudo_failed", e)));
                }
            });
            return;
        }
        // 节点没保存密码:确认框里让用户输入
        let saved = self.conn_for(node).auth.password.is_some();
        let input = (!saved || kind == Fg::Su).then(|| cx.new(|cx| InputState::new(window, cx).placeholder(if kind == Fg::Su { "root 密码" } else { "sudo 密码" }).masked(true)));
        if let Some(i) = &input {
            let fh = i.focus_handle(cx);
            window.focus(&fh, cx);
        }
        let reason = clip(params.get("reason").and_then(Value::as_str).unwrap_or("").trim(), 200);
        alog(format!("{} 申请在 {} 提权({})", client.name, self.node_name(Some(node)), kind.name()));
        self.sudo_prompt = Some(SudoPrompt { tab_id: sid, client: client.clone(), reason, line, cmd, kind, input, reply });
        cx.activate(true);
        self.activate(ix);
        cx.notify();
    }

    /// 先在远端验证终端前台确实是等密码的 sudo(`sudo_at_prompt`),通过了才把密码写进终端,结果交给 `then`。
    /// 验证用同一条 SSH 连接另开的 exec 通道,不碰终端里的内容。
    fn sudo_fill_verified(&mut self, ix: usize, expect: Fg, typed: Option<Zeroizing<String>>, cx: &mut Context<Self>, then: impl FnOnce(&mut Self, Result<(), String>, &mut Context<Self>) + 'static) {
        let tab_id = self.workspace.tabs[ix].id;
        // su 要的是 root 密码,不能拿节点保存的登录密码去填
        if expect == Fg::Su && typed.is_none() {
            then(self, Err("su 需要 root 密码".into()), cx);
            return;
        }
        let Some(session) = self.workspace.tabs[ix].term.as_ref().and_then(|t| t.handle.session()) else {
            then(self, Err("终端还没有连上".into()), cx);
            return;
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        crate::terminal::ssh::runtime().spawn(async move {
            let r = crate::docker::exec(&session, SUDO_FG_SCRIPT, None, Duration::from_secs(8)).await;
            let _ = tx.send(r.map(|o| o.stdout));
        });
        cx.spawn(async move |this, cx| {
            let out = rx.await.unwrap_or_else(|_| Err("验证被中断".into()));
            let _ = this.update(cx, |this, cx| {
                let checked = out.and_then(|o| sudo_at_prompt(&o)).and_then(|fg| if fg == expect { Ok(()) } else { Err(format!("终端前台是 {},不是申请时说的 {}", fg.name(), expect.name())) });
                let res = match (checked, this.tab_ix_by_id(tab_id)) {
                    (Err(e), _) => {
                        alog(format!("没有代填 sudo 密码:{e}"));
                        Err(format!("没有填密码:{e}。屏幕上的提示可能不是 sudo/su 发出的"))
                    }
                    (Ok(()), None) => Err("标签已关闭".into()),
                    (Ok(()), Some(ix)) => this.sudo_fill(ix, typed, cx),
                };
                then(this, res, cx);
                cx.notify();
            });
        })
        .detach();
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
            if !t.typed.iter().any(|x| **x == *pw) {
                t.typed.push(pw.clone());
            }
            t.last_action = "提权:已代为输入密码".into();
        }
        alog(format!("→ {name} 已代为输入密码"));
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
        let (client_name, reply, tab_id, kind) = (p.client.name.clone(), p.reply, p.tab_id, p.kind);
        let typed2 = typed.clone();
        self.sudo_fill_verified(ix, kind, typed, cx, move |this, res, cx| {
            match res {
                Ok(()) => {
                    if let (Some(d), Some(ix), Fg::Sudo) = (scope.window(), this.tab_ix_by_id(tab_id), kind) {
                        if let Some(t) = this.workspace.tabs[ix].takeover.as_mut() {
                            t.sudo = Some(SudoGrant { until: Instant::now() + d, password: typed2 });
                        }
                    }
                    alog(format!("用户允许 {} 在 {} 提权({})", client_name, node_name, scope.label()));
                    let _ = reply.send(Ok(json!({ "filled": true, "granted": scope.label() })));
                }
                Err(e) => {
                    let _ = reply.send(Err(RpcError::new("sudo_failed", e)));
                }
            }
            cx.notify();
        });
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
        let hero = crate::ui::alert_hero(IconName::KeyRound, c::warn(), format!("允许 {} 提权？", p.client.name), if p.kind == Fg::Su { "由 Otter 代为输入你填的 root 密码,agent 看不到密码" } else { "由 Otter 代为输入 sudo 密码,agent 看不到密码" });
        let target = Self::node_target_card(node);
        let mut rows: Vec<(IconName, Rgba, String)> = vec![];
        if !p.reason.is_empty() {
            rows.push((IconName::Sparkles, c::accent(), format!("原因:{}", p.reason)));
        }
        if !p.cmd.is_empty() {
            rows.push((IconName::Terminal, c::accent(), format!("agent 刚执行的命令:{}", p.cmd)));
        }
        rows.push((IconName::Terminal, c::muted(), format!("屏幕上的提示:{}", p.line)));
        if opens_root_shell(&p.cmd) {
            rows.push((IconName::TriangleAlert, c::danger(), "这会打开 root shell:同意后 agent 之后的所有命令都以 root 运行,不会再询问,直到它退出。只想授权一条命令的话,请拒绝并让它改用 su -c '…' 或 sudo <命令>".into()));
        }
        rows.push((IconName::KeyRound, c::warn(), if p.kind == Fg::Su { "su 要的是 root 密码,请在下面输入;只用这一次,不会保存".into() } else if p.input.is_some() { "这个节点没有保存密码,请在下面输入".into() } else { "将使用节点里保存的密码".into() }));
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
            .child(if crate::prefs::sudo_allow_window() && p.kind == Fg::Sudo {
                div().w_full().flex().flex_row().gap(px(10.0)).child(m30).child(m10).child(once)
            } else {
                // 设置里关了「允许一段时间内免确认」:每次都要确认,只留「仅这一次」
                div().w_full().flex().flex_row().gap(px(10.0)).child(once.w(px(crate::ui::alert_btn_w(1))))
            })
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
    use super::{is_sudo_prompt, opens_root_shell, redact_secrets, root_shell_active, sudo_at_prompt, Fg, OUTPUT_MAX, Takeover, key_bytes, limit_output, new_lease, trim_trailing_blank};
    use zeroize::Zeroizing;
    use crate::ctl::Client;
    use serde_json::json;
    use std::time::Instant;

    fn client(id: u64) -> Client {
        Client { id, name: "c".into(), parent_exe: String::new() }
    }

    fn takeover(client_id: u64, lease: Option<&str>) -> Takeover {
        let now = Instant::now();
        Takeover { client_id, client_name: "c".into(), started: now, last_call: now, last_action: String::new(), waiting: false, lease: lease.map(String::from), sudo: None, typed: Vec::new(), last_cmd: String::new(), root_shell: false, rs_checked: now, rs_busy: false }
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
    fn sudo_must_be_the_foreground_process_as_root() {
        // 列:pid ppid pgid tpgid tty user comm
        let head = "ME=500\n  500 1 500 500 ? chen sshd\n  600 500 600 700 pts/0 chen bash\n";
        // 真的停在 sudo 提示上:前台进程组 700 里有 root 的 sudo,没有子进程
        assert!(sudo_at_prompt(&format!("{head}  700 600 700 700 pts/0 root sudo\n")).is_ok());
        // agent 自己的脚本在前台(普通用户):拒绝
        assert!(sudo_at_prompt(&format!("{head}  700 600 700 700 pts/0 chen sh\n")).is_err());
        // 普通用户自己拷了一份叫 sudo 的程序:不是 root,拒绝
        assert!(sudo_at_prompt(&format!("{head}  700 600 700 700 pts/0 chen sudo\n")).is_err());
        // sudo 已经认证通过并起了命令(有子进程):输入会交给那个命令,拒绝
        assert!(sudo_at_prompt(&format!("{head}  700 600 700 700 pts/0 root sudo\n  701 700 700 700 pts/1 root sh\n")).is_err());
        // 别的连接(ppid 不同)上的终端不算
        assert!(sudo_at_prompt("ME=999\n  600 500 600 700 pts/0 chen bash\n  700 600 700 700 pts/0 root sudo\n").is_err());
        // su 停在密码提示上也认,并告诉调用方是 su
        assert_eq!(sudo_at_prompt(&format!("{head}  700 600 700 700 pts/0 root su\n")), Ok(Fg::Su));
        assert_eq!(sudo_at_prompt(&format!("{head}  700 600 700 700 pts/0 root sudo\n")), Ok(Fg::Sudo));
        assert!(sudo_at_prompt(&format!("{head}  700 600 700 700 pts/0 chen su\n")).is_err());
        // 看不懂输出:拒绝
        assert!(sudo_at_prompt("").is_err());
        assert!(sudo_at_prompt("ME=500\n").is_err());
    }

    #[test]
    fn detects_commands_that_open_a_root_shell() {
        for c in ["su", "su -", "su root", "su - root", "sudo -i", "sudo -s", "sudo su", "sudo su -", "sudo bash", "sudo -u root -i", "sudo -iu postgres", "cd /x && su", "sudo --login", "sudo -u app bash"] {
            assert!(opens_root_shell(c), "{c}");
        }
        for c in ["su -c 'apt-get update'", "su root -c ls", "su -lc 'id'", "sudo apt-get update", "sudo -u postgres psql", "sudo systemctl restart nginx", "echo hi", "sudo -n true", "su --command=id", "ls /usr/bin/sudoku"] {
            assert!(!opens_root_shell(c), "{c}");
        }
    }

    #[test]
    fn root_shell_shown_only_after_login_user_became_root() {
        let head = "ME=500\n  500 1 500 500 ? chen sshd\n  600 500 600 700 pts/0 chen bash\n";
        // 前台进程组里是 root 的 bash:登录用户是 chen,说明 su/sudo -i 之后
        assert!(root_shell_active(&format!("{head}  700 600 700 700 pts/0 root bash\n")));
        // 前台是普通命令或还在等密码的 su:不算
        assert!(!root_shell_active(&format!("{head}  700 600 700 700 pts/0 root su\n")));
        assert!(!root_shell_active(&format!("{head}  700 600 700 700 pts/0 chen vim\n")));
        assert!(!root_shell_active(head));
        // 本来就是 root 登录:不提示
        assert!(!root_shell_active("ME=500\n  500 1 500 500 ? root sshd\n  600 500 600 600 pts/0 root bash\n"));
        assert!(!root_shell_active(""));
    }

    #[test]
    fn typed_passwords_are_redacted_for_agents() {
        let sec = vec![Zeroizing::new("Yi#2026@".to_string())];
        assert_eq!(redact_secrets(&sec, "[sudo] 密码:\nYi#2026@\nok".into()), "[sudo] 密码:\n••••\nok");
        // 被终端换行拆开的密码也要抹掉
        assert_eq!(redact_secrets(&sec, "x Yi#20\n26@ y".into()), "x •••• y");
        assert_eq!(redact_secrets(&[], "abc".into()), "abc");
        assert_eq!(redact_secrets(&[Zeroizing::new(String::new())], "abc".into()), "abc");
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
