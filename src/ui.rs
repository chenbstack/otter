//! 通用界面基础:色板、尺寸常量、小部件。标准值见 docs/design/tokens.md。

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::mock::SPARK_N;

// ---- 色板(来自 theme.ts,略微拉开层次)---------------------------------------
pub mod c {
    use gpui_kit::{Rgba, rgb, rgba};
    // 配色:Velum "Nocturne" 暗色(墨黑底 + 光晕 + 烟熏玻璃卡片)。表面色是半透明白/黑,叠在 `backdrop` 上
    pub fn bg() -> Rgba { rgb(0x0B0A0F) }
    pub fn chrome() -> Rgba { rgba(0x0B0A0F6B) }
    pub fn raised() -> Rgba { rgba(0xFFFFFF1B) }
    pub fn raised_hi() -> Rgba { rgba(0xFFFFFF2B) }
    pub fn sunken() -> Rgba { rgba(0x00000080) }
    pub fn selected() -> Rgba { rgba(0xFFFFFF2B) }
    pub fn hover() -> Rgba { rgba(0xFFFFFF14) }
    pub fn border() -> Rgba { rgba(0xFFFFFF0F) }
    pub fn border_strong() -> Rgba { rgba(0xFFFFFF38) }
    pub fn text() -> Rgba { rgb(0xF5F4F7) }
    pub fn secondary() -> Rgba { rgb(0xC9C8D0) }
    pub fn muted() -> Rgba { rgb(0x9E9DA9) }
    pub fn ghost() -> Rgba { rgb(0x605F6C) }
    /// 强调色(按钮、选中、焦点);绿色 `online` 只表示成功/运行中/已连接
    pub fn accent() -> Rgba { rgb(0x8B80FF) }
    pub fn accent_soft() -> Rgba { rgba(0x8B80FF38) }
    pub fn online() -> Rgba { rgb(0x46D9A0) }
    pub fn warn() -> Rgba { rgb(0xFFB24A) }
    pub fn danger() -> Rgba { rgb(0xFF5C6C) }
    pub fn online_soft() -> Rgba { rgba(0x46D9A02B) }
    pub fn warn_soft() -> Rgba { rgba(0xFFB24A2B) }
    pub fn idle_soft() -> Rgba { rgba(0xFFFFFF14) }
    pub fn cpu() -> Rgba { rgb(0x7f89f0) }
    pub fn mem() -> Rgba { rgb(0xb18cf2) }
    pub fn net() -> Rgba { rgb(0x3fd0c9) }
    pub fn disk() -> Rgba { rgb(0xe8964c) }
    // 弹窗、抽屉:要盖住下层内容,所以不透明
    pub fn dialog() -> Rgba { rgb(0x15141F) }
    /// 半透明的弹窗底(确认框用):和节点卡片同一色系的靛紫,约 93% 不透明,隐约透出下层
    pub fn dialog_glass() -> Rgba { rgba(0x1C1B32EE) }
    pub fn dialog_border() -> Rgba { rgba(0x9A94E02A) }
    pub fn line() -> Rgba { rgba(0x9A94E016) }
    pub fn field() -> Rgba { rgba(0x9A94E00A) }
    pub fn field_hover() -> Rgba { rgba(0x9A94E014) }
    pub fn field_border() -> Rgba { rgba(0x9A94E022) }
    pub fn on_accent() -> Rgba { rgb(0xFFFFFF) }
    // 遮罩
    pub fn scrim_dialog() -> Rgba { rgba(0x0000008C) }
    pub fn scrim_drop() -> Rgba { rgba(0x0B0A0FE6) }
    // 文件类型
    pub fn folder() -> Rgba { rgb(0x7fa8f0) }
}

