//! SSH 会话:russh 跑在后台 tokio 运行时里,和界面之间用 channel 传字节。
//!
//! 对外是一对阻塞的 `Read`/`Write`(喂给 `gpui_xterm::TerminalView::new`)、
//! 一个调整行列的句柄和一个状态。密码、口令在终端里以提示行的形式输入;主机密钥确认由「连接中」页的卡片完成(`Handle::host_prompt/answer_host`)。

use crate::logs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use zeroize::Zeroizing;

use russh::client;
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use tokio::sync::mpsc as tokio_mpsc;

/// 私钥认证材料(PEM 文本和可选口令),用完即清零。
#[derive(Clone)]
pub struct KeyAuth {
    pub pem: Zeroizing<String>,
    pub passphrase: Option<Zeroizing<String>>,
    /// 来源标签(导入时的路径),只用于日志
    pub label: String,
}

/// 一个节点的认证方式,可叠加;连接时依次尝试 私钥 → SSH Agent → 密码。
/// 三项都没有时,连接时在终端里问密码。
#[derive(Clone, Default)]
pub struct Auth {
    pub key: Option<KeyAuth>,
    pub agent: bool,
    pub password: Option<Zeroizing<String>>,
}

impl std::fmt::Debug for Auth {
    // 不输出任何秘密内容
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Auth {{ key: {}, agent: {}, password: {} }}", self.key.is_some(), self.agent, self.password.is_some())
    }
}

impl Auth {
    fn describe(&self) -> String {
        let mut v = Vec::new();
        if let Some(k) = &self.key {
            v.push(format!("私钥 {}", k.label));
        }
        if self.agent {
            v.push("SSH Agent".into());
        }
        if self.password.is_some() {
            v.push("密码".into());
        }
        if v.is_empty() { "密码(手动输入)".into() } else { v.join("、") }
    }
}

#[derive(Clone, Debug)]
pub struct Conn {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: Auth,
    /// 读取已保存的认证信息失败(如系统凭据库不可用);连接时直接报这个错
    pub auth_error: Option<String>,
}

impl Conn {
    /// 是否有可自动使用的认证(私钥 / Agent / 已保存的密码);没有就要在终端里手输密码,不能自动重连。
    pub fn has_saved_auth(&self) -> bool {
        self.auth.key.is_some() || self.auth.agent || self.auth.password.is_some()
    }

    /// 从 `user@host` / `user@host:port` 解析,认证默认 SSH Agent。
    pub fn parse(user_host: &str) -> Conn {
        let (user, rest) = user_host.split_once('@').unwrap_or(("root", user_host));
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse().unwrap()),
            _ => (rest, 22),
        };
        Conn { host: host.into(), port, user: user.into(), auth: Auth { agent: true, ..Default::default() }, auth_error: None }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    /// 正在终端里等用户输入(确认指纹 / 密码 / 口令)
    Waiting,
    Connected,
    /// 连接中断后正在自动重连(第几次);终端画面保留,界面盖一层加载状态
    Reconnecting(u32),
    Closed(Option<String>),
}

/// 首次连接时等用户确认的主机密钥,界面的「连接中」页据此显示确认卡片。
#[derive(Clone, Debug, PartialEq)]
pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    pub algo: String,
    pub fingerprint: String,
}

/// 终端已认证的 SSH 连接,连上后放在这里;SFTP 直接在它上面开通道,不用再连一次。
pub(crate) type Link = Arc<Mutex<Option<Arc<client::Handle<Client>>>>>;

/// 一次会话结束的原因
enum End {
    /// shell 正常退出(用户 exit / 服务器关闭了 shell)
    Exited,
    /// 连接意外中断(网络断开、服务器重启),可以自动重连
    Lost,
    /// 界面丢弃了句柄
    Dropped,
}

/// 自动重连的最多次数和每次之前等待的秒数
const RECONNECT_DELAYS: [u64; 6] = [1, 2, 4, 8, 15, 30];

/// 待确认的主机密钥 + 回复通道。
pub(crate) type PendingHost = Arc<Mutex<Option<(HostKeyPrompt, Option<tokio::sync::oneshot::Sender<bool>>)>>>;

/// 连接过程里的一步,界面的「连接中」页据此逐步展示。
#[derive(Clone, Debug, PartialEq)]
pub enum StepState {
    Running,
    Done,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub label: &'static str,
    pub state: StepState,
}

