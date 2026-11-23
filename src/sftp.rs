//! SFTP 客户端:后台任务 + 命令通道,界面通过轮询共享状态拿结果。
//!
//! 用独立的 SSH 连接(和终端标签互不影响),认证复用 `Conn`;首次连接的主机指纹确认通过
//! `status()` 的 `HostPrompt` 交给界面。每条命令各开一个任务,长时间的传输不会堵住列目录。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::logs;
use crate::terminal::ssh::{Client, Conn, HostKeyPrompt, Io, PendingHost, Step, open_authenticated, runtime};
use russh::client::Handle as SshHandle;

const CHUNK: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    /// 首次连接,等用户确认主机指纹
    HostPrompt(HostKeyPrompt),
    Ready,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    /// Unix 秒
    pub mtime: Option<i64>,
    /// 形如 `drwxr-xr-x`
    pub mode: String,
    pub owner: String,
    pub link: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct DirState {
    pub entries: Vec<RemoteEntry>,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Direction {
    Up,
    Down,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransferState {
    Running,
    Done,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct Transfer {
    pub id: u64,
    pub name: String,
    pub dir: Direction,
    pub total: u64,
    pub done: u64,
    pub state: TransferState,
}

enum Cmd {
    List(String),
    Mkdir(String),
    Remove { path: String, is_dir: bool },
    Upload { locals: Vec<PathBuf>, remote_dir: String },
    Download { remote: String, is_dir: bool, local_dir: PathBuf },
}

struct Shared {
    status: Mutex<Status>,
    home: Mutex<String>,
    dirs: Mutex<HashMap<String, DirState>>,
    transfers: Mutex<Vec<Transfer>>,
    notice: Mutex<Option<String>>,
    host_prompt: PendingHost,
    steps: Arc<Mutex<Vec<Step>>>,
    next_id: AtomicU64,
    /// 就绪后的 SFTP 会话,服务器之间中转时从这里取
    session: Mutex<Option<Arc<SftpSession>>>,
}

/// 界面持有的一端;丢弃它会断开连接。
pub struct Sftp {
    cmd: mpsc::UnboundedSender<Cmd>,
    sh: Arc<Shared>,
    abort: tokio::task::AbortHandle,
    /// 复用终端连接时,对应的终端连接代数;终端重连后代数变了,这个 SFTP 就作废
    epoch: Option<u32>,
}

impl Drop for Sftp {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

// ---- 纯函数 ------------------------------------------------------------------
pub fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

fn mode_string(is_dir: bool, is_link: bool, perms: impl std::fmt::Display) -> String {
    format!("{}{perms}", if is_link { 'l' } else if is_dir { 'd' } else { '-' })
}

/// 一个待上传条目:本地路径、远程路径、是否目录、字节数
#[derive(Debug, PartialEq)]
struct PlanItem {
    local: PathBuf,
    remote: String,
    is_dir: bool,
    size: u64,
}

/// 展开要上传的文件/文件夹(目录先于其内容),跳过符号链接和读不了的项。
fn plan_upload(locals: &[PathBuf], remote_dir: &str) -> Vec<PlanItem> {
    fn walk(local: &Path, remote: String, out: &mut Vec<PlanItem>) {
        let Ok(md) = std::fs::symlink_metadata(local) else { return };
        if md.file_type().is_symlink() {
            return;
        }
        if md.is_dir() {
            out.push(PlanItem { local: local.to_path_buf(), remote: remote.clone(), is_dir: true, size: 0 });
            let mut names: Vec<_> = std::fs::read_dir(local).map(|rd| rd.flatten().map(|e| e.file_name()).collect()).unwrap_or_default();
            names.sort();
            for n in names {
                let n = n.to_string_lossy().into_owned();
                walk(&local.join(&n), join_remote(&remote, &n), out);
            }
        } else if md.is_file() {
            out.push(PlanItem { local: local.to_path_buf(), remote, is_dir: false, size: md.len() });
        }
    }
    let mut out = Vec::new();
    for p in locals {
        if let Some(name) = p.file_name() {
            walk(p, join_remote(remote_dir, &name.to_string_lossy()), &mut out);
        }
    }
    out
}

/// 服务器给的文件名必须是单个普通路径分量:不能是空、`.`、`..`,不能含分隔符或 NUL。
/// 否则恶意服务器可以用 `../../x` 这类名字让下载写到本地目标目录之外。
fn safe_component(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return false;
    }
    let mut comps = Path::new(name).components();
    matches!((comps.next(), comps.next()), (Some(std::path::Component::Normal(_)), None))
}

fn fmt_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ---- 对外接口 ----------------------------------------------------------------
impl Sftp {
    /// `shared`:复用终端已认证的连接(和它的代数),直接开 SFTP 通道,不用再连一次;None 则自己连。
    pub fn connect(conn: Conn, shared: Option<(Arc<SshHandle<Client>>, u32)>) -> Sftp {
        let epoch = shared.as_ref().map(|s| s.1);
        let shared = shared.map(|s| s.0);
        let (tx, rx) = mpsc::unbounded_channel();
        let sh = Arc::new(Shared {
            status: Mutex::new(Status::Connecting),
            home: Mutex::new(String::new()),
            dirs: Mutex::new(HashMap::new()),
            transfers: Mutex::new(Vec::new()),
            notice: Mutex::new(None),
            host_prompt: Arc::new(Mutex::new(None)),
            steps: Arc::new(Mutex::new(Vec::new())),
            next_id: AtomicU64::new(1),
            session: Mutex::new(None),
        });
        let sh2 = sh.clone();
        let join = runtime().spawn(async move {
            let label = format!("{}@{}:{}", conn.user, conn.host, conn.port);
            logs::push(logs::Level::Info, &label, "开始连接");
            let io = Io::headless(sh2.host_prompt.clone(), sh2.steps.clone());
            if let Err(e) = session_task(&conn, &label, &io, sh2.clone(), rx, shared).await {
                logs::push(logs::Level::Error, &label, e.clone());
                // 还没走到任何一步(没有认证信息等)也要让连接页显示失败
                if io.steps_empty() {
                    io.begin_step("连接服务器");
                }
                io.fail_step(&e);
                *sh2.status.lock().unwrap() = Status::Failed(e);
            }
        });
        Sftp { cmd: tx, sh, abort: join.abort_handle(), epoch }
    }

    /// 把 `src` 服务器上的 `remote`(文件或文件夹)中转到本连接(目的服务器)的 `dst_dir` 下:
    /// 数据只经过本机内存,按块读源、写目的,读写流水线并行,不落盘。进度记在本连接的传输列表里。
    pub fn relay_from(&self, src: &Sftp, remote: &str, is_dir: bool, dst_dir: &str) {
        let (Some(from), Some(to)) = (src.session(), self.session()) else {
            notice(&self.sh, "服务器还没连上,无法传输".into());
            return;
        };
        let (sh, remote, dst_dir) = (self.sh.clone(), remote.to_string(), dst_dir.to_string());
        runtime().spawn(async move { relay(&from, &to, &sh, remote, is_dir, dst_dir).await });
    }

    pub(crate) fn session(&self) -> Option<Arc<SftpSession>> {
        self.sh.session.lock().ok().and_then(|g| g.clone())
    }

    pub fn epoch(&self) -> Option<u32> {
        self.epoch
    }

    pub fn status(&self) -> Status {
        if let Some(p) = self.sh.host_prompt.lock().ok().and_then(|g| g.as_ref().map(|(p, _)| p.clone())) {
            return Status::HostPrompt(p);
        }
        self.sh.status.lock().map(|s| s.clone()).unwrap_or(Status::Failed("内部错误".into()))
    }

    /// 连接过程的步骤,给「连接中」页用
    pub fn steps(&self) -> Vec<Step> {
        self.sh.steps.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// 回答主机指纹确认
    pub fn answer_host(&self, yes: bool) {
        if let Ok(mut g) = self.sh.host_prompt.lock() {
            if let Some(tx) = g.as_mut().and_then(|(_, tx)| tx.take()) {
                let _ = tx.send(yes);
            }
        }
    }

    /// 远程主目录(连接成功后才有)
    pub fn home(&self) -> String {
        self.sh.home.lock().map(|h| h.clone()).unwrap_or_default()
    }

    pub fn dir(&self, path: &str) -> Option<DirState> {
        self.sh.dirs.lock().ok().and_then(|d| d.get(path).cloned())
    }

    /// 请求(重新)读取目录;已有的旧内容保留到新结果回来
    pub fn list(&self, path: &str) {
        if let Ok(mut d) = self.sh.dirs.lock() {
            let e = d.entry(path.to_string()).or_default();
            e.loading = true;
            e.error = None;
        }
        let _ = self.cmd.send(Cmd::List(path.to_string()));
    }

    pub fn mkdir(&self, path: &str) {
        let _ = self.cmd.send(Cmd::Mkdir(path.to_string()));
    }

    pub fn remove(&self, path: &str, is_dir: bool) {
        let _ = self.cmd.send(Cmd::Remove { path: path.to_string(), is_dir });
    }

    pub fn upload(&self, locals: Vec<PathBuf>, remote_dir: &str) {
        let _ = self.cmd.send(Cmd::Upload { locals, remote_dir: remote_dir.to_string() });
    }

    pub fn download(&self, remote: &str, is_dir: bool, local_dir: PathBuf) {
        let _ = self.cmd.send(Cmd::Download { remote: remote.to_string(), is_dir, local_dir });
    }

    pub fn transfers(&self) -> Vec<Transfer> {
        self.sh.transfers.lock().map(|t| t.clone()).unwrap_or_default()
    }

    /// 取走一条提示(完成/失败消息)
    pub fn take_notice(&self) -> Option<String> {
        self.sh.notice.lock().ok().and_then(|mut n| n.take())
    }

    /// 是否还有需要界面继续刷新的事:连接中、等确认、目录加载中、传输中
    pub fn busy(&self) -> bool {
        match self.status() {
            Status::Connecting | Status::HostPrompt(_) => return true,
            Status::Failed(_) => return false,
            Status::Ready => {}
        }
        let loading = self.sh.dirs.lock().map(|d| d.values().any(|s| s.loading)).unwrap_or(false);
        let moving = self.sh.transfers.lock().map(|t| t.iter().any(|x| x.state == TransferState::Running)).unwrap_or(false);
        loading || moving || self.sh.notice.lock().map(|n| n.is_some()).unwrap_or(false)
    }
}

// ---- 后台任务 ----------------------------------------------------------------
async fn session_task(conn: &Conn, label: &str, io: &Io, sh: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Cmd>, shared: Option<Arc<SshHandle<Client>>>) -> Result<(), String> {
    if let Some(e) = &conn.auth_error {
        return Err(e.clone());
    }
    if conn.auth.key.is_none() && !conn.auth.agent && conn.auth.password.is_none() {
        return Err("这个节点还没有保存密码或私钥,请先在节点的「编辑」里填写".into());
    }
    let session = match shared {
        Some(s) => {
            logs::push(logs::Level::Info, label, "复用终端的连接");
            s
        }
        None => {
            let s = Arc::new(open_authenticated(conn, io).await?);
            logs::push(logs::Level::Info, label, "认证成功");
            s
        }
    };
    io.begin_step("打开 SFTP");
    let ch = session.channel_open_session().await.map_err(|e| format!("打开会话失败:{e}"))?;
    ch.request_subsystem(true, "sftp").await.map_err(|e| format!("服务器没有启用 SFTP:{e}"))?;
    let sftp = Arc::new(SftpSession::new(ch.into_stream()).await.map_err(|e| format!("SFTP 初始化失败:{e}"))?);
    let home = sftp.canonicalize(".").await.unwrap_or_else(|_| "/".into());
    io.done_step();
    *sh.home.lock().unwrap() = home;
    *sh.session.lock().unwrap() = Some(sftp.clone());
    *sh.status.lock().unwrap() = Status::Ready;
    logs::push(logs::Level::Info, label, "SFTP 已就绪");

    while let Some(cmd) = rx.recv().await {
        let (sftp, sh) = (sftp.clone(), sh.clone());
        tokio::spawn(async move { handle(cmd, &sftp, &sh).await });
    }
    // session 在这里释放,连接关闭
    drop(session);
    Ok(())
}

fn notice(sh: &Shared, msg: String) {
    if let Ok(mut n) = sh.notice.lock() {
        *n = Some(msg);
    }
}

fn set_dir(sh: &Shared, path: &str, state: DirState) {
    if let Ok(mut d) = sh.dirs.lock() {
        d.insert(path.to_string(), state);
    }
}

async fn relist(sftp: &SftpSession, sh: &Shared, path: &str) {
    match list_dir(sftp, path).await {
        Ok(entries) => set_dir(sh, path, DirState { entries, loading: false, error: None }),
        Err(e) => set_dir(sh, path, DirState { entries: vec![], loading: false, error: Some(e) }),
    }
}

fn parent_of(path: &str) -> String {
    match path.trim_end_matches('/').rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((p, _)) => p.to_string(),
    }
}

async fn handle(cmd: Cmd, sftp: &SftpSession, sh: &Shared) {
    match cmd {
        Cmd::List(path) => relist(sftp, sh, &path).await,
        Cmd::Mkdir(path) => {
            match sftp.create_dir(path.clone()).await {
                Ok(()) => {}
                Err(e) => notice(sh, format!("新建文件夹失败:{e}")),
            }
            relist(sftp, sh, &parent_of(&path)).await;
        }
        Cmd::Remove { path, is_dir } => {
            let name = path.rsplit('/').next().unwrap_or(&path).to_string();
            match remove_tree(sftp, path.clone(), is_dir).await {
                Ok(()) => notice(sh, format!("已删除 {name}")),
                Err(e) => notice(sh, format!("删除 {name} 失败:{e}")),
            }
            relist(sftp, sh, &parent_of(&path)).await;
        }
        Cmd::Upload { locals, remote_dir } => upload(sftp, sh, locals, remote_dir).await,
        Cmd::Download { remote, is_dir, local_dir } => download(sftp, sh, remote, is_dir, local_dir).await,
    }
}

fn new_transfer(sh: &Shared, name: String, dir: Direction, total: u64) -> u64 {
    let id = sh.next_id.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut t) = sh.transfers.lock() {
        // 只保留最近的 20 条,旧的已结束项先丢
        while t.len() >= 20 {
            match t.iter().position(|x| x.state != TransferState::Running) {
                Some(i) => {
                    t.remove(i);
                }
                None => break,
            }
        }
        t.push(Transfer { id, name, dir, total, done: 0, state: TransferState::Running });
    }
    id
}

fn progress(sh: &Shared, id: u64, add: u64) {
    if let Ok(mut t) = sh.transfers.lock() {
        if let Some(x) = t.iter_mut().find(|x| x.id == id) {
            x.done += add;
        }
    }
}

fn finish(sh: &Shared, id: u64, r: Result<(), String>) {
    let mut msg = None;
    if let Ok(mut t) = sh.transfers.lock() {
        if let Some(x) = t.iter_mut().find(|x| x.id == id) {
            match r {
                Ok(()) => {
                    x.done = x.total;
                    x.state = TransferState::Done;
                    msg = Some(format!("{} {}完成", x.name, if x.dir == Direction::Up { "上传" } else { "下载" }));
                }
                Err(e) => {
                    msg = Some(format!("{} {}失败:{e}", x.name, if x.dir == Direction::Up { "上传" } else { "下载" }));
                    x.state = TransferState::Failed(e);
                }
            }
        }
    }
    if let Some(m) = msg {
        notice(sh, m);
    }
}

fn transfer_name(names: &[String]) -> String {
    match names {
        [] => "文件".into(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} 等 {} 项", rest.len() + 1),
    }
}

async fn list_dir(sftp: &SftpSession, path: &str) -> Result<Vec<RemoteEntry>, String> {
    let rd = sftp.read_dir(path.to_string()).await.map_err(fmt_err)?;
    let mut out = Vec::new();
    for e in rd {
        let name = e.file_name();
        if !safe_component(&name) {
            logs::push(logs::Level::Warn, "SFTP", format!("已忽略非法文件名 {name:?}"));
            continue;
        }
        let md = e.metadata();
        let ft = e.file_type();
        let is_link = ft.is_symlink();
        let full = join_remote(path, &name);
        let (mut is_dir, mut link) = (ft.is_dir(), None);
        let mut size = md.size.unwrap_or(0);
        if is_link {
            link = sftp.read_link(full.clone()).await.ok();
            if let Ok(t) = sftp.metadata(full.clone()).await {
                is_dir = t.file_type().is_dir();
                size = t.size.unwrap_or(size);
            }
        }
        let perms = md.permissions();
        out.push(RemoteEntry {
            name,
            is_dir,
            is_link,
            size,
            mtime: md.mtime.map(|t| t as i64),
            mode: mode_string(ft.is_dir(), is_link, perms),
            owner: md.user.clone().or(md.uid.map(|u| u.to_string())).unwrap_or_default(),
            link,
        });
    }
    // 文件夹在前,同类按名称(不分大小写)
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

/// 递归删除。符号链接只删链接本身,不跟进。
fn remove_tree<'a>(sftp: &'a SftpSession, path: String, is_dir: bool) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        if !is_dir {
            return sftp.remove_file(path).await.map_err(fmt_err);
        }
        if sftp.symlink_metadata(path.clone()).await.map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            return sftp.remove_file(path).await.map_err(fmt_err);
        }
        for e in list_dir(sftp, &path).await? {
            let child = join_remote(&path, &e.name);
            remove_tree(sftp, child, e.is_dir && !e.is_link).await?;
        }
        sftp.remove_dir(path).await.map_err(fmt_err)
    })
}

