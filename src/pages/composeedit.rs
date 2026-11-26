//! 编辑堆栈的 compose 文件。
//!
//! 文件位置从容器标签里读(`docker inspect`),内容用 SSH 读写;保存只写文件,
//! 「重新部署」另外一个按钮(`docker compose up -d`),确认后才会影响正在运行的服务。

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Editor, EditorState, Input, InputState};

use super::super::*;
use super::dockerpages::DockAsk;
use crate::ctfiles::CtKind;
use crate::docker::Action;
use crate::sysops::{self, Job, Kind};

#[derive(Default)]
pub(crate) enum Phase {
    /// 读文件位置 / 文件内容中
    #[default]
    Loading,
    Ready,
    Err(String),
}

/// 新建堆栈
#[derive(Default)]
pub(crate) struct CreateUi {
    open: bool,
    name: Option<Entity<InputState>>,
    path: Option<Entity<InputState>>,
    /// 上一次自动填进「存储地址」的值:地址还等于它,说明用户没改过,可以跟着名称变
    last_auto: String,
    editor: Option<Entity<EditorState>>,
    env_editor: Option<Entity<EditorState>>,
    tab: Tab,
    creating: bool,
}

const DEFAULT_ROOT: &str = "/opt/otter/docker/compose";

const NEW_TEMPLATE: &str = "services:
  app:
    image: nginx:alpine
    restart: unless-stopped
    ports:
      - \"8080:80\"
";

#[derive(Clone, Copy, PartialEq, Default)]
enum Tab {
    #[default]
    Compose,
    Env,
}

#[derive(Default)]
pub(crate) struct CompUi {
    tab: Tab,
    /// env 文件:路径、内容状态(切到这个标签才读)、编辑器、原内容、写入要不要管理员权限
    env_path: String,
    env_started: bool,
    env_phase: Phase,
    env_missing: bool,
    env_root: bool,
    env_retried: bool,
    env_editor: Option<Entity<EditorState>>,
    env_orig: String,
    /// 正在编辑的堆栈
    open: Option<String>,
    phase: Phase,
    /// compose 文件和它所在的目录
    path: String,
    dir: String,
    /// 堆栈由多个文件组成时,其余的文件(这里只编辑第一个)
    others: Vec<String>,
    /// 启动时指定的 env 文件(`--env-file`),重新部署要原样带上
    envs: Vec<String>,
    /// 写入时要不要用管理员权限(文件当前登录用户写不了)
    root: bool,
    /// 读文件时已经用过管理员权限重试
    retried: bool,
    editor: Option<Entity<EditorState>>,
    /// 重新部署时强制拉取最新镜像
    pull: bool,
    /// 「应用」正在执行(收到结果后关闭抽屉或恢复按钮)
    pub(super) applying: bool,
    pub(super) create: CreateUi,
    /// 打开(或上次保存)时的内容,用来判断有没有改动
    orig: String,
}

/// `docker inspect` 读 compose 标签:`文件1,文件2|工作目录`
/// 文件列表|工作目录|env 文件列表(旧版 compose 没有最后一项)
const LABEL_FMT: &str = r#"{{index .Config.Labels "com.docker.compose.project.config_files"}}|{{index .Config.Labels "com.docker.compose.project.working_dir"}}|{{index .Config.Labels "com.docker.compose.project.environment_file"}}"#;

/// 读文件内容:第一行标出当前用户能不能写它(`@@W` 能 / `@@R` 不能),后面是内容
fn read_script(path: &str) -> String {
    let q = sysops::quote(path);
    format!("f={q}; if [ -w \"$f\" ]; then echo '@@W'; else echo '@@R'; fi; cat -- \"$f\"")
}

/// 读 env 文件:第一行 `@@W`/`@@R` 同上(文件不存在时看目录能不能写),`@@N` 表示文件不存在,后面是内容
fn env_read_script(path: &str) -> String {
    let q = sysops::quote(path);
    format!(
        "f={q}; if [ -e \"$f\" ]; then t=\"$f\"; else t=$(dirname -- \"$f\"); fi; if [ -w \"$t\" ]; then w=W; else w=R; fi; if [ -e \"$f\" ]; then echo \"@@$w\"; else echo \"@@N$w\"; fi; if [ -f \"$f\" ]; then cat -- \"$f\"; fi"
    )
}

