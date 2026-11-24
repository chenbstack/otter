//! 容器日志:从容器列表右侧滑出的抽屉,不占标签页。文件管理用的是和 SFTP 同一套文件界面(见 `files.rs`)。
//!
//! 日志来自 `docker logs`(抽屉开着时每 2 秒刷新),数据由后台 SSH 任务读取(`crate::docker` 的 `Cmd::Ct`),这里只负责界面。

use std::time::{Duration, Instant};

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};

use super::super::*;
use super::dockerpages::{icon_btn, loading};
use super::syspages::{chip, tip};
use crate::ctfiles::{self, CtKind, Level, LogLine};
use crate::files::Target;

/// 日志刷新间隔
const LOG_EVERY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Default)]
pub(crate) enum CtTab {
    #[default]
    Logs,
    Files,
}

#[derive(Default)]
pub(crate) struct CtUi {
    tab: CtTab,
    /// 打开的容器:名字、是否在运行
    open: Option<(String, bool)>,
    lines: Option<Result<Vec<LogLine>, String>>,
    /// 不再跟随最新日志
    paused: bool,
    search: Option<Entity<InputState>>,
    scroll: ScrollHandle,
    log_asked: Option<Instant>,
    log_inflight: bool,
    demo_opened: bool,
}

impl Home {
    fn ct_docker(&self) -> Option<&crate::docker::Docker> {
        self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref())
    }

    pub(crate) fn ct_open(&mut self, name: String, running: bool, tab: CtTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.ct.open.as_ref().is_none_or(|(n, _)| *n != name) {
            let ct = &mut self.dock.ct;
            ct.lines = None;
            ct.paused = false;
            ct.log_asked = None;
            ct.log_inflight = false;
        }
        self.dock.ct.open = Some((name.clone(), running));
        self.dock.ct.tab = tab;
        if tab == CtTab::Files {
            self.open_files(Target::Container { stack: String::new(), svc: name }, window, cx);
        }
        if self.dock.ct.search.is_none() {
            let s = cx.new(|cx| InputState::new(window, cx).placeholder("搜索日志"));
            cx.observe(&s, |_, _, cx| cx.notify()).detach();
            self.dock.ct.search = Some(s);
        }
        cx.notify();
    }

    /// 堆栈卡片上的「文件」:找到对应的容器,在抽屉的文件页打开
    pub(crate) fn ct_open_stack(&mut self, stack: &str, svc: &str, target: &Target, window: &mut Window, cx: &mut Context<Self>) {
        let found = self.dock.snap.containers.iter().find(|c| c.project.as_deref() == Some(stack) && c.service.as_deref() == Some(svc)).or_else(|| self.dock.snap.containers.iter().find(|c| c.name == svc)).map(|c| (c.name.clone(), c.running()));
        match found {
            Some((n, up)) => self.ct_open(n, up, CtTab::Files, window, cx),
            None => self.open_files(target.clone(), window, cx),
        }
    }

    fn ct_close(&mut self, cx: &mut Context<Self>) {
        self.dock.ct.open = None;
        self.files.open = false;
        cx.notify();
    }

    /// 每次渲染前调用:收后台读到的日志,按需要发起读取
    pub(crate) fn ct_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 调试:截图时直接打开抽屉
        if self.demo && self.dock.ct.open.is_none() && !self.dock.ct.demo_opened && std::env::var_os("OTTER_CT").is_some() {
            self.dock.ct.demo_opened = true;
            if let Some(ct) = self.dock.snap.containers.first().cloned() {
                let tab = if std::env::var("OTTER_CT").is_ok_and(|v| v == "files") { CtTab::Files } else { CtTab::Logs };
                self.ct_open(ct.name.clone(), ct.running(), tab, window, cx);
            }
        }
        let Some((name, _)) = self.dock.ct.open.clone() else { return };
        if !self.page.is_docker() {
            return;
        }
        let up = self.dock.snap.containers.iter().find(|c| c.name == name).map(|c| c.running());
        if let (Some(up), Some(o)) = (up, self.dock.ct.open.as_mut()) {
            o.1 = up;
        }
        if self.dock.ct.tab != CtTab::Logs {
            return;
        }
        if self.demo {
            if self.dock.ct.lines.is_none() {
                self.dock.ct.lines = Some(Ok(ctfiles::parse_logs(DEMO_LOGS)));
            }
            return;
        }
        let got = self.ct_docker().and_then(|d| d.ct_take(CtKind::Logs));
        if let Some((tag, res)) = got {
            self.dock.ct.log_inflight = false;
            if tag == name {
                self.dock.ct.lines = Some(res.map(|t| ctfiles::parse_logs(&t)));
            }
        }
        if !self.dock.ct.log_inflight && self.dock.ct.log_asked.is_none_or(|t| t.elapsed() >= LOG_EVERY) {
            self.dock.ct.log_asked = Some(Instant::now());
            self.dock.ct.log_inflight = true;
            let args = ["logs", "--tail", "500", "-t", &name].into_iter().map(String::from).collect();
            if let Some(d) = self.ct_docker() {
                d.ct_load(CtKind::Logs, &name, args, true);
            }
        }
    }

    /// 日志抽屉在前台、过了刷新间隔:需要重绘来触发下一次读取
    pub(crate) fn ct_due(&self) -> bool {
        self.dock.ct.open.is_some() && self.dock.ct.tab == CtTab::Logs && !self.demo && !self.dock.ct.log_inflight && self.dock.ct.log_asked.is_some_and(|t| t.elapsed() >= LOG_EVERY)
    }

    /// 抽屉本体(只在容器、堆栈这些 Docker 页上显示),遮罩铺满整个窗口(含标题栏),点遮罩关闭
    pub(crate) fn ct_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (name, _) = self.dock.ct.open.clone()?;
        if !self.page.is_docker() {
            return None;
        }
        let tab = self.dock.ct.tab;
        let w = (f32::from(window.viewport_size().width) * 0.62).clamp(520.0, 820.0);
        let header = div()
            .h(px(52.0))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(16.0))
            .border_b_1()
            .border_color(c::border())
            .child(icon(IconName::Container, 16.0, c::secondary()))
            .child(div().min_w_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(name.clone()))
            .child(div().w(px(8.0)))
            .child(chip(("ct-tab", 0), "日志", tab == CtTab::Logs).on_click(cx.listener(|this, _, _, cx| {
                this.dock.ct.tab = CtTab::Logs;
                cx.notify();
            })))
            .child(chip(("ct-tab", 1), "文件", tab == CtTab::Files).on_click(cx.listener(|this, _, window, cx| {
                if let Some((n, up)) = this.dock.ct.open.clone() {
                    this.ct_open(n, up, CtTab::Files, window, cx);
                }
            })))
            .child(div().flex_1())
            .child(tip(icon_btn(("ct-close", 0), IconName::X, c::muted()).on_click(cx.listener(|this, _, _, cx| this.ct_close(cx))), "关闭"));
        let body = match tab {
            CtTab::Logs => self.ct_logs(cx),
            CtTab::Files => self.files_embedded(window, cx, w),
        };
        Some(
            div()
                .id("ct-scrim")
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .flex_row()
                .justify_end()
                .bg(c::scrim_dialog())
                .occlude()
                .on_click(cx.listener(|this, _, _, cx| this.ct_close(cx)))
                .child(
                    div()
                        .id("ct-drawer")
                        .occlude()
                        .h_full()
                        .w(px(w))
                        .flex()
                        .flex_col()
                        // 和应用窗口同一个底色
                        .bg(c::bg())
                        .border_l_1()
                        .border_color(c::dialog_border())
                        .shadow_lg()
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(header)
                        .child(body),
                )
                .into_any_element(),
        )
    }

    fn ct_logs(&self, cx: &mut Context<Self>) -> AnyElement {
        let ct = &self.dock.ct;
        let q = ct.search.as_ref().map(|s| s.read(cx).value().trim().to_lowercase()).unwrap_or_default();
        let follow = !ct.paused;
        let toolbar = div()
            .h(px(46.0))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(16.0))
            .children(ct.search.clone().map(|s| {
                div()
                    .flex_1()
                    .h(px(30.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .pl(px(10.0))
                    .rounded(px(t::RADIUS_MD))
                    .bg(c::field())
                    .border_1()
                    .border_color(c::field_border())
                    .text_size(px(t::TYPE_CONTROL))
                    .child(icon(IconName::Search, 13.0, c::ghost()))
                    .child(div().flex_1().min_w_0().ml(px(-4.0)).child(Input::new(&s).appearance(false).with_size(px(12.0 / 0.875))))
            }))
            .child(chip(("ct-follow", 0), "跟随最新", follow).on_click(cx.listener(|this, _, _, cx| {
                this.dock.ct.paused = !this.dock.ct.paused;
                cx.notify();
            })))
            .child(tip(
                icon_btn(("ct-log-refresh", 0), IconName::RefreshCw, c::muted()).on_click(cx.listener(|this, _, _, cx| {
                    this.dock.ct.log_asked = None;
                    cx.notify();
                })),
                "刷新",
            ));
        let content: AnyElement = match &ct.lines {
            None => loading("正在读取日志…").into_any_element(),
            Some(Err(e)) => ct_note_view(IconName::TriangleAlert, "读不到日志", e.clone()),
            Some(Ok(lines)) => {
                let shown: Vec<&LogLine> = lines.iter().filter(|l| q.is_empty() || l.msg.to_lowercase().contains(&q)).collect();
                if shown.is_empty() {
                    ct_note_view(IconName::ScrollText, if lines.is_empty() { "还没有日志" } else { "没有匹配的日志" }, if lines.is_empty() { "容器没有输出任何内容" } else { "换个关键字试试" })
                } else {
                    if follow {
                        ct.scroll.scroll_to_bottom();
                    }
                    div()
                        .id("ct-log-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&ct.scroll)
                        .px(px(16.0))
                        .py(px(6.0))
                        .flex()
                        .flex_col()
                        .children(shown.into_iter().map(|l| {
                            let color = match l.level {
                                Level::Error => c::danger(),
                                Level::Warn => c::warn(),
                                Level::Info => c::secondary(),
                            };
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(10.0))
                                .py(px(1.0))
                                .text_size(px(t::TYPE_CAPTION))
                                .font_family(MONO)
                                .child(div().w(px(100.0)).flex_shrink_0().text_color(c::ghost()).child(l.ts.clone()))
                                .child(div().flex_1().min_w_0().text_color(color).child(l.msg.clone()))
                        }))
                        .into_any_element()
                }
            }
        };
        div().flex_1().min_h_0().flex().flex_col().child(toolbar).child(content).into_any_element()
    }
}

