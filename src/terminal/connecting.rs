//! 「连接中」页:点节点卡片后先显示它,逐步展示连接过程;失败时留在这页显示原因,可重试。
//! 没有卡片:整页居中排版,大图标 + 标题 + 横向步骤条 + 失败原因 + 操作。
//! 首次连接的主机指纹确认以卡片形式显示在本页(信任并继续 / 取消);密码、口令仍在终端里输入,进入等待时自动切到终端画布。

use ::gpui::{Animation, AnimationExt as _, bounce, ease_in_out, ease_out_quint, linear};
use std::time::Duration;
use gpui_kit::prelude::FluentBuilder as _;

use super::ssh::{STEP_LABELS, Step, StepState};
use super::*;

/// 连接页服务的对象:终端标签(下标),或文件页的 SFTP 连接。
#[derive(Clone, Copy)]
pub(crate) enum ConnTarget {
    Terminal(usize),
    /// 参数:文件页是标签(true)还是抽屉(false),决定「取消」关谁
    Sftp(bool),
}

pub(crate) const SFTP_STEP_LABELS: [&str; 3] = ["连接服务器", "身份认证", "打开 SFTP"];

impl Home {
    pub(crate) fn connecting_view(&self, steps: Vec<Step>, cx: &mut Context<Self>) -> Div {
        let Some(a) = self.workspace.active else { return div() };
        let prompt = self.workspace.tabs[a].term.as_ref().and_then(|t| t.handle.host_prompt());
        self.connecting_page(ConnTarget::Terminal(a), steps, prompt, STEP_LABELS, cx)
    }

