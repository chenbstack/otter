//! 节点页侧栏和它的四个视图:主机、钥匙串、已知主机、日志。
//! 「主机」视图在 `nodes.rs`;这里是侧栏外壳和其余三个视图。

use super::*;
use crate::nodes::NodesView;

impl Home {
    /// 节点页:左侧栏 + 右侧当前视图
    pub(crate) fn nodes_page(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let body = match self.nodes_view {
            NodesView::Hosts => self.hosts_view(window, cx),
            NodesView::Keychain => self.keychain_view(cx),
            NodesView::KnownHosts => self.known_hosts_view(cx),
            NodesView::Logs => self.logs_page(cx),
        };
        div().flex_1().min_h_0().flex().flex_row().child(self.nodes_side(cx)).child(body)
    }

    fn view_count(&self, v: NodesView) -> usize {
        match v {
            NodesView::Hosts => self.groups.iter().map(|g| g.nodes.len()).sum(),
            NodesView::Keychain => self.keys.len(),
            NodesView::KnownHosts => self.known_hosts.len() + self.grants.len(),
            NodesView::Logs => crate::logs::len(),
        }
    }

    pub(crate) fn set_nodes_view(&mut self, v: NodesView, cx: &mut Context<Self>) {
        self.nodes_view = v;
        if v == NodesView::KnownHosts {
            self.known_hosts = crate::known_hosts::load();
            self.reload_grants();
        }
        cx.notify();
    }

