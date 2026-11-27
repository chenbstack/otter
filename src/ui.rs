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
    pub fn scrim_takeover() -> Rgba { rgba(0x0B0A0F66) }
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

type ModalKey = std::rc::Rc<dyn Fn(&KeyDownEvent, &mut Window, &mut App)>;

thread_local! {
    /// 当前帧最后构建的弹窗的按键处理。按键事件只会送到焦点所在元素的祖先链,弹窗打开时焦点常常不在弹窗里
    /// (点开弹窗的按钮、后面的列表等),弹窗自己的监听就收不到 Esc;所以根元素统一转发到这里。
    static TOP_MODAL_KEY: std::cell::RefCell<Option<ModalKey>> = const { std::cell::RefCell::new(None) };
}

/// 每帧渲染开头调用,清掉上一帧的弹窗按键处理(弹窗关了就不再转发)
pub fn modal_key_reset() {
    TOP_MODAL_KEY.with(|k| *k.borrow_mut() = None);
}

/// 当前有没有弹窗在等按键
pub fn modal_key_active() -> bool {
    TOP_MODAL_KEY.with(|k| k.borrow().is_some())
}

/// 根元素的按键捕获调用:有弹窗就把按键交给它(弹窗的处理里 `stop_propagation` 即吞掉)
pub fn modal_key_dispatch(ev: &KeyDownEvent, window: &mut Window, cx: &mut App) {
    let f = TOP_MODAL_KEY.with(|k| k.borrow().clone());
    if let Some(f) = f {
        f(ev, window, cx);
    }
}

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

// ---- 提示框公用部件(接管确认、连接已断开等同一套样式)-----------------------------------------
/// 提示框的内边距与按钮宽度:`n` 个等宽按钮排一行时每个的宽度。
pub fn alert_btn_w(n: usize) -> f32 {
    let gap = 10.0;
    (crate::confirm::CONFIRM_W - 58.0 - gap * (n as f32 - 1.0)) / n as f32
}

/// 顶部:带两层光圈的图标(`tone` 决定颜色)、标题、副标题。
pub fn alert_hero(ic: IconName, tone: Rgba, title: String, sub: &str) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(4.0))
        .child(
            div()
                .size(px(68.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(t::RADIUS_2XL))
                .bg(tone.opacity(0.08))
                .border_1()
                .border_color(tone.opacity(0.18))
                .child(
                    div()
                        .size(px(48.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(t::RADIUS_XL))
                        .bg(tone.opacity(0.16))
                        .border_1()
                        .border_color(tone.opacity(0.36))
                        .child(icon(ic, 22.0, tone)),
                ),
        )
        .child(div().mt(px(10.0)).max_w(px(340.0)).truncate().text_size(px(t::TYPE_TITLE_LG)).font_weight(FontWeight::BOLD).text_color(c::text()).child(title))
        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).child(SharedString::from(sub.to_string())))
}

