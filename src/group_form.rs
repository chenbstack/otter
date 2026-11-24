//! 新建/编辑分组弹窗:分组名 + 一行颜色。「新建节点」旁下拉里的「新建分组」和分组卡片右键菜单的「编辑」打开。
//! 名称不能为空、不能和已有分组重名(忽略大小写);颜色从 `GROUP_COLORS` 里选。

use gpui_kit::assets::IconName;
use gpui_kit::component::input::InputState;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::mock::Group;
use crate::node_form::{ctl_btn, input_cell};
use crate::ui::{GROUP_COLORS, field, modal};
use crate::{Home, c, icon, t};

pub struct GroupForm {
    name: Entity<InputState>,
    color: u32,
    /// 正在编辑的分组下标;`None` 是新建
    editing: Option<usize>,
    error: Option<SharedString>,
}

fn val(s: &Entity<InputState>, cx: &App) -> String {
    s.read(cx).value().trim().to_string()
}

impl Home {
    pub(crate) fn open_add_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("分组名称"));
        cx.observe(&name, |_, _, cx| cx.notify()).detach();
        let focus = name.focus_handle(cx);
        window.focus(&focus, cx);
        // 默认灰色(group.slate),和"未分组"一致,用户想要颜色再选
        let color = GROUP_COLORS[2];
        self.group_form = Some(GroupForm { name, color, editing: None, error: None });
        cx.notify();
    }

    /// 编辑分组:改名和换颜色
    pub(crate) fn open_edit_group(&mut self, gi: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(g) = self.groups.get(gi) else { return };
        let (old, color) = (g.name.clone(), g.color);
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("分组名称"));
        name.update(cx, |s, cx| s.set_value(old, window, cx));
        cx.observe(&name, |_, _, cx| cx.notify()).detach();
        let focus = name.focus_handle(cx);
        window.focus(&focus, cx);
        self.group_form = Some(GroupForm { name, color, editing: Some(gi), error: None });
        cx.notify();
    }

    fn close_group_form(&mut self, cx: &mut Context<Self>) {
        self.group_form = None;
        cx.notify();
    }

    fn submit_group_form(&mut self, cx: &mut Context<Self>) {
        let Some(f) = &self.group_form else { return };
        let (name, color, editing) = (val(&f.name, cx), f.color, f.editing);
        let err = if name.is_empty() {
            Some("请输入分组名称")
        } else if self.groups.iter().enumerate().any(|(i, g)| Some(i) != editing && g.name.to_lowercase() == name.to_lowercase()) {
            Some("已经有同名的分组")
        } else {
            None
        };
        if let Some(e) = err {
            if let Some(f) = self.group_form.as_mut() {
                f.error = Some(e.into());
            }
            cx.notify();
            return;
        }
        if let Some(gi) = editing {
            let Some(gid) = self.groups.get(gi).map(|g| g.id) else { return };
            if let Some(Err(e)) = self.store.as_mut().map(|s| s.update_group(gid, &name, color)) {
                crate::logs::push(crate::logs::Level::Error, "本地库", &format!("编辑分组失败: {e}"));
                if let Some(f) = self.group_form.as_mut() {
                    f.error = Some(format!("保存失败:{e}").into());
                }
                cx.notify();
                return;
            }
            self.groups[gi].name = name;
            self.groups[gi].color = color;
            self.group_form = None;
            cx.notify();
            return;
        }
        // 先写库,成功才改内存;没有库(--demo)时只改内存,分组 id 为 0
        let id = match self.store.as_mut().map(|s| s.add_group(&name, color)) {
            Some(Ok(id)) => id,
            Some(Err(e)) => {
                crate::logs::push(crate::logs::Level::Error, "本地库", &format!("新建分组失败: {e}"));
                if let Some(f) = self.group_form.as_mut() {
                    f.error = Some(format!("保存失败:{e}").into());
                }
                cx.notify();
                return;
            }
            None => 0,
        };
        self.groups.push(Group { id, name, color, nodes: vec![] });
        self.group_form = None;
        cx.notify();
    }

    pub(crate) fn group_form_dialog(&self, window: &Window, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let f = self.group_form.as_ref()?;
        let focused = f.name.focus_handle(cx).is_focused(window);
        let editing = f.editing.is_some();
        let can = !val(&f.name, cx).is_empty();

        let title = div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .px(px(24.0))
            .pt(px(24.0))
            .pb(px(14.0))
            .child(div().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).child(if editing { "编辑分组" } else { "新建分组" }))
            .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(if editing { "修改分组的名称和颜色" } else { "用分组整理节点,可以随时把节点拖进来" }));

        // 颜色:一行圆形色块,选中的外面套一圈描边
        let swatches = GROUP_COLORS.iter().enumerate().map(|(i, &col)| {
            let on = f.color == col;
            div()
                .id(("group-color", i))
                .size(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_2()
                .border_color(if on { rgb(col) } else { gpui_kit::rgba(0x00000000) })
                .cursor_pointer()
                .child(div().size(px(18.0)).rounded_full().bg(rgb(col)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(f) = this.group_form.as_mut() {
                        f.color = col;
                    }
                    cx.notify();
                }))
        });
        let body = div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .px(px(24.0))
            .pb(px(20.0))
            .child(field(IconName::Folder, focused, f.error.is_some(), input_cell(&f.name, false)))
            .child(div().flex().flex_row().items_center().justify_between().children(swatches));

        let cancel = ctl_btn("gf-cancel", None, "取消", 34.0).on_click(cx.listener(|this, _, _, cx| this.close_group_form(cx)));
        let submit = div()
            .id("gf-ok")
            .flex()
            .flex_row()
            .items_center()
            .h(px(34.0))
            .px(px(16.0))
            .rounded(px(t::RADIUS_MD))
            .text_size(px(t::TYPE_CONTROL))
            .font_weight(FontWeight::SEMIBOLD)
            .when(can, |d| d.bg(c::accent()).text_color(c::on_accent()).cursor_pointer().hover(|s| s.opacity(0.88)))
            .when(!can, |d| d.bg(c::field()).border_1().border_color(c::field_border()).text_color(c::ghost()))
            .child(if editing { "保存" } else { "创建" })
            .on_click(cx.listener(|this, _, _, cx| this.submit_group_form(cx)));
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .px(px(24.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(c::line())
            .when_some(f.error.clone(), |d, e| {
                d.child(div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(5.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::danger()).child(icon(IconName::CircleX, 13.0, c::danger())).child(e))
            })
            .child(div().flex_1())
            .child(cancel)
            .child(submit);

        Some(modal(
            "group-form-dialog",
            440.0,
            cx.listener(|this, e: &KeyDownEvent, _, cx| match e.keystroke.key.as_str() {
                "escape" => {
                    this.close_group_form(cx);
                    cx.stop_propagation();
                }
                "enter" => {
                    this.submit_group_form(cx);
                    cx.stop_propagation();
                }
                _ => {}
            }),
            [title.into_any_element(), body.into_any_element(), footer.into_any_element()],
        ))
    }
}
