//! 节点菜单、堆栈页、汇总与容器卡片。

use super::super::*;

pub(crate) fn menu_count(node: &Node, snap: &crate::docker::Snapshot, page: Page) -> Option<usize> {
    match page {
        Page::Overview | Page::Registry | Page::Config | Page::Processes | Page::Services | Page::Storage | Page::Cron | Page::Websites => None,
        Page::Containers => Some(snap.containers.len()),
        Page::Database => Some(crate::dbinst::detect(&snap.containers).len()),
        Page::Stacks => Some(node.stacks.len()),
        Page::Images => Some(snap.images.len()),
        Page::Volumes => Some(snap.volumes.len()),
    }
}

/// 若干服务的合计:CPU 求和、内存求和、两条曲线取均值。
pub(crate) struct Agg {
    cpu: f32,
    mem_mb: f32,
    cpu_s: [f32; SPARK_N],
    mem_s: [f32; SPARK_N],
}

impl Agg {
    pub(crate) fn cpu(&self) -> f32 {
        self.cpu
    }
    pub(crate) fn mem_mb(&self) -> f32 {
        self.mem_mb
    }
    pub(crate) fn cpu_s(&self) -> [f32; SPARK_N] {
        self.cpu_s
    }
    pub(crate) fn mem_s(&self) -> [f32; SPARK_N] {
        self.mem_s
    }
}

pub(crate) fn aggregate<'a>(svcs: impl Iterator<Item = &'a Service>) -> Agg {
    let mut a = Agg { cpu: 0.0, mem_mb: 0.0, cpu_s: [0.0; SPARK_N], mem_s: [0.0; SPARK_N] };
    let mut n = 0.0;
    for s in svcs {
        n += 1.0;
        a.cpu += s.cpu_v;
        a.mem_mb += s.mem_mb;
        for i in 0..SPARK_N {
            a.cpu_s[i] += s.cpu_s[i];
            a.mem_s[i] += s.mem_s[i];
        }
    }
    if n > 0.0 {
        for i in 0..SPARK_N {
            a.cpu_s[i] /= n;
            a.mem_s[i] /= n;
        }
    }
    a
}

pub(crate) fn status_of(up: usize, total: usize) -> (&'static str, Rgba, Rgba) {
    if up == total {
        ("运行中", c::online(), c::online_soft())
    } else if up == 0 {
        ("已停止", c::muted(), c::idle_soft())
    } else {
        ("部分运行", c::warn(), c::warn_soft())
    }
}