/// 一组「图标 + 一行说明」的清单。
pub fn alert_rows(rows: Vec<(IconName, Rgba, String)>) -> Div {
    let n = rows.len();
    div().w_full().flex().flex_col().rounded(px(t::RADIUS_MD)).border_1().border_color(c::field_border()).bg(c::field()).children(rows.into_iter().enumerate().map(|(i, (ic, color, text))| {
        div()
            .min_h(px(38.0))
            .px(px(12.0))
            .py(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .when(i + 1 < n, |d| d.border_b_1().border_color(c::line()))
            .child(icon(ic, 14.0, color))
            .child(div().flex_1().min_w_0().text_size(px(t::TYPE_CONTROL)).text_color(c::secondary()).child(text))
    }))
}

/// 把各部分按统一的间距和内边距排好:hero、(可选)节点卡片、清单、按钮。
pub fn alert_body(hero: Div, target: Option<Div>, rows: Div, buttons: Div) -> Div {
    div().flex().flex_col().items_center().gap(px(18.0)).px(px(28.0)).pt(px(28.0)).pb(px(24.0)).child(hero).children(target).child(rows).child(buttons)
}

/// 实心按钮:`w` 为 None 时占满整行;`bg` 是底色(主色或危险色)。
pub fn alert_solid_btn(id: &'static str, ic: Option<IconName>, label: &'static str, w: Option<f32>, bg: Rgba) -> Stateful<Div> {
    div()
        .id(id)
        .when_some(w, |d, w| d.w(px(w)).flex_none())
        .when(w.is_none(), |d| d.w_full())
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
        .when_some(ic, |d, ic| d.child(icon(ic, 14.0, c::on_accent())))
        .child(label)
}

/// 主色的实心按钮(默认项)。
pub fn alert_primary_btn(id: &'static str, ic: Option<IconName>, label: &'static str, w: Option<f32>) -> Stateful<Div> {
    alert_solid_btn(id, ic, label, w, c::accent())
}

/// 对象卡片:左边图标块 + 名称 + 副标题,和节点页的节点卡片同款。
pub fn alert_target(tile: impl IntoElement, name: SharedString, detail: SharedString) -> Div {
    div()
        .w_full()
        .h(px(crate::NODE_CARD_H))
        .glass(t::RADIUS_LG, GlassStyle::CARD)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .px(px(12.0))
        .child(tile)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(div().truncate().text_size(px(t::TYPE_BODY)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(name))
                .child(div().truncate().text_size(px(t::TYPE_MICRO)).font_family(crate::MONO).text_color(c::ghost()).child(detail)),
        )
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
    TOP_MODAL_KEY.with(|k| *k.borrow_mut() = Some(std::rc::Rc::new(key)));
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
        .child(panel)
}

/// 抽屉组件:铺满整个窗口(含标题栏)的遮罩 + 靠右的整高面板。
/// - 面板底色是应用底色 `bg`,并铺一层和窗口一样的光团 `backdrop`,看起来和应用本身是同一块;左边一条描边加阴影和遮罩区分;
/// - 点遮罩调用 `on_close`,点面板本身不会关闭;
/// - 面板宽 `width`,内容用 flex 纵向排(`content` 里自己的区域用 `flex_1().min_h_0()` 撑满、自己滚动)。
/// 必须作为根元素靠后的子元素渲染,盖在最上层。
pub fn drawer(
    id: &'static str,
    width: f32,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    content: impl IntoIterator<Item = AnyElement>,
) -> Stateful<Div> {
    div()
        .id((id, 0usize))
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .flex()
        .flex_row()
        .justify_end()
        .bg(c::scrim_dialog())
        .occlude()
        .on_click(on_close)
        .child(
            div()
                .id((id, 1usize))
                .occlude()
                        .relative()
                .overflow_hidden()
                .h_full()
                .w(px(width))
                .flex()
                .flex_col()
                .bg(c::bg())
                .border_l_1()
                .border_color(c::dialog_border())
                .shadow_lg()
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(backdrop())
                .children(content),
        )
}

// ---- 表单输入框与下拉选择 ------------------------------------------------------

/// 独立的描边输入框:左侧图标 + 内容(只靠占位符说明字段,不写标签);聚焦描边变强调色,出错变红。
pub fn field(ic: IconName, focused: bool, error: bool, content: impl IntoElement) -> Div {
    field_h(42.0, ic, focused, error, content)
}

/// 同 `field`,高度可调(设置页等行内控件用 `control.h.md` = 30)
pub fn field_h(h: f32, ic: IconName, focused: bool, error: bool, content: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .h(px(h))
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
    dropdown_menu_impl(id, items, selected, side, offset, false, MENU_MAX_H, None, on_pick, on_dismiss)
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
    dropdown_menu_impl(id, items, selected, side, offset, true, MENU_MAX_H, None, on_pick, on_dismiss)
}

/// 下拉菜单的最大高度:选项再多也不超过它,超出的在菜单里滚动
pub const MENU_MAX_H: f32 = 360.0;

fn dropdown_menu_impl(
    id: &'static str,
    items: Vec<MenuItem>,
    selected: Option<usize>,
    side: MenuSide,
    offset: f32,
    compact: bool,
    max_h: f32,
    // 面板顶部固定的一行(搜索框);有它时列表单独滚动,它不跟着滚
    header: Option<AnyElement>,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
) -> Deferred {
    let on_pick = std::rc::Rc::new(on_pick);
    let has_header = header.is_some();
    let empty_list = items.is_empty();
    let menu = div()
        .id(id)
        .absolute()
        .left_0()
        .right_0()
        .when(matches!(side, MenuSide::Below), |d| d.top(px(offset)))
        .when(matches!(side, MenuSide::Above), |d| d.bottom(px(offset)))
        .p(px(if compact { 3.0 } else { 4.0 }))
        .max_h(px(max_h))
        .when(!has_header, |d| d.overflow_y_scroll())
        .flex()
        .flex_col()
        .gap(px(2.0))
        .rounded(px(if compact { t::RADIUS_MD } else { t::RADIUS_LG }))
        .bg(c::dialog())
        .border_1()
        .border_color(c::dialog_border())
        .shadow_lg()
        .occlude()
        .on_mouse_down_out(move |_, window, cx| on_dismiss(window, cx));
    let rows = items.into_iter().enumerate().map(|(i, it)| {
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
                // 当前项:底色 + 强调色文字,一眼能找到
                .when(selected == Some(i), |d| d.bg(c::field_hover()))
                .when_some(it.color, |d, col| d.child(div().size(px(8.0)).flex_shrink_0().rounded(px(2.0)).bg(rgb(col))))
                .when_some(it.icon, |d, ic| d.child(icon(ic, if compact { 14.0 } else { 18.0 }, c::muted())))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .child(div().text_size(px(if compact { t::TYPE_CONTROL } else { t::TYPE_BODY })).text_color(if selected == Some(i) { c::accent() } else { c::text() }).when(selected == Some(i), |d| d.font_weight(FontWeight::MEDIUM)).child(it.label))
                        .when_some(it.sub, |d, sub| d.child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(sub))),
                )
                .when(selected == Some(i), |d| d.child(icon(IconName::Check, 13.0, c::accent())))
                .on_click(move |_, window, cx| pick(i, window, cx))
        });
    let menu = match header {
        None => menu.children(rows),
        Some(h) => menu.child(h).child(
            div()
                .id((id, 100_000usize))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px((max_h - 52.0).max(80.0)))
                .overflow_y_scroll()
                .children(rows)
                .when(empty_list, |d| d.child(div().h(px(36.0)).px(px(12.0)).flex().items_center().text_size(px(t::TYPE_BODY)).text_color(c::ghost()).child("没有匹配的选项"))),
        ),
    };
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
    select_sized(42.0, id, ic, items, selected, open, on_toggle, on_pick, on_dismiss, trailing)
}

