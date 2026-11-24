//! 「仓库」页:管理服务器上登录过的镜像仓库(`docker login` / `docker logout`)。
//!
//! 列表读自服务器的 `config.json`,只取仓库地址和用户名,凭据本身不会读进界面。
//! 添加时密码只通过 stdin 交给 `docker login --password-stdin`,不进命令行和日志,提交后输入框立刻清空。

use std::time::Instant;

use gpui_kit::component::input::InputState;

use super::super::*;
use super::daemonpage::field;
use super::dockerpages::{DockAsk, head, icon_btn, row, small, table};
use crate::docker::{Action, RegAuth};

#[derive(Default)]
pub(crate) struct RegUi {
    /// 已登录的仓库;None 是还没读到
    list: Option<Result<Vec<RegAuth>, String>>,
    requested: bool,
    /// 「添加仓库」表单是否展开
    pub open: bool,
    /// 仓库地址、用户名、密码
    inputs: Option<Vec<Entity<InputState>>>,
    /// 仓库类型:true 是 HTTP(需要加进 daemon.json 的 insecure-registries)
    http: bool,
}

/// Docker Hub 在 config.json 里的键是个 URL,界面上显示成名字
fn host_name(host: &str) -> (String, bool) {
    if host.contains("index.docker.io") { ("Docker Hub".into(), true) } else { (host.to_string(), false) }
}

