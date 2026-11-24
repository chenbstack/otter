//! 本地接口的传输层:Unix 上是 `data_dir/ctl.sock`(0600),Windows 上是命名管道
//! `\\.\pipe\otter-ctl-<用户名>`(默认拒绝远程客户端,别的用户没有读写权限)。
//! 上层(`ctl.rs`、`ctl_cli.rs`)只看到“按行读写的双工流”。

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}
pub type Conn = Box<dyn Duplex>;

pub enum BindError {
    /// 另一个 Otter 已经在监听
    InUse,
    Other(String),
}

// ---------------- Unix ----------------

#[cfg(unix)]
pub type SyncStream = std::os::unix::net::UnixStream;

#[cfg(unix)]
fn sock_path() -> Option<std::path::PathBuf> {
    crate::store::data_dir().map(|d| d.join("ctl.sock"))
}

/// 同步地连上运行中的 Otter(CLI / MCP 用)。
#[cfg(unix)]
pub fn connect() -> std::io::Result<SyncStream> {
    let path = sock_path().ok_or_else(|| std::io::Error::other("找不到数据目录"))?;
    SyncStream::connect(path)
}

/// 开始监听,新连接从返回的 channel 里出来。必须在 tokio 运行时里调用。
#[cfg(unix)]
pub fn bind() -> Result<mpsc::UnboundedReceiver<Conn>, BindError> {
    use std::os::unix::fs::PermissionsExt;
    let path = sock_path().ok_or_else(|| BindError::Other("找不到数据目录".into()))?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if path.exists() {
        // 还能连上说明另一个 Otter 正在运行,不去抢它的 socket
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            return Err(BindError::InUse);
        }
        let _ = std::fs::remove_file(&path);
    }
    let listener = tokio::net::UnixListener::bind(&path).map_err(|e| BindError::Other(e.to_string()))?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let (tx, rx) = mpsc::unbounded_channel::<Conn>();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            if tx.send(Box::new(stream)).is_err() {
                break;
            }
        }
    });
    Ok(rx)
}

// ---------------- Windows ----------------

#[cfg(windows)]
pub type SyncStream = std::fs::File;

#[cfg(windows)]
fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    // 用户名里的非字母数字都换掉,管道名不能带路径分隔符
    let user: String = user.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!(r"\\.\pipe\otter-ctl-{user}")
}

#[cfg(windows)]
pub fn connect() -> std::io::Result<SyncStream> {
    std::fs::OpenOptions::new().read(true).write(true).open(pipe_name())
}

#[cfg(windows)]
pub fn bind() -> Result<mpsc::UnboundedReceiver<Conn>, BindError> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let name = pipe_name();
    let make = |first: bool| ServerOptions::new().first_pipe_instance(first).reject_remote_clients(true).create(&name);
    let mut server = match make(true) {
        Ok(s) => s,
        // 第一个实例已被别的进程占着:ERROR_ACCESS_DENIED
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Err(BindError::InUse),
        Err(e) => return Err(BindError::Other(e.to_string())),
    };
    let (tx, rx) = mpsc::unbounded_channel::<Conn>();
    tokio::spawn(async move {
        loop {
            if server.connect().await.is_err() {
                continue;
            }
            let Ok(next) = make(false) else { break };
            let conn = std::mem::replace(&mut server, next);
            if tx.send(Box::new(conn)).is_err() {
                break;
            }
        }
    });
    Ok(rx)
}

/// 调用方进程(MCP 客户端 / shell)的可执行路径,只用来在确认框里给用户看。
#[cfg(unix)]
pub fn parent_exe() -> String {
    let ppid = std::os::unix::process::parent_id();
    std::process::Command::new("ps").args(["-o", "comm=", "-p", &ppid.to_string()]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

#[cfg(windows)]
pub fn parent_exe() -> String {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
    let mut sys = System::new_with_specifics(RefreshKind::nothing());
    let me = Pid::from_u32(std::process::id());
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::OnlyIfNotSet));
    sys.process(me)
        .and_then(|p| p.parent())
        .and_then(|pp| sys.process(pp))
        .and_then(|p| p.exe().map(|e| e.to_string_lossy().into_owned()))
        .unwrap_or_default()
}
