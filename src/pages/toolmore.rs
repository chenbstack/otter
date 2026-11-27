//! 工具箱新增的六个工具页:主机名与 hosts/DNS、内核参数、SSH 公钥、SSH 安全设置、用户与 sudo、fail2ban。
//!
//! 逻辑(脚本、解析、防锁死检查)在 `crate::toolbox::{host,sysctl,ssh,users,fail2ban}`,这里只管界面和确认框。
//! 要改服务器的操作都先弹确认框(`DockAsk` + `Action::Sys`),完成后重读该工具的状态。设计见 `docs/modules/toolbox.md`。

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};

use super::super::*;
use super::daemonpage::{labeled, switch};
use super::dockerpages::{DockAsk, head, row, small, table_flow};
use super::syspages::{chip, message, reading};
use super::toolboxpage::{Tool, card, mono_text, section};
use crate::docker::Action as DockAction;
use crate::sysops::{Guard, Job, Kind};
use crate::toolbox::fail2ban::{self as f2b, F2bInfo};
use crate::toolbox::host::{self, DnsMode, HostEntry, HostInfo};
use crate::toolbox::ssh::{self, AuthKey, SshInfo};
use crate::toolbox::sysctl::{self, Control, SysctlInfo};
use crate::toolbox::users::{self, Op, Pw, UsersInfo};

// 输入框编号
const I_HOSTNAME: usize = 0;
const I_HIP: usize = 1;
const I_HNAMES: usize = 2;
const I_HNOTE: usize = 3;
const I_DNS1: usize = 4;
const I_DNS2: usize = 5;
const I_SSHPORT: usize = 6;
const I_USER: usize = 7;
const I_UPW: usize = 8;
const I_PWNEW: usize = 9;
const I_IGN: usize = 10;
const INPUTS: [&str; 11] = ["新主机名,如 web-01", "IP,如 10.0.0.5", "域名,多个用空格分开", "备注(可选)", "主 DNS,如 1.1.1.1", "备用 DNS(可选)", "新端口,如 2222", "用户名,如 deploy", "密码(可留空,用公钥登录)", "新密码", "IP 或网段,如 10.0.0.0/8"];
// 多行输入编号
const A_KEY: usize = 0;
const A_UKEY: usize = 1;

#[derive(Default)]
pub(crate) struct MoreUi {
    pub(super) host: Option<Result<HostInfo, String>>,
    pub(super) sysctl: Option<Result<SysctlInfo, String>>,
    pub(super) ssh: Option<Result<SshInfo, String>>,
    pub(super) users: Option<Result<UsersInfo, String>>,
    pub(super) f2b: Option<Result<F2bInfo, String>>,
    inputs: Vec<Entity<InputState>>,
    areas: Vec<Entity<TextareaState>>,
    /// 正在编辑的 hosts 记录:(行号, 原文)
    host_edit: Option<(usize, String)>,
    /// 公钥页选中的用户
    key_user: String,
    new_sudo: bool,
    del_home: bool,
    /// 正在改密码的用户
    pw_target: Option<String>,
}

impl Tool {
    /// 读取这个工具状态的脚本和是否要 root
    pub(super) fn read_script(self) -> Option<(String, bool)> {
        Some(match self {
            Tool::Host => (host::HOST_SCRIPT.to_string(), false),
            Tool::Sysctl => (sysctl::read_script(), false),
            Tool::Ssh | Tool::Keys => (ssh::SSH_SCRIPT.to_string(), true),
            Tool::Users => (users::USERS_SCRIPT.to_string(), true),
            Tool::Fail2ban => (f2b::F2B_SCRIPT.to_string(), true),
            _ => return None,
        })
    }
}

fn srow(title: &'static str, desc: impl Into<SharedString>, ctl: impl IntoElement) -> Div {
    div()
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(16.0))
        .py(px(10.0))
        .border_b_1()
        .border_color(c::border())
        .child(div().flex_1().min_w_0().flex().flex_col().gap(px(2.0)).child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child(title)).child(small(desc, c::ghost())))
        .child(div().flex_shrink_0().flex().flex_row().items_center().gap(px(6.0)).child(ctl))
}

fn input_box(e: &Entity<InputState>) -> Div {
    div().h(px(32.0)).flex_1().min_w_0().flex().flex_row().items_center().rounded(px(t::RADIUS_MD)).bg(c::field()).border_1().border_color(c::field_border()).text_size(px(t::TYPE_CONTROL)).child(div().flex_1().min_w_0().child(Input::new(e).appearance(false).with_size(px(12.0 / 0.875))))
}

fn area_box(e: &Entity<TextareaState>, h: f32) -> Div {
    div().h(px(h)).rounded(px(t::RADIUS_MD)).bg(c::field()).border_1().border_color(c::field_border()).text_size(px(t::TYPE_CONTROL)).child(Textarea::new(e).appearance(false).with_size(px(12.0 / 0.875)).h_full())
}

impl Home {
    fn more_make(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dock.sys.tool.more.inputs.is_empty() {
            return;
        }
        let inputs: Vec<Entity<InputState>> = INPUTS
            .iter()
            .enumerate()
            .map(|(i, ph)| {
                let masked = i == I_UPW || i == I_PWNEW;
                let e = cx.new(|cx| InputState::new(window, cx).placeholder(*ph).masked(masked));
                cx.observe(&e, |_, _, cx| cx.notify()).detach();
                e
            })
            .collect();
        let areas: Vec<Entity<TextareaState>> = ["粘贴公钥(一行,通常是 id_ed25519.pub 的内容)", "公钥(可选,一行)"]
            .iter()
            .map(|ph| {
                let e = cx.new(|cx| TextareaState::new(window, cx).placeholder(*ph).rows(3));
                cx.observe(&e, |_, _, cx| cx.notify()).detach();
                e
            })
            .collect();
        let m = &mut self.dock.sys.tool.more;
        m.inputs = inputs;
        m.areas = areas;
    }

