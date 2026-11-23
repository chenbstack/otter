//! 终端标签的占位内容(mock)。真实终端见 P2 计划。

use super::super::*;

impl Home {
    pub(crate) fn terminal_view(&self, node_name: &str, user_host: &str, container: Option<&(String, String)>) -> Div {
        let (who, cmd) = match container {
            Some((_, svc)) => (format!("root@{svc}:/#"), format!("docker exec -it {svc} sh")),
            None => (format!("{user_host}:~$"), "ssh".to_string()),
        };
        let host_line = match container {
            Some((stack, svc)) => format!("{node_name} · {stack}/{svc}"),
            None => node_name.to_string(),
        };
        let lines: Vec<(bool, String)> = match container {
            Some(_) => vec![
                (true, "ls /config".into()),
                (false, "cache  logs  plugins  config.xml  settings.json  system.xml".into()),
                (true, "ps aux | head -3".into()),
                (false, "USER  PID %CPU %MEM COMMAND".into()),
                (false, "root    1  0.3  1.2 /app/server --config /config".into()),
            ],
            None => vec![
                (true, "uname -sr".into()),
                (false, "Linux 6.1.0-18-amd64".into()),
                (true, "docker ps --format '{{.Names}}\t{{.Status}}' | head -3".into()),
                (false, "emby_av\tUp 35 days".into()),
                (false, "watchtower\tUp 35 days".into()),
            ],
        };
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
                    .child(icon(IconName::SquareTerminal, 15.0, c::accent()))
                    .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child("终端"))
                    .child(div().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::ghost()).child(host_line)),
            )
            .child(
                div()
                    .mx(px(16.0))
                    .mt(px(12.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(t::RADIUS_MD))
                    .bg(c::warn_soft())
                    .text_size(px(t::TYPE_CONTROL))
                    .text_color(c::warn())
                    .child(icon(IconName::TriangleAlert, 13.0, c::warn()))
                    .child("这是外观占位,真实终端(pty、SSH、输入法)在 P2 实现"),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .m(px(16.0))
                    .p(px(14.0))
                    .rounded(px(t::RADIUS_LG))
                    .border_1()
                    .border_color(c::border())
                    .bg(c::sunken())
                    .font_family(MONO)
                    .text_size(px(t::TYPE_CONTROL))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .children(lines.into_iter().map(|(is_cmd, text)| {
                        if is_cmd {
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(8.0))
                                .child(div().text_color(c::accent()).child(who.clone()))
                                .child(div().text_color(c::text()).child(text))
                        } else {
                            div().text_color(c::secondary()).child(text)
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .child(div().text_color(c::accent()).child(who.clone()))
                            .child(div().w(px(7.0)).h(px(14.0)).bg(c::text().opacity(0.8)))
                            .child(div().text_color(c::ghost()).child(format!("# {cmd}"))),
                    ),
            )
    }
}
