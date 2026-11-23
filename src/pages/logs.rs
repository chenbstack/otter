//! 日志标签的内容(mock):按容器生成确定性日志,可切换"跟随最新"。

use super::super::*;

const MSGS: [(&str, &str); 12] = [
    ("INFO", "GET /api/health 200 3ms"),
    ("INFO", "connection accepted from 172.18.0.4:51022"),
    ("INFO", "cache refreshed (214 entries)"),
    ("INFO", "worker 3 started"),
    ("INFO", "checkpoint complete, 12 segments"),
    ("INFO", "POST /api/session 201 18ms"),
    ("WARN", "slow request: GET /api/library 842ms"),
    ("WARN", "retrying upstream: connection reset by peer"),
    ("INFO", "config reloaded"),
    ("ERROR", "TLS handshake error from 10.0.0.9:51832: EOF"),
    ("INFO", "GET /metrics 200 1ms"),
    ("INFO", "gc: freed 38 MB in 12ms"),
];

fn seed(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// 确定性 mock 日志:(时间, 级别, 内容)
pub(crate) fn mock_logs(stack: &str, svc: &str, n: usize) -> Vec<(String, &'static str, &'static str)> {
    let mut x = seed(&format!("{stack}/{svc}")) | 1;
    let mut sec = 8 * 3600 + 14 * 60;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            sec += 1 + (x % 9) as usize;
            let (lv, msg) = MSGS[(x >> 8) as usize % MSGS.len()];
            (format!("{:02}:{:02}:{:02}", sec / 3600 % 24, sec / 60 % 60, sec % 60), lv, msg)
        })
        .collect()
}

impl Home {
    pub(crate) fn logs_view(&self, tab_ix: usize, stack: &str, svc: &str, cx: &mut Context<Self>) -> Div {
        let tab = &self.workspace.tabs[tab_ix];
        let follow = tab.follow;
        let handle = tab.log_scroll.clone();
        if follow {
            handle.scroll_to_bottom();
        }
        let lines = mock_logs(stack, svc, 400);
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(44.0))
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(16.0))
                    .border_b_1()
                    .border_color(c::border())
                    .child(icon(IconName::ScrollText, 15.0, c::accent()))
                    .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child(svc.to_string()))
                    .child(div().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::ghost()).child(stack.to_string()))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("logs-follow")
                            .h(px(26.0))
                            .px(px(10.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .rounded(px(t::RADIUS_SM))
                            .text_size(px(t::TYPE_CONTROL))
                            .cursor_pointer()
                            .when(follow, |d| d.bg(c::accent_soft()).text_color(c::accent()))
                            .when(!follow, |d| d.text_color(c::muted()).hover(|s| s.bg(c::hover()).text_color(c::secondary())))
                            .child(icon(IconName::ArrowDownToLine, 13.0, if follow { c::accent() } else { c::ghost() }))
                            .child("跟随最新")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(t) = this.workspace.tabs.get_mut(tab_ix) {
                                    t.follow = !t.follow;
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id(("logs-body", tab_ix))
                    .flex_1()
                    .min_h_0()
                    .m(px(16.0))
                    .rounded(px(t::RADIUS_LG))
                    .border_1()
                    .border_color(c::border())
                    .bg(c::sunken())
                    .overflow_y_scroll()
                    .track_scroll(&handle)
                    .py(px(8.0))
                    .font_family(MONO)
                    .text_size(px(t::TYPE_CAPTION))
                    .children(lines.into_iter().map(|(ts, lv, msg)| {
                        let color = match lv {
                            "WARN" => c::warn(),
                            "ERROR" => c::danger(),
                            _ => c::muted(),
                        };
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(12.0))
                            .px(px(14.0))
                            .h(px(20.0))
                            .items_center()
                            .child(div().w(px(60.0)).flex_shrink_0().text_color(c::ghost()).child(ts))
                            .child(div().w(px(40.0)).flex_shrink_0().text_color(color).child(lv))
                            .child(div().text_color(if lv == "INFO" { c::secondary() } else { color }).child(msg))
                    })),
            )
    }
}
