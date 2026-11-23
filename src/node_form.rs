//! 添加 / 编辑节点的弹窗表单。
//!
//! 目前只接了"添加";字段和布局按编辑复用来设计(标题、提交按钮文案由 `NodeForm::editing` 决定)。

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Home, MONO, c, icon, t};
use crate::ui::{Glass as _, GlassHover as _, GlassStyle};
use crate::mock::{Node, NodeId};

#[derive(Clone, PartialEq)]
pub enum TestState {
    Idle,
    Running,
    Ok(u32),
    Err(SharedString),
}

pub struct NodeForm {
    /// Some((分组, 节点)) = 编辑已有节点;None = 新增
    pub editing: Option<NodeId>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    key_path: Entity<InputState>,
    passphrase: Entity<InputState>,
    /// 认证方式是叠加的(参考 Termius):用户名 + 密码始终显示,私钥、SSH Agent 按需添加
    use_key: bool,
    use_agent: bool,
    show_pwd: bool,
    /// 「添加认证方式」下拉是否展开
    add_menu: bool,
    group: usize,
    test: TestState,
    /// 点过提交后才显示必填项的错误态,避免一打开就满屏红框
    tried: bool,
}

impl NodeForm {
    pub fn new(group: usize, window: &mut Window, cx: &mut App) -> Self {
        let input = |placeholder: &'static str, value: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
        };
        let form = Self {
            editing: None,
            name: input("例如 prod-web-01", "", window, cx),
            host: input("IP 或域名", "", window, cx),
            port: input("22", "22", window, cx),
            user: input("root", "root", window, cx),
            password: cx.new(|cx| InputState::new(window, cx).placeholder("SSH 密码").masked(true)),
            key_path: input("~/.ssh/id_ed25519", "~/.ssh/id_ed25519", window, cx),
            passphrase: cx.new(|cx| InputState::new(window, cx).placeholder("可选,没有可留空").masked(true)),
            use_key: false,
            use_agent: false,
            show_pwd: false,
            add_menu: false,
            group,
            test: TestState::Idle,
            tried: false,
        };
        let focus = form.name.focus_handle(cx);
        window.focus(&focus, cx);
        form
    }

    fn val(state: &Entity<InputState>, cx: &App) -> String {
        state.read(cx).value().trim().to_string()
    }
}

// ---- 交互 --------------------------------------------------------------------
impl Home {
    pub fn open_add_node(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (group, _) = self.locate(self.sel);
        self.form = Some(NodeForm::new(group, window, cx));
        cx.notify();
    }

    fn close_form(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        cx.notify();
    }

    fn submit_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = &mut self.form else { return };
        f.tried = true;
        let name = NodeForm::val(&f.name, cx);
        let host = NodeForm::val(&f.host, cx);
        if name.is_empty() || host.is_empty() {
            cx.notify();
            return;
        }
        let user = NodeForm::val(&f.user, cx);
        let user = if user.is_empty() { "root".to_string() } else { user };
        let port = NodeForm::val(&f.port, cx).parse::<u16>().unwrap_or(22);
        let user_host = if port == 22 { format!("{user}@{host}") } else { format!("{user}@{host}:{port}") };
        let g = f.group;
        let id = NodeId(self.next_id);
        self.next_id += 1;
        self.groups[g].nodes.push(Node { id, name, user_host, stacks: vec![] });
        self.open_node(id, Some(crate::workspace::NodeMode::Containers), window, cx);
        self.scroll = gpui_kit::component::VirtualListScrollHandle::new();
        self.close_form(cx);
    }

    /// 模拟测试连接:等一会儿,主机为空或以 "bad" 开头则失败。
    fn test_connection(&mut self, cx: &mut Context<Self>) {
        let Some(f) = &mut self.form else { return };
        let host = NodeForm::val(&f.host, cx);
        f.test = TestState::Running;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(800)).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(f) = &mut this.form {
                    f.test = if host.is_empty() {
                        TestState::Err("请先填写主机地址".into())
                    } else if host.starts_with("bad") {
                        TestState::Err("连接超时".into())
                    } else {
                        TestState::Ok(30 + (host.len() as u32 * 17) % 200)
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn toggle_pwd(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = &mut self.form else { return };
        f.show_pwd = !f.show_pwd;
        let masked = !f.show_pwd;
        f.password.update(cx, |s, cx| s.set_masked(masked, window, cx));
        cx.notify();
    }

    fn browse_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择私钥".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(f) = &this.form {
                    f.key_path.update(cx, |s, cx| s.set_value(path.display().to_string(), window, cx));
                }
            });
        })
        .detach();
    }
}

