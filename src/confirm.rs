//! 确认框组件(删除节点、分组、密钥、Docker 对象共用):红色警示 + 对象卡片 + 后果清单 + 取消/确认。
//! 和提示框(`ui::alert_*`,接管确认、连接已断开)是两个组件:确认框只管「要不要做这件破坏性操作」,固定的版式和按钮,调用方只给内容。

use super::*;
use crate::ui::{alert_body, alert_hero, alert_rows, alert_target, modal_glass};

/// 删除确认框的内容(见 `confirm_parts`)
pub(crate) struct Confirm {
    /// 取消、确认两个按钮的元素 id
    pub ids: [&'static str; 2],
    pub title: &'static str,
    /// 对象卡片左边的图标块
    pub tile: AnyElement,
    pub name: SharedString,
    pub detail: SharedString,
    /// 后果清单,一条一行:(图标, 图标颜色, 文字)。会丢的东西用红色,不受影响的用绿色
    pub points: Vec<(IconName, Rgba, SharedString)>,
    pub ok_label: &'static str,
    /// 紧凑模式:图标和标题同一行、不显示对象卡片、清单行更矮,用于清单较长的确认框
    pub compact: bool,
    /// 危险操作(红色、垃圾桶、「无法撤销」)还是普通操作(强调色、对勾、无副标题警告)
    pub kind: ConfirmKind,
    /// 清单下面的额外内容(比如一个勾选项);没有就 None
    pub extra: Option<AnyElement>,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ConfirmKind {
    Danger,
    Action,
}

/// 删除确认框宽度(比表单弹窗窄:内容少,居中排版)
pub(crate) const CONFIRM_W: f32 = 400.0;


/// 危险色的确认按钮(撑满剩余宽度,和取消按钮等宽)
fn danger_btn(id: &'static str, label: &'static str, kind: ConfirmKind) -> Stateful<Div> {
    let (bg, ic) = if kind == ConfirmKind::Danger { (c::danger(), IconName::Trash) } else { (c::accent(), IconName::Check) };
    div()
        .id(id)
        .flex_1()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .h(px(38.0))
        .rounded(px(t::RADIUS_MD))
        .bg(bg)
        .text_color(c::on_accent())
        .text_size(px(t::TYPE_CONTROL))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|s| s.opacity(0.88))
        .child(icon(ic, 13.0, c::on_accent()))
        .child(label)
}

impl Home {
    /// 删除确认框:包一层弹窗外壳,Esc 取消。内容见 `confirm_parts`。
    pub(crate) fn delete_confirm(
        &self,
        dialog_id: &'static str,
        spec: Confirm,
        cancel_fn: impl Fn(&mut Self) + Copy + 'static,
        ok_fn: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let parts = self.confirm_parts(spec, cancel_fn, ok_fn, cx);
        modal_glass(
            dialog_id,
            CONFIRM_W,
            cx.listener(move |this, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" {
                    cancel_fn(this);
                    cx.notify();
                    cx.stop_propagation();
                }
            }),
            parts,
        )
    }

    /// 删除确认框的内容(节点、分组、密钥共用),居中排版,自上而下:
    /// 红色图标(带一圈淡光晕) → 标题 + “此操作无法撤销” → 对象卡片(和节点页的节点卡片同款) → 后果清单(一行一项,图标区分“会丢的”和“不受影响的”) → 取消 / 删除两个等宽按钮。
    /// 不再单独做底栏:按钮直接放在内容里,整体更紧凑。
    pub(crate) fn confirm_parts(
        &self,
        spec: Confirm,
        cancel_fn: impl Fn(&mut Self) + Copy + 'static,
        ok_fn: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if spec.compact {
            return self.confirm_compact(spec, cancel_fn, ok_fn, cx);
        }
        let (hic, tone, sub) = if spec.kind == ConfirmKind::Danger { (IconName::Trash, c::danger(), "此操作无法撤销") } else { (IconName::Check, c::accent(), "") };
        let hero = alert_hero(hic, tone, spec.title.to_string(), sub);
        let target = alert_target(spec.tile, spec.name, spec.detail);
        let rows = alert_rows(spec.points.into_iter().map(|(ic, color, text)| (ic, color, text.to_string())).collect());
        let cancel = crate::node_form::ctl_btn(spec.ids[0], None, "取消", 38.0).flex_1().justify_center().on_click(cx.listener(move |this, _, _, cx| {
            cancel_fn(this);
            cx.notify();
        }));
        let ok = danger_btn(spec.ids[1], spec.ok_label, spec.kind).on_click(cx.listener(move |this, _, _, cx| ok_fn(this, cx)));
        let buttons = div().w_full().flex().flex_row().gap(px(10.0)).child(cancel).child(ok);
        vec![alert_body(hero, Some(target), rows, buttons).into_any_element()]
    }

    /// 紧凑版:左上小图标 + 标题/副标题一行,下面直接是清单和按钮,不再显示对象卡片(标题已经说明对象)。
    fn confirm_compact(
        &self,
        spec: Confirm,
        cancel_fn: impl Fn(&mut Self) + Copy + 'static,
        ok_fn: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let danger = spec.kind == ConfirmKind::Danger;
        let (tone, hic, sub, gap) = if danger { (c::danger(), IconName::Trash, "此操作无法撤销", 12.0) } else { (c::accent(), IconName::Download, "", 16.0) };
        let head = div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .size(px(40.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(t::RADIUS_LG))
                    .bg(tone.opacity(0.16))
                    .border_1()
                    .border_color(tone.opacity(0.36))
                    .child(icon(hic, 18.0, tone)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(div().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).text_color(c::text()).child(SharedString::from(format!("{} · {}", spec.title, spec.name))))
                    .children((!sub.is_empty()).then(|| div().text_size(px(t::TYPE_MICRO)).text_color(c::muted()).child(sub))),
            );
        let n = spec.points.len();
        let rows = div().w_full().flex().flex_col().rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).bg(c::field()).children(spec.points.into_iter().enumerate().map(|(i, (ic, color, text))| {
            div()
                .min_h(px(if danger { 28.0 } else { 40.0 }))
                .px(px(if danger { 10.0 } else { 14.0 }))
                .py(px(if danger { 4.0 } else { 10.0 }))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(if danger { 8.0 } else { 10.0 }))
                .when(i + 1 < n, |d| d.border_b_1().border_color(c::line()))
                .child(icon(ic, 13.0, color))
                .child(div().flex_1().min_w_0().when(!danger, |d| d.line_height(relative(1.5))).text_size(px(t::TYPE_MICRO + 1.0)).text_color(c::secondary()).child(text))
        }));
        let cancel = crate::node_form::ctl_btn(spec.ids[0], None, "取消", 32.0).flex_1().justify_center().on_click(cx.listener(move |this, _, _, cx| {
            cancel_fn(this);
            cx.notify();
        }));
        let ok = danger_btn(spec.ids[1], spec.ok_label, spec.kind).h(px(32.0)).on_click(cx.listener(move |this, _, _, cx| ok_fn(this, cx)));
        let buttons = div().w_full().flex().flex_row().gap(px(10.0)).child(cancel).child(ok);
        vec![div().flex().flex_col().gap(px(gap)).px(px(20.0)).pt(px(20.0)).pb(px(20.0)).child(head).child(rows).children(spec.extra).child(buttons).into_any_element()]
    }
}