    /// 「连接中」整页:步骤条、指纹确认、失败原因和重试;终端和 SFTP 共用。
    pub(crate) fn connecting_page(&self, target: ConnTarget, steps: Vec<Step>, prompt: Option<super::ssh::HostKeyPrompt>, labels: [&'static str; 3], cx: &mut Context<Self>) -> Div {
        let (name, conn, log_scroll) = match target {
            ConnTarget::Terminal(a) => {
                let tab = &self.workspace.tabs[a];
                match (tab.node(), tab.term.as_ref()) {
                    (Some(id), Some(t)) => {
                        let (g, n) = self.locate(id);
                        (self.groups[g].nodes[n].name.clone(), format!("{}@{}:{}", t.conn.user, t.conn.host, t.conn.port), tab.log_scroll.clone())
                    }
                    _ => (String::new(), String::new(), tab.log_scroll.clone()),
                }
            }
            ConnTarget::Sftp(_) => {
                let (g, n) = self.locate(self.files.node());
                let node = &self.groups[g].nodes[n];
                let c = super::ssh::Conn::parse(&node.user_host);
                let scroll = self.workspace.active.and_then(|a| self.workspace.tabs.get(a)).map(|t| t.log_scroll.clone()).unwrap_or_default();
                (node.name.clone(), format!("{}@{}:{}", c.user, c.host, c.port), scroll)
            }
        };
        let failed = steps.iter().any(|s| matches!(s.state, StepState::Failed(_)));
        let hue = if failed { c::danger() } else { c::accent() };

        // 横向步骤条:每列上面是「线 图标 线」,下面是名称;已完成的步骤后面那段线是绿色
        let n = STEP_LABELS.len();
        let col = |i: usize| {
            let state = steps.get(i).map(|s| s.state.clone());
            let mark: AnyElement = match &state {
                Some(StepState::Done) => icon(IconName::CircleCheck, 20.0, c::online()).into_any_element(),
                Some(StepState::Failed(_)) => icon(IconName::CircleX, 20.0, c::danger()).into_any_element(),
                Some(StepState::Running) => div()
                    .size(px(10.0))
                    .rounded_full()
                    .bg(c::accent())
                    .with_animation(("conn-pulse", i), Animation::new(std::time::Duration::from_millis(900)).repeat().with_easing(bounce(ease_in_out)), |d, t| d.opacity(0.3 + 0.7 * t))
                    .into_any_element(),
                None => div().size(px(8.0)).rounded_full().border_1().border_color(c::ghost()).into_any_element(),
            };
            let code = match &state {
                Some(StepState::Done) => 1,
                Some(StepState::Failed(_)) => 2,
                Some(StepState::Running) => 3,
                None => 0,
            };
            // 状态变化时图标淡入(id 带上状态,换状态就重新播一次)
            let mark = div().child(mark).with_animation(("conn-mark", i * 10 + code), Animation::new(Duration::from_millis(320)).with_easing(ease_out_quint()), |d, t| d.opacity(t));
            let tone = match &state {
                Some(StepState::Failed(_)) => c::danger(),
                Some(StepState::Running) => c::text(),
                Some(StepState::Done) => c::secondary(),
                None => c::ghost(),
            };
            let line = |on: bool, show: bool| div().flex_1().h(px(1.0)).when(show, |d| d.bg(if on { c::online().opacity(0.6) } else { c::border() }));
            let left_on = i > 0 && matches!(steps.get(i - 1).map(|s| &s.state), Some(StepState::Done));
            let right_on = matches!(state, Some(StepState::Done));
            // 正在进行的步骤后面那段线呼吸,表示在往下一步走
            // 最后一步右侧没有下一步,线只占位不显示(否则会多出一截)
            let right: AnyElement = if matches!(state, Some(StepState::Running)) && i + 1 < n {
                div()
                    .flex_1()
                    .h(px(1.0))
                    .bg(c::accent())
                    .with_animation(("conn-line", i), Animation::new(Duration::from_millis(1200)).repeat().with_easing(bounce(ease_in_out)), |d, t| d.opacity(0.12 + 0.6 * t))
                    .into_any_element()
            } else {
                line(right_on, i + 1 < n).into_any_element()
            };
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .w_full()
                        .h(px(20.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(line(left_on, i > 0))
                        .child(div().w(px(20.0)).flex_shrink_0().flex().items_center().justify_center().child(mark))
                        .child(right),
                )
                .child(div().text_size(px(t::TYPE_CONTROL)).text_color(tone).child(labels[i]))
        };

        // 本次连接的日志:只看这台主机、从最近一次「开始连接」起,旧的在上,始终滚到最下面
        let mut mine: Vec<_> = crate::logs::snapshot().into_iter().filter(|e| e.host == conn).collect();
        mine.reverse();
        let from = mine.iter().rposition(|e| e.msg == "开始连接").unwrap_or(0);
        let mine = mine.split_off(from);
        let log_rows: Vec<_> = mine
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                let col = match e.level {
                    crate::logs::Level::Info => c::muted(),
                    crate::logs::Level::Warn => c::warn(),
                    crate::logs::Level::Error => c::danger(),
                };
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(10.0))
                    .text_size(px(t::TYPE_MICRO))
                    .font_family(MONO)
                    .child(div().w(px(56.0)).flex_shrink_0().text_color(c::ghost()).child(e.time))
                    .child(div().flex_1().min_w_0().truncate().text_color(col).child(e.msg))
                    .with_animation(("conn-log-row", i), Animation::new(Duration::from_millis(300)).with_easing(ease_out_quint()), |d, t| d.opacity(t))
            })
            .collect();
        log_scroll.scroll_to_bottom();
        let log_rows = div().id("conn-log").flex_1().min_h_0().overflow_y_scroll().track_scroll(&log_scroll).flex().flex_col().gap(px(6.0)).children(log_rows);
        let log_area = div()
            .w(px(380.0))
            .h(px(170.0))
            .flex_shrink_0()
            .mt(px(40.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(div().text_size(px(t::TYPE_MICRO)).font_weight(FontWeight::SEMIBOLD).text_color(c::muted()).child("连接日志"))
            .child(log_rows);

        // 首次连接的主机指纹确认:卡片替代下面的日志和操作按钮
        let confirm = prompt.as_ref().map(|p| {
            host_key_card(
                p,
                cx.listener(move |this, _, _, cx| {
                    this.answer_host_for(target, true);
                    cx.notify();
                }),
                cx.listener(move |this, _, _, cx| {
                    this.answer_host_for(target, false);
                    cx.notify();
                }),
            )
        });

        let title = if failed { format!("无法连接 {name}") } else { format!("正在连接 {name}") };
        let actions = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .when(failed, |d| {
                d.child(
                    div()
                        .id("conn-retry")
                        .h(px(30.0))
                        .px(px(14.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(t::RADIUS_MD))
                        .bg(c::accent())
                        .text_color(c::on_accent())
                        .text_size(px(t::TYPE_CONTROL))
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.88))
                        .child(icon(IconName::RotateCw, 13.0, c::on_accent()))
                        .child("重新连接")
                        .on_click(cx.listener(move |this, _, window, cx| match target {
                            ConnTarget::Terminal(_) => this.reconnect_terminal(window, cx),
                            ConnTarget::Sftp(_) => {
                                this.files.drop_sftp();
                                cx.notify();
                            }
                        })),
                )
                .child(button("conn-logs", IconName::ScrollText, "查看日志").on_click(cx.listener(|this, _, _, cx| {
                    this.activate(0);
                    this.set_nodes_view(crate::nodes::NodesView::Logs, cx);
                    cx.notify();
                })))
            })
            .child(button("conn-close", IconName::X, if failed { "关闭" } else { "取消" }).on_click(cx.listener(move |this, _, _, cx| match target {
            ConnTarget::Terminal(a) => this.close_tab(a, cx),
            ConnTarget::Sftp(tab) => this.close_sftp_page(tab, cx),
        })));

        // 图标区:连接中向外扩散两圈涟漪;失败时图标左右抖一下
        let ring = |k: usize, ms: u64| {
            div().absolute().inset_0().flex().items_center().justify_center().child(
                div()
                    .rounded_full()
                    .border_1()
                    .border_color(c::accent())
                    .with_animation(("conn-ring", k), Animation::new(Duration::from_millis(ms)).repeat().with_easing(ease_out_quint()), |d, t| d.size(px(44.0 + 60.0 * t)).opacity(0.5 * (1.0 - t))),
            )
        };
        let glyph: AnyElement = if failed {
            div()
                .relative()
                .child(icon(IconName::Server, 40.0, hue))
                .with_animation("conn-shake", Animation::new(Duration::from_millis(460)).with_easing(linear), |d, t| d.left(px(9.0 * (1.0 - t) * (t * 20.0).sin())))
                .into_any_element()
        } else {
            icon(IconName::Server, 40.0, hue).into_any_element()
        };
        let hero = div()
            .relative()
            .size(px(104.0))
            .flex()
            .items_center()
            .justify_center()
            .when(!failed, |d| d.child(ring(0, 2000)).child(ring(1, 3000)))
            .child(glyph);

        div().flex_1().min_w_0().min_h_0().bg(c::bg()).flex().items_center().justify_center().child(
            div()
                .w(px(520.0))
                .max_w_full()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0))
                .child(hero)
                .child(div().text_size(px(20.0)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(title))
                .child(div().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::ghost()).child(conn))
                .child(div().w_full().mt(px(32.0)).flex().flex_row().children((0..n).map(col)))
                .children(confirm)
                .when(prompt.is_none(), |d| d.child(div().mt(px(28.0)).child(actions)).child(log_area))
                // 页面进入:淡入并从下方轻轻升起
                .with_animation("conn-enter", Animation::new(Duration::from_millis(420)).with_easing(ease_out_quint()), |d, t| d.opacity(t).mt(px(16.0 * (1.0 - t)))),
        )
    }
}

