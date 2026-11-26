//! 设置页:节点页侧栏最底部的「设置」(`NodesView::Settings`),⌘, 直接打开。
//! 第一版只有终端块(字体、字号、主题、关键字高亮);通用/日志/关于块见计划 2026-11-24-settings-page。

use super::*;
use crate::terminal::{highlight, theme};
use crate::ui::{MenuItem, button, select_sized};

/// 日志页「显示详情」的默认值,值 `on` / `off`
pub const KEY_LOGS_VERBOSE: &str = "logs_verbose";
/// 日志保留时间可选的天数
const KEEP_CHOICES: [i64; 4] = [7, 30, 90, 180];

#[derive(Clone, Copy, PartialEq, Default)]
pub(crate) enum SettingsTab {
    #[default]
    Terminal,
    General,
    About,
}

impl SettingsTab {
    pub(crate) const ALL: [SettingsTab; 3] = [SettingsTab::Terminal, SettingsTab::General, SettingsTab::About];

    fn label(self) -> &'static str {
        match self {
            SettingsTab::Terminal => "终端",
            SettingsTab::General => "通用",
            SettingsTab::About => "关于",
        }
    }

    fn icon(self) -> IconName {
        match self {
            SettingsTab::Terminal => IconName::SquareTerminal,
            SettingsTab::General => IconName::SlidersHorizontal,
            SettingsTab::About => IconName::Info,
        }
    }
}

/// 设置页自己的状态
#[derive(Default)]
pub(crate) struct SettingsUi {
    pub tab: SettingsTab,
    pub keep_open: bool,
    /// 「检查更新」正在进行 / 上次结果
    pub checking: bool,
    pub update_msg: Option<String>,
    /// 系统里的等宽字体;`None` = 还没枚举完
    pub mono_fonts: Option<Vec<String>>,
    pub loading: bool,
    pub font_open: bool,
    /// 当前展开的「连接」「AI 接管」下拉(控件 id)
    pub open_sel: Option<&'static str>,
    /// 内容区的滚动句柄:标题滚出去后,页签吸附在顶部
    pub scroll: ScrollHandle,
}

/// 设置页标题区的高度(标题滚出这么多后,页签吸附到顶部)
const SETTINGS_TITLE_H: f32 = 76.0;
const KEEPALIVE_CHOICES: [u64; 4] = [10, 15, 30, 60];
const TIMEOUT_CHOICES: [u64; 4] = [10, 15, 30, 60];
const IDLE_CHOICES: [u64; 4] = [10, 30, 60, 120];
const MAX_CHOICES: [u64; 4] = [30, 60, 120, 240];

/// 枚举系统字体并只留等宽的("i"和"W"一样宽)。字体多,放后台线程;解析不了的字体直接跳过。
fn detect_mono(ts: &std::sync::Arc<TextSystem>) -> Vec<String> {
    ts.all_font_names()
        .into_iter()
        .filter(|n| !n.starts_with('.') && !n.is_empty())
        .filter(|n| {
            let ts = ts.clone();
            let name = n.clone();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let id = ts.resolve_font(&font(name));
                match (ts.advance(id, px(100.0), 'i'), ts.advance(id, px(100.0), 'W')) {
                    (Ok(a), Ok(b)) => (f32::from(a.width) - f32::from(b.width)).abs() < 0.5 && f32::from(a.width) > 1.0,
                    _ => false,
                }
            }))
            .unwrap_or(false)
        })
        .collect()
}

