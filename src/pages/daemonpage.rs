//! 「配置」页:读写服务器上的 Docker 守护进程配置(`/etc/docker/daemon.json`)。
//!
//! 常用项的表单(镜像加速、日志、数据目录、不安全仓库、live-restore);顶部的开关切到「原始 JSON」,
//! 整个 daemon.json 变成文本编辑器。表单没涉及的键原样保留。保存前先备份,保存并重启要二次确认。表单模型和校验在 `crate::daemon_conf`。

use std::time::Instant;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Editor, EditorState, Input, InputState, Textarea, TextareaState};
use serde_json::Value;

use super::super::*;
use super::dockerpages::DockAsk;
use crate::daemon_conf::{self, Form};
use crate::docker::Action;

const DRIVERS: [(&str, &str); 5] = [("默认", ""), ("json-file", "json-file"), ("local", "local"), ("journald", "journald"), ("syslog", "syslog")];
const DEMO_TEXT: &str = "{\n  \"registry-mirrors\": [\"https://mirror.example.com\"],\n  \"log-driver\": \"json-file\",\n  \"log-opts\": {\"max-size\": \"10m\", \"max-file\": \"3\"},\n  \"live-restore\": true\n}\n";

/// 配置页的状态(挂在 `DockUi` 上)
#[derive(Default)]
pub(crate) struct ConfUi {
    /// 镜像加速、不安全仓库、数据目录、日志大小、日志个数
    inputs: Option<Vec<Entity<InputState>>>,
    /// 镜像加速、HTTP 仓库:一行一个
    mirrors: Option<Entity<TextareaState>>,
    http: Option<Entity<TextareaState>>,
    /// 读到的完整 JSON(保存时在它上面改,保留表单外的键)
    base: Option<Value>,
    /// 读到时的表单,和现在的表单比较得出「有改动」
    orig: Form,
    exists: bool,
    loaded: bool,
    requested: bool,
    err: Option<String>,
    driver: String,
    live: bool,
    /// 原始 JSON 模式:下面是整个文件的编辑器
    raw_mode: bool,
    raw: Option<Entity<EditorState>>,
    /// 读到时的文件内容(规范化后),原始模式用它判断有没有改动
    orig_text: String,
    /// 表单模式下整页一起滚,保存栏钉在下面:配置页把栏放在这里,`content` 取走
    pub(crate) bar: Option<AnyElement>,
}

impl Home {
    /// 配置页是不是「整页一起滚、保存栏钉在下面」:读到了配置、没出错、不是原始 JSON 模式(编辑器自己定高滚动)
    pub(crate) fn conf_scrolls_whole(&self) -> bool {
        self.page == Page::Config && self.dock.conf.loaded && self.dock.conf.err.is_none() && !self.dock.conf.raw_mode
    }

