//! 关闭终端标签 / 退出窗口前的确认,以及「启动时恢复上次打开的标签」。偏好见 `prefs.rs` 和设置页「通用」。

use super::*;
use crate::terminal::ssh::Status;
use crate::workspace::{NodeMode, TabKind};

/// 等用户确认的「离开」操作
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum LeaveAsk {
    /// 关闭这个标签(标签 id,不是下标)
    Tab(u64),
    /// 关闭整个窗口
    Quit,
}

fn mode_code(m: NodeMode) -> char {
    match m {
        NodeMode::Terminal => 't',
        NodeMode::Files => 'f',
        NodeMode::Containers => 'c',
    }
}

fn mode_from(c: char) -> NodeMode {
    match c {
        'f' => NodeMode::Files,
        'c' => NodeMode::Containers,
        _ => NodeMode::Terminal,
    }
}

/// 页面(服务器模式里的二级页)的存储码
fn page_code(p: Page) -> &'static str {
    match p {
        Page::Overview => "ov",
        Page::Containers => "ct",
        Page::Database => "db",
        Page::Stacks => "st",
        Page::Images => "im",
        Page::Volumes => "vo",
        Page::Registry => "rg",
        Page::Config => "cf",
        Page::Processes => "pr",
        Page::Services => "sv",
        Page::Firewall => "fw",
        Page::Storage => "sg",
        Page::Cron => "cr",
        Page::Websites => "ws",
        Page::Store => "ap",
    }
}

fn page_from(c: &str) -> Option<Page> {
    [Page::Overview, Page::Containers, Page::Database, Page::Stacks, Page::Images, Page::Volumes, Page::Registry, Page::Config, Page::Processes, Page::Services, Page::Firewall, Page::Storage, Page::Cron, Page::Websites, Page::Store].into_iter().find(|p| page_code(*p) == c)
}

impl Home {
    /// 有 SSH 连接还开着的标签数
    fn live_terms(&self) -> usize {
        self.workspace.tabs.iter().filter(|t| t.term.as_ref().is_some_and(|s| s.handle.status() == Status::Connected)).count()
    }

