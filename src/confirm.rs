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
}

/// 删除确认框宽度(比表单弹窗窄:内容少,居中排版)
pub(crate) const CONFIRM_W: f32 = 400.0;


/// 危险色的确认按钮(撑满剩余宽度,和取消按钮等宽)
fn danger_btn(id: &'static str, label: &'static str) -> Stateful<Div> {
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
        .bg(c::danger())
        .text_color(c::on_accent())
        .text_size(px(t::TYPE_CONTROL))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|s| s.opacity(0.88))
        .child(icon(IconName::Trash, 13.0, c::on_accent()))
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
        let hero = alert_hero(IconName::Trash, c::danger(), spec.title.to_string(), "此操作无法撤销");
        let target = alert_target(spec.tile, spec.name, spec.detail);
        let rows = alert_rows(spec.points.into_iter().map(|(ic, color, text)| (ic, color, text.to_string())).collect());
        let cancel = crate::node_form::ctl_btn(spec.ids[0], None, "取消", 38.0).flex_1().justify_center().on_click(cx.listener(move |this, _, _, cx| {
            cancel_fn(this);
            cx.notify();
        }));
        let ok = danger_btn(spec.ids[1], spec.ok_label).on_click(cx.listener(move |this, _, _, cx| ok_fn(this, cx)));
        let buttons = div().w_full().flex().flex_row().gap(px(10.0)).child(cancel).child(ok);
        vec![alert_body(hero, Some(target), rows, buttons).into_any_element()]
    }

}