/// 玻璃的顶边镜面亮线:一条两头渐隐的 1px 白线(不再有左上角的白色柔光,那会让卡片发白发灰)。
/// 使用方要先 `.relative()`;`radius` 取元素自己的圆角;`strength` 是强度倍数。
pub fn glass_sheen(radius: f32, strength: f32) -> Vec<Div> {
    use gpui_kit::rgba;
    let w = |a: f32| -> gpui_kit::Rgba { gpui_kit::rgb(0xFFFFFF).opacity(a * strength) };
    let line = div()
        .absolute()
        .top_0()
        .left(px(radius))
        .right(px(radius))
        .h(px(1.0))
        .flex()
        .flex_row()
        .child(div().flex_1().h_full().bg(gpui::linear_gradient(
            90.0,
            gpui::linear_color_stop(rgba(0xFFFFFF00), 0.0),
            gpui::linear_color_stop(w(0.30), 1.0),
        )))
        .child(div().flex_1().h_full().bg(gpui::linear_gradient(
            90.0,
            gpui::linear_color_stop(w(0.30), 0.0),
            gpui::linear_color_stop(rgba(0xFFFFFF00), 1.0),
        )));
    vec![line]
}

/// 玻璃风格的参数。预设:`GlassStyle::CARD`(卡片、列表行)、`GlassStyle::CONTROL`(按钮、标签、小控件)。
/// 深色半透明玻璃:底色是带一点靛蓝的近黑色渐变(不用白色提亮,避免发灰发白),描边用很淡的鸢尾色,只有顶边一条细亮线。
/// 颜色字段都是 0xRRGGBBAA。
#[derive(Clone, Copy)]
pub struct GlassStyle {
    /// 底色渐变(顶、底);悬停时换成 `hover_*`
    pub fill_top: u32,
    pub fill_bottom: u32,
    pub hover_top: u32,
    pub hover_bottom: u32,
    /// 描边
    pub border: u32,
    pub hover_border: u32,
    /// 顶边亮线强度倍数,见 `glass_sheen`
    pub sheen: f32,
    /// 是否有外侧投影(小控件不要)
    pub shadow: bool,
    /// 扁平:纯色底(取 `fill_top`/`hover_top`),没有顶边亮线、底部暗边和外阴影
    pub flat: bool,
}

impl GlassStyle {
    pub const CARD: Self = Self { fill_top: 0x201F3A78, fill_bottom: 0x201F3A78, hover_top: 0x2C2B4E90, hover_bottom: 0x2C2B4E90, border: 0x9A94E024, hover_border: 0x9A94E045, sheen: 0.0, shadow: false, flat: true };
    pub const CONTROL: Self = Self { fill_top: 0x2A295098, fill_bottom: 0x2A295098, hover_top: 0x34335CB0, hover_bottom: 0x34335CB0, border: 0x9A94E030, hover_border: 0x9A94E050, sheen: 0.0, shadow: false, flat: true };
}

fn glass_fill(top: u32, bottom: u32) -> ::gpui::Background {
    use gpui_kit::rgba;
    ::gpui::linear_gradient(
        180.0,
        ::gpui::linear_color_stop(rgba(top), 0.0),
        ::gpui::linear_color_stop(rgba(bottom), 1.0),
    )
}

/// 给任何元素套上「玻璃」外观:深色半透明渐变底 + 淡鸢尾描边 + 顶边镜面亮线 + 底部暗边 + 可选外阴影。
/// ```ignore
/// div().id("card").glass(t::RADIUS_LG, GlassStyle::CARD).glass_hover(GlassStyle::CARD)
/// ```
/// `glass` 会设置 `.relative()`、圆角、边框、底色、阴影并追加亮线子元素,不要再覆盖这些属性。
/// 需要手型光标和悬停提亮的元素再接 `glass_hover`(要求元素有 id)。
///
/// GPUI 没有背景模糊,所以不追求"透出模糊背景"的 iOS 玻璃,而是做深色磨砂质感:靠半透明让窗口底的光晕隐约透出。
pub trait Glass: Styled + ParentElement + Sized {
    fn glass(self, radius: f32, g: GlassStyle) -> Self {
        use ::gpui::{BoxShadow, Hsla};
        use gpui_kit::rgba;
        let h = |c: gpui_kit::Rgba| -> Hsla { c.into() };
        let mut shadows = Vec::new();
        if g.shadow {
            shadows.push(BoxShadow::new(px(0.0), px(6.0), h(rgba(0x0000004D))).blur_radius(px(22.0)));
        }
        if !g.flat {
            shadows.push(BoxShadow::new(px(0.0), px(-1.0), h(rgba(0x00000040))).inset());
        }
        self.relative()
            .rounded(px(radius))
            .border_1()
            .border_color(rgba(g.border))
            .bg(glass_fill(g.fill_top, g.fill_bottom))
            .shadow(shadows)
            .children(if g.flat { vec![] } else { glass_sheen(radius, g.sheen) })
    }
}
impl<T: Styled + ParentElement + Sized> Glass for T {}