    /// 每次渲染前调用(配置页在前台时):建输入框、请求并接收 daemon.json
    pub(crate) fn conf_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Config {
            return;
        }
        if self.dock.conf.inputs.is_none() {
            let hints = ["/var/lib/docker", "10m", "3"];
            let v = hints
                .iter()
                .map(|h| {
                    let e = cx.new(|cx| InputState::new(window, cx).placeholder(*h));
                    cx.observe(&e, |_, _, cx| cx.notify()).detach();
                    e
                })
                .collect();
            self.dock.conf.inputs = Some(v);
            let area = |window: &mut Window, cx: &mut Context<Self>| {
                let e = cx.new(|cx| TextareaState::new(window, cx).rows(3));
                cx.observe(&e, |_, _, cx| cx.notify()).detach();
                e
            };
            self.dock.conf.mirrors = Some(area(window, cx));
            self.dock.conf.http = Some(area(window, cx));
            let raw = cx.new(|cx| EditorState::new(window, cx).language("json").line_number(false));
            // 键用主色,字符串绿,数字橙,true/false/null 紫
            let colors = [c::accent(), c::online(), c::warn(), gpui::rgba(0xD79BFFFF)];
            raw.update(cx, |s, cx| s.set_highlighter_factory(std::rc::Rc::new(move |_| Some(Box::new(crate::json_hl::JsonHighlighter::new(colors)))), cx));
            cx.observe(&raw, |_, _, cx| cx.notify()).detach();
            self.dock.conf.raw = Some(raw);
        }
        let got = if self.demo {
            (!self.dock.conf.loaded).then(|| Ok((true, DEMO_TEXT.to_string())))
        } else {
            let Some(d) = self.workspace.active.and_then(|i| self.workspace.tabs.get(i)).and_then(|t| t.docker.as_ref()) else { return };
            if !self.dock.conf.requested {
                self.dock.conf.requested = true;
                d.load_daemon();
            }
            d.take_daemon()
        };
        match got {
            Some(Ok((exists, text))) => self.conf_apply(exists, &text, window, cx),
            Some(Err(e)) => {
                self.dock.conf.err = Some(e);
                self.dock.conf.loaded = false;
                cx.notify();
            }
            None => {}
        }
    }

    fn conf_apply(&mut self, exists: bool, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        match daemon_conf::parse(text) {
            Ok((base, f)) => {
                self.conf_fill(&f, window, cx);
                let c = &mut self.dock.conf;
                c.driver = f.driver.clone();
                c.live = f.live_restore;
                c.base = Some(base);
                c.orig = f;
                c.exists = exists;
                c.loaded = true;
                c.err = None;
                c.orig_text = daemon_conf::render(c.base.as_ref().unwrap_or(&Value::Null), &c.orig).unwrap_or_else(|_| text.to_string());
                // 调试:OTTER_CONF_RAW=1 截图时直接进原始 JSON 模式
                if std::env::var("OTTER_CONF_RAW").is_ok() {
                    c.raw_mode = true;
                }
                if c.raw_mode {
                    if let Some(r) = c.raw.clone() {
                        let t = c.orig_text.clone();
                        r.update(cx, |s, cx| s.set_value(t.as_str(), window, cx));
                    }
                }
            }
            Err(e) => {
                self.dock.conf.err = Some(e);
                self.dock.conf.loaded = false;
            }
        }
        cx.notify();
    }

    /// 重新读一遍(刷新按钮)
    pub(crate) fn conf_reload(&mut self) {
        self.dock.conf.requested = false;
        self.dock.conf.loaded = false;
        self.dock.conf.err = None;
    }

    fn conf_form(&self, cx: &App) -> Form {
        let c = &self.dock.conf;
        let get = |i: usize| c.inputs.as_ref().and_then(|v| v.get(i)).map(|e| e.read(cx).value().trim().to_string()).unwrap_or_default();
        let area = |e: &Option<Entity<TextareaState>>| e.as_ref().map(|e| e.read(cx).value().trim().to_string()).unwrap_or_default();
        Form { mirrors: area(&c.mirrors), insecure: area(&c.http), data_root: get(0), max_size: get(1), max_file: get(2), driver: c.driver.clone(), live_restore: c.live }
    }

    /// 把表单的值填进各输入框
    fn conf_fill(&mut self, f: &Form, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ins) = self.dock.conf.inputs.clone() {
            for (e, v) in ins.iter().zip([&f.data_root, &f.max_size, &f.max_file]) {
                e.update(cx, |s, cx| s.set_value(v.as_str(), window, cx));
            }
        }
        for (e, v) in [(self.dock.conf.mirrors.clone(), &f.mirrors), (self.dock.conf.http.clone(), &f.insecure)] {
            if let Some(e) = e {
                e.update(cx, |s, cx| s.set_value(v.as_str(), window, cx));
            }
        }
    }

    fn conf_raw_value(&self, cx: &App) -> String {
        self.dock.conf.raw.as_ref().map(|r| r.read(cx).value().to_string()).unwrap_or_default()
    }

    /// 现在对应的文件内容(表单或原始编辑器);有错就是错误原因
    fn conf_text(&self, cx: &App) -> Result<String, String> {
        if self.dock.conf.raw_mode {
            let t = self.conf_raw_value(cx);
            daemon_conf::parse(&t)?;
            return Ok(format!("{}\n", t.trim()));
        }
        let base = self.dock.conf.base.clone().unwrap_or_else(|| serde_json::json!({}));
        daemon_conf::render(&base, &self.conf_form(cx))
    }

    fn conf_save(&mut self, restart: bool, cx: &mut Context<Self>) {
        match self.conf_text(cx) {
            Err(e) => self.dock.notice = Some((e.into(), Instant::now())),
            Ok(text) if restart => {
                let name = self.node().name.clone();
                self.ask(
                    DockAsk {
                        title: "重启 Docker",
                        tile: IconName::Container,
                        name,
                        detail: "daemon.json",
                        body: "保存后会重启 Docker 让配置生效:没开 live-restore 时,正在运行的容器会被停止再拉起",
                        label: "保存并重启",
                        action: Action::SaveRestartDaemon(text),
                        list: Vec::new(),
                    },
                    cx,
                );
                return;
            }
            Ok(text) => self.dock_act(Action::SaveDaemon(text), cx),
        }
        cx.notify();
    }

    pub(crate) fn config_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(e) = self.dock.conf.err.clone() {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .child(empty_state(IconName::TriangleAlert, "读取不了 daemon.json", "处理后点右上角「刷新」重试"))
                .child(div().pb(px(60.0)).flex().justify_center().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child(SharedString::from(e)))
                .into_any_element();
        }
        if !self.dock.conf.loaded {
            return crate::ui::loading("正在读取 daemon.json…").into_any_element();
        }
        let ins = self.dock.conf.inputs.clone().unwrap_or_default();
        let form = self.conf_form(cx);
        let raw_mode = self.dock.conf.raw_mode;
        let dirty = if raw_mode { self.conf_raw_value(cx).trim() != self.dock.conf.orig_text.trim() } else { form != self.dock.conf.orig };
        let saving = self.is_busy("daemon.json");

        let mirrors = section(IconName::Globe, "镜像加速", "拉取镜像时优先走这些地址")
            .child(lines_field("加速地址", "https:// 开头,一行一个", self.dock.conf.mirrors.as_ref()));
        let mut drivers = div().flex().flex_row().flex_wrap().gap(px(4.0));
        for (i, (label, val)) in DRIVERS.iter().enumerate() {
            let active = self.dock.conf.driver == *val;
            drivers = drivers.child(
                div()
                    .id(("conf-drv", i))
                    .h(px(26.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(t::TYPE_CAPTION))
                    .when(active, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::SEMIBOLD))
                    .when(!active, |d| d.rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).text_color(c::muted()).hover(|s| s.bg(c::field_hover()).text_color(c::secondary())))
                    .child(*label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.dock.conf.driver = val.to_string();
                        cx.notify();
                    })),
            );
        }
        let logs = section(IconName::ScrollText, "容器日志", "不限制大小的话,日志会一直涨满磁盘")
            .child(labeled("日志驱动", "留空是 Docker 默认(json-file)", drivers))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.0))
                    .child(div().flex_1().min_w_0().child(field("单个文件上限", "如 10m、100k、1g", &ins[1])))
                    .child(div().flex_1().min_w_0().child(field("保留文件个数", "超过后轮转删除最旧的", &ins[2]))),
            );
        let storage = section(IconName::HardDrive, "存储", "改了数据目录,需要先把旧目录的数据迁过去").child(field("数据目录", "留空是 /var/lib/docker", &ins[0]));
        let live = self.dock.conf.live;
        let switch_live = switch("conf-live", live).on_click(cx.listener(|this, _, _, cx| {
            this.dock.conf.live = !this.dock.conf.live;
            cx.notify();
        }));
        let safety = section(IconName::ShieldCheck, "仓库与可靠性", "")
            .child(lines_field("HTTP 仓库", "走 HTTP 或自签证书的仓库,一行一个,写成 主机:端口", self.dock.conf.http.as_ref()))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(div().text_size(px(t::TYPE_CAPTION)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child("live-restore"))
                            .child(div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child("重启 Docker 时容器保持运行")),
                    )
                    .child(switch_live),
            );

        let forms = div().w_full().flex().flex_col().gap(px(12.0)).child(mirrors).child(logs).child(storage).child(safety);
        let mode_row = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child("编辑原始 JSON"))
                    .child(div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child("打开后直接编辑整个 daemon.json,表单里没有的选项也能改")),
            )
            .child(switch("conf-raw", raw_mode).on_click(cx.listener(|this, _, window, cx| this.conf_toggle_raw(window, cx))));
        let body = if raw_mode {
            let editor = self.dock.conf.raw.clone();
            div()
                .flex_1()
                .min_h_0()
                .rounded(px(t::RADIUS_XL))
                .overflow_hidden()
                .font_family(MONO)
                .text_size(px(t::TYPE_CONTROL))
                .children(editor.map(|e| Editor::new(&e).h_full()))
                .into_any_element()
        } else {
            forms.into_any_element()
        };

        let info = if saving {
            "正在写入…".to_string()
        } else if !self.dock.conf.exists {
            format!("{} 还不存在,保存时会创建", daemon_conf::PATH)
        } else if dirty {
            "有未保存的改动".to_string()
        } else {
            format!("{} · 保存前会备份为 daemon.json.bak", daemon_conf::PATH)
        };
        let bar = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CAPTION)).text_color(if dirty { c::warn() } else { c::ghost() }).child(SharedString::from(info)))
            .child(button("conf-reset", IconName::RotateCw, "还原").when(!dirty, |d| d.opacity(0.45)).on_click(cx.listener(|this, _, window, cx| {
                this.conf_revert(window, cx);
            })))
            .child(button("conf-save", IconName::Check, "保存").when(!dirty || saving, |d| d.opacity(0.45)).on_click(cx.listener(|this, _, _, cx| {
                if this.dock.conf.loaded && !this.is_busy("daemon.json") {
                    this.conf_save(false, cx);
                }
            })))
            .child(button("conf-restart", IconName::Container, "保存并重启 Docker").when(saving, |d| d.opacity(0.45)).on_click(cx.listener(|this, _, _, cx| {
                if this.dock.conf.loaded && !this.is_busy("daemon.json") {
                    this.conf_save(true, cx);
                }
            })));

        if !raw_mode {
            // 表单模式:开关行和表单是页面内容,跟页头一起滚;保存栏钉在下面
            self.dock.conf.bar = Some(div().pt(px(10.0)).pb(px(14.0)).flex_shrink_0().child(bar).into_any_element());
            return div().flex().flex_col().gap(px(10.0)).child(mode_row).child(body).into_any_element();
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .pb(px(14.0))
            .child(mode_row)
            .child(body)
            .child(bar)
            .into_any_element()
    }

    /// 切换表单 / 原始 JSON:进原始模式时把表单现在的内容放进编辑器;回表单时把编辑器的内容解析回表单(JSON 不合法就留在原始模式)
    fn conf_toggle_raw(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dock.conf.loaded {
            return;
        }
        if !self.dock.conf.raw_mode {
            let t = self.conf_text(cx).unwrap_or_else(|_| self.dock.conf.orig_text.clone());
            if let Some(r) = self.dock.conf.raw.clone() {
                r.update(cx, |s, cx| s.set_value(t.as_str(), window, cx));
            }
            self.dock.conf.raw_mode = true;
        } else {
            match daemon_conf::parse(&self.conf_raw_value(cx)) {
                Ok((base, f)) => {
                    self.conf_fill(&f, window, cx);
                    self.dock.conf.driver = f.driver;
                    self.dock.conf.live = f.live_restore;
                    self.dock.conf.base = Some(base);
                    self.dock.conf.raw_mode = false;
                }
                Err(e) => self.dock.notice = Some((e.into(), Instant::now())),
            }
        }
        cx.notify();
    }

    /// 丢掉未保存的改动,回到读到的内容
    fn conf_revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let f = self.dock.conf.orig.clone();
        if let Some(r) = self.dock.conf.raw.clone() {
            let t = self.dock.conf.orig_text.clone();
            r.update(cx, |s, cx| s.set_value(t.as_str(), window, cx));
        }
        self.conf_fill(&f, window, cx);
        self.dock.conf.driver = f.driver;
        self.dock.conf.live = f.live_restore;
        cx.notify();
    }
}