// ---- 渲染 --------------------------------------------------------------------
// 布局:分组圆角容器(iOS 设置式),每行「左标签 + 右输入」,输入框不再各自带边框;
// 焦点用整行淡紫底表示,错误用整行淡红底 + 红标签。

/// 一行里的输入:无边框,撑满剩余宽度
fn input_cell(state: &Entity<InputState>, mono: bool) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .text_color(c::text())
        .text_size(px(t::TYPE_BODY))
        .when(mono, |d| d.font_family(MONO))
        // 组件按 size*0.875 算字号:换算成 13px 正文
        .child(Input::new(state).appearance(false).with_size(px(13.0 / 0.875)))
}

/// 分组容器里的一行:固定宽标签 + 内容;`last` 的行不画底部分隔线
fn form_row(label: &'static str, focused: bool, error: bool, last: bool, content: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .min_h(px(46.0))
        .pl(px(16.0))
        .pr(px(8.0))
        .when(!last, |d| d.border_b_1().border_color(c::line()))
        .when(error, |d| d.bg(c::danger().opacity(0.07)))
        .when(!error && focused, |d| d.bg(c::field_focus_bg()))
        .child(
            div()
                .w(px(60.0))
                .flex_shrink_0()
                .text_size(px(t::TYPE_CONTROL))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if error {
                    c::danger()
                } else if focused {
                    c::text()
                } else {
                    c::label()
                })
                .child(label),
        )
        .child(content)
}

/// 带小标题的分组容器
fn form_group(title: &'static str, rows: Vec<Div>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(div().px(px(4.0)).text_size(px(t::TYPE_CAPTION)).font_weight(FontWeight::SEMIBOLD).text_color(c::ghost()).child(title))
        .child(
            div()
                .flex()
                .flex_col()
                .rounded(px(t::RADIUS_LG))
                .bg(c::field())
                .border_1()
                .border_color(c::field_border())
                .overflow_hidden()
                .children(rows),
        )
}

/// 下拉菜单里的一项:图标 + 标题 + 说明
fn menu_item(id: &'static str, ic: IconName, title: &'static str, sub: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .min_h(px(52.0))
        .px(px(12.0))
        .rounded(px(t::RADIUS_MD))
        .cursor_pointer()
        .hover(|s| s.bg(c::field_hover()))
        .child(icon(ic, 18.0, c::muted()))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(div().text_size(px(t::TYPE_BODY)).text_color(c::text()).child(title))
                .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(sub)),
        )
}

/// 行尾的小图标按钮(移除)
fn x_btn(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(28.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(t::RADIUS_SM))
        .cursor_pointer()
        .hover(|s| s.bg(c::hover()))
        .child(icon(IconName::X, 14.0, c::muted()))
}

/// 弹窗里的次要按钮:玻璃 CONTROL 样式
fn ctl_btn(id: &'static str, ic: Option<IconName>, text: &'static str, h: f32) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(h))
        .px(px(12.0))
        .glass(t::RADIUS_MD, GlassStyle::CONTROL)
        .glass_hover(GlassStyle::CONTROL)
        .text_size(px(t::TYPE_CONTROL))
        .text_color(c::secondary())
        .hover(|s| s.text_color(c::text()))
        .when_some(ic, |d, ic| d.child(icon(ic, 13.0, c::muted())))
        .child(text)
}

