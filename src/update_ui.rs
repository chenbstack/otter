//! 自动更新的界面侧:后台定时检查、点击后下载并安装。纯逻辑在 `update.rs`。

use std::time::Duration;

use crate::Home;
use crate::logs::{self, Level};
use crate::update::{self, State};

/// 「自动检查更新」开关,存 `update_auto`(`on`/`off`,缺省开)
static AUTO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
pub const KEY_AUTO: &str = "update_auto";

pub fn auto_check() -> bool {
    AUTO.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_auto_check(on: bool) {
    AUTO.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// 启动时按记住的设置恢复开关
pub fn restore(st: &crate::store::Store) {
    set_auto_check(st.get_setting(KEY_AUTO).as_deref() != Some("off"));
}

/// 两次检查的间隔
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

impl Home {
    /// 启动几秒后检查一次,之后每隔几小时再查。检查失败(没网、仓库不公开等)只记日志,不打扰用户。
    pub(crate) fn start_update_check(&mut self, cx: &mut gpui::Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(4)).await;
            loop {
                if !auto_check() {
                    // 开关关着:不查,过一会儿再看开关
                    if this.update(cx, |_, _| {}).is_err() {
                        break;
                    }
                    cx.background_executor().timer(Duration::from_secs(60)).await;
                    continue;
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(update::check());
                });
                let res = rx.await;
                let alive = this.update(cx, |this, cx| {
                    match res {
                        Ok(Ok(Some(u))) => {
                            // 下载中不打断;已经提示过同一版本也不重复改状态
                            if matches!(this.update, State::Idle) {
                                logs::push(Level::Debug, "更新", format!("发现新版本 {}", u.version));
                                this.update = State::Available(u);
                                cx.notify();
                            }
                        }
                        Ok(Ok(None)) => {}
                        Ok(Err(e)) => logs::push(Level::Debug, "更新", format!("检查更新失败: {e}")),
                        Err(_) => {}
                    }
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(CHECK_EVERY).await;
            }
        })
        .detach();
    }

    /// 设置页的「检查更新」:立刻查一次,结果写进 `settings.update_msg`;有新版本就和后台检查一样进入「可更新」状态。
    pub(crate) fn check_update_now(&mut self, cx: &mut gpui::Context<Self>) {
        if self.settings.checking {
            return;
        }
        self.settings.checking = true;
        self.settings.update_msg = None;
        cx.notify();
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(update::check());
        });
        cx.spawn(async move |this, cx| {
            let res = rx.await.unwrap_or_else(|_| Err("检查线程异常退出".into()));
            let _ = this.update(cx, |this, cx| {
                this.settings.checking = false;
                this.settings.update_msg = Some(match res {
                    Ok(Some(u)) => {
                        let msg = format!("发现新版本 {}", u.version);
                        if matches!(this.update, State::Idle) {
                            this.update = State::Available(u);
                        }
                        msg
                    }
                    Ok(None) => "已是最新版本".to_string(),
                    Err(e) => format!("检查失败:{e}"),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// 点击下载图标:有安装包就下载并交给平台安装流程(成功后退出,由安装流程重新打开新版),否则打开发布页。
    pub(crate) fn start_update(&mut self, cx: &mut gpui::Context<Self>) {
        let (State::Available(u) | State::Failed(u, _)) = self.update.clone() else { return };
        let Some(asset) = u.asset.clone() else {
            update::open_page(&u.page);
            return;
        };
        logs::push(Level::Info, "更新", format!("开始下载 {}", asset.name));
        self.update = State::Downloading(u.clone());
        cx.notify();
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(update::download(&asset).and_then(|p| update::install(&p)));
        });
        cx.spawn(async move |this, cx| {
            let res = rx.await.unwrap_or_else(|_| Err("更新线程异常退出".into()));
            let _ = this.update(cx, |this, cx| match res {
                Ok(()) => {
                    logs::push(Level::Info, "更新", "已交给安装程序,退出并重新打开");
                    cx.quit();
                }
                Err(e) => {
                    logs::push(Level::Error, "更新", format!("更新失败: {e}"));
                    this.update = State::Failed(u, e);
                    cx.notify();
                }
            });
        })
        .detach();
    }
}