pub trait GlassHover: Styled + StatefulInteractiveElement + Sized {
    fn glass_hover(self, g: GlassStyle) -> Self {
        use gpui_kit::rgba;
        self.cursor_pointer().hover(move |s| s.bg(glass_fill(g.hover_top, g.hover_bottom)).border_color(rgba(g.hover_border)))
    }
}
impl<T: Styled + StatefulInteractiveElement + Sized> GlassHover for T {}

/// 弹窗组件:铺满整个窗口(含标题栏)的遮罩 + 居中面板,必须作为根元素最后一个子元素渲染,盖在最上层。
/// - 遮罩 `.occlude()` 拦住下面的界面,但**不响应点击关闭**(拖动窗口、误触都不会丢掉已填内容);
/// - `key` 在捕获阶段拦截按键(Esc 关闭、Enter 提交等),输入框里按也生效;处理了就自己 `cx.stop_propagation()`;
/// - 面板:宽 `width`、不透明 `surface.dialog`、`radius.2xl`、描边 + 大阴影,内容用 flex 纵向排,超高部分自己滚动(见调用方)。
/// 面板里的关闭按钮等绝对定位元素也放进 `content`(面板是 `.relative()`)。
/// 弹窗尺寸过渡时长(毫秒),缓动 `ease_out_quint`
pub const DIALOG_ANIM_MS: u64 = 200;

pub fn modal(
    id: &'static str,
    width: f32,
    key: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
    content: impl IntoIterator<Item = AnyElement>,
) -> Stateful<Div> {
    modal_panel(id, width, None, None, c::dialog(), key, content)
}

/// 指定面板底色的弹窗(钥匙串弹窗里,删除确认用半透明底,其余用不透明底)。
pub fn modal_bg(
    id: &'static str,
    width: f32,
    bg: Rgba,
    key: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
    content: impl IntoIterator<Item = AnyElement>,
) -> Stateful<Div> {
    modal_panel(id, width, None, None, bg, key, content)
}

/// 半透明版弹窗(用于确认框):底色和节点卡片同一色系,带一点透明,其余和 `modal` 一样。
pub fn modal_glass(
    id: &'static str,
    width: f32,
    key: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
    content: impl IntoIterator<Item = AnyElement>,
) -> Stateful<Div> {
    modal_panel(id, width, None, None, c::dialog_glass(), key, content)
}

/// 分步窗口:尺寸 `(宽, 高)` 从 `from` 过渡到 `to`(切换步骤时用);相同则不播动画。高度固定,内容多的步骤自己滚动。
pub fn modal_anim(
    id: &'static str,
    from: (f32, f32),
    to: (f32, f32),
    key: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
    content: impl IntoIterator<Item = AnyElement>,
) -> Stateful<Div> {
    modal_panel(id, to.0, Some(to.1), (from != to).then_some(from), c::dialog(), key, content)
}

fn modal_panel(
    id: &'static str,
    width: f32,
    height: Option<f32>,
    from: Option<(f32, f32)>,
    bg: Rgba,
    key: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static,
    content: impl IntoIterator<Item = AnyElement>,
) -> Stateful<Div> {
    let panel = div()
        .id((id, 1usize))
        .w(px(width))
        .when_some(height, |d, h| d.h(px(h)))
        .max_h_full()
        .flex()
        .flex_col()
        .rounded(px(t::RADIUS_2XL))
        .border_1()
        .border_color(c::dialog_border())
        .bg(bg)
        .shadow_2xl()
        .overflow_hidden()
        .relative()
        .occlude()
        .children(content);
    let panel = match from {
        None => panel.into_any_element(),
        Some((fw, fh)) => {
            let (tw, th) = (width, height.unwrap_or(fh));
            panel
                .with_animation(
                    SharedString::from(format!("{id}-{fw}x{fh}-{tw}x{th}")),
                    Animation::new(Duration::from_millis(DIALOG_ANIM_MS)).with_easing(ease_out_quint()),
                    move |d, t| d.w(px(fw + (tw - fw) * t)).h(px(fh + (th - fh) * t)),
                )
                .into_any_element()
        }
    };
    div()
        .id(id)
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .flex()
        .items_center()
        .justify_center()
        .p(px(24.0))
        .bg(c::scrim_dialog())
        .occlude()
        .capture_key_down(key)
        .child(panel)
}