impl Home {
    /// 第一次打开设置页时,后台枚举等宽字体
    pub(crate) fn load_mono_fonts(&mut self, cx: &mut Context<Self>) {
        if self.settings.mono_fonts.is_some() || self.settings.loading {
            return;
        }
        self.settings.loading = true;
        let ts = cx.text_system().clone();
        cx.spawn(async move |this, cx| {
            let list = cx.background_executor().spawn(async move { detect_mono(&ts) }).await;
            let _ = this.update(cx, |this, cx| {
                this.settings.mono_fonts = Some(list);
                this.settings.loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn save_pref(&self, key: &str, val: &str) {
        if let Some(st) = self.store.as_ref() {
            let _ = st.set_setting(key, val);
        }
    }

    /// 一行设置:左边名称+说明,右边控件
    fn setting_row(label: &'static str, desc: &'static str, control: impl IntoElement) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(16.0))
            .min_h(px(54.0))
            .px(px(16.0))
            .py(px(10.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_size(px(t::TYPE_BODY)).text_color(c::text()).child(label))
                    .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(desc)),
            )
            .child(div().flex_shrink_0().child(control))
    }

    fn setting_group(title: &'static str, rows: Vec<Div>) -> Div {
        let n = rows.len();
        // 和容器页的列表同一套外框:圆角描边、不加底色
        let mut card = div().flex().flex_col().rounded(px(t::RADIUS_XL)).border_1().border_color(c::dialog_border()).overflow_hidden();
        for (i, r) in rows.into_iter().enumerate() {
            card = card.child(r);
            if i + 1 < n {
                card = card.child(div().h(px(1.0)).mx(px(16.0)).bg(c::border()));
            }
        }
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().text_size(px(t::TYPE_CAPTION)).font_weight(FontWeight::SEMIBOLD).text_color(c::muted()).child(title))
            .child(card)
    }

    pub(crate) fn pill_switch(id: &'static str, on: bool) -> Stateful<Div> {
        div()
            .id(id)
            .w(px(36.0))
            .h(px(20.0))
            .rounded_full()
            .cursor_pointer()
            .flex()
            .flex_row()
            .items_center()
            .px(px(2.0))
            .bg(if on { c::accent() } else { c::idle_soft() })
            .when(on, |d| d.justify_end())
            .child(div().size(px(16.0)).rounded_full().bg(c::on_accent()))
    }

    fn term_theme_cards(&self, cx: &mut Context<Self>) -> Div {
        let cur = theme::current_id();
        div().flex().flex_row().flex_wrap().gap(px(10.0)).children(theme::THEMES.iter().enumerate().map(|(i, th)| {
            let active = th.id == cur;
            let rgb8 = |v: theme::Rgb| rgb(((v.0 as u32) << 16) | ((v.1 as u32) << 8) | v.2 as u32);
            let id = th.id;
            div()
                .id(("term-theme", i))
                .w(px(150.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .p(px(8.0))
                .rounded(px(t::RADIUS_MD))
                .cursor_pointer()
                .border_1()
                .border_color(if active { c::accent() } else { c::border() })
                .hover(|s| s.bg(c::hover()))
                .child(
                    div()
                        .h(px(44.0))
                        .px(px(8.0))
                        .py(px(6.0))
                        .rounded(px(t::RADIUS_SM))
                        .border_1()
                        .border_color(c::border())
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(div().flex().flex_row().gap(px(4.0)).children([2usize, 3, 4, 1].map(|k| div().w(px(22.0)).h(px(5.0)).rounded(px(2.0)).bg(rgb8(th.ansi[k])))))
                        .child(div().flex().flex_row().gap(px(4.0)).children([5usize, 6, 10, 12].map(|k| div().w(px(22.0)).h(px(5.0)).rounded(px(2.0)).bg(rgb8(th.ansi[k]))))),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(div().flex_1().text_size(px(t::TYPE_CONTROL)).text_color(if active { c::text() } else { c::secondary() }).child(th.name))
                        .when(active, |d| d.child(icon(IconName::Check, 13.0, c::accent()))),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    theme::set_theme(id);
                    this.save_pref(theme::KEY_THEME, id);
                    this.apply_term_prefs(cx);
                }))
        }))
    }

    /// 预览:当前主题 + 字体 + 字号下的几行假输出;背景和真实终端一样透明,不画主题的背景色
    fn term_preview() -> Div {
        let th = theme::current();
        let rgb8 = |v: theme::Rgb| rgb(((v.0 as u32) << 16) | ((v.1 as u32) << 8) | v.2 as u32);
        let line = |parts: Vec<(&'static str, theme::Rgb)>| div().flex().flex_row().children(parts.into_iter().map(|(s, col)| div().text_color(rgb8(col)).child(s)));
        div()
            .flex()
            .flex_col()
            .p(px(12.0))
            .rounded(px(t::RADIUS_MD))
            .border_1()
            .border_color(c::border())
            .font_family(theme::font())
            .text_size(px(theme::size()))
            .overflow_hidden()
            .whitespace_nowrap()
            // 提示符
            .child(line(vec![("deploy@web-01", th.ansi[10]), (":", th.fg), ("~/app", th.ansi[12]), ("$ ", th.fg), ("ls -l", th.fg)]))
            .child(line(vec![("drwxr-xr-x  5 deploy deploy 4096 Nov 24 09:12 ", th.fg), ("src", th.ansi[12])]))
            .child(line(vec![("-rwxr-xr-x  1 deploy deploy  812 Nov 23 18:40 ", th.fg), ("deploy.sh", th.ansi[10])]))
            .child(line(vec![("-rw-r--r--  1 deploy deploy 1204 Nov 20 11:03 compose.yaml", th.fg)]))
            .child(line(vec![("deploy@web-01", th.ansi[10]), (":", th.fg), ("~/app", th.ansi[12]), ("$ ", th.fg), ("git status -sb", th.fg)]))
            .child(line(vec![("## ", th.fg), ("main", th.ansi[2]), ("...", th.fg), ("origin/main", th.ansi[1]), (" [ahead 1]", th.fg)]))
            .child(line(vec![(" M", th.ansi[1]), (" src/app.rs", th.fg)]))
            .child(line(vec![("??", th.ansi[1]), (" notes.md", th.fg)]))
            .child(line(vec![("deploy@web-01", th.ansi[10]), (":", th.fg), ("~/app", th.ansi[12]), ("$ ", th.fg), ("docker ps", th.fg)]))
            .child(line(vec![("CONTAINER ID   IMAGE         STATUS         PORTS", th.fg)]))
            .child(line(vec![("a1b2c3d4e5f6   ", th.fg), ("nginx:1.27   ", th.ansi[6]), ("Up 3 days     ", th.ansi[2]), ("0.0.0.0:80->80/tcp", th.fg)]))
            .child(line(vec![("9f8e7d6c5b4a   ", th.fg), ("redis:7       ", th.ansi[6]), ("Exited (1)    ", th.ansi[1])]))
            .child(line(vec![("deploy@web-01", th.ansi[10]), (":", th.fg), ("~/app", th.ansi[12]), ("$ ", th.fg), ("tail -n 3 app.log", th.fg)]))
            .child(line(vec![("09:12:01 ", th.ansi[8]), ("INFO  ", th.ansi[2]), ("listening on ", th.fg), ("192.168.1.8:8080", th.ansi[13])]))
            .child(line(vec![("09:12:07 ", th.ansi[8]), ("WARN  ", th.ansi[3]), ("disk usage 91% on /var", th.fg)]))
            .child(line(vec![("09:12:09 ", th.ansi[8]), ("ERROR ", th.ansi[1]), ("connect to ", th.fg), ("https://api.example.com", th.ansi[12]), (" refused", th.fg)]))
            .child(line(vec![("deploy@web-01", th.ansi[10]), (":", th.fg), ("~/app", th.ansi[12]), ("$ ", th.fg), ("中文显示 0O1lI ", th.fg), ("█", th.cursor)]))
    }

    fn settings_term_group(&self, cx: &mut Context<Self>) -> Div {
        // 字体
        let me = cx.entity();
        let fonts = self.settings.mono_fonts.clone().unwrap_or_default();
        let choice = theme::font_choice();
        let mut items = vec![MenuItem::new(format!("默认({})", crate::ui::MONO))];
        items.extend(fonts.iter().map(|f| MenuItem::new(f.clone())));
        let selected = if choice.is_empty() { 0 } else { fonts.iter().position(|f| *f == choice).map_or(0, |i| i + 1) };
        let font_ctl = div().w(px(240.0)).child(select_sized(
            30.0,
            "set-font",
            IconName::Type,
            items,
            selected,
            self.settings.font_open,
            {
                let me = me.clone();
                move |_, app| {
                    me.update(app, |this, cx| {
                        this.settings.font_open = !this.settings.font_open;
                        this.load_mono_fonts(cx);
                        cx.notify();
                    })
                }
            },
            {
                let me = me.clone();
                move |i, _, app| {
                    let name = if i == 0 { String::new() } else { fonts.get(i - 1).cloned().unwrap_or_default() };
                    me.update(app, |this, cx| {
                        this.settings.font_open = false;
                        theme::set_font(&name);
                        this.save_pref(theme::KEY_FONT, &name);
                        this.apply_term_prefs(cx);
                    })
                }
            },
            move |_, app| {
                me.update(app, |this, cx| {
                    this.settings.font_open = false;
                    cx.notify();
                })
            },
            None,
        ));
        // 字号
        let size = theme::size();
        let step = |id: &'static str, ic: IconName, d: f32| {
            crate::pages::dockerpages::icon_btn((id, 0), ic, c::muted()).on_click(cx.listener(move |this, _, _, cx| {
                theme::set_size(theme::size() + d);
                this.save_pref(theme::KEY_SIZE, &theme::size().to_string());
                this.apply_term_prefs(cx);
            }))
        };
        let size_ctl = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(step("set-size-dec", IconName::Minus, -1.0))
            .child(div().w(px(36.0)).flex().justify_center().text_size(px(t::TYPE_BODY)).font_family(MONO).text_color(c::text()).child(format!("{}", size as i32)))
            .child(step("set-size-inc", IconName::Plus, 1.0));
        // 关键字高亮
        let hl = Self::pill_switch("set-highlight", highlight::enabled()).on_click(cx.listener(|this, _, _, cx| {
            let on = !highlight::enabled();
            highlight::set_enabled(on);
            this.save_pref(highlight::SETTING_KEY, if on { "on" } else { "off" });
            this.apply_term_prefs(cx);
        }));
        Self::setting_group(
            "终端",
            vec![
                Self::setting_row("字体", "只列出系统里的等宽字体;已打开的终端立即生效", font_ctl),
                Self::setting_row("字号", "10–24", size_ctl),
                Self::setting_row("关键字高亮", "给 IP、网址、error/warn 等词上色,只影响显示", hl),
                div().flex().flex_col().gap(px(10.0)).px(px(16.0)).py(px(12.0)).child(div().text_size(px(t::TYPE_BODY)).text_color(c::text()).child("主题")).child(self.term_theme_cards(cx)).child(Self::term_preview()),
            ],
        )
    }

    /// 页签:和容器页同一套(选中的有玻璃底)
    fn settings_tabs(&self, cx: &mut Context<Self>, sticky: bool) -> Div {
        let mut row = div().flex().flex_row().items_center().gap(px(4.0)).px(px(CONTENT_PAD)).pb(px(16.0)).flex_shrink_0();
        for (i, tab) in SettingsTab::ALL.into_iter().enumerate() {
            let active = self.settings.tab == tab;
            row = row.child(
                div()
                    .id((if sticky { "stab-sticky" } else { "stab" }, i))
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
                    .child(icon(tab.icon(), 14.0, if active { c::accent() } else { c::ghost() }))
                    .child(tab.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.tab = tab;
                        this.settings.keep_open = false;
                        cx.notify();
                    })),
            );
        }
        row
    }

    fn settings_general_group(&self, cx: &mut Context<Self>) -> Div {
        let remember = Self::pill_switch("set-win-remember", winstate::remember()).on_click(cx.listener(|this, _, _, cx| {
            let on = !winstate::remember();
            winstate::set_remember(on);
            this.save_pref(winstate::KEY_REMEMBER, if on { "on" } else { "off" });
            cx.notify();
        }));
        let clear = button("set-win-clear", IconName::Trash, "清除").on_click(cx.listener(|this, _, _, cx| {
            // 空串解析不出窗口位置,下次启动用默认大小并居中
            this.save_pref(winstate::KEY, "");
            this.win_saved = None;
            cx.notify();
        }));
        Self::setting_group(
            "窗口",
            vec![
                Self::setting_row("记住窗口位置和大小", "关闭后每次启动都用默认大小并居中", remember),
                Self::setting_row("清除窗口记忆", "删掉已保存的位置和大小,下次启动用默认值", clear),
            ],
        )
    }

    /// 数值下拉:`choices` 里选一个,`fmt` 生成显示文字,选中后调 `apply`(设置全局值 + 返回要保存的字符串)
    fn num_select(&self, cx: &mut Context<Self>, id: &'static str, icon: IconName, choices: &'static [u64], cur: u64, fmt: fn(u64) -> String, key: &'static str, apply: fn(u64)) -> Div {
        let me = cx.entity();
        let items: Vec<MenuItem> = choices.iter().map(|v| MenuItem::new(fmt(*v))).collect();
        let selected = choices.iter().position(|v| *v == cur).unwrap_or(0);
        let open = self.settings.open_sel == Some(id);
        div().w(px(160.0)).child(select_sized(
            30.0,
            id,
            icon,
            items,
            selected,
            open,
            {
                let me = me.clone();
                move |_, app| me.update(app, |this, cx| {
                    this.settings.open_sel = if this.settings.open_sel == Some(id) { None } else { Some(id) };
                    cx.notify();
                })
            },
            {
                let me = me.clone();
                move |i, _, app| {
                    let v = choices.get(i).copied().unwrap_or(cur);
                    me.update(app, |this, cx| {
                        this.settings.open_sel = None;
                        apply(v);
                        this.save_pref(key, &v.to_string());
                        this.apply_term_prefs(cx);
                        cx.notify();
                    })
                }
            },
            move |_, app| me.update(app, |this, cx| {
                this.settings.open_sel = None;
                cx.notify();
            }),
            None,
        ))
    }

    /// 开关:点一下取反、保存;`term` 为真时让已开的终端立即应用
    fn pref_switch(&self, cx: &mut Context<Self>, id: &'static str, get: fn() -> bool, set: fn(bool), key: &'static str) -> Stateful<Div> {
        Self::pill_switch(id, get()).on_click(cx.listener(move |this, _, _, cx| {
            let on = !get();
            set(on);
            this.save_pref(key, if on { "on" } else { "off" });
            this.apply_term_prefs(cx);
            cx.notify();
        }))
    }

    fn settings_term_behavior_group(&self, cx: &mut Context<Self>) -> Div {
        use crate::prefs as p;
        let rows_fmt = |v: u64| format!("{v} 行");
        let cursor_fmt = |v: u64| ["块", "竖线", "下划线"][v.min(2) as usize].to_string();
        let bell_fmt = |v: u64| ["无", "视觉闪烁", "系统提示音"][v.min(2) as usize].to_string();
        let sb = self.num_select(cx, "set-scrollback", IconName::ScrollText, &[1000, 5000, 10000, 50000, 100000], p::scrollback(), rows_fmt, p::KEY_SCROLLBACK, p::set_scrollback);
        // 触发框前面的图标跟着当前选择走:块 / 竖线 / 下划线
        let cs_icon = match p::cursor_style() {
            1 => IconName::TextCursor,
            2 => IconName::Underline,
            _ => IconName::RectangleVertical,
        };
        let cs = self.num_select(cx, "set-cursor", cs_icon, &[0, 1, 2], p::cursor_style(), cursor_fmt, p::KEY_CURSOR_STYLE, p::set_cursor_style);
        let bell = self.num_select(cx, "set-bell", IconName::Bell, &[0, 1, 2], p::bell(), bell_fmt, p::KEY_BELL, p::set_bell);
        let blink = self.pref_switch(cx, "set-blink", p::cursor_blink, p::set_cursor_blink, p::KEY_CURSOR_BLINK);
        let cos = self.pref_switch(cx, "set-cos", p::copy_on_select, p::set_copy_on_select, p::KEY_COPY_ON_SELECT);
        let rcp = self.pref_switch(cx, "set-rcp", p::right_click_paste, p::set_right_click_paste, p::KEY_RIGHT_PASTE);
        let cct = self.pref_switch(cx, "set-cct", p::confirm_close_tab, p::set_confirm_close_tab, p::KEY_CONFIRM_TAB);
        Self::setting_group(
            "终端行为",
            vec![
                Self::setting_row("回滚缓冲", "终端能往回翻的行数,越大越占内存;只对新打开的终端生效", sb),
                Self::setting_row("光标样式", "块、竖线或下划线", cs),
                Self::setting_row("光标闪烁", "", blink),
                Self::setting_row("选中即复制", "松开鼠标时自动把选中的文字复制到剪贴板", cos),
                Self::setting_row("右键粘贴", "在终端里点右键直接粘贴剪贴板内容", rcp),
                Self::setting_row("关闭标签前确认", "终端连接还开着时,关闭标签先确认", cct),
                Self::setting_row("响铃", "程序输出响铃字符(BEL)时的提示方式", bell),
            ],
        )
    }

    fn settings_start_group(&self, cx: &mut Context<Self>) -> Div {
        use crate::prefs as p;
        let mode_fmt = |v: u64| crate::workspace::NodeMode::ALL[v.min(2) as usize].label().to_string();
        let secs = |v: u64| format!("{v} 秒");
        let rt = self.pref_switch(cx, "set-rt", p::restore_tabs, p::set_restore_tabs, p::KEY_RESTORE_TABS);
        let cq = self.pref_switch(cx, "set-cq", p::confirm_quit, p::set_confirm_quit, p::KEY_CONFIRM_QUIT);
        let dm = self.num_select(cx, "set-dmode", IconName::SquareTerminal, &[0, 1, 2], p::default_mode(), mode_fmt, p::KEY_DEFAULT_MODE, p::set_default_mode);
        let pr = self.num_select(cx, "set-proc", IconName::RefreshCw, &[1, 3, 5, 10], p::proc_secs(), secs, p::KEY_PROC_SECS, p::set_proc_secs);
        let pg = self.num_select(cx, "set-page", IconName::RefreshCw, &[5, 10, 30, 60], p::page_secs(), secs, p::KEY_PAGE_SECS, p::set_page_secs);
        let cl = self.num_select(cx, "set-ctlog", IconName::RefreshCw, &[1, 2, 5, 10], p::ctlog_secs(), secs, p::KEY_CTLOG_SECS, p::set_ctlog_secs);
        div()
            .flex()
            .flex_col()
            .gap(px(20.0))
            .child(Self::setting_group(
                "启动与退出",
                vec![
                    Self::setting_row("启动时恢复上次的标签", "重新打开退出时开着的节点标签(含各自的模式和活动标签)", rt),
                    Self::setting_row("新开节点默认进入", "从节点页打开一个还没开过的节点时,先进哪个模式", dm),
                    Self::setting_row("退出前确认", "还有终端连接开着时,关闭窗口前先确认", cq),
                ],
            ))
            .child(Self::setting_group(
                "刷新频率",
                vec![
                    Self::setting_row("进程列表", "进程页自动刷新的间隔", pr),
                    Self::setting_row("其他页面", "容器、镜像、存储卷、服务、存储、计划任务、网站、数据库自动刷新的间隔;窗口在后台时不刷新,回到前台立刻刷新一次", pg),
                    Self::setting_row("容器日志", "容器日志面板自动刷新的间隔", cl),
                ],
            ))
    }

    fn settings_conn_group(&self, cx: &mut Context<Self>) -> Div {
        use crate::terminal::ssh;
        let secs = |v: u64| format!("{v} 秒");
        let keep = self.num_select(cx, "set-ssh-keepalive", IconName::Activity, &KEEPALIVE_CHOICES, ssh::keepalive_secs(), secs, ssh::KEY_KEEPALIVE, ssh::set_keepalive_secs);
        let timeout = self.num_select(cx, "set-ssh-timeout", IconName::Clock, &TIMEOUT_CHOICES, ssh::connect_secs(), secs, ssh::KEY_CONNECT_TIMEOUT, ssh::set_connect_secs);
        let auto = Self::pill_switch("set-ssh-auto", ssh::auto_reconnect()).on_click(cx.listener(|this, _, _, cx| {
            let on = !crate::terminal::ssh::auto_reconnect();
            crate::terminal::ssh::set_auto_reconnect(on);
            this.save_pref(crate::terminal::ssh::KEY_AUTO_RECONNECT, if on { "on" } else { "off" });
            cx.notify();
        }));
        Self::setting_group(
            "SSH 连接",
            vec![
                Self::setting_row("保活间隔", "每隔这么久向服务器发一次探测,防止空闲时被路由或防火墙断开;对新连接生效", keep),
                Self::setting_row("连接超时", "建立连接超过这个时间就放弃;对新连接生效", timeout),
                Self::setting_row("断线自动重连", "连接意外中断后自动重试最多 5 次(只对保存了认证信息的节点)", auto),
            ],
        )
    }

    fn settings_takeover_group(&self, cx: &mut Context<Self>) -> Div {
        use crate::takeover as tk;
        let min = |v: u64| if v % 60 == 0 { format!("{} 小时", v / 60) } else { format!("{v} 分钟") };
        let idle = self.num_select(cx, "set-tk-idle", IconName::Clock, &IDLE_CHOICES, tk::idle_min(), min, tk::KEY_IDLE_MIN, tk::set_idle_min);
        let max = self.num_select(cx, "set-tk-max", IconName::Timer, &MAX_CHOICES, tk::max_min(), min, tk::KEY_MAX_MIN, tk::set_max_min);
        let sw = self.pref_switch(cx, "set-sudo-win", crate::prefs::sudo_allow_window, crate::prefs::set_sudo_allow_window, crate::prefs::KEY_SUDO_WINDOW);
        Self::setting_group(
            "AI 接管",
            vec![
                Self::setting_row("提权可选「一段时间内免确认」", "关闭后 agent 每次 sudo 都要你确认,确认框里只有「仅这一次」", sw),
                Self::setting_row("空闲超时", "agent 这么久没有任何操作,自动结束接管,终端交还给你(等待输出期间不算)", idle),
                Self::setting_row("单次最长时长", "一次接管最多持续这么久,到点自动结束接管,终端交还给你", max),
            ],
        )
    }

    fn settings_logs_group(&self, cx: &mut Context<Self>) -> Div {
        let me = cx.entity();
        let items: Vec<MenuItem> = KEEP_CHOICES.iter().map(|d| MenuItem::new(format!("{d} 天"))).collect();
        let cur = crate::logs::keep_days();
        let selected = KEEP_CHOICES.iter().position(|d| *d == cur).unwrap_or(1);
        let keep = div().w(px(160.0)).child(select_sized(
            30.0,
            "set-log-keep",
            IconName::ScrollText,
            items,
            selected,
            self.settings.keep_open,
            {
                let me = me.clone();
                move |_, app| me.update(app, |this, cx| {
                    this.settings.keep_open = !this.settings.keep_open;
                    cx.notify();
                })
            },
            {
                let me = me.clone();
                move |i, _, app| {
                    let d = KEEP_CHOICES.get(i).copied().unwrap_or(30);
                    me.update(app, |this, cx| {
                        this.settings.keep_open = false;
                        crate::logs::set_keep_days(d);
                        this.save_pref(crate::logs::KEY_KEEP_DAYS, &d.to_string());
                        // 缩短保留时间后马上清掉过期的,不等到下次启动
                        if let Some(st) = &this.store {
                            let _ = st.log_purge(chrono::Local::now().timestamp_millis() - d * 24 * 3600 * 1000, crate::store::LOG_KEEP_ROWS);
                        }
                        cx.notify();
                    })
                }
            },
            move |_, app| me.update(app, |this, cx| {
                this.settings.keep_open = false;
                cx.notify();
            }),
            None,
        ));
        let verbose = Self::pill_switch("set-log-verbose", self.logs_verbose).on_click(cx.listener(|this, _, _, cx| {
            this.logs_verbose = !this.logs_verbose;
            this.save_pref(KEY_LOGS_VERBOSE, if this.logs_verbose { "on" } else { "off" });
            cx.notify();
        }));
        Self::setting_group(
            "日志",
            vec![
                Self::setting_row("保留时间", "超过这个时间的日志会被自动清掉(启动时和每天一次)", keep),
                Self::setting_row("显示详情", "日志页里显示连接的分步细节,默认只显示汇总", verbose),
            ],
        )
    }

    fn settings_about_group(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(20.0))
            .child(self.settings_about_info(cx))
            .child(self.settings_update_group(cx))
    }

    fn settings_about_info(&self, cx: &mut Context<Self>) -> Div {
        let version = div().text_size(px(t::TYPE_BODY)).font_family(MONO).text_color(c::text()).child(crate::update::CURRENT);
        let home = button("set-about-home", IconName::ExternalLink, "打开").on_click(cx.listener(|_, _, _, _| crate::update::open_page(&format!("https://github.com/{}", crate::update::REPO))));
        let data = crate::store::data_dir();
        let data_txt: SharedString = data.as_ref().map_or("未知".to_string(), |d| d.display().to_string()).into();
        let reveal = button("set-about-data", IconName::FolderOpen, "打开").on_click(cx.listener(move |_, _, _, _| {
            if let Some(d) = crate::store::data_dir() {
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("open").arg(d).spawn();
                #[cfg(windows)]
                let _ = std::process::Command::new("explorer").arg(d).spawn();
                #[cfg(all(unix, not(target_os = "macos")))]
                let _ = std::process::Command::new("xdg-open").arg(d).spawn();
            }
        }));
        let row = |label: &'static str, desc: SharedString, control: AnyElement| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(16.0))
                .min_h(px(54.0))
                .px(px(16.0))
                .py(px(10.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(div().text_size(px(t::TYPE_BODY)).text_color(c::text()).child(label))
                        .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(desc)),
                )
                .child(div().flex_shrink_0().child(control))
        };
        // 顶部:logo + 名称 + 版本
        let head = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.0))
            .py(px(8.0))
            .child(gpui::img("logo/otter.png").size(px(72.0)).flex_shrink_0())
            .child(div().mt(px(6.0)).text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).text_color(c::text()).child("Otter"))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(format!("版本 {}", crate::update::CURRENT)));
        let group = Self::setting_group(
            "关于 Otter",
            vec![
                Self::setting_row("版本", "经 SSH 管理多台服务器的桌面客户端,服务器上不装任何东西", version),
                Self::setting_row("开源协议", "", div().text_size(px(t::TYPE_BODY)).text_color(c::text()).child("MIT")),
                Self::setting_row("项目主页", "在浏览器里打开 GitHub 仓库", home),
                row("数据目录", data_txt, reveal.into_any_element()),
            ],
        );
        div().flex().flex_col().gap(px(16.0)).child(head).child(group)
    }

    fn settings_update_group(&self, cx: &mut Context<Self>) -> Div {
        let checking = self.settings.checking;
        let check = button("set-update-check", IconName::RefreshCw, if checking { "检查中…" } else { "检查更新" }).on_click(cx.listener(|this, _, _, cx| this.check_update_now(cx)));
        let auto = Self::pill_switch("set-update-auto", crate::update_ui::auto_check()).on_click(cx.listener(|this, _, _, cx| {
            let on = !crate::update_ui::auto_check();
            crate::update_ui::set_auto_check(on);
            this.save_pref(crate::update_ui::KEY_AUTO, if on { "on" } else { "off" });
            cx.notify();
        }));
        let status: SharedString = match (&self.settings.update_msg, &self.update) {
            (Some(m), _) => m.clone().into(),
            (None, crate::update::State::Available(u)) => format!("发现新版本 {},点标题栏的更新按钮安装", u.version).into(),
            _ => "手动查一次是否有新版本".into(),
        };
        Self::setting_group(
            "更新",
            vec![
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(16.0))
                    .min_h(px(54.0))
                    .px(px(16.0))
                    .py(px(10.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(div().text_size(px(t::TYPE_BODY)).text_color(c::text()).child("检查更新"))
                            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(status)),
                    )
                    .child(div().flex_shrink_0().child(check)),
                Self::setting_row("自动检查更新", "启动后和每 6 小时检查一次;发现新版本时标题栏出现更新按钮", auto),
            ],
        )
    }

    pub(crate) fn settings_page(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let content = match self.settings.tab {
            SettingsTab::Terminal => div().flex().flex_col().gap(px(20.0)).child(self.settings_term_group(cx)).child(self.settings_term_behavior_group(cx)),
            SettingsTab::General => div().flex().flex_col().gap(px(20.0)).child(self.settings_start_group(cx)).child(self.settings_general_group(cx)).child(self.settings_conn_group(cx)).child(self.settings_takeover_group(cx)).child(self.settings_logs_group(cx)),
            SettingsTab::About => self.settings_about_group(cx),
        };
        // 标题在上、页签在下,一起放在滚动区里;标题滚出去后,页签吸附在顶部(同一排页签再画一份盖在上面)
        let title = div()
            .h(px(SETTINGS_TITLE_H))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .pt(px(20.0))
            .child(div().text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child("设置"))
            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child("终端、通用和关于"));
        // 吸顶条的底:把窗口底(底色 + 三团光晕)按窗口坐标原样再画一份,和周围严丝合缝
        let vp = window.viewport_size();
        let sticky_bg = div().absolute().left(px(-NODES_SIDE_W)).top(px(-crate::ui::TITLEBAR_H)).w(vp.width).h(vp.height).child(backdrop());
        let scrolled = (-f32::from(self.settings.scroll.offset().y)).min(f32::from(self.settings.scroll.max_offset().y));
        let stuck = scrolled > SETTINGS_TITLE_H;
        let scroll = div()
            .id("settings-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.settings.scroll)
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
            .pl(px(CONTENT_PAD))
            .pr(px(CONTENT_PAD))
            .pb(px(CONTENT_PAD))
            .flex()
            .flex_col()
            .child(title)
            .child(self.settings_tabs(cx, false).px(px(0.0)))
            .child(content);
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(scroll)
            .when(stuck, |d| d.child(div().absolute().top_0().left_0().right_0().pt(px(12.0)).overflow_hidden().bg(c::bg()).child(sticky_bg).child(self.settings_tabs(cx, true))))
    }
}