impl Home {
    pub fn node_dialog(&self, window: &Window, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let f = self.form.as_ref()?;
        let name_err = f.tried && NodeForm::val(&f.name, cx).is_empty();
        let host_err = f.tried && NodeForm::val(&f.host, cx).is_empty();
        let focus = |s: &Entity<InputState>, cx: &Context<Self>| s.focus_handle(cx).is_focused(window);

        // 分组:彩色药丸单选
        let groups = div().flex_1().flex().flex_row().flex_wrap().gap(px(6.0)).py(px(9.0)).children(self.groups.iter().enumerate().map(|(gi, g)| {
            let on = f.group == gi;
            div()
                .id(("fg", gi))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(7.0))
                .h(px(26.0))
                .px(px(11.0))
                .rounded_full()
                .border_1()
                .cursor_pointer()
                .text_size(px(t::TYPE_CONTROL))
                .when(on, |d| d.border_color(rgb(g.color).opacity(0.6)).bg(rgb(g.color).opacity(0.14)).text_color(c::text()))
                .when(!on, |d| d.border_color(c::field_border()).text_color(c::muted()).hover(|s| s.bg(c::field_hover())))
                .child(div().size(px(7.0)).rounded(px(2.0)).bg(rgb(g.color)))
                .child(g.name.clone())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(f) = &mut this.form {
                        f.group = gi;
                        cx.notify();
                    }
                }))
        }));

        // ---- 连接 ----
        let host_cell = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(input_cell(&f.host, true))
            .child(div().w(px(1.0)).h(px(18.0)).bg(c::line()))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child("端口"))
            .child(input_cell(&f.port, true).flex_none().w(px(64.0)));
        let conn = form_group(
            "连接",
            vec![
                form_row("名称", focus(&f.name, cx), name_err, false, input_cell(&f.name, false)),
                form_row("主机", focus(&f.host, cx) || focus(&f.port, cx), host_err, false, host_cell),
                form_row("分组", false, false, true, groups),
            ],
        );

        // ---- 认证 ----
        let eye = div()
            .id("toggle-pwd")
            .size(px(28.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(t::RADIUS_SM))
            .cursor_pointer()
            .hover(|s| s.bg(c::hover()))
            .child(icon(if f.show_pwd { IconName::EyeOff } else { IconName::Eye }, 15.0, c::muted()))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_pwd(window, cx)));
        // 已添加的方式:私钥(路径 + 口令)、SSH Agent,每项可移除
        let mut extra: Vec<(&'static str, bool, Div)> = vec![];
        if f.use_key {
            extra.push((
                "私钥",
                focus(&f.key_path, cx),
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(input_cell(&f.key_path, true))
                    .child(
                        ctl_btn("browse-key", Some(IconName::FolderOpen), "浏览", 28.0)
                            .on_click(cx.listener(|this, _, window, cx| this.browse_key(window, cx))),
                    )
                    .child(x_btn("rm-key").on_click(cx.listener(|this, _, _, cx| {
                        if let Some(f) = &mut this.form {
                            f.use_key = false;
                            cx.notify();
                        }
                    }))),
            ));
            extra.push(("口令", focus(&f.passphrase, cx), input_cell(&f.passphrase, false)));
        }
        if f.use_agent {
            extra.push((
                "Agent",
                false,
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().flex_1().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("使用本机 ssh-agent 中已加载的密钥"))
                    .child(x_btn("rm-agent").on_click(cx.listener(|this, _, _, cx| {
                        if let Some(f) = &mut this.form {
                            f.use_agent = false;
                            cx.notify();
                        }
                    }))),
            ));
        }
        let mut cred_rows = vec![
            form_row("用户名", focus(&f.user, cx), false, false, input_cell(&f.user, true)),
        ];
        let pwd_row = form_row(
            "密码",
            focus(&f.password, cx),
            false,
            extra.is_empty(),
            div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(4.0)).child(input_cell(&f.password, false)).child(eye),
        );
        cred_rows.push(pwd_row);
        let n = extra.len();
        for (i, (label, focused, content)) in extra.into_iter().enumerate() {
            cred_rows.push(form_row(label, focused, false, i + 1 == n, content));
        }

        // 「+ 添加私钥、SSH Agent」链接,点开下拉(向上展开,盖在表单上)
        let mut remaining: Vec<&'static str> = vec![];
        if !f.use_key {
            remaining.push("私钥");
        }
        if !f.use_agent {
            remaining.push("SSH Agent");
        }
        let add_link = (!remaining.is_empty()).then(|| {
            let menu = f.add_menu.then(|| {
                div()
                    .id("add-auth-menu")
                    .absolute()
                    .left_0()
                    .bottom(px(38.0))
                    .w(px(320.0))
                    .p(px(4.0))
                    .flex()
                    .flex_col()
                    .rounded(px(t::RADIUS_LG))
                    .bg(c::dialog())
                    .border_1()
                    .border_color(c::dialog_border())
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        if let Some(f) = &mut this.form {
                            f.add_menu = false;
                            cx.notify();
                        }
                    }))
                    .when(!f.use_key, |d| {
                        d.child(menu_item("menu-key", IconName::KeyRound, "私钥", "使用本机的私钥文件").on_click(cx.listener(|this, _, _, cx| {
                            if let Some(f) = &mut this.form {
                                f.use_key = true;
                                f.add_menu = false;
                                cx.notify();
                            }
                        })))
                    })
                    .when(!f.use_agent, |d| {
                        d.child(menu_item("menu-agent", IconName::FingerprintPattern, "SSH Agent", "使用本机 ssh-agent 中的密钥").on_click(cx.listener(
                            |this, _, _, cx| {
                                if let Some(f) = &mut this.form {
                                    f.use_agent = true;
                                    f.add_menu = false;
                                    cx.notify();
                                }
                            },
                        )))
                    })
            });
            div()
                .relative()
                .child(
                    div()
                        .id("add-auth")
                        .h(px(30.0))
                        .px(px(4.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .rounded(px(t::RADIUS_SM))
                        .cursor_pointer()
                        .text_size(px(t::TYPE_CONTROL))
                        .text_color(c::ghost())
                        .hover(|s| s.text_color(c::secondary()))
                        .child(icon(IconName::Plus, 14.0, c::muted()))
                        .child(format!("添加 {}", remaining.join("、")))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(f) = &mut this.form {
                                f.add_menu = true;
                                cx.notify();
                            }
                        })),
                )
                .children(menu)
        });
        let cred = form_group("SSH 认证", cred_rows).children(add_link);

        let body = div().flex().flex_col().gap(px(20.0)).px(px(24.0)).pt(px(6.0)).pb(px(22.0)).child(conn).child(cred);

        let test_result = match &f.test {
            TestState::Idle => None,
            TestState::Running => Some((IconName::LoaderCircle, c::muted(), "正在连接…".to_string())),
            TestState::Ok(ms) => Some((IconName::CircleCheck, c::online(), format!("连接成功 · {ms}ms"))),
            TestState::Err(e) => Some((IconName::CircleX, c::danger(), e.to_string())),
        };
        let can_submit = !NodeForm::val(&f.name, cx).is_empty() && !NodeForm::val(&f.host, cx).is_empty();

        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(24.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(c::line())
            .bg(c::dialog_footer())
            .child(ctl_btn("test-conn", Some(IconName::Zap), "测试连接", 34.0).on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))))
            .when_some(test_result, |d, (ic, color, text)| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(5.0))
                        .text_size(px(t::TYPE_CONTROL))
                        .text_color(color)
                        .child(icon(ic, 13.0, color))
                        .child(text),
                )
            })
            .child(div().flex_1())
            .child(ctl_btn("cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| this.close_form(cx))))
            .child(
                div()
                    .id("submit")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(34.0))
                    .px(px(16.0))
                    .rounded(px(t::RADIUS_MD))
                    .text_size(px(t::TYPE_CONTROL))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .when(can_submit, |d| d.bg(c::accent()).text_color(c::on_accent()).hover(|s| s.opacity(0.88)))
                    .when(!can_submit, |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()).cursor_default())
                    .child(icon(IconName::Plus, 13.0, if can_submit { c::on_accent() } else { c::ghost() }))
                    .child(if f.editing.is_some() { "保存" } else { "添加节点" })
                    .on_click(cx.listener(|this, _, window, cx| this.submit_form(window, cx))),
            );

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.0))
            .pl(px(24.0))
            .pr(px(52.0)) // 给右上角的关闭按钮让位
            .pt(px(24.0))
            .pb(px(18.0))
            .child(
                div()
                    .size(px(40.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(t::RADIUS_LG))
                    .border_1()
                    .border_color(c::accent().opacity(0.28))
                    .bg(c::accent_soft())
                    .child(icon(IconName::Server, 20.0, c::accent())),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .text_size(px(t::TYPE_TITLE_LG))
                            .font_weight(FontWeight::BOLD)
                            .child(if f.editing.is_some() { "编辑节点" } else { "添加节点" }),
                    )
                    .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("通过 SSH 连接一台运行 Docker 的服务器")),
            );

        let close = div()
            .id("close-form")
            .absolute()
            .top(px(14.0))
            .right(px(14.0))
            .size(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(t::RADIUS_SM))
            .cursor_pointer()
            .hover(|s| s.bg(c::hover()))
            .child(icon(IconName::X, 14.0, c::muted()))
            .on_click(cx.listener(|this, _, _, cx| this.close_form(cx)));

        Some(crate::ui::modal(
            "node-dialog",
            520.0,
            cx.listener(|this, e: &KeyDownEvent, window, cx| match e.keystroke.key.as_str() {
                "escape" => {
                    match this.form.as_mut() {
                        Some(f) if f.add_menu => {
                            f.add_menu = false;
                            cx.notify();
                        }
                        _ => this.close_form(cx),
                    }
                    cx.stop_propagation();
                }
                "enter" => {
                    this.submit_form(window, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }),
            [
                header.into_any_element(),
                div().id("form-body").flex_1().min_h_0().overflow_y_scroll().child(body).into_any_element(),
                footer.into_any_element(),
                close.into_any_element(),
            ],
        ))
    }
}

// ---- 端口悬停提示 ------------------------------------------------------------
pub struct PortsTip(pub Vec<String>);

impl Render for PortsTip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(10.0))
            .rounded(px(t::RADIUS_MD))
            .border_1()
            .border_color(c::border_strong())
            .bg(c::raised_hi())
            .shadow_lg()
            .child(
                div()
                    .text_size(px(t::TYPE_CAPTION))
                    .text_color(c::ghost())
                    .child(format!("全部端口 · {}", self.0.len())),
            )
            .children(self.0.iter().map(|p| {
                div()
                    .text_size(px(t::TYPE_CAPTION))
                    .font_family(MONO)
                    .text_color(c::secondary())
                    .child(p.clone())
            }))
    }
}
