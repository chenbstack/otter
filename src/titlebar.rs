//! 标题栏:红绿灯 + 标签(固定的“节点”页标签在最左)。标签是垂直居中的胶囊。

use std::time::Duration;

use super::*;
use workspace::{NodeMode, TabKind};

impl Home {
    pub(crate) fn titlebar(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        self.titlebar_inner(window, cx, false)
    }

    /// 接管确认框弹出时盖在遮罩上面的标题栏:透明底(下面就是遮罩),别的标签压暗且点不了,只有被申请的那个标签保持原样。
    pub(crate) fn titlebar_spotlight(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        self.titlebar_inner(window, cx, true).absolute().top_0().left_0().right_0()
    }

    /// Windows(自绘标题栏,系统不画按钮)和 Linux 客户端装饰的窗口:右上角自己画 最小化 / 最大化 / 关闭。
    /// macOS 的红绿灯由系统画在左侧;Linux 服务端装饰时窗口管理器自己有标题栏,这里不画。
    fn window_buttons(&self, window: &Window) -> Option<Div> {
        let win = cfg!(target_os = "windows");
        let csd = cfg!(target_os = "linux") && matches!(window.window_decorations(), Decorations::Client { .. });
        if !win && !csd {
            return None;
        }
        let sup = window.window_controls();
        let max = window.is_maximized();
        let btn = |id: &'static str, ic: IconName, area: WindowControlArea, close: bool| {
            div()
                .id(id)
                .w(px(46.0))
                .h_full()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .text_color(c::muted())
                .hover(move |s| if close { s.bg(c::danger()).text_color(c::on_accent()) } else { s.bg(c::hover()).text_color(c::text()) })
                // Windows 靠区域命中测试(系统处理点击、悬停的 Snap 布局);Linux 自己调窗口方法
                .when(win, |d| d.window_control_area(area))
                .when(!win, |d| {
                    d.on_mouse_down(MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        match area {
                            WindowControlArea::Min => window.minimize_window(),
                            WindowControlArea::Max => window.zoom_window(),
                            _ => window.remove_window(),
                        }
                    })
                })
                .child(icon(ic, 14.0, c::muted()))
        };
        Some(
            div()
                .flex_shrink_0()
                .h_full()
                .flex()
                .flex_row()
                .items_center()
                .when(win || sup.minimize, |d| d.child(btn("win-min", IconName::WindowMinimize, WindowControlArea::Min, false)))
                .when(win || sup.maximize, |d| d.child(btn("win-max", if max { IconName::WindowRestore } else { IconName::WindowMaximize }, WindowControlArea::Max, false)))
                .child(btn("win-close", IconName::WindowClose, WindowControlArea::Close, true)),
        )
    }

    fn titlebar_inner(&self, window: &mut Window, cx: &mut Context<Self>, spotlight: bool) -> Div {
        let active = self.workspace.active;

        let tab = |ix: usize, cx: &mut Context<Self>| {
            let tab = &self.workspace.tabs[ix];
            let on = active == Some(ix);
            let (title, kind_icon): (String, IconName) = match tab.node() {
                None => ("节点".into(), IconName::LayoutGrid),
                Some(id) => {
                    let (g, n) = self.locate(id);
                    let node = &self.groups[g].nodes[n];
                    match &tab.kind {
                        TabKind::Terminal { container, n, .. } => {
                            let who = &container.1;
                            let title = if *n > 1 { format!("{who} · 终端 {n}") } else { format!("{who} · 终端") };
                            (title, IconName::SquareTerminal)
                        }
                        TabKind::Logs { svc, .. } => (format!("{svc} · 日志"), IconName::ScrollText),
                        _ => {
                            // 同一节点开了多个标签(复制出来的)时加序号区分
                            let same: Vec<usize> = (0..self.workspace.tabs.len())
                                .filter(|&i| matches!(self.workspace.tabs[i].kind, TabKind::Node(n) if n == id))
                                .collect();
                            let title = match same.iter().position(|&i| i == ix) {
                                Some(k) if same.len() > 1 => format!("{} · {}", node.name, k + 1),
                                _ => node.name.clone(),
                            };
                            (title, tab.mode.icon())
                        }
                    }
                }
            };
            let pinned = matches!(tab.kind, TabKind::Nodes);
            // 服务器终端标签的图标颜色就是连接状态
            let icon_color = match tab.term.as_ref().map(|t| t.handle.status()) {
                Some(crate::terminal::ssh::Status::Connected) => c::online(),
                Some(crate::terminal::ssh::Status::Closed(_)) => c::danger(),
                Some(_) => c::warn(),
                None => if on { c::accent() } else { c::ghost() },
            };
            let taken = tab.takeover.is_some();
            // 有 agent 正在申请接管这个标签:警示色描边 + 闪烁的机器人徽标
            let asking = self.takeover_prompts.front().is_some_and(|p| p.tab_id == tab.id);
            let taken_by = tab.takeover.as_ref().map(|t| t.client_name.clone());
            let dim = spotlight && !asking;
            let busy = tab.busy_elapsed_ms();
            let group: SharedString = format!("tab-{ix}").into();
            div()
                .id(("tab", ix))
                .group(group.clone())
                .h(px(TAB_H))
                .flex_shrink_0()
                .when(!pinned, |d| d.min_w(px(110.0)).max_w(px(200.0)))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(7.0))
                .pl(px(10.0))
                .pr(px(if pinned { 12.0 } else { 6.0 }))
                .rounded(px(t::RADIUS_MD))
                .border_1()
                .text_size(px(t::TYPE_CONTROL))
                .cursor_pointer()
                .relative()
                .when(on, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::MEDIUM))
                .when(!on, |d| {
                    d.border_color(gpui_kit::transparent_black())
                        .text_color(c::muted())
                        .hover(|s| s.bg(c::hover()).text_color(c::secondary()))
                })
                // 被 agent 接管:整个标签描一圈强调色边,标题后面放一个呼吸的机器人徽标(后台标签也看得见)
                .when(taken, |d| d.border_color(c::accent().opacity(0.55)).bg(c::accent().opacity(0.08)))
                .when(dim, |d| d.opacity(0.4))
                .when(asking, |d| d.border_color(c::warn()).bg(c::warn().opacity(0.14)))
                .when_some(taken_by, |d, name| d.tooltip(move |_, cx| cx.new(|_| crate::files::Tip(format!("{name} 正在接管"))).into()))
                .child(icon(kind_icon, 14.0, icon_color))
                .child(div().flex_1().min_w_0().truncate().child(title))
                // 正在执行任务:一小段亮光沿标签边框绕圈,带渐隐的尾巴;后台标签也看得见
                .when_some(busy, |d, ms| d.child(busy_border(t::RADIUS_MD, c::accent(), ms)))
                .when(asking, |d| {
                    d.child(
                        div()
                            .flex_shrink_0()
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(t::RADIUS_XS))
                            .bg(c::warn().opacity(0.28))
                            .child(icon(IconName::Bot, 12.0, c::warn()))
                            .with_animation(
                                ("tab-asking", ix),
                                Animation::new(Duration::from_millis(900)).repeat().with_easing(ease_in_out),
                                |d, t| d.opacity(0.35 + 0.65 * (1.0 - (2.0 * t - 1.0).abs())),
                            ),
                    )
                })
                .when(taken, |d| {
                    d.child(
                        div()
                            .flex_shrink_0()
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(t::RADIUS_XS))
                            .bg(c::accent().opacity(0.2))
                            .child(icon(IconName::Bot, 12.0, c::accent()))
                            .with_animation(
                                ("tab-takeover", ix),
                                Animation::new(Duration::from_millis(2200)).repeat().with_easing(ease_in_out),
                                |d, t| d.opacity(0.6 + 0.4 * (1.0 - (2.0 * t - 1.0).abs())),
                            ),
                    )
                })
                .when(!pinned, |d| {
                    d.child(
                        div()
                            .id(("tab-close", ix))
                            .size(px(18.0))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(t::RADIUS_XS))
                            .when(!on, |d| d.opacity(0.0).group_hover(group.clone(), |s| s.opacity(1.0)))
                            .hover(|s| s.bg(c::selected()))
                            .child(icon(IconName::X, 12.0, c::muted()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, cx);
                            })),
                    )
                })
                .when(!spotlight, |d| {
                    d.on_click(cx.listener(move |this, _, _, cx| {
                        this.activate(ix);
                        cx.notify();
                    }))
                })
                .when(!pinned, |d| {
                    d.on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                            this.tab_menu = Some((ix, e.position));
                            cx.notify();
                        }),
                    )
                })
        };

        // 节点页固定在最左,其余标签在可横向滚动的区域里
        let pinned = tab(0, cx);
        let rest: Vec<_> = (1..self.workspace.tabs.len()).map(|ix| tab(ix, cx)).collect();

        // 最左侧:logo + 名称(不可点;Windows 上也算拖动区域)
        let brand = div()
            .id("titlebar-brand")
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .pr(px(14.0))
            .window_control_area(WindowControlArea::Drag)
            .child(gpui::img("logo/otter.png").size(px(24.0)).flex_shrink_0())
            .child(div().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).text_color(c::text()).child("Otter"));
        let buttons = self.window_buttons(window);
        let has_buttons = buttons.is_some();
        // 左边:macOS 给红绿灯留位置;其他系统和内容区对齐
        let pad_l = if cfg!(target_os = "macos") { TITLEBAR_PAD_L } else { CONTENT_PAD };
        // 标题栏的空白处可以拖动窗口:Windows 用命中区域(不能罩住标签,否则标签点不了),Linux 调 start_window_move
        let drag = div()
            .id("titlebar-drag")
            .flex_1()
            .min_w(px(12.0))
            .h_full()
            .window_control_area(WindowControlArea::Drag)
            .when(cfg!(target_os = "linux"), |d| {
                d.on_mouse_down(MouseButton::Left, |e, window, _| {
                    if e.click_count >= 2 {
                        window.zoom_window();
                    } else {
                        window.start_window_move();
                    }
                })
            });
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .h(px(TITLEBAR_H))
            .flex_shrink_0()
            .pl(px(pad_l))
            .pr(px(if has_buttons { 0.0 } else { CONTENT_PAD }))
            .child(brand)
            .child(pinned)
            .child(div().w(px(8.0)).flex_shrink_0())
            .child(div().id("tabs").flex().flex_row().items_center().gap(px(4.0)).min_w_0().overflow_x_scroll().children(rest))
            .child(drag)
            .child(div().flex().flex_row().items_center().when(has_buttons, |d| d.pr(px(8.0))).when(spotlight, |d| d.opacity(0.4)).children(self.mode_switch(cx)).children(self.update_button(cx)))
            .children(buttons)
    }

    /// 标题栏最右侧的更新按钮:发现新版本时出现一个小的下载图标,点一下直接更新。
    fn update_button(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        use crate::update::State;
        let (ic, color, bg, tip, clickable) = match &self.update {
            State::Idle => return None,
            State::Available(u) => (
                IconName::Download,
                c::accent(),
                c::accent_soft(),
                if u.asset.is_some() { format!("发现新版本 {},点击更新", u.version) } else { format!("发现新版本 {},点击前往下载", u.version) },
                true,
            ),
            State::Downloading(u) => (IconName::LoaderCircle, c::muted(), c::hover(), format!("正在下载 {}…", u.version), false),
            State::Failed(u, e) => (IconName::Download, c::warn(), c::warn_soft(), format!("更新到 {} 失败:{e}。点击重试", u.version), true),
        };
        Some(
            div()
                .id("update-btn")
                .ml(px(8.0))
                .flex_shrink_0()
                .size(px(TAB_H))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(t::RADIUS_MD))
                .bg(bg)
                .child(icon(ic, 15.0, color))
                .tooltip(move |_, cx| cx.new(|_| crate::files::Tip(tip.clone())).into())
                .when(clickable, |d| d.cursor_pointer().hover(|s| s.opacity(0.8)).on_click(cx.listener(|this, _, _, cx| this.start_update(cx)))),
        )
    }

    /// 标题栏右侧的模式切换:只在活动标签是节点标签时出现。
    fn mode_switch(&self, cx: &mut Context<Self>) -> Option<Div> {
        let a = self.workspace.active?;
        let tab = &self.workspace.tabs[a];
        if !matches!(tab.kind, TabKind::Node(_)) {
            return None;
        }
        let cur = tab.mode;
        let items = NodeMode::ALL.iter().enumerate().map(|(i, &m)| {
            let on = m == cur;
            // 和抽屉里的切换(`syspages::chip`)同一个样式:选中是玻璃底,没选中只有文字
            div()
                .id(("node-mode", i))
                .h(px(30.0))
                .px(px(12.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .text_size(px(t::TYPE_CONTROL))
                .when(on, |d| d.glass(t::RADIUS_MD, GlassStyle::CONTROL).text_color(c::text()).font_weight(FontWeight::SEMIBOLD))
                .when(!on, |d| d.rounded(px(t::RADIUS_MD)).border_1().border_color(gpui_kit::transparent_black()).text_color(c::muted()).hover(|s| s.bg(c::field_hover()).text_color(c::secondary())))
                .child(m.label())
                .on_click(cx.listener(move |this, _, window, cx| this.set_mode(m, window, cx)))
        });
        Some(div().flex_shrink_0().h(px(TAB_H)).flex().flex_row().items_center().gap(px(4.0)).children(items))
    }

    /// 标签右键菜单:铺满窗口的透明遮罩 + 菜单面板。
    pub(crate) fn tab_menu_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (ix, pos) = self.tab_menu?;
        let tab = self.workspace.tabs.get(ix)?;
        let is_node = matches!(tab.kind, TabKind::Node(_));
        let item = |id: &'static str, ic: IconName, label: &'static str, enabled: bool| {
            div()
                .id(id)
                .h(px(28.0))
                .px(px(10.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .rounded(px(t::RADIUS_SM))
                .text_size(px(t::TYPE_CONTROL))
                .when(enabled, |d| d.cursor_pointer().text_color(c::secondary()).hover(|s| s.bg(c::hover()).text_color(c::text())))
                .when(!enabled, |d| d.text_color(c::ghost()))
                .child(icon(ic, 14.0, c::ghost()))
                .child(label)
        };
        let panel = div()
            .id("tab-menu")
            .absolute()
            .left(pos.x)
            .top(pos.y)
            .w(px(168.0))
            .p(px(4.0))
            .flex()
            .flex_col()
            .rounded(px(t::RADIUS_MD))
            .bg(c::dialog())
            .border_1()
            .border_color(c::border())
            .occlude()
            .child(item("tab-dup", IconName::Copy, "复制标签", is_node).when(is_node, |d| {
                d.on_click(cx.listener(move |this, _, window, cx| {
                    this.tab_menu = None;
                    this.duplicate_tab(ix, window, cx);
                }))
            }))
            .child(item("tab-close-menu", IconName::X, "关闭标签", true).on_click(cx.listener(move |this, _, _, cx| {
                this.tab_menu = None;
                this.close_tab(ix, cx);
            })));
        Some(
            div()
                .id("tab-menu-mask")
                .absolute()
                .size_full()
                .top_0()
                .left_0()
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.tab_menu = None;
                    cx.notify();
                }))
                .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| {
                    this.tab_menu = None;
                    cx.notify();
                }))
                .child(panel)
                .into_any_element(),
        )
    }
}

