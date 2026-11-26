//! 防火墙页:状态(ufw / firewalld)、规则列表、新增规则窗口;其余没有 ufw/firewalld 生效时显示 nftables/iptables 的只读规则。
//!
//! 逻辑(读取、解析、写入脚本、防锁死)在 `crate::firewall`;设计见 `docs/modules/firewall.md`。所有操作以 root 执行,
//! 启用、改默认策略前先放行 SSH 端口,删除放行 SSH 端口的规则会被挡住。

use std::time::Instant;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::InputState;

use super::super::*;
use super::daemonpage::{field, labeled};
use super::dockerpages::{DockAsk, head, icon_btn, row};
use super::syspages::{chip, message, reading, tip};
use crate::docker::Action as DockAction;
use crate::firewall::{self, Action, FwInfo, Front, NewRule, PortMode, Proto, Rule};
use crate::sysops::{Job, Kind};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum FwTab {
    #[default]
    Host,
    Docker,
}

/// 新增规则窗口的状态(输入框之外的部分)
#[derive(Default)]
pub(crate) struct FwForm {
    pub(super) open: bool,
    tab: FwTab,
    action: Option<Action>,
    proto: Option<Proto>,
    /// 端口、来源
    pub(super) inputs: Option<Vec<Entity<InputState>>>,
    /// 「限制来源」窗口:(协议, 端口, 容器名);None 表示没打开
    du: Option<(String, u16, String)>,
    du_input: Option<Entity<InputState>>,
}

impl FwForm {
    fn action(&self) -> Action {
        self.action.unwrap_or(Action::Allow)
    }

    fn proto(&self) -> Proto {
        self.proto.unwrap_or(Proto::Tcp)
    }
}

fn act_color(a: Action) -> (Rgba, Rgba) {
    match a {
        Action::Allow => (c::online(), c::online_soft()),
        Action::Deny | Action::Reject => (c::danger(), c::danger().opacity(0.17)),
        Action::Limit => (c::warn(), c::warn_soft()),
    }
}

fn rule_text(r: &Rule) -> String {
    let zone = if r.zone.is_empty() { String::new() } else { format!("[{}] ", r.zone) };
    format!("{zone}{} {},来自 {}", r.action.label(), r.to, r.from)
}

impl Home {
    fn fw_info(&self) -> Option<&FwInfo> {
        self.dock.sys.fw.as_ref()?.as_ref().ok()
    }

    /// 要防住的 SSH 端口:服务器上 sshd 监听的,加上当前连接用的端口
    fn fw_ssh(&self, info: &FwInfo) -> Vec<u16> {
        let mut v = info.ssh_ports.clone();
        let p = crate::ctl::split_user_host(&self.node().user_host).2;
        if !v.contains(&p) {
            v.push(p);
        }
        v
    }

    fn fw_job(label: &str, target: &str, script: String) -> Job {
        Job { label: label.into(), target: target.into(), script, root: true, input: None, reload: Some((Kind::Firewall, firewall::READ_SCRIPT.to_string(), true)), guard: None }
    }

    fn fw_notice(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.dock.notice = Some((msg.into(), Instant::now()));
        cx.notify();
    }

    /// 状态:(标题, 说明, 颜色)。标题里有「已启用 / 未启用」,页面副标题和提示卡都用它
    fn fw_status(&self, info: &FwInfo) -> (String, String, Rgba) {
        let ssh = self.fw_ssh(info);
        let front = info.manage();
        if info.conflict() {
                ("ufw 和 firewalld 同时在运行".into(), "两个同时启用会互相覆盖规则,请到终端里停掉其中一个;Otter 暂不改规则".into(), c::warn())
            } else if let Some(f) = front {
                let ssh_txt = ssh.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("、");
                let policy = match f {
                    Front::Ufw => format!("默认入站:{}", match info.default_in.as_str() { "deny" => "拒绝", "allow" => "允许", "reject" => "拒绝(返回错误)", _ => "未知" }),
                    Front::Firewalld => format!("默认区域:{}", if info.default_zone.is_empty() { "—" } else { &info.default_zone }),
                    Front::Iptables => format!("INPUT 链默认:{}", match info.input_policy.as_str() { "DROP" => "丢弃", "ACCEPT" => "放行", "" => "未知", p => p }),
                };
                if f == Front::Iptables {
                    if info.ipt_live {
                        ("iptables · 已启用".into(), format!("{policy} · SSH 端口 {ssh_txt} · Otter 的规则放在 OTTER-INPUT 链,不改其他规则"), c::online())
                    } else {
                        ("iptables · 未启用".into(), format!("Otter 的规则目前没有套用到 iptables,打开开关后生效 · {policy} · SSH 端口 {ssh_txt}"), c::warn())
                    }
                } else if info.active().is_some() {
                    (format!("{} · 已启用", f.label()), format!("{policy} · SSH 端口 {ssh_txt}"), c::online())
                } else {
                    (format!("{} · 未启用", f.label()), format!("已安装但没有生效,服务器目前没有防火墙保护 · SSH 端口 {ssh_txt}"), c::warn())
                }
            } else {
                ("没有检测到 ufw、firewalld 或 iptables".into(), if info.low_level { "Otter 管理 ufw、firewalld、iptables;这台只有 nftables,下面的规则只读".into() } else { "服务器上没有装防火墙".into() }, c::muted())
            }
    }