/// 写文件:用 `cat >` 原地写入,保留文件的属主和权限
fn write_script(path: &str) -> String {
    let q = sysops::quote(path);
    format!("f={q}; cat > \"$f\"")
}

impl Home {
    fn comp_docker(&self) -> Option<&crate::docker::Docker> {
        self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref())
    }

    pub(crate) fn comp_open(&mut self, stack: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.comp.editor.is_none() {
            let ed = cx.new(|cx| EditorState::new(window, cx).language("yaml").line_number(true));
            cx.observe(&ed, |_, _, cx| cx.notify()).detach();
            self.dock.comp.editor = Some(ed);
        }
        if self.dock.comp.env_editor.is_none() {
            let ed = cx.new(|cx| EditorState::new(window, cx).language("yaml").line_number(true));
            cx.observe(&ed, |_, _, cx| cx.notify()).detach();
            self.dock.comp.env_editor = Some(ed);
        }
        let c = &mut self.dock.comp;
        c.tab = Tab::Compose;
        c.env_path.clear();
        c.env_started = false;
        c.env_phase = Phase::Loading;
        c.env_missing = false;
        c.env_root = false;
        c.env_retried = false;
        c.env_orig.clear();
        if let Some(ed) = c.env_editor.clone() {
            ed.update(cx, |s, cx| s.set_value("", window, cx));
        }
        c.open = Some(stack.to_string());
        c.phase = Phase::Loading;
        c.path.clear();
        c.dir.clear();
        c.others.clear();
        c.envs.clear();
        c.root = false;
        c.retried = false;
        c.pull = false;
        c.applying = false;
        c.orig.clear();
        if let Some(ed) = c.editor.clone() {
            ed.update(cx, |s, cx| s.set_value("", window, cx));
        }
        if self.demo {
            let (path, dir) = (format!("/opt/stacks/{stack}/docker-compose.yml"), format!("/opt/stacks/{stack}"));
            self.comp_ready(path, dir, DEMO_COMPOSE.replace("{name}", stack), false, window, cx);
            return;
        }
        let name = self.dock.snap.containers.iter().find(|c| c.project.as_deref() == Some(stack)).map(|c| c.name.clone());
        match (name, self.comp_docker()) {
            (Some(n), Some(d)) => d.ct_load(CtKind::Compose, stack, ["inspect", "-f", LABEL_FMT, &n].map(String::from).to_vec(), false),
            _ => self.dock.comp.phase = Phase::Err("找不到这个堆栈的容器".into()),
        }
        // 丢掉上次没取走的结果
        if let Some(d) = self.comp_docker() {
            let _ = d.sys_take(Kind::ComposeFile);
            let _ = d.sys_take(Kind::ComposeEnv);
        }
        cx.notify();
    }

    /// 收到「应用」的结果:成功关闭抽屉,失败恢复按钮(失败原因由通知条显示)
    pub(super) fn comp_applied(&mut self, notice: &str, cx: &mut Context<Self>) {
        if self.dock.comp.create.creating {
            if notice.starts_with("已创建") {
                self.dock.comp.create.creating = false;
                self.dock.comp.create.open = false;
                cx.notify();
            } else if notice.starts_with("创建失败") {
                self.dock.comp.create.creating = false;
            }
        }
        if !self.dock.comp.applying {
            return;
        }
        if notice.starts_with("已应用") {
            self.dock.comp.applying = false;
            self.comp_close(cx);
        } else if notice.starts_with("应用失败") {
            self.dock.comp.applying = false;
        }
    }

    fn comp_close(&mut self, cx: &mut Context<Self>) {
        self.dock.comp.open = None;
        cx.notify();
    }

    fn comp_ready(&mut self, path: String, dir: String, text: String, root: bool, window: &mut Window, cx: &mut Context<Self>) {
        let c = &mut self.dock.comp;
        c.path = path;
        c.dir = dir;
        c.env_path = match c.envs.first() {
            Some(e) if e.starts_with('/') => e.clone(),
            Some(e) => format!("{}/{e}", c.dir),
            None => format!("{}/.env", c.dir),
        };
        c.root = root;
        c.orig = text.clone();
        c.phase = Phase::Ready;
        if let Some(ed) = c.editor.clone() {
            ed.update(cx, |s, cx| s.set_value(&text, window, cx));
            let f = ed.focus_handle(cx);
            window.focus(&f, cx);
        }
    }

    fn comp_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let c = &mut self.dock.comp;
        if !matches!(c.phase, Phase::Ready) {
            return;
        }
        c.tab = tab;
        if tab == Tab::Env && !c.env_started {
            c.env_started = true;
            c.env_phase = Phase::Loading;
            if self.demo {
                self.env_ready("TZ=Asia/Shanghai\nPOSTGRES_PASSWORD=example\n".into(), false, false, window, cx);
            } else {
                let script = env_read_script(&self.dock.comp.env_path);
                if let Some(d) = self.comp_docker() {
                    d.sys_load(Kind::ComposeEnv, &script, false);
                }
            }
        }
        cx.notify();
    }

    fn env_ready(&mut self, text: String, missing: bool, root: bool, window: &mut Window, cx: &mut Context<Self>) {
        let c = &mut self.dock.comp;
        c.env_phase = Phase::Ready;
        c.env_missing = missing;
        c.env_root = root;
        c.env_orig = text.clone();
        if let Some(ed) = c.env_editor.clone() {
            ed.update(cx, |s, cx| s.set_value(&text, window, cx));
            if c.tab == Tab::Env {
                let f = ed.focus_handle(cx);
                window.focus(&f, cx);
            }
        }
    }

    fn env_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.demo || !self.dock.comp.env_started || !matches!(self.dock.comp.env_phase, Phase::Loading) {
            return;
        }
        let Some(res) = self.comp_docker().and_then(|d| d.sys_take(Kind::ComposeEnv)) else { return };
        match res {
            Ok(t) => {
                let (head, body) = t.split_once('\n').unwrap_or((t.as_str(), ""));
                let head = head.trim();
                let (missing, root) = (head.starts_with("@@N"), head.ends_with('R') || self.dock.comp.env_retried);
                self.env_ready(body.to_string(), missing, root, window, cx);
            }
            Err(e) if !self.dock.comp.env_retried && e.to_lowercase().contains("permission") => {
                self.dock.comp.env_retried = true;
                let script = env_read_script(&self.dock.comp.env_path);
                if let Some(d) = self.comp_docker() {
                    d.sys_load(Kind::ComposeEnv, &script, true);
                }
            }
            Err(e) => self.dock.comp.env_phase = Phase::Err(e),
        }
    }

    /// 每次渲染前调用:收后台读到的文件位置和内容
    pub(crate) fn comp_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.create_sync(window, cx);
        self.api_sync(window, cx);
        let Some(stack) = self.dock.comp.open.clone() else { return };
        self.env_sync(window, cx);
        if self.demo || !matches!(self.dock.comp.phase, Phase::Loading) {
            return;
        }
        let info = self.comp_docker().and_then(|d| d.ct_take(CtKind::Compose));
        if let Some((tag, res)) = info {
            if tag == stack {
                match res {
                    Err(e) => self.dock.comp.phase = Phase::Err(e),
                    Ok(t) => {
                        let mut parts = t.trim().splitn(3, '|');
                        let (files, dir, envs) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                        self.dock.comp.envs = envs.split(',').map(str::trim).filter(|f| !f.is_empty()).map(String::from).collect();
                        let mut list = files.split(',').map(str::trim).filter(|f| !f.is_empty());
                        match list.next() {
                            None => self.dock.comp.phase = Phase::Err("这个堆栈的容器没有记录 compose 文件的位置,可能不是用 docker compose 启动的".into()),
                            Some(first) if !first.starts_with('/') => self.dock.comp.phase = Phase::Err(format!("compose 文件路径不是绝对路径:{first}")),
                            Some(first) => {
                                self.dock.comp.path = first.to_string();
                                self.dock.comp.dir = if dir.is_empty() { first.rsplit_once('/').map(|x| x.0.to_string()).unwrap_or_default() } else { dir.to_string() };
                                self.dock.comp.others = list.map(String::from).collect();
                                let script = read_script(first);
                                if let Some(d) = self.comp_docker() {
                                    d.sys_load(Kind::ComposeFile, &script, false);
                                }
                            }
                        }
                    }
                }
            }
        }
        let file = self.comp_docker().and_then(|d| d.sys_take(Kind::ComposeFile));
        if let Some(res) = file {
            match res {
                Ok(t) => {
                    let (head, body) = t.split_once('\n').unwrap_or((t.as_str(), ""));
                    let (path, dir, root) = (self.dock.comp.path.clone(), self.dock.comp.dir.clone(), head.trim() == "@@R" || self.dock.comp.retried);
                    self.comp_ready(path, dir, body.to_string(), root, window, cx);
                }
                Err(e) if !self.dock.comp.retried && e.to_lowercase().contains("permission") => {
                    // 当前用户读不了:用管理员权限再读一次
                    self.dock.comp.retried = true;
                    let script = read_script(&self.dock.comp.path);
                    if let Some(d) = self.comp_docker() {
                        d.sys_load(Kind::ComposeFile, &script, true);
                    }
                }
                Err(e) => self.dock.comp.phase = Phase::Err(e),
            }
        }
    }

    fn tab_text(&self, tab: Tab, cx: &App) -> String {
        let c = &self.dock.comp;
        let ed = if tab == Tab::Env { &c.env_editor } else { &c.editor };
        ed.as_ref().map(|e| e.read(cx).value().to_string()).unwrap_or_default()
    }

    /// 这个标签的内容有没有改动(env 没读出来就没有改动)
    fn tab_dirty(&self, tab: Tab, cx: &App) -> bool {
        let c = &self.dock.comp;
        match tab {
            Tab::Compose => matches!(c.phase, Phase::Ready) && self.tab_text(tab, cx) != c.orig,
            Tab::Env => c.env_started && matches!(c.env_phase, Phase::Ready) && self.tab_text(tab, cx) != c.env_orig,
        }
    }

    fn comp_dirty(&self, cx: &App) -> bool {
        self.tab_dirty(self.dock.comp.tab, cx)
    }

    fn tab_save(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let mut text = self.tab_text(tab, cx);
        if tab == Tab::Compose && text.trim().is_empty() {
            self.dock.notice = Some(("compose 文件不能是空的".into(), std::time::Instant::now()));
            cx.notify();
            return;
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let c = &mut self.dock.comp;
        let (path, root) = if tab == Tab::Env {
            c.env_orig = text.clone();
            c.env_missing = false;
            (c.env_path.clone(), c.env_root)
        } else {
            c.orig = text.clone();
            (c.path.clone(), c.root)
        };
        let job = Job { label: "保存".into(), target: path.clone(), script: write_script(&path), root, input: Some(text), reload: None };
        self.dock_act(Action::Sys(job), cx);
    }

    fn comp_save(&mut self, cx: &mut Context<Self>) {
        self.tab_save(self.dock.comp.tab, cx);
    }

    fn comp_redeploy(&mut self, cx: &mut Context<Self>) {
        let Some(stack) = self.dock.comp.open.clone() else { return };
        // 有没保存的修改先保存;确认弹窗要等用户点,写文件早就完成了
        for tab in [Tab::Compose, Tab::Env] {
            if self.tab_dirty(tab, cx) {
                self.tab_save(tab, cx);
            }
        }
        let action = Action::ComposeUp { project: stack.clone(), dir: self.dock.comp.dir.clone(), files: std::iter::once(self.dock.comp.path.clone()).chain(self.dock.comp.others.clone()).collect(), envs: self.dock.comp.envs.clone(), pull: self.dock.comp.pull };
        self.ask(
            DockAsk {
                title: "应用到堆栈",
                tile: IconName::Layers,
                name: stack,
                detail: "Docker Compose",
                body: "按 compose 文件和 env 重新创建有变化的服务,这些服务会短暂重启;没有变化的服务不受影响",
                label: "应用",
                action,
                list: Vec::new(),
            },
            cx,
        );
    }

    /// 编辑抽屉
    pub(crate) fn comp_dialog(&self, window: &Window, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let stack = self.dock.comp.open.clone()?;
        let c = &self.dock.comp;
        let tab = c.tab;
        let (phase, editor, path) = if tab == Tab::Env { (&c.env_phase, c.env_editor.clone(), &c.env_path) } else { (&c.phase, c.editor.clone(), &c.path) };
        let ready = matches!(phase, Phase::Ready);
        let dirty = self.comp_dirty(cx);
        let w = (f32::from(window.viewport_size().width) * 0.62).clamp(520.0, 820.0);
        let sub = if path.is_empty() { "正在读取…".to_string() } else { path.clone() };
        let info = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .gap(px(12.0))
            .mx(px(16.0))
            .mt(px(14.0))
            .child(field_col("堆栈名称", Some(190.0), div().truncate().child(stack.clone()).into_any_element(), false, true))
            .child(field_col("文件路径", None, div().min_w_0().truncate().child(sub).into_any_element(), true, true));
        let head = div()
            .h(px(52.0))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(16.0))
            .border_b_1()
            .border_color(c::border())
            .child(icon(IconName::Layers, 16.0, c::secondary()))
            .child(div().flex_shrink_0().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(format!("编辑 {stack}")))
            .child(div().w(px(4.0)))
            .child(super::syspages::chip(("comp-tab", 0), "Compose", tab == Tab::Compose).on_click(cx.listener(|this, _, window, cx| this.comp_tab(Tab::Compose, window, cx))))
            .child(super::syspages::chip(("comp-tab", 1), ".env", tab == Tab::Env).on_click(cx.listener(|this, _, window, cx| this.comp_tab(Tab::Env, window, cx))))
            .child(div().flex_1())
            .child(super::syspages::tip(super::dockerpages::icon_btn(("comp-x", 0), IconName::X, c::muted()).on_click(cx.listener(|this, _, _, cx| this.comp_close(cx))), "关闭"));
        let hint = if ready && tab == Tab::Compose {
            managed_hint(&c.path, c.others.len())
        } else if ready && c.env_missing {
            Some("这个 env 文件还不存在,保存时会创建".to_string())
        } else {
            None
        };
        let hint = hint.map(|h| {
            div()
                .flex_shrink_0()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(8.0))
                .mx(px(16.0))
                .mt(px(12.0))
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(t::RADIUS_MD))
                .bg(c::warn().opacity(0.12))
                .child(div().pt(px(1.0)).child(icon(IconName::TriangleAlert, 14.0, c::warn())))
                .child(div().flex_1().min_w_0().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(h))
        });
        let body: AnyElement = match phase {
            Phase::Loading => crate::ui::loading("正在读取…").into_any_element(),
            Phase::Err(e) => div()
                .flex_1()
                .px(px(32.0))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(icon(IconName::TriangleAlert, 24.0, c::warn()))
                .child(div().text_center().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(e.clone()))
                .into_any_element(),
            Phase::Ready => div()
                .flex_1()
                .min_h_0()
                .m(px(16.0))
                .rounded(px(t::RADIUS_MD))
                .border_1()
                .border_color(c::field_border())
                .bg(c::field())
                .overflow_hidden()
                .children(editor.map(|e| Editor::new(&e).h_full()))
                .into_any_element(),
        };
        let applying = c.applying;
        let on = c.pull;
        let pull = div()
            .id("comp-pull")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .child(
                div()
                    .size(px(16.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .border_1()
                    .when(on, |d| d.bg(c::accent()).border_color(c::accent()))
                    .when(!on, |d| d.border_color(c::field_border()))
                    .children(on.then(|| icon(IconName::Check, 12.0, c::on_accent()))),
            )
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("强制拉取镜像"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.dock.comp.pull = !this.dock.comp.pull;
                cx.notify();
            }));
        let save = div()
            .id("comp-save")
            .h(px(34.0))
            .px(px(16.0))
            .flex()
            .items_center()
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .when(ready && dirty, |d| d.bg(c::accent()).text_color(c::on_accent()).cursor_pointer().hover(|s| s.opacity(0.88)))
            .when(!(ready && dirty), |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()))
            .child("保存")
            .on_click(cx.listener(|this, _, _, cx| {
                if this.comp_dirty(cx) {
                    this.comp_save(cx);
                }
            }));
        let foot = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(16.0))
            .py(px(12.0))
            .border_t_1()
            .border_color(c::border())
            .child(pull)
            .child(div().flex_1())
            .child(
                crate::node_form::ctl_btn("comp-redeploy", None, if applying { "应用中…" } else { "应用" }, 34.0)
                    .when(applying, |d| d.opacity(0.6))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.dock.comp.applying {
                            this.comp_redeploy(cx);
                        }
                    })),
            )
            .child(save);
        Some(
            crate::ui::drawer("comp-drawer", w, cx.listener(|this, _, _, cx| this.comp_close(cx)), [head.into_any_element(), info.into_any_element()].into_iter().chain(hint.map(|h| h.into_any_element())).chain([body, foot.into_any_element()])).on_key_down(
                cx.listener(|this, e: &KeyDownEvent, _, cx| {
                    if e.keystroke.key == "escape" {
                        this.comp_close(cx);
                        cx.stop_propagation();
                    } else if e.keystroke.key == "s" && e.keystroke.modifiers.platform && this.comp_dirty(cx) {
                        this.comp_save(cx);
                        cx.stop_propagation();
                    }
                }),
            ),
        )
    }
}