/// 连接要走的步骤,顺序固定。
pub const STEP_LABELS: [&str; 3] = ["连接服务器", "身份认证", "打开终端"];

/// 后台 tokio 运行时,整个进程一个。
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().thread_name("ssh").build().expect("tokio runtime")
    })
}

/// 终端读到的字节(阻塞读)。会话结束、发送端释放后返回 0,终端据此显示退出。
pub struct Output {
    rx: std_mpsc::Receiver<Vec<u8>>,
    left: Vec<u8>,
}

impl Read for Output {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left.is_empty() {
            match self.rx.recv() {
                Ok(v) => self.left = v,
                Err(_) => return Ok(0),
            }
        }
        let n = self.left.len().min(buf.len());
        buf[..n].copy_from_slice(&self.left[..n]);
        self.left.drain(..n);
        Ok(n)
    }
}

/// 终端写出的字节(按键、粘贴)。
pub struct Input(tokio_mpsc::UnboundedSender<Vec<u8>>);

impl Write for Input {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // 会话已结束时丢弃输入,不报错
        let _ = self.0.send(buf.to_vec());
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 界面持有的一端。丢弃它就会断开连接。
pub struct Handle {
    input: tokio_mpsc::UnboundedSender<Vec<u8>>,
    resize: tokio_mpsc::UnboundedSender<(u16, u16)>,
    pub status: Arc<Mutex<Status>>,
    pub steps: Arc<Mutex<Vec<Step>>>,
    host_prompt: PendingHost,
    ever: Arc<AtomicBool>,
    epoch: Arc<AtomicU32>,
    link: Link,
    abort: tokio::task::AbortHandle,
}

impl Handle {
    /// 当前可用的已认证连接(没连上、重连中为 None)
    pub(crate) fn session(&self) -> Option<Arc<client::Handle<Client>>> {
        self.link.lock().ok().and_then(|g| g.clone())
    }
    /// 每次(重新)连上加一;SFTP 据此知道旧通道已随旧连接失效
    pub fn generation(&self) -> u32 {
        self.epoch.load(Ordering::Relaxed)
    }
    /// 往会话里直接写字节(用来补发终端组件漏掉的按键)
    pub fn inputter(&self) -> tokio_mpsc::UnboundedSender<Vec<u8>> {
        self.input.clone()
    }
    pub fn resizer(&self) -> tokio_mpsc::UnboundedSender<(u16, u16)> {
        self.resize.clone()
    }
    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or(Status::Closed(None))
    }
    /// 正在等用户确认的新主机指纹
    pub fn host_prompt(&self) -> Option<HostKeyPrompt> {
        self.host_prompt.lock().ok().and_then(|g| g.as_ref().map(|(p, _)| p.clone()))
    }
    /// 回答主机指纹确认:`true` 信任并继续,`false` 取消连接
    pub fn answer_host(&self, yes: bool) {
        if let Ok(mut g) = self.host_prompt.lock() {
            if let Some(tx) = g.as_mut().and_then(|(_, tx)| tx.take()) {
                let _ = tx.send(yes);
            }
        }
    }
    pub fn steps(&self) -> Vec<Step> {
        self.steps.lock().map(|s| s.clone()).unwrap_or_default()
    }
    /// 是否进过 shell。没进过就是还在连或者连接失败,界面显示「连接中」页;进过之后断线只是盖一层重连状态。
    pub fn ever_connected(&self) -> bool {
        self.ever.load(Ordering::Relaxed)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

pub fn connect(conn: Conn, cols: usize, rows: usize) -> (Input, Output, Handle) {
    let (out_tx, out_rx) = std_mpsc::channel::<Vec<u8>>();
    let (in_tx, in_rx) = tokio_mpsc::unbounded_channel::<Vec<u8>>();
    let (rs_tx, rs_rx) = tokio_mpsc::unbounded_channel::<(u16, u16)>();
    let status = Arc::new(Mutex::new(Status::Connecting));
    let steps = Arc::new(Mutex::new(Vec::new()));
    let host_prompt: PendingHost = Arc::new(Mutex::new(None));
    let st = status.clone();
    let steps_io = steps.clone();
    let hp_io = host_prompt.clone();
    let ever = Arc::new(AtomicBool::new(false));
    let epoch = Arc::new(AtomicU32::new(0));
    let link: Link = Arc::new(Mutex::new(None));
    let (ever_io, epoch_io, link_io) = (ever.clone(), epoch.clone(), link.clone());
    let join = runtime().spawn(async move {
        let io = Io { out: out_tx, inp: Arc::new(tokio::sync::Mutex::new(in_rx)), status: st.clone(), steps: steps_io, host_prompt: hp_io, ever: ever_io, epoch: epoch_io, link: link_io };
        let label = format!("{}@{}:{}", conn.user, conn.host, conn.port);
        logs::push(logs::Level::Info, &label, "开始连接");
        let size = Arc::new(Mutex::new((cols as u16, rows as u16)));
        let mut rs_rx = rs_rx;
        let mut attempt = 0usize;
        let reason = loop {
            let epoch0 = io.epoch.load(Ordering::Relaxed);
            let (c, r) = size.lock().map(|g| *g).unwrap_or((80, 24));
            let res = run(&conn, c, r, io.clone(), &mut rs_rx, &size).await;
            if io.epoch.load(Ordering::Relaxed) != epoch0 {
                attempt = 0; // 这一轮连上过,重新计数
            }
            let lost = match res {
                Ok(End::Exited) => {
                    logs::push(logs::Level::Info, &label, "连接已关闭");
                    break None;
                }
                Ok(End::Dropped) => break None,
                Ok(End::Lost) => "连接中断".to_string(),
                Err(e) => e,
            };
            // 没进过 shell、认证/指纹问题、没有可自动使用的认证、次数用完:不再自动重连
            let retry = io.ever.load(Ordering::Relaxed) && conn.has_saved_auth() && !io.failed_at_auth() && !is_fatal(&lost) && attempt < RECONNECT_DELAYS.len();
            if !retry {
                logs::push(logs::Level::Error, &label, lost.clone());
                io.say(&format!("\r\n\x1b[31m{lost}\x1b[0m\r\n"));
                io.fail_step(&lost);
                break Some(lost);
            }
            let delay = RECONNECT_DELAYS[attempt];
            attempt += 1;
            set(&st, Status::Reconnecting(attempt as u32));
            logs::push(logs::Level::Warn, &label, format!("{lost},{delay} 秒后自动重连({attempt}/{})", RECONNECT_DELAYS.len()));
            io.say(&format!("\r\n\x1b[33m[{lost}:{delay} 秒后自动重连({attempt}/{})]\x1b[0m\r\n", RECONNECT_DELAYS.len()));
            // 断线期间敲的键不要在重连后一股脑发出去
            {
                let mut g = io.inp.lock().await;
                while g.try_recv().is_ok() {}
            }
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            io.steps.lock().map(|mut g| g.clear()).ok();
            logs::push(logs::Level::Info, &label, "开始连接");
        };
        set(&st, Status::Closed(reason));
        // io 在这里释放,Output 读到 0
    });
    (Input(in_tx.clone()), Output { rx: out_rx, left: Vec::new() }, Handle { input: in_tx, resize: rs_tx, status, steps, host_prompt, ever, epoch, link, abort: join.abort_handle() })
}

/// 重连也没有用的错误:主机指纹问题、认证被拒。
fn is_fatal(e: &str) -> bool {
    ["主机密钥", "未确认新主机", "认证失败", "读取认证信息失败"].iter().any(|k| e.contains(k))
}

fn set(st: &Arc<Mutex<Status>>, s: Status) {
    if let Ok(mut g) = st.lock() {
        *g = s;
    }
}

#[derive(Clone)]
pub(crate) struct Io {
    out: std_mpsc::Sender<Vec<u8>>,
    inp: Arc<tokio::sync::Mutex<tokio_mpsc::UnboundedReceiver<Vec<u8>>>>,
    status: Arc<Mutex<Status>>,
    steps: Arc<Mutex<Vec<Step>>>,
    host_prompt: PendingHost,
    ever: Arc<AtomicBool>,
    epoch: Arc<AtomicU32>,
    link: Link,
}

impl Io {
    /// 没有终端的场景(SFTP):输出和输入都丢弃,只保留主机指纹确认。
    pub(crate) fn headless(host_prompt: PendingHost, steps: Arc<Mutex<Vec<Step>>>) -> Io {
        let (out, _) = std_mpsc::channel();
        let (_, inp) = tokio_mpsc::unbounded_channel();
        Io { out, inp: Arc::new(tokio::sync::Mutex::new(inp)), status: Arc::new(Mutex::new(Status::Connecting)), steps, host_prompt, ever: Arc::new(AtomicBool::new(false)), epoch: Arc::new(AtomicU32::new(0)), link: Arc::new(Mutex::new(None)) }
    }

    /// 挂出主机指纹确认,等界面回答(取消、会话被丢弃都算拒绝)
    async fn confirm_host(&self, prompt: HostKeyPrompt) -> bool {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Ok(mut g) = self.host_prompt.lock() {
            *g = Some((prompt, Some(tx)));
        }
        let ans = rx.await.unwrap_or(false);
        if let Ok(mut g) = self.host_prompt.lock() {
            *g = None;
        }
        ans
    }

    /// 开始下一步
    /// 失败时正停在「身份认证」这一步(重试也没用)
    pub(crate) fn failed_at_auth(&self) -> bool {
        self.steps.lock().map(|g| g.len() == 2 && g.last().is_some_and(|s| s.state == StepState::Running)).unwrap_or(false)
    }

    pub(crate) fn steps_empty(&self) -> bool {
        self.steps.lock().map(|g| g.is_empty()).unwrap_or(true)
    }

    pub(crate) fn begin_step(&self, label: &'static str) {
        if let Ok(mut g) = self.steps.lock() {
            g.push(Step { label, state: StepState::Running });
        }
    }

    /// 当前这步完成
    pub(crate) fn done_step(&self) {
        if let Ok(mut g) = self.steps.lock() {
            if let Some(last) = g.last_mut().filter(|s| s.state == StepState::Running) {
                last.state = StepState::Done;
            }
        }
    }

    /// 当前这步失败并记下原因。已经有失败原因(更具体的)就不覆盖。
    pub(crate) fn fail_step(&self, msg: &str) {
        if let Ok(mut g) = self.steps.lock() {
            if let Some(last) = g.last_mut().filter(|s| s.state == StepState::Running) {
                last.state = StepState::Failed(msg.to_string());
            }
        }
    }

    fn say(&self, text: &str) {
        let _ = self.out.send(text.as_bytes().to_vec());
    }

    /// 在终端里问一行。`echo=false` 不回显(密码)。Ctrl-C 或输入通道关闭返回 None。
    async fn ask(&self, prompt: &str, echo: bool) -> Option<String> {
        self.say(prompt);
        let prev = self.status.lock().map(|s| s.clone()).unwrap_or(Status::Connecting);
        set(&self.status, Status::Waiting);
        let mut line = String::new();
        let mut inp = self.inp.lock().await;
        let result = 'outer: loop {
            let chunk = inp.recv().await?;
            for ch in String::from_utf8_lossy(&chunk).chars() {
                match ch {
                    '\r' | '\n' => {
                        self.say("\r\n");
                        break 'outer Some(std::mem::take(&mut line));
                    }
                    '\u{3}' => {
                        self.say("^C\r\n");
                        break 'outer None;
                    }
                    '\u{7f}' | '\u{8}' => {
                        if line.pop().is_some() && echo {
                            self.say("\x08 \x08");
                        }
                    }
                    c if c.is_control() => {}
                    c => {
                        line.push(c);
                        if echo {
                            self.say(&c.to_string());
                        }
                    }
                }
            }
        };
        set(&self.status, prev);
        result
    }
}

pub(crate) fn known_hosts_file() -> PathBuf {
    let base = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    base.join("Library/Application Support/dockyard/known_hosts")
}

pub(crate) struct Client {
    io: Io,
    host: String,
    port: u16,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            self.io.say("\x1b[31m不支持主机证书\x1b[0m\r\n");
            return Ok(false);
        };
        verify_host(&self.io, &self.host, self.port, key).await
    }
}

