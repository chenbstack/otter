//! 确认框组件(删除节点、分组、密钥、Docker 对象共用):红色警示 + 对象卡片 + 后果清单 + 取消/确认。
//! 和提示框(`ui::alert_*`,接管确认、连接已断开)是两个组件:确认框只管「要不要做这件破坏性操作」,固定的版式和按钮,调用方只给内容。

use super::*;
use crate::ui::modal;

/// 删除确认框的内容(见 `confirm_parts`)
pub(crate) struct Confirm {
    /// 取消、确认两个按钮的元素 id
    pub ids: [&'static str; 2],
    pub title: &'static str,
    /// 标题行左边的图标块(对象自己的图标,32)
    pub tile: AnyElement,
    pub name: SharedString,
    pub detail: SharedString,
    /// 后果清单,一条一行:(图标, 图标颜色, 文字)。会丢的东西用红色,不受影响的用绿色
    pub points: Vec<(IconName, Rgba, SharedString)>,
    pub ok_label: &'static str,
    /// 危险操作(红色、垃圾桶、「无法撤销」)还是普通操作(强调色、对勾、无副标题警告)
    pub kind: ConfirmKind,
    /// 清单下面的额外内容(比如一个勾选项);没有就 None
    pub extra: Option<AnyElement>,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ConfirmKind {
    Danger,
    /// 危险操作,但确认按钮暂时不可点(倒数中):样式同 `Danger`,按钮变淡
    DangerWait,
    Action,
}

/// 删除确认框宽度(比表单弹窗窄:内容少,居中排版)
pub(crate) const CONFIRM_W: f32 = 400.0;
/// 确认框面板宽度(标题行左对齐、文件名等详情要一行放下,比提示框宽一点)
const CONFIRM_PANEL_W: f32 = 460.0;


/// 危险色的确认按钮(撑满剩余宽度,和取消按钮等宽)
fn danger_btn(id: &'static str, label: &'static str, kind: ConfirmKind) -> Stateful<Div> {
    let wait = kind == ConfirmKind::DangerWait;
    let (bg, ic) = if kind != ConfirmKind::Action { (c::danger(), IconName::Trash) } else { (c::accent(), IconName::Check) };
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .h(px(32.0))
        .px(px(16.0))
        .rounded(px(t::RADIUS_MD))
        .bg(bg)
        .text_color(c::on_accent())
        .text_size(px(t::TYPE_CONTROL))
        .font_weight(FontWeight::SEMIBOLD)
        .when(wait, |d| d.opacity(0.4))
        .when(!wait, |d| d.cursor_pointer().hover(|s| s.opacity(0.88)))
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
        modal(
            dialog_id,
            CONFIRM_PANEL_W,
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

    /// 确认框的内容(节点、分组、密钥、Docker、数据库共用),版式和备份弹窗、连接信息弹窗一致:
    /// 左对齐标题行(对象图标块 + 标题 + 副标题;危险操作标题带对象名、副标题红字「此操作无法撤销」)→ 后果清单(一条一个描边小框)→ 额外内容 → 右对齐的取消 / 确认按钮。没有单独的对象卡片。
    pub(crate) fn confirm_parts(
        &self,
        spec: Confirm,
        cancel_fn: impl Fn(&mut Self) + Copy + 'static,
        ok_fn: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let danger = spec.kind != ConfirmKind::Action;
        let title: SharedString = if danger { format!("{} · {}", spec.title, spec.name).into() } else { spec.title.into() };
        // 副标题:危险操作是红字「此操作无法撤销」+ 详情;普通操作只有详情(没有就写对象名)
        let detail: SharedString = if !spec.detail.is_empty() { spec.detail } else if danger { "".into() } else { spec.name };
        let head = div()
            .mb(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(spec.tile)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::BOLD).text_color(c::text()).child(title))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(6.0))
                            .text_size(px(t::TYPE_CAPTION))
                            .children(danger.then(|| div().flex_none().text_color(c::danger()).child("此操作无法撤销")))
                            .children((!detail.is_empty()).then(|| div().min_w_0().truncate().text_color(c::ghost()).child(if danger { format!("· {detail}") } else { detail.to_string() }))),
                    ),
            );
        let rows = div().w_full().flex().flex_col().gap(px(6.0)).children(spec.points.into_iter().map(|(ic, color, text)| {
            div()
                .min_h(px(40.0))
                .px(px(12.0))
                .py(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .rounded(px(t::RADIUS_MD))
                .bg(c::field())
                .border_1()
                .border_color(c::field_border())
                .child(icon(ic, 14.0, color))
                .child(div().flex_1().min_w_0().line_height(relative(1.5)).text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(text))
        }));
        let cancel = crate::node_form::ctl_btn(spec.ids[0], None, "取消", 32.0).on_click(cx.listener(move |this, _, _, cx| {
            cancel_fn(this);
            cx.notify();
        }));
        let ok = danger_btn(spec.ids[1], spec.ok_label, spec.kind).on_click(cx.listener(move |this, _, _, cx| ok_fn(this, cx)));
        let buttons = div().w_full().flex().flex_row().justify_end().gap(px(10.0)).child(cancel).child(ok);
        vec![div().flex().flex_col().gap(px(14.0)).px(px(20.0)).pt(px(20.0)).pb(px(18.0)).child(head).child(rows).children(spec.extra).child(buttons).into_any_element()]
    }
}
