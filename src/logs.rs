//! 应用日志:连接、认证、主机密钥等事件。只存在内存里(环形缓冲,重启清空)。
//! 终端会话在后台 tokio 线程里写入,界面线程读取,所以用全局锁而不是 `Home` 的字段。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const CAP: usize = 500;

#[derive(Clone, Copy, PartialEq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone)]
pub struct Entry {
    /// 本地时间 HH:MM:SS
    pub time: String,
    pub level: Level,
    /// 事件对应的主机,如 `root@1.2.3.4:22`;没有则为空
    pub host: String,
    pub msg: String,
}

static LOG: Mutex<VecDeque<Entry>> = Mutex::new(VecDeque::new());
static VERSION: AtomicUsize = AtomicUsize::new(0);

pub fn push(level: Level, host: &str, msg: impl Into<String>) {
    let entry = Entry { time: chrono::Local::now().format("%H:%M:%S").to_string(), level, host: host.to_string(), msg: msg.into() };
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

pub fn clear() {
    if let Ok(mut log) = LOG.lock() {
        log.clear();
    }
    VERSION.fetch_add(1, Ordering::Relaxed);
}