    /// 右上角的开关:ufw / firewalld 是启用与停用,iptables 是 OTTER-INPUT 链挂不挂在 INPUT 上
    pub(crate) fn fw_header_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(info) = self.fw_info() else { return Vec::new() };
        let (Some(f), false) = (info.manage(), info.conflict()) else { return Vec::new() };
        let on = if f == Front::Iptables { info.ipt_live } else { info.active().is_some() };
        let (ssh2, name) = (self.fw_ssh(info), f.label().to_string());
        let sw = super::daemonpage::switch("fw-switch", on);
        vec![tip(sw, if on { "停用防火墙" } else { "启用防火墙" })
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.op_busy("防火墙").is_some() {
                    return;
                }
                let rules = this.fw_info().map(|i| i.rules.clone()).unwrap_or_default();
                if on {
                    let script = Ok(match f {
                        Front::Ufw => firewall::ufw_disable(),
                        Front::Firewalld => firewall::fwd_disable(),
                        Front::Iptables => firewall::ipt_disable(),
                    });
                    let body = if f == Front::Iptables { "停用后 Otter 写的入站规则不再生效(规则保留,重新打开即可恢复)" } else { "停用后服务器的所有端口不再受它保护" };
                    this.fw_ask("停用防火墙", body, "停用防火墙", name.clone(), script, cx);
                } else {
                    let script = match f {
                        Front::Ufw => firewall::ufw_enable(&ssh2),
                        Front::Firewalld => firewall::fwd_enable(&ssh2),
                        Front::Iptables => Ok(firewall::ipt_enable(&rules)),
                    };
                    let body = if f == Front::Iptables { "把 Otter 的入站规则套用到 iptables,并设为开机自动套用" } else { "启用前会检查有没有规则挡住 SSH 端口,并先放行它;启用后如果连不上服务器,2 分钟后服务器会自动关掉防火墙。其他没放行的入站端口会被挡住" };
                    this.fw_ask("启用防火墙", body, "启用防火墙", f.label().to_string(), script, cx);
                }
            }))
            .into_any_element()]
    }

    /// 页面副标题:当前防火墙和状态
    pub(crate) fn fw_subtitle(&self) -> String {
        match self.fw_info() {
            Some(i) => {
                let (title, sub, _) = self.fw_status(i);
                // 没有可管理的防火墙或冲突时,说明放在下面的提示卡里,副标题不重复
                if i.manage().is_none() || i.conflict() { title } else { format!("{title} · {sub}") }
            }
            None => format!("{} 上的防火墙(ufw、firewalld、iptables)", self.node().name),
        }
    }

    pub(super) fn fw_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.sys.fwf.inputs.is_none() {
            let v = ["80 或 8000-8100", "留空表示任意地址;可填 IP 或网段"]
                .iter()
                .map(|ph| {
                    let e = cx.new(|cx| InputState::new(window, cx).placeholder(*ph));
                    cx.observe(&e, |_, _, cx| cx.notify()).detach();
                    e
                })
                .collect();
            self.dock.sys.fwf.inputs = Some(v);
        }
        let f = &mut self.dock.sys.fwf;
        f.action = None;
        f.proto = None;
        f.open = true;
        if let Some(ins) = &f.inputs {
            for i in ins {
                i.update(cx, |s, cx| s.set_value("", window, cx));
            }
        }
        cx.notify();
    }

    /// 表单里填的规则
    fn fw_new_rules(&self, cx: &App) -> Option<Vec<NewRule>> {
        let ins = self.dock.sys.fwf.inputs.as_ref()?;
        let f = &self.dock.sys.fwf;
        let from = ins[1].read(cx).value().trim().to_string();
        let mut ports = fw_ports(ins[0].read(cx).value().as_ref());
        if ports.is_empty() {
            ports.push(String::new());
        }
        Some(ports.into_iter().map(|port| NewRule { action: f.action(), port, proto: f.proto(), from: from.clone() }).collect())
    }

    fn fw_submit(&mut self, cx: &mut Context<Self>) {
        let Some(ns) = self.fw_new_rules(cx) else { return };
        let Some(info) = self.fw_info().cloned() else { return };
        let Some(front) = info.manage() else { return };
        for n in &ns {
            if let Err(e) = n.validate() {
                return self.fw_notice(e, cx);
            }
            if firewall::blocks_ssh(n, &self.fw_ssh(&info)) {
                return self.fw_notice("这条规则会挡住 SSH 端口,保存后可能连不上服务器", cx);
            }
        }
        let zone = if info.default_zone.is_empty() { "public" } else { &info.default_zone };
        let each = |f: &dyn Fn(&NewRule) -> Result<String, String>| -> Result<String, String> { ns.iter().map(f).collect::<Result<Vec<_>, _>>().map(|v| v.concat()) };
        let script = match front {
            Front::Ufw => each(&firewall::ufw_add),
            Front::Firewalld if !info.firewalld_active => Err("firewalld 没有运行,先启用防火墙再加规则".to_string()),
            Front::Firewalld => each(&|n| firewall::fwd_add(zone, n)),
            Front::Iptables => firewall::ipt_add_many(&ns, &info.rules),
        };
        match script {
            Ok(s) => {
                self.dock.sys.fwf.open = false;
                self.sys_run(Self::fw_job("添加规则", &fw_describe(&ns), s), cx);
            }
            Err(e) => self.fw_notice(e, cx),
        }
    }

    fn fw_ask(&mut self, title: &'static str, body: &'static str, verb: &'static str, name: String, script: Result<String, String>, cx: &mut Context<Self>) {
        match script {
            Ok(s) => {
                let mut job = Self::fw_job(verb, &name, s);
                // 脚本里排了定时回滚:做完后由本机检查新连接,连得上才取消回滚
                if job.script.contains(firewall::GUARD_MARK) {
                    let (_, host, port) = crate::ctl::split_user_host(&self.node().user_host);
                    job.guard = Some(crate::sysops::Guard { host, port, secs: firewall::GUARD_SECS, cancel: firewall::guard_cancel() });
                }
                self.ask(DockAsk { title, tile: IconName::Shield, name: name.clone(), detail: "防火墙", body, label: verb, action: DockAction::Sys(job), list: Vec::new() }, cx)
            }
            Err(e) => self.fw_notice(e, cx),
        }
    }


    /// 打开「限制来源」窗口
    fn fw_du_open(&mut self, proto: String, port: u16, who: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.sys.fwf.du_input.is_none() {
            let e = cx.new(|cx| InputState::new(window, cx).placeholder("如 203.0.113.5、10.0.0.0/8,多个用逗号隔开"));
            cx.observe(&e, |_, _, cx| cx.notify()).detach();
            self.dock.sys.fwf.du_input = Some(e);
        }
        if let Some(e) = &self.dock.sys.fwf.du_input {
            e.update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.dock.sys.fwf.du = Some((proto, port, who));
        cx.notify();
    }

    /// 窗口里填的来源
    fn fw_du_sources(&self, cx: &App) -> Vec<String> {
        let Some(e) = &self.dock.sys.fwf.du_input else { return Vec::new() };
        e.read(cx).value().split([',', ' ', ';', '\n']).map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
    }

    /// 设置或取消一个端口的限制:先弹确认框,框里列出会写进 DOCKER-USER 的规则
    fn fw_du_ask(&mut self, proto: &str, port: u16, who: &str, mode: Option<PortMode>, cx: &mut Context<Self>) {
        let Some(info) = self.fw_info().cloned() else { return };
        let script = firewall::du_set(proto, port, mode.as_ref(), &info.du, &self.fw_ssh(&info));
        let (title, body, label, list) = match &mode {
            None => ("取消端口限制", "取消后这个端口恢复 Docker 默认的对外开放", "取消限制", Vec::new()),
            Some(m) => (
                "禁止外部访问端口",
                "外部(公网和局域网)访问不到宿主机的这个端口;本机和 Docker 内部网络不受影响。规则写进 iptables 的 DOCKER-USER 链,重启后自动恢复",
                "写入规则",
                firewall::du_preview(proto, port, m),
            ),
        };
        let name = format!("{who} · {port}/{proto}");
        match script {
            Ok(s) => self.ask(DockAsk { title, tile: IconName::Shield, name: name.clone(), detail: "防火墙", body, label, action: DockAction::Sys(Self::fw_job(label, &name, s)), list }, cx),
            Err(e) => self.fw_notice(e, cx),
        }
    }

    fn fw_du_submit(&mut self, cx: &mut Context<Self>) {
        let Some((proto, port, who)) = self.dock.sys.fwf.du.clone() else { return };
        let src = self.fw_du_sources(cx);
        let Some(info) = self.fw_info().cloned() else { return };
        match firewall::du_set(&proto, port, Some(&PortMode::Only(src.clone())), &info.du, &self.fw_ssh(&info)) {
            Ok(s) => {
                self.dock.sys.fwf.du = None;
                self.sys_run(Self::fw_job("限制来源", &format!("{who} · {port}/{proto}"), s), cx);
            }
            Err(e) => self.fw_notice(e, cx),
        }
    }

    /// 「容器暴露端口」:每个发布到宿主机的端口现在谁能访问,并给出限制操作
    fn fw_exposed(&self, info: &FwInfo, busy: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        struct Item {
            who: String,
            p: crate::docker::Publish,
        }
        let mut items: Vec<Item> = Vec::new();
        for ct in self.dock.snap.containers.iter().filter(|c| c.running()) {
            for p in &ct.publish {
                items.push(Item { who: ct.name.clone(), p: p.clone() });
            }
        }
        if items.is_empty() {
            return None;
        }
        items.sort_by(|a, b| (a.p.host.parse::<u32>().unwrap_or(u32::MAX), &a.who).cmp(&(b.p.host.parse::<u32>().unwrap_or(u32::MAX), &b.who)));
        let front = info.manage().map(|f| f.label()).unwrap_or("防火墙");
        let header = div().child(head("容器").w(px(170.0))).child(head("端口").w(px(150.0))).child(head("绑定").w(px(120.0))).child(head("现在谁能访问").flex_1().min_w_0()).child(div().w(px(250.0)));
        let rows: Vec<Stateful<Div>> = items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let p = &it.p;
                let port: Option<u16> = p.host.parse().ok();
                let limit = port.and_then(|n| info.du.iter().find(|l| l.proto == p.proto && l.port == n));
                let (text, fg): (String, Rgba) = if p.loopback() {
                    ("仅本机".into(), c::muted())
                } else if !p.wildcard() {
                    (format!("仅 {} 的网卡", p.ip), c::muted())
                } else {
                    match limit.map(|l| (&l.mode, l.live)) {
                        Some((PortMode::Deny, true)) => ("已禁止外部访问".into(), c::online()),
                        Some((PortMode::Only(s), true)) => (format!("只允许 {}", s.join("、")), c::online()),
                        Some((_, false)) => ("已设置限制,但还没套用到 iptables".into(), c::warn()),
                        None if info.manage() == Some(Front::Iptables) => ("任何人(不受上面的入站规则约束)".into(), c::warn()),
                        None => (format!("任何人(不受 {front} 规则约束)"), c::warn()),
                    }
                };
                let mut act = div().w(px(250.0)).flex_shrink_0().flex().flex_row().justify_end().gap(px(6.0));
                if let (Some(n), true, false, true) = (port, p.wildcard(), busy, info.iptables) {
                    let (proto, who) = (p.proto.clone(), it.who.clone());
                    if limit.is_some() {
                        let (pr, w) = (proto.clone(), who.clone());
                        act = act.child(chip(("fw-du-off", i), "取消限制", false).on_click(cx.listener(move |this, _, _, cx| this.fw_du_ask(&pr, n, &w, None, cx))));
                    }
                    let (pr, w) = (proto.clone(), who.clone());
                    act = act.child(chip(("fw-du-src", i), "限制来源", false).on_click(cx.listener(move |this, _, window, cx| this.fw_du_open(pr.clone(), n, w.clone(), window, cx))));
                    act = act.child(chip(("fw-du-deny", i), "禁止外部", false).on_click(cx.listener(move |this, _, _, cx| this.fw_du_ask(&proto, n, &who, Some(PortMode::Deny), cx))));
                }
                row(("fw-x", i))
                    .child(div().w(px(170.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).child(SharedString::from(it.who.clone())))
                    .child(div().w(px(150.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).child(SharedString::from(format!("{}/{} → {}", p.host, p.proto, p.container))))
                    .child(div().w(px(120.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(SharedString::from(if p.wildcard() { "0.0.0.0".to_string() } else { p.ip.clone() })))
                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(fg).child(SharedString::from(text)))
                    .child(act)
            })
            .collect();
        let hint = if info.iptables {
            if info.manage() == Some(Front::Iptables) {
                "Docker 发布的端口不经过上面的入站规则,要在这里限制;规则写进 iptables 的 DOCKER-USER 链。更简单的做法:容器端口只绑定 127.0.0.1,再用网站反代对外"
            } else {
                "Docker 发布的端口不经过 ufw / firewalld 的规则,要在这里限制;规则写进 iptables 的 DOCKER-USER 链。更简单的做法:容器端口只绑定 127.0.0.1,再用网站反代对外"
            }
        } else {
            "服务器上没有 iptables 命令,这里只能查看,没法限制"
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(div().flex().flex_row().items_baseline().gap(px(8.0)).child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child("容器暴露端口")).child(small_n(items.len())))
                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(hint))
                .child(super::dockerpages::table_flow(header, rows))
                .into_any_element(),
        )
    }

    /// 「限制来源」窗口(没打开时是 None)
    pub(crate) fn fw_du_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let (proto, port, who) = self.dock.sys.fwf.du.clone().filter(|_| self.page == Page::Firewall)?;
        let input = self.dock.sys.fwf.du_input.clone()?;
        let info = self.fw_info()?;
        let src = self.fw_du_sources(cx);
        let mode = PortMode::Only(src.clone());
        let check = firewall::du_set(&proto, port, Some(&mode), &info.du, &self.fw_ssh(info));
        let (can, summary) = match &check {
            Ok(_) => (true, format!("只允许 {} 访问 {port}/{proto},其他外部来源会被丢弃", src.join("、"))),
            Err(e) if src.is_empty() => (false, e.clone()),
            Err(e) => (false, e.clone()),
        };
        let preview = firewall::du_preview(&proto, port, &mode).join("\n");
        let title = div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .px(px(24.0))
            .pt(px(24.0))
            .pb(px(14.0))
            .child(div().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).child("限制访问来源"))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(format!("{who} 发布的 {port}/{proto}:只有下面的来源能从外部访问,本机和 Docker 内部网络不受影响"))));
        let body = div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .px(px(24.0))
            .pb(px(20.0))
            .child(field("允许的来源", "IPv4 地址或网段,多个用逗号隔开", &input))
            .child(div().p(px(12.0)).rounded(px(t::RADIUS_MD)).bg(c::field()).text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::muted()).child(SharedString::from(preview)));
        let cancel = crate::node_form::ctl_btn("fw-du-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
            this.dock.sys.fwf.du = None;
            cx.notify();
        }));
        let submit = div()
            .id("fw-du-ok")
            .flex()
            .flex_row()
            .items_center()
            .h(px(34.0))
            .px(px(16.0))
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .when(can, |d| d.bg(c::accent()).text_color(c::on_accent()).cursor_pointer().hover(|s| s.opacity(0.88)))
            .when(!can, |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()))
            .child("写入规则")
            .on_click(cx.listener(|this, _, _, cx| this.fw_du_submit(cx)));
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(24.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(c::line())
            .child(div().flex_1().min_w_0().text_size(px(t::TYPE_MICRO)).text_color(if can { c::ghost() } else { c::warn() }).child(SharedString::from(summary)))
            .child(cancel)
            .child(submit);
        Some(crate::ui::modal(
            "fw-du-dialog",
            560.0,
            cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                "escape" => {
                    this.dock.sys.fwf.du = None;
                    cx.notify();
                    cx.stop_propagation();
                }
                "enter" => {
                    this.fw_du_submit(cx);
                    cx.stop_propagation();
                }
                _ => {}
            }),
            [title.into_any_element(), body.into_any_element(), footer.into_any_element()],
        ))
    }

    pub(crate) fn fw_debug_tab(&mut self) {
        if std::env::var_os("OTTER_FW_TAB").is_some() {
            self.dock.sys.fwf.tab = FwTab::Docker;
        }
    }

    pub(crate) fn firewall_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let info = match &self.dock.sys.fw {
            None => return reading("读取服务器上的防火墙"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了防火墙状态", e.clone()),
            Some(Ok(i)) => i.clone(),
        };
        let ssh = self.fw_ssh(&info);
        let front = info.manage();
        let busy = self.op_busy("防火墙").is_some();

        // ---- 状态:没有可管理的防火墙或冲突时才用提示卡,其余状态显示在页面副标题 ----
        let (title, sub, color) = self.fw_status(&info);
        let mut actions = div().flex().flex_row().items_center().gap(px(8.0));
        if let (Some(Front::Ufw), false) = (front, info.conflict()) {
            if info.active().is_some() {
                let allow = info.default_in == "allow";
                let (ssh3, name3) = (ssh.clone(), "ufw".to_string());
                actions = actions.child(chip(("fw-default", 0), if allow { "默认改为拒绝入站" } else { "默认改为允许入站" }, false).on_click(cx.listener(move |this, _, _, cx| {
                    let script = firewall::ufw_default(!allow, &ssh3);
                    if allow {
                        this.fw_ask("默认拒绝入站", "没有明确放行的端口都会被挡住;会先放行 SSH 端口,连不上服务器时 2 分钟后自动改回允许", "改默认策略", name3.clone(), script, cx);
                    } else {
                        this.fw_ask("默认允许入站", "没有明确拒绝的端口都会放行,防火墙基本等于只剩你写的拒绝规则", "改默认策略", name3.clone(), script, cx);
                    }
                })));
            }
        }
        if front.is_some() && !info.conflict() && self.dock.sys.fwf.tab == FwTab::Host {
            actions = actions.child(button("fw-add", IconName::Plus, "新增规则").on_click(cx.listener(|this, _, window, cx| this.fw_open(window, cx))));
        }
        let status = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.0))
            .p(px(16.0))
            .rounded(px(t::RADIUS_LG))
            .border_1()
            .border_color(c::dialog_border())
            .child(div().size(px(36.0)).flex_shrink_0().flex().items_center().justify_center().rounded(px(t::RADIUS_MD)).bg(color.opacity(0.17)).child(icon(IconName::Shield, 18.0, color)))
            .child(div().flex_1().min_w_0().flex().flex_col().gap(px(2.0)).child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(title))).child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(sub))))
            ;

        let tab = self.dock.sys.fwf.tab;
        let n_ports: usize = self.dock.snap.containers.iter().filter(|c| c.running()).map(|c| c.publish.len()).sum();
        let tab_chips = div().flex().flex_row().gap(px(6.0)).children([("主机防火墙", FwTab::Host), ("容器端口防护", FwTab::Docker)].into_iter().enumerate().map(|(i, (l, tb))| {
            let label = if tb == FwTab::Docker && n_ports > 0 { format!("{l} {n_ports}") } else { l.to_string() };
            chip(("fw-tab", i), label, tab == tb).on_click(cx.listener(move |this, _, _, cx| {
                this.dock.sys.fwf.tab = tb;
                cx.notify();
            }))
        }));
        let tabs = div().flex_shrink_0().flex().flex_row().items_center().gap(px(8.0)).child(tab_chips).child(div().flex_1()).child(actions);
        let mut col = div().flex_grow(1.0).flex_shrink_0().pb(px(14.0)).flex().flex_col().gap(px(12.0)).child(tabs);
        if front.is_none() || info.conflict() {
            col = col.child(status);
        }

        // ---- 规则 ----
        if tab == FwTab::Host && front.is_some() && !info.conflict() {
            let firewalld = front == Some(Front::Firewalld);
            let mut header = div().child(head("目标").w(px(200.0))).child(head("动作").w(px(70.0))).child(head("来源").flex_1().min_w_0());
            if firewalld {
                header = header.child(head("区域").w(px(100.0)));
            }
            header = header.child(div().w(px(36.0)));
            let rows: Vec<Stateful<Div>> = info
                .rules
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let (fg, bg) = act_color(r.action);
                    let guarded = firewall::is_ssh_rule(r, &ssh);
                    let mut act = div().w(px(36.0)).flex_shrink_0().flex().flex_row().justify_end();
                    if !busy {
                        let (rule, ssh, front, rules_now) = (r.clone(), ssh.clone(), front, info.rules.clone());
                        act = act.child(tip(icon_btn(("fw-del", i), IconName::Trash, if guarded { c::ghost() } else { c::danger() }), if guarded { "放行 SSH 端口的规则,不能在这里删" } else { "删除" }).on_click(cx.listener(move |this, _, _, cx| {
                            if firewall::is_ssh_rule(&rule, &ssh) {
                                return this.fw_notice("这条规则放行了 SSH 端口,删除后可能连不上服务器,请在终端里处理", cx);
                            }
                            let script = match front {
                                Some(Front::Ufw) => match rule.kind { firewall::RuleKind::Ufw(n) => firewall::ufw_delete(n, &rule.key), _ => Err("不是 ufw 规则".into()) },
                                Some(Front::Iptables) => firewall::ipt_delete(&rule, &rules_now),
                                _ => firewall::fwd_delete(&rule),
                            };
                            this.fw_ask("删除防火墙规则", "删除后这条规则立即失效", "删除规则", rule_text(&rule), script, cx);
                        })));
                    }
                    let mut rw = row(("fw-r", i))
                        .child(div().w(px(200.0)).flex_shrink_0().flex().flex_row().items_center().gap(px(6.0)).child(div().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).child(SharedString::from(r.to.clone()))).children(r.v6.then(|| div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child("v6"))))
                        .child(div().w(px(70.0)).flex_shrink_0().child(div().px(px(6.0)).rounded(px(t::RADIUS_SM)).bg(bg).text_size(px(t::TYPE_CAPTION)).text_color(fg).child(r.action.label())))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(if r.from == "Anywhere" { "任意地址".to_string() } else { r.from.clone() })));
                    if firewalld {
                        rw = rw.child(div().w(px(100.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(r.zone.clone())));
                    }
                    rw.child(act)
                })
                .collect();
            col = col.child(div().flex().flex_row().items_baseline().gap(px(8.0)).child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child("规则")).child(small_n(info.rules.len())));
            if rows.is_empty() {
                let msg = if info.active().is_some() || front == Some(Front::Iptables) { "还没有规则" } else { "防火墙没启用时读不到规则,启用后这里会列出" };
                col = col.child(div().flex_shrink_0().py(px(28.0)).flex().justify_center().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(msg));
            } else {
                col = col.child(super::dockerpages::table_flow(header, rows));
            }
        }

        // ---- 容器暴露端口 ----
        if tab == FwTab::Docker {
            col = match self.fw_exposed(&info, busy, cx) {
                Some(x) => col.child(x),
                None => col.child(div().flex_shrink_0().py(px(28.0)).flex().justify_center().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child("没有容器发布端口到宿主机")),
            };
        }

        // ---- 只读:nftables / iptables ----
        if tab == FwTab::Host && front.is_none() && !info.raw.trim().is_empty() {
            col = col.child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child("当前规则(nftables,只读)")).child(
                div()
                    .flex_shrink_0()
                    .p(px(14.0))
                    .rounded(px(t::RADIUS_LG))
                    .border_1()
                    .border_color(c::dialog_border())
                    .text_size(px(t::TYPE_CAPTION))
                    .font_family(MONO)
                    .text_color(c::muted())
                    .child(SharedString::from(info.raw.clone())),
            );
        }
        col.into_any_element()
    }

    /// 新增规则的窗口(没打开时是 None)
    pub(crate) fn fw_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if self.page != Page::Firewall || !self.dock.sys.fwf.open {
            return None;
        }
        let ins = self.dock.sys.fwf.inputs.clone()?;
        let info = self.fw_info()?;
        let f = &self.dock.sys.fwf;
        let rules = self.fw_new_rules(cx)?;
        let ssh = self.fw_ssh(info);
        let (can, summary) = match rules.iter().find_map(|r| r.validate().err()) {
            Some(e) => (false, e),
            None if rules.iter().any(|r| firewall::blocks_ssh(r, &ssh)) => (false, "这条规则会挡住 SSH 端口,保存后可能连不上服务器".to_string()),
            None => (true, fw_describe(&rules)),
        };
        let acts = div().flex().flex_row().gap(px(6.0)).children([("放行", Action::Allow), ("丢弃", Action::Deny), ("拒绝", Action::Reject)].into_iter().enumerate().map(|(i, (l, a))| {
            chip(("fw-act", i), l, f.action() == a).on_click(cx.listener(move |this, _, _, cx| {
                this.dock.sys.fwf.action = Some(a);
                cx.notify();
            }))
        }));
        let protos = div().flex().flex_row().gap(px(6.0)).children([("TCP", Proto::Tcp), ("UDP", Proto::Udp), ("TCP 和 UDP", Proto::Both)].into_iter().enumerate().map(|(i, (l, p))| {
            chip(("fw-proto", i), l, f.proto() == p).on_click(cx.listener(move |this, _, _, cx| {
                this.dock.sys.fwf.proto = Some(p);
                cx.notify();
            }))
        }));
        // 常用端口预设:点一下填入端口并选 TCP
        let port_in = ins[0].clone();
        let picked = fw_ports(ins[0].read(cx).value().as_ref());
        let presets = div().flex().flex_row().flex_wrap().gap(px(6.0)).children([("SSH 22", "22"), ("HTTP 80", "80"), ("HTTPS 443", "443"), ("FTP 21", "21"), ("SMTP 25", "25"), ("DNS 53", "53"), ("MySQL 3306", "3306"), ("PostgreSQL 5432", "5432"), ("Redis 6379", "6379"), ("MongoDB 27017", "27017")].into_iter().enumerate().map(|(i, (l, port))| {
            let port_in = port_in.clone();
            let on = picked.iter().any(|p| p == port);
            div()
                .id(("fw-preset", i))
                .h(px(24.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .rounded(px(t::RADIUS_SM))
                .border_1()
                .text_size(px(t::TYPE_CAPTION))
                .when(on, |d| d.bg(c::accent_soft()).border_color(c::accent().opacity(0.6)).text_color(c::accent()).font_weight(FontWeight::MEDIUM))
                .when(!on, |d| d.bg(c::field()).border_color(c::field_border()).text_color(c::muted()).hover(|s| s.bg(c::field_hover()).text_color(c::secondary())))
                .child(l)
                .on_click(cx.listener(move |this, _, window, cx| {
                // 再点一次取消;可以连续点多个
                let mut v = fw_ports(port_in.read(cx).value().as_ref());
                match v.iter().position(|p| p == port) {
                    Some(k) => {
                        v.remove(k);
                    }
                    None => v.push(port.to_string()),
                }
                port_in.update(cx, |s, cx| s.set_value(v.join(","), window, cx));
                if this.dock.sys.fwf.proto.is_none() {
                    this.dock.sys.fwf.proto = Some(Proto::Tcp);
                }
                cx.notify();
            }))
        }));
        let title = div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .px(px(24.0))
            .pt(px(24.0))
            .pb(px(14.0))
            .child(div().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).child("新增防火墙规则"))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("保存后立即生效;「丢弃」直接不回应,「拒绝」会回复对方端口不可用"));
        let body = div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .px(px(24.0))
            .pb(px(20.0))
            .child(labeled("动作", "", acts))
            .child(labeled("协议", "", protos))
            .child(labeled(
                "端口",
                "单个端口或一段,如 80、8000-8100;多个用逗号隔开,如 22,80,443",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .h(px(32.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .rounded(px(t::RADIUS_MD))
                            .bg(c::field())
                            .border_1()
                            .border_color(c::field_border())
                            .text_size(px(t::TYPE_CONTROL))
                            .child(div().flex_1().min_w_0().child(gpui_kit::component::input::Input::new(&ins[0]).appearance(false).with_size(px(12.0 / 0.875)))),
                    )
                    .child(presets),
            ))
            .child(field("来源", "只对这个 IP 或网段生效;留空表示任意地址", &ins[1]));
        let cancel = crate::node_form::ctl_btn("fw-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| {
            this.dock.sys.fwf.open = false;
            cx.notify();
        }));
        let submit = div()
            .id("fw-ok")
            .flex()
            .flex_row()
            .items_center()
            .h(px(34.0))
            .px(px(16.0))
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .when(can, |d| d.bg(c::accent()).text_color(c::on_accent()).cursor_pointer().hover(|s| s.opacity(0.88)))
            .when(!can, |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()))
            .child("保存")
            .on_click(cx.listener(|this, _, _, cx| this.fw_submit(cx)));
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(24.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(c::line())
            .child(div().flex_1().min_w_0().text_size(px(t::TYPE_MICRO)).text_color(if can { c::ghost() } else { c::warn() }).child(SharedString::from(summary)))
            .child(cancel)
            .child(submit);
        Some(crate::ui::modal(
            "fw-dialog",
            520.0,
            cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                "escape" => {
                    this.dock.sys.fwf.open = false;
                    cx.notify();
                    cx.stop_propagation();
                }
                "enter" => {
                    this.fw_submit(cx);
                    cx.stop_propagation();
                }
                _ => {}
            }),
            [title.into_any_element(), body.into_any_element(), footer.into_any_element()],
        ))
    }
}