/// 表单里「标签 + 一个框」的一列;`w` 为空时占满剩余宽度
pub(super) fn field_col(label: impl Into<SharedString>, w: Option<f32>, inner: AnyElement, mono: bool, dim: bool) -> Div {
    field_col_with(label, w, inner, mono, dim, None)
}

/// 同 `field_col`,框内右侧可以再放一个后缀(单位、小按钮等),不占框外的宽度
pub(super) fn field_col_with(label: impl Into<SharedString>, w: Option<f32>, inner: AnyElement, mono: bool, dim: bool, suffix: Option<AnyElement>) -> Div {
    div()
        .flex_shrink_0()
        .when(w.is_none(), |d| d.flex_1().min_w_0())
        .when_some(w, |d, w| d.w(px(w)))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(label.into()))
        .child(
            div()
                .h(px(34.0))
                .flex()
                .flex_row()
                .items_center()
                // 输入组件自带左右各约 8px 的内边距,这里只补足到 12px,不再叠加
                .pl(px(4.0))
                .pr(px(6.0))
                .rounded(px(t::RADIUS_MD))
                .bg(c::field())
                .border_1()
                .border_color(c::field_border())
                .text_size(px(t::TYPE_CONTROL))
                .when(mono, |d| d.font_family(MONO))
                .when(dim, |d| d.text_color(c::muted()).opacity(0.8))
                .child(inner)
                .children(suffix.map(|x| div().flex_shrink_0().child(x))),
        )
}