/// 同 `select_with`,触发框高度可调;菜单离触发框 4px。
pub fn select_sized(
    h: f32,
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
    select_impl(h, id, ic, items, selected, open, on_toggle, on_pick, on_dismiss, trailing, None)
}

/// 带搜索的下拉:展开后面板顶部固定一个搜索框,下面的选项按输入实时过滤(不分大小写,匹配标签或副标题)。
/// `search` 是 (搜索框状态, 当前输入),输入由调用方在渲染时读出来传入;`on_pick` 收到的始终是**原 `items` 里的序号**。
/// 调用方要在 `on_toggle` 展开时清空搜索框并聚焦它(`set_value("")` + `window.focus`)。
pub fn select_search(
    h: f32,
    id: &'static str,
    ic: IconName,
    items: Vec<MenuItem>,
    selected: usize,
    open: bool,
    search: (Entity<gpui_kit::component::input::InputState>, String),
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
    trailing: Option<AnyElement>,
) -> Div {
    select_impl(h, id, ic, items, selected, open, on_toggle, on_pick, on_dismiss, trailing, Some(search))
}

fn select_impl(
    h: f32,
    id: &'static str,
    ic: IconName,
    items: Vec<MenuItem>,
    selected: usize,
    open: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
    trailing: Option<AnyElement>,
    search: Option<(Entity<gpui_kit::component::input::InputState>, String)>,
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
    // 菜单往哪边开:每帧在布局时记下触发框下方还剩多少空间,展开时据此判断放得下放不下(放不下又是上方更宽裕就往上开)
    let need = items.len() as f32 * 36.0 + 8.0 + h + 16.0;
    let below = SELECT_ROOM.with(|m| m.borrow().get(id).copied()).unwrap_or(f32::MAX);
    let above = SELECT_ROOM_ABOVE.with(|m| m.borrow().get(id).copied()).unwrap_or(0.0);
    let side = if below < need && above > below { MenuSide::Above } else { MenuSide::Below };
    div()
        .relative()
        .child(
            canvas(
                move |bounds, window, _| {
                    let vh = f32::from(window.viewport_size().height);
                    SELECT_ROOM.with(|m| m.borrow_mut().insert(id, vh - f32::from(bounds.bottom())));
                    SELECT_ROOM_ABOVE.with(|m| m.borrow_mut().insert(id, f32::from(bounds.top())));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .child(field_h(h, ic, open, false, div().flex_1().min_w_0().flex().flex_row().items_center().gap(px(6.0)).child(trigger).children(trailing)))
        // 菜单高度不超过所在方向剩余的空间(留 16 边距),再多的选项在菜单里滚动
        .when(open, |d| {
            let room = if matches!(side, MenuSide::Below) { below } else { above };
            let max_h = (room - (h + 4.0) - 16.0).clamp(120.0, MENU_MAX_H);
            match search {
                None => d.child(dropdown_menu_impl(id, items, Some(selected), side, h + 4.0, true, max_h, None, on_pick, on_dismiss)),
                Some((input, q)) => {
                    use gpui_kit::component::input::Input;
                    use gpui_kit::component::Sizable as _;
                    let q = q.trim().to_lowercase();
                    let map: Vec<usize> = items.iter().enumerate().filter(|(_, it)| q.is_empty() || it.label.to_lowercase().contains(&q) || it.sub.as_ref().is_some_and(|s| s.to_lowercase().contains(&q))).map(|(i, _)| i).collect();
                    let shown: Vec<MenuItem> = map.iter().map(|&i| items[i].clone()).collect();
                    let sel = map.iter().position(|&i| i == selected);
                    let header = div()
                        .h(px(34.0))
                        .flex_shrink_0()
                        .px(px(10.0))
                        .mb(px(2.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .rounded(px(t::RADIUS_MD))
                        .bg(c::field())
                        .child(icon(IconName::Search, 14.0, c::ghost()))
                        .child(div().flex_1().min_w_0().child(Input::new(&input).appearance(false).with_size(px(12.0 / 0.875))))
                        .into_any_element();
                    d.child(dropdown_menu_impl(id, shown, sel, side, h + 4.0, true, max_h, Some(header), move |pos, window, cx| on_pick(map.get(pos).copied().unwrap_or(0), window, cx), on_dismiss))
                }
            }
        })
}

thread_local! {
    /// `select_sized` 触发框下方 / 上方到窗口边缘的空间(逻辑像素),按控件 id 记
    static SELECT_ROOM: std::cell::RefCell<std::collections::HashMap<&'static str, f32>> = Default::default();
    static SELECT_ROOM_ABOVE: std::cell::RefCell<std::collections::HashMap<&'static str, f32>> = Default::default();
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

/// 等宽字体:终端和界面里所有等宽文字。字体名必须是该系统上真有的,否则会退回成比例字体,
/// 终端里按固定格子摆放就会出现字符间距忽大忽小(Windows 上没有 Menlo 就是这样)。
#[cfg(target_os = "macos")]
pub const MONO: &str = "Menlo";
#[cfg(target_os = "windows")]
pub const MONO: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const MONO: &str = "DejaVu Sans Mono";

// ---- 尺寸 --------------------------------------------------------------------
pub const MENU_W: f32 = 200.0;
/// 表单里「添加认证方式」这类下拉菜单卡片的宽度(比输入框窄,放得下两行文字)
pub const DROPDOWN_W: f32 = 260.0;
/// 工具栏「更多」这类紧凑操作菜单的宽度
pub const DROPDOWN_COMPACT_W: f32 = 148.0;
/// 窗口最小大小
pub const WINDOW_MIN_W: f32 = 1100.0;
pub const WINDOW_MIN_H: f32 = 700.0;
/// 第一次启动(没有记忆)的默认窗口大小
pub const WINDOW_DEFAULT_W: f32 = 1360.0;
pub const WINDOW_DEFAULT_H: f32 = 860.0;
pub const TITLEBAR_H: f32 = 46.0;
/// 标题栏里的标签(胶囊)高度,垂直居中
pub const TAB_H: f32 = 30.0;
/// 标题栏左侧给红绿灯留的宽度
pub const TITLEBAR_PAD_L: f32 = 84.0;
/// 节点页卡片:最小宽度(决定列数)、高度、间距
pub const NODE_CARD_MIN_W: f32 = 232.0;
/// 网站设置页卡片最小宽度:放得下就并排,放不下换行
pub const SET_CARD_MIN_W: f32 = 440.0;
pub const NODE_CARD_H: f32 = 56.0;
/// 节点页:卡片里的图标块边长、顶部搜索条和主按钮高度
pub const NODE_ICON: f32 = 32.0;
/// 新建分组时可选的颜色(`group.*` token),第一个是蓝、第二个是玫红、第三个是石板灰,和 mock 分组一致
pub const GROUP_COLORS: [u32; 8] = [0x5DBBFF, 0xFF5C93, 0x7D8EA6, 0x46D9A0, 0xFFB24A, 0xB07CE0, 0xFF8A5B, 0x8B80FF];
pub const NODE_BAR_H: f32 = 30.0;
/// 终端字号、终端画布内边距
pub const TERM_FONT: f32 = 13.0;
pub const TERM_PAD: f32 = 8.0;
pub const NODE_CARD_GAP: f32 = 10.0;
/// 节点页左侧栏宽度
pub const NODES_SIDE_W: f32 = 184.0;
pub const CONTENT_PAD: f32 = 20.0;

pub const STACK_HEADER_H: f32 = 50.0;
pub const STACK_BODY_PAD: f32 = 14.0;
/// 堆栈页虚拟列表第 0 项(页头 + 页签)的高度
pub const STACK_HEAD_H: f32 = 124.0;
/// 容器页页头(图标 + 标题,不含页签)的高度 = 上边距 18 + 图标 36 + 下边距 14;页签行 = 30 + 下边距 10。页头滚出这么多后页签吸顶
pub const DOCK_HEAD_H: f32 = 68.0;
pub const DOCK_TABS_H: f32 = 40.0;
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

/// 接管遮罩中间图标和文字区域的柔和暗影(token `shadow.overlay_content`):遮罩是半透明的,底下终端内容会透出来干扰阅读。
pub fn shadow_overlay_content() -> Vec<::gpui::BoxShadow> {
    vec![::gpui::BoxShadow { color: ::gpui::Hsla::from(rgba(0x0000008C)), offset: point(px(0.0), px(2.0)), blur_radius: px(64.0), spread_radius: px(14.0), inset: false }]
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

/// 中性小标签(端口、算法、挂载它的容器名等)。**一律用它,不要手写**:
/// 外面包了一层行布局,所以不管放进块级容器(列、单元格)还是弹性容器,标签都只有文字那么宽,不会被撑满整列。
/// 例:`tag("ssh-ed25519")`、`tag(port).mono().small()`;要加 id、悬停、提示的用 `.decorate(|pill| …)`,作用在标签本体上。
pub struct Tag {
    label: SharedString,
    mono: bool,
    size: f32,
    color: Rgba,
    bg: Rgba,
    decorate: Option<Box<dyn FnOnce(Div) -> AnyElement>>,
}

pub fn tag(label: impl Into<SharedString>) -> Tag {
    Tag { label: label.into(), mono: false, size: t::TYPE_CAPTION, color: c::secondary(), bg: c::idle_soft(), decorate: None }
}

impl Tag {
    /// 等宽字体(端口、哈希这类)。
    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }
    /// 小一号的字(表格行里放多个时用)。
    pub fn small(mut self) -> Self {
        self.size = t::TYPE_MICRO;
        self
    }
    pub fn color(mut self, color: Rgba) -> Self {
        self.color = color;
        self
    }
    /// 换底色(状态类标签:放行绿、丢弃红……),和 `color` 配成一对。
    pub fn bg(mut self, bg: Rgba) -> Self {
        self.bg = bg;
        self
    }
    /// 给标签本体加东西(id、悬停、提示、光标),返回最终元素。
    pub fn decorate(mut self, f: impl FnOnce(Div) -> AnyElement + 'static) -> Self {
        self.decorate = Some(Box::new(f));
        self
    }
}

impl IntoElement for Tag {
    type Element = Div;

    fn into_element(self) -> Div {
        let pill = div().flex_shrink_0().px(px(7.0)).py(px(2.0)).rounded(px(t::RADIUS_SM)).bg(self.bg).text_size(px(self.size)).text_color(self.color).when(self.mono, |d| d.font_family(crate::MONO)).child(self.label);
        let inner = match self.decorate {
            Some(f) => f(pill),
            None => pill.into_any_element(),
        };
        div().flex().flex_row().items_center().flex_shrink_0().child(inner)
    }
}

/// 空状态组件:图标块 + 标题 + 说明 + 可选操作。**新页面的空/失败/不可用状态用它,不要手写。**
/// 自动高度:撑满父容器剩下的空间,内容水平居中、垂直偏上(顶部留 96px,不在正中);父容器要是 flex 列。
/// 图标颜色表达语气:默认灰(没有数据),`.warn()` 黄(不可用),`.danger()` 红(失败)。
/// 例:`empty(IconName::Database, "还没有业务数据库").desc("点右上角新建").action(button(..))`
pub struct Empty {
    icon: IconName,
    title: SharedString,
    desc: Option<SharedString>,
    tone: Rgba,
    action: Option<AnyElement>,
}

pub fn empty(ic: IconName, title: impl Into<SharedString>) -> Empty {
    Empty { icon: ic, title: title.into(), desc: None, tone: c::ghost(), action: None }
}

impl Empty {
    pub fn desc(mut self, d: impl Into<SharedString>) -> Self {
        self.desc = Some(d.into());
        self
    }
    pub fn warn(mut self) -> Self {
        self.tone = c::warn();
        self
    }
    pub fn danger(mut self) -> Self {
        self.tone = c::danger();
        self
    }
    pub fn action(mut self, a: impl IntoElement) -> Self {
        self.action = Some(a.into_any_element());
        self
    }
}

impl IntoElement for Empty {
    type Element = Div;

    fn into_element(self) -> Div {
        div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_start()
            .gap(px(10.0))
            .flex_1()
            .pt(px(96.0))
            .pb(px(40.0))
            .child(div().size(px(52.0)).flex().items_center().justify_center().rounded(px(t::RADIUS_2XL)).border_1().border_color(c::border()).bg(c::raised()).child(icon(self.icon, 22.0, self.tone)))
            .child(div().mt(px(4.0)).text_size(px(t::TYPE_TITLE_SM)).font_weight(FontWeight::SEMIBOLD).text_color(c::secondary()).child(self.title))
            .children(self.desc.map(|d| div().max_w(px(520.0)).text_center().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(d)))
            .children(self.action.map(|a| div().mt(px(6.0)).child(a)))
    }
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

/// 旧名字,等价于 `empty(图标, 标题).desc(说明)`;新代码直接用 `empty`。
pub fn empty_state(ic: IconName, title: &'static str, sub: &'static str) -> Empty {
    empty(ic, title).desc(sub)
}

pub fn button(id: &'static str, ic: IconName, label: &'static str) -> Stateful<Div> {
    button_busy(id, ic, label, false)
}

/// 带文字的按钮;`busy` 时图标换成加载动画(这时不要再挂点击)。
pub fn button_busy(id: &'static str, ic: IconName, label: &'static str, busy: bool) -> Stateful<Div> {
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
        .child(if busy { dot_spinner((id, 0), c::muted()).into_any_element() } else { icon(ic, 13.0, c::muted()).into_any_element() })
        .child(label)
}

/// 3×3 小方格,一个亮点沿外圈 8 格转圈,用作按钮的加载动画
pub fn dot_spinner(id: (&'static str, usize), color: Rgba) -> impl IntoElement {
    dot_spinner_scaled(id, color, 1.0)
}

/// 放大版点阵加载图标(`scale` 倍,1.0 就是 14px 的标准尺寸)
pub fn dot_spinner_scaled(id: (&'static str, usize), color: Rgba, scale: f32) -> impl IntoElement {
    let cell = 4.0 * scale;
    let gap = 1.0 * scale;
    // 外圈顺时针的 8 个格子 (行, 列)
    const RING: [(usize, usize); 8] = [(0, 0), (0, 1), (0, 2), (1, 2), (2, 2), (2, 1), (2, 0), (1, 0)];
    div().size(px(14.0 * scale)).flex().items_center().justify_center().child(
        div().with_animation(id, Animation::new(std::time::Duration::from_millis(900)).repeat(), move |d, t| {
            let lit = RING[((t * 8.0) as usize).min(7)];
            let mut grid = d.flex().flex_col().gap(px(gap));
            for r in 0..3 {
                let mut row = div().flex().flex_row().gap(px(gap));
                for col in 0..3 {
                    row = row.child(div().size(px(cell)).rounded(px(1.0 * scale)).bg(if (r, col) == lit { color } else { color.opacity(0.18) }));
                }
                grid = grid.child(row);
            }
            grid
        }),
    )
}

/// 整页的「正在读取」:放大的点阵加载图标 + 一行说明,位置和 `empty` 一致(水平居中、垂直偏上)。**页面加载状态一律用它。**
pub fn loading(msg: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_start()
        .gap(px(16.0))
        .pt(px(96.0))
        .pb(px(40.0))
        .child(dot_spinner_scaled(("page-loading", 0), c::accent(), 2.5))
        .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(msg.into()))
}

/// 全局"点击即失焦":在窗口层监听捕获阶段的鼠标按下,不看悬停/遮挡,先清掉当前焦点,
/// 之后被点的输入框在自己的处理里再抢回焦点。放在根元素最前面一次即可,弹窗、抽屉、菜单都生效。
/// `skip` 为 true 时不处理(终端内容区要保留键盘焦点)。
pub fn blur_on_click(skip: bool) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |_, _, window, _| {
            if !skip {
                window.on_mouse_event(move |_: &MouseDownEvent, phase, window, cx| {
                    if phase == DispatchPhase::Capture {
                        window.blur(cx);
                    }
                });
            }
        },
    )
    .absolute()
    .size_0()
}

/// 代码编辑器(`Editor`)的配色:背景透明(外面的容器负责底色),文字、行号、当前行和语法高亮都取自 `mod c` 色板,
/// 这样编辑器和界面同一套风格。语法名见 gpui-component 的 `SyntaxColors`
pub fn editor_highlight_theme() -> Option<std::sync::Arc<gpui_kit::component::highlighter::HighlightTheme>> {
    let hex = |r: Rgba| format!("#{:02x}{:02x}{:02x}{:02x}", (r.r * 255.0).round() as u8, (r.g * 255.0).round() as u8, (r.b * 255.0).round() as u8, (r.a * 255.0).round() as u8);
    let s = |col: Rgba| serde_json::json!({ "color": hex(col) });
    let it = |col: Rgba| serde_json::json!({ "color": hex(col), "font_style": "italic" });
    let v = serde_json::json!({
        "name": "otter",
        "appearance": "dark",
        "style": {
            "editor.background": "#00000000",
            "editor.gutter.background": "#00000000",
            "editor.foreground": hex(c::text()),
            "editor.active_line.background": hex(c::field_hover()),
            "editor.line_number": hex(c::ghost()),
            "editor.active_line_number": hex(c::muted()),
            "syntax": {
                "comment": it(c::ghost()),
                "comment_doc": it(c::ghost()),
                "keyword": s(c::accent()),
                "function": s(c::accent()),
                "property": s(c::accent()),
                "attribute": s(c::accent()),
                "label": s(c::accent()),
                "string": s(c::online()),
                "string.escape": s(c::warn()),
                "string.regex": s(c::warn()),
                "string.special": s(c::online()),
                "number": s(c::warn()),
                "boolean": s(c::warn()),
                "constant": s(c::warn()),
                "variable": s(c::secondary()),
                "variable.special": s(c::secondary()),
                "type": s(c::warn()),
                "tag": s(c::accent()),
                "operator": s(c::muted()),
                "punctuation": s(c::muted()),
                "punctuation.bracket": s(c::muted()),
                "punctuation.delimiter": s(c::muted()),
                "punctuation.special": s(c::muted()),
                "preproc": s(c::danger()),
                "title": s(c::text())
            }
        }
    });
    serde_json::from_value(v).ok().map(std::sync::Arc::new)
}
