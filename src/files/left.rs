//! 双栏左侧的「来源」切换:本地 / 另一台服务器。
//! 左栏选了服务器时,用独立的 SFTP 连接浏览(功能是右栏的精简版:浏览、返回/前进/上级、刷新、拖拽);
//! 服务器之间拖拽传输走 `Sftp::relay_from`(经本机内存分块中转,不落盘)。

use super::*;
use crate::DROPDOWN_COMPACT_W;

/// 左栏的服务器来源状态
#[derive(Default)]
pub(super) struct LeftSrv {
    pub node: Option<NodeId>,
    pub sftp: Option<Sftp>,
    pub path: String,
    pub back: Vec<String>,
    pub fwd: Vec<String>,
    pub sel: Option<String>,
    pub ready: bool,
    /// 来源下拉是否展开
    pub menu: bool,
    /// 来源下拉里的搜索框(展开时才有)
    pub search: Option<Entity<InputState>>,
    pub restored: bool,
    pub polling: bool,
}

impl Home {
    fn node_rows(&self) -> Vec<(NodeId, String, String)> {
        self.groups.iter().flat_map(|g| g.nodes.iter()).map(|n| (n.id, n.name.clone(), n.user_host.clone())).collect()
    }

    /// 首次显示双栏时,恢复上次选的来源(节点已删除就保持本地)。
    pub(super) fn restore_left(&mut self, cx: &mut Context<Self>) {
        if self.files.left.restored {
            return;
        }
        self.files.left.restored = true;
        let id = self.store.as_ref().and_then(|s| s.get_setting("left_source")).and_then(|v| v.parse::<u32>().ok()).map(NodeId);
        if let Some(id) = id.filter(|id| self.node_rows().iter().any(|n| n.0 == *id)) {
            self.set_left_source(Some(id), false, cx);
        }
    }

    pub(super) fn set_left_source(&mut self, node: Option<NodeId>, save: bool, cx: &mut Context<Self>) {
        self.files.left = LeftSrv { node, restored: true, ..Default::default() };
        if let Some(id) = node {
            let conn = self.conn_for(id);
            self.files.left.sftp = Some(Sftp::connect(conn, None));
        }
        if save && let Some(st) = self.store.as_ref() {
            let _ = st.set_setting("left_source", &node.map(|n| n.0.to_string()).unwrap_or_default());
        }
        self.files.ctx = None;
        self.files.more = 0;
        self.files.addr_edit = 0;
        cx.notify();
    }