impl Home {
    fn answer_host_for(&mut self, target: ConnTarget, yes: bool) {
        match target {
            ConnTarget::Terminal(a) => {
                if let Some(t) = self.workspace.tabs.get(a).and_then(|t| t.term.as_ref()) {
                    t.handle.answer_host(yes);
                }
            }
            ConnTarget::Sftp(_) => {
                if let Some(s) = self.files.sftp_ref() {
                    s.answer_host(yes);
                }
            }
        }
    }
}

/// 首次连接的主机指纹确认卡片(连接页和文件页共用)。
pub(crate) fn host_key_card(
    p: &super::ssh::HostKeyPrompt,
    accept: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    deny: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
            let row = |k: &'static str, v: String, mono: bool| {
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(12.0))
                    .child(div().w(px(40.0)).flex_shrink_0().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(k))
                    .child(div().flex_1().min_w_0().text_size(px(if mono { t::TYPE_CONTROL } else { t::TYPE_BODY })).when(mono, |d| d.font_family(MONO)).text_color(c::text()).child(v))
            };
            div()
                .w_full()
                .mt(px(28.0))
                .p(px(18.0))
                .flex()
                .flex_col()
                .gap(px(14.0))
                .rounded(px(t::RADIUS_LG))
                .bg(c::dialog())
                .border_1()
                .border_color(c::dialog_border())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(12.0))
                        .child(
                            div()
                                .size(px(36.0))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(t::RADIUS_MD))
                                .bg(c::accent_soft())
                                .child(icon(IconName::FingerprintPattern, 18.0, c::accent())),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child("首次连接这台服务器"))
                                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child("dockyard 还不认识它。请核对指纹与服务器一致,再继续;继续后会记住这台主机。")),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .p(px(12.0))
                        .rounded(px(t::RADIUS_MD))
                        .bg(c::field())
                        .border_1()
                        .border_color(c::field_border())
                        .child(row("主机", format!("{}:{}", p.host, p.port), true))
                        .child(row("算法", p.algo.clone(), true))
                        .child(row("指纹", p.fingerprint.clone(), true)),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(button("host-deny", IconName::X, "取消").on_click(deny))
                        .child(
                            div()
                                .id("host-accept")
                                .h(px(30.0))
                                .px(px(14.0))
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .rounded(px(t::RADIUS_MD))
                                .bg(c::accent())
                                .text_color(c::on_accent())
                                .text_size(px(t::TYPE_CONTROL))
                                .font_weight(FontWeight::MEDIUM)
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.88))
                                .child(icon(IconName::Check, 13.0, c::on_accent()))
                                .child("信任并继续")
                                .on_click(accept),
                        ),
                )
}
