//! SSH 会话:russh 跑在后台 tokio 运行时里,和界面之间用 channel 传字节。
//!
//! 对外是一对阻塞的 `Read`/`Write`(喂给 `gpui_xterm::TerminalView::new`)、
//! 一个调整行列的句柄和一个状态。密码、口令、主机密钥确认都在终端里以提示行的形式完成。

use crate::logs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, OnceLock};

use russh::client;
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use tokio::sync::mpsc as tokio_mpsc;

#[derive(Clone, Debug, PartialEq)]
pub enum AuthKind {
    Agent,
    Key { path: String },
    Password,
}

#[derive(Clone, Debug)]
pub struct Conn {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthKind,
}

impl Conn {
    /// 从 `user@host` / `user@host:port` 解析,认证默认 SSH Agent。
    pub fn parse(user_host: &str) -> Conn {
        let (user, rest) = user_host.split_once('@').unwrap_or(("root", user_host));
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse().unwrap()),
            _ => (rest, 22),
        };
        Conn { host: host.into(), port, user: user.into(), auth: AuthKind::Agent }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    /// 正在终端里等用户输入(确认指纹 / 密码 / 口令)
    Waiting,
    Connected,
    Closed(Option<String>),
}

/// 后台 tokio 运行时,整个进程一个。
fn runtime() -> &'static tokio::runtime::Runtime {
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
    abort: tokio::task::AbortHandle,
}

impl Handle {
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
    let st = status.clone();
    let join = runtime().spawn(async move {
        let io = Io { out: out_tx, inp: Arc::new(tokio::sync::Mutex::new(in_rx)), status: st.clone() };
        let label = format!("{}@{}:{}", conn.user, conn.host, conn.port);
        logs::push(logs::Level::Info, &label, "开始连接");
        let res = run(&conn, cols as u16, rows as u16, io.clone(), rs_rx).await;
        let reason = match res {
            Ok(()) => {
                logs::push(logs::Level::Info, &label, "连接已关闭");
                None
            }
            Err(e) => {
                logs::push(logs::Level::Error, &label, e.clone());
                io.say(&format!("\r\n\x1b[31m{e}\x1b[0m\r\n"));
                Some(e)
            }
        };
        set(&st, Status::Closed(reason));
        // io 在这里释放,Output 读到 0
    });
    (Input(in_tx.clone()), Output { rx: out_rx, left: Vec::new() }, Handle { input: in_tx, resize: rs_tx, status, abort: join.abort_handle() })
}

fn set(st: &Arc<Mutex<Status>>, s: Status) {
    if let Ok(mut g) = st.lock() {
        *g = s;
    }
}

#[derive(Clone)]
struct Io {
    out: std_mpsc::Sender<Vec<u8>>,
    inp: Arc<tokio::sync::Mutex<tokio_mpsc::UnboundedReceiver<Vec<u8>>>>,
    status: Arc<Mutex<Status>>,
}

impl Io {
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

struct Client {
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
            io.say(&format!(
                "首次连接 {host}:{port}\r\n{} 指纹:{fp}\r\n",
                key.algorithm()
            ));
            let ans = io.ask("确认继续连接?(yes/no) ", true).await;
            if matches!(ans.as_deref(), Some("yes")) {
                let _ = keys::known_hosts::learn_known_hosts_path(host, port, key, &path);
                logs::push(logs::Level::Info, &format!("{host}:{port}"), format!("已信任新主机 {fp}"));
                io.say("已记住该主机。\r\n");
                Ok(true)
            } else {
                logs::push(logs::Level::Warn, &format!("{host}:{port}"), "未确认新主机,已取消连接");
                io.say("\x1b[31m已取消。\x1b[0m\r\n");
                Ok(false)
            }
        }
        Err(keys::Error::KeyChanged { .. }) => {
            logs::push(logs::Level::Error, &format!("{host}:{port}"), format!("主机密钥与记录不一致,已拒绝连接(新指纹 {fp})"));
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

fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(path),
    }
}

