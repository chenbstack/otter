//! 工具箱页:卡片列表 → 端口检查、网络诊断、时间与时区、Swap 四个工具页。
//!
//! 逻辑(脚本、解析、判断)在 `crate::toolbox`,真机用例在 `src/orbtest/toolbox.rs`;设计见 `docs/modules/toolbox.md`。
//! 读取走 `Kind::Tool*`,要改服务器的操作都先弹确认框(`DockAsk` + `Action::Sys`)。

use std::time::{Duration, Instant};

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::InputState;

use super::super::*;
use super::daemonpage::field;
use super::dockerpages::{DockAsk, head, row, small, table_flow};
use super::syspages::{chip, message, reading};
use crate::docker::Action as DockAction;
use crate::sysops::{Job, Kind};
use crate::toolbox::{self, Finding, Level, ListenGroup, NetResult, NetTool, SwapDev, SwapInfo, SwapKind, TimeInfo};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    Ports,
    Net,
    Time,
    Swap,
}

impl Tool {
    const ALL: [Tool; 4] = [Tool::Ports, Tool::Net, Tool::Time, Tool::Swap];

    fn title(self) -> &'static str {
        match self {
            Tool::Ports => "端口检查",
            Tool::Net => "网络诊断",
            Tool::Time => "时间与时区",
            Tool::Swap => "Swap",
        }
    }

    fn blurb(self) -> &'static str {
        match self {
            Tool::Ports => "哪些端口在监听、是谁在用;查一个端口为什么连不上",
            Tool::Net => "ping、路由追踪、DNS 解析、TCP 连通性、出口公网 IP",
            Tool::Time => "服务器时间和本机的差、时区、自动对时",
            Tool::Swap => "查看、创建、调整、停用和删除 swap,设置 swappiness",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Tool::Ports => IconName::Plug,
            Tool::Net => IconName::Network,
            Tool::Time => IconName::Clock,
            Tool::Swap => IconName::MemoryStick,
        }
    }

    fn kind(self) -> Kind {
        match self {
            Tool::Ports => Kind::ToolPorts,
            Tool::Net => Kind::ToolNet,
            Tool::Time => Kind::ToolTime,
            Tool::Swap => Kind::ToolSwap,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum NetKind {
    #[default]
    Ping,
    Trace,
    Dns,
    Tcp,
    PublicIp,
}

impl NetKind {
    const ALL: [NetKind; 5] = [NetKind::Ping, NetKind::Trace, NetKind::Dns, NetKind::Tcp, NetKind::PublicIp];

    fn label(self) -> &'static str {
        match self {
            NetKind::Ping => "Ping",
            NetKind::Trace => "路由追踪",
            NetKind::Dns => "DNS 解析",
            NetKind::Tcp => "TCP 连通",
            NetKind::PublicIp => "出口公网 IP",
        }
    }
}

#[derive(Default)]
pub(crate) struct ToolUi {
    tool: Option<Tool>,
    // 端口检查
    ports: Option<Result<Vec<toolbox::Listen>, String>>,
    port_in: Option<Entity<InputState>>,
    diag: Option<(u16, String, Vec<Finding>)>,
    // 网络诊断
    net_kind: NetKind,
    net_host: Option<Entity<InputState>>,
    net_extra: Option<Entity<InputState>>,
    dns_ty: usize,
    net_out: Option<(String, NetResult)>,
    net_running: Option<String>,
    /// 本机网卡地址(出口公网 IP 页默认显示);None 是还没读
    nics: Option<Vec<(String, String)>>,
    // 时间与时区
    time: Option<Result<TimeInfo, String>>,
    /// 读到服务器时间的本机时刻;页面上的时钟按它往后推
    time_at: Option<Instant>,
    /// 上次画出来的时钟秒数(变了才重画)
    clock_sec: i64,
    tz_open: bool,
    ntp_open: bool,
    tz_in: Option<Entity<InputState>>,
    // Swap
    swap: Option<Result<SwapInfo, String>>,
    swap_path: Option<Entity<InputState>>,
    swap_size: Option<Entity<InputState>>,
    /// 正在调整大小的 swap 文件;None 是「新建」
    swap_resize: Option<String>,
    demo: bool,
}

const NICS_LABEL: &str = "本机网卡";

fn level_style(l: Level) -> (IconName, Rgba) {
    match l {
        Level::Good => (IconName::CircleCheck, c::online()),
        Level::Info => (IconName::Info, c::muted()),
        Level::Warn => (IconName::TriangleAlert, c::warn()),
        Level::Bad => (IconName::CircleX, c::danger()),
    }
}

fn card() -> Div {
    div().flex_shrink_0().flex().flex_col().gap(px(10.0)).p(px(16.0)).rounded(px(t::RADIUS_LG)).border_1().border_color(c::dialog_border())
}

fn section(title: &'static str) -> Div {
    div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child(title)
}

fn mono_text(s: impl Into<SharedString>) -> Div {
    div().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(s.into())
}

impl Home {
    fn tool_input(&self, which: u8) -> Option<&Entity<InputState>> {
        let t = &self.dock.sys.tool;
        match which {
            0 => t.port_in.as_ref(),
            1 => t.net_host.as_ref(),
            2 => t.net_extra.as_ref(),
            3 => t.tz_in.as_ref(),
            4 => t.swap_path.as_ref(),
            _ => t.swap_size.as_ref(),
        }
    }

    fn tool_text(&self, which: u8, cx: &App) -> String {
        self.tool_input(which).map(|e| e.read(cx).value().trim().to_string()).unwrap_or_default()
    }

    fn tool_set(&self, which: u8, v: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(e) = self.tool_input(which).cloned() {
            e.update(cx, |s, cx| s.set_value(v, window, cx));
        }
    }

    fn tool_make_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.sys.tool.port_in.is_some() {
            return;
        }
        let mk = |ph: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            let e = cx.new(|cx| InputState::new(window, cx).placeholder(ph));
            cx.observe(&e, |_, _, cx| cx.notify()).detach();
            e
        };
        let t = &mut self.dock.sys.tool;
        t.port_in = Some(mk("端口或进程名,如 8080、nginx", window, cx));
        t.net_host = Some(mk("域名或 IP,如 example.com", window, cx));
        t.net_extra = Some(mk("", window, cx));
        t.tz_in = Some(mk("搜索时区,如 Shanghai、UTC", window, cx));
        t.swap_path = Some(mk("/swapfile", window, cx));
        t.swap_size = Some(mk("大小(MB),如 1024", window, cx));
    }

    /// 每次渲染前:建输入框、收新数据、按需要发起读取
    pub(super) fn toolbox_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tool_make_inputs(window, cx);
        if self.demo {
            self.tool_demo();
            return;
        }
        let got: Vec<(Kind, Result<String, String>)> = match self.sys_docker() {
            Some(d) => [Kind::ToolPorts, Kind::ToolNet, Kind::ToolTime, Kind::ToolSwap, Kind::Firewall].into_iter().filter_map(|k| d.sys_take(k).map(|r| (k, r))).collect(),
            None => return,
        };
        for (k, r) in got {
            self.sys_store(k, r, window, cx);
        }
        let Some(tool) = self.dock.sys.tool.tool else { return };
        if tool == Tool::Net {
            let t = &self.dock.sys.tool;
            if t.net_kind == NetKind::PublicIp && t.nics.is_none() && t.net_running.is_none() && !self.dock.sys.inflight.contains(&Kind::ToolNet) {
                if let Ok(script) = toolbox::net_script(&NetTool::LocalIps, "") {
                    self.dock.sys.tool.net_running = Some(NICS_LABEL.to_string());
                    self.sys_ask(Kind::ToolNet, &script, false);
                }
            }
            return;
        }
        let kind = tool.kind();
        let s = &self.dock.sys;
        let due = match s.asked.get(&kind) {
            None => true,
            Some(t) => window.is_window_active() && t.elapsed() >= Duration::from_secs(crate::prefs::page_secs()) && s.tool.swap_resize.is_none(),
        };
        if due && !s.inflight.contains(&kind) {
            let (script, root) = match tool {
                Tool::Ports => (toolbox::PORTS_SCRIPT, true),
                Tool::Time => (toolbox::TIME_SCRIPT, false),
                _ => (toolbox::SWAP_SCRIPT, true),
            };
            self.sys_ask(kind, script, root);
            // 端口诊断要看防火墙:进页读一次
            if tool == Tool::Ports && self.dock.sys.fw.is_none() && !self.dock.sys.inflight.contains(&Kind::Firewall) {
                self.sys_ask(Kind::Firewall, crate::firewall::READ_SCRIPT, true);
            }
        }
    }

    pub(super) fn tool_loaded(&mut self, kind: Kind, res: Result<String, String>) {
        let t = &mut self.dock.sys.tool;
        match kind {
            Kind::ToolPorts => {
                let r = res.map(|s| toolbox::parse_listens(&s));
                if r.is_ok() || !matches!(t.ports, Some(Ok(_))) {
                    t.ports = Some(r);
                }
            }
            Kind::ToolNet => {
                let label = t.net_running.take().unwrap_or_default();
                if label == NICS_LABEL {
                    t.nics = Some(res.map(|s| toolbox::parse_nics(&s)).unwrap_or_default());
                    return;
                }
                t.net_out = Some((label, match res {
                    Ok(s) => toolbox::parse_net(&s),
                    Err(e) => NetResult { text: e, code: -1, missing: None },
                }));
            }
            Kind::ToolTime => {
                t.time_at = Some(Instant::now());
                let r = res.and_then(|s| toolbox::parse_time(&s));
                if r.is_ok() || !matches!(t.time, Some(Ok(_))) {
                    t.time = Some(r);
                }
            }
            Kind::ToolSwap => {
                let r = res.and_then(|s| toolbox::parse_swap(&s));
                if r.is_ok() || !matches!(t.swap, Some(Ok(_))) {
                    t.swap = Some(r);
                }
            }
            _ => {}
        }
    }

    fn tool_open(&mut self, tool: Option<Tool>, cx: &mut Context<Self>) {
        let t = &mut self.dock.sys.tool;
        t.tool = tool;
        t.diag = None;
        t.swap_resize = None;
        if let Some(tl) = tool {
            self.dock.sys.asked.remove(&tl.kind());
        }
        cx.notify();
    }

    fn tool_notice(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.dock.notice = Some((msg.into(), Instant::now()));
        cx.notify();
    }

    /// 要在服务器上改东西:先弹确认框
    #[allow(clippy::too_many_arguments)]
    fn tool_ask(&mut self, tool: Tool, title: &'static str, body: String, verb: &'static str, name: String, script: Result<String, String>, calm: bool, secs: u64, list: Vec<String>, cx: &mut Context<Self>) {
        let script = match script {
            Ok(s) => s,
            Err(e) => return self.tool_notice(e, cx),
        };
        let (reload, root) = match tool {
            Tool::Time => ((Kind::ToolTime, toolbox::TIME_SCRIPT.to_string(), false), true),
            _ => ((Kind::ToolSwap, toolbox::SWAP_SCRIPT.to_string(), true), true),
        };
        let job = Job { label: verb.into(), target: name.clone(), script, root, input: None, reload: Some(reload), guard: None, secs, calm };
        // DockAsk.body 要 &'static str:正文放进清单第一行,body 只留固定的一句
        let mut l = vec![body];
        l.extend(list);
        self.ask(DockAsk { title, tile: tool.icon(), name, detail: tool.title(), body: "", label: verb, action: DockAction::Sys(job), list: l }, cx);
    }

    // ================= 页面 =================

    pub(crate) fn toolbox_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(tool) = self.dock.sys.tool.tool else { return self.tool_cards(cx) };
        let back = chip(("tool-back", 0), "← 工具箱", false).on_click(cx.listener(|this, _, _, cx| this.tool_open(None, cx)));
        let head_row = div().flex_shrink_0().flex().flex_row().items_center().gap(px(10.0)).child(back).child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child(tool.title()));
        let body = match tool {
            Tool::Ports => self.ports_tool(cx),
            Tool::Net => self.net_tool(cx),
            Tool::Time => self.time_tool(cx),
            Tool::Swap => self.swap_tool(cx),
        };
        div().flex_grow(1.0).flex_shrink_0().pb(px(14.0)).flex().flex_col().gap(px(12.0)).child(head_row).child(body).into_any_element()
    }

    fn tool_cards(&self, cx: &mut Context<Self>) -> AnyElement {
        // 按窗口宽度算列数,每行等分,最后一行用空块补齐(见 guidelines 卡片网格)
        let cols = (((self.dock.view_w - 56.0) / 300.0).floor() as usize).clamp(1, 4);
        let cards: Vec<Stateful<Div>> = Tool::ALL
            .into_iter()
            .enumerate()
            .map(|(i, tl)| {
                div()
                    .id(("tool-card", i))
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(14.0))
                    .p(px(16.0))
                    .rounded(px(t::RADIUS_LG))
                    .border_1()
                    .border_color(c::dialog_border())
                    .cursor_pointer()
                    .hover(|s| s.bg(c::field_hover()))
                    .child(div().size(px(36.0)).flex_shrink_0().flex().items_center().justify_center().rounded(px(t::RADIUS_MD)).bg(c::accent_soft()).child(icon(tl.icon(), 18.0, c::accent())))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child(tl.title()))
                            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(tl.blurb())),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.tool_open(Some(tl), cx)))
            })
            .collect();
        let mut rows = Vec::new();
        let mut it = cards.into_iter();
        loop {
            let mut row = div().flex_shrink_0().flex().flex_row().gap(px(12.0));
            let mut n = 0;
            for c in it.by_ref().take(cols) {
                row = row.child(c);
                n += 1;
            }
            if n == 0 {
                break;
            }
            for _ in n..cols {
                row = row.child(div().flex_1().min_w_0().p(px(16.0)).border_1());
            }
            rows.push(row);
        }
        div().flex_grow(1.0).flex_shrink_0().pb(px(14.0)).flex().flex_col().gap(px(12.0)).children(rows).into_any_element()
    }

    fn tool_input_box(&self, which: u8) -> Div {
        let e = self.tool_input(which).cloned();
        div()
            .h(px(32.0))
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .rounded(px(t::RADIUS_MD))
            .bg(c::field())
            .border_1()
            .border_color(c::field_border())
            .text_size(px(t::TYPE_CONTROL))
            .children(e.map(|e| div().flex_1().min_w_0().child(gpui_kit::component::input::Input::new(&e).appearance(false).with_size(px(12.0 / 0.875)))))
    }

    fn findings(list: &[Finding]) -> Div {
        let mut col = div().flex().flex_col().gap(px(8.0));
        for (i, f) in list.iter().enumerate() {
            let (ic, color) = level_style(f.level);
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(8.0))
                    .child(div().pt(px(2.0)).flex_shrink_0().child(icon(ic, 14.0, color)))
                    .child(div().flex_1().min_w_0().text_size(px(if i == 0 { t::TYPE_BODY } else { t::TYPE_CONTROL })).font_weight(if i == 0 { FontWeight::SEMIBOLD } else { FontWeight::NORMAL }).text_color(if i == 0 { c::text() } else { c::secondary() }).child(SharedString::from(f.text.clone()))),
            );
        }
        col
    }

    // ---- 端口检查 ----

    fn tool_diag(&mut self, port: u16, cx: &mut Context<Self>) {
        let Some(Ok(listens)) = self.dock.sys.tool.ports.clone() else { return };
        let proto = if listens.iter().any(|l| l.port == port && l.proto == "tcp") || !listens.iter().any(|l| l.port == port) { "tcp" } else { "udp" };
        let fw = self.dock.sys.fw.as_ref().and_then(|r| r.as_ref().ok());
        let d = toolbox::diagnose(port, proto, &listens, &self.dock.snap.containers, fw);
        self.dock.sys.tool.diag = Some((port, proto.to_string(), d));
        cx.notify();
    }

    fn ports_tool(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let listens = match &self.dock.sys.tool.ports {
            None => return reading("读取服务器上的监听端口"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了监听端口", e.clone()),
            Some(Ok(l)) => l.clone(),
        };
        let q = self.tool_text(0, cx);
        let q_port = q.parse::<u16>().ok().filter(|p| *p > 0);
        let mut col = div().flex().flex_col().gap(px(12.0));
        let go = q_port.is_some();
        let input_row = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(self.tool_input_box(0))
            .child(button("tool-diag", IconName::Search, "检查这个端口").when(!go, |d| d.opacity(0.45)).on_click(cx.listener(|this, _, _, cx| {
                if let Some(p) = this.tool_text(0, cx).parse::<u16>().ok().filter(|p| *p > 0) {
                    this.tool_diag(p, cx);
                }
            })));
        col = col.child(input_row);
        if let Some((port, proto, list)) = &self.dock.sys.tool.diag {
            col = col.child(card().child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(format!("端口 {port}/{proto} 的检查结果")))).child(Self::findings(list)));
        }
        let groups: Vec<ListenGroup> = toolbox::group_listens(&listens);
        let ct_of = |g: &ListenGroup| -> String {
            self.dock.snap.containers.iter().filter(|c| c.running() && c.publish.iter().any(|p| (p.proto.is_empty() || p.proto == g.proto) && crate::firewall::parse_port(&p.host).is_some_and(|(a, b)| (a..=b).contains(&g.port)))).map(|c| c.name.clone()).collect::<Vec<_>>().join("、")
        };
        let shown: Vec<&ListenGroup> = groups.iter().filter(|g| q.is_empty() || g.port.to_string().contains(&q) || g.procs.iter().any(|(n, _)| n.to_lowercase().contains(&q.to_lowercase())) || ct_of(g).to_lowercase().contains(&q.to_lowercase())).collect();
        col = col.child(div().flex().flex_row().items_baseline().gap(px(8.0)).child(section("监听中的端口")).child(small(format!("{}", shown.len()), c::ghost())).child(small("点一行检查它", c::ghost())));
        if shown.is_empty() {
            col = col.child(div().flex_shrink_0().py(px(28.0)).flex().justify_center().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child("没有匹配的端口"));
        } else {
            let header = div().child(head("端口").w(px(90.0))).child(head("协议").w(px(60.0))).child(head("监听地址").w(px(190.0))).child(head("进程").flex_1().min_w_0()).child(head("容器").w(px(170.0)));
            let rows: Vec<Stateful<Div>> = shown
                .iter()
                .enumerate()
                .map(|(i, g)| {
                    let port = g.port;
                    let local = g.local_only();
                    let procs = if g.procs.is_empty() { "—".to_string() } else { g.procs.iter().map(|(n, p)| format!("{n}({p})")).collect::<Vec<_>>().join("、") };
                    let ct = ct_of(g);
                    row(("tool-port", i))
                        .cursor_pointer()
                        .child(div().w(px(90.0)).flex_shrink_0().text_size(px(t::TYPE_CONTROL)).font_family(MONO).child(SharedString::from(g.port.to_string())))
                        .child(div().w(px(60.0)).flex_shrink_0().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(g.proto.to_uppercase())))
                        .child(div().w(px(190.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(if local { c::warn() } else { c::muted() }).child(SharedString::from(if local { format!("{}(仅本机)", g.addr_text()) } else { g.addr_text() })))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(procs)))
                        .child(div().w(px(170.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(if ct.is_empty() { "—".to_string() } else { ct })))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.tool_set(0, &port.to_string(), window, cx);
                            this.tool_diag(port, cx);
                        }))
                })
                .collect();
            col = col.child(table_flow(header, rows));
        }
        col.into_any_element()
    }

    // ---- 网络诊断 ----

    fn net_run(&mut self, cx: &mut Context<Self>) {
        let host = self.tool_text(1, cx);
        let extra = self.tool_text(2, cx);
        let kind = self.dock.sys.tool.net_kind;
        let tool = match kind {
            NetKind::Ping => NetTool::Ping,
            NetKind::Trace => NetTool::Trace,
            NetKind::Dns => NetTool::Dns(toolbox::DNS_TYPES[self.dock.sys.tool.dns_ty.min(toolbox::DNS_TYPES.len() - 1)].to_string(), extra),
            NetKind::Tcp => match extra.parse::<u16>() {
                Ok(p) if p > 0 => NetTool::Tcp(p),
                _ => return self.tool_notice("端口要填 1-65535 之间的数字", cx),
            },
            NetKind::PublicIp => NetTool::PublicIp,
        };
        if kind != NetKind::PublicIp && host.is_empty() {
            return self.tool_notice("先填要检查的域名或 IP", cx);
        }
        match toolbox::net_script(&tool, &host) {
            Err(e) => self.tool_notice(e, cx),
            Ok(script) => {
                let label = if kind == NetKind::PublicIp { kind.label().to_string() } else { format!("{} {host}", kind.label()) };
                self.dock.sys.tool.net_running = Some(label);
                self.dock.sys.tool.net_out = None;
                self.sys_ask(Kind::ToolNet, &script, false);
                cx.notify();
            }
        }
    }

    fn net_tool(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let kind = self.dock.sys.tool.net_kind;
        let running = self.dock.sys.tool.net_running.as_deref().is_some_and(|l| l != NICS_LABEL);
        let mut kinds = div().flex().flex_row().gap(px(6.0));
        for (i, k) in NetKind::ALL.into_iter().enumerate() {
            kinds = kinds.child(chip(("net-kind", i), k.label(), kind == k).on_click(cx.listener(move |this, _, window, cx| {
                this.dock.sys.tool.net_kind = k;
                this.dock.sys.tool.net_out = None;
                this.tool_set(2, "", window, cx);
                cx.notify();
            })));
        }
        // 输入区:所有输入框同一行、底部对齐,说明文字放到整行下面,不挤在输入框旁边
        let mut inputs = div().flex_shrink_0().flex().flex_row().items_end().gap(px(12.0));
        if kind != NetKind::PublicIp {
            inputs = inputs.child(div().flex_1().min_w_0().child(field("主机", "", self.tool_input(1).unwrap())));
        }
        match kind {
            NetKind::Tcp => inputs = inputs.child(div().w(px(160.0)).child(field("端口", "", self.tool_input(2).unwrap()))),
            NetKind::Dns => inputs = inputs.child(div().w(px(260.0)).child(field("解析服务器(可选)", "", self.tool_input(2).unwrap()))),
            _ => {}
        }
        let go = button("net-go", IconName::Play, if running { "执行中…" } else if kind == NetKind::PublicIp { "检测" } else { "开始" }).when(running, |d| d.opacity(0.6)).on_click(cx.listener(|this, _, _, cx| {
            if this.dock.sys.tool.net_running.is_none() {
                this.net_run(cx);
            }
        }));
        let mut panel = if kind == NetKind::PublicIp {
            // 默认显示本机网卡上的地址(不联网);点「检测」才去问第三方
            let mut nics = div().flex().flex_col().gap(px(6.0)).child(small("本机网卡地址", c::ghost()));
            match &self.dock.sys.tool.nics {
                None => nics = nics.child(small("读取中…", c::ghost())),
                Some(l) if l.is_empty() => nics = nics.child(small("没有读到网卡地址", c::ghost())),
                Some(l) => {
                    for (n, a) in l {
                        nics = nics.child(div().flex().flex_row().items_center().gap(px(12.0)).child(div().w(px(110.0)).flex_shrink_0().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(n.clone()))).child(mono_text(SharedString::from(a.clone()))));
                    }
                }
            }
            card().child(div().flex().flex_row().items_end().gap(px(12.0)).child(div().flex_1().min_w_0().child(nics)).child(go))
        } else {
            card().child(inputs.child(go))
        };
        if kind == NetKind::Dns {
            let sel = self.dock.sys.tool.dns_ty;
            panel = panel.child(div().flex().flex_row().items_center().gap(px(6.0)).child(small("记录类型", c::ghost())).children(toolbox::DNS_TYPES.iter().enumerate().map(|(i, ty)| chip(("dns-ty", i), *ty, sel == i).on_click(cx.listener(move |this, _, _, cx| {
                this.dock.sys.tool.dns_ty = i;
                cx.notify();
            })))));
        }
        let note = match kind {
            NetKind::Ping => "从服务器 ping 目标 4 次",
            NetKind::Trace => "从服务器追踪到目标的路由;服务器上要有 traceroute 或 tracepath",
            NetKind::Dns => "解析服务器留空时用服务器自己的 DNS 设置",
            NetKind::Tcp => "从服务器尝试连接目标的 TCP 端口,5 秒超时",
            NetKind::PublicIp => "上面是服务器网卡上的地址,可能是内网地址;点「检测」会让服务器去访问第三方查询服务(api.ipify.org 等)查出口公网 IP,不点不会请求",
        };
        panel = panel.child(small(note, c::ghost()));
        let mut col = div().flex().flex_col().gap(px(12.0)).child(div().flex_shrink_0().flex().flex_row().items_center().gap(px(8.0)).child(kinds)).child(panel);
        if let Some(label) = self.dock.sys.tool.net_running.as_ref().filter(|l| l.as_str() != NICS_LABEL) {
            col = col.child(crate::ui::loading(format!("正在执行:{label}…")).h(px(80.0)));
        } else if let (NetKind::PublicIp, Some((_, r))) = (kind, &self.dock.sys.tool.net_out) {
            // 只显示成功的结果;查询失败的细节(curl 报错)不展示
            col = col.child(if r.code == 0 && !r.text.is_empty() {
                card().child(small("出口公网 IP", c::ghost())).child(div().text_size(px(t::TYPE_BODY)).font_family(MONO).font_weight(FontWeight::SEMIBOLD).text_color(c::online()).child(SharedString::from(r.text.clone())))
            } else {
                let msg = match &r.missing {
                    Some(m) => format!("服务器上没有 {m},这个检查没法做(Otter 不代装)"),
                    None => "没有查到出口公网 IP,服务器可能不能访问外网".to_string(),
                };
                card().child(small(msg, c::warn()))
            });
        } else if let Some((label, r)) = &self.dock.sys.tool.net_out {
            let (txt, color) = match (&r.missing, r.code) {
                (Some(m), _) => (format!("服务器上没有 {m},这个检查没法做(Otter 不代装)"), c::warn()),
                (None, 0) => ("成功".to_string(), c::online()),
                (None, code) => (format!("没有成功(退出码 {code})"), c::danger()),
            };
            let mut out = div().flex().flex_col().p(px(12.0)).rounded(px(t::RADIUS_MD)).bg(c::field()).border_1().border_color(c::field_border());
            for l in r.text.lines().take(400) {
                out = out.child(mono_text(SharedString::from(if l.is_empty() { " ".to_string() } else { l.to_string() })));
            }
            col = col.child(div().flex().flex_row().items_baseline().gap(px(8.0)).child(section("结果")).child(small(label.clone(), c::ghost())).child(small(txt, color)));
            if !r.text.is_empty() {
                col = col.child(out);
            }
        }
        col.into_any_element()
    }

    // ---- 时间与时区 ----

    /// 页面上的时钟每秒走一格:`dock_tick` 问是不是该重画
    pub(super) fn tool_clock_due(&mut self) -> bool {
        let t = &mut self.dock.sys.tool;
        if self.page != Page::Toolbox || t.tool != Some(Tool::Time) {
            return false;
        }
        let Some(at) = t.time_at else { return false };
        let sec = at.elapsed().as_secs() as i64;
        let due = sec != t.clock_sec;
        t.clock_sec = sec;
        due
    }

    fn time_tool(&mut self, cx: &mut Context<Self>) -> AnyElement {
        use crate::ui::{MenuItem, select_search, select_sized};
        let t = match &self.dock.sys.tool.time {
            None => return reading("读取服务器时间"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了服务器时间", e.clone()),
            Some(Ok(t)) => t.clone(),
        };
        let ui = &self.dock.sys.tool;
        let shown = toolbox::shift_date(&t.date, ui.time_at.map(|a| a.elapsed().as_secs() as i64).unwrap_or(0));
        let (tz_open, ntp_open) = (ui.tz_open, ui.ntp_open);
        let row = |k: &'static str, v: AnyElement| div().flex_shrink_0().flex().flex_row().items_center().gap(px(16.0)).min_h(px(34.0)).child(div().w(px(110.0)).flex_shrink_0().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(k)).child(v).into_any_element();
        let text = |s: String, color: Rgba| div().text_size(px(t::TYPE_CONTROL)).text_color(color).child(SharedString::from(s)).into_any_element();

        // 时区:带搜索的选择
        let tz_el: AnyElement = if t.has_timedatectl && !t.zones.is_empty() {
            let items: Vec<MenuItem> = t.zones.iter().map(|z| MenuItem::new(z.clone())).collect();
            let sel = t.zones.iter().position(|z| *z == t.tz).unwrap_or(0);
            let search = self.tool_input(3).cloned().map(|i| {
                let q = i.read(cx).value().to_string();
                (i, q)
            });
            let (e1, e2, e3) = (cx.entity(), cx.entity(), cx.entity());
            let (zones, cur) = (t.zones.clone(), t.tz.clone());
            let toggle = move |window: &mut Window, cx: &mut App| {
                e1.update(cx, |h, cx| {
                    let u = &mut h.dock.sys.tool;
                    u.tz_open = !u.tz_open;
                    u.ntp_open = false;
                    if u.tz_open {
                        if let Some(s) = u.tz_in.clone() {
                            s.update(cx, |s, cx| s.set_value("", window, cx));
                            let f = s.focus_handle(cx);
                            window.focus(&f, cx);
                        }
                    }
                    cx.notify();
                })
            };
            let pick = move |i: usize, _: &mut Window, cx: &mut App| {
                e2.update(cx, |h, cx| {
                    h.dock.sys.tool.tz_open = false;
                    if let Some(z) = zones.get(i).filter(|z| **z != cur).cloned() {
                        h.tool_ask(Tool::Time, "修改时区", format!("服务器时区改成 {z};已有的计划任务(cron)按新时区的时间触发"), "修改时区", z.clone(), toolbox::set_tz_script(&z), true, 0, vec!["可以随时改回原来的时区".to_string()], cx);
                    }
                    cx.notify();
                })
            };
            let dismiss = move |_: &mut Window, cx: &mut App| {
                e3.update(cx, |h, cx| {
                    h.dock.sys.tool.tz_open = false;
                    cx.notify();
                })
            };
            let el = match search {
                Some(sq) => select_search(34.0, "tool-tz", IconName::Clock, items, sel, tz_open, sq, toggle, pick, dismiss, None),
                None => select_sized(34.0, "tool-tz", IconName::Clock, items, sel, tz_open, toggle, pick, dismiss, None),
            };
            div().w(px(340.0)).h(px(34.0)).child(el).into_any_element()
        } else {
            text(if t.tz.is_empty() { "—".into() } else { t.tz.clone() }, c::muted())
        };

        // NTP 开关:机器上有时间服务才能开关
        let on = t.ntp == Some(true);
        let can = t.has_timedatectl && !t.services.is_empty();
        let mut sw = div().id("tool-ntp-sw").flex_shrink_0().child(super::daemonpage::switch("tool-ntp", on));
        if can {
            sw = sw.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                let (body, verb) = if on { ("关闭后服务器时间不再自动校准,时间会慢慢跑偏", "关闭 NTP") } else { ("开启后服务器会通过时间服务自动校准时间", "开启 NTP") };
                this.tool_ask(Tool::Time, "NTP 对时", body.to_string(), verb, "NTP".into(), Ok(toolbox::set_ntp_script(!on)), true, 0, Vec::new(), cx);
            }));
        } else {
            sw = sw.opacity(0.45);
        }
        let svc = if t.services.is_empty() { "服务器上没有时间同步服务(chrony、systemd-timesyncd)".to_string() } else { t.services.iter().map(|(n, up)| format!("{n}{}", if *up { "" } else { "(未运行)" })).collect::<Vec<_>>().join("、") };
        let (st, st_color) = match (t.ntp, t.synced) {
            (Some(true), Some(true)) => ("已同步", c::online()),
            (Some(true), _) => ("还没同步上", c::warn()),
            (Some(false), _) => ("未开启", c::ghost()),
            _ => ("读不到状态", c::ghost()),
        };
        let ntp_el = div().flex().flex_row().items_center().gap(px(12.0)).child(sw).child(small(st, st_color)).child(small(svc, c::ghost())).into_any_element();

        // NTP 服务器:选择
        let srv_el: AnyElement = if t.ntp_backend.is_empty() {
            text("没有可改的时间服务".into(), c::ghost())
        } else {
            let cur = t.ntp_servers.join("、");
            let mut items = vec![MenuItem::new("系统默认")];
            if !t.ntp_by_otter && !cur.is_empty() {
                items[0] = MenuItem::new("系统默认").sub(cur.clone());
            }
            items.extend(toolbox::NTP_PRESETS.iter().map(|(h, n)| MenuItem::new(*h).sub(*n)));
            let first = t.ntp_servers.first().cloned().unwrap_or_default();
            let sel = if !t.ntp_by_otter {
                0
            } else if let Some(i) = toolbox::NTP_PRESETS.iter().position(|(h, _)| *h == first) {
                i + 1
            } else {
                items.push(MenuItem::new(first.clone()).sub("自定义"));
                items.len() - 1
            };
            let (e1, e2, e3) = (cx.entity(), cx.entity(), cx.entity());
            let backend = t.ntp_backend.clone();
            let toggle = move |_: &mut Window, cx: &mut App| {
                e1.update(cx, |h, cx| {
                    let u = &mut h.dock.sys.tool;
                    u.ntp_open = !u.ntp_open;
                    u.tz_open = false;
                    cx.notify();
                })
            };
            let pick = move |i: usize, _: &mut Window, cx: &mut App| {
                e2.update(cx, |h, cx| {
                    h.dock.sys.tool.ntp_open = false;
                    if i != sel && i <= toolbox::NTP_PRESETS.len() {
                        let srv = (i > 0).then(|| toolbox::NTP_PRESETS[i - 1].0);
                        let name = srv.unwrap_or("系统默认").to_string();
                        let body = match srv {
                            Some(s) => format!("NTP 服务器改为 {s},并重启 {backend}"),
                            None => format!("NTP 服务器改回系统默认,并重启 {backend}"),
                        };
                        h.tool_ask(Tool::Time, "NTP 服务器", body, "修改 NTP 服务器", name, toolbox::set_ntp_server_script(&backend, srv), true, 0, vec!["原来的服务器配置会保留,改回「系统默认」即可恢复".to_string()], cx);
                    }
                    cx.notify();
                })
            };
            let dismiss = move |_: &mut Window, cx: &mut App| {
                e3.update(cx, |h, cx| {
                    h.dock.sys.tool.ntp_open = false;
                    cx.notify();
                })
            };
            div().w(px(340.0)).h(px(34.0)).child(select_sized(34.0, "tool-ntp-srv", IconName::Network, items, sel, ntp_open, toggle, pick, dismiss, None)).into_any_element()
        };

        card()
            .child(row("服务器时间", div().text_size(px(t::TYPE_BODY)).font_family(MONO).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(shown)).into_any_element()))
            .child(row("时区", tz_el))
            .child(row("NTP 对时", ntp_el))
            .child(row("NTP 服务器", srv_el))
            .into_any_element()
    }

    // ---- Swap ----

    fn swap_form_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dock.sys.tool.swap_resize = None;
        self.tool_set(4, "", window, cx);
        self.tool_set(5, "", window, cx);
        cx.notify();
    }

    fn swap_submit(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(info)) = self.dock.sys.tool.swap.clone() else { return };
        let size = self.tool_text(5, cx);
        let Ok(mb) = size.parse::<u64>() else { return self.tool_notice("大小要填数字(单位 MB)", cx) };
        if let Some(path) = self.dock.sys.tool.swap_resize.clone() {
            let cur = info.devs.iter().find(|d| d.path == path).map(|d| d.size_kb / 1024).unwrap_or(0);
            let body = format!("把 {path} 从 {} 调整为 {} MB,调整期间会短暂停用", cur, mb);
            let list = vec![format!("先在旁边建好新文件,再换上去;磁盘需要同时放得下新旧两个文件(含 1 GB 余量)")];
            self.tool_ask(Tool::Swap, "调整 Swap 大小", body, "调整大小", path.clone(), toolbox::swap_resize_script(&path, mb), true, 900, list, cx);
        } else {
            let path = { let p = self.tool_text(4, cx); if p.is_empty() { "/swapfile".to_string() } else { p } };
            if info.devs.iter().any(|d| d.path == path) {
                return self.tool_notice(format!("{path} 已经是 swap 了,要改大小请点那一行的「调整」"), cx);
            }
            let body = format!("在 {path} 创建 {mb} MB 的 swap 文件并启用,写进 /etc/fstab,重启后自动启用");
            let list = vec![format!("磁盘剩余 {},会留出至少 1 GB 余量", toolbox::fmt_kb(info.root_avail_kb)), "文件权限 600;/etc/fstab 先备份为 fstab.otter-bak".to_string()];
            self.tool_ask(Tool::Swap, "创建 Swap 文件", body, "创建 Swap", path.clone(), toolbox::swap_create_script(&path, mb), true, 900, list, cx);
        }
    }

    fn swap_dev_row(&self, i: usize, d: &SwapDev, cx: &mut Context<Self>) -> Stateful<Div> {
        let (kind_txt, kind_color) = match d.kind {
            SwapKind::File => ("文件", c::accent()),
            SwapKind::Partition => ("分区", c::muted()),
            SwapKind::Zram => ("zram", c::muted()),
        };
        let frac = if d.size_kb == 0 { 0.0 } else { d.used_kb as f32 / d.size_kb as f32 };
        let pct = (frac * 100.0).round() as u8;
        let color = if pct >= 90 { c::danger() } else if pct >= 70 { c::warn() } else { c::online() };
        let mut acts = div().w(px(210.0)).flex_shrink_0().flex().flex_row().justify_end().items_center().gap(px(2.0));
        let path = d.path.clone();
        if d.kind == SwapKind::File {
            let (p, mb) = (path.clone(), d.size_kb / 1024);
            acts = acts.child(chip(("sw-resize", i), "调整", false).on_click(cx.listener(move |this, _, window, cx| {
                this.dock.sys.tool.swap_resize = Some(p.clone());
                this.tool_set(4, &p, window, cx);
                this.tool_set(5, &mb.to_string(), window, cx);
                cx.notify();
            })));
        }
        let p = path.clone();
        let (used, persist) = (d.used_kb, d.in_fstab);
        acts = acts.child(chip(("sw-off", i), "停用", false).on_click(cx.listener(move |this, _, _, cx| {
            let mut list = vec![if persist { "/etc/fstab 里对应的行会注释掉,以后可以再启用".to_string() } else { "它没有记在 /etc/fstab 里,停用后要重新创建或手动 swapon".to_string() }];
            if used > 0 {
                list.push(format!("里面已用 {},会先换回物理内存;内存不够时会拒绝", toolbox::fmt_kb(used)));
            }
            this.tool_ask(Tool::Swap, "停用 Swap", format!("{p} 不再作为 swap 使用"), "停用 Swap", p.clone(), toolbox::swap_off_script(&p), true, 300, list, cx);
        })));
        if d.kind == SwapKind::File {
            let p = path.clone();
            acts = acts.child(chip(("sw-del", i), "删除", false).on_click(cx.listener(move |this, _, _, cx| {
                this.tool_ask(Tool::Swap, "删除 Swap 文件", format!("停用并删除 {p},同时去掉 /etc/fstab 里对应的行"), "删除 Swap", p.clone(), toolbox::swap_delete_script(&p), false, 300, vec!["已用的部分会先换回物理内存;内存不够时会拒绝".to_string()], cx);
            })));
        }
        row(("sw-r", i))
            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).child(SharedString::from(d.path.clone())))
            .child(div().w(px(80.0)).flex_shrink_0().child(crate::ui::tag(kind_txt).color(kind_color)))
            .child(div().w(px(100.0)).flex_shrink_0().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(toolbox::fmt_kb(d.size_kb))))
            .child(div().w(px(150.0)).flex_shrink_0().flex().flex_row().items_center().gap(px(8.0)).child(div().w(px(60.0)).h(px(6.0)).rounded_full().bg(c::idle_soft()).overflow_hidden().child(div().h_full().w(px((60.0 * frac.clamp(0.0, 1.0)).max(if frac > 0.0 { 2.0 } else { 0.0 }))).rounded_full().bg(color))).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(SharedString::from(toolbox::fmt_kb(d.used_kb)))))
            .child(div().w(px(90.0)).flex_shrink_0().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(if d.in_fstab { "重启后保留" } else { "仅当前" }))
            .child(acts)
    }

    fn swap_tool(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let info = match &self.dock.sys.tool.swap {
            None => return reading("读取服务器上的 swap"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了 swap 信息", e.clone()),
            Some(Ok(i)) => i.clone(),
        };
        let used = info.swap_total_kb.saturating_sub(info.swap_free_kb);
        let bar = |frac: f32| {
            let color = if frac >= 0.9 { c::danger() } else if frac >= 0.7 { c::warn() } else { c::online() };
            div().h(px(6.0)).rounded_full().bg(c::idle_soft()).overflow_hidden().child(div().h_full().w(relative(frac.clamp(0.0, 1.0))).rounded_full().bg(color))
        };
        let stat = |title: &'static str, big: String, sub: String, frac: Option<f32>| {
            let mut d = card().flex_1().min_w_0().gap(px(6.0)).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(title)).child(div().text_size(px(t::TYPE_DISPLAY)).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(big)));
            if let Some(f) = frac {
                d = d.child(bar(f));
            }
            d.child(small(sub, c::ghost()))
        };
        let mem_used = info.mem_total_kb.saturating_sub(info.mem_avail_kb);
        let mem_frac = if info.mem_total_kb == 0 { 0.0 } else { mem_used as f32 / info.mem_total_kb as f32 };
        let swap_frac = if info.swap_total_kb == 0 { 0.0 } else { used as f32 / info.swap_total_kb as f32 };
        let stats = div().flex_shrink_0().flex().flex_row().gap(px(12.0))
            .child(stat("物理内存", toolbox::fmt_kb(info.mem_total_kb), format!("已用 {} · 可用 {}", toolbox::fmt_kb(mem_used), toolbox::fmt_kb(info.mem_avail_kb)), Some(mem_frac)))
            .child(if info.swap_total_kb == 0 { stat("Swap", "未启用".into(), "没有任何 swap".into(), None) } else { stat("Swap", toolbox::fmt_kb(info.swap_total_kb), format!("已用 {} · 剩余 {}", toolbox::fmt_kb(used), toolbox::fmt_kb(info.swap_free_kb)), Some(swap_frac)) })
            .child(stat("磁盘剩余(/)", toolbox::fmt_kb(info.root_avail_kb), format!("文件系统 {}", info.root_fs), None));
        let mut col = div().flex().flex_col().gap(px(12.0)).child(stats);

        // swappiness:一张卡,左边说明、右边选值
        let cur = info.swappiness;
        let mut vals = div().flex().flex_row().items_center().gap(px(6.0));
        for (i, v) in [0u32, 10, 30, 60, 100].into_iter().enumerate() {
            vals = vals.child(chip(("swp", i), v.to_string(), cur == Some(v)).on_click(cx.listener(move |this, _, _, cx| {
                if this.dock.sys.tool.swap.as_ref().and_then(|r| r.as_ref().ok()).and_then(|i| i.swappiness) == Some(v) {
                    return;
                }
                this.tool_ask(Tool::Swap, "设置 swappiness", format!("vm.swappiness 设为 {v},立即生效,写进 /etc/sysctl.d/99-otter-swap.conf,重启后保留"), "设置 swappiness", v.to_string(), toolbox::set_swappiness_script(v), true, 0, vec!["值越小越倾向留在内存里(数据库服务器常用 10),越大越早用 swap;默认 60".to_string()], cx);
            })));
        }
        col = col.child(
            card().flex_row().items_center().justify_between().child(
                div().flex().flex_col().gap(px(2.0)).child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child("swappiness")).child(small(match cur { Some(v) => format!("当前 {v}。值越小越倾向留在内存里,越大越早用 swap"), None => "读不到当前值".into() }, c::ghost())),
            ).child(vals),
        );

        // 设备列表
        col = col.child(div().flex().flex_row().items_baseline().gap(px(8.0)).child(section("Swap 空间")).child(small(format!("{}", info.devs.len()), c::ghost())));
        if info.devs.is_empty() {
            col = col.child(card().items_center().py(px(24.0)).gap(px(6.0)).child(icon(IconName::MemoryStick, 22.0, c::ghost())).child(div().text_size(px(t::TYPE_BODY)).child("这台服务器没有启用 swap")).child(small("内存吃紧的小机器建议开 1-2 GB,在下面创建", c::ghost())));
        } else {
            let header = div().child(head("位置").flex_1().min_w_0()).child(head("类型").w(px(80.0))).child(head("大小").w(px(100.0))).child(head("已用").w(px(150.0))).child(head("持久化").w(px(90.0))).child(div().w(px(210.0)));
            let rows: Vec<Stateful<Div>> = info.devs.iter().enumerate().map(|(i, d)| self.swap_dev_row(i, d, cx)).collect();
            col = col.child(table_flow(header, rows));
        }
        if info.devs.iter().any(|d| d.kind == SwapKind::Zram) {
            col = col.child(small("zram 是内存压缩 swap,由系统服务(zram-generator 等)管理:这里的「停用」只到下次重启", c::ghost()));
        }
        if info.devs.iter().any(|d| d.path.contains(".1panel")) {
            col = col.child(small("这是 1Panel 创建的 swap 文件:在这里改了之后,面板里显示的记录可能和实际不一致", c::warn()));
        }
        // fstab 里有、现在没启用的
        if !info.off.is_empty() {
            let rows: Vec<Stateful<Div>> = info
                .off
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    row(("sw-off-r", i))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).child(SharedString::from(o.spec.clone())))
                        .child(div().w(px(220.0)).flex_shrink_0().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(if o.by_otter { "已停用(fstab 里的行已注释)" } else { "在 /etc/fstab 里,但没启用" }))
                        .child(div().w(px(80.0)).flex_shrink_0().flex().justify_end().child(chip(("sw-on", i), "启用", false).on_click(cx.listener(move |this, _, _, cx| {
                            this.tool_ask(Tool::Swap, "启用 Swap", "恢复 /etc/fstab 里被停用的 swap 行并启用,重启后也会启用".to_string(), "启用 Swap", "fstab".into(), Ok(toolbox::swap_on_script()), true, 120, Vec::new(), cx);
                        }))))
                })
                .collect();
            col = col.child(section("没有启用的")).child(table_flow(div().child(head("设备或文件")), rows));
        }

        // 创建 / 调整:路径、大小、常用大小、按钮在同一行,说明放下面
        let resize = self.dock.sys.tool.swap_resize.clone();
        let mut form = div().flex_shrink_0().flex().flex_row().items_end().gap(px(12.0));
        if let Some(p) = &resize {
            form = form.child(div().flex_1().min_w_0().child(super::daemonpage::labeled("文件", "", div().h(px(32.0)).flex().items_center().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::secondary()).child(SharedString::from(p.clone())))));
        } else {
            form = form.child(div().flex_1().min_w_0().child(field("路径", "", self.tool_input(4).unwrap())));
        }
        let mut presets = div().flex().flex_row().items_center().h(px(34.0)).gap(px(4.0));
        for (i, mb) in [512u64, 1024, 2048, 4096].into_iter().enumerate() {
            presets = presets.child(chip(("sw-pre", i), if mb >= 1024 { format!("{} GB", mb / 1024) } else { format!("{mb} MB") }, false).on_click(cx.listener(move |this, _, window, cx| this.tool_set(5, &mb.to_string(), window, cx))));
        }
        form = form
            .child(div().w(px(200.0)).child(field("大小(MB)", "", self.tool_input(5).unwrap())))
            .child(presets)
            .child(button("sw-submit", IconName::Check, if resize.is_some() { "调整" } else { "创建" }).on_click(cx.listener(|this, _, _, cx| this.swap_submit(cx))))
            .children(resize.is_some().then(|| button("sw-cancel", IconName::X, "取消").on_click(cx.listener(|this, _, window, cx| this.swap_form_reset(window, cx)))));
        let note = if resize.is_some() { "先在旁边建好新文件再换上去,磁盘要同时放得下新旧两个文件(含 1 GB 余量)" } else { "路径留空用 /swapfile;权限 600,写进 /etc/fstab;btrfs 自动设为 nodatacow;tmpfs、overlay、zfs 等不支持的位置会拒绝" };
        col = col.child(card().child(section(if resize.is_some() { "调整大小" } else { "创建 Swap 文件" })).child(form).child(small(note, c::ghost())));
        col.into_any_element()
    }

    // ---- 截图用的示例数据 ----

    fn tool_demo(&mut self) {
        let t = &mut self.dock.sys.tool;
        if t.demo {
            return;
        }
        t.demo = true;
        t.ports = Some(Ok(toolbox::parse_listens("tcp LISTEN 0 4096 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=744,fd=3))\ntcp LISTEN 0 4096 [::]:22 [::]:* users:((\"sshd\",pid=744,fd=4))\ntcp LISTEN 0 4096 127.0.0.1:5432 0.0.0.0:* users:((\"postgres\",pid=900,fd=5))\ntcp LISTEN 0 511 0.0.0.0:80 0.0.0.0:* users:((\"nginx\",pid=1200,fd=6))\ntcp LISTEN 0 511 0.0.0.0:8080 0.0.0.0:* users:((\"docker-proxy\",pid=2100,fd=4))\nudp UNCONN 0 0 127.0.0.53:53 0.0.0.0:*\n")));
        t.time = Some(toolbox::parse_time("@@EPOCH=1790000000\n@@DATE=2026-11-26 14:03:12 +0800\n@@TDC=1\nTimezone=Asia/Shanghai\nCanNTP=yes\nNTP=yes\nNTPSynchronized=yes\n@@SVC=systemd-timesyncd:active\n@@NTPB=systemd-timesyncd\n@@NTPSRV=\n@@ZONES\nAsia/Shanghai\nAsia/Tokyo\nAmerica/New_York\nUTC\n"));
        t.swap = Some(toolbox::parse_swap("@@MEM\nMemTotal: 4014080 kB\nMemAvailable: 2400000 kB\nSwapTotal: 3145724 kB\nSwapFree: 3000000 kB\n@@SWAPS\nFilename Type Size Used Priority\n/swapfile file 2097148 120000 -2\n/dev/zram0 partition 1048576 25724 100\n@@FSTAB\n/swapfile none swap sw 0 0 # otter\n# otter-off /dev/sdb2 none swap sw 0 0\n@@SWAPPINESS\n60\n@@ROOT\n/dev/sda1 ext4 41152736 10000000 28000000 27% /\n@@MANAGED\n"));
        match std::env::var("OTTER_TOOL").as_deref() {
            Ok("ports") => t.tool = Some(Tool::Ports),
            Ok("net") => {
                t.tool = Some(Tool::Net);
                t.net_out = Some(("ping example.com".into(), toolbox::parse_net("PING example.com (93.184.216.34) 56(84) bytes of data.\n64 bytes from 93.184.216.34: icmp_seq=1 ttl=56 time=11.2 ms\n64 bytes from 93.184.216.34: icmp_seq=2 ttl=56 time=11.0 ms\n--- example.com ping statistics ---\n4 packets transmitted, 4 received, 0% packet loss\n@@EXIT=0\n")));
            }
            Ok("ip") => {
                t.tool = Some(Tool::Net);
                t.net_kind = NetKind::PublicIp;
                t.nics = Some(toolbox::parse_nics("eth0 10.0.4.12/24\ndocker0 172.17.0.1/16\neth1 2408:8000::5/64\n"));
                t.net_out = Some(("出口公网 IP".into(), toolbox::parse_net("203.0.113.7\n@@EXIT=0\n")));
            }
            Ok("time") => t.tool = Some(Tool::Time),
            Ok("swap") => t.tool = Some(Tool::Swap),
            _ => {}
        }
        if t.tool == Some(Tool::Ports) {
            let listens = toolbox::parse_listens("tcp LISTEN 0 4096 127.0.0.1:5432 0.0.0.0:* users:((\"postgres\",pid=900,fd=5))\n");
            t.diag = Some((5432, "tcp".into(), toolbox::diagnose(5432, "tcp", &listens, &[], None)));
        }
    }
}