async fn upload(sftp: &SftpSession, sh: &Shared, locals: Vec<PathBuf>, remote_dir: String) {
    let names: Vec<String> = locals.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
    let rd = remote_dir.clone();
    let plan = tokio::task::spawn_blocking(move || plan_upload(&locals, &rd)).await.unwrap_or_default();
    let total: u64 = plan.iter().map(|p| p.size).sum();
    let id = new_transfer(sh, transfer_name(&names), Direction::Up, total.max(1));
    let r = async {
        for it in &plan {
            if it.is_dir {
                // 已存在不算错
                if let Err(e) = sftp.create_dir(it.remote.clone()).await {
                    if !sftp.try_exists(it.remote.clone()).await.unwrap_or(false) {
                        return Err(format!("{}:{e}", it.remote));
                    }
                }
                continue;
            }
            let mut src = tokio::fs::File::open(&it.local).await.map_err(|e| format!("{}:{e}", it.local.display()))?;
            let mut dst = sftp.create(it.remote.clone()).await.map_err(|e| format!("{}:{e}", it.remote))?;
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = src.read(&mut buf).await.map_err(fmt_err)?;
                if n == 0 {
                    break;
                }
                dst.write_all(&buf[..n]).await.map_err(|e| format!("{}:{e}", it.remote))?;
                progress(sh, id, n as u64);
            }
            dst.shutdown().await.map_err(|e| format!("{}:{e}", it.remote))?;
        }
        Ok(())
    }
    .await;
    finish(sh, id, r);
    relist(sftp, sh, &remote_dir).await;
}