fn small_n(n: usize) -> Div {
    div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(n.to_string())
}

/// 截图用的示例数据:`OTTER_FW_DEMO=firewalld|off|none` 换场景
pub(crate) fn demo_fw() -> &'static str {
    match std::env::var("OTTER_FW_DEMO").as_deref() {
        Ok("firewalld") => "@@HAS firewall-cmd nft\n@@ON firewalld\n@@SSH 22\n@@FWD public\n@@ZONE public\npublic (active)\n  target: default\n  interfaces: eth0\n  sources: 10.8.0.0/24\n  services: cockpit ssh\n  ports: 80/tcp 443/tcp 8000-8100/udp\n  rich rules: \n\trule family=\"ipv4\" source address=\"203.0.113.5\" port port=\"3306\" protocol=\"tcp\" accept\n\trule port port=\"6379\" protocol=\"tcp\" drop\n",
        Ok("off") => "@@HAS ufw nft\n@@ON\n@@SSH 22\n@@UFW\nStatus: inactive\n@@UFWN\nStatus: inactive\n@@RAW\n",
        Ok("ipt") => "@@HAS iptables nft\n@@ON\n@@SSH 22\n@@RAW\n-P INPUT ACCEPT\n-A INPUT -j OTTER-INPUT\n@@IPT\nallow tcp 80 -\nallow tcp 443 -\ndrop tcp 3306 -\nallow tcp 5432 203.0.113.0/24\n@@POL ACCEPT\n@@IPTON\n@@DU\ntcp 8097 deny\n@@DUL\n-A DOCKER-USER -p tcp -m conntrack --ctorigdstport 8097 --ctdir ORIGINAL ! -i docker0 ! -i br-+ -m comment --comment otter:dp -j DROP\n",
        Ok("none") => "@@HAS nft\n@@ON\n@@SSH 22\n@@RAW\ntable inet filter {\n\tchain input {\n\t\ttype filter hook input priority filter; policy accept;\n\t\ttcp dport 22 accept\n\t}\n}\n",
        _ => "@@HAS ufw iptables nft\n@@ON ufw\n@@SSH 22\n@@UFW\nStatus: active\nLogging: on (low)\nDefault: deny (incoming), allow (outgoing), disabled (routed)\n@@UFWN\nStatus: active\n\n     To                         Action      From\n     --                         ------      ----\n[ 1] 22/tcp                     ALLOW IN    Anywhere\n[ 2] 80/tcp                     ALLOW IN    Anywhere\n[ 3] 443/tcp                    ALLOW IN    Anywhere\n[ 4] 3306/tcp                   ALLOW IN    203.0.113.0/24\n[ 5] 6379                       DENY IN     Anywhere\n[ 6] 22/tcp (v6)                ALLOW IN    Anywhere (v6)\n[ 7] 80/tcp (v6)                ALLOW IN    Anywhere (v6)\n@@DU\ntcp 8097 deny\n@@DUL\n-A DOCKER-USER -p tcp -m conntrack --ctorigdstport 8097 --ctdir ORIGINAL ! -i docker0 ! -i br-+ -m comment --comment otter:dp -j DROP\n",
    }
}

/// 端口框里的内容拆成一个个端口(逗号、空格、顿号隔开)
fn fw_ports(s: &str) -> Vec<String> {
    s.split([',', ',', '、', ' ', ';']).map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
}

/// 一次添加多个端口时的一句话摘要
fn fw_describe(ns: &[NewRule]) -> String {
    match ns {
        [one] => one.describe(),
        [first, ..] => NewRule { port: ns.iter().map(|n| n.port.as_str()).collect::<Vec<_>>().join("、"), ..first.clone() }.describe(),
        [] => String::new(),
    }
}