fn section(ic: IconName, title: &'static str, sub: &'static str) -> Div {
    div()
        .rounded(px(t::RADIUS_XL))
        .border_1()
        .border_color(c::dialog_border())
        .p(px(14.0))
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(icon(ic, 15.0, c::accent()))
                .child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(title))
                .when(!sub.is_empty(), |d| d.child(div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(sub))),
        )
}

pub(super) fn labeled(label: &'static str, hint: &'static str, body: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(5.0))
        .child(div().text_size(px(t::TYPE_CAPTION)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child(label))
        .child(body)
        .child(div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(hint))
}

pub(super) fn field(label: &'static str, hint: &'static str, input: &Entity<InputState>) -> Div {
    labeled(
        label,
        hint,
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
            .child(div().flex_1().min_w_0().child(Input::new(input).appearance(false).with_size(px(12.0 / 0.875)))),
    )
}

/// 开关:开着是主色,滑块靠右
fn switch(id: &'static str, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(36.0))
        .h(px(20.0))
        .flex_none()
        .px(px(2.0))
        .flex()
        .items_center()
        .when(on, |d| d.justify_end().bg(c::accent()))
        .when(!on, |d| d.justify_start().bg(c::field_border()))
        .rounded_full()
        .cursor_pointer()
        .child(div().size(px(16.0)).rounded_full().bg(c::on_accent()))
}

/// 多行输入,一行一个
fn lines_field(label: &'static str, hint: &'static str, input: Option<&Entity<TextareaState>>) -> Div {
    labeled(
        label,
        hint,
        div()
            .h(px(84.0))
            .rounded(px(t::RADIUS_MD))
            .bg(c::field())
            .border_1()
            .border_color(c::field_border())
            .text_size(px(t::TYPE_CONTROL))
            .children(input.map(|e| Textarea::new(e).appearance(false).with_size(px(12.0 / 0.875)).h_full())),
    )
}