/// 下载要做的事:先建本地目录,再逐个文件
enum DlItem {
    Dir(PathBuf),
    File { remote: String, local: PathBuf, size: u64 },
}

fn collect_download<'a>(sftp: &'a SftpSession, remote: String, local: PathBuf, is_dir: bool, size: u64, out: &'a mut Vec<DlItem>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        if !is_dir {
            out.push(DlItem::File { remote, local, size });
            return Ok(());
        }
        out.push(DlItem::Dir(local.clone()));
        for e in list_dir(sftp, &remote).await? {
            if e.is_link {
                continue;
            }
            if !safe_component(&e.name) {
                return Err(format!("服务器返回了非法文件名 {:?}", e.name));
            }
            collect_download(sftp, join_remote(&remote, &e.name), local.join(&e.name), e.is_dir, e.size, out).await?;
        }
        Ok(())
    })
}

/// 本地已存在同名项时加 " (n)" 后缀,不覆盖
fn unique_local(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (1..)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

async fn download(sftp: &SftpSession, sh: &Shared, remote: String, is_dir: bool, local_dir: PathBuf) {
    let name = remote.trim_end_matches('/').rsplit('/').next().unwrap_or("文件").to_string();
    if !safe_component(&name) {
        notice(sh, format!("下载失败:非法文件名 {name:?}"));
        return;
    }
    let local = unique_local(&local_dir, &name);
    let id = new_transfer(sh, name, Direction::Down, 1);
    let r = async {
        let size = if is_dir { 0 } else { sftp.metadata(remote.clone()).await.map_err(fmt_err)?.size.unwrap_or(0) };
        let mut items = Vec::new();
        collect_download(sftp, remote.clone(), local.clone(), is_dir, size, &mut items).await?;
        let total: u64 = items.iter().map(|i| if let DlItem::File { size, .. } = i { *size } else { 0 }).sum();
        if let Ok(mut t) = sh.transfers.lock() {
            if let Some(x) = t.iter_mut().find(|x| x.id == id) {
                x.total = total.max(1);
            }
        }
        for it in items {
            match it {
                DlItem::Dir(p) if !p.starts_with(&local) => return Err(format!("路径越界 {}", p.display())),
                DlItem::File { local: l, .. } if !l.starts_with(&local) => return Err(format!("路径越界 {}", l.display())),
                DlItem::Dir(p) => tokio::fs::create_dir_all(&p).await.map_err(|e| format!("{}:{e}", p.display()))?,
                DlItem::File { remote, local, .. } => {
                    let mut src = sftp.open(remote.clone()).await.map_err(|e| format!("{remote}:{e}"))?;
                    let mut dst = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&local).await.map_err(|e| format!("{}:{e}", local.display()))?;
                    let mut buf = vec![0u8; CHUNK];
                    loop {
                        let n = src.read(&mut buf).await.map_err(|e| format!("{remote}:{e}"))?;
                        if n == 0 {
                            break;
                        }
                        dst.write_all(&buf[..n]).await.map_err(|e| format!("{}:{e}", local.display()))?;
                        progress(sh, id, n as u64);
                    }
                    dst.flush().await.map_err(fmt_err)?;
                }
            }
        }
        Ok(())
    }
    .await;
    finish(sh, id, r);
}