// ---- 表单输入框与下拉选择 ------------------------------------------------------

/// 独立的描边输入框:左侧图标 + 内容(只靠占位符说明字段,不写标签);聚焦描边变强调色,出错变红。
pub fn field(ic: IconName, focused: bool, error: bool, content: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .h(px(42.0))
        .pl(px(14.0))
        .pr(px(8.0))
        .rounded(px(t::RADIUS_MD))
        .bg(c::field())
        .border_1()
        .border_color(if error {
            c::danger().opacity(0.7)
        } else if focused {
            c::accent().opacity(0.65)
        } else {
            c::field_border()
        })
        .child(icon(ic, 16.0, if focused { c::accent() } else { c::ghost() }))
        .child(content)
}

/// 下拉里的一项:可选色块 / 图标、标题、可选说明。
#[derive(Clone)]
pub struct MenuItem {
    pub label: SharedString,
    pub sub: Option<SharedString>,
    pub icon: Option<IconName>,
    pub color: Option<u32>,
}

impl MenuItem {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self { label: label.into(), sub: None, icon: None, color: None }
    }
    pub fn sub(mut self, s: impl Into<SharedString>) -> Self {
        self.sub = Some(s.into());
        self
    }
    pub fn icon(mut self, i: IconName) -> Self {
        self.icon = Some(i);
        self
    }
    pub fn color(mut self, c: u32) -> Self {
        self.color = Some(c);
        self
    }
}

/// 菜单相对触发元素的展开方向。
#[derive(Clone, Copy)]
pub enum MenuSide {
    Below,
    Above,
}

/// 下拉菜单浮层:绝对定位在最近的 `.relative()` 祖先里,铺满它的宽度,`offset` 是离它边缘的距离。
/// 用 `deferred` 最后绘制,盖在后面的输入框之上;点菜单外面触发 `on_dismiss`。
/// `selected` 的项右侧画勾。展开状态由调用方持有(和 `modal` 一样,组件不存状态)。
pub fn dropdown_menu(
    id: &'static str,
    items: Vec<MenuItem>,
    selected: Option<usize>,
    side: MenuSide,
    offset: f32,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
) -> Deferred {
    dropdown_menu_impl(id, items, selected, side, offset, false, on_pick, on_dismiss)
}

/// 紧凑版下拉菜单:行更矮、字号和图标更小,用于工具栏里的「更多」这类操作菜单。
pub fn dropdown_menu_compact(
    id: &'static str,
    items: Vec<MenuItem>,
    selected: Option<usize>,
    side: MenuSide,
    offset: f32,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
) -> Deferred {
    dropdown_menu_impl(id, items, selected, side, offset, true, on_pick, on_dismiss)
}

fn dropdown_menu_impl(
    id: &'static str,
    items: Vec<MenuItem>,
    selected: Option<usize>,
    side: MenuSide,
    offset: f32,
    compact: bool,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
) -> Deferred {
    let on_pick = std::rc::Rc::new(on_pick);
    let menu = div()
        .id(id)
        .absolute()
        .left_0()
        .right_0()
        .when(matches!(side, MenuSide::Below), |d| d.top(px(offset)))
        .when(matches!(side, MenuSide::Above), |d| d.bottom(px(offset)))
        .p(px(if compact { 3.0 } else { 4.0 }))
        .flex()
        .flex_col()
        .rounded(px(if compact { t::RADIUS_MD } else { t::RADIUS_LG }))
        .bg(c::dialog())
        .border_1()
        .border_color(c::dialog_border())
        .shadow_lg()
        .occlude()
        .on_mouse_down_out(move |_, window, cx| on_dismiss(window, cx))
        .children(items.into_iter().enumerate().map(|(i, it)| {
            let pick = on_pick.clone();
            div()
                .id((id, i + 1))
                .min_h(px(if compact { 30.0 } else if it.sub.is_some() { 46.0 } else { 36.0 }))
                .px(px(if compact { 9.0 } else { 12.0 }))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(if compact { 8.0 } else { 10.0 }))
                .rounded(px(if compact { t::RADIUS_SM } else { t::RADIUS_MD }))
                .cursor_pointer()
                .hover(|s| s.bg(c::field_hover()))
                .when_some(it.color, |d, col| d.child(div().size(px(8.0)).flex_shrink_0().rounded(px(2.0)).bg(rgb(col))))
                .when_some(it.icon, |d, ic| d.child(icon(ic, if compact { 14.0 } else { 18.0 }, c::muted())))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .child(div().text_size(px(if compact { t::TYPE_CONTROL } else { t::TYPE_BODY })).text_color(c::text()).child(it.label))
                        .when_some(it.sub, |d, sub| d.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(sub))),
                )
                .when(selected == Some(i), |d| d.child(icon(IconName::Check, 14.0, c::accent())))
                .on_click(move |_, window, cx| pick(i, window, cx))
        }));
    deferred(menu).with_priority(1)
}