/// 主机密钥:已知且一致才放行;首次见到要用户确认;不一致直接拒绝。
async fn verify_host(io: &Io, host: &str, port: u16, key: &PublicKey) -> Result<bool, russh::Error> {
    let path = known_hosts_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::OpenOptions::new().create(true).append(true).open(&path);
    let fp = key.fingerprint(HashAlg::Sha256);
    match keys::known_hosts::check_known_hosts_path(host, port, key, &path) {
        Ok(true) => Ok(true),
        Ok(false) => {
            logs::push(logs::Level::Info, &format!("{host}:{port}"), format!("首次连接,等待确认指纹 {fp}"));
            let prompt = HostKeyPrompt { host: host.to_string(), port, algo: key.algorithm().to_string(), fingerprint: fp.to_string() };
            if io.confirm_host(prompt).await {
                let _ = keys::known_hosts::learn_known_hosts_path(host, port, key, &path);
                logs::push(logs::Level::Info, &format!("{host}:{port}"), format!("已信任新主机 {fp}"));
                Ok(true)
            } else {
                logs::push(logs::Level::Warn, &format!("{host}:{port}"), "未确认新主机,已取消连接");
                io.fail_step("未确认新主机的指纹,已取消连接");
                Ok(false)
            }
        }
        Err(keys::Error::KeyChanged { .. }) => {
            logs::push(logs::Level::Error, &format!("{host}:{port}"), format!("主机密钥与记录不一致,已拒绝连接(新指纹 {fp})"));
            io.fail_step(&format!("主机密钥与已记录的不一致,已拒绝连接(新指纹 {fp})。可能存在中间人攻击;若服务器确实重装过,请到「已知主机」里删除该主机的记录后重试"));
            io.say(&format!(
                "\x1b[1;31m警告:{host}:{port} 的主机密钥与记录不一致!\x1b[0m\r\n新指纹:{fp}\r\n可能存在中间人攻击,已拒绝连接。若服务器确实重装过,请先从\r\n{} 中删除该主机的记录。\r\n",
                path.display()
            ));
            Ok(false)
        }
        Err(e) => {
            io.say(&format!("\x1b[31m读取已知主机失败:{e}\x1b[0m\r\n"));
            Ok(false)
        }
    }
}