/// 抽屉里居中的说明(没有内容、出错、容器没运行)
fn ct_note_view(ic: IconName, title: impl Into<SharedString>, text: impl Into<SharedString>) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .px(px(32.0))
        .child(icon(ic, 26.0, c::ghost()))
        .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).text_color(c::secondary()).child(title.into()))
        .child(div().max_w(px(380.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(text.into()))
        .into_any_element()
}

// ---- 演示数据 ----

const DEMO_LOGS: &str = "\
2026-11-24T09:12:01.120Z /docker-entrypoint.sh: Launching /docker-entrypoint.d/30-tune-worker-processes.sh
2026-11-24T09:12:01.340Z /docker-entrypoint.sh: Configuration complete; ready for start up
2026-11-24T09:12:01.402Z 2026/11/24 09:12:01 [notice] 1#1: nginx/1.27.2
2026-11-24T09:12:01.402Z 2026/11/24 09:12:01 [notice] 1#1: start worker processes
2026-11-24T09:12:18.771Z 172.18.0.1 - - [24/Nov/2026:09:12:18 +0000] \"GET / HTTP/1.1\" 200 615 \"-\" \"curl/8.4.0\"
2026-11-24T09:12:19.002Z 172.18.0.1 - - [24/Nov/2026:09:12:19 +0000] \"GET /favicon.ico HTTP/1.1\" 404 153 \"-\" \"Mozilla/5.0\"
2026-11-24T09:13:02.550Z 2026/11/24 09:13:02 [warn] 29#29: *14 upstream server temporarily disabled while connecting to upstream
2026-11-24T09:13:02.551Z 2026/11/24 09:13:02 [error] 29#29: *14 connect() failed (111: Connection refused) while connecting to upstream, client: 172.18.0.1, server: _, request: \"GET /api/health HTTP/1.1\"
2026-11-24T09:13:30.090Z 172.18.0.1 - - [24/Nov/2026:09:13:30 +0000] \"GET /api/health HTTP/1.1\" 502 157 \"-\" \"curl/8.4.0\"
2026-11-24T09:14:11.832Z 172.18.0.1 - - [24/Nov/2026:09:14:11 +0000] \"GET /api/health HTTP/1.1\" 200 17 \"-\" \"curl/8.4.0\"
";

