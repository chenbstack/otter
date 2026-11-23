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
use crate::mock::{Node, NodeId};

#[derive(Clone, Copy, PartialEq)]
pub enum Auth {
    Password,
    Key,
    Agent,
}

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
    auth: Auth,
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
            passphrase: cx.new(|cx| InputState::new(window, cx).placeholder("没有可留空").masked(true)),
            auth: Auth::Key,
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

    fn submit_form(&mut self, cx: &mut Context<Self>) {
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
        self.open_node(id);
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
/// 弹窗专用色:输入框、按钮都用"半透明白叠加",比主界面的 sunken 黑底柔和。
fn label(text: &'static str) -> Div {
    div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::MEDIUM).text_color(c::label()).child(text)
}

fn field_box(state: &Entity<InputState>, error: bool, mono: bool, window: &Window, cx: &App) -> Div {
    let focused = state.focus_handle(cx).is_focused(window);
    div()
        .h(px(34.0))
        .px(px(3.0))
        .flex()
        .items_center()
        .rounded(px(t::RADIUS_MD))
        .border_1()
        .when(error, |d| d.bg(c::danger().opacity(0.06)).border_color(c::danger().opacity(0.6)))
        .when(!error && focused, |d| d.bg(c::field_focus_bg()).border_color(c::accent().opacity(0.55)))
        .when(!error && !focused, |d| {
            d.bg(c::field()).border_color(c::field_border()).hover(|s| s.bg(c::field_hover()))
        })
        .text_color(c::text())
        .text_size(px(t::TYPE_BODY))
        .when(mono, |d| d.font_family(MONO))
        .child(
            div()
                .flex_1()
                // 组件按 size*0.875 算字号:换算成 13px 正文;水平内边距由组件给(8px)
                .child(Input::new(state).appearance(false).with_size(px(13.0 / 0.875))),
        )
}

fn field(name: &'static str, hint: Option<&'static str>, body: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap(px(6.0))
                .child(label(name))
                .when_some(hint, |d, h| d.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(h))),
        )
        .child(body)
}

fn ghost_btn(id: &'static str, ic: Option<IconName>, text: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(32.0))
        .px(px(12.0))
        .rounded(px(t::RADIUS_MD))
        .border_1()
        .border_color(c::field_border())
        .bg(c::field())
        .text_size(px(t::TYPE_BODY))
        .text_color(c::secondary())
        .cursor_pointer()
        .hover(|s| s.bg(c::field_hover()).text_color(c::text()))
        .when_some(ic, |d, ic| d.child(icon(ic, 13.0, c::muted())))
        .child(text)
}