/// 目的服务器上已存在同名项时加 " (n)" 后缀,不覆盖
async fn unique_remote(sftp: &SftpSession, dir: &str, name: &str) -> String {
    let first = join_remote(dir, name);
    if !sftp.try_exists(first.clone()).await.unwrap_or(false) {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for i in 1.. {
        let p = join_remote(dir, &format!("{stem} ({i}){ext}"));
        if !sftp.try_exists(p.clone()).await.unwrap_or(false) {
            return p;
        }
    }
    unreachable!()
}

/// 中转要做的事:先建目的目录,再逐个文件
enum RelayItem {
    Dir(String),
    File { from: String, to: String, size: u64 },
}

fn collect_relay<'a>(src: &'a SftpSession, from: String, to: String, is_dir: bool, size: u64, out: &'a mut Vec<RelayItem>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        if !is_dir {
            out.push(RelayItem::File { from, to, size });
            return Ok(());
        }
        out.push(RelayItem::Dir(to.clone()));
        for e in list_dir(src, &from).await? {
            if e.is_link {
                continue;
            }
            // 源服务器给的名字要当成目的服务器上的路径分量,同样不能含 `..`、分隔符
            if !safe_component(&e.name) {
                return Err(format!("源服务器返回了非法文件名 {:?}", e.name));
            }
            collect_relay(src, join_remote(&from, &e.name), join_remote(&to, &e.name), e.is_dir, e.size, out).await?;
        }
        Ok(())
    })
}

