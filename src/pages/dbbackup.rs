//! 数据库备份与恢复弹窗(数据库页里某个库的「备份」按钮)。
//! 备份文件在服务器 `~/otter-backups/<容器名>/`,脚本构造和解析见 `dbinst`;长任务在服务器上脱离运行,这里只轮询列表。

use super::super::*;
use super::dockerpages::{icon_btn_c, loading};
use crate::dbinst::{self, Backups};
use std::time::{Duration, Instant};

/// 正在忙时多久刷新一次列表
const POLL: Duration = Duration::from_secs(2);

pub(super) struct BackupDlg {
    pub db: String,
    pub data: Option<Result<Backups, String>>,
    pub last: Instant,
    /// 已发出「立即备份/恢复」,等脚本启动
    pub starting: bool,
    /// 启动命令已返回、等列表里出现这次备份时的(进行中+已有备份)数;用来让占位行不消失
    pub launched: Option<usize>,
    pub err: Option<String>,
    pub restore_ask: Option<String>,
    /// 恢复前是否先备份当前数据;由用户在确认框里勾选,每次打开确认框都从不勾选开始
    pub rs_pre: bool,
    pub del_ask: Option<String>,
    /// 这次打开弹窗后发起过恢复,等它出结果
    pub restoring: bool,
    /// 刚发起任务:到这个时刻之前一直刷新列表,等脚本把 `.part` 建出来
    pub watch: Instant,
    /// 要立刻刷新一次列表
    pub want: bool,
    /// 正在删除的备份文件名(删除按钮上转圈)
    pub deleting: Option<String>,
    /// 正在恢复的备份文件名(恢复按钮上转圈)
    pub rs_file: Option<String>,
    /// 正在下载的 (文件名, 下载 tag),进度从执行层取
    pub dl: Option<(String, String)>,
}

impl BackupDlg {
    pub fn new(db: String) -> Self {
        Self { db, data: None, last: Instant::now(), starting: false, launched: None, err: None, restore_ask: None, rs_pre: false, del_ask: None, restoring: false, watch: Instant::now(), want: false, deleting: None, rs_file: None, dl: None }
    }

    /// 备份/恢复确实在进行(不含刚发起后的观察窗口):此时不能再发起新的
    fn active(&self) -> bool {
        self.starting || matches!(&self.data, Some(Ok(b)) if !b.running.is_empty() || b.restore.as_deref() == Some("running"))
    }

    fn busy(&self) -> bool {
        self.starting || Instant::now() < self.watch || matches!(&self.data, Some(Ok(b)) if !b.running.is_empty() || b.restore.as_deref() == Some("running"))
    }

    /// 该刷新列表了(有任务在跑,隔 `POLL` 一次)
    pub fn poll_due(&self) -> bool {
        self.want || (self.busy() && self.last.elapsed() >= POLL)
    }
}

/// 从备份文件名里取时间:`<库>_20261125-221835[-pre].dump[.part]` → `2026-11-25 22:18`
fn name_time(name: &str) -> String {
    let ts = name.split('.').next().and_then(|st| st.rsplit_once('_')).map(|(_, t)| t).unwrap_or("");
    match (ts.get(0..4), ts.get(4..6), ts.get(6..8), ts.get(9..11), ts.get(11..13)) {
        (Some(y), Some(m), Some(d), Some(h), Some(mi)) => format!("{y}-{m}-{d} {h}:{mi}"),
        _ => name.to_string(),
    }
}