/// 文件看起来由其他程序管理,或堆栈由多个文件组成时的提示
fn managed_hint(path: &str, extra: usize) -> Option<String> {
    let low = path.to_lowercase();
    let mut v: Vec<String> = Vec::new();
    if low.contains("/1panel/") && low.contains("/apps/") {
        v.push("这是 1Panel 应用商店装的应用,1Panel 升级或修改参数时会重新生成这个文件,你的改动可能被覆盖".into());
    } else if low.contains("portainer") || low.contains("/data/compose/") {
        v.push("这个堆栈可能由 Portainer 管理,它会用自己保存的内容覆盖这个文件".into());
    }
    if extra > 0 {
        v.push(format!("这个堆栈由 {} 个 compose 文件组成,这里只编辑第一个;应用时会带上全部文件", extra + 1));
    }
    (!v.is_empty()).then(|| v.join("\n"))
}

const DEMO_COMPOSE: &str = "services:
  app:
    image: nginx:alpine
    container_name: {name}-app
    restart: unless-stopped
    ports:
      - \"8080:80\"
    volumes:
      - ./html:/usr/share/nginx/html:ro
    environment:
      - TZ=Asia/Shanghai
  db:
    image: postgres:16
    restart: unless-stopped
    environment:
      POSTGRES_PASSWORD: example
    volumes:
      - db-data:/var/lib/postgresql/data