    pub(super) fn more_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.more_make(window, cx);
    }

    fn mi(&self, i: usize) -> Option<&Entity<InputState>> {
        self.dock.sys.tool.more.inputs.get(i)
    }

    fn mtext(&self, i: usize, cx: &App) -> String {
        self.mi(i).map(|e| e.read(cx).value().trim().to_string()).unwrap_or_default()
    }

    fn mset(&self, i: usize, v: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(e) = self.mi(i).cloned() {
            e.update(cx, |s, cx| s.set_value(v, window, cx));
        }
    }

    fn atext(&self, i: usize, cx: &App) -> String {
        self.dock.sys.tool.more.areas.get(i).map(|e| e.read(cx).value().trim().to_string()).unwrap_or_default()
    }

    fn aclear(&self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(e) = self.dock.sys.tool.more.areas.get(i).cloned() {
            e.update(cx, |s, cx| s.set_value("", window, cx));
        }
    }

    fn login_user(&self) -> String {
        crate::ctl::split_user_host(&self.node().user_host).0
    }

    pub(super) fn more_loaded(&mut self, kind: Kind, res: Result<String, String>) {
        let m = &mut self.dock.sys.tool.more;
        fn keep<T>(slot: &mut Option<Result<T, String>>, r: Result<T, String>) {
            if r.is_ok() || !matches!(slot, Some(Ok(_))) {
                *slot = Some(r);
            }
        }
        match kind {
            Kind::ToolHost => keep(&mut m.host, res.and_then(|s| host::parse_host(&s))),
            Kind::ToolSysctl => keep(&mut m.sysctl, res.and_then(|s| sysctl::parse_sysctl(&s))),
            Kind::ToolSsh => keep(&mut m.ssh, res.and_then(|s| ssh::parse_ssh(&s))),
            Kind::ToolUsers => keep(&mut m.users, res.and_then(|s| users::parse_users(&s))),
            Kind::ToolF2b => keep(&mut m.f2b, res.and_then(|s| f2b::parse_f2b(&s))),
            _ => {}
        }
    }

    pub(super) fn more_reset_forms(&mut self) {
        let m = &mut self.dock.sys.tool.more;
        m.host_edit = None;
        m.pw_target = None;
    }

    /// 要在服务器上改东西:先弹确认框;`input` 经 stdin 交给脚本(密码、公钥不进命令行)
    #[allow(clippy::too_many_arguments)]
    fn more_ask(&mut self, tool: Tool, title: &'static str, body: String, verb: &'static str, name: String, script: Result<String, String>, input: Option<String>, calm: bool, secs: u64, guard: Option<Guard>, list: Vec<String>, cx: &mut Context<Self>) {
        let script = match script {
            Ok(s) => s,
            Err(e) => return self.tool_notice_pub(e, cx),
        };
        let reload = tool.read_script().map(|(s, root)| (tool.kind(), s, root));
        let job = Job { label: verb.into(), target: name.clone(), script, root: true, input, reload, guard, secs, calm };
        let mut l = vec![body];
        l.extend(list);
        self.ask(DockAsk { title, tile: tool.icon(), name, detail: tool.title(), body: "", label: verb, action: DockAction::Sys(job), list: l }, cx);
    }

    // ================= 主机名与 hosts/DNS =================

    fn host_submit_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (ip, names, note) = (self.mtext(I_HIP, cx), self.mtext(I_HNAMES, cx), self.mtext(I_HNOTE, cx));
        let line = match host::hosts_line(&ip, &names, &note) {
            Ok(l) => l,
            Err(e) => return self.tool_notice_pub(e, cx),
        };
        match self.dock.sys.tool.more.host_edit.clone() {
            Some((n, old)) => {
                if line == old {
                    return self.tool_notice_pub("没有改动", cx);
                }
                self.more_ask(Tool::Host, "修改 hosts 记录", format!("把 /etc/hosts 的这一行:{old}"), "修改记录", ip.clone(), Ok(host::hosts_edit_script(n, &old, &line)), None, true, 0, None, vec![format!("改成:{line}"), "改之前备份为 /etc/hosts.otter-bak".into()], cx);
            }
            None => {
                self.more_ask(Tool::Host, "添加 hosts 记录", format!("在 /etc/hosts 末尾添加:{line}"), "添加记录", ip.clone(), Ok(host::hosts_add_script(&line)), None, true, 0, None, vec!["改之前备份为 /etc/hosts.otter-bak".into()], cx);
            }
        }
        self.host_form_reset(window, cx);
    }

    fn host_form_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dock.sys.tool.more.host_edit = None;
        for i in [I_HIP, I_HNAMES, I_HNOTE] {
            self.mset(i, "", window, cx);
        }
        cx.notify();
    }

    fn host_entry_row(&self, i: usize, e: &HostEntry, cx: &mut Context<Self>) -> Stateful<Div> {
        let editing = self.dock.sys.tool.more.host_edit.as_ref().is_some_and(|(n, _)| *n == e.line);
        let e1 = e.clone();
        let e2 = e.clone();
        let acts = div()
            .w(px(130.0))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .justify_end()
            .gap(px(2.0))
            .child(chip(("hs-edit", i), "编辑", editing).on_click(cx.listener(move |this, _, window, cx| {
                this.dock.sys.tool.more.host_edit = Some((e1.line, e1.raw.clone()));
                this.mset(I_HIP, &e1.ip, window, cx);
                this.mset(I_HNAMES, &e1.names.join(" "), window, cx);
                this.mset(I_HNOTE, &e1.comment, window, cx);
                cx.notify();
            })))
            .child(chip(("hs-del", i), "删除", false).on_click(cx.listener(move |this, _, _, cx| {
                let sys = e2.names.iter().any(|n| n == "localhost") || e2.ip.starts_with("127.") || e2.ip == "::1";
                let mut list = vec!["删除前备份为 /etc/hosts.otter-bak".to_string()];
                if sys {
                    list.push("这是本机回环地址的记录,很多程序依赖它,删掉可能让本机服务连不上自己".into());
                }
                this.more_ask(Tool::Host, "删除 hosts 记录", format!("删除 /etc/hosts 里的这一行:{}", e2.raw.trim()), "删除记录", e2.ip.clone(), Ok(host::hosts_delete_script(e2.line, &e2.raw)), None, !sys, 0, None, list, cx);
            })));
        row(("hs-r", i))
            .child(div().w(px(150.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).child(SharedString::from(e.ip.clone())))
            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::secondary()).child(SharedString::from(e.names.join(" "))))
            .child(div().w(px(200.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(e.comment.clone())))
            .child(acts)
    }

    pub(crate) fn host_tool(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let info = match &self.dock.sys.tool.more.host {
            None => return reading("读取主机名和 hosts"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了主机信息", e.clone()),
            Some(Ok(i)) => i.clone(),
        };
        let (Some(name_in), Some(dns1), Some(dns2), Some(ip_in), Some(names_in), Some(note_in)) = (self.mi(I_HOSTNAME).cloned(), self.mi(I_DNS1).cloned(), self.mi(I_DNS2).cloned(), self.mi(I_HIP).cloned(), self.mi(I_HNAMES).cloned(), self.mi(I_HNOTE).cloned()) else { return reading("准备") };
        let mut col = div().flex().flex_col().gap(px(12.0));

        // 主机名
        let cur = info.hostname.clone();
        col = col.child(
            card()
                .child(section("主机名"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_end()
                        .gap(px(12.0))
                        .child(labeled("当前", "", div().h(px(32.0)).flex().items_center().child(mono_text(if cur.is_empty() { "—".to_string() } else { cur.clone() }))))
                        .child(div().flex_1().min_w_0().child(labeled("改成", "", input_box(&name_in))))
                        .child(button("hn-set", IconName::Check, "修改").on_click(cx.listener(move |this, _, _, cx| {
                            let n = this.mtext(I_HOSTNAME, cx);
                            if n == cur {
                                return this.tool_notice_pub("和现在的主机名一样", cx);
                            }
                            this.more_ask(Tool::Host, "修改主机名", format!("主机名改成 {n},立即生效、重启后保留;/etc/hosts 里的 127.0.1.1 行同步改"), "修改主机名", n.clone(), host::set_hostname_script(&n), None, true, 0, None, vec!["已经打开的终端里提示符要重新登录才会变".into()], cx);
                        }))),
                )
                .child(small("小写字母、数字和 -,可以用 . 分隔成域名形式", c::ghost())),
        );

        // DNS
        let dns_now = if info.dns.is_empty() { "—".to_string() } else { info.dns.join("  ") };
        let mut dns = card().child(section("DNS 服务器")).child(div().flex().flex_row().items_baseline().gap(px(10.0)).child(small("当前生效", c::ghost())).child(mono_text(dns_now)));
        match &info.mode {
            DnsMode::Readonly(why) => {
                dns = dns.child(small(format!("这里不能改:{why}"), c::warn()));
            }
            mode => {
                let mode_c = mode.clone();
                let resolved = *mode == DnsMode::Resolved;
                let mut form = div().flex().flex_row().items_end().gap(px(12.0)).child(div().flex_1().min_w_0().child(labeled("主 DNS", "", input_box(&dns1)))).child(div().flex_1().min_w_0().child(labeled("备用 DNS", "", input_box(&dns2)))).child(button("dns-set", IconName::Check, "应用").on_click(cx.listener(move |this, _, _, cx| {
                    let list: Vec<String> = [this.mtext(I_DNS1, cx), this.mtext(I_DNS2, cx)].into_iter().filter(|s| !s.is_empty()).collect();
                    let body = format!("DNS 服务器改成 {}", list.join("、"));
                    let note = if resolved { "写入 /etc/systemd/resolved.conf.d/99-otter.conf 并重启 systemd-resolved;和 DHCP 下发的 DNS 并存".to_string() } else { "改写 /etc/resolv.conf 的 nameserver 行(原文件备份为 resolv.conf.otter-bak);网络服务重启后可能被还原".to_string() };
                    this.more_ask(Tool::Host, "修改 DNS", body, "修改 DNS", list.join(" "), host::dns_set_script(&mode_c, &list), None, true, 0, None, vec![note, "改完会读回确认生效,没生效自动还原".into()], cx);
                })));
                if resolved && !info.dns_otter.is_empty() {
                    form = form.child(button("dns-reset", IconName::RefreshCw, "恢复默认").on_click(cx.listener(|this, _, _, cx| {
                        this.more_ask(Tool::Host, "恢复默认 DNS", "删除 Otter 设置的 DNS,恢复成网络下发的默认值".into(), "恢复默认 DNS", "DNS".into(), Ok(host::dns_reset_script()), None, true, 0, None, Vec::new(), cx);
                    })));
                }
                dns = dns.child(form).child(small(if resolved { "由 systemd-resolved 管理" } else { "直接改 /etc/resolv.conf" }, c::ghost()));
            }
        }
        col = col.child(dns);

        // hosts
        let editing = self.dock.sys.tool.more.host_edit.is_some();
        col = col.child(div().flex().flex_row().items_baseline().gap(px(8.0)).child(section("hosts 记录")).child(small(format!("{} 条", info.entries.len()), c::ghost())));
        let header = div().child(head("IP").w(px(150.0))).child(head("域名").flex_1().min_w_0()).child(head("备注").w(px(200.0))).child(div().w(px(130.0)));
        let rows: Vec<Stateful<Div>> = info.entries.iter().enumerate().map(|(i, e)| self.host_entry_row(i, e, cx)).collect();
        col = col.child(table_flow(header, rows));
        let form = div()
            .flex()
            .flex_row()
            .items_end()
            .gap(px(12.0))
            .child(div().w(px(200.0)).child(labeled("IP", "", input_box(&ip_in))))
            .child(div().flex_1().min_w_0().child(labeled("域名", "", input_box(&names_in))))
            .child(div().w(px(200.0)).child(labeled("备注", "", input_box(&note_in))))
            .child(button("hs-submit", IconName::Check, if editing { "保存" } else { "添加" }).on_click(cx.listener(|this, _, window, cx| this.host_submit_entry(window, cx))))
            .children(editing.then(|| button("hs-cancel", IconName::X, "取消").on_click(cx.listener(|this, _, window, cx| this.host_form_reset(window, cx)))));
        col = col.child(card().child(section(if editing { "编辑记录" } else { "添加记录" })).child(form).child(small("每一行都能改,包括系统自带的 localhost;改之前先核对原文,文件在你打开之后被别人改过会拒绝", c::ghost())));
        col.into_any_element()
    }

    // ================= 内核参数 =================

    pub(crate) fn sysctl_tool(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let info = match &self.dock.sys.tool.more.sysctl {
            None => return reading("读取内核参数"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了内核参数", e.clone()),
            Some(Ok(i)) => i.clone(),
        };
        let mut c1 = card().gap(px(0.0)).child(div().pb(px(6.0)).child(section("常用内核参数")));
        for (i, it) in sysctl::ITEMS.iter().enumerate() {
            let cur = info.value(it.key).to_string();
            let saved = info.saved(it.key).is_some() || (it.control == Control::Bbr && info.saved("net.core.default_qdisc").is_some());
            let mut ctl = div().flex().flex_row().items_center().gap(px(8.0));
            match it.control {
                Control::Switch { on, off } => {
                    let is_on = cur == on;
                    let mut sw = div().id(("sy-sw", i)).flex_shrink_0().child(switch("sy-s", is_on)).cursor_pointer();
                    let (key, target) = (it.key, if is_on { off } else { on });
                    sw = sw.on_click(cx.listener(move |this, _, _, cx| this.sysctl_apply(key, target, cx)));
                    ctl = ctl.child(sw);
                }
                Control::Presets(list) => {
                    for (j, v) in list.iter().enumerate() {
                        let (key, v) = (it.key, *v);
                        ctl = ctl.child(chip(("sy-p", i * 10 + j), v, cur == v).on_click(cx.listener(move |this, _, _, cx| {
                            if this.dock.sys.tool.more.sysctl.as_ref().and_then(|r| r.as_ref().ok()).map(|i| i.value(key) == v).unwrap_or(false) {
                                return;
                            }
                            this.sysctl_apply(key, v, cx)
                        })));
                    }
                }
                Control::Bbr => {
                    let is_on = cur == "bbr";
                    let mut sw = div().id(("sy-sw", i)).flex_shrink_0().child(switch("sy-s", is_on));
                    if info.bbr || is_on {
                        let target = if is_on { "cubic" } else { "bbr" };
                        sw = sw.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.sysctl_apply("net.ipv4.tcp_congestion_control", target, cx)));
                    } else {
                        sw = sw.opacity(0.45);
                    }
                    ctl = ctl.child(small(if cur.is_empty() { "—".to_string() } else { cur.clone() }, c::ghost())).child(sw);
                }
            }
            if saved {
                let key = it.key;
                ctl = ctl.child(chip(("sy-rs", i), "恢复默认", false).on_click(cx.listener(move |this, _, _, cx| {
                    this.more_ask(Tool::Sysctl, "恢复默认", format!("删除 Otter 在 /etc/sysctl.d/99-otter.conf 里写的 {key}"), "恢复默认", key.to_string(), sysctl::reset_script(key), None, true, 0, None, vec!["当前运行值不变,重启后恢复系统默认".into()], cx);
                })));
            }
            let mut desc = it.desc.to_string();
            if it.control == Control::Bbr && !info.bbr {
                desc.push_str("(这台服务器的内核不支持)");
            }
            let mut r = srow(it.title, desc, ctl);
            if saved {
                r = r.child(div().flex_shrink_0().child(crate::ui::tag("Otter 设置").color(c::accent())));
            }
            c1 = c1.child(r);
        }
        div().flex().flex_col().gap(px(12.0)).child(c1).child(small("改动立即生效,并写进 /etc/sysctl.d/99-otter.conf,重启后保留。swappiness 在 Swap 工具里设置", c::ghost())).into_any_element()
    }

    fn sysctl_apply(&mut self, key: &'static str, val: &'static str, cx: &mut Context<Self>) {
        let Some(it) = sysctl::item(key) else { return };
        let mut list = vec![format!("立即生效,写进 /etc/sysctl.d/99-otter.conf,重启后保留")];
        if !it.warn.is_empty() {
            list.push(it.warn.to_string());
        }
        let shown = if it.control == Control::Bbr { format!("{} = {val}(同时设置 net.core.default_qdisc = fq)", key) } else { format!("{key} = {val}") };
        self.more_ask(Tool::Sysctl, "修改内核参数", shown, "修改内核参数", it.title.to_string(), sysctl::set_script(key, val), None, it.warn.is_empty(), 0, None, list, cx);
    }

    // ================= SSH 设置 =================

    fn ssh_apply(&mut self, key: &'static str, val: String, title: &'static str, desc: String, cx: &mut Context<Self>) {
        let Some(Ok(info)) = self.dock.sys.tool.more.ssh.clone() else { return };
        let login = self.login_user();
        if let Err(e) = ssh::check_change(&info, &login, key, &val) {
            return self.tool_notice_pub(e, cx);
        }
        let script = ssh::set_script(key, &val);
        let (_, host, port) = crate::ctl::split_user_host(&self.node().user_host);
        // 改端口后,要连新端口才算成功
        let gport = if key == "Port" { val.parse().unwrap_or(port) } else { port };
        let guard = Some(Guard { host, port: gport, secs: ssh::GUARD_SECS, cancel: ssh::guard_cancel(), what: "SSH 设置".into() });
        let mut list = vec![format!("改完后服务器会等 {} 秒:本机能新建 SSH 连接就保留;连不上会自动还原,不会把你锁在外面", ssh::GUARD_SECS), "已经打开的连接不受影响".to_string()];
        if key == "Port" {
            list.push(format!("请先确认防火墙和云厂商安全组已放行 TCP {val};连接设置里的端口也要改成新的"));
        }
        self.more_ask(Tool::Ssh, title, desc, "修改 SSH 设置", key.to_string(), script, None, false, 120, guard, list, cx);
    }

    pub(crate) fn ssh_tool(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let info = match &self.dock.sys.tool.more.ssh {
            None => return reading("读取 SSH 设置"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了 SSH 设置", e.clone()),
            Some(Ok(i)) => i.clone(),
        };
        let Some(port_in) = self.mi(I_SSHPORT).cloned() else { return reading("准备") };
        let mark = |k: &str| info.otter_value(k).is_some();
        let tag_if = |r: Div, k: &str| if mark(k) { r.child(div().flex_shrink_0().child(crate::ui::tag("Otter 设置").color(c::accent()))) } else { r };
        let mut c1 = card().gap(px(0.0)).child(div().pb(px(6.0)).child(section("SSH 登录安全")));

        // 端口
        let cur_port = info.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("、");
        let ctl = div().flex().flex_row().items_center().gap(px(8.0)).child(mono_text(cur_port.clone())).child(div().w(px(130.0)).child(input_box(&port_in))).child(chip(("ssh-port", 0), "修改", false).on_click(cx.listener(|this, _, _, cx| {
            let v = this.mtext(I_SSHPORT, cx);
            this.ssh_apply("Port", v.clone(), "修改 SSH 端口", format!("sshd 的监听端口改成 {v}"), cx);
        })));
        c1 = c1.child(tag_if(srow("端口", "改掉默认的 22 能挡掉大部分自动扫描", ctl), "Port"));

        // root 登录
        let mut ctl = div().flex().flex_row().items_center().gap(px(4.0));
        for (i, (v, label)) in [("yes", "允许"), ("prohibit-password", "仅密钥"), ("no", "禁止")].into_iter().enumerate() {
            ctl = ctl.child(chip(("ssh-root", i), label, info.root_login == v).on_click(cx.listener(move |this, _, _, cx| {
                if this.dock.sys.tool.more.ssh.as_ref().and_then(|r| r.as_ref().ok()).is_some_and(|i| i.root_login == v) {
                    return;
                }
                this.ssh_apply("PermitRootLogin", v.to_string(), "修改 root 登录", format!("root 登录方式改成「{label}」"), cx);
            })));
        }
        c1 = c1.child(tag_if(srow("root 登录", "允许 = 密码或密钥;仅密钥 = 只能用公钥;禁止 = 不能直接登录 root", ctl), "PermitRootLogin"));

        // 密码 / 公钥登录开关
        for (id, key, title, desc, on) in [("ssh-pw", "PasswordAuthentication", "密码登录", "关掉后只能用公钥登录,能挡住暴力破解。关之前要先有公钥", info.password), ("ssh-pk", "PubkeyAuthentication", "公钥登录", "一般不要关", info.pubkey)] {
            let sw = div().id(id).flex_shrink_0().cursor_pointer().child(switch(id, on)).on_click(cx.listener(move |this, _, _, cx| {
                let target = if on { "no" } else { "yes" };
                this.ssh_apply(key, target.to_string(), if key == "PasswordAuthentication" { "修改密码登录" } else { "修改公钥登录" }, format!("{title}{}", if on { "关闭" } else { "开启" }), cx);
            }));
            c1 = c1.child(tag_if(srow(title, desc, sw), key));
        }

        // 数值项
        let mut ctl = div().flex().flex_row().items_center().gap(px(4.0));
        for (i, n) in [3u32, 4, 6, 10].into_iter().enumerate() {
            ctl = ctl.child(chip(("ssh-try", i), n.to_string(), info.max_tries == Some(n)).on_click(cx.listener(move |this, _, _, cx| this.ssh_apply("MaxAuthTries", n.to_string(), "修改最大尝试次数", format!("单次连接最多尝试 {n} 次认证"), cx))));
        }
        c1 = c1.child(tag_if(srow("最多尝试次数", "单次连接里认证失败几次就断开", ctl), "MaxAuthTries"));
        let mut ctl = div().flex().flex_row().items_center().gap(px(4.0));
        for (i, (n, label)) in [(0u32, "关闭"), (30, "30 秒"), (60, "60 秒"), (120, "120 秒")].into_iter().enumerate() {
            ctl = ctl.child(chip(("ssh-alive", i), label, info.alive_interval == Some(n)).on_click(cx.listener(move |this, _, _, cx| this.ssh_apply("ClientAliveInterval", n.to_string(), "修改保活间隔", if n == 0 { "关闭服务器端的保活探测".to_string() } else { format!("每 {n} 秒向客户端发一次保活探测,连不上会断开") }, cx))));
        }
        c1 = c1.child(tag_if(srow("保活探测", "服务器隔多久探测一次空闲连接,防止被 NAT 悄悄丢掉", ctl), "ClientAliveInterval"));

        let how = if info.include { "设置写进 /etc/ssh/sshd_config.d/00-otter.conf(排在其它配置前面才会生效)".to_string() } else { "这台服务器的 sshd_config 没有引入 sshd_config.d,设置写在主配置开头,行尾带 # otter".to_string() };
        let socket = if info.socket { "。这台用 ssh.socket 监听,改端口会同时更新 socket 单元" } else { "" };
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(c1)
            .child(small(format!("{how}{socket}。每次修改前先校验配置(sshd -t)并备份;改完服务器排一个 {} 秒的自动回滚,本机确认能新建连接才取消", ssh::GUARD_SECS), c::ghost()))
            .into_any_element()
    }

    // ================= SSH 公钥 =================

    fn key_row(&self, i: usize, k: &AuthKey, cx: &mut Context<Self>) -> Stateful<Div> {
        let key = k.clone();
        row(("key-r", i))
            .child(div().w(px(90.0)).flex_shrink_0().child(crate::ui::tag(k.alg.trim_matches(|c| c == '(' || c == ')').to_string()).color(c::muted())))
            .child(div().w(px(180.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).child(SharedString::from(if k.comment.is_empty() { "(无备注)".to_string() } else { k.comment.clone() })))
            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::ghost()).child(SharedString::from(k.fp.clone())))
            .children(k.options.then(|| div().flex_shrink_0().child(crate::ui::tag("带限制").color(c::warn()))))
            .child(div().w(px(70.0)).flex_shrink_0().flex().justify_end().child(chip(("key-del", i), "删除", false).on_click(cx.listener(move |this, _, _, cx| {
                let Some(Ok(info)) = this.dock.sys.tool.more.ssh.clone() else { return };
                let login = this.login_user();
                if let Err(e) = ssh::check_delete_key(&info, &login, &key) {
                    return this.tool_notice_pub(e, cx);
                }
                let mut list = vec![format!("从 {} 的 authorized_keys 里删掉这一行", key.user), "删除前备份为 authorized_keys.otter-bak".to_string()];
                if info.password {
                    list.push("密码登录是开着的,删公钥后仍可用密码登录".into());
                }
                let name = if key.comment.is_empty() { key.fp.clone() } else { key.comment.clone() };
                this.more_ask(Tool::Keys, "删除公钥", format!("删除 {} 的公钥「{name}」,拿着这把私钥的人不能再登录", key.user), "删除公钥", name, ssh::delete_key_script(&key.user, &key.raw), None, false, 0, None, list, cx);
            }))))
    }

    pub(crate) fn keys_tool(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let info = match &self.dock.sys.tool.more.ssh {
            None => return reading("读取 SSH 公钥"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了 SSH 公钥", e.clone()),
            Some(Ok(i)) => i.clone(),
        };
        let login = self.login_user();
        let sel = {
            let cur = &self.dock.sys.tool.more.key_user;
            if info.user(cur).is_some() { cur.clone() } else if info.user(&login).is_some() { login.clone() } else { info.users.first().map(|u| u.name.clone()).unwrap_or_default() }
        };
        let Some(user) = info.user(&sel).cloned() else { return message(IconName::TriangleAlert, "没有可登录的用户", "服务器上找不到 root 或普通登录用户".into()) };
        let Some(area) = self.dock.sys.tool.more.areas.get(A_KEY).cloned() else { return reading("准备") };
        let mut chips = div().flex().flex_row().flex_wrap().gap(px(4.0));
        for (i, u) in info.users.iter().enumerate() {
            let n = u.name.clone();
            chips = chips.child(chip(("key-user", i), format!("{}({})", u.name, u.keys.len()), u.name == sel).on_click(cx.listener(move |this, _, _, cx| {
                this.dock.sys.tool.more.key_user = n.clone();
                cx.notify();
            })));
        }
        let mut col = div().flex().flex_col().gap(px(12.0)).child(div().flex().flex_row().items_center().gap(px(10.0)).child(small("用户", c::ghost())).child(chips));
        col = col.child(div().flex().flex_row().items_baseline().gap(px(8.0)).child(section("授权的公钥")).child(small(format!("{} 把 · {}/.ssh/authorized_keys", user.keys.len(), user.home), c::ghost())));
        if user.keys.is_empty() {
            col = col.child(card().items_center().py(px(24.0)).gap(px(6.0)).child(icon(IconName::KeyRound, 22.0, c::ghost())).child(div().text_size(px(t::TYPE_BODY)).child(format!("{} 还没有授权任何公钥", user.name))).child(small("在下面粘贴公钥添加后,就能用对应的私钥免密登录", c::ghost())));
        } else {
            let header = div().child(head("类型").w(px(90.0))).child(head("备注").w(px(180.0))).child(head("指纹").flex_1().min_w_0()).child(div().w(px(70.0)));
            let rows: Vec<Stateful<Div>> = user.keys.iter().enumerate().map(|(i, k)| self.key_row(i, k, cx)).collect();
            col = col.child(table_flow(header, rows));
        }
        if !info.password {
            col = col.child(small("密码登录已关闭:删除当前登录用户的最后一把公钥会被拒绝", c::ghost()));
        }
        let uname = user.name.clone();
        col = col.child(
            card()
                .child(section("添加公钥"))
                .child(area_box(&area, 76.0))
                .child(div().flex().flex_row().items_center().justify_between().child(small(format!("添加到 {}。支持 ssh-ed25519、ssh-rsa、ecdsa 等,一次一把", user.name), c::ghost())).child(button("key-add", IconName::Plus, "添加").on_click(cx.listener(move |this, _, window, cx| {
                    let text = this.atext(A_KEY, cx);
                    let key = match ssh::check_pubkey(&text) {
                        Ok(k) => k,
                        Err(e) => return this.tool_notice_pub(e, cx),
                    };
                    let comment = key.split_whitespace().nth(2).unwrap_or("(无备注)").to_string();
                    this.more_ask(Tool::Keys, "添加公钥", format!("给 {uname} 添加公钥「{comment}」,持有对应私钥的人可以免密登录"), "添加公钥", comment, ssh::add_key_script(&uname), Some(format!("{key}\n")), true, 0, None, vec!["目录 ~/.ssh 权限 700,authorized_keys 权限 600".into()], cx);
                    this.aclear(A_KEY, window, cx);
                })))),
        );
        col.into_any_element()
    }

    // ================= 用户与 sudo =================

    fn user_op(&mut self, name: String, op: Op, cx: &mut Context<Self>) {
        let Some(Ok(info)) = self.dock.sys.tool.more.users.clone() else { return };
        let login = self.login_user();
        if let Err(e) = users::check_op(&info, &login, &name, op) {
            return self.tool_notice_pub(e, cx);
        }
        let group = info.admin_group.clone().unwrap_or_default();
        let del_home = self.dock.sys.tool.more.del_home;
        let (title, body, verb, script, calm, list): (&'static str, String, &'static str, Result<String, String>, bool, Vec<String>) = match op {
            Op::Lock => ("锁定用户", format!("锁定 {name}:密码和公钥都不能再登录,正在登录的会被踢下线"), "锁定用户", users::lock_script(&name), true, vec!["之后可以解锁".into()]),
            Op::Unlock => ("解锁用户", format!("解锁 {name},恢复登录"), "解锁用户", users::unlock_script(&name), true, Vec::new()),
            Op::SudoOn => ("加入管理员组", format!("把 {name} 加入 {group} 组,可以用 sudo 执行管理员命令"), "加入管理员组", users::sudo_script(&name, &group, true), false, vec!["管理员可以改动整台服务器,请只给信任的人".into()]),
            Op::SudoOff => ("移出管理员组", format!("把 {name} 从 {group} 组移出,不能再用 sudo"), "移出管理员组", users::sudo_script(&name, &group, false), true, Vec::new()),
            Op::Delete => ("删除用户", format!("删除用户 {name},结束它所有的进程"), "删除用户", users::delete_script(&name, del_home), false, vec![if del_home { format!("同时删除家目录,里面的文件无法恢复") } else { "保留家目录和里面的文件".to_string() }]),
            Op::Password => return,
        };
        self.more_ask(Tool::Users, title, body, verb, name, script, None, calm, 0, None, list, cx);
    }

    fn user_row(&self, i: usize, u: &users::UserRow, info: &UsersInfo, login: &str, cx: &mut Context<Self>) -> Stateful<Div> {
        let admin = info.is_admin(u);
        let mut tags = div().flex_1().min_w_0().flex().flex_row().flex_wrap().items_center().gap(px(6.0));
        if admin {
            tags = tags.child(crate::ui::tag(if u.uid == 0 { "root" } else { "管理员" }).color(c::accent()));
        }
        if u.name == login {
            tags = tags.child(crate::ui::tag("当前连接").color(c::online()));
        }
        if u.locked() {
            tags = tags.child(crate::ui::tag("已锁定").color(c::danger()));
        }
        match u.pw {
            Pw::Empty => tags = tags.child(crate::ui::tag("空密码").color(c::danger())),
            Pw::None => tags = tags.child(crate::ui::tag("仅公钥").color(c::muted())),
            _ => {}
        }
        if !u.has_shell() {
            tags = tags.child(crate::ui::tag("不能登录").color(c::ghost()));
        }
        let name = u.name.clone();
        let (n1, n2, n3, n4) = (name.clone(), name.clone(), name.clone(), name.clone());
        let locked = u.locked();
        let is_root = u.uid == 0;
        let mut acts = div().flex_shrink_0().flex().flex_row().justify_end().gap(px(2.0)).child(chip(("us-pw", i), "改密码", false).on_click(cx.listener(move |this, _, window, cx| {
            this.dock.sys.tool.more.pw_target = Some(n1.clone());
            this.mset(I_PWNEW, "", window, cx);
            cx.notify();
        })));
        if !is_root {
            acts = acts
                .child(chip(("us-lock", i), if locked { "解锁" } else { "锁定" }, false).on_click(cx.listener(move |this, _, _, cx| this.user_op(n2.clone(), if locked { Op::Unlock } else { Op::Lock }, cx))))
                .child(chip(("us-sudo", i), if admin { "移出管理员" } else { "设为管理员" }, false).on_click(cx.listener(move |this, _, _, cx| this.user_op(n3.clone(), if admin { Op::SudoOff } else { Op::SudoOn }, cx))))
                .child(chip(("us-del", i), "删除", false).on_click(cx.listener(move |this, _, _, cx| this.user_op(n4.clone(), Op::Delete, cx))));
        }
        row(("us-r", i))
            .child(div().w(px(130.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(u.name.clone())))
            .child(div().w(px(60.0)).flex_shrink_0().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(u.uid.to_string()))
            .child(div().w(px(150.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::ghost()).child(SharedString::from(u.home.clone())))
            .child(tags)
            .child(acts)
    }

    pub(crate) fn users_tool(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let info = match &self.dock.sys.tool.more.users {
            None => return reading("读取用户列表"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了用户列表", e.clone()),
            Some(Ok(i)) => i.clone(),
        };
        let (Some(name_in), Some(pw_in), Some(area), Some(pwnew)) = (self.mi(I_USER).cloned(), self.mi(I_UPW).cloned(), self.dock.sys.tool.more.areas.get(A_UKEY).cloned(), self.mi(I_PWNEW).cloned()) else { return reading("准备") };
        let login = self.login_user();
        let (new_sudo, del_home) = (self.dock.sys.tool.more.new_sudo, self.dock.sys.tool.more.del_home);
        let mut col = div().flex().flex_col().gap(px(12.0));
        col = col.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(div().flex().flex_row().items_baseline().gap(px(8.0)).child(section("用户")).child(small(format!("{} 个 · 管理员组 {}", info.users.len(), info.admin_group.as_deref().unwrap_or("无")), c::ghost())))
                .child(chip(("us-delhome", 0), if del_home { "删除用户时同时删家目录:开" } else { "删除用户时同时删家目录:关" }, del_home).on_click(cx.listener(|this, _, _, cx| {
                    let m = &mut this.dock.sys.tool.more;
                    m.del_home = !m.del_home;
                    cx.notify();
                }))),
        );
        let header = div().child(head("用户名").w(px(130.0))).child(head("UID").w(px(60.0))).child(head("家目录").w(px(150.0))).child(head("状态").flex_1().min_w_0()).child(div().w(px(300.0)));
        let rows: Vec<Stateful<Div>> = info.users.iter().enumerate().map(|(i, u)| self.user_row(i, u, &info, &login, cx)).collect();
        col = col.child(table_flow(header, rows));

        // 改密码
        if let Some(target) = self.dock.sys.tool.more.pw_target.clone() {
            let t2 = target.clone();
            col = col.child(
                card().child(section("修改密码")).child(
                    div()
                        .flex()
                        .flex_row()
                        .items_end()
                        .gap(px(12.0))
                        .child(labeled("用户", "", div().h(px(32.0)).flex().items_center().child(mono_text(target.clone()))))
                        .child(div().flex_1().min_w_0().child(labeled("新密码", "", input_box(&pwnew))))
                        .child(button("us-pw-ok", IconName::Check, "修改").on_click(cx.listener(move |this, _, window, cx| {
                            let pw = this.mi(I_PWNEW).map(|e| e.read(cx).value().to_string()).unwrap_or_default();
                            match users::password_script(&t2, &pw) {
                                Ok((script, input)) => {
                                    this.more_ask(Tool::Users, "修改密码", format!("修改 {t2} 的登录密码"), "修改密码", t2.clone(), Ok(script), Some(input), true, 0, None, vec!["密码经加密通道传给 chpasswd,不会出现在命令行和日志里".into()], cx);
                                    this.dock.sys.tool.more.pw_target = None;
                                    this.mset(I_PWNEW, "", window, cx);
                                }
                                Err(e) => this.tool_notice_pub(e, cx),
                            }
                        })))
                        .child(button("us-pw-cancel", IconName::X, "取消").on_click(cx.listener(|this, _, _, cx| {
                            this.dock.sys.tool.more.pw_target = None;
                            cx.notify();
                        }))),
                ),
            );
        }

        // 新建
        let group = info.admin_group.clone();
        col = col.child(
            card()
                .child(section("新建用户"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_end()
                        .gap(px(12.0))
                        .child(div().w(px(220.0)).child(labeled("用户名", "", input_box(&name_in))))
                        .child(div().flex_1().min_w_0().child(labeled("密码", "", input_box(&pw_in))))
                        .child(chip(("us-newsudo", 0), "加入管理员组", new_sudo).on_click(cx.listener(|this, _, _, cx| {
                            let m = &mut this.dock.sys.tool.more;
                            m.new_sudo = !m.new_sudo;
                            cx.notify();
                        }))),
                )
                .child(labeled("公钥", "", area_box(&area, 60.0)))
                .child(div().flex().flex_row().items_center().justify_between().child(small("密码和公钥至少填一个。家目录 /home/用户名,shell 用 bash", c::ghost())).child(button("us-create", IconName::UserPlus, "创建").on_click(cx.listener(move |this, _, window, cx| {
                    let (name, pw, key) = (this.mtext(I_USER, cx), this.mi(I_UPW).map(|e| e.read(cx).value().to_string()).unwrap_or_default(), this.atext(A_UKEY, cx));
                    let sudo = this.dock.sys.tool.more.new_sudo;
                    if sudo && group.is_none() {
                        return this.tool_notice_pub("这台服务器上没有 sudo、wheel 或 admin 组", cx);
                    }
                    if info.users.iter().any(|u| u.name == name) {
                        return this.tool_notice_pub(format!("用户 {name} 已经存在"), cx);
                    }
                    match users::create_script(&name, sudo.then(|| group.as_deref().unwrap_or("")), &pw, &key) {
                        Ok((script, input)) => {
                            let mut list = vec![if pw.is_empty() { "没有设密码,只能用公钥登录".to_string() } else { "密码经加密通道传给 chpasswd".to_string() }];
                            if !key.is_empty() {
                                list.push("公钥写进 ~/.ssh/authorized_keys".into());
                            }
                            this.more_ask(Tool::Users, "新建用户", format!("新建用户 {name}{}", if sudo { ",并加入管理员组" } else { "" }), "新建用户", name.clone(), Ok(script), Some(input), true, 0, None, list, cx);
                            this.mset(I_USER, "", window, cx);
                            this.mset(I_UPW, "", window, cx);
                            this.aclear(A_UKEY, window, cx);
                        }
                        Err(e) => this.tool_notice_pub(e, cx),
                    }
                })))),
        );
        col.into_any_element()
    }

    // ================= fail2ban =================

    /// 写配置前的底稿:Otter 还没写过时,先把回环和本次连接来源放进白名单(否则会覆盖系统默认的白名单)
    fn f2b_conf(info: &F2bInfo) -> f2b::Conf {
        let mut c = info.conf.clone();
        if c.get("ignoreip").is_none() {
            c.set_ignore(&f2b::initial_conf(&info.client).ignore());
        }
        c
    }

    fn f2b_save(&mut self, conf: f2b::Conf, title: &'static str, body: String, cx: &mut Context<Self>) {
        match f2b::save_conf_script(&conf) {
            Ok((s, input)) => self.more_ask(Tool::Fail2ban, title, body, "保存 fail2ban 配置", "otter.local".into(), Ok(s), Some(input), true, 0, None, vec![format!("写入 {}", f2b::CONF_FILE), "先用 fail2ban-client -t 检查,没通过就还原;通过后重启服务".into()], cx),
            Err(e) => self.tool_notice_pub(e, cx),
        }
    }

    pub(crate) fn f2b_tool(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let info = match &self.dock.sys.tool.more.f2b {
            None => return reading("读取 fail2ban 状态"),
            Some(Err(e)) => return message(IconName::TriangleAlert, "读取不了 fail2ban", e.clone()),
            Some(Ok(i)) => i.clone(),
        };
        let Some(ign_in) = self.mi(I_IGN).cloned() else { return reading("准备") };
        let mut col = div().flex().flex_col().gap(px(12.0));

        // 没装
        if info.bin.is_none() {
            let pkg = info.pkg.clone();
            let conf = f2b::initial_conf(&info.client);
            let how = match pkg.as_str() {
                "apt" => "用 apt-get 安装 fail2ban",
                "dnf" | "yum" => "用 dnf/yum 安装 fail2ban(源里没有时先装 epel-release)",
                _ => "",
            };
            let mut c1 = card().items_center().py(px(28.0)).gap(px(8.0)).child(icon(IconName::Ban, 24.0, c::ghost())).child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child("这台服务器没有安装 fail2ban")).child(small("fail2ban 会盯着登录日志,连续输错密码的 IP 自动封一段时间,挡 SSH 暴力破解。它是发行版的软件包,由 systemd 托管,不依赖 Otter 一直开着", c::ghost()));
            if how.is_empty() {
                c1 = c1.child(small("没找到包管理器(apt、dnf、yum),请手动安装", c::warn()));
            } else {
                let how = how.to_string();
                c1 = c1.child(small(format!("点安装:{how},写入白名单(含本次连接的来源 IP),打开 sshd 保护"), c::ghost())).child(button("f2b-install", IconName::Download, "安装并启用").on_click(cx.listener(move |this, _, _, cx| {
                    match f2b::install_script(&pkg, &conf) {
                        Ok((s, input)) => this.more_ask(Tool::Fail2ban, "安装 fail2ban", how.clone(), "安装 fail2ban", "fail2ban".into(), Ok(s), Some(input), true, 600, None, vec![format!("白名单:{}", conf.ignore().join(" ")), "sshd 保护默认打开;其它服务的保护需要自己在配置里启用".into(), "安装需要联网下载软件包,可能要一两分钟".into()], cx),
                        Err(e) => this.tool_notice_pub(e, cx),
                    }
                })));
            }
            return col.child(c1).into_any_element();
        }

        // 没在运行
        if !info.active {
            return col
                .child(card().items_center().py(px(28.0)).gap(px(8.0)).child(icon(IconName::Ban, 24.0, c::warn())).child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).child("fail2ban 已安装,但没有运行")).child(button("f2b-start", IconName::Play, "启动并设为开机自启").on_click(cx.listener(|this, _, _, cx| {
                    this.more_ask(Tool::Fail2ban, "启动 fail2ban", "启动 fail2ban 并设为开机自启".into(), "启动 fail2ban", "fail2ban".into(), Ok(f2b::service_script(true)), None, true, 0, None, Vec::new(), cx);
                }))))
                .into_any_element();
        }

        // 总览
        let (banned_now, banned_total, failed_now): (u64, u64, u64) = info.jails.iter().fold((0, 0, 0), |a, j| (a.0 + j.banned_now, a.1 + j.banned_total, a.2 + j.failed_now));
        let stat = |title: &'static str, big: String, sub: String| card().flex_1().min_w_0().gap(px(6.0)).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(title)).child(div().text_size(px(t::TYPE_DISPLAY)).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(big))).child(small(sub, c::ghost()));
        col = col.child(div().flex_shrink_0().flex().flex_row().gap(px(12.0)).child(stat("现在被封", banned_now.to_string(), format!("累计封过 {banned_total} 次"))).child(stat("正在失败计数", failed_now.to_string(), "还没到封禁条件的失败登录".into())).child(stat("保护中的 jail", info.jails.len().to_string(), info.jails.iter().map(|j| j.name.clone()).collect::<Vec<_>>().join("、"))));

        // jail 和封禁列表
        let mut n = 0usize;
        for j in &info.jails {
            col = col.child(div().flex().flex_row().items_baseline().gap(px(8.0)).child(section("被封的 IP")).child(small(format!("{} · {} 个", j.name, j.banned.len()), c::ghost())));
            if j.banned.is_empty() {
                col = col.child(card().py(px(14.0)).child(small("现在没有被封的 IP", c::ghost())));
            } else {
                let rows: Vec<Stateful<Div>> = j
                    .banned
                    .iter()
                    .map(|ip| {
                        let (jn, ip2) = (j.name.clone(), ip.clone());
                        n += 1;
                        row(("f2b-ip", n)).child(div().flex_1().min_w_0().text_size(px(t::TYPE_CONTROL)).font_family(MONO).child(SharedString::from(ip.clone()))).child(chip(("f2b-unban", n), "解封", false).on_click(cx.listener(move |this, _, _, cx| {
                            this.more_ask(Tool::Fail2ban, "解封 IP", format!("解封 {ip2}(jail:{jn}),它可以重新尝试登录"), "解封", ip2.clone(), f2b::unban_script(&jn, &ip2), None, true, 0, None, Vec::new(), cx);
                        })))
                    })
                    .collect();
                col = col.child(table_flow(div().child(head("IP")), rows));
            }
        }

        // 封禁参数
        let cur = info.jails.iter().find(|j| j.name == "sshd").or(info.jails.first());
        let mut c1 = card().gap(px(0.0)).child(div().pb(px(6.0)).child(section("封禁规则")));
        let mut ctl = div().flex().flex_row().items_center().gap(px(4.0));
        for (i, (secs, label)) in f2b::BANTIMES.into_iter().enumerate() {
            let on = cur.and_then(|j| j.bantime) == Some(secs);
            let info2 = info.clone();
            ctl = ctl.child(chip(("f2b-bt", i), label, on).on_click(cx.listener(move |this, _, _, cx| {
                let mut conf = Self::f2b_conf(&info2);
                conf.set("bantime", &f2b::time_value(secs));
                this.f2b_save(conf, "修改封禁时长", format!("被封的 IP 封 {}", f2b::fmt_secs(secs)), cx);
            })));
        }
        c1 = c1.child(srow("封禁多久", "触发封禁后,这个 IP 被拦多长时间", ctl));
        let mut ctl = div().flex().flex_row().items_center().gap(px(4.0));
        for (i, (secs, label)) in f2b::FINDTIMES.into_iter().enumerate() {
            let on = cur.and_then(|j| j.findtime) == Some(secs);
            let info2 = info.clone();
            ctl = ctl.child(chip(("f2b-ft", i), label, on).on_click(cx.listener(move |this, _, _, cx| {
                let mut conf = Self::f2b_conf(&info2);
                conf.set("findtime", &f2b::time_value(secs));
                this.f2b_save(conf, "修改统计窗口", format!("在 {} 内统计失败次数", f2b::fmt_secs(secs)), cx);
            })));
        }
        c1 = c1.child(srow("统计窗口", "在这段时间内累计失败次数", ctl));
        let mut ctl = div().flex().flex_row().items_center().gap(px(4.0));
        for (i, n) in f2b::MAXRETRIES.into_iter().enumerate() {
            let on = cur.and_then(|j| j.maxretry) == Some(n);
            let info2 = info.clone();
            ctl = ctl.child(chip(("f2b-mr", i), format!("{n} 次"), on).on_click(cx.listener(move |this, _, _, cx| {
                let mut conf = Self::f2b_conf(&info2);
                conf.set("maxretry", &n.to_string());
                this.f2b_save(conf, "修改失败次数", format!("统计窗口内失败 {n} 次就封禁"), cx);
            })));
        }
        c1 = c1.child(srow("失败几次封禁", "统计窗口内失败达到这个次数就封", ctl));
        let on = info.jails.iter().any(|j| j.name == "sshd");
        let info2 = info.clone();
        c1 = c1.child(srow("SSH 保护", "盯着 sshd 的登录日志,连续失败的 IP 封掉", div().id("f2b-sshd").flex_shrink_0().cursor_pointer().child(switch("f2b-sshd-sw", on)).on_click(cx.listener(move |this, _, _, cx| {
            let mut conf = Self::f2b_conf(&info2);
            conf.set_sshd_enabled(!on);
            this.f2b_save(conf, "SSH 保护", if on { "关闭 sshd 的 fail2ban 保护".to_string() } else { "打开 sshd 的 fail2ban 保护".to_string() }, cx);
        }))));
        col = col.child(c1);

        // 白名单
        let ign = info.conf.ignore();
        let shown = if ign.is_empty() { vec!["127.0.0.1/8".to_string(), "::1".to_string()] } else { ign.clone() };
        let mut chips = div().flex().flex_row().flex_wrap().gap(px(6.0));
        for (i, ip) in shown.iter().enumerate() {
            let (info2, ip2) = (info.clone(), ip.clone());
            let is_client = *ip == info.client;
            chips = chips.child(
                div().h(px(28.0)).pl(px(10.0)).pr(px(4.0)).flex().flex_row().items_center().gap(px(4.0)).rounded(px(t::RADIUS_MD)).bg(c::idle_soft()).child(div().text_size(px(t::TYPE_CONTROL)).font_family(MONO).child(SharedString::from(ip.clone()))).children(is_client.then(|| small("本次连接", c::accent()))).child(
                    div().id(("f2b-ig-x", i)).size(px(20.0)).flex().items_center().justify_center().cursor_pointer().rounded(px(t::RADIUS_XS)).hover(|s| s.bg(c::raised())).child(icon(IconName::X, 12.0, c::ghost())).on_click(cx.listener(move |this, _, _, cx| {
                        let mut conf = Self::f2b_conf(&info2);
                        let list: Vec<String> = conf.ignore().into_iter().filter(|x| *x != ip2).collect();
                        conf.set_ignore(&list);
                        this.f2b_save(conf, "移出白名单", format!("{ip2} 以后输错密码也会被封"), cx);
                    })),
                ),
            );
        }
        let info2 = info.clone();
        col = col.child(
            card()
                .child(section("白名单"))
                .child(small("白名单里的 IP 不会被封。默认放了本机回环和你这次连接的来源 IP,免得输错密码把自己封了", c::ghost()))
                .child(chips)
                .child(div().flex().flex_row().items_center().gap(px(12.0)).child(input_box(&ign_in)).child(button("f2b-ig-add", IconName::Plus, "加入白名单").on_click(cx.listener(move |this, _, window, cx| {
                    let v = this.mtext(I_IGN, cx);
                    if !crate::toolbox::common::valid_ip_or_cidr(&v) {
                        return this.tool_notice_pub(format!("「{v}」不是有效的 IP 或网段"), cx);
                    }
                    let mut conf = Self::f2b_conf(&info2);
                    let mut list = conf.ignore();
                    if list.contains(&v) {
                        return this.tool_notice_pub("已经在白名单里了", cx);
                    }
                    list.push(v.clone());
                    conf.set_ignore(&list);
                    this.f2b_save(conf, "加入白名单", format!("{v} 不会被 fail2ban 封禁"), cx);
                    this.mset(I_IGN, "", window, cx);
                })))),
        );

        // 服务
        col = col.child(div().flex().flex_row().items_center().justify_between().child(small("fail2ban 由 systemd 托管,Otter 关掉后它照常工作", c::ghost())).child(button("f2b-stop", IconName::Square, "停止 fail2ban").on_click(cx.listener(|this, _, _, cx| {
            this.more_ask(Tool::Fail2ban, "停止 fail2ban", "停止 fail2ban 并取消开机自启,现有的封禁会被解除".into(), "停止 fail2ban", "fail2ban".into(), Ok(f2b::service_script(false)), None, false, 0, None, vec!["停止期间没有任何防暴力破解保护".into()], cx);
        }))));
        col.into_any_element()
    }

    // ---- 截图用的示例数据 ----

    pub(super) fn more_demo(&mut self) {
        let m = &mut self.dock.sys.tool.more;
        m.host = Some(host::parse_host("@@HOSTNAME\nweb-1\n@@STATIC\nweb-1\n@@HOSTS\n127.0.0.1\tlocalhost\n127.0.1.1\tweb-1\n10.0.4.20\tdb.internal db  # 数据库\n::1\tip6-localhost ip6-loopback\n@@MODE\nresolved\n@@DNS\nGlobal: 1.1.1.1 8.8.8.8\nLink 2 (eth0): 10.0.0.2\n@@CONF\n[Resolve]\nDNS=1.1.1.1 8.8.8.8\n"));
        m.sysctl = Some(sysctl::parse_sysctl("@@VALUES\nnet.ipv4.ip_forward 1\nnet.ipv4.tcp_congestion_control bbr\nnet.ipv4.tcp_syncookies 1\nnet.ipv4.icmp_echo_ignore_all 0\nnet.core.somaxconn 4096\nfs.file-max 524288\nvm.overcommit_memory 0\nvm.max_map_count 262144\n@@QDISC\nfq\n@@BBR\nyes\n@@CONF\nnet.core.somaxconn = 4096\nnet.core.default_qdisc = fq\nnet.ipv4.tcp_congestion_control = bbr\n"));
        m.ssh = Some(ssh::parse_ssh("@@SSHD\nport 2222\npermitrootlogin without-password\npasswordauthentication no\npubkeyauthentication yes\nmaxauthtries 4\nclientaliveinterval 60\n@@UNIT\nssh\n@@SOCKET\nno\n@@INCLUDE\nyes\n@@OTTER\nPort 2222\nPasswordAuthentication no\nMaxAuthTries 4\n@@KEYS\nU\troot\t/root\nK\tSHA256:Vq3xH7mP0uYk1bJ2nQeR9sTzL4wA6cDfG8hIoKpMlNs\t(ED25519)\tssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBqX me@laptop\nK\tSHA256:Bn7yT2kLq9XcR4vWmZ1oPeA5sDfG8hJ3uIiKlMnOpQr\t(RSA)\tfrom=\"10.0.0.0/8\" ssh-rsa AAAAB3NzaC1yc2E ci-runner\nU\tdeploy\t/home/deploy\nK\tSHA256:Zx1cV2bN3mQ4wE5rT6yU7iO8pAsDfGhJkLzXcVbNmQw\t(ED25519)\tssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPqX deploy@ci\n"));
        m.users = Some(users::parse_users("@@GROUP\nsudo\n@@USERS\nroot\t0\t/root\t/bin/bash\tpw\t0\troot\troot\ndeploy\t1000\t/home/deploy\t/bin/bash\tnopw\t0\tdeploy,sudo\tDeploy\nbackup\t1001\t/home/backup\t/bin/bash\tpw\t0\tbackup\t\nold-dev\t1002\t/home/old-dev\t/bin/bash\tlocked\t0\told-dev\t\nsvc\t1003\t/srv/svc\t/usr/sbin/nologin\tnopw\t0\tsvc\t\n"));
        m.f2b = Some(f2b::parse_f2b("@@BIN\n/usr/bin/fail2ban-client\n@@PKG\napt\n@@ACTIVE\nactive\n@@CLIENT\n203.0.113.9\n@@CONF\n[DEFAULT]\nbantime = 86400\nignoreip = 127.0.0.1/8 ::1 203.0.113.9\n\n[sshd]\nenabled = true\n@@JAILS\nJ\tsshd\nCurrently failed:\t3\nTotal failed:\t412\nCurrently banned:\t2\nTotal banned:\t57\nBanned IP list:\t198.51.100.7 192.0.2.44\nS\tbantime\t86400\nS\tfindtime\t600\nS\tmaxretry\t5\n"));
    }
}