    fn nodes_side(&self, cx: &mut Context<Self>) -> Div {
        let mut col = div().w(px(NODES_SIDE_W)).flex_shrink_0().h_full().flex().flex_col().gap(px(6.0)).px(px(12.0)).pt(px(16.0)).pb(px(12.0)).bg(c::chrome()).border_r_1().border_color(c::border());
        for (i, v) in NodesView::ALL.into_iter().enumerate() {
            let active = self.nodes_view == v;
            let n = self.view_count(v);
            col = col.child(
                div()
                    .id(("nodes-side", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .h(px(34.0))
                    .px(px(12.0))
                    .rounded(px(t::RADIUS_MD))
                    .cursor_pointer()
                    .text_size(px(t::TYPE_BODY))
                    .when(active, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::SEMIBOLD))
                    .when(!active, |d| {
                        d.border_1()
                            .border_color(gpui_kit::transparent_black())
                            .text_color(c::muted())
                            .hover(|s| s.bg(c::hover()).text_color(c::secondary()))
                    })
                    .child(icon(v.icon(), 15.0, if active { c::accent() } else { c::ghost() }))
                    .child(div().flex_1().child(v.label()))
                    .when(n > 0, |d| {
                        d.child(
                            div()
                                .min_w(px(20.0))
                                .h(px(18.0))
                                .px(px(5.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(if active { c::accent_soft() } else { c::idle_soft() })
                                .text_size(px(t::TYPE_MICRO))
                                .font_family(MONO)
                                .text_color(if active { c::accent() } else { c::muted() })
                                .child(n.to_string()),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.set_nodes_view(v, cx))),
            );
        }
        col
    }

    /// 非主机视图的外壳:标题、副标题、右侧操作,下面是内容
    pub(crate) fn side_view(title: &'static str, sub: &'static str, action: Option<Stateful<Div>>, body: AnyElement) -> Div {
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_shrink_0()
                    .gap(px(10.0))
                    .px(px(CONTENT_PAD))
                    .pt(px(16.0))
                    .pb(px(12.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(div().text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(title))
                            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(sub)),
                    )
                    .children(action),
            )
            .child(body)
    }

    /// 玻璃质感的行容器(与节点卡片同一套底色和描边)
    pub(crate) fn glass_row(id: impl Into<ElementId>) -> Stateful<Div> {
        div().id(id).glass(t::RADIUS_LG, GlassStyle::CARD).flex().flex_row().items_center().gap(px(12.0)).px(px(14.0)).min_h(px(46.0))
    }

    pub(crate) fn rows_scroll(id: &'static str, rows: Vec<Stateful<Div>>) -> Stateful<Div> {
        div().id(id).flex_1().min_h_0().overflow_y_scroll().flex().flex_col().gap(px(6.0)).px(px(CONTENT_PAD)).pb(px(CONTENT_PAD)).children(rows)
    }

    // ---- 信任:已知主机 + Agent 授权 ----

    fn reload_grants(&mut self) {
        self.grants = self.store.as_ref().and_then(|s| s.list_grants().ok()).unwrap_or_default();
    }

    /// 撤销一条授权;这个 Agent 正在接管该节点的话,接管立刻结束
    fn revoke_grant(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(g) = self.grants.get(i).cloned() else { return };
        if let Some(s) = &self.store {
            if let Err(e) = s.grant_remove(&g.client_name, &g.parent_exe, g.node_id) {
                crate::logs::push(crate::logs::Level::Error, "Agent", format!("撤销授权失败: {e}"));
                return;
            }
        }
        let node = self.node_name(Some(g.node_id));
        crate::logs::push(crate::logs::Level::Info, "Agent", format!("已撤销 {} 对 {} 的接管授权", g.client_name, node));
        let live: Vec<u64> = self
            .workspace
            .tabs
            .iter()
            .filter(|t| t.node() == Some(g.node_id) && t.takeover.as_ref().is_some_and(|k| k.client_name == g.client_name))
            .map(|t| t.id)
            .collect();
        for id in live {
            self.end_takeover(id, Some("user_stopped"), window, cx);
        }
        self.reload_grants();
        cx.notify();
    }

    fn section_title(text: &'static str, n: usize) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(CONTENT_PAD))
            .pt(px(8.0))
            .pb(px(2.0))
            .child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child(text))
            .child(div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(n.to_string()))
    }

    fn section_empty(text: &'static str) -> Div {
        div().px(px(CONTENT_PAD)).py(px(10.0)).text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(text)
    }

    fn known_hosts_view(&self, cx: &mut Context<Self>) -> Div {
        let mut hosts = div().flex().flex_col().gap(px(6.0)).px(px(CONTENT_PAD));
        for (i, h) in self.known_hosts.iter().enumerate() {
            hosts = hosts.child(
                Self::glass_row(("kh", i))
                    .group(format!("kh-{i}"))
                    .child(icon(IconName::ShieldCheck, 16.0, c::online()))
                    .child(div().w(px(200.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(h.host.clone()))
                    .child(tag(h.algo.clone()))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::ghost()).child(h.fingerprint.clone()))
                    .child(
                        div()
                            .id(("kh-del", i))
                            .size(px(26.0))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(t::RADIUS_SM))
                            .opacity(0.0)
                            .group_hover(format!("kh-{i}"), |s| s.opacity(1.0))
                            .hover(|s| s.bg(c::hover()))
                            .child(icon(IconName::Trash, 14.0, c::danger()))
                            .tooltip(|_, cx| cx.new(|_| crate::files::Tip("删除记录".into())).into())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(h) = this.known_hosts.get(i) {
                                    if crate::known_hosts::remove(h) {
                                        crate::logs::push(crate::logs::Level::Info, &h.host, "已删除已知主机记录");
                                    }
                                }
                                this.known_hosts = crate::known_hosts::load();
                                cx.notify();
                            })),
                    ),
            );
        }
        let mut grants = div().flex().flex_col().gap(px(6.0)).px(px(CONTENT_PAD));
        for (i, g) in self.grants.iter().enumerate() {
            let when = chrono::DateTime::from_timestamp(g.created_at, 0)
                .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            grants = grants.child(
                Self::glass_row(("grant", i))
                    .child(icon(IconName::Bot, 16.0, c::accent()))
                    .child(
                        div()
                            .w(px(200.0))
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .child(div().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(g.client_name.clone()))
                            .child(div().truncate().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(self.node_name(Some(g.node_id)))),
                    )
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::ghost()).child(g.parent_exe.clone()))
                    .child(div().flex_shrink_0().text_size(px(t::TYPE_MICRO)).text_color(c::muted()).child(when))
                    .child(button("grant-revoke", IconName::Trash, "撤销").id(("grant-revoke", i)).on_click(cx.listener(move |this, _, window, cx| this.revoke_grant(i, window, cx)))),
            );
        }
        let body = div()
            .id("trust-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .pb(px(CONTENT_PAD))
            .child(Self::section_title("Agent 授权", self.grants.len()))
            .child(if self.grants.is_empty() { Self::section_empty("没有「总是允许」的授权,Agent 每次接管都会先询问你").into_any_element() } else { grants.into_any_element() })
            .child(Self::section_title("已知主机", self.known_hosts.len()))
            .child(if self.known_hosts.is_empty() { Self::section_empty("还没有已知主机,首次连接并确认指纹后会记录在这里").into_any_element() } else { hosts.into_any_element() });
        Self::side_view("信任", "已信任的服务器指纹,以及被你「总是允许」接管的 Agent。服务器重装后指纹会变,可删除旧记录再重新连接", None, body.into_any_element())
    }

    // ---- 日志 ----

    fn logs_page(&self, cx: &mut Context<Self>) -> Div {
        let entries = crate::logs::snapshot();
        let clear = button("logs-clear", IconName::Trash, "清空").on_click(cx.listener(|_, _, _, cx| {
            crate::logs::clear();
            cx.notify();
        }));
        let body = if entries.is_empty() {
            empty_state(IconName::ScrollText, "暂无日志", "连接、认证、主机密钥等事件会记录在这里,重启后清空").into_any_element()
        } else {
            let rows: Vec<_> = entries
                .into_iter()
                .enumerate()
                .map(|(i, e)| {
                    let (ic, col) = match e.level {
                        crate::logs::Level::Info => (IconName::CircleCheck, c::online()),
                        crate::logs::Level::Warn => (IconName::TriangleAlert, c::warn()),
                        crate::logs::Level::Error => (IconName::CircleX, c::danger()),
                    };
                    Self::glass_row(("log", i))
                        .min_h(px(36.0))
                        .child(icon(ic, 14.0, col))
                        .child(div().w(px(64.0)).flex_shrink_0().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::ghost()).child(e.time))
                        .child(div().w(px(200.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::muted()).child(e.host))
                        .child(div().flex_1().min_w_0().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(e.msg))
                })
                .collect();
            Self::rows_scroll("logs-scroll", rows).into_any_element()
        };
        Self::side_view("日志", "连接、认证和主机密钥事件,最新的在上面", Some(clear), body)
    }
}