/// 读写之间缓冲的块数:读下一块的同时,前几块在写,吞吐接近两端中较慢的一边
const PIPE_DEPTH: usize = 6;

async fn relay_file(from_s: &SftpSession, to_s: &SftpSession, sh: &Shared, id: u64, from: &str, to: &str) -> Result<(), String> {
    let mut rd = from_s.open(from.to_string()).await.map_err(|e| format!("{from}:{e}"))?;
    let mut wr = to_s.create(to.to_string()).await.map_err(|e| format!("{to}:{e}"))?;
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(PIPE_DEPTH);
    // move:读完后 tx 随之释放,写端的 recv 才会结束
    let reader = async move {
        loop {
            let mut buf = vec![0u8; CHUNK];
            let n = rd.read(&mut buf).await.map_err(|e| format!("{from}:{e}"))?;
            if n == 0 {
                return Ok::<(), String>(());
            }
            buf.truncate(n);
            if tx.send(buf).await.is_err() {
                return Ok(()); // 写端出错已退出,错误由写端报告
            }
        }
    };
    let writer = async {
        while let Some(b) = rx.recv().await {
            wr.write_all(&b).await.map_err(|e| format!("{to}:{e}"))?;
            progress(sh, id, b.len() as u64);
        }
        wr.shutdown().await.map_err(|e| format!("{to}:{e}"))
    };
    tokio::try_join!(reader, writer).map(|_| ())
}

