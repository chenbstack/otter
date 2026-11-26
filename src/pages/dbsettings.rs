//! 数据库页的「设置」页签:运行控制、常用参数、数据与存储。参数的定义和语句见 `dbparams`,
//! 都经容器里的客户端执行,密码不经过 Otter;不改配置文件、不自动重启。

use super::super::*;
use crate::dbinst::{DbInstance, DbKind};
use crate::dbparams as dp;
use crate::docker::Action;
use std::time::Instant;
use gpui_kit::component::input::{Editor, EditorState, Input, InputState};
use gpui_kit::component::Sizable as _;

/// 配置修改抽屉的状态
pub(crate) struct ConfDlg {
    pub(super) file: usize,
    pub(super) ed: Entity<EditorState>,
    pub(super) loading: bool,
    pub(super) busy: bool,
    pub(super) err: Option<String>,
    pub(super) saved: Option<&'static str>,
    /// 文件在挂载进来的目录里(容器重建不丢)
    pub(super) mounted: bool,
    /// 读回来的内容,等 `dbm_prepare`(有窗口)填进编辑器
    pub(super) fill: Option<String>,
    pub(super) kind: DbKind,
    pub(super) cont: String,
    /// 「挂载到主机」:等确认、执行中、成功后要重新读文件
    pub(super) mount_ask: bool,
    pub(super) mounting: bool,
    pub(super) reopen: bool,
}

fn card(title: &'static str, sub: impl Into<SharedString>, right: Option<AnyElement>, body: impl IntoElement) -> Div {
    div()
        .flex_shrink_0()
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
                .child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(title))
                .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(sub.into()))
                .children(right),
        )
        .child(body)
}

fn kv(k: &'static str, v: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_row()
        .gap(px(10.0))
        .child(div().w(px(72.0)).flex_shrink_0().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(k))
        .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::secondary()).child(v.into()))
}