/// 沿圆角矩形周长走 `d` 像素后的位置(从上边左端起,顺时针)。
fn rrect_point(b: Bounds<Pixels>, r: f32, d: f32) -> Point<Pixels> {
    let (x, y, w, h) = (f32::from(b.origin.x), f32::from(b.origin.y), f32::from(b.size.width), f32::from(b.size.height));
    let r = r.min(w / 2.0).min(h / 2.0);
    let (sw, sh, arc) = (w - 2.0 * r, h - 2.0 * r, std::f32::consts::FRAC_PI_2 * r);
    let total = 2.0 * (sw + sh) + 4.0 * arc;
    let mut d = d.rem_euclid(total);
    // 每段:长度 + 起点到点的映射
    let pt = |px_: f32, py_: f32| point(px(px_), px(py_));
    for (len, f) in [
        (sw, 0u8), (arc, 1), (sh, 2), (arc, 3), (sw, 4), (arc, 5), (sh, 6), (arc, 7),
    ] {
        if d <= len {
            let u = if len > 0.0 { d / len } else { 0.0 };
            let ang = std::f32::consts::FRAC_PI_2 * u;
            return match f {
                0 => pt(x + r + sw * u, y),
                1 => pt(x + w - r + r * ang.sin(), y + r - r * ang.cos()),
                2 => pt(x + w, y + r + sh * u),
                3 => pt(x + w - r + r * ang.cos(), y + h - r + r * ang.sin()),
                4 => pt(x + w - r - sw * u, y + h),
                5 => pt(x + r - r * ang.sin(), y + h - r + r * ang.cos()),
                6 => pt(x, y + h - r - sh * u),
                _ => pt(x + r - r * ang.cos(), y + r - r * ang.sin()),
            };
        }
        d -= len;
    }
    pt(x + r, y)
}