impl Home {
    pub fn node_dialog(&self, window: &Window, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let f = self.form.as_ref()?;
        let name_err = f.tried && NodeForm::val(&f.name, cx).is_empty();
        let host_err = f.tried && NodeForm::val(&f.host, cx).is_empty();

        // 认证方式:分段控件
        let seg = |id: &'static str, text: &'static str, ic: IconName, a: Auth, cx: &mut Context<Self>| {
            let on = f.auth == a;
            div()
                .id(id)
                .flex_1()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .h(px(28.0))
                .rounded(px(t::RADIUS_SM))
                .text_size(px(t::TYPE_CONTROL))
                .cursor_pointer()
                .when(on, |d| d.bg(c::seg_on()).text_color(c::text()).font_weight(FontWeight::SEMIBOLD).shadow_xs())
                .when(!on, |d| d.text_color(c::muted()).hover(|s| s.text_color(c::secondary())))
                .child(icon(ic, 13.0, if on { c::accent() } else { c::ghost() }))
                .child(text)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(f) = &mut this.form {
                        f.auth = a;
                        cx.notify();
                    }
                }))
        };
        let auth_seg = div()
            .flex()
            .flex_row()
            .p(px(3.0))
            .rounded(px(t::RADIUS_MD))
            .bg(c::field())
            .border_1()
            .border_color(c::field_border())
            .child(seg("auth-key", "私钥", IconName::KeyRound, Auth::Key, cx))
            .child(seg("auth-pwd", "密码", IconName::LockKeyhole, Auth::Password, cx))
            .child(seg("auth-agent", "SSH Agent", IconName::FingerprintPattern, Auth::Agent, cx));

        let auth_body: AnyElement = match f.auth {
            Auth::Password => field("密码", None, field_box(&f.password, false, false, window, cx)).into_any_element(),
            Auth::Key => div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(field(
                    "私钥文件",
                    None,
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .child(div().flex_1().child(field_box(&f.key_path, false, true, window, cx)))
                        .child(
                            ghost_btn("browse-key", Some(IconName::FolderOpen), "浏览")
                                .h(px(34.0))
                                .on_click(cx.listener(|this, _, window, cx| this.browse_key(window, cx))),
                        ),
                ))
                .child(field("私钥口令", Some("可选"), field_box(&f.passphrase, false, false, window, cx)))
                .into_any_element(),
            Auth::Agent => div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .py(px(10.0))
                .rounded(px(t::RADIUS_MD))
                .bg(c::idle_soft())
                .text_size(px(t::TYPE_CONTROL))
                .text_color(c::muted())
                .child(icon(IconName::Info, 14.0, c::ghost()))
                .child("使用本机 ssh-agent 中已加载的密钥,无需在这里填写凭据。")
                .into_any_element(),
        };

        // 分组:彩色药丸单选
        let groups = div().flex().flex_row().flex_wrap().gap(px(8.0)).children(self.groups.iter().enumerate().map(|(gi, g)| {
            let on = f.group == gi;
            div()
                .id(("fg", gi))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(7.0))
                .h(px(28.0))
                .px(px(11.0))
                .rounded_full()
                .border_1()
                .cursor_pointer()
                .text_size(px(t::TYPE_CONTROL))
                .when(on, |d| d.border_color(rgb(g.color).opacity(0.7)).bg(rgb(g.color).opacity(0.12)).text_color(c::text()))
                .when(!on, |d| d.border_color(c::field_border()).bg(c::field()).text_color(c::muted()).hover(|s| s.bg(c::field_hover())))
                .child(div().size(px(7.0)).rounded(px(2.0)).bg(rgb(g.color)))
                .child(g.name.clone())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(f) = &mut this.form {
                        f.group = gi;
                        cx.notify();
                    }
                }))
        }));

        let section = |title: &'static str| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .text_size(px(t::TYPE_CAPTION))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(c::ghost())
                .child(title)
                .child(div().flex_1().h(px(1.0)).bg(c::line()))
        };

        let body = div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .px(px(22.0))
            .py(px(18.0))
            .child(section("基本信息"))
            .child(field("节点名称", None, field_box(&f.name, name_err, false, window, cx)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.0))
                    .child(div().flex_1().child(field("主机地址", None, field_box(&f.host, host_err, true, window, cx))))
                    .child(div().w(px(86.0)).child(field("端口", None, field_box(&f.port, false, true, window, cx)))),
            )
            .child(field("分组", None, groups))
            .child(div().h(px(2.0)))
            .child(section("SSH 认证"))
            .child(field("用户名", None, field_box(&f.user, false, true, window, cx)))
            .child(field("认证方式", None, auth_seg))
            .child(auth_body);

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
            .px(px(22.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(c::line())
            .bg(c::dialog_footer())
            .child(
                ghost_btn("test-conn", Some(IconName::Zap), "测试连接")
                    .on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))),
            )
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
            .child(ghost_btn("cancel", None, "取消").on_click(cx.listener(|this, _, _, cx| this.close_form(cx))))
            .child(
                div()
                    .id("submit")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(32.0))
                    .px(px(14.0))
                    .rounded(px(t::RADIUS_MD))
                    .text_size(px(t::TYPE_BODY))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .when(can_submit, |d| d.bg(c::accent()).text_color(c::on_accent()).hover(|s| s.opacity(0.88)))
                    .when(!can_submit, |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()).cursor_default())
                    .child(icon(IconName::Plus, 13.0, if can_submit { c::on_accent() } else { c::ghost() }))
                    .child(if f.editing.is_some() { "保存" } else { "添加节点" })
                    .on_click(cx.listener(|this, _, _, cx| this.submit_form(cx))),
            );

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .pl(px(22.0))
            .pr(px(48.0)) // 给右上角的关闭按钮让位
            .pt(px(20.0))
            .pb(px(16.0))
            .border_b_1()
            .border_color(c::line())
            .child(
                div()
                    .size(px(36.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(t::RADIUS_LG))
                    .bg(c::accent_soft())
                    .child(icon(IconName::Server, 18.0, c::accent())),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(t::TYPE_TITLE))
                            .font_weight(FontWeight::BOLD)
                            .child(if f.editing.is_some() { "编辑节点" } else { "添加节点" }),
                    )
                    .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("通过 SSH 连接一台运行 Docker 的服务器")),
            );

        let close = div()
            .id("close-form")
            .absolute()
            .top(px(10.0))
            .right(px(10.0))
            .size(px(26.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(t::RADIUS_MD))
            .cursor_pointer()
            .hover(|s| s.bg(c::hover()))
            .child(icon(IconName::X, 14.0, c::muted()))
            .on_click(cx.listener(|this, _, _, cx| this.close_form(cx)));

        let dialog = div()
            .id("node-dialog")
            .w(px(500.0))
            .max_h_full()
            .flex()
            .flex_col()
            .rounded(px(t::RADIUS_2XL))
            .border_1()
            .border_color(c::dialog_border())
            .bg(c::dialog())
            .shadow_2xl()
            .overflow_hidden()
            .relative()
            .occlude()
            .child(header)
            .child(div().id("form-body").flex_1().min_h_0().overflow_y_scroll().child(body))
            .child(footer)
            .child(close);

        Some(
            div()
                .id("dialog-backdrop")
                .absolute()
                .top(px(super::TITLEBAR_H))
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .items_center()
                .justify_center()
                .p(px(24.0))
                .bg(c::scrim_dialog())
                // 遮罩不响应点击关闭(拖动窗口、误触都不会丢掉已填内容);只拦住后面的界面
                .occlude()
                // Esc 关闭、Enter 提交:捕获阶段拦截,输入框里按也生效
                .capture_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                    "escape" => {
                        this.close_form(cx);
                        cx.stop_propagation();
                    }
                    "enter" => {
                        this.submit_form(cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }))
                .child(dialog),
        )
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
