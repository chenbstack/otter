//! 通用界面基础:色板、尺寸常量、小部件。标准值见 docs/design/tokens.md。

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::mock::SPARK_N;

// ---- 色板(来自 theme.ts,略微拉开层次)---------------------------------------
pub mod c {
    use gpui_kit::{Rgba, rgb, rgba};
    pub fn bg() -> Rgba { rgb(0x141416) }
    pub fn chrome() -> Rgba { rgb(0x101012) }
    pub fn raised() -> Rgba { rgb(0x1A1A1D) }
    pub fn raised_hi() -> Rgba { rgb(0x1F1F23) }
    pub fn sunken() -> Rgba { rgb(0x0C0C0E) }
    pub fn selected() -> Rgba { rgb(0x26262B) }
    pub fn hover() -> Rgba { rgba(0xFFFFFF0A) }
    pub fn border() -> Rgba { rgb(0x222226) }
    pub fn border_strong() -> Rgba { rgb(0x34343B) }
    pub fn text() -> Rgba { rgb(0xECECEE) }
    pub fn secondary() -> Rgba { rgb(0xC6C6CA) }
    pub fn muted() -> Rgba { rgb(0x8A8A90) }
    pub fn ghost() -> Rgba { rgb(0x5A5A63) }
    pub fn online() -> Rgba { rgb(0x4ce3a0) }
    pub fn warn() -> Rgba { rgb(0xe3b34c) }
    pub fn danger() -> Rgba { rgb(0xe35c5c) }
    pub fn online_soft() -> Rgba { rgba(0x4ce3a01F) }
    pub fn warn_soft() -> Rgba { rgba(0xe3b34c1F) }
    pub fn idle_soft() -> Rgba { rgba(0xFFFFFF0F) }
    pub fn cpu() -> Rgba { rgb(0x7f89f0) }
    pub fn mem() -> Rgba { rgb(0xb18cf2) }
    pub fn net() -> Rgba { rgb(0x3fd0c9) }
    pub fn disk() -> Rgba { rgb(0xe8964c) }
}

pub const MONO: &str = "Menlo";

// ---- 尺寸 --------------------------------------------------------------------
pub const SIDEBAR_W: f32 = 260.0;
pub const MENU_W: f32 = 200.0;
pub const TITLEBAR_H: f32 = 46.0;
pub const CONTENT_PAD: f32 = 20.0;

pub const STACK_HEADER_H: f32 = 50.0;
pub const STACK_BODY_PAD: f32 = 14.0;
pub const STACK_GAP: f32 = 10.0;
pub const SVC_GAP: f32 = 12.0;
pub const SVC_H_CHART: f32 = 180.0;
pub const SVC_H_PLAIN: f32 = 154.0;
/// 服务卡片最小宽度:放得下几张就分几列,卡片再均分剩余宽度(宽度落在 [MIN, 2*MIN+GAP) 内)。
pub const SVC_MIN_W: f32 = 300.0;
/// 服务卡片底栏右侧操作按钮总宽(终端/日志/打开/分隔/停止/重启/删除)
pub const ACTIONS_W: f32 = 258.0;
/// 10.5px Menlo 单字符宽度,以及 "+N" 标签宽度
pub const PORT_CHAR_W: f32 = 6.4;
pub const PORT_MORE_W: f32 = 34.0;

// ---- 小部件 ------------------------------------------------------------------
pub fn icon(name: IconName, size: f32, color: Rgba) -> Icon {
    Icon::new(name).size(px(size)).text_color(color)
}

/// 状态药丸:带圆点的软底色标签。
pub fn pill(label: impl Into<SharedString>, fg: Rgba, bg: Rgba) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(22.0))
        .px(px(9.0))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_size(px(11.0))
        .font_weight(FontWeight::MEDIUM)
        .child(div().size(px(6.0)).rounded_full().bg(fg))
        .child(label.into())
}

/// 中性小标签(节点名、端口等)。
pub fn tag(label: impl Into<SharedString>) -> Div {
    div()
        .flex_shrink_0()
        .px(px(7.0))
        .py(px(2.0))
        .rounded(px(5.0))
        .bg(c::idle_soft())
        .text_size(px(11.0))
        .text_color(c::secondary())
        .child(label.into())
}

/// 状态圆点 + 一圈柔光。
pub fn status_dot(color: Rgba, on: bool) -> Div {
    div()
        .size(px(14.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .when(on, |d| d.bg(color.opacity(0.16)))
        .child(div().size(px(6.0)).rounded_full().bg(color))
}

/// 迷你柱状图:越旧越淡,最新一根满色,读起来有"时间往右走"的方向感。
pub fn spark(values: &[f32; SPARK_N], color: Rgba, h: f32, gap: f32) -> Div {
    let n = values.len() as f32;
    div()
        .flex()
        .flex_row()
        .items_end()
        .gap(px(gap))
        .h(px(h))
        .children(values.iter().enumerate().map(move |(i, v)| {
            let alpha = 0.45 + 0.55 * (i as f32 + 1.0) / n;
            div()
                .flex_1()
                .h(px((h * *v).max(2.0)))
                .rounded_t(px(1.5))
                .bg(color.opacity(alpha))
        }))
}

pub fn latency_color(ms: u32) -> Rgba {
    if ms >= 2000 { c::warn() } else { c::muted() }
}

pub fn empty_state(ic: IconName, title: &'static str, sub: &'static str) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(10.0))
        .pb(px(80.0))
        .child(
            div()
                .size(px(52.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(14.0))
                .border_1()
                .border_color(c::border())
                .bg(c::raised())
                .child(icon(ic, 22.0, c::ghost())),
        )
        .child(div().mt(px(4.0)).text_size(px(14.0)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child(title))
        .child(div().text_size(px(12.0)).text_color(c::ghost()).child(sub))
}

pub fn button(id: &'static str, ic: IconName, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(30.0))
        .px(px(11.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(c::border_strong())
        .bg(c::raised())
        .text_size(px(12.0))
        .text_color(c::secondary())
        .cursor_pointer()
        .hover(|s| s.bg(c::raised_hi()).text_color(c::text()))
        .child(icon(ic, 13.0, c::muted()))
        .child(label)
}