/// 标签「正在执行」的边框跑马灯:一段亮光绕边框一圈,尾巴渐隐。
fn busy_border(radius: f32, color: Rgba, elapsed_ms: u64) -> impl IntoElement {
    // 相位随这一轮忙碌的开始计时,所以每次都从头转起;之后每帧按实际流逝时间推进
    let began = std::time::Instant::now() - std::time::Duration::from_millis(elapsed_ms);
    gpui::canvas(
        |_, _, _| (),
        move |b, _, window, _| {
            const TAIL: usize = 14;
            const SEG: f32 = 7.0;
            let t = began.elapsed().as_secs_f32();
            let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
            let r = radius.min(w / 2.0).min(h / 2.0);
            let total = 2.0 * (w - 2.0 * r + h - 2.0 * r) + 2.0 * std::f32::consts::PI * r;
            let head = (crate::terminal::lap_phase(t as f64).fract() as f32) * total;
            for k in 0..TAIL {
                let (d1, d0) = (head - k as f32 * SEG, head - (k as f32 + 1.0) * SEG);
                let mut pb = gpui::PathBuilder::stroke(px(1.6));
                let steps = 3;
                for i in 0..=steps {
                    let p = rrect_point(b, r, d0 + (d1 - d0) * i as f32 / steps as f32);
                    if i == 0 { pb.move_to(p) } else { pb.line_to(p) }
                }
                if let Ok(path) = pb.build() {
                    let fade = 1.0 - k as f32 / TAIL as f32;
                    window.paint_path(path, color.opacity(0.9 * fade * fade));
                }
            }
            window.request_animation_frame();
        },
    )
    .absolute()
    .inset_0()
}