async fn run(conn: &Conn, cols: u16, rows: u16, io: Io, mut resize: tokio_mpsc::UnboundedReceiver<(u16, u16)>) -> Result<(), String> {
    let config = Arc::new(client::Config { inactivity_timeout: None, keepalive_interval: Some(std::time::Duration::from_secs(30)), ..Default::default() });
    let handler = Client { io: io.clone(), host: conn.host.clone(), port: conn.port };
    let mut session = tokio::time::timeout(std::time::Duration::from_secs(15), client::connect(config, (conn.host.as_str(), conn.port), handler))
        .await
        .map_err(|_| "连接超时".to_string())?
        .map_err(|e| match e {
            russh::Error::UnknownKey => "主机密钥未被接受,连接已中止".to_string(),
            e => format!("连接失败:{e}"),
        })?;

    authenticate(conn, &io, &mut session).await?;
    logs::push(logs::Level::Info, &format!("{}@{}:{}", conn.user, conn.host, conn.port), "认证成功");

    let mut ch = session.channel_open_session().await.map_err(|e| format!("打开会话失败:{e}"))?;
    ch.request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[]).await.map_err(|e| format!("申请终端失败:{e}"))?;
    ch.request_shell(true).await.map_err(|e| format!("启动 shell 失败:{e}"))?;
    set(&io.status, Status::Connected);

    let inp = io.inp.clone();
    let mut inp = inp.lock().await;
    loop {
        tokio::select! {
            data = inp.recv() => match data {
                Some(d) => ch.data(&d[..]).await.map_err(|e| e.to_string())?,
                None => break,
            },
            Some((c, r)) = resize.recv() => {
                let _ = ch.window_change(c as u32, r as u32, 0, 0).await;
            }
            msg = ch.wait() => match msg {
                Some(ChannelMsg::Data { data }) => { let _ = io.out.send(data.to_vec()); }
                Some(ChannelMsg::ExtendedData { data, .. }) => { let _ = io.out.send(data.to_vec()); }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    io.say(&format!("\r\n[连接已关闭,退出码 {exit_status}]\r\n"));
                    break;
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    io.say("\r\n[连接已关闭]\r\n");
                    break;
                }
                _ => {}
            },
        }
    }
    let _ = session.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(())
}

async fn authenticate(conn: &Conn, io: &Io, session: &mut client::Handle<Client>) -> Result<(), String> {
    let user = conn.user.clone();
    let ok = match &conn.auth {
        AuthKind::Password => {
            let pw = io.ask(&format!("{}@{} 的密码:", conn.user, conn.host), false).await.ok_or("已取消")?;
            let r = session.authenticate_password(user, pw).await.map_err(|e| e.to_string())?;
            r.success()
        }
        AuthKind::Key { path } => {
            let p = expand(path);
            let key = match keys::load_secret_key(&p, None) {
                Ok(k) => k,
                Err(keys::Error::KeyIsEncrypted) => {
                    let pass = io.ask(&format!("私钥 {path} 的口令:"), false).await.ok_or("已取消")?;
                    keys::load_secret_key(&p, Some(&pass)).map_err(|e| format!("私钥口令不对或私钥无法读取:{e}"))?
                }
                Err(e) => return Err(format!("读取私钥 {path} 失败:{e}")),
            };
            let hash = session.best_supported_rsa_hash().await.map_err(|e| e.to_string())?.flatten();
            session.authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await.map_err(|e| e.to_string())?.success()
        }
        AuthKind::Agent => {
            let mut agent = keys::agent::client::AgentClient::connect_env().await.map_err(|e| format!("连不上 SSH Agent:{e}"))?;
            let ids = agent.request_identities().await.map_err(|e| format!("读取 Agent 密钥失败:{e}"))?;
            if ids.is_empty() {
                return Err("SSH Agent 里没有密钥(可先 ssh-add)".into());
            }
            let hash = session.best_supported_rsa_hash().await.map_err(|e| e.to_string())?.flatten();
            let mut done = false;
            for id in ids {
                let keys::agent::AgentIdentity::PublicKey { key, .. } = id else { continue };
                let r = session.authenticate_publickey_with(user.clone(), key, hash, &mut agent).await.map_err(|e| e.to_string())?;
                if r.success() {
                    done = true;
                    break;
                }
            }
            done
        }
    };
    if ok { Ok(()) } else { Err("认证失败".into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

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