    /// 每次渲染前调用:连上后取主目录并列目录,收通知,有后台活动时轮询重绘。
    pub(super) fn sync_left(&mut self, cx: &mut Context<Self>) {
        let fs = &mut self.files;
        let l = &mut fs.left;
        let Some(s) = l.sftp.as_ref() else { return };
        if matches!(s.status(), SftpStatus::Ready) {
            if !l.ready {
                l.ready = true;
                l.path = Some(s.home()).filter(|h| !h.is_empty()).unwrap_or_else(|| "/".into());
            }
            if s.dir(&l.path).is_none() {
                s.list(&l.path);
            }
            if let Some(n) = s.take_notice() {
                fs.notice = Some(n.into());
            }
        }
        if s.busy() && !l.polling {
            l.polling = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_millis(150)).await;
                    let Ok(more) = this.update(cx, |this, cx| {
                        cx.notify();
                        let more = this.files.left.sftp.as_ref().is_some_and(|s| s.busy());
                        if !more {
                            this.files.left.polling = false;
                        }
                        more
                    }) else {
                        break;
                    };
                    if !more {
                        break;
                    }
                }
            })
            .detach();
        }
    }

    fn left_go(&mut self, to: String, cx: &mut Context<Self>) {
        let l = &mut self.files.left;
        if to != l.path {
            let old = std::mem::replace(&mut l.path, to);
            l.back.push(old);
            l.fwd.clear();
        }
        l.sel = None;
        if let Some(s) = l.sftp.as_ref() {
            s.list(&l.path);
        }
        cx.notify();
    }

    fn left_back(&mut self, cx: &mut Context<Self>) {
        let l = &mut self.files.left;
        if let Some(p) = l.back.pop() {
            let cur = std::mem::replace(&mut l.path, p);
            l.fwd.push(cur);
            l.sel = None;
            if let Some(s) = l.sftp.as_ref() {
                s.list(&l.path);
            }
        }
        cx.notify();
    }

    fn left_fwd(&mut self, cx: &mut Context<Self>) {
        let l = &mut self.files.left;
        if let Some(p) = l.fwd.pop() {
            let cur = std::mem::replace(&mut l.path, p);
            l.back.push(cur);
            l.sel = None;
            if let Some(s) = l.sftp.as_ref() {
                s.list(&l.path);
            }
        }
        cx.notify();
    }

    fn left_ready(&self) -> Option<&Sftp> {
        self.files.left.sftp.as_ref().filter(|s| matches!(s.status(), SftpStatus::Ready))
    }

    /// 本地项拖到左栏服务器:上传到 `dir`
    fn left_upload(&mut self, paths: Vec<PathBuf>, dir: String, cx: &mut Context<Self>) {
        if let Some(s) = self.left_ready() {
            let n = paths.len();
            s.upload(paths, &dir);
            self.files.notice = Some(format!("开始上传 {n} 项 → {dir}").into());
        } else {
            self.files.notice = Some("还没连上服务器".into());
        }
        cx.notify();
    }

    /// 服务器之间拖拽:`to_left` 为真是右栏 → 左栏,否则左栏 → 右栏。`dir` 是目的目录。
    pub(super) fn relay_drop(&mut self, it: &DragRemote, to_left: bool, dir: String, cx: &mut Context<Self>) {
        let fs = &self.files;
        let right = fs.sftp.as_ref().filter(|s| self.sftp_real() && matches!(s.status(), SftpStatus::Ready));
        let left = self.left_ready();
        let msg = match (left, right) {
            (Some(l), Some(r)) => {
                let (src, dst, from) = if to_left { (r, l, join(&fs.path, &it.name)) } else { (l, r, join(&fs.left.path, &it.name)) };
                dst.relay_from(src, &from, it.is_dir, &dir);
                format!("开始传输 {} → {dir}", it.name)
            }
            _ => "两台服务器都连上后才能互传".to_string(),
        };
        self.files.notice = Some(msg.into());
        cx.notify();
    }

    /// 来源下拉里按关键字过滤后的项:None = 本地,Some = 节点;关键字匹配名字或 user@host
    fn src_matches(&self, q: &str) -> Vec<(Option<NodeId>, String, IconName)> {
        let q = q.trim().to_lowercase();
        let mut v = Vec::new();
        if q.is_empty() || "本地".contains(&q) || "local".contains(&q) {
            v.push((None, "本地".to_string(), IconName::Laptop));
        }
        for (id, n, host) in self.node_rows() {
            if q.is_empty() || n.to_lowercase().contains(&q) || host.to_lowercase().contains(&q) {
                v.push((Some(id), n, IconName::Server));
            }
        }
        v
    }

    fn pick_source(&mut self, to: Option<NodeId>, cx: &mut Context<Self>) {
        if to == self.files.left.node {
            self.files.left.menu = false;
            self.files.left.search = None;
            cx.notify();
        } else {
            self.set_left_source(to, true, cx);
        }
    }

    /// 标题行上的来源按钮:当前来源 + 带搜索框的下拉(本地 / 全部节点)
    pub(super) fn source_button(&self, cx: &mut Context<Self>) -> Div {
        let l = &self.files.left;
        let nodes = self.node_rows();
        let cur = l.node.and_then(|id| nodes.iter().find(|n| n.0 == id));
        let (ic, name) = match cur {
            Some(n) => (IconName::Server, n.1.clone()),
            None => (IconName::Laptop, "本地".to_string()),
        };
        let open = l.menu;
        let query = l.search.as_ref().map(|s| s.read(cx).value().to_string()).unwrap_or_default();
        let rows = if open { self.src_matches(&query) } else { Vec::new() };
        let cur_node = l.node;
        let search = l.search.clone();
        div()
            .relative()
            .flex_shrink_0()
            .child(
                div()
                    .id("left-src")
                    .h(px(28.0))
                    .max_w(px(120.0))
                    .px(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .rounded(px(t::RADIUS_MD))
                    .cursor_pointer()
                    .bg(if open { c::hover() } else { c::sunken() })
                    .border_1()
                    .border_color(c::border())
                    .hover(|s| s.bg(c::hover()))
                    .child(icon(ic, 13.0, c::accent()))
                    .child(div().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::text()).child(name))
                    .child(icon(IconName::ChevronDown, 11.0, c::muted()))
                    .tooltip(|_, cx| cx.new(|_| Tip("切换左栏来源".into())).into())
                    .on_click(cx.listener(|this, _, window, cx| {
                        let l = &mut this.files.left;
                        l.menu = !l.menu;
                        if l.menu {
                            let st = cx.new(|cx| InputState::new(window, cx).placeholder("搜索服务器"));
                            cx.observe(&st, |_, _, cx| cx.notify()).detach();
                            // 回车选中第一个匹配项
                            cx.subscribe_in(&st, window, |this, st, ev: &InputEvent, _, cx| {
                                if matches!(ev, InputEvent::PressEnter { .. }) {
                                    let q = st.read(cx).value().to_string();
                                    if let Some(first) = this.src_matches(&q).into_iter().next() {
                                        this.pick_source(first.0, cx);
                                    }
                                }
                            })
                            .detach();
                            window.focus(&st.focus_handle(cx), cx);
                            l.search = Some(st);
                        } else {
                            l.search = None;
                        }
                        cx.notify();
                    })),
            )
            .when(open, |d| {
                let panel = div()
                    .id("left-src-menu")
                    .absolute()
                    .top(px(32.0))
                    .left_0()
                    .w(px(DROPDOWN_COMPACT_W + 52.0))
                    .p(px(3.0))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .rounded(px(t::RADIUS_MD))
                    .bg(c::dialog())
                    .border_1()
                    .border_color(c::dialog_border())
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.files.left.menu = false;
                        this.files.left.search = None;
                        cx.notify();
                    }))
                    .children(search.map(|st| {
                        div()
                            .h(px(30.0))
                            .px(px(8.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .rounded(px(t::RADIUS_SM))
                            .bg(c::sunken())
                            .border_1()
                            .border_color(c::border())
                            .child(icon(IconName::Search, 12.0, c::muted()))
                            .child(div().flex_1().min_w_0().child(Input::new(&st).appearance(false).with_size(px(12.0 / 0.875))))
                    }))
                    .child(
                        div()
                            .id("left-src-list")
                            .max_h(px(300.0))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .when(rows.is_empty(), |d| d.child(div().h(px(30.0)).px(px(9.0)).flex().items_center().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child("没有匹配的服务器")))
                            .children(rows.into_iter().enumerate().map(|(i, (id, label, ic))| {
                                div()
                                    .id(("left-src-row", i))
                                    .min_h(px(30.0))
                                    .flex_shrink_0()
                                    .px(px(9.0))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .rounded(px(t::RADIUS_SM))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(c::field_hover()))
                                    .child(icon(ic, 14.0, c::muted()))
                                    .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).text_color(c::text()).child(label))
                                    .when(id == cur_node, |d| d.child(icon(IconName::Check, 14.0, c::accent())))
                                    .on_click(cx.listener(move |this, _, _, cx| this.pick_source(id, cx)))
                            })),
                    );
                d.child(deferred(panel).with_priority(1))
            })
    }

    /// 左栏为服务器时的整栏
    pub(super) fn server_left_pane(&self, pane_w: f32, cx: &mut Context<Self>) -> Stateful<Div> {
        let l = &self.files.left;
        let path = l.path.clone();
        let up = parent(&path);
        let Some(s) = l.sftp.as_ref() else { return div().id("left-server") };
        let ready = matches!(s.status(), SftpStatus::Ready);
        let name = l.node.and_then(|id| self.node_rows().into_iter().find(|n| n.0 == id)).map(|n| n.1).unwrap_or_default();

        let mut crumbs = div()
            .flex_1()
            .min_w_0()
            .h(px(30.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .px(px(3.0))
            .rounded(px(t::RADIUS_MD))
            .bg(c::sunken())
            .border_1()
            .border_color(c::border())
            .overflow_hidden()
            .text_size(px(t::TYPE_CONTROL))
            .font_family(MONO)
            .child(
                div()
                    .id("scrumb-root")
                    .h(px(22.0))
                    .w(px(22.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(t::RADIUS_XS))
                    .cursor_pointer()
                    .hover(|s| s.bg(c::hover()))
                    .child(icon(IconName::HardDrive, 13.0, c::muted()))
                    .on_click(cx.listener(|this, _, _, cx| this.left_go("/".into(), cx))),
            );
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        for (i, sg) in segs.iter().enumerate() {
            let to = format!("/{}", segs[..=i].join("/"));
            let last = i + 1 == segs.len();
            crumbs = crumbs.child(icon(IconName::ChevronRight, 11.0, c::ghost())).child(
                div()
                    .id(("scrumb", i))
                    .flex_shrink_0()
                    .h(px(22.0))
                    .px(px(5.0))
                    .flex()
                    .items_center()
                    .rounded(px(t::RADIUS_SM))
                    .cursor_pointer()
                    .text_color(if last { c::text() } else { c::muted() })
                    .hover(|s| s.bg(c::hover()).text_color(c::text()))
                    .child(sg.to_string())
                    .on_click(cx.listener(move |this, _, _, cx| this.left_go(to.clone(), cx))),
            );
        }
        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .h(px(48.0))
            .flex_shrink_0()
            .px(px(12.0))
            .child(tool_btn("s-back", IconName::ArrowLeft, !l.back.is_empty()).on_click(cx.listener(|this, _, _, cx| this.left_back(cx))))
            .child(tool_btn("s-fwd", IconName::ArrowRight, !l.fwd.is_empty()).on_click(cx.listener(|this, _, _, cx| this.left_fwd(cx))))
            .child(tool_btn("s-up", IconName::ArrowUp, ready && path != "/").on_click(cx.listener(move |this, _, _, cx| this.left_go(up.clone(), cx))))
            .child(tool_btn("s-refresh", IconName::RefreshCw, ready).on_click(cx.listener(|this, _, _, cx| {
                let p = this.files.left.path.clone();
                if let Some(s) = this.files.left.sftp.as_ref() {
                    s.list(&p);
                }
                cx.notify();
            })))
            .child(crumbs);

        let centered = |ic: IconName, color: Rgba, title: String, sub: String| {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(48.0))
                .px(px(24.0))
                .gap(px(6.0))
                .child(icon(ic, 26.0, color))
                .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(title))
                .child(div().max_w(px(360.0)).text_center().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(sub))
        };
        let action = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .mt(px(8.0))
                .h(px(28.0))
                .px(px(14.0))
                .flex()
                .items_center()
                .rounded(px(t::RADIUS_MD))
                .bg(c::accent_soft())
                .text_size(px(t::TYPE_CONTROL))
                .text_color(c::accent())
                .cursor_pointer()
                .hover(|s| s.bg(c::accent().opacity(0.3)))
                .child(label)
        };

        let body: AnyElement = match s.status() {
            SftpStatus::Connecting => centered(IconName::LoaderCircle, c::accent(), format!("正在连接 {name}"), "稍候…".into()).into_any_element(),
            SftpStatus::HostPrompt(p) => div()
                .flex_1()
                .px(px(20.0))
                .child(crate::terminal::connecting::host_key_card(
                    &p,
                    cx.listener(|this, _, _, cx| {
                        if let Some(s) = this.files.left.sftp.as_ref() {
                            s.answer_host(true);
                        }
                        cx.notify();
                    }),
                    cx.listener(|this, _, _, cx| {
                        if let Some(s) = this.files.left.sftp.as_ref() {
                            s.answer_host(false);
                        }
                        cx.notify();
                    }),
                ))
                .into_any_element(),
            SftpStatus::Failed(e) => centered(IconName::TriangleAlert, c::warn(), format!("无法连接 {name}"), e)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .child(action("s-retry", "重试").on_click(cx.listener(|this, _, _, cx| {
                            let n = this.files.left.node;
                            this.set_left_source(n, false, cx);
                        })))
                        .child(action("s-local", "切回本地").on_click(cx.listener(|this, _, _, cx| this.set_left_source(None, true, cx)))),
                )
                .into_any_element(),
            SftpStatus::Ready => {
                let d = s.dir(&path);
                let mut entries: Vec<Entry> = d.as_ref().map(|d| d.entries.iter().map(entry_from_remote).collect()).unwrap_or_default();
                sort_entries(&mut entries, (SortKey::Name, true));
                let thead = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .h(px(30.0))
                    .flex_shrink_0()
                    .px(px(18.0))
                    .border_b_1()
                    .border_color(c::border())
                    .text_size(px(t::TYPE_CAPTION))
                    .text_color(c::ghost())
                    .child(div().flex_1().min_w_0().child("名称"))
                    .child(div().w(px(76.0)).flex_shrink_0().flex().justify_end().child("大小"))
                    .child(div().w(px(118.0)).flex_shrink_0().child("修改时间"));
                let mut rows = div().id("left-srv-rows").flex_1().min_h_0().overflow_y_scroll().flex().flex_col().px(px(8.0)).py(px(4.0));
                if let Some(err) = d.as_ref().and_then(|d| d.error.clone()) {
                    rows = rows.child(centered(IconName::TriangleAlert, c::warn(), "读取目录失败".into(), err));
                } else if entries.is_empty() {
                    rows = if d.as_ref().is_none_or(|d| d.loading) {
                        rows.child(centered(IconName::LoaderCircle, c::accent(), "正在读取".into(), String::new()))
                    } else {
                        rows.child(super::super::empty_state(IconName::FolderOpen, "空文件夹", "或没有访问权限"))
                    };
                }
                for (i, e) in entries.into_iter().enumerate() {
                    let on = l.sel.as_deref() == Some(e.name.as_str());
                    let is_dir = e.is_dir();
                    let (ic, color) = if is_dir { (IconName::Folder, c::accent()) } else { (IconName::File, c::muted()) };
                    let enter = is_dir.then(|| join(&path, &e.name));
                    let n = e.name.clone();
                    let row_w = pane_w - 16.0;
                    let drag_item = DragRemote { name: e.name.clone(), is_dir, left: true };
                    let folder_to = is_dir.then(|| join(&path, &e.name));
                    rows = rows.child(
                        div()
                            .id(("srow", i))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .h(px(32.0))
                            .flex_shrink_0()
                            .px(px(10.0))
                            .rounded(px(t::RADIUS_MD))
                            .cursor_pointer()
                            .when(on, |d| d.bg(c::selected()))
                            .when(!on, |d| d.hover(|s| s.bg(c::hover())))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(9.0))
                                    .child(icon(ic, 15.0, color))
                                    .child(div().min_w_0().truncate().text_size(px(t::TYPE_BODY)).text_color(if on { c::text() } else { c::secondary() }).child(e.name.clone())),
                            )
                            .child(
                                div()
                                    .w(px(76.0))
                                    .flex_shrink_0()
                                    .flex()
                                    .justify_end()
                                    .text_size(px(t::TYPE_CAPTION))
                                    .font_family(MONO)
                                    .text_color(c::muted())
                                    .child(if is_dir { "—".to_string() } else { fmt_size(e.size) }),
                            )
                            .child(div().w(px(118.0)).flex_shrink_0().truncate().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::muted()).child(e.modified.clone()))
                            .on_drag(drag_item, move |d, _, _, cx| {
                                let (name, is_dir) = (d.name.clone(), d.is_dir);
                                cx.new(|_| DragGhost { name, is_dir, w: row_w })
                            })
                            .when_some(folder_to, |d, to| {
                                let (to2, to3) = (to.clone(), to);
                                d.drag_over::<DragLocal>(|s, _, _, _| s.bg(c::accent_soft()))
                                    .on_drop(cx.listener(move |this, it: &DragLocal, _, cx| {
                                        cx.stop_propagation();
                                        this.left_upload(vec![it.path.clone()], to2.clone(), cx);
                                    }))
                                    .drag_over::<DragRemote>(|s, it, _, _| if it.left { s } else { s.bg(c::accent_soft()) })
                                    .on_drop(cx.listener(move |this, it: &DragRemote, _, cx| {
                                        cx.stop_propagation();
                                        if !it.left {
                                            this.relay_drop(it, true, to3.clone(), cx);
                                        }
                                    }))
                            })
                            .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                                if ev.click_count() >= 2
                                    && let Some(to) = &enter
                                {
                                    this.left_go(to.clone(), cx);
                                } else {
                                    this.files.left.sel = Some(n.clone());
                                    cx.notify();
                                }
                            })),
                    );
                }
                // 传输进度(目的是左栏的传输记在左栏连接上)
                let running: Vec<_> = s.transfers().into_iter().filter(|x| x.state == TransferState::Running).collect();
                let status = (!running.is_empty()).then(|| {
                    let total: f32 = running.iter().map(|x| x.total as f32).sum();
                    let done: f32 = running.iter().map(|x| x.done as f32).sum();
                    let pct = if total > 0.0 { (done / total * 100.0).min(100.0) } else { 0.0 };
                    div()
                        .h(px(28.0))
                        .flex_shrink_0()
                        .px(px(18.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .border_t_1()
                        .border_color(c::border())
                        .text_size(px(t::TYPE_CAPTION))
                        .text_color(c::muted())
                        .child(icon(IconName::LoaderCircle, 12.0, c::accent()))
                        .child(format!("传输中 {} 项 · {pct:.0}%", running.len()))
                });
                div().flex_1().min_h_0().flex().flex_col().child(thead).child(rows).children(status).into_any_element()
            }
        };

        let cur = path.clone();
        div()
            .id("left-server")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(c::border())
            .drag_over::<DragLocal>(|s, _, _, _| s.bg(c::accent().opacity(0.05)))
            .on_drop(cx.listener({
                let cur = cur.clone();
                move |this, it: &DragLocal, _, cx| this.left_upload(vec![it.path.clone()], cur.clone(), cx)
            }))
            .drag_over::<DragRemote>(|s, it, _, _| if it.left { s } else { s.bg(c::accent().opacity(0.05)) })
            .on_drop(cx.listener(move |this, it: &DragRemote, _, cx| {
                if !it.left {
                    this.relay_drop(it, true, cur.clone(), cx);
                }
            }))
            .child(toolbar)
            .child(body)
    }
}
