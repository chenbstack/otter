//! 本地接口的共享部分:端点位置、同步客户端连接、调用方父进程路径。界面(`ipc_server.rs`)和独立的命令行
//! 程序(`src/bin/otter.rs`)都用它,所以这里不能依赖界面代码。
//!
//! Unix 上端点是 `data_dir/ctl.sock`(0600),Windows 上是命名管道 `\\.\pipe\otter-ctl-<用户名>`。

#![allow(dead_code)] // 界面和命令行各用其中一部分

/// 各平台的数据目录:macOS `~/Library/Application Support/otter`、Windows `%APPDATA%\otter`、Linux `$XDG_DATA_HOME/otter`。
pub fn data_dir() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("", "", "otter").map(|d| d.data_dir().to_path_buf())
}

// ---------------- Unix ----------------

#[cfg(unix)]
pub type SyncStream = std::os::unix::net::UnixStream;

#[cfg(unix)]
pub(crate) fn sock_path() -> Option<std::path::PathBuf> {
    data_dir().map(|d| d.join("ctl.sock"))
}

/// 同步地连上运行中的 Otter(CLI / MCP 用)。
#[cfg(unix)]
pub fn connect() -> std::io::Result<SyncStream> {
    let path = sock_path().ok_or_else(|| std::io::Error::other("找不到数据目录"))?;
    SyncStream::connect(path)
}

// ---------------- Windows ----------------

#[cfg(windows)]
pub type SyncStream = std::fs::File;

#[cfg(windows)]
pub(crate) fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    // 用户名里的非字母数字都换掉,管道名不能带路径分隔符
    let user: String = user.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!(r"\\.\pipe\otter-ctl-{user}")
}

#[cfg(windows)]
pub fn connect() -> std::io::Result<SyncStream> {
    std::fs::OpenOptions::new().read(true).write(true).open(pipe_name())
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