impl Home {
    pub(crate) fn menu(&self, cx: &mut Context<Self>) -> Div {
        let node = self.node();
        let mut col = div()
            .w(px(MENU_W))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(12.0))
            .bg(c::chrome())
            .border_l_1()
            .border_r_1()
            .border_color(c::border())
            .child(
                div()
                    .px(px(10.0))
                    .pt(px(4.0))
                    .pb(px(8.0))
                    .text_size(px(t::TYPE_CAPTION))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c::ghost())
                    .child("节点详情"),
            );
        // 侧边栏:概览,把容器/堆栈/镜像/存储卷合在一起的「容器」(里面用页签切换),以及服务器的运维页
        for (i, p) in [Page::Overview, Page::Containers, Page::Database, Page::Processes, Page::Services, Page::Storage, Page::Cron, Page::Websites].into_iter().enumerate() {
            let active = if p == Page::Containers { self.page.is_docker() } else { self.page == p };
            let count = menu_count(node, &self.dock.snap, p);
            col = col.child(
                div()
                    .id(("m", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .h(px(34.0))
                    .px(px(10.0))
                    .rounded(px(t::RADIUS_MD))
                    .cursor_pointer()
                    .text_size(px(t::TYPE_BODY))
                    .when(active, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::SEMIBOLD))
                    .when(!active, |d| {
                        d.border_1()
                            .border_color(gpui_kit::transparent_black())
                            .text_color(c::muted())
                            .hover(|s| s.bg(c::hover()).text_color(c::secondary()))
                    })
                    .child(icon(p.icon(), 15.0, if active { c::accent() } else { c::ghost() }))
                    .child(div().flex_1().child(p.label()))
                    .when_some(count.filter(|n| *n > 0), |d, n| {
                        d.child(
                            div()
                                .min_w(px(20.0))
                                .h(px(18.0))
                                .px(px(5.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(if active { c::accent_soft() } else { c::idle_soft() })
                                .text_size(px(t::TYPE_MICRO))
                                .font_family(MONO)
                                .text_color(if active { c::accent() } else { c::muted() })
                                .child(n.to_string()),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.page = p;
                        cx.notify();
                    })),
            );
        }
        col
    }

    pub(crate) fn content(&mut self, window: &Window, cx: &mut Context<Self>) -> Div {
        let key = self.workspace.active.map(|a| (self.workspace.tabs[a].id, self.page));
        if self.page_scroll_key != key {
            self.page_scroll_key = key;
            self.page_scroll.set_offset(point(px(0.0), px(0.0)));
        }
        let width = f32::from(window.viewport_size().width) - MENU_W - CONTENT_PAD * 2.0;
        let inner = width - 2.0 - STACK_BODY_PAD * 2.0;
        self.cols = (((inner + SVC_GAP) / (SVC_MIN_W + SVC_GAP)).floor() as usize).max(1);
        self.svc_w = ((inner - SVC_GAP * (self.cols - 1) as f32) / self.cols as f32).floor();

        self.dock.view_w = width;
        let gate = self.docker_gate(cx);
        let gated = gate.is_some();
        let body: AnyElement = if let Some(g) = gate {
            g
        } else if self.page != Page::Stacks {
            self.docker_page(cx)
        } else if self.node().stacks.is_empty() {
            empty_state(IconName::Layers, "还没有堆栈", "这个节点上没有发现 Compose 项目").into_any_element()
        } else {
            // 页头和页签是列表的第 0 项,跟着一起滚;其余项下标减一
            let sizes: Rc<Vec<Size<Pixels>>> = Rc::new(
                std::iter::once(size(px(800.0), px(STACK_HEAD_H)))
                    .chain(self.node().stacks.iter().map(|s| size(px(800.0), px(self.stack_height(s)))))
                    .collect(),
            );
            let view = cx.entity();
            // 列表放进绝对定位的满尺寸容器:给它一个确定的高度,
            // 否则 Infer 尺寸在上方有汇总行时会按内容高度撑开,首项被顶出视口。
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
                .child(div().absolute().inset_0().child(
                    v_virtual_list(view, "stacks", sizes, |this, range, _w, cx| {
                        range.map(|ix| if ix == 0 { this.dock_head_block(cx) } else { this.stack_card(ix - 1, cx) }).collect::<Vec<_>>()
                    })
                    .track_scroll(&self.scroll),
                ))
                .into_any_element()
        };

        let header = self.dock_head(gated, cx);
        let head_in_list = self.page == Page::Stacks && !gated && !self.node().stacks.is_empty();
        let scroll_header = !gated && (self.page == Page::Overview || self.page.scrolls_whole() || self.conf_scrolls_whole());

        let tabs = (self.has_page_tabs() && !gated).then(|| self.docker_tabs_slot(cx).into_any_element());
        let sticky = (!gated && self.dock_stuck()).then(|| self.dock_sticky_bar(window, cx));
        let inner: Vec<AnyElement> = if head_in_list {
            vec![body]
        } else if scroll_header {
            // 配置页表单模式:保存栏钉在滚动区下面
            let pinned = if self.page == Page::Config { self.dock.conf.bar.take() } else { None };
            let mut v = vec![div().id("overview-scroll").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.page_scroll).on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify())).flex().flex_col().child(header).children(tabs).child(body).into_any_element()];
            v.extend(pinned);
            v
        } else {
            vec![header.into_any_element(), tabs.unwrap_or_else(|| div().into_any_element()), body]
        };
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .px(px(CONTENT_PAD))
            .relative()
            // 页头跟着内容一起滚动(列表页整页滚;堆栈页页头是虚拟列表的第一项)
            .children(inner)
            .children(sticky)
    }

    /// 页头:图标、标题、副标题和右侧的操作按钮。`gated` 为真(没连上、没权限等)时不显示操作
    fn dock_head(&self, gated: bool, cx: &mut Context<Self>) -> Div {
        let hide = self.hide_charts;
        // 吸顶时按钮已经移到顶部那一条里,这里不再画(同一个按钮只画一份)
        let gated = gated || self.dock_stuck();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .pt(px(18.0))
            .pb(px(14.0))
            .flex_shrink_0()
            .child(
                div()
                    .size(px(36.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .glass(t::RADIUS_LG, GlassStyle::CONTROL)
                    .child(icon(self.page.icon(), 18.0, c::secondary())),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).child(self.page.label()))
                    // 副标题行定高:数据库页这里是带标签的实例信息,比纯文字高,不定高的话切页签时页头会跳
                    .child(
                        div().h(px(24.0)).flex().items_center().child(match (self.page == Page::Database && !gated).then(|| self.dbm_header_info()).flatten() {
                            Some(info) => info.into_any_element(),
                            None => div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(self.dock_subtitle()).into_any_element(),
                        }),
                    ),
            )
            .child(div().flex_1())
            .when(!gated, |d| {
                d.gap(px(8.0))
                    .when(self.page == Page::Stacks, |d| {
                        d.child(
                            button("toggle-charts", if hide { IconName::Eye } else { IconName::EyeOff }, if hide { "显示图表" } else { "隐藏图表" })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.hide_charts = !this.hide_charts;
                                    cx.notify();
                                })),
                        )
                    })
                    .children(self.dock_header_actions(cx))
            })
    }

    /// 页头和页签一起,堆栈页放在虚拟列表的第一项里跟着滚动
    fn dock_head_block(&self, cx: &mut Context<Self>) -> Div {
        div().h(px(STACK_HEAD_H)).flex().flex_col().child(self.dock_head(false, cx)).child(self.docker_tabs_slot(cx))
    }

    /// 页签在页头下面的位置:吸顶时真正的页签画在顶部,这里留一个同样高的空位,内容不跳
    fn docker_tabs_slot(&self, cx: &mut Context<Self>) -> Div {
        if self.dock_stuck() { div().h(px(DOCK_TABS_H)).flex_shrink_0() } else { self.docker_tabs(cx) }
    }

    /// 页头(图标、标题、副标题)已经滚出去、页签/按钮该吸在顶部了吗。整页滚动的容器、镜像、存储卷、仓库、进程、服务、存储、定时任务页和堆栈页有
    pub(crate) fn dock_stuck(&self) -> bool {
        if !(self.page.is_docker() || self.page == Page::Database || self.page.is_sys()) || (self.page == Page::Config && !self.conf_scrolls_whole()) {
            return false;
        }
        let h = if self.page == Page::Stacks { self.scroll.base_handle() } else { &self.page_scroll };
        // 内容没超出高度时滚轮也会把偏移改成负数,实际没有滚动;偏移不能超过可滚动的最大值
        let off = (-f32::from(h.offset().y)).min(f32::from(h.max_offset().y));
        off > DOCK_HEAD_H
    }

    /// 吸顶条:页签 + 页头右侧的按钮/搜索,底是窗口底(底色 + 光晕)按窗口坐标对齐画的一份
    fn dock_sticky_bar(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let vp = window.viewport_size();
        let bg = div().absolute().left(px(-MENU_W)).top(px(-TITLEBAR_H)).w(vp.width).h(vp.height).child(backdrop());
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .overflow_hidden()
            .bg(c::bg())
            .child(bg)
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(CONTENT_PAD))
                    .pt(px(10.0))
                    .child(if self.has_page_tabs() {
                        self.docker_tabs(cx)
                    } else {
                        // 没有页签的页面(进程、服务等):吸顶条左边放页面图标和名字
                        div().h(px(30.0)).mb(px(10.0)).flex().flex_row().items_center().gap(px(8.0)).child(icon(self.page.icon(), 16.0, c::secondary())).child(div().text_size(px(t::TYPE_TITLE)).font_weight(FontWeight::BOLD).child(self.page.label()))
                    })
                    .child(div().flex_1())
                    .children(self.dock_header_actions(cx)),
            )
    }

    /// 容器相关四个页面的页签:容器 / 堆栈 / 镜像 / 存储卷
    /// 页头下面有页签的页面:容器相关页、数据库页、装好了的网站页
    fn has_page_tabs(&self) -> bool {
        self.page.is_docker() || self.page == Page::Database || (self.page == Page::Websites && self.web_installed())
    }

    fn docker_tabs(&self, cx: &mut Context<Self>) -> Div {
        if self.page == Page::Database {
            return self.db_tabs(cx);
        }
        if self.page == Page::Websites {
            return self.web_tabs(cx);
        }
        let node = self.node();
        let mut row = div().flex().flex_row().items_center().gap(px(4.0)).pb(px(10.0)).flex_shrink_0();
        for (i, p) in [Page::Containers, Page::Stacks, Page::Images, Page::Volumes, Page::Registry, Page::Config].into_iter().enumerate() {
            let active = self.page == p;
            let count = menu_count(node, &self.dock.snap, p).filter(|n| *n > 0);
            row = row.child(
                div()
                    .id(("dtab", i))
                    .h(px(30.0))
                    .px(px(12.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .text_size(px(t::TYPE_CONTROL))
                    .when(active, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::SEMIBOLD))
                    .when(!active, |d| d.rounded(px(t::RADIUS_MD)).border_1().border_color(gpui_kit::transparent_black()).text_color(c::muted()).hover(|s| s.bg(c::field_hover()).text_color(c::secondary())))
                    .child(icon(p.icon(), 14.0, if active { c::accent() } else { c::ghost() }))
                    .child(p.label())
                    .when_some(count, |d, n| d.child(div().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(if active { c::accent() } else { c::ghost() }).child(n.to_string())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.page = p;
                        cx.notify();
                    })),
            );
        }
        row
    }

    pub(crate) fn stack_card(&self, ix: usize, cx: &mut Context<Self>) -> Div {
        let s = &self.node().stacks[ix];
        let up = s.running();
        let total = s.services.len();
        let (label, fg, bg) = status_of(up, total);
        let agg = aggregate(s.services.iter());
        let hide = self.hide_charts;
        let first = &s.services[0];
        let image = if total > 1 {
            format!("{} +{}", first.image, total - 1)
        } else {
            first.image.clone()
        };

        // 每个服务一个小方块,一眼看出几个在跑
        let pips = div().flex().flex_row().gap(px(3.0)).children(s.services.iter().map(|svc| {
            div()
                .size(px(7.0))
                .rounded(px(2.0))
                .bg(if svc.state == Running::Up { c::online() } else { c::border_strong() })
        }));

        let header = div()
            .id(("sh", ix))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            // 减去卡片上下边框,保证实际高度 == stack_height()
            .h(px(STACK_HEADER_H - 2.0))
            .pl(px(14.0))
            .pr(px(10.0))
            .cursor_pointer()
            .when(s.expanded, |d| d.rounded_t(px(t::RADIUS_XL - 1.0)))
            .when(!s.expanded, |d| d.rounded(px(t::RADIUS_XL - 1.0)))
            .hover(|st| st.bg(c::field_hover()))
            .child(icon(
                if s.expanded { IconName::ChevronDown } else { IconName::ChevronRight },
                14.0,
                c::ghost(),
            ))
            .child(
                div()
                    .size(px(28.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(t::RADIUS_MD))
                    .bg(bg)
                    .child(icon(IconName::Layers, 14.0, fg)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(div().text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::BOLD).child(s.name.clone()))
                            .child(pips),
                    )
                    .child(
                        div()
                            .text_size(px(t::TYPE_CAPTION))
                            .font_family(MONO)
                            .text_color(c::ghost())
                            .truncate()
                            .child(image),
                    ),
            )
            .child(metric_col("CPU", format!("{:.1}%", agg.cpu), &agg.cpu_s, c::cpu(), hide))
            .child(metric_col("内存", mock::fmt_mem(agg.mem_mb), &agg.mem_s, c::mem(), hide))
            .child(div().w(px(92.0)).flex().justify_end().child(pill(label, fg, bg)))
            .children((s.name != "独立容器").then(|| {
                let name = s.name.clone();
                super::syspages::tip(super::dockerpages::icon_btn(("stack-edit", ix), IconName::Pencil, c::muted()), "编辑 compose 文件").on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.comp_open(&name, window, cx);
                }))
            }))
            .child(
                div()
                    .id(("more", ix))
                    .size(px(28.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(t::RADIUS_SM))
                    .hover(|st| st.bg(c::hover()))
                    .child(icon(IconName::Ellipsis, 15.0, c::muted())),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                let st = &mut this.node_mut().stacks[ix];
                st.expanded = !st.expanded;
                cx.notify();
            }));

        let mut card = div()
            .w_full()
            .rounded(px(t::RADIUS_XL))
            .border_1()
            .border_color(c::dialog_border())
            .overflow_hidden()
            .child(header);

        if s.expanded {
            let svc_h = if hide { SVC_H_PLAIN } else { SVC_H_CHART };
            let mut grid = div()
                .flex()
                .flex_col()
                .gap(px(SVC_GAP))
                .p(px(STACK_BODY_PAD))
                .pt(px(STACK_BODY_PAD - 1.0)) // 让出 border_t 的 1px
                .bg(c::sunken().opacity(0.5))
                .border_t_1()
                .border_color(c::border());
            for (ri, row) in s.services.chunks(self.cols).enumerate() {
                let mut r = div().flex().flex_row().gap(px(SVC_GAP));
                for (ci, svc) in row.iter().enumerate() {
                    let target = files::Target::Container { stack: s.name.clone(), svc: svc.name.clone() };
                    let (fst, fsv) = (s.name.clone(), svc.name.clone());
                    let on_files = cx.listener(move |this, _, window, cx| this.ct_open_stack(&fst, &fsv, &target, super::ctpanel::CtTab::Files, window, cx));
                    let (st, sv, tg) = (s.name.clone(), svc.name.clone(), files::Target::Container { stack: s.name.clone(), svc: svc.name.clone() });
                    let tg_logs = tg.clone();
                    let on_term = cx.listener(move |this, _, window, cx| this.ct_open_stack(&st, &sv, &tg, super::ctpanel::CtTab::Term, window, cx));
                    let (st, sv) = (s.name.clone(), svc.name.clone());
                    let on_logs = cx.listener(move |this, _, window, cx| this.ct_open_stack(&st, &sv, &tg_logs, super::ctpanel::CtTab::Logs, window, cx));
                    let (st, sv) = (s.name.clone(), svc.name.clone());
                    let on_toggle = cx.listener(move |this, _, _, cx| {
                        if let Some((n, up, false)) = this.ct_lookup(&st, &sv) {
                            this.dock_act(if up { crate::docker::Action::Stop(n) } else { crate::docker::Action::Start(n) }, cx);
                        }
                    });
                    let (st, sv) = (s.name.clone(), svc.name.clone());
                    let on_restart = cx.listener(move |this, _, _, cx| {
                        if let Some((n, _, false)) = this.ct_lookup(&st, &sv) {
                            this.dock_act(crate::docker::Action::Restart(n), cx);
                        }
                    });
                    let (st, sv) = (s.name.clone(), svc.name.clone());
                    let on_remove = cx.listener(move |this, _, _, cx| {
                        if let Some((n, _, false)) = this.ct_lookup(&st, &sv) {
                            this.ask(super::dockerpages::DockAsk { title: "删除容器", tile: IconName::Box, name: n.clone(), detail: "Docker 容器", body: "强制删除,容器内未保存到存储卷的数据会丢失", label: "删除容器", action: crate::docker::Action::Remove(n), list: Vec::new() }, cx);
                        }
                    });
                    let busy = self.ct_busy(&s.name, &svc.name);
                    r = r.child(service_card(ix * 1000 + ri * 10 + ci, svc, svc_h, self.svc_w, hide, busy, on_files, on_term, on_logs, on_toggle, on_restart, on_remove));
                }
                for _ in row.len()..self.cols {
                    r = r.child(div().flex_1());
                }
                grid = grid.child(r);
            }
            card = card.child(grid);
        }

        div().w_full().h(px(self.stack_height(s))).pb(px(STACK_GAP)).child(card)
    }
}