pub(crate) fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(path),
    }
}

/// 连接服务器、校验主机密钥、认证,返回已认证的会话(终端和 SFTP 共用)。
/// 过程会通过 `io` 记录步骤和日志;首次连接的指纹确认走 `io.confirm_host`。
pub(crate) async fn open_authenticated(conn: &Conn, io: &Io) -> Result<client::Handle<Client>, String> {
    let config = Arc::new(client::Config { inactivity_timeout: None, keepalive_interval: Some(std::time::Duration::from_secs(15)), ..Default::default() });
    let handler = Client { io: io.clone(), host: conn.host.clone(), port: conn.port };
    let label = format!("{}@{}:{}", conn.user, conn.host, conn.port);
    io.begin_step(STEP_LABELS[0]);
    logs::push(logs::Level::Info, &label, format!("正在连接 {}:{}", conn.host, conn.port));
    let mut session = tokio::time::timeout(std::time::Duration::from_secs(15), client::connect(config, (conn.host.as_str(), conn.port), handler))
        .await
        .map_err(|_| "连接超时".to_string())?
        .map_err(|e| match e {
            russh::Error::UnknownKey => "主机密钥未被接受,连接已中止".to_string(),
            e => format!("连接失败:{e}"),
        })?;

    io.done_step();
    logs::push(logs::Level::Info, &label, "已建立连接,主机密钥校验通过");
    io.begin_step(STEP_LABELS[1]);
    logs::push(
        logs::Level::Info,
        &label,
        format!("开始认证({})", conn.auth.describe()),
    );
    authenticate(conn, io, &mut session).await?;
    io.done_step();
    Ok(session)
}