impl Home {
    /// 渲染前准备输入框(需要窗口):参数读回后建输入框并填入当前值
    pub(crate) fn dbm_prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.dbm.conf.as_ref().is_some_and(|d| d.reopen) {
            if let Some(d) = self.dock.dbm.conf.as_mut() {
                d.reopen = false;
            }
            let d = self.dock.dbm.conf.as_ref().map(|d| (d.kind, d.cont.clone(), d.file));
            if let Some((k, c0, f)) = d {
                self.dbm_conf_open(k, c0, f, window, cx);
                if let Some(d) = self.dock.dbm.conf.as_mut() {
                    d.saved = Some("已挂载到主机:配置在堆栈目录的 conf/ 下,重建容器不会丢");
                }
            }
        }
        if let Some(d) = self.dock.dbm.conf.as_mut() {
            if let Some(text) = d.fill.take() {
                d.ed.update(cx, |s, cx| s.set_value(text.as_str(), window, cx));
                cx.notify();
            }
        }
        let m = &self.dock.dbm;
        if self.page != Page::Database || m.sub != 2 || !m.pfill {
            return;
        }
        let Some(Ok(cur)) = m.params.clone() else { return };
        let Some(inst) = self.dbm_inst() else { return };
        let ps = dp::params(inst.kind);
        if self.dock.dbm.pins.len() != ps.len() {
            self.dock.dbm.pins = ps.iter().map(|p| cx.new(|cx| InputState::new(window, cx).placeholder(p.default))).collect();
        }
        for (pin, c0) in self.dock.dbm.pins.iter().zip(cur.iter()) {
            let v = c0.as_ref().map(|x| x.value.clone()).unwrap_or_default();
            pin.update(cx, |s, cx| s.set_value(&v, window, cx));
        }
        self.dock.dbm.pfill = false;
        cx.notify();
    }

    pub(super) fn dbm_settings(&mut self, inst: &DbInstance, cx: &mut Context<Self>) -> AnyElement {
        let cell = |d: Div| d.flex_1().min_w(px(crate::ui::SET_CARD_MIN_W));
        let wrap = || div().flex().flex_row().flex_wrap().gap(px(14.0));
        let mut col = div().flex().flex_col().gap(px(14.0));
        col = col.child(wrap().child(cell(self.dbm_run_card(inst, cx))).child(cell(self.dbm_data_card(inst, cx))));
        col = col.child(self.dbm_params_card(inst, cx));
        div().id("dbm-settings").flex_1().min_h_0().overflow_y_scroll().child(col).into_any_element()
    }

    fn dbm_run_card(&mut self, inst: &DbInstance, cx: &mut Context<Self>) -> Div {
        let ct = &inst.container;
        let name = ct.name.clone();
        let busy = self.is_busy(&name);
        let ro = ct.panel.is_some();
        let mut acts = div().flex().flex_row().items_center().gap(px(8.0));
        if !ro {
            let n = name.clone();
            if ct.running() {
                let (kind, cont) = (inst.kind, name.clone());
                acts = acts.child(button("dbm-conf-open", IconName::FileText, "配置修改").on_click(cx.listener(move |this, _, window, cx| this.dbm_conf_open(kind, cont.clone(), 0, window, cx))));
                acts = acts.child(button_busy("dbm-restart", IconName::RotateCw, "重启", busy).on_click(cx.listener({
                    let n = n.clone();
                    move |this, _, _, cx| this.dock_act(Action::Restart(n.clone()), cx)
                })));
                acts = acts.child(button("dbm-stop", IconName::Square, "停止").on_click(cx.listener(move |this, _, _, cx| this.dock_act(Action::Stop(n.clone()), cx))));
            } else {
                acts = acts.child(button_busy("dbm-start", IconName::Play, "启动", busy).on_click(cx.listener(move |this, _, _, cx| this.dock_act(Action::Start(n.clone()), cx))));
            }
        }
        let body = div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(kv("状态", format!("{}  {}", if ct.running() { "运行中" } else { "已停止" }, ct.status)))
            .child(kv("镜像", ct.image.clone()))
            .child(kv("容器名", ct.name.clone()))
            .children(ct.project.clone().map(|p| kv("Compose", p)))
            .children(ro.then(|| div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child("面板管理的实例只读")));
        card("运行", "", (!ro).then(|| acts.into_any_element()), body)
    }

    fn dbm_data_card(&mut self, inst: &DbInstance, _cx: &mut Context<Self>) -> Div {
        let ct = &inst.container;
        let list: Vec<Div> = ct.mounts.iter().map(|m| div().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::secondary()).child(m.clone())).collect();
        let body = if list.is_empty() {
            div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("没有挂载").into_any_element()
        } else {
            div().flex().flex_col().gap(px(6.0)).children(list).into_any_element()
        };
        card("数据与存储", "容器里的数据放在这些位置", None, body)
    }

    fn dbm_params_card(&mut self, inst: &DbInstance, cx: &mut Context<Self>) -> Div {
        let kind = inst.kind;
        if !dp::supported(kind) {
            return card("常用参数", "", None, div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("MariaDB 没有运行期持久化参数的语法,暂不提供参数表;请改配置文件。"));
        }
        let m = &self.dock.dbm;
        let sub = "改完点保存;「需重启」的参数要重启容器才生效。留空 = 恢复默认";
        let cur = match &m.params {
            None => return card("常用参数", sub, None, super::dockerpages::loading("正在读取…")),
            Some(Err(e)) => {
                let retry = button("dbm-p-retry", IconName::RefreshCw, "重试").on_click(cx.listener(|this, _, _, cx| {
                    this.dock.dbm.params = None;
                    cx.notify();
                }));
                return card("常用参数", sub, None, div().flex().flex_col().gap(px(8.0)).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child(e.clone())).child(retry));
            }
            Some(Ok(v)) => v.clone(),
        };
        let ps = dp::params(kind);
        let pins = m.pins.clone();
        let (busy, msg) = (m.pbusy, m.pmsg.clone());
        // 逐项比较:输入框的内容和当前值不同才算改动
        let mut changes: Vec<dp::Change> = Vec::new();
        let mut errs: Vec<String> = Vec::new();
        let mut items: Vec<Div> = Vec::new();
        for (i, p) in ps.iter().enumerate() {
            let Some(pin) = pins.get(i) else { break };
            let now = cur.get(i).cloned().flatten();
            let text = pin.read(cx).value().trim().to_string();
            let same = now.as_ref().is_some_and(|x| x.value.eq_ignore_ascii_case(&text)) || (now.is_none() && text.is_empty());
            let mut bad = false;
            if !same {
                match dp::normalize(kind, p, &text) {
                    // 规范化后与当前值一样(如 1024M 和 1G)不算改动
                    Ok(v) if v.as_deref().zip(now.as_ref()).is_some_and(|(a, b)| a.eq_ignore_ascii_case(&b.value)) => {}
                    Ok(v) => changes.push((i, v)),
                    Err(e) => {
                        bad = true;
                        errs.push(e);
                    }
                }
            }
            let changed = !same && !bad;
            let pending = now.as_ref().is_some_and(|x| x.pending);
            let mut head = div().flex().flex_row().items_center().gap(px(8.0)).child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::MEDIUM).text_color(c::text()).child(p.label));
            if p.restart {
                head = head.child(pill("需重启", c::warn(), c::warn_soft()));
            }
            if pending {
                head = head.child(pill("待重启生效", c::warn(), c::warn_soft()));
            }
            let row = div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .p(px(12.0))
                .rounded(px(t::RADIUS_MD))
                .border_1()
                .border_color(c::dialog_border())
                .child(head)
                .child(div().text_size(px(t::TYPE_MICRO)).text_color(c::muted()).child(p.desc))
                .child(div().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::ghost()).child(format!("{}  ·  默认 {}", p.name, p.default)))
                .child(
                    div()
                        .h(px(30.0))
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .rounded(px(t::RADIUS_MD))
                        .bg(c::field())
                        .border_1()
                        .border_color(if bad { c::danger() } else if changed { c::accent() } else { c::field_border() })
                        .text_size(px(t::TYPE_CONTROL))
                        .child(div().flex_1().min_w_0().child(Input::new(pin).appearance(false).with_size(px(12.0 / 0.875)))),
                );
            items.push(row);
        }
        // 两列各占一半,窄了整列换行
        let (mut left, mut right) = (div().flex_1().min_w(px(crate::ui::SET_CARD_MIN_W)).flex().flex_col().gap(px(10.0)), div().flex_1().min_w(px(crate::ui::SET_CARD_MIN_W)).flex().flex_col().gap(px(10.0)));
        for (i, it) in items.into_iter().enumerate() {
            if i % 2 == 0 { left = left.child(it) } else { right = right.child(it) }
        }
        let rows = div().flex().flex_row().flex_wrap().gap(px(10.0)).child(left).child(right);
        let n = changes.len();
        let cont = inst.container.name.clone();
        let mut save = button_busy("dbm-p-save", IconName::Save, "保存", busy);
        if n == 0 || busy || !errs.is_empty() {
            save = save.opacity(0.5);
        } else {
            save = save.on_click(cx.listener(move |this, _, _, cx| this.dbm_params_apply(&cont, kind, changes.clone(), cx)));
        }
        let note: Option<AnyElement> = if let Some(e) = errs.first() {
            Some(div().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child(e.clone()).into_any_element())
        } else {
            msg.map(|r| match r {
                Ok(s) => div().text_size(px(t::TYPE_CAPTION)).text_color(c::online()).child(s).into_any_element(),
                Err(e) => div().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child(e).into_any_element(),
            })
        };
        let foot = div().flex().flex_row().items_center().gap(px(10.0)).child(div().flex_1().min_w_0().children(note)).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(if n == 0 { "没有改动".to_string() } else { format!("{n} 项待保存") })).child(save);
        card("常用参数", sub, None, div().flex().flex_col().gap(px(12.0)).child(rows).child(foot))
    }

    fn dbm_params_apply(&mut self, cont: &str, kind: DbKind, changes: Vec<dp::Change>, cx: &mut Context<Self>) {
        let sql = match dp::apply_sql(kind, &changes) {
            Ok(s) => s,
            Err(e) => {
                self.dock.dbm.pmsg = Some(Err(e));
                cx.notify();
                return;
            }
        };
        if self.demo {
            self.dock.notice = Some(("演示数据,不会真的执行".into(), Instant::now()));
            cx.notify();
            return;
        }
        let tag = format!("dbm:papply:{cont}");
        if let Some(d) = self.dbm_docker() {
            d.db_run(&tag, kind.exec_args(cont), Some(sql));
            let m = &mut self.dock.dbm;
            m.inflight.push(tag);
            m.pbusy = true;
            m.pmsg = None;
        }
        cx.notify();
    }
}