/// 堆栈头部的定宽指标列:上面标签+数值,下面迷你图。定宽保证各行上下对齐。
pub(crate) fn metric_col(label: &'static str, value: String, s: &[f32; SPARK_N], color: Rgba, hide: bool) -> Div {
    div()
        .w(px(110.0))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .justify_between()
                .child(div().text_size(px(t::TYPE_MICRO)).text_color(c::ghost()).child(label))
                .child(
                    div()
                        .text_size(px(t::TYPE_CONTROL))
                        .font_family(MONO)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(c::secondary())
                        .child(value),
                ),
        )
        .when(!hide, |d| d.child(spark(s, color, 12.0, 1.5)))
}

pub(crate) fn service_card(
    key: usize,
    svc: &Service,
    h: f32,
    w: f32,
    hide_charts: bool,
    busy: Option<&'static str>,
    on_files: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_term: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_logs: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_restart: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_remove: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let up = svc.state == Running::Up;
    let metric = |label: &'static str, value: &str, s: &[f32; SPARK_N], color: Rgba| {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.0))
                    .text_size(px(t::TYPE_MICRO))
                    .text_color(c::ghost())
                    .child(div().size(px(6.0)).rounded(px(1.5)).bg(color))
                    .child(label),
            )
            .child(
                div()
                    .truncate()
                    .text_size(px(t::TYPE_CONTROL))
                    .font_family(MONO)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c::secondary())
                    .child(value.to_string()),
            )
            .when(!hide_charts, |d| d.child(div().mt(px(2.0)).child(spark(s, if up { color } else { c::ghost() }, 20.0, 1.5))))
    };

    let head = div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(10.0))
        .child(
            div()
                .size(px(26.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(t::RADIUS_MD))
                .bg(if up { c::online_soft() } else { c::idle_soft() })
                .child(icon(IconName::Box, 14.0, if up { c::online() } else { c::ghost() })),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(div().truncate().text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::BOLD).child(svc.name.clone()))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(t::TYPE_CAPTION))
                        .text_color(c::ghost())
                        .child(div().flex_1().min_w_0().truncate().font_family(MONO).child(svc.image.clone()))
                        .child(
                            div()
                                .flex_shrink_0()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(4.0))
                                .text_size(px(t::TYPE_MICRO))
                                .child(icon(IconName::Clock, 11.0, c::ghost()))
                                .child(svc.uptime.clone()),
                        ),
                ),
        )
        .child(if up {
            pill("运行中", c::online(), c::online_soft())
        } else {
            pill("已停止", c::muted(), c::idle_soft())
        });

    // 端口能放几个:卡片宽 - 内边距 - 右侧操作按钮 - 插头图标,按等宽字估算每个标签宽度,
    // 放不下的折叠成 "+N"(只要有折叠,就给 +N 标签预留位置)。
    let port_w = |p: &str| 14.0 + p.chars().count() as f32 * PORT_CHAR_W + 5.0;
    let mut room = w - 24.0 - ACTIONS_W - 17.0;
    let mut shown = 0;
    for (i, p) in svc.ports.iter().enumerate() {
        let need = port_w(p) + if i + 1 < svc.ports.len() { PORT_MORE_W } else { 0.0 };
        if need > room && shown > 0 {
            break;
        }
        room -= port_w(p);
        shown += 1;
    }
    let ports = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .h(px(22.0))
        .overflow_hidden()
        .child(icon(IconName::Plug, 12.0, c::ghost()))
        .children(svc.ports[..shown].iter().map(|p| tag(p.clone()).mono().small()))
        .when(shown < svc.ports.len(), |d| {
            let all = svc.ports.clone();
            d.child(
                tag(format!("+{}", svc.ports.len() - shown)).mono().small().color(c::muted()).decorate(move |pill| {
                    pill.id(("more-ports", key))
                        .cursor_default()
                        .hover(|s| s.bg(c::selected()).text_color(c::text()))
                        .tooltip(move |_, cx| cx.new(|_| node_form::PortsTip(all.clone())).into())
                        .into_any_element()
                }),
            )
        });

    let action = |id: usize, ic: IconName, color: Rgba, tip: &'static str, verb: &'static str| {
        div()
            .id(("act", key * 10 + id))
            .h(px(24.0))
            .w(px(24.0))
            .justify_center()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .rounded(px(t::RADIUS_SM))
            .cursor_pointer()
            .text_size(px(t::TYPE_CAPTION))
            .text_color(c::muted())
            .hover(|s| s.bg(c::hover()).text_color(c::text()))
            .child(if busy == Some(verb) { dot_spinner(("act-spin", key * 10 + id), color).into_any_element() } else { icon(ic, 14.0, color).into_any_element() })
            .tooltip(move |_, cx| cx.new(|_| crate::files::Tip(tip.into())).into())
    };

    let actions = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .pt(px(6.0))
        .border_t_1()
        .border_color(c::border())
        .child(ports)
        .child(action(0, IconName::SquareTerminal, c::muted(), "终端", "").on_click(on_term))
        .child(action(1, IconName::ScrollText, c::muted(), "日志", "").on_click(on_logs))
        .child(action(6, IconName::FolderOpen, c::muted(), "文件", "").on_click(on_files))
        .child(div().w(px(1.0)).h(px(14.0)).mx(px(4.0)).bg(c::border()))
        .child(action(3, if up { IconName::Square } else { IconName::Play }, c::muted(), if up { "停止" } else { "启动" }, if up { "停止" } else { "启动" }).on_click(on_toggle))
        .child(action(4, IconName::RefreshCw, c::muted(), "重启", "重启").on_click(on_restart))
        .child(action(5, IconName::Trash, c::danger().opacity(0.85), "删除", "删除").on_click(on_remove));

    div()
        .flex_1()
        .min_w_0()
        .h(px(h))
        .flex()
        .flex_col()
        .justify_between()
        .p(px(12.0))
        .rounded(px(t::RADIUS_LG))
        .border_1()
        .border_color(c::border())
        .bg(c::sunken())
        .overflow_hidden()
        .child(head)
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(12.0))
                .child(metric("CPU", &svc.cpu, &svc.cpu_s, c::cpu()))
                .child(metric("内存", &svc.mem, &svc.mem_s, c::mem()))
                .child(metric("网络", &svc.net, &svc.net_s, c::net()))
                .child(metric("磁盘", &svc.disk, &svc.disk_s, c::disk())),
        )
        .child(actions)
}