/// 下拉选择:`field` 样式的触发框(左图标 + 当前值 + 上下箭头),点开向下展开 `dropdown_menu`。
/// `items[selected]` 就是当前值;`open` 由调用方持有。
pub fn select(
    id: &'static str,
    ic: IconName,
    items: Vec<MenuItem>,
    selected: usize,
    open: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
) -> Div {
    select_with(id, ic, items, selected, open, on_toggle, on_pick, on_dismiss, None)
}

/// 同 `select`,`trailing` 放在触发框内部最右边(比如清除按钮),不额外占框外的宽度。
pub fn select_with(
    id: &'static str,
    ic: IconName,
    items: Vec<MenuItem>,
    selected: usize,
    open: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
    trailing: Option<AnyElement>,
) -> Div {
    let cur = items.get(selected).cloned().unwrap_or_else(|| MenuItem::new(""));
    let trigger = div()
        .id((id, 0usize))
        .flex_1()
        .h_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .cursor_pointer()
        .text_size(px(t::TYPE_BODY))
        .text_color(c::text())
        .when_some(cur.color, |d, col| d.child(div().size(px(8.0)).rounded(px(2.0)).bg(rgb(col))))
        .child(div().flex_1().child(cur.label))
        .child(icon(IconName::ChevronsUpDown, 14.0, c::ghost()))
        .on_click(move |_, window, cx| on_toggle(window, cx));
    div()
        .relative()
        .child(field(ic, open, false, div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(6.0)).child(trigger).children(trailing)))
        .when(open, |d| d.child(dropdown_menu(id, items, Some(selected), MenuSide::Below, 46.0, on_pick, on_dismiss)))
}

/// 窗口底:墨黑上铺三团光晕(玫红、紫、蓝),各区域的半透明表面叠在它上面。
/// GPUI 没有径向渐变,用一摞半透明同心圆近似。
pub fn backdrop() -> Div {
    use gpui_kit::{Rgba, rgba};
    const LAYERS: usize = 40;
    fn blob(x: f32, y: f32, radius: f32, hex: u32) -> impl Iterator<Item = Div> {
        (0..LAYERS).map(move |i| {
            let r = radius * (1.0 - i as f32 / LAYERS as f32);
            let color: Rgba = rgba((hex << 8) | 0x07);
            div().absolute().left(relative(x)).top(relative(y)).ml(px(-r)).mt(px(-r)).size(px(r * 2.0)).rounded_full().bg(color)
        })
    }
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .children(blob(0.06, 0.0, 640.0, 0x240F1A))
        .children(blob(1.0, 0.08, 600.0, 0x171229))
        .children(blob(0.72, 1.05, 680.0, 0x0D1830))
}

/// 字号与圆角 token,标准值见 docs/design/tokens.md
pub mod t {
    pub const TYPE_DISPLAY: f32 = 20.0;
    pub const TYPE_TITLE_LG: f32 = 18.0;
    pub const TYPE_TITLE: f32 = 15.0;
    pub const TYPE_TITLE_SM: f32 = 14.0;
    pub const TYPE_BODY: f32 = 13.0;
    pub const TYPE_CONTROL: f32 = 12.0;
    pub const TYPE_CAPTION: f32 = 11.0;
    pub const TYPE_MICRO: f32 = 10.5;