volumes:
  db-data:
";

// ---- 新建堆栈 ----------------------------------------------------------------

fn default_dir(name: &str) -> String {
    format!("{DEFAULT_ROOT}/{name}")
}

impl Home {
    pub(crate) fn create_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let c = &mut self.dock.comp.create;
        if c.name.is_none() {
            let n = cx.new(|cx| InputState::new(window, cx).placeholder("小写字母、数字、- 和 _,如 myapp"));
            cx.observe(&n, |_, _, cx| cx.notify()).detach();
            let p = cx.new(|cx| InputState::new(window, cx).placeholder("堆栈文件所在的目录"));
            cx.observe(&p, |_, _, cx| cx.notify()).detach();
            let e = cx.new(|cx| EditorState::new(window, cx).language("yaml").line_number(true));
            cx.observe(&e, |_, _, cx| cx.notify()).detach();
            c.name = Some(n);
            c.path = Some(p);
            let ev = cx.new(|cx| EditorState::new(window, cx).language("yaml").line_number(true));
            cx.observe(&ev, |_, _, cx| cx.notify()).detach();
            c.editor = Some(e);
            c.env_editor = Some(ev);
        }
        c.open = true;
        c.creating = false;
        c.tab = Tab::Compose;
        c.last_auto = default_dir("");
        let (n, p, e) = (c.name.clone(), c.path.clone(), c.editor.clone());
        let auto = c.last_auto.clone();
        if let (Some(n), Some(p), Some(e)) = (n, p, e) {
            n.update(cx, |s, cx| s.set_value("", window, cx));
            p.update(cx, |s, cx| s.set_value(&auto, window, cx));
            e.update(cx, |s, cx| s.set_value(NEW_TEMPLATE, window, cx));
            if let Some(ev) = self.dock.comp.create.env_editor.clone() {
                ev.update(cx, |s, cx| s.set_value("", window, cx));
            }
            let f = n.focus_handle(cx);
            window.focus(&f, cx);
        }
        cx.notify();
    }

    fn create_close(&mut self, cx: &mut Context<Self>) {
        self.dock.comp.create.open = false;
        cx.notify();
    }

    /// 存储地址没被用户改过时,跟着名称自动变
    fn create_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let c = &self.dock.comp.create;
        if !c.open {
            return;
        }
        let (Some(n), Some(p)) = (c.name.clone(), c.path.clone()) else { return };
        let auto = default_dir(n.read(cx).value().trim());
        let cur = p.read(cx).value().to_string();
        if cur == c.last_auto && cur != auto {
            self.dock.comp.create.last_auto = auto.clone();
            p.update(cx, |s, cx| s.set_value(&auto, window, cx));
        }
    }

    fn create_submit(&mut self, cx: &mut Context<Self>) {
        let c = &self.dock.comp.create;
        if c.creating {
            return;
        }
        let (Some(n), Some(p), Some(e)) = (c.name.clone(), c.path.clone(), c.editor.clone()) else { return };
        let name = n.read(cx).value().trim().to_string();
        let dir = p.read(cx).value().trim().to_string();
        let mut text = e.read(cx).value().to_string();
        let mut env = c.env_editor.clone().map(|v| v.read(cx).value().to_string()).unwrap_or_default();
        let err = if !crate::docker::valid_project(&name) {
            Some("堆栈名只能用小写字母、数字、- 和 _,并以字母或数字开头")
        } else if !dir.starts_with('/') || dir.chars().any(|c| c.is_control()) {
            Some("存储地址要是以 / 开头的绝对路径")
        } else if text.trim().is_empty() {
            Some("compose 内容不能是空的")
        } else if self.node().stacks.iter().any(|s| s.name == name) {
            Some("已经有同名的堆栈")
        } else {
            None
        };
        if let Some(m) = err {
            self.dock.notice = Some((m.into(), std::time::Instant::now()));
            cx.notify();
            return;
        }
        if !text.ends_with('\n') {
            text.push('\n');
        }
        if !self.demo {
            self.dock.comp.create.creating = true;
        }
        if !env.trim().is_empty() && !env.ends_with('\n') {
            env.push('\n');
        }
        if env.trim().is_empty() {
            env.clear();
        }
        self.dock_act(Action::ComposeCreate { project: name, dir, text, env }, cx);
    }

    /// 新建抽屉
    pub(crate) fn create_drawer(&self, window: &Window, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let c = &self.dock.comp.create;
        if !c.open {
            return None;
        }
        let w = (f32::from(window.viewport_size().width) * 0.62).clamp(520.0, 820.0);
        let creating = c.creating;
        let name = c.name.clone().map(|v| v.read(cx).value().trim().to_string()).unwrap_or_default();
        let dir = c.path.clone().map(|v| v.read(cx).value().trim().to_string()).unwrap_or_default();
        let head = div()
            .h(px(52.0))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(16.0))
            .border_b_1()
            .border_color(c::border())
            .child(icon(IconName::Layers, 16.0, c::secondary()))
            .child(div().flex_1().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child("创建堆栈"))
            .child(super::syspages::tip(super::dockerpages::icon_btn(("create-x", 0), IconName::X, c::muted()).on_click(cx.listener(|this, _, _, cx| this.create_close(cx))), "关闭"));
        let input = |i: Option<Entity<InputState>>| i.map(|i| div().flex_1().min_w_0().child(Input::new(&i).appearance(false).with_size(px(12.0 / 0.875))).into_any_element()).unwrap_or_else(|| div().into_any_element());
        let tab = c.tab;
        let tabs = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(super::syspages::chip(("create-tab", 0), "docker-compose.yml", tab == Tab::Compose).on_click(cx.listener(|this, _, _, cx| {
                this.dock.comp.create.tab = Tab::Compose;
                cx.notify();
            })))
            .child(super::syspages::chip(("create-tab", 1), ".env", tab == Tab::Env).on_click(cx.listener(|this, _, _, cx| {
                this.dock.comp.create.tab = Tab::Env;
                cx.notify();
            })));
        let ed = if tab == Tab::Env { c.env_editor.clone() } else { c.editor.clone() };
        let editor = div().flex_1().min_h_0().flex().flex_col().gap(px(8.0)).child(tabs).child(
            div().flex_1().min_h_0().rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).bg(c::field()).overflow_hidden().children(ed.map(|e| Editor::new(&e).h_full())),
        );
        let row = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .gap(px(12.0))
            .child(field_col("堆栈名称", Some(190.0), input(c.name.clone()), false, false))
            .child(field_col("存储地址", None, input(c.path.clone()), true, false));
        let body = div().flex_1().min_h_0().flex().flex_col().gap(px(14.0)).p(px(16.0)).child(row).child(editor);
        let hint = if name.is_empty() { String::new() } else { format!("将创建 {}/docker-compose.yml 并启动", dir.trim_end_matches('/')) };
        let create = div()
            .id("create-ok")
            .h(px(34.0))
            .px(px(18.0))
            .flex()
            .items_center()
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .bg(c::accent())
            .text_color(c::on_accent())
            .cursor_pointer()
            .hover(|s| s.opacity(0.88))
            .when(creating, |d| d.opacity(0.6))
            .gap(px(8.0))
            .when(creating, |d| d.child(crate::ui::dot_spinner(("create-ok-spin", 0), c::on_accent())))
            .child(if creating { "创建中" } else { "创建" })
            .on_click(cx.listener(|this, _, _, cx| this.create_submit(cx)));
        let foot = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(16.0))
            .py(px(12.0))
            .border_t_1()
            .border_color(c::border())
            .child(div().flex_1().min_w_0().truncate().font_family(MONO).text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(hint))
            .child(create);
        Some(crate::ui::drawer("create-drawer", w, cx.listener(|this, _, _, cx| this.create_close(cx)), [head.into_any_element(), body.into_any_element(), foot.into_any_element()]).on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
            if e.keystroke.key == "escape" {
                this.create_close(cx);
                cx.stop_propagation();
            }
        })))
    }
}