async fn run(conn: &Conn, cols: u16, rows: u16, io: Io, resize: &mut tokio_mpsc::UnboundedReceiver<(u16, u16)>, size: &Arc<Mutex<(u16, u16)>>) -> Result<End, String> {
    let label = format!("{}@{}:{}", conn.user, conn.host, conn.port);
    let session = Arc::new(open_authenticated(conn, &io).await?);
    io.begin_step(STEP_LABELS[2]);
    logs::push(logs::Level::Info, &label, "认证成功");

    let mut ch = session.channel_open_session().await.map_err(|e| format!("打开会话失败:{e}"))?;
    ch.request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[]).await.map_err(|e| format!("申请终端失败:{e}"))?;
    ch.request_shell(true).await.map_err(|e| format!("启动 shell 失败:{e}"))?;
    io.done_step();
    logs::push(logs::Level::Info, &label, "已打开终端");
    set(&io.status, Status::Connected);
    io.ever.store(true, Ordering::Relaxed);
    io.epoch.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut g) = io.link.lock() {
        *g = Some(session.clone());
    }

    let inp = io.inp.clone();
    let mut inp = inp.lock().await;
    let end = loop {
        tokio::select! {
            data = inp.recv() => match data {
                Some(d) => {
                    if let Err(e) = ch.data(&d[..]).await {
                        break Err(e.to_string());
                    }
                }
                None => break Ok(End::Dropped),
            },
            Some((c, r)) = resize.recv() => {
                if let Ok(mut g) = size.lock() {
                    *g = (c, r);
                }
                let _ = ch.window_change(c as u32, r as u32, 0, 0).await;
            }
            msg = ch.wait() => match msg {
                Some(ChannelMsg::Data { data }) => { let _ = io.out.send(data.to_vec()); }
                Some(ChannelMsg::ExtendedData { data, .. }) => { let _ = io.out.send(data.to_vec()); }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    io.say(&format!("\r\n[连接已关闭,退出码 {exit_status}]\r\n"));
                    break Ok(End::Exited);
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    // 正常退出时 exit-status 可能晚一点到:再等一小会儿,等不到才算意外中断
                    let mut exited = false;
                    let _ = tokio::time::timeout(std::time::Duration::from_millis(600), async {
                        while let Some(m) = ch.wait().await {
                            if let ChannelMsg::ExitStatus { exit_status } = m {
                                io.say(&format!("\r\n[连接已关闭,退出码 {exit_status}]\r\n"));
                                exited = true;
                                break;
                            }
                        }
                    })
                    .await;
                    break Ok(if exited { End::Exited } else { End::Lost });
                }
                _ => {}
            },
        }
    };
    if let Ok(mut g) = io.link.lock() {
        *g = None;
    }
    let _ = session.disconnect(Disconnect::ByApplication, "", "en").await;
    end
}

