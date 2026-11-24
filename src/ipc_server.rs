//! 本地接口的服务端监听(界面进程用):Unix socket / Windows 命名管道。上层(`ctl.rs`)只看到“按行读写的双工流”。
//! 端点位置和客户端连接见 `ipc.rs`。

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

/// 开始监听,新连接从返回的 channel 里出来。必须在 tokio 运行时里调用。
#[cfg(unix)]
pub fn bind() -> Result<mpsc::UnboundedReceiver<Conn>, BindError> {
    use std::os::unix::fs::PermissionsExt;
    let path = crate::ipc::sock_path().ok_or_else(|| BindError::Other("找不到数据目录".into()))?;
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


#[cfg(windows)]
pub fn bind() -> Result<mpsc::UnboundedReceiver<Conn>, BindError> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let name = crate::ipc::pipe_name();
    let make = move |first: bool| ServerOptions::new().first_pipe_instance(first).reject_remote_clients(true).create(&name);
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