fn fmt_time(t: u64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(t as i64, 0).single().map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

impl Home {
    pub(super) fn dbm_bk_open(&mut self, db: String, cx: &mut Context<Self>) {
        let mut d = BackupDlg::new(db);
        if self.demo {
            d.data = Some(Ok(Backups {
                home: "/home/demo".into(),
                files: vec![
                    dbinst::BackupFile { name: "app_20261125-101500.dump".into(), db: "app".into(), size: 48_300_000, mtime: 1_795_600_000, pre: false },
                    dbinst::BackupFile { name: "app_20261124-030000-pre.dump".into(), db: "app".into(), size: 47_100_000, mtime: 1_795_500_000, pre: true },
                ],
                ..Default::default()
            }));
        }
        self.dock.dbm.bk = Some(d);
        cx.notify();
    }

    fn dbm_bk_close(&mut self, cx: &mut Context<Self>) {
        self.dock.dbm.bk = None;
        cx.notify();
    }

    /// 该发列表请求就发(刚打开、或有任务在跑到点了)
    pub(super) fn dbm_bk_request(&mut self) {
        if self.demo {
            return;
        }
        let Some(inst) = self.dbm_inst() else { return };
        let Some(b) = &self.dock.dbm.bk else { return };
        let tag = format!("dbm:bkls:{}", inst.container.name);
        if self.dock.dbm.inflight.contains(&tag) || !(b.data.is_none() || b.poll_due()) {
            return;
        }
        match dbinst::backup_list_script(&inst.container.name) {
            Ok(s) => {
                if let Some(d) = self.dbm_docker() {
                    d.shell_run(&tag, &s);
                    self.dock.dbm.inflight.push(tag);
                }
                if let Some(b) = self.dock.dbm.bk.as_mut() {
                    b.last = Instant::now();
                    b.want = false;
                }
            }
            Err(e) => {
                if let Some(b) = self.dock.dbm.bk.as_mut() {
                    b.data = Some(Err(e));
                }
            }
        }
    }

    /// 备份/恢复/删除/下载/列表的结果回来了(`tag` 形如 `dbm:bkls:<容器>`)
    pub(super) fn dbm_bk_result(&mut self, tag: &str, r: Result<String, String>) {
        let Some(b) = self.dock.dbm.bk.as_mut() else { return };
        let mut refresh = false;
        match tag.split(':').nth(1) {
            Some("bkls") => {
                b.data = Some(r.map(|o| dbinst::parse_backups(&o, &b.db)));
                if let (Some(before), Some(Ok(d))) = (b.launched, &b.data) {
                    // 列表里出现了这次备份(进行中或已完成/失败),或等太久了,占位行才撤掉
                    if d.running.len() + d.files.len() + d.failed.len() > before || Instant::now() >= b.watch {
                        b.launched = None;
                        b.starting = false;
                    }
                }
                if let Some(Ok(d)) = &b.data {
                    match d.restore.as_deref() {
                        Some("ok") if b.restoring => {
                            b.restoring = false;
                            b.rs_file = None;
                            self.dock.notice = Some(("恢复完成".into(), Instant::now()));
                        }
                        Some(s) if b.restoring && s.starts_with("fail") => {
                            b.restoring = false;
                            b.rs_file = None;
                            b.err = Some(s.trim_start_matches("fail").trim().to_string());
                        }
                        _ => {}
                    }
                }
            }
            Some("bkrun" | "bkrs") => {
                let before = match &b.data {
                    Some(Ok(d)) => d.running.len() + d.files.len() + d.failed.len(),
                    _ => 0,
                };
                let backup = tag.starts_with("dbm:bkrun");
                b.starting = false;
                match r {
                    Ok(_) => {
                        if backup {
                            b.starting = true;
                            b.launched = Some(before);
                        }
                        refresh = true;
                        b.watch = Instant::now() + Duration::from_secs(10);
                    }
                    Err(e) => {
                        b.restoring = false;
                        b.rs_file = None;
                        b.launched = None;
                        b.err = Some(e);
                    }
                }
            }
            Some("bkdel") => {
                b.deleting = None;
                match r {
                    Ok(_) => refresh = true,
                    Err(e) => b.err = Some(e),
                }
            }
            Some("bkcancel") => match r {
                Ok(_) => {
                    self.dock.notice = Some(("已中止备份".into(), Instant::now()));
                    b.launched = None;
                    b.starting = false;
                    refresh = true;
                }
                Err(e) => b.err = Some(e),
            },
            Some("bkdl") => {
                b.dl = None;
                match r {
                    Ok(p) => self.dock.notice = Some((format!("已保存到 {p}").into(), Instant::now())),
                    Err(e) => b.err = Some(e),
                }
            }
            _ => {}
        }
        if refresh {
            // 立刻再读一次列表,让「进行中」马上出现
            b.want = true;
        }
    }

    fn dbm_bk_start(&mut self, cx: &mut Context<Self>) {
        let Some(inst) = self.dbm_inst() else { return };
        let Some(b) = self.dock.dbm.bk.as_mut() else { return };
        if b.active() {
            return;
        }
        b.err = None;
        if self.demo {
            self.dock.notice = Some(("演示数据,没有真的备份".into(), Instant::now()));
            cx.notify();
            return;
        }
        match inst.kind.backup_script(&inst.container.name, &b.db) {
            Ok(s) => {
                b.starting = true;
                let tag = format!("dbm:bkrun:{}", inst.container.name);
                if let Some(d) = self.dbm_docker() {
                    d.host_run(&tag, s, 60);
                    self.dock.dbm.inflight.push(tag);
                }
            }
            Err(e) => b.err = Some(e),
        }
        cx.notify();
    }

    /// 中止服务器上进行中的备份(`part` 是 `.part` 文件名)
    fn dbm_bk_cancel(&mut self, part: String, cx: &mut Context<Self>) {
        let Some(inst) = self.dbm_inst() else { return };
        let Some(b) = self.dock.dbm.bk.as_mut() else { return };
        b.err = None;
        if self.demo {
            self.dock.notice = Some(("演示数据,没有真的中止".into(), Instant::now()));
            cx.notify();
            return;
        }
        match dbinst::backup_cancel_script(&inst.container.name, &part) {
            Ok(s) => {
                self.dock.notice = Some(("正在中止…".into(), Instant::now()));
                let tag = format!("dbm:bkcancel:{}", inst.container.name);
                if let Some(d) = self.dbm_docker() {
                    d.host_run(&tag, s, 30);
                    self.dock.dbm.inflight.push(tag);
                }
            }
            Err(e) => b.err = Some(e),
        }
        cx.notify();
    }

    fn dbm_bk_restore(&mut self, file: String, cx: &mut Context<Self>) {
        let Some(inst) = self.dbm_inst() else { return };
        let Some(b) = self.dock.dbm.bk.as_mut() else { return };
        b.restore_ask = None;
        b.err = None;
        if self.demo {
            self.dock.notice = Some(("演示数据,没有真的恢复".into(), Instant::now()));
            cx.notify();
            return;
        }
        match inst.kind.restore_script(&inst.container.name, &b.db, &file, b.rs_pre) {
            Ok(s) => {
                b.starting = true;
                b.restoring = true;
                b.rs_file = Some(file.clone());
                let tag = format!("dbm:bkrs:{}", inst.container.name);
                if let Some(d) = self.dbm_docker() {
                    d.host_run(&tag, s, 60);
                    self.dock.dbm.inflight.push(tag);
                }
            }
            Err(e) => b.err = Some(e),
        }
        cx.notify();
    }

    fn dbm_bk_delete(&mut self, file: String, cx: &mut Context<Self>) {
        let Some(inst) = self.dbm_inst() else { return };
        let Some(b) = self.dock.dbm.bk.as_mut() else { return };
        b.del_ask = None;
        b.err = None;
        if self.demo {
            if let Some(Ok(d)) = &mut b.data {
                d.files.retain(|f| f.name != file);
            }
            cx.notify();
            return;
        }
        match dbinst::backup_delete_script(&inst.container.name, &file) {
            Ok(s) => {
                b.deleting = Some(file.clone());
                let tag = format!("dbm:bkdel:{}", inst.container.name);
                if let Some(d) = self.dbm_docker() {
                    d.shell_run(&tag, &s);
                    self.dock.dbm.inflight.push(tag);
                }
            }
            Err(e) => b.err = Some(e),
        }
        cx.notify();
    }

    /// 下载:先让用户选保存位置,再从服务器读回来
    fn dbm_bk_download(&mut self, file: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(inst) = self.dbm_inst() else { return };
        let Some(b) = self.dock.dbm.bk.as_mut() else { return };
        b.err = None;
        if self.demo {
            self.dock.notice = Some(("演示数据,没有真的下载".into(), Instant::now()));
            cx.notify();
            return;
        }
        let home = match &b.data {
            Some(Ok(d)) => d.home.clone(),
            _ => return,
        };
        let Some(remote) = dbinst::backup_remote_path(&home, &inst.container.name, &file) else {
            b.err = Some("文件名里有不支持的字符".into());
            cx.notify();
            return;
        };
        let dir = crate::platform::home_dir().join("Downloads");
        let rx = cx.prompt_for_new_path(&dir, Some(&file));
        let tag = format!("dbm:bkdl:{}", inst.container.name);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else { return };
            let _ = this.update(cx, |this, cx| {
                if let Some(d) = this.dbm_docker() {
                    d.fetch(&tag, &remote, path);
                    this.dock.dbm.inflight.push(tag.clone());
                    if let Some(b) = this.dock.dbm.bk.as_mut() {
                        b.dl = Some((file, tag));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn dbm_bk_overlays(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v = Vec::new();
        v.extend(self.dbm_bk_dialog(cx));
        v.extend(self.dbm_bk_confirms(cx));
        v
    }

    fn dbm_bk_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let b = self.dock.dbm.bk.as_ref()?;
        let inst = self.dbm_inst()?;
        let busy = b.active();
        let backing = (b.starting && !b.restoring) || matches!(&b.data, Some(Ok(d)) if !d.running.is_empty());
        let running = matches!(&b.data, Some(Ok(d)) if d.restore.as_deref() == Some("running"));
        let dir = format!("服务器 ~/otter-backups/{}/", inst.container.name);

        let start = div()
            .id("dbm-bk-start")
            .h(px(30.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(c::accent())
            .cursor_pointer()
            .hover(|s| s.bg(c::field_hover()))
            .when(busy, |d| d.opacity(if backing { 1.0 } else { 0.5 }))
            .child(if backing { crate::ui::dot_spinner(("dbm-bk-start-spin", 0), c::accent()).into_any_element() } else { icon(IconName::DatabaseBackup, 14.0, c::accent()).into_any_element() })
            .child(if backing { "备份中…" } else { "立即备份" })
            .on_click(cx.listener(|this, _, _, cx| this.dbm_bk_start(cx)));

        let head = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(Self::icon_tile(IconName::DatabaseBackup, c::accent(), c::accent().opacity(0.16)).size(px(36.0)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).child(format!("备份 · {}", b.db)))
                    .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).truncate().child(dir)),
            )
            .child(icon_btn_c(("dbm-bk-close", 0), IconName::X, c::muted()).on_click(cx.listener(|this, _, _, cx| this.dbm_bk_close(cx))));

        let banner = |spin: bool, text: String, col: Rgba| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .text_size(px(t::TYPE_CAPTION))
                .text_color(col)
                .when(spin, |d| d.child(crate::ui::dot_spinner(("dbm-bk-spin", 0), col)))
                .child(div().flex_1().min_w_0().child(text))
        };
        let mut notes = div().flex().flex_col().gap(px(6.0));
        if running {
            notes = notes.child(banner(true, "正在恢复所选备份…".into(), c::accent()));
        }
        if b.starting && !running && !backing {
            notes = notes.child(banner(true, "正在启动…".into(), c::accent()));
        }
        if let Some(e) = &b.err {
            notes = notes.child(banner(false, e.clone(), c::danger()));
        }

        let item = |id: usize, left: AnyElement, ops: Vec<AnyElement>| {
            div()
                .id(("dbm-bk-row", id))
                .min_h(px(42.0))
                .px(px(12.0))
                .py(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .rounded(px(t::RADIUS_MD))
                .bg(c::field())
                .border_1()
                .border_color(c::field_border())
                .child(div().flex_1().min_w_0().child(left))
                .child(div().flex().flex_row().items_center().gap(px(2.0)).children(ops))
        };
        let deleting = b.deleting.clone();
        let restoring_file = if b.restoring { b.rs_file.clone() } else { None };
        let dl = b.dl.as_ref().map(|(n, tag)| (n.clone(), self.dbm_docker().and_then(|d| d.fetch_progress(tag))));
        let spin = |i: usize, col: Rgba| div().size(px(24.0)).flex().items_center().justify_center().child(crate::ui::dot_spinner(("dbm-bk-spin", i), col)).into_any_element();
        let mut list = div().flex().flex_col().gap(px(6.0));
        match &b.data {
            None => list = list.child(div().py(px(14.0)).child(loading("正在读取备份…"))),
            Some(Err(e)) => list = list.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child(e.clone())),
            Some(Ok(d)) => {
                let mut i = 0usize;
                if b.starting && !b.restoring && d.running.is_empty() {
                    let left = div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.0))
                        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child("正在启动备份…"))
                        .into_any_element();
                    list = list.child(item(i, left, Vec::new()));
                    i += 1;
                }
                for (name, sz) in &d.running {
                    let part = name.clone();
                    let left = div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.0))
                        .child(div().font_family(MONO).text_size(px(t::TYPE_CONTROL)).child(name_time(name)))
                        .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(format!("已写 {}", dbinst::fmt_size(*sz))))
                        .child(div().px(px(6.0)).rounded(px(t::RADIUS_SM)).bg(c::accent().opacity(0.18)).text_size(px(t::TYPE_CAPTION)).text_color(c::accent()).child("备份中"))
                        .children(name.contains("-pre.").then(|| div().px(px(6.0)).rounded(px(t::RADIUS_SM)).bg(c::field_hover()).text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child("恢复前")))
                        .into_any_element();
                    let ops = vec![super::syspages::tip(icon_btn_c(("dbm-bk-stop", i), IconName::X, c::danger()), "中止备份").on_click(cx.listener(move |this, _, _, cx| this.dbm_bk_cancel(part.clone(), cx))).into_any_element()];
                    list = list.child(item(i, left, ops));
                    i += 1;
                }
                for (name, msg) in &d.failed {
                    let n = name.clone();
                    list = list.child(item(
                        i,
                        div().text_size(px(t::TYPE_CAPTION)).text_color(c::danger()).child(format!("备份失败:{msg}")).into_any_element(),
                        vec![if deleting.as_deref() == Some(name.as_str()) { spin(i, c::danger()) } else { super::syspages::tip(icon_btn_c(("dbm-bk-fdel", i), IconName::Trash, c::danger()), "清除记录").on_click(cx.listener(move |this, _, _, cx| this.dbm_bk_delete(n.clone(), cx))).into_any_element() }],
                    ));
                    i += 1;
                }
                for f in &d.files {
                    let (n1, n2, n3) = (f.name.clone(), f.name.clone(), f.name.clone());
                    let line = div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.0))
                        .child(div().font_family(MONO).text_size(px(t::TYPE_CONTROL)).child(fmt_time(f.mtime)))
                        .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(dbinst::fmt_size(f.size)))
                        .children(f.pre.then(|| div().px(px(6.0)).rounded(px(t::RADIUS_SM)).bg(c::field_hover()).text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child("恢复前")));
                    // 正在下载这一份:下面多一条进度条和百分比
                    let downloading = dl.as_ref().filter(|(n, _)| *n == f.name);
                    let left = div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(line)
                        .children(downloading.map(|(_, p)| {
                            let (done, total) = p.unwrap_or((0, f.size.max(1)));
                            let frac = if total == 0 { 0.0 } else { (done as f32 / total as f32).clamp(0.0, 1.0) };
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(10.0))
                                .child(div().flex_1().h(px(4.0)).rounded_full().bg(c::field_hover()).child(div().h_full().w(relative(frac)).rounded_full().bg(c::accent())))
                                .child(div().w(px(130.0)).flex_none().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(format!("{:.0}%  {} / {}", frac * 100.0, dbinst::fmt_size(done), dbinst::fmt_size(total))))
                        }))
                        .into_any_element();
                    let ops = vec![
                        if downloading.is_some() {
                            spin(i, c::accent())
                        } else {
                            super::syspages::tip(icon_btn_c(("dbm-bk-dl", i), IconName::Download, c::muted()), "下载到本地").on_click(cx.listener(move |this, _, window, cx| this.dbm_bk_download(n1.clone(), window, cx))).into_any_element()
                        },
                        if restoring_file.as_deref() == Some(f.name.as_str()) {
                            spin(i + 1000, c::accent())
                        } else {
                            super::syspages::tip(icon_btn_c(("dbm-bk-rs", i), IconName::Undo2, c::muted()), "恢复").on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(b) = this.dock.dbm.bk.as_mut() {
                                    b.restore_ask = Some(n2.clone());
                                    b.rs_pre = false;
                                }
                                cx.notify();
                            })).into_any_element()
                        },
                        if deleting.as_deref() == Some(f.name.as_str()) {
                            spin(i + 2000, c::danger())
                        } else {
                            super::syspages::tip(icon_btn_c(("dbm-bk-del", i), IconName::Trash, c::danger()), "删除").on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(b) = this.dock.dbm.bk.as_mut() {
                                    b.del_ask = Some(n3.clone());
                                }
                                cx.notify();
                            })).into_any_element()
                        },
                    ];
                    list = list.child(item(i, left, ops));
                    i += 1;
                }
                if i == 0 {
                    list = list.child(
                        div()
                            .py(px(26.0))
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(6.0))
                            .rounded(px(t::RADIUS_MD))
                            .border_1()
                            .border_dashed()
                            .border_color(c::field_border())
                            .child(icon(IconName::DatabaseBackup, 24.0, c::ghost()))
                            .child(div().text_size(px(t::TYPE_BODY)).text_color(c::muted()).child("还没有备份"))
                            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("导出在服务器上进行,关掉 Otter 也不会中断")),
                    );
                }
            }
        }

        let n = match &b.data {
            Some(Ok(d)) => d.files.len(),
            _ => 0,
        };
        let bar = div()
            .flex()
            .flex_row()
            .items_center()
            .child(div().flex_1().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(if n > 0 { format!("{n} 份备份") } else { "备份".to_string() }))
            .child(start);
        let has_notes = running || (b.starting && !backing) || b.err.is_some();
        let body = div()
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(head)
            .child(bar)
            .children(has_notes.then_some(notes))
            .child(div().id("dbm-bk-list").max_h(px(300.0)).overflow_y_scroll().child(list))
            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child("恢复会覆盖库里现有数据;恢复时可以选择先备份当前数据"));
        let esc = cx.listener(|this: &mut Self, e: &KeyDownEvent, _, cx| {
            if e.keystroke.key == "escape" {
                this.dbm_bk_close(cx);
                cx.stop_propagation();
            }
        });
        Some(crate::ui::modal("dbm-bk-dialog", 560.0, esc, [body.into_any_element()]).into_any_element())
    }

    /// 恢复、删除前的确认框
    fn dbm_bk_confirms(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut v = Vec::new();
        let Some(b) = self.dock.dbm.bk.as_ref() else { return v };
        let db = b.db.clone();
        if let Some(file) = b.restore_ask.clone() {
            let on = b.rs_pre;
            let pre_check = div()
                .id("dbm-bkrs-pre")
                .w_full()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(10.0))
                .cursor_pointer()
                .child(
                    div()
                        .mt(px(1.0))
                        .size(px(16.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .border_1()
                        .when(on, |d| d.bg(c::accent()).border_color(c::accent()))
                        .when(!on, |d| d.border_color(c::field_border()))
                        .children(on.then(|| icon(IconName::Check, 12.0, c::on_accent()))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child("恢复前先备份当前数据"))
                        .child(div().text_size(px(t::TYPE_MICRO + 1.0)).text_color(c::ghost()).child("多占一份磁盘空间;不勾选则直接覆盖,无法回退")),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(b) = this.dock.dbm.bk.as_mut() {
                        b.rs_pre = !b.rs_pre;
                    }
                    cx.notify();
                }))
                .into_any_element();
            let spec = crate::confirm::Confirm {
                ids: ["dbm-bkrs-cancel", "dbm-bkrs-ok"],
                title: "恢复数据库",
                tile: Self::icon_tile(IconName::Undo2, c::accent(), c::accent().opacity(0.18)).into_any_element(),
                name: file.clone().into(),
                detail: "".into(),
                points: vec![
                    (IconName::Database, c::warn(), "库里现有的数据会被这份备份覆盖;备份之后新增的数据会丢失".into()),
                    (IconName::TriangleAlert, c::warn(), "恢复期间用到这个库的应用可能报错".into()),
                ],
                ok_label: "覆盖并恢复",
                kind: crate::confirm::ConfirmKind::Action,
                extra: Some(pre_check),
            };
            v.push(self.delete_confirm("dbm-bkrs-dialog", spec, |this| { if let Some(b) = this.dock.dbm.bk.as_mut() { b.restore_ask = None; } }, move |this, cx| this.dbm_bk_restore(file.clone(), cx), cx).into_any_element());
        }
        if let Some(file) = b.del_ask.clone() {
            let spec = crate::confirm::Confirm {
                ids: ["dbm-bkdel-cancel", "dbm-bkdel-ok"],
                title: "删除备份",
                tile: Self::icon_tile(IconName::DatabaseBackup, c::danger(), c::danger().opacity(0.18)).into_any_element(),
                name: file.clone().into(),
                detail: format!("{db} 的备份").into(),
                points: vec![
                    (IconName::Trash, c::danger(), "服务器上的这份备份文件会被删除,无法恢复".into()),
                    (IconName::Database, c::online(), "数据库本身不受影响".into()),
                ],
                ok_label: "删除备份",
                kind: crate::confirm::ConfirmKind::Danger,
                extra: None,
            };
            v.push(self.delete_confirm("dbm-bkdel-dialog", spec, |this| { if let Some(b) = this.dock.dbm.bk.as_mut() { b.del_ask = None; } }, move |this, cx| this.dbm_bk_delete(file.clone(), cx), cx).into_any_element());
        }
        v
    }
}