impl Home {
    /// 打开(或切换文件)并读取服务器上的内容
    fn dbm_conf_open(&mut self, kind: DbKind, cont: String, file: usize, window: &mut Window, cx: &mut Context<Self>) {
        let ed = match self.dock.dbm.conf.take() {
            Some(d) => d.ed,
            None => {
                let ed = cx.new(|cx| EditorState::new(window, cx).language("bash").line_number(true));
                cx.observe(&ed, |_, _, cx| cx.notify()).detach();
                ed
            }
        };
        ed.update(cx, |s, cx| s.set_value("", window, cx));
        let mut d = ConfDlg { file, ed, loading: true, busy: false, err: None, saved: None, mounted: true, fill: None, kind, cont: cont.clone(), mount_ask: false, mounting: false, reopen: false };
        if self.demo {
            d.loading = false;
            d.fill = Some("# 演示数据\n".into());
        } else {
            match crate::dbconf::read_script(kind, &cont, file) {
                Ok(script) => {
                    let tag = format!("dbm:cread:{cont}");
                    if let Some(dk) = self.dbm_docker() {
                        dk.host_run(&tag, script, 30);
                        self.dock.dbm.inflight.push(tag);
                    }
                }
                Err(e) => {
                    d.loading = false;
                    d.err = Some(e);
                }
            }
        }
        self.dock.dbm.conf = Some(d);
        cx.notify();
    }