async fn relay(from_s: &SftpSession, to_s: &SftpSession, sh: &Shared, remote: String, is_dir: bool, dst_dir: String) {
    let name = remote.trim_end_matches('/').rsplit('/').next().unwrap_or("文件").to_string();
    if !safe_component(&name) {
        notice(sh, format!("传输失败:非法文件名 {name:?}"));
        return;
    }
    let dest = unique_remote(to_s, &dst_dir, &name).await;
    let id = new_transfer(sh, name, Direction::Up, 1);
    let r = async {
        let size = if is_dir { 0 } else { from_s.metadata(remote.clone()).await.map_err(fmt_err)?.size.unwrap_or(0) };
        let mut items = Vec::new();
        collect_relay(from_s, remote.clone(), dest.clone(), is_dir, size, &mut items).await?;
        let total: u64 = items.iter().map(|i| if let RelayItem::File { size, .. } = i { *size } else { 0 }).sum();
        if let Ok(mut t) = sh.transfers.lock() {
            if let Some(x) = t.iter_mut().find(|x| x.id == id) {
                x.total = total.max(1);
            }
        }
        for it in items {
            match it {
                RelayItem::Dir(p) => {
                    if let Err(e) = to_s.create_dir(p.clone()).await {
                        if !to_s.try_exists(p.clone()).await.unwrap_or(false) {
                            return Err(format!("{p}:{e}"));
                        }
                    }
                }
                RelayItem::File { from, to, .. } => relay_file(from_s, to_s, sh, id, &from, &to).await?,
            }
        }
        Ok(())
    }
    .await;
    finish(sh, id, r);
    relist(to_s, sh, &dst_dir).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_and_parent() {
        assert_eq!(join_remote("/", "a"), "/a");
        assert_eq!(join_remote("/var/lib", "docker"), "/var/lib/docker");
        assert_eq!(join_remote("/var/", "x"), "/var/x");
        assert_eq!(parent_of("/var/lib/docker"), "/var/lib");
        assert_eq!(parent_of("/var"), "/");
        assert_eq!(parent_of("/var/"), "/");
    }

    #[test]
    fn plan_expands_dirs_before_children() {
        let d = std::env::temp_dir().join(format!("dockyard-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("proj/sub")).unwrap();
        std::fs::write(d.join("proj/a.txt"), b"hello").unwrap();
        std::fs::write(d.join("proj/sub/b.txt"), b"12").unwrap();
        let plan = plan_upload(&[d.join("proj")], "/srv");
        let got: Vec<_> = plan.iter().map(|p| (p.remote.as_str(), p.is_dir, p.size)).collect();
        assert_eq!(got, vec![("/srv/proj", true, 0), ("/srv/proj/a.txt", false, 5), ("/srv/proj/sub", true, 0), ("/srv/proj/sub/b.txt", false, 2)]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unique_local_adds_suffix() {
        let d = std::env::temp_dir().join(format!("dockyard-uniq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(unique_local(&d, "a.txt"), d.join("a.txt"));
        std::fs::write(d.join("a.txt"), b"").unwrap();
        assert_eq!(unique_local(&d, "a.txt"), d.join("a (1).txt"));
        std::fs::write(d.join("a (1).txt"), b"").unwrap();
        assert_eq!(unique_local(&d, "a.txt"), d.join("a (2).txt"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rejects_unsafe_names() {
        for bad in ["", ".", "..", "../x", "a/b", "/etc/passwd", "a\\b", "a\0b"] {
            assert!(!safe_component(bad), "{bad:?}");
        }
        for ok in ["a", ".bashrc", "新建 文件夹", "a b.tar.gz", "..x", "x.."] {
            assert!(safe_component(ok), "{ok:?}");
        }
    }

    #[test]
    fn mode_strings() {
        assert_eq!(mode_string(true, false, "rwxr-xr-x"), "drwxr-xr-x");
        assert_eq!(mode_string(false, true, "rwxrwxrwx"), "lrwxrwxrwx");
        assert_eq!(mode_string(false, false, "rw-r--r--"), "-rw-r--r--");
    }

    /// 端到端:对真实 sshd 走一遍 列目录 / 新建 / 上传(文件+文件夹)/ 下载 / 删除。
    /// 需要 `DOCKYARD_TEST_SSH=user@host:port`(认证走 SSH Agent),且 `HOME` 指向临时目录;默认跳过。
    #[test]
    fn end_to_end_against_real_sshd() {
        let Ok(target) = std::env::var("DOCKYARD_TEST_SSH") else { return };
        let work = std::env::var("DOCKYARD_TEST_DIR").expect("DOCKYARD_TEST_DIR 远程可写目录");
        let sftp = Sftp::connect(Conn::parse(&target), None);
        let wait = |cond: &dyn Fn() -> bool, what: &str| {
            for _ in 0..200 {
                if cond() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            panic!("等待超时:{what};状态 {:?}", sftp.status());
        };
        // 首次连接:确认主机指纹
        wait(&|| match sftp.status() {
            Status::HostPrompt(_) => {
                sftp.answer_host(true);
                false
            }
            Status::Ready => true,
            Status::Failed(e) => panic!("连接失败:{e}"),
            _ => false,
        }, "连接");
        assert!(!sftp.home().is_empty());

        // 本地素材
        let loc = std::env::temp_dir().join(format!("dockyard-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&loc);
        std::fs::create_dir_all(loc.join("proj/sub")).unwrap();
        std::fs::write(loc.join("proj/a.txt"), vec![7u8; 700_000]).unwrap();
        std::fs::write(loc.join("proj/sub/b.txt"), b"bee").unwrap();
        std::fs::write(loc.join("single.txt"), b"hello").unwrap();

        let list = |p: &str| {
            sftp.list(p);
            wait(&|| sftp.dir(p).is_some_and(|d| !d.loading), "列目录");
            sftp.dir(p).unwrap()
        };
        let busy = || sftp.transfers().iter().any(|t| t.state == TransferState::Running);

        // 新建文件夹
        sftp.mkdir(&join_remote(&work, "e2e"));
        let d = join_remote(&work, "e2e");
        wait(&|| sftp.dir(&work).is_some_and(|x| x.entries.iter().any(|e| e.name == "e2e" && e.is_dir)), "mkdir");

        // 上传文件 + 文件夹
        sftp.upload(vec![loc.join("proj"), loc.join("single.txt")], &d);
        std::thread::sleep(std::time::Duration::from_millis(200));
        wait(&|| !busy(), "上传");
        let ts = sftp.transfers();
        assert!(ts.iter().all(|t| t.state == TransferState::Done), "{ts:?}");
        let up = list(&d);
        assert!(up.error.is_none());
        let names: Vec<_> = up.entries.iter().map(|e| (e.name.as_str(), e.is_dir, e.size)).collect();
        assert_eq!(names, vec![("proj", true, up.entries[0].size), ("single.txt", false, 5)]);
        assert_eq!(list(&join_remote(&d, "proj")).entries.iter().find(|e| e.name == "a.txt").unwrap().size, 700_000);
        assert_eq!(list(&join_remote(&d, "proj/sub")).entries[0].name, "b.txt");

        // 下载文件夹(本地已有同名 → 自动加后缀)
        let dl = loc.join("dl");
        std::fs::create_dir_all(dl.join("proj")).unwrap();
        sftp.download(&join_remote(&d, "proj"), true, dl.clone());
        std::thread::sleep(std::time::Duration::from_millis(200));
        wait(&|| !busy(), "下载");
        assert_eq!(std::fs::read(dl.join("proj (1)/a.txt")).unwrap().len(), 700_000);
        assert_eq!(std::fs::read(dl.join("proj (1)/sub/b.txt")).unwrap(), b"bee");
        sftp.download(&join_remote(&d, "single.txt"), false, dl.clone());
        std::thread::sleep(std::time::Duration::from_millis(200));
        wait(&|| !busy(), "下载文件");
        assert_eq!(std::fs::read(dl.join("single.txt")).unwrap(), b"hello");

        // 服务器 → 服务器:第二条独立连接作为目的端,中转文件夹(含 700KB 文件)和单文件;重名自动加后缀
        let dst = Sftp::connect(Conn::parse(&target), None);
        for _ in 0..200 {
            if matches!(dst.status(), Status::Ready) {
                break;
            }
            if matches!(dst.status(), Status::HostPrompt(_)) {
                dst.answer_host(true);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let d2 = join_remote(&work, "e2e-dst");
        dst.mkdir(&d2);
        std::thread::sleep(std::time::Duration::from_millis(500));
        let dbusy = || dst.transfers().iter().any(|t| t.state == TransferState::Running);
        for _ in 0..3 {
            dst.relay_from(&sftp, &join_remote(&d, "proj"), true, &d2);
            std::thread::sleep(std::time::Duration::from_millis(300));
            wait(&|| !dbusy(), "中转文件夹");
        }
        dst.relay_from(&sftp, &join_remote(&d, "single.txt"), false, &d2);
        std::thread::sleep(std::time::Duration::from_millis(300));
        wait(&|| !dbusy(), "中转文件");
        let ts = dst.transfers();
        assert!(ts.len() == 4 && ts.iter().all(|t| t.state == TransferState::Done && t.done == t.total), "{ts:?}");
        let list2 = |p: &str| {
            dst.list(p);
            for _ in 0..100 {
                if dst.dir(p).is_some_and(|d| !d.loading) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            dst.dir(p).unwrap()
        };
        let top: Vec<_> = list2(&d2).entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(top, vec!["proj", "proj (1)", "proj (2)", "single.txt"]);
        assert_eq!(list2(&join_remote(&d2, "proj (1)")).entries.iter().find(|e| e.name == "a.txt").unwrap().size, 700_000);
        assert_eq!(list2(&join_remote(&d2, "proj/sub")).entries[0].name, "b.txt");
        sftp.remove(&d2, true);

        // 递归删除
        sftp.remove(&d, true);
        wait(&|| sftp.dir(&work).is_some_and(|x| !x.loading && !x.entries.iter().any(|e| e.name == "e2e")), "删除");

        // 不存在的目录 → 报错而不是卡住
        let bad = list("/definitely/not/here");
        assert!(bad.error.is_some() && bad.entries.is_empty());
        let _ = std::fs::remove_dir_all(&loc);
    }

    #[test]
    fn transfer_names() {
        assert_eq!(transfer_name(&["a".into()]), "a");
        assert_eq!(transfer_name(&["a".into(), "b".into(), "c".into()]), "a 等 3 项");
    }
}
