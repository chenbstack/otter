//! 应用日志:连接、认证、主机密钥等事件。内存里是环形缓冲(界面读它);有本地库时同时由后台线程批量落盘,
//! 启动时读回最近一批,保留 30 天(见 `store::LOG_KEEP_DAYS`)。不记终端内容和任何秘密。
//! 终端会话在后台 tokio 线程里写入,界面线程读取,所以用全局锁而不是 `Home` 的字段。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const CAP: usize = 1000;

#[derive(Clone, Copy, PartialEq)]
pub enum Level {
    /// 连接过程的分步细节;日志页默认不显示,连接页照常显示
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone)]
pub struct Entry {
    /// 显示用:今天 HH:MM:SS,更早 MM-DD HH:MM
    pub time: String,
    /// Unix 毫秒
    pub ts: i64,
    pub level: Level,
    /// 事件对应的主机,如 `root@1.2.3.4:22`;没有则为空
    pub host: String,
    pub msg: String,
}

/// 日志保留天数(设置页可改,存 `log_keep_days`)
static KEEP_DAYS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(crate::store::LOG_KEEP_DAYS);
pub const KEY_KEEP_DAYS: &str = "log_keep_days";

pub fn keep_days() -> i64 {
    KEEP_DAYS.load(Ordering::Relaxed)
}

pub fn set_keep_days(d: i64) {
    KEEP_DAYS.store(d.clamp(1, 3650), Ordering::Relaxed);
}

static LOG: Mutex<VecDeque<Entry>> = Mutex::new(VecDeque::new());
static VERSION: AtomicUsize = AtomicUsize::new(0);

impl Level {
    fn code(self) -> i64 {
        match self {
            Level::Debug => 0,
            Level::Info => 1,
            Level::Warn => 2,
            Level::Error => 3,
        }
    }
    fn from_code(c: i64) -> Self {
        match c {
            0 => Level::Debug,
            2 => Level::Warn,
            3 => Level::Error,
            _ => Level::Info,
        }
    }
}

enum Msg {
    Entry(Entry),
    Clear,
}

static WRITER: Mutex<Option<std::sync::mpsc::Sender<Msg>>> = Mutex::new(None);

/// 今天的只显示时分秒,更早的带上月-日
fn fmt_time(ts_ms: i64) -> String {
    use chrono::TimeZone as _;
    let Some(t) = chrono::Local.timestamp_millis_opt(ts_ms).single() else { return String::new() };
    if t.date_naive() == chrono::Local::now().date_naive() { t.format("%H:%M:%S").to_string() } else { t.format("%m-%d %H:%M").to_string() }
}

/// 启动时从库里读回最近的日志,放在内存缓冲的最前面(此前已写入的新条目保持在后)
pub fn restore(st: &crate::store::Store) {
    if let Some(d) = st.get_setting(KEY_KEEP_DAYS).and_then(|v| v.parse::<i64>().ok()) {
        set_keep_days(d);
    }
    let day_ms = 24 * 3600 * 1000;
    let now = chrono::Local::now().timestamp_millis();
    let _ = st.log_purge(now - keep_days() * day_ms, crate::store::LOG_KEEP_ROWS);
    let Ok(rows) = st.log_recent(CAP) else { return };
    let old: Vec<Entry> = rows.into_iter().map(|(ts, lv, host, msg)| Entry { time: fmt_time(ts), ts, level: Level::from_code(lv), host, msg }).collect();
    if let Ok(mut log) = LOG.lock() {
        for e in old.into_iter().rev() {
            log.push_front(e);
        }
        while log.len() > CAP {
            log.pop_front();
        }
    }
    VERSION.fetch_add(1, Ordering::Relaxed);
}

/// 开始落盘:后台线程自己开一个库连接,每 0.5 秒或攒够 50 条写一批,每天清一次过期记录。
pub fn start_writer() {
    let Ok(mut st) = crate::store::Store::open_default() else { return };
    let (tx, rx) = std::sync::mpsc::channel::<Msg>();
    let spawned = std::thread::Builder::new().name("log-writer".into()).spawn(move || {
        let flush = |st: &mut crate::store::Store, batch: &mut Vec<(i64, i64, String, String)>| {
            if !batch.is_empty() {
                if let Err(e) = st.log_insert(batch) {
                    eprintln!("写日志失败: {e}");
                }
                batch.clear();
            }
        };
        let mut batch = Vec::new();
        let mut last_purge = std::time::Instant::now();
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(Msg::Entry(e)) => {
                    batch.push((e.ts, e.level.code(), e.host, e.msg));
                    if batch.len() >= 50 {
                        flush(&mut st, &mut batch);
                    }
                }
                Ok(Msg::Clear) => {
                    batch.clear();
                    if let Err(e) = st.log_clear() {
                        eprintln!("清空日志失败: {e}");
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => flush(&mut st, &mut batch),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    flush(&mut st, &mut batch);
                    return;
                }
            }
            if last_purge.elapsed() > std::time::Duration::from_secs(24 * 3600) {
                last_purge = std::time::Instant::now();
                let now = chrono::Local::now().timestamp_millis();
                let _ = st.log_purge(now - keep_days() * 24 * 3600 * 1000, crate::store::LOG_KEEP_ROWS);
            }
        }
    });
    if spawned.is_ok() {
        if let Ok(mut w) = WRITER.lock() {
            *w = Some(tx);
        }
    }
}

pub fn push(level: Level, host: &str, msg: impl Into<String>) {
    let ts = chrono::Local::now().timestamp_millis();
    let entry = Entry { time: fmt_time(ts), ts, level, host: host.to_string(), msg: msg.into() };
    if let Ok(w) = WRITER.lock() {
        if let Some(tx) = w.as_ref() {
            let _ = tx.send(Msg::Entry(entry.clone()));
        }
    }
    if let Ok(mut log) = LOG.lock() {
        if log.len() >= CAP {
            log.pop_front();
        }
        log.push_back(entry);
    }
    VERSION.fetch_add(1, Ordering::Relaxed);
}

/// 最新的在前
pub fn snapshot() -> Vec<Entry> {
    LOG.lock().map(|l| l.iter().rev().cloned().collect()).unwrap_or_default()
}

pub fn len() -> usize {
    LOG.lock().map(|l| l.len()).unwrap_or(0)
}

/// 每写入或清空一次加一,界面据此判断要不要重绘
pub fn version() -> usize {
    VERSION.load(Ordering::Relaxed)
}

/// 清空内存里的和库里的
pub fn clear() {
    if let Ok(w) = WRITER.lock() {
        if let Some(tx) = w.as_ref() {
            let _ = tx.send(Msg::Clear);
        }
    }
    if let Ok(mut log) = LOG.lock() {
        log.clear();
    }
    VERSION.fetch_add(1, Ordering::Relaxed);
}