async fn authenticate(conn: &Conn, io: &Io, session: &mut client::Handle<Client>) -> Result<(), String> {
    if let Some(e) = &conn.auth_error {
        return Err(e.clone());
    }
    let label = format!("{}@{}:{}", conn.user, conn.host, conn.port);
    let auth = &conn.auth;
    let mut errors: Vec<String> = Vec::new();

    if let Some(k) = &auth.key {
        match try_key(conn, io, session, k).await {
            Ok(true) => return Ok(()),
            Ok(false) => errors.push(format!("私钥 {} 被服务器拒绝", k.label)),
            Err(e) => {
                logs::push(logs::Level::Warn, &label, e.clone());
                errors.push(e);
            }
        }
    }
    if auth.agent {
        match try_agent(conn, session).await {
            Ok(true) => return Ok(()),
            Ok(false) => errors.push("SSH Agent 里的密钥都被服务器拒绝".into()),
            Err(e) => {
                logs::push(logs::Level::Warn, &label, e.clone());
                errors.push(e);
            }
        }
    }
    let password = match &auth.password {
        Some(p) => Some(p.clone()),
        // 什么都没配置:在终端里问密码
        None if auth.key.is_none() && !auth.agent => Some(Zeroizing::new(io.ask(&format!("{}@{} 的密码:", conn.user, conn.host), false).await.ok_or("已取消")?)),
        None => None,
    };
    if let Some(pw) = password {
        let r = session.authenticate_password(conn.user.clone(), pw.as_str().to_string()).await.map_err(|e| e.to_string())?;
        if r.success() {
            return Ok(());
        }
        errors.push("密码不对或服务器不允许密码登录".into());
    }
    Err(if errors.is_empty() { "认证失败".into() } else { errors.join(";") })
}