    pub const RADIUS_XS: f32 = 6.0;
    pub const RADIUS_SM: f32 = 8.0;
    pub const RADIUS_MD: f32 = 11.0;
    pub const RADIUS_LG: f32 = 14.0;
    pub const RADIUS_XL: f32 = 18.0;
    pub const RADIUS_2XL: f32 = 20.0;
}

pub const MONO: &str = "Menlo";

// ---- 尺寸 --------------------------------------------------------------------
pub const MENU_W: f32 = 200.0;
/// 表单里「添加认证方式」这类下拉菜单卡片的宽度(比输入框窄,放得下两行文字)
pub const DROPDOWN_W: f32 = 260.0;
/// 工具栏「更多」这类紧凑操作菜单的宽度
pub const DROPDOWN_COMPACT_W: f32 = 148.0;
pub const TITLEBAR_H: f32 = 46.0;
/// 标题栏里的标签(胶囊)高度,垂直居中
pub const TAB_H: f32 = 30.0;
/// 标题栏左侧给红绿灯留的宽度
pub const TITLEBAR_PAD_L: f32 = 84.0;
/// 节点页卡片:最小宽度(决定列数)、高度、间距
pub const NODE_CARD_MIN_W: f32 = 232.0;
pub const NODE_CARD_H: f32 = 56.0;
/// 节点页:卡片里的图标块边长、顶部搜索条和主按钮高度
pub const NODE_ICON: f32 = 32.0;
/// 新建分组时可选的颜色(`group.*` token),第一个是蓝、第二个是玫红、第三个是石板灰,和 mock 分组一致
pub const GROUP_COLORS: [u32; 8] = [0x5DBBFF, 0xFF5C93, 0x7D8EA6, 0x46D9A0, 0xFFB24A, 0xB07CE0, 0xFF8A5B, 0x8B80FF];
pub const NODE_BAR_H: f32 = 34.0;
/// 终端字号、终端画布内边距
pub const TERM_FONT: f32 = 13.0;
pub const TERM_PAD: f32 = 8.0;
/// 终端默认前景色,见 `docs/design/tokens.md` 1.9 `terminal.fg`
pub const TERM_FG: (u8, u8, u8) = (0xB3, 0xAE, 0xDB);
/// 终端 ANSI 16 色(0–7 普通,8–15 明亮),取值见 `docs/design/tokens.md` 1.9
pub const TERM_ANSI: [(u8, u8, u8); 16] = [
    (0x1A, 0x19, 0x23), (0xFF, 0x5C, 0x6C), (0x46, 0xD9, 0xA0), (0xFF, 0xB2, 0x4A),
    (0x7F, 0x9B, 0xFF), (0xB1, 0x8C, 0xF2), (0x3F, 0xD0, 0xC9), (0xC9, 0xC8, 0xD0),
    (0x60, 0x5F, 0x6C), (0xFF, 0x8A, 0x96), (0x74, 0xE8, 0xBB), (0xFF, 0xC8, 0x78),
    (0xA5, 0xB8, 0xFF), (0xC9, 0xAA, 0xFF), (0x6E, 0xE3, 0xDD), (0xF5, 0xF4, 0xF7),
];
pub const NODE_CARD_GAP: f32 = 10.0;
/// 节点页左侧栏宽度
pub const NODES_SIDE_W: f32 = 184.0;
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
        .text_size(px(t::TYPE_CAPTION))
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
        .rounded(px(t::RADIUS_SM))
        .bg(c::idle_soft())
        .text_size(px(t::TYPE_CAPTION))
        .text_color(c::secondary())
        .child(label.into())
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
                .rounded(px(t::RADIUS_2XL))
                .border_1()
                .border_color(c::border())
                .bg(c::raised())
                .child(icon(ic, 22.0, c::ghost())),
        )
        .child(div().mt(px(4.0)).text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child(title))
        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(sub))
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
        .glass(t::RADIUS_MD, GlassStyle::CONTROL)
        .glass_hover(GlassStyle::CONTROL)
        .text_size(px(t::TYPE_CONTROL))
        .text_color(c::secondary())
        .hover(|s| s.text_color(c::text()))
        .child(icon(ic, 13.0, c::muted()))
        .child(label)
}