impl Home {
    /// 每次渲染前调用(仓库页在前台时):建输入框、请求并接收已登录的仓库
    pub(crate) fn reg_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // HTTP 仓库要加进 daemon.json 并重启 Docker:不管用户这时在哪一页,都弹确认框
        let ask = self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref()).and_then(|d| d.take_insecure_ask());
        if let Some(host) = ask {
            self.ask(
                DockAsk {
                    title: "启用 HTTP 仓库",
                    tile: IconName::Globe,
                    name: host.clone(),
                    detail: "镜像仓库",
                    body: "登录 HTTP 仓库前,要先把它加进 Docker 的 insecure-registries(daemon.json)并重启 Docker。没开 live-restore 时,正在运行的容器会被停止再拉起",
                    label: "重启并登录",
                    action: Action::ConfirmInsecure(host),
                    list: Vec::new(),
                },
                cx,
            );
        }
        if self.page != Page::Registry {
            return;
        }
        if self.dock.reg.inputs.is_none() {
            let specs: [(&str, bool); 3] = [("registry.example.com:5000", false), ("用户名", false), ("密码或访问令牌", true)];
            let v = specs
                .iter()
                .map(|(ph, masked)| {
                    let e = cx.new(|cx| InputState::new(window, cx).placeholder(*ph).masked(*masked));
                    cx.observe(&e, |_, _, cx| cx.notify()).detach();
                    e
                })
                .collect();
            self.dock.reg.inputs = Some(v);
        }
        let got = if self.demo {
            self.dock.reg.list.is_none().then(|| {
                Ok(vec![
                    RegAuth { host: "https://index.docker.io/v1/".into(), user: Some("dev-mac".into()) },
                    RegAuth { host: "ghcr.io".into(), user: Some("octocat".into()) },
                    RegAuth { host: "registry.example.com:5000".into(), user: None },
                ])
            })
        } else {
            let Some(d) = self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref()) else { return };
            if !self.dock.reg.requested {
                self.dock.reg.requested = true;
                d.load_registries();
            }
            d.take_registries()
        };
        if let Some(r) = got {
            // 调试:OTTER_REG_OPEN=1 截图时直接展开添加表单
            if self.demo && std::env::var("OTTER_REG_OPEN").is_ok() {
                self.dock.reg.open = true;
                self.dock.reg.http = std::env::var("OTTER_REG_HTTP").is_ok();
            }
            self.dock.reg.list = Some(r);
            cx.notify();
        }
    }

    /// 重新读一遍(刷新按钮)
    pub(crate) fn reg_reload(&mut self) {
        self.dock.reg.requested = false;
        self.dock.reg.list = None;
    }

    /// 取消 HTTP 仓库的确认:丢掉后台暂存的凭据
    pub(crate) fn reg_drop_pending(&mut self) {
        if let Some(d) = self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref()) {
            d.drop_pending_login();
        }
    }

    fn reg_login(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ins) = self.dock.reg.inputs.clone() else { return };
        let (registry, user) = (ins[0].read(cx).value().trim().to_string(), ins[1].read(cx).value().trim().to_string());
        let pass = ins[2].read(cx).value().to_string();
        let bad = |this: &mut Self, msg: &'static str, cx: &mut Context<Self>| {
            this.dock.notice = Some((msg.into(), Instant::now()));
            cx.notify();
        };
        if self.dock.reg.http && registry.is_empty() {
            return bad(self, "HTTP 仓库要填仓库地址,写成 主机:端口", cx);
        }
        if !registry.is_empty() && !crate::docker::safe_name(&registry) {
            return bad(self, "仓库地址只能包含字母、数字和 _ . : / @ - +,不要带 https://", cx);
        }
        if user.is_empty() || pass.is_empty() {
            return bad(self, "用户名和密码都要填", cx);
        }
        if user.chars().any(|c| c.is_control()) || pass.contains(['\n', '\r']) {
            return bad(self, "用户名或密码里有不支持的字符", cx);
        }
        // 密码输入框立刻清空,不在界面里留着
        ins[2].update(cx, |s, cx| s.set_value("", window, cx));
        self.dock.reg.open = false;
        if self.demo {
            return bad(self, "演示数据,不会真的执行", cx);
        }
        if let Some(d) = self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref()) {
            if self.dock.reg.http {
                d.login_http(registry, user, pass);
            } else {
                d.login(registry, user, pass);
            }
        }
        self.dock.notice = Some(("正在登录…".into(), Instant::now()));
        cx.notify();
    }

    /// 仓库类型:HTTPS / HTTP
    fn reg_type_row(&self, cx: &mut Context<Self>) -> Div {
        let mut chips = div().flex().flex_row().gap(px(4.0));
        for (i, (label, http)) in [("HTTPS", false), ("HTTP", true)].into_iter().enumerate() {
            let active = self.dock.reg.http == http;
            chips = chips.child(
                div()
                    .id(("reg-type", i))
                    .h(px(26.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(t::TYPE_CAPTION))
                    .when(active, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::SEMIBOLD))
                    .when(!active, |d| d.rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).text_color(c::muted()).hover(|s| s.bg(c::field_hover()).text_color(c::secondary())))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.dock.reg.http = http;
                        cx.notify();
                    })),
            );
        }
        div().flex().flex_col().gap(px(5.0)).child(div().text_size(px(t::TYPE_CAPTION)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child("仓库类型")).child(chips)
    }

    pub(crate) fn registry_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let list = match &self.dock.reg.list {
            None => return empty_state(IconName::LoaderCircle, "正在读取", "读取服务器上登录过的镜像仓库").into_any_element(),
            Some(Err(e)) => {
                return div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(empty_state(IconName::TriangleAlert, "读取不了登录信息", "处理后点右上角「刷新」重试"))
                    .child(div().pb(px(60.0)).flex().justify_center().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child(SharedString::from(e.clone())))
                    .into_any_element();
            }
            Some(Ok(l)) => l.clone(),
        };
        let http = self.dock.reg.http;
        let mut col = div().flex_1().min_h_0().pb(px(14.0)).flex().flex_col().gap(px(12.0));
        if self.dock.reg.open {
            if let Some(ins) = self.dock.reg.inputs.clone() {
                col = col.child(
                    div()
                        .flex_shrink_0()
                        .rounded(px(t::RADIUS_XL))
                        .border_1()
                        .border_color(c::dialog_border())
                        .p(px(14.0))
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(self.reg_type_row(cx))
                        .child(field("仓库地址", if http { "写成 主机:端口,不带 http://;首次添加会提示重启 Docker" } else { "留空是 Docker Hub;自建仓库写成 主机:端口,不带 https://" }, &ins[0]))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(10.0))
                                .child(div().flex_1().min_w_0().child(field("用户名", "登录这个仓库用的账号", &ins[1])))
                                .child(div().flex_1().min_w_0().child(field("密码 / 访问令牌", "建议用访问令牌,不要用账号密码", &ins[2]))),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(div().flex_1().min_w_0().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child("凭据保存在服务器的 config.json 里,只做了 base64 编码,没有加密"))
                                .child(button("reg-cancel", IconName::X, "取消").on_click(cx.listener(|this, _, _, cx| {
                                    this.dock.reg.open = false;
                                    cx.notify();
                                })))
                                .child(button("reg-login", IconName::KeyRound, "登录").on_click(cx.listener(|this, _, window, cx| this.reg_login(window, cx)))),
                        ),
                );
            }
        }
        if list.is_empty() {
            return col.child(empty_state(IconName::KeyRound, "还没有登录任何仓库", "点右上角「添加仓库」,登录后才能拉取私有镜像")).into_any_element();
        }
        let header = div().child(head("仓库").flex_1().min_w_0()).child(head("用户").w(px(220.0))).child(div().w(px(40.0)));
        let mut rows = Vec::new();
        for (i, r) in list.iter().enumerate() {
            let (name, hub) = host_name(&r.host);
            let busy = self.is_busy(&r.host);
            let mut act = div().w(px(40.0)).flex().justify_end();
            if busy {
                act = act.child(small("…", c::muted()));
            } else {
                let (host, nm) = (r.host.clone(), name.clone());
                act = act.child(icon_btn(("reg-out", i), IconName::Trash, c::danger()).on_click(cx.listener(move |this, _, _, cx| {
                    this.ask(
                        DockAsk {
                            title: "退出仓库登录",
                            tile: IconName::KeyRound,
                            name: nm.clone(),
                            detail: "镜像仓库",
                            body: "删除服务器上保存的这个仓库的登录信息,之后拉取私有镜像需要重新登录",
                            label: "退出登录",
                            action: Action::Logout(host.clone()),
                            list: Vec::new(),
                        },
                        cx,
                    );
                })));
            }
            rows.push(
                row(("reg", i))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(icon(IconName::Globe, 14.0, c::ghost()))
                            .child(div().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::MEDIUM).child(SharedString::from(name)))
                            .children(hub.then(|| small(r.host.clone(), c::ghost()))),
                    )
                    .child(match &r.user {
                        Some(u) => div().w(px(220.0)).truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(SharedString::from(u.clone())),
                        None => div().w(px(220.0)).text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("凭据由系统保存"),
                    })
                    .child(act),
            );
        }
        col.child(table("reg-list", header, rows)).into_any_element()
    }
}