async fn try_key(conn: &Conn, io: &Io, session: &mut client::Handle<Client>, k: &KeyAuth) -> Result<bool, String> {
    let pass = k.passphrase.as_ref().map(|p| p.as_str().to_string());
    let key = match keys::decode_secret_key(&k.pem, pass.as_deref()) {
        Ok(key) => key,
        Err(keys::Error::KeyIsEncrypted) => {
            let p = Zeroizing::new(io.ask(&format!("私钥 {} 的口令:", k.label), false).await.ok_or("已取消")?);
            keys::decode_secret_key(&k.pem, Some(p.as_str())).map_err(|e| format!("私钥口令不对或私钥无法读取:{e}"))?
        }
        Err(e) => return Err(format!("读取私钥 {} 失败:{e}", k.label)),
    };
    let hash = session.best_supported_rsa_hash().await.map_err(|e| e.to_string())?.flatten();
    Ok(session.authenticate_publickey(conn.user.clone(), PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await.map_err(|e| e.to_string())?.success())
}

async fn try_agent(conn: &Conn, session: &mut client::Handle<Client>) -> Result<bool, String> {
    let mut agent = keys::agent::client::AgentClient::connect_env().await.map_err(|e| format!("连不上 SSH Agent:{e}"))?;
    let ids = agent.request_identities().await.map_err(|e| format!("读取 Agent 密钥失败:{e}"))?;
    if ids.is_empty() {
        return Err("SSH Agent 里没有密钥(可先 ssh-add)".into());
    }
    let hash = session.best_supported_rsa_hash().await.map_err(|e| e.to_string())?.flatten();
    for id in ids {
        let keys::agent::AgentIdentity::PublicKey { key, .. } = id else { continue };
        let r = session.authenticate_publickey_with(conn.user.clone(), key, hash, &mut agent).await.map_err(|e| e.to_string())?;
        if r.success() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端到端(默认跳过):终端连上 → SFTP 复用它的连接 → 连接被外部切断后自动重连。
    /// 需要 `DOCKYARD_TEST_SSH=user@host:port`(SSH Agent 认证)、临时 `HOME`、`DOCKYARD_TEST_READY=<文件>`:
    /// 测试在复用成功后写这个文件,等外部杀掉 sshd 会话进程,再期待状态走 Reconnecting → Connected 且代数加一。
    #[test]
    fn terminal_shares_session_and_reconnects() {
        let Ok(target) = std::env::var("DOCKYARD_TEST_SSH") else { return };
        let ready = std::env::var("DOCKYARD_TEST_READY").expect("DOCKYARD_TEST_READY");
        let conn = Conn::parse(&target);
        let (_i, _o, h) = connect(conn.clone(), 80, 24);
        let wait = |secs: u64, what: &str, f: &dyn Fn() -> bool| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
            while std::time::Instant::now() < end {
                if f() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            panic!("等待超时:{what};状态 {:?}", h.status());
        };
        wait(20, "首次连上", &|| {
            if h.host_prompt().is_some() {
                h.answer_host(true);
            }
            h.status() == Status::Connected
        });
        let e0 = h.generation();
        assert!(h.ever_connected() && e0 == 1);

        // SFTP 复用终端的连接
        let sess = h.session().expect("连上后应有共享连接");
        let sftp = crate::sftp::Sftp::connect(conn, Some((sess, e0)));
        wait(20, "SFTP 就绪", &|| matches!(sftp.status(), crate::sftp::Status::Ready));
        let home = sftp.home();
        sftp.list(&home);
        wait(20, "列目录", &|| sftp.dir(&home).is_some_and(|d| !d.loading));
        assert!(sftp.dir(&home).unwrap().error.is_none());
        assert_eq!(sftp.epoch(), Some(e0));

        // 通知外部:可以切断连接了
        std::fs::write(&ready, b"ready").unwrap();
        wait(60, "进入重连", &|| matches!(h.status(), Status::Reconnecting(_)));
        assert!(h.session().is_none(), "重连期间不应有可用连接");
        wait(90, "重连成功", &|| h.status() == Status::Connected);
        assert!(h.generation() > e0, "重连后代数应加一");
        assert!(h.session().is_some());
        assert!(h.ever_connected());
    }

    #[test]
    fn parse_user_host() {
        let c = Conn::parse("root@1.2.3.4");
        assert_eq!((c.user.as_str(), c.host.as_str(), c.port), ("root", "1.2.3.4", 22));
        let c = Conn::parse("deploy@h.example.com:2222");
        assert_eq!((c.user.as_str(), c.host.as_str(), c.port), ("deploy", "h.example.com", 2222));
        let c = Conn::parse("h.example.com");
        assert_eq!((c.user.as_str(), c.port), ("root", 22));
    }
}