    fn dbm_conf_mount(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.dock.dbm.conf.as_mut() else { return };
        let cont = d.cont.clone();
        d.mount_ask = false;
        match crate::dbconf::mount_script(&cont) {
            Ok(script) if !self.demo => {
                let tag = format!("dbm:cmount:{cont}");
                if let Some(dk) = self.dbm_docker() {
                    dk.host_run(&tag, script, 180);
                    self.dock.dbm.inflight.push(tag);
                    if let Some(d) = self.dock.dbm.conf.as_mut() {
                        d.mounting = true;
                        d.err = None;
                        d.saved = None;
                    }
                }
            }
            Ok(_) => self.dock.notice = Some(("演示数据,不会真的执行".into(), Instant::now())),
            Err(e) => d.err = Some(e),
        }
        cx.notify();
    }

    fn dbm_conf_save(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.dock.dbm.conf.as_ref() else { return };
        if d.busy || d.loading {
            return;
        }
        let text = d.ed.read(cx).value().to_string();
        let text = if text.is_empty() || text.ends_with('\n') { text } else { text + "\n" };
        let (kind, cont, file) = (d.kind, d.cont.clone(), d.file);
        match crate::dbconf::write_script(kind, &cont, file, &text) {
            Ok(script) => {
                if self.demo {
                    self.dock.notice = Some(("演示数据,不会真的执行".into(), Instant::now()));
                } else if let Some(dk) = self.dbm_docker() {
                    let tag = format!("dbm:cwrite:{cont}");
                    dk.host_run(&tag, script, 120);
                    self.dock.dbm.inflight.push(tag);
                    if let Some(d) = self.dock.dbm.conf.as_mut() {
                        d.busy = true;
                        d.err = None;
                        d.saved = None;
                    }
                }
            }
            Err(e) => {
                if let Some(d) = self.dock.dbm.conf.as_mut() {
                    d.err = Some(e);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn dbm_conf_drawer(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let d = self.dock.dbm.conf.as_ref()?;
        let files = crate::dbconf::files(d.kind);
        let (file, kind, cont, busy, loading) = (d.file, d.kind, d.cont.clone(), d.busy, d.loading);
        let mut head = div()
            .h(px(52.0))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(16.0))
            .border_b_1()
            .border_color(c::border())
            .child(icon(IconName::FileText, 16.0, c::secondary()))
            .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(format!("配置修改 · {}", files[file.min(files.len() - 1)].name)));
        if files.len() > 1 {
            for (i, f) in files.iter().enumerate() {
                let (name, cont) = (f.name, cont.clone());
                head = head.child(
                    div()
                        .id(("dbm-conf-file", i))
                        .h(px(24.0))
                        .px(px(9.0))
                        .flex()
                        .items_center()
                        .rounded(px(t::RADIUS_MD))
                        .cursor_pointer()
                        .text_size(px(t::TYPE_CAPTION))
                        .when(i == file, |d| d.bg(c::field()).text_color(c::text()))
                        .when(i != file, |d| d.text_color(c::muted()).hover(|s| s.bg(c::field_hover())))
                        .child(name)
                        .on_click(cx.listener(move |this, _, window, cx| this.dbm_conf_open(kind, cont.clone(), i, window, cx))),
                );
            }
        }
        head = head
            .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(files[file.min(files.len() - 1)].hint))
            .child(super::syspages::tip(super::dockerpages::icon_btn(("dbm-conf-x", 0), IconName::X, c::muted()).on_click(cx.listener(|this, _, _, cx| {
                this.dock.dbm.conf = None;
                cx.notify();
            })), "关闭"));
        let mut body = div().flex_1().min_h_0().flex().flex_col();
        if !d.mounted {
            let can_mount = matches!(d.kind, DbKind::Mysql | DbKind::Mariadb);
            let (ask, mounting) = (d.mount_ask, d.mounting);
            let msg = if mounting {
                "正在修改 compose 并重建容器…"
            } else if ask {
                "会在 compose 里加一行 ./conf:/etc/mysql/conf.d,把现有配置拷到堆栈目录的 conf/ 下,再重建容器:数据不丢,会重启一次。"
            } else {
                "这个文件在容器自己的文件系统里,容器重新创建(升级镜像、重建)后会丢失。"
            };
            let mut row = div().flex_shrink_0().m(px(12.0)).p(px(10.0)).flex().flex_row().items_center().gap(px(8.0)).rounded(px(t::RADIUS_MD)).bg(c::warn_soft()).child(icon(IconName::TriangleAlert, 14.0, c::warn())).child(div().flex_1().min_w_0().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(msg));
            if can_mount && !mounting {
                row = row.child(
                    button(if ask { "dbm-mount-go" } else { "dbm-mount-ask" }, IconName::FolderOpen, if ask { "确认挂载" } else { "把配置挂载到主机" }).on_click(cx.listener(move |this, _, _, cx| {
                        if ask { this.dbm_conf_mount(cx) } else if let Some(d) = this.dock.dbm.conf.as_mut() { d.mount_ask = true; cx.notify() }
                    })),
                );
            }
            body = body.child(row);
        }
        body = body.child(div().flex_1().min_h_0().overflow_hidden().child(if loading { super::dockerpages::loading("正在读取…").into_any_element() } else { Editor::new(&d.ed).appearance(false).h_full().into_any_element() }));
        let note: Option<(String, Rgba)> = d.err.clone().map(|e| (e, c::danger())).or(d.saved.map(|s| (s.to_string(), c::online())));
        let (text, color) = note.unwrap_or_default();
        let can = !busy && !loading && d.err.as_deref().map_or(true, |_| true);
        let ok = div()
            .id("dbm-conf-ok")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(34.0))
            .px(px(16.0))
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .when(can, |d| d.bg(c::accent()).text_color(c::on_accent()).cursor_pointer().hover(|s| s.opacity(0.88)))
            .when(!can, |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()))
            .when(busy, |d| d.child(crate::ui::dot_spinner(("dbm-conf-spin", 0), c::muted())))
            .child(if busy { "保存中…" } else { "保存" })
            .on_click(cx.listener(|this, _, _, cx| this.dbm_conf_save(cx)));
        let cancel = crate::node_form::ctl_btn("dbm-conf-cancel", None, "关闭", 34.0).on_click(cx.listener(|this, _, _, cx| {
            this.dock.dbm.conf = None;
            cx.notify();
        }));
        let foot = div().px(px(24.0)).py(px(14.0)).border_t_1().border_color(c::line()).flex().flex_row().items_center().gap(px(10.0)).child(div().flex_1().min_w_0().text_size(px(t::TYPE_CAPTION)).text_color(color).child(SharedString::from(text))).child(cancel).child(ok);
        Some(crate::ui::drawer("dbm-conf", 860.0, |_, _, _| {}, [head.into_any_element(), body.into_any_element(), foot.into_any_element()]).into_any_element())
    }
}