    /// 用户点关闭标签:开了确认且这个标签的终端还连着,先弹确认框;否则直接关。
    pub(crate) fn request_close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(t) = self.workspace.tabs.get(ix) else { return };
        if crate::prefs::confirm_close_tab() && t.term.as_ref().is_some_and(|s| s.handle.status() == Status::Connected) {
            self.leave_ask = Some(LeaveAsk::Tab(t.id));
            cx.notify();
        } else {
            self.close_tab(ix, cx);
        }
    }

    /// 窗口要关了:开了确认且有终端连着就拦下来问,返回 false 表示先不关。
    pub(crate) fn request_quit(&mut self, cx: &mut Context<Self>) -> bool {
        if self.demo || !crate::prefs::confirm_quit() || self.live_terms() == 0 {
            return true;
        }
        self.leave_ask = Some(LeaveAsk::Quit);
        cx.notify();
        false
    }

    fn resolve_leave(&mut self, ok: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ask) = self.leave_ask.take() else { return };
        if ok {
            match ask {
                LeaveAsk::Tab(id) => {
                    if let Some(ix) = self.workspace.tabs.iter().position(|t| t.id == id) {
                        self.close_tab(ix, cx);
                    }
                }
                LeaveAsk::Quit => window.remove_window(),
            }
        }
        cx.notify();
    }

    pub(crate) fn leave_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let ask = self.leave_ask?;
        let (title, sub, rows, ok_label) = match ask {
            LeaveAsk::Tab(id) => {
                let name = self.workspace.tabs.iter().find(|t| t.id == id).and_then(|t| t.node()).map(|n| self.node_name(Some(n))).unwrap_or_default();
                (format!("关闭「{name}」的标签?"), "终端连接还开着", vec![(IconName::TriangleAlert, c::warn(), "关闭后连接会断开,正在运行的命令会被中断".to_string())], "关闭标签")
            }
            LeaveAsk::Quit => {
                let n = self.live_terms();
                (format!("退出 Otter?"), "还有终端连接开着", vec![(IconName::TriangleAlert, c::warn(), format!("{n} 个终端连接会断开,正在运行的命令会被中断"))], "退出")
            }
        };
        let hero = crate::ui::alert_hero(IconName::TriangleAlert, c::warn(), title, sub);
        let rows = crate::ui::alert_rows(rows);
        let btn_w = crate::ui::alert_btn_w(2);
        let cancel = crate::node_form::ctl_btn("leave-cancel", None, "取消", 38.0).w(px(btn_w)).flex_none().justify_center().on_click(cx.listener(|this, _, window, cx| this.resolve_leave(false, window, cx)));
        let ok = crate::ui::alert_solid_btn("leave-ok", None, ok_label, Some(btn_w), c::danger()).on_click(cx.listener(|this, _, window, cx| this.resolve_leave(true, window, cx)));
        let buttons = div().w_full().flex().flex_row().gap(px(10.0)).child(cancel).child(ok);
        let body = crate::ui::alert_body(hero, None, rows, buttons);
        Some(crate::ui::modal_bg(
            "leave-dialog",
            crate::confirm::CONFIRM_W,
            c::dialog_glass(),
            cx.listener(|this, e: &KeyDownEvent, window, cx| {
                match e.keystroke.key.as_str() {
                    "escape" => this.resolve_leave(false, window, cx),
                    "enter" => this.resolve_leave(true, window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }),
            [body.into_any_element()],
        ))
    }

    /// 每帧调用:打开的节点标签有变化就存一份(`id:模式` 逗号分隔 + 活动标签下标)。只在开了「恢复标签」时写。
    pub(crate) fn track_tabs(&mut self) {
        if self.demo || !crate::prefs::restore_tabs() {
            return;
        }
        let Some(st) = &self.store else { return };
        let mut list = String::new();
        // 活动的是「节点」标签时记 usize::MAX,恢复后也停在它上面
        let mut active = usize::MAX;
        for (i, t) in self.workspace.tabs.iter().enumerate() {
            let TabKind::Node(id) = t.kind else { continue };
            if self.workspace.active == Some(i) {
                active = list.matches(',').count() + usize::from(!list.is_empty());
            }
            if !list.is_empty() {
                list.push(',');
            }
            // 当前标签的页面在 `self.page`,其它标签的存在标签自己身上
            let page = if self.workspace.active == Some(i) { self.page } else { t.page };
            list.push_str(&format!("{}:{}:{}", id.0, mode_code(t.mode), page_code(page)));
        }
        let key = format!("{list}|{active}");
        if self.tabs_saved.as_deref() == Some(key.as_str()) {
            return;
        }
        let _ = st.set_setting(crate::prefs::KEY_LAST_NODES, &list);
        let _ = st.set_setting(crate::prefs::KEY_LAST_ACTIVE, &active.to_string());
        self.tabs_saved = Some(key);
    }

    /// 启动时恢复上次的节点标签;节点已被删除的跳过。
    pub(crate) fn restore_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.demo || !crate::prefs::restore_tabs() {
            return;
        }
        let Some(st) = &self.store else { return };
        let list = st.get_setting(crate::prefs::KEY_LAST_NODES).unwrap_or_default();
        let active = st.get_setting(crate::prefs::KEY_LAST_ACTIVE).and_then(|v| v.parse::<usize>().ok()).unwrap_or(usize::MAX);
        // 一个节点可以有多个标签(右键「复制标签」),所以不能走 `open_node`(它遇到已有的会复用):逐个在最后新建,
        // 同一个节点的多个标签各自恢复自己的模式和页面。
        let mut opened: Vec<usize> = Vec::new();
        for item in list.split(',').filter(|s| !s.is_empty()) {
            let mut parts = item.split(':');
            let (Some(id), Some(m)) = (parts.next(), parts.next()) else { continue };
            let page = parts.next().and_then(page_from);
            let Ok(id) = id.parse::<u32>() else { continue };
            let id = crate::mock::NodeId(id);
            if !self.all_nodes().any(|n| n.id == id) {
                continue;
            }
            let ix = self.workspace.tabs.len();
            let mut tab = crate::workspace::Tab::node_tab(id);
            if let Some(p) = page {
                tab.page = p;
            }
            self.workspace.tabs.push(tab);
            self.activate(ix);
            self.set_mode(mode_from(m.chars().next().unwrap_or('t')), window, cx);
            opened.push(ix);
        }
        // 保存时的「活动标签」是节点标签列表里的第几个
        if let Some(&ix) = opened.get(active) {
            self.activate(ix);
        } else {
            self.activate(0);
        }
        cx.notify();
    }
}
