//! 概览页:整机资源监控(CPU、内存、磁盘、网络、进程),有 Docker 时再加一块容器。
//! 数据来自 `crate::monitor`(经 `Docker` 连接采样),界面只读 `MonSnapshot`。

use super::super::*;
use crate::monitor::{self, MonSnapshot};

/// 图里画最近多少个点(2 秒一个,3 分钟)
const POINTS: usize = 90;

fn pt(x: f32, y: f32) -> Point<Pixels> {
    point(px(x), px(y))
}


fn pct(v: f32) -> String {
    format!("{v:.0}%")
}

/// 竖条图(btop 的块状图):每个点一根竖条,高度按行量化,颜色从底部的 `low` 渐变到顶部随数值变化的 `high`。
/// `mirror` 为真时从中线向下长(画上传、写入这类反向数据)。
fn block_graph(vals: Vec<f32>, hover: (&'static str, &'static str, fn(f32) -> String), max: Option<f32>, floor: f32, h: f32, palette: Palette, mirror: bool) -> impl IntoElement {
    let (id, name, fmt) = hover;
    let samples = vals[vals.len().saturating_sub(POINTS)..].to_vec();
    let canvas = block_canvas(vals, max, floor, palette, mirror);
    let tips = samples.into_iter().map(|val| format!("{name} {}", fmt(val))).collect();
    graph_box(id, tips, h, canvas)
}

fn block_canvas(vals: Vec<f32>, max: Option<f32>, floor: f32, palette: Palette, mirror: bool) -> impl IntoElement {
    gpui::canvas(
        |_, _, _| (),
        move |b, _, window, _| {
            let (x0, y0) = (f32::from(b.origin.x), f32::from(b.origin.y));
            let (w, hh) = (f32::from(b.size.width), f32::from(b.size.height));
            let v = &vals[vals.len().saturating_sub(POINTS)..];
            let top = max.unwrap_or_else(|| v.iter().cloned().fold(floor, f32::max)).max(f32::EPSILON);
            let slot = w / POINTS as f32;
            let bw = (slot - 2.0).max(1.0);
            let row = 4.0;
            let rows = (hh / row).floor().max(1.0);
            let n = v.len();
            for (i, val) in v.iter().enumerate() {
                let f = (val / top).clamp(0.0, 1.0);
                let ch = ((f * rows).ceil() * row).min(hh);
                if ch <= 0.0 {
                    continue;
                }
                let x = x0 + w - (n - i) as f32 * slot + 1.0;
                let (y, angle, from, to) = if mirror { (y0, 180.0, palette.low, palette.at(f)) } else { (y0 + hh - ch, 0.0, palette.low, palette.at(f)) };
                let g = gpui::linear_gradient(angle, gpui::linear_color_stop(from, 0.0), gpui::linear_color_stop(to, 1.0));
                let mut q = gpui::fill(Bounds::new(pt(x, y), size(px(bw), px(ch))), g);
                q.corner_radii = gpui::Corners::all(px(1.0));
                window.paint_quad(q);
            }
        },
    )
    .absolute()
    .size_full()
}

/// 图 + 悬浮层:每根竖条上盖一个透明格子,悬停时高亮,并在上方显示 `tips[i]` 和多久以前。
/// `tips` 最后一项是最新的一点;历史不满 `POINTS` 时靠右。
fn graph_box(id: &'static str, tips: Vec<String>, h: f32, canvas: impl IntoElement) -> Div {
    let n = tips.len();
    let mut slots = div().w_full().h(px(h)).flex().flex_row();
    for (i, text) in tips.into_iter().enumerate() {
        let ago = (n - 1 - i) * 2;
        let when = if ago == 0 { "现在".to_string() } else { format!("{ago} 秒前") };
        // 按这一格在整幅图里的真实横向位置决定提示朝哪边展开(历史不满时图靠右)
        let left_half = (POINTS - n.min(POINTS) + i) * 2 < POINTS;
        let g: SharedString = format!("{id}-{i}").into();
        let label = div()
            .absolute()
            .bottom_full()
            .mb(px(4.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .when(left_half, |d| d.left_0())
            .when(!left_half, |d| d.right_0())
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(t::RADIUS_SM))
            .bg(rgb(0x1E1D2E))
            .border_1()
            .border_color(c::dialog_border())
            .shadow_md()
            .whitespace_nowrap()
            .text_size(px(t::TYPE_CAPTION))
            .font_family(MONO)
            .text_color(c::secondary())
            .opacity(0.0)
            .group_hover(g.clone(), |s| s.opacity(1.0))
            .children(text.lines().map(|l| div().child(l.to_string())).collect::<Vec<_>>())
            .child(div().text_color(c::ghost()).child(when));
        slots = slots.child(div().relative().group(g).flex_1().h(px(h)).hover(|s| s.bg(gpui::white().opacity(0.07))).child(label));
    }
    // 格子宽度和竖条一致:总宽 POINTS 个槽位
    div().relative().w_full().h(px(h)).child(canvas).child(div().absolute().top_0().right_0().h(px(h)).w(relative(n as f32 / POINTS as f32)).child(slots))
}

/// 内存竖条图:每点两段,下面已用、上面可用,加起来是总量(缓存和空闲只在悬浮提示和下面的条里)
fn mem_graph(hist: Vec<[f32; 3]>, total: f32, h: f32, tone: Rgba) -> impl IntoElement {
    let samples = hist[hist.len().saturating_sub(POINTS)..].to_vec();
    let cols = mem_colors(tone);
    let paint = samples.clone();
    let canvas = gpui::canvas(
        |_, _, _| (),
        move |b, _, window, _| {
            let (x0, y0) = (f32::from(b.origin.x), f32::from(b.origin.y));
            let (w, hh) = (f32::from(b.size.width), f32::from(b.size.height));
            let slot = w / POINTS as f32;
            let bw = (slot - 2.0).max(1.0);
            let n = paint.len();
            for (i, p) in paint.iter().enumerate() {
                let x = x0 + w - (n - i) as f32 * slot + 1.0;
                // 不含缓存:已用从底往上,剩下的整段就是可用(= 总量 - 已用,包含空闲和可回收的缓存)
                let used_h = (p[0] / total * hh).round().clamp(0.0, hh);
                let avail_h = hh - used_h;
                if used_h >= 1.0 {
                    let mut q = gpui::fill(Bounds::new(pt(x, y0 + hh - used_h), size(px(bw), px(used_h))), cols[0]);
                    q.corner_radii = gpui::Corners::all(px(1.0));
                    window.paint_quad(q);
                }
                if avail_h >= 1.0 {
                    let mut q = gpui::fill(Bounds::new(pt(x, y0), size(px(bw), px(avail_h))), tone.opacity(0.22));
                    q.corner_radii = gpui::Corners::all(px(1.0));
                    window.paint_quad(q);
                }
            }
        },
    )
    .absolute()
    .size_full();
    let tips = samples.iter().map(|p| format!("已用 {}\n缓存 {}\n空闲 {}\n可用 {}", monitor::fmt_bytes(p[0] as f64), monitor::fmt_bytes(p[1] as f64), monitor::fmt_bytes(p[2] as f64), monitor::fmt_bytes((total - p[0]).max(0.0) as f64))).collect();
    graph_box("mem-g", tips, h, canvas)
}

/// 内存三段的颜色:已用 / 缓存 / 空闲(图和下面的进度条共用)

/// 竖条图的配色:一种颜色,底部淡、顶部实
#[derive(Clone, Copy)]
struct Palette {
    low: Rgba,
    high: Rgba,
}

impl Palette {
    fn fixed(c0: Rgba) -> Self {
        Self { low: c0.opacity(0.25), high: c0 }
    }
    fn at(&self, _f: f32) -> Rgba {
        self.high
    }
}

// ---- HUD 样式(token 见 docs/design/tokens.md「概览页 HUD」)--------------------------

const HUD_GRID_STEP: f32 = 28.0;
const HUD_GRID_ALPHA: f32 = 0.035;
const HUD_TILE_TOP: f32 = 0.16;
const HUD_TILE_BOTTOM: f32 = 0.04;
const HUD_BORDER: f32 = 0.34;
const HUD_BORDER_HOT: f32 = 0.5;
const HUD_GLOW: f32 = 0.18;
const HUD_GLOW_BLUR: f32 = 14.0;
const HUD_BRACKET: f32 = 9.0;
/// 占用告警线:CPU/内存/磁盘用量
const WARN_AT: f32 = 75.0;
const DANGER_AT: f32 = 90.0;

fn hsl(base: Rgba, s: f32, l: f32, a: f32) -> gpui::Hsla {
    let h = gpui::Hsla::from(base);
    gpui::hsla(h.h, s, l, a)
}

/// 超过告警线时整张卡换成 warn / danger 色,否则保持自己的色相
fn alert(base: Rgba, v: f32) -> Rgba {
    if v >= DANGER_AT {
        c::danger()
    } else if v >= WARN_AT {
        c::warn()
    } else {
        base
    }
}

/// 色相玻璃块:顶亮底暗的渐变 + 色相描边;悬停时发光并出现四角瞄准框(同空间透镜)
fn tile(id: impl Into<SharedString>, tone: Rgba) -> Stateful<Div> {
    let id: SharedString = id.into();
    let g = gpui::Hsla::from(tone);
    let gid = id.clone();
    let bracket = move |top: bool, left: bool| {
        let d = div().absolute().size(px(HUD_BRACKET)).border_color(hsl(tone, 0.8, 0.72, 0.8)).opacity(0.0).group_hover(gid.clone(), |s| s.opacity(1.0));
        let d = if top { d.top(px(3.0)).border_t_2() } else { d.bottom(px(3.0)).border_b_2() };
        if left { d.left(px(3.0)).border_l_2() } else { d.right(px(3.0)).border_r_2() }
    };
    div()
        .id(id.clone())
        .group(id.clone())
        .relative()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(14.0))
        .border_1()
        .border_color(gpui::hsla(g.h, 0.72, 0.70, HUD_BORDER))
        .bg(gpui::linear_gradient(180.0, gpui::linear_color_stop(gpui::hsla(g.h, 0.7, 0.58, HUD_TILE_TOP), 0.0), gpui::linear_color_stop(gpui::hsla(g.h, 0.6, 0.46, HUD_TILE_BOTTOM), 1.0)))
        .hover(move |s| s.border_color(gpui::hsla(g.h, 0.72, 0.70, HUD_BORDER_HOT)).shadow(vec![gpui::BoxShadow { color: gpui::hsla(g.h, 0.9, 0.65, HUD_GLOW), offset: point(px(0.0), px(0.0)), blur_radius: px(HUD_GLOW_BLUR), spread_radius: px(0.0), inset: false }]))
        .child(bracket(true, true))
        .child(bracket(true, false))
        .child(bracket(false, true))
        .child(bracket(false, false))
}

/// 块标题:色相等宽小标签 + 中文名 + 右侧说明
fn tag(en: &'static str, cn: &'static str, tone: Rgba, right: String) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(div().text_size(px(t::TYPE_MICRO)).font_family(MONO).font_weight(FontWeight::BOLD).text_color(hsl(tone, 0.85, 0.75, 1.0)).child(en))
        .child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(cn))
        .child(div().flex_1())
        .child(div().min_w_0().truncate().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::muted()).child(right))
}

/// 大数字:等宽粗体 + 小单位
fn big(value: String, unit: &'static str, tone: Rgba) -> Div {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(4.0))
        .child(div().text_size(px(t::TYPE_DISPLAY * 1.6)).font_family(MONO).font_weight(FontWeight::BOLD).text_color(tone).child(value))
        .child(div().text_size(px(t::TYPE_CONTROL)).font_family(MONO).text_color(c::muted()).child(unit))
}

fn note(s: String) -> Div {
    div().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::muted()).child(s)
}

fn kv(k: &'static str, v: String, color: Rgba) -> Div {
    div().flex().flex_row().items_baseline().gap(px(5.0)).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(k)).child(div().text_size(px(t::TYPE_CONTROL)).font_family(MONO).font_weight(FontWeight::SEMIBOLD).text_color(color).child(v))
}

fn pct_of(a: u64, b: u64) -> f32 {
    if b == 0 { 0.0 } else { a as f32 / b as f32 * 100.0 }
}

/// 按占比切分的横条:每段 (权重, 颜色),高 `h`
fn segbar(h: f32, segs: Vec<(f32, Rgba)>) -> Div {
    let sum: f32 = segs.iter().map(|(w, _)| w.max(0.0)).sum::<f32>().max(f32::EPSILON);
    div().w_full().h(px(h)).flex().flex_row().gap(px(2.0)).overflow_hidden().children(segs.into_iter().filter(|(w, _)| *w > 0.0).map(|(w, col)| div().h_full().w(relative(w / sum)).min_w(px(2.0)).bg(col)))
}

/// 内存三段的颜色:已用 / 缓存 / 空闲
fn mem_colors(tone: Rgba) -> [Rgba; 3] {
    [tone, tone.opacity(0.4), c::field_border()]
}

/// 四张指标卡统一的内容分区:头部数字区、图、单行脚注(各自固定高度,不换行)
const CARD_HEAD_H: f32 = 52.0;
const CARD_GRAPH_H: f32 = 65.0;
const CARD_FOOT_H: f32 = 18.0;

fn head(el: impl IntoElement) -> Div {
    div().h(px(CARD_HEAD_H)).flex_shrink_0().flex().items_center().child(el)
}

fn foot(el: impl IntoElement) -> Div {
    div().h(px(CARD_FOOT_H)).flex_shrink_0().min_w_0().overflow_hidden().whitespace_nowrap().flex().items_center().child(el)
}

fn cpu_card(m: &MonSnapshot) -> Stateful<Div> {
    let n = &m.now;
    let tone = alert(c::cpu(), n.cpu);
    tile("hud-cpu", tone)
        .child(tag("CPU", "处理器", tone, n.host.cpu_model.clone()))
        .child(head(big(format!("{:.0}", n.cpu), "%", tone)))
        .child(block_graph(m.cpu_h.clone(), ("cpu-g", "CPU", pct), Some(100.0), 100.0, CARD_GRAPH_H, Palette::fixed(tone), false))
        .child(foot(div().flex().flex_row().gap_x(px(16.0)).child(kv("用户", format!("{:.0}%", n.cpu_user), c::text())).child(kv("系统", format!("{:.0}%", n.cpu_sys), c::text())).child(kv("等待", format!("{:.0}%", n.cpu_wait), c::text())).children(n.temp_c.map(|t| kv("温度", format!("{t:.0}°C"), alert(c::text(), t))))))
}

fn mem_card(m: &MonSnapshot) -> Stateful<Div> {
    let n = &m.now;
    let p = pct_of(n.mem_used, n.mem_total);
    let tone = alert(c::mem(), p);
    tile("hud-mem", tone)
        .child(tag("MEM", "内存", tone, format!("共 {}", monitor::fmt_bytes(n.mem_total as f64))))
        .child(head(big(format!("{p:.0}"), "%", tone)))
        .child(mem_graph(m.mem_s.clone(), n.mem_total.max(1) as f32, CARD_GRAPH_H, tone))
        .child(foot(div().flex().flex_row().gap_x(px(16.0)).child(kv("已用", monitor::fmt_bytes(n.mem_used as f64), c::text())).child(kv("可用", monitor::fmt_bytes(n.mem_total.saturating_sub(n.mem_used) as f64), c::text()))))
}

fn disk_card(m: &MonSnapshot) -> Stateful<Div> {
    let n = &m.now;
    let tone = alert(c::disk(), n.disk_util);
    let peak = |h: &Vec<f32>| h.iter().rev().take(POINTS).cloned().fold(0.0, f32::max);
    let (pr, pw) = (peak(&m.rd_h), peak(&m.wr_h));
    let top = pr.max(pw).max(1_048_576.0);
    tile("hud-disk", tone)
        .child(tag("DISK·IO", "磁盘读写", tone, format!("繁忙 {:.0}%", n.disk_util)))
        .child(head(div().flex().flex_row().gap(px(18.0)).child(kv("↑ 读", monitor::fmt_rate(n.disk_r), c::disk())).child(kv("↓ 写", monitor::fmt_rate(n.disk_w), c::cpu()))))
        .child(mirror_graph("io-g", ("↑ 读", &m.rd_h, c::disk()), ("↓ 写", &m.wr_h, c::cpu()), top, (CARD_GRAPH_H - 1.0) / 2.0))
        .child(foot(note(format!("IOPS 读 {:.0} · 写 {:.0}", n.disk_riops, n.disk_wiops))))
}

fn net_card(m: &MonSnapshot) -> Stateful<Div> {
    let n = &m.now;
    let tone = c::net();
    let peak_rx = m.rx_h.iter().rev().take(POINTS).cloned().fold(0.0, f32::max);
    let peak_tx = m.tx_h.iter().rev().take(POINTS).cloned().fold(0.0, f32::max);
    let top = peak_rx.max(peak_tx).max(10_240.0);
    tile("hud-net", tone)
        .child(tag("NET", "网络", tone, format!("峰值 ↓{} ↑{}", monitor::fmt_rate(peak_rx), monitor::fmt_rate(peak_tx))))
        .child(head(div().flex().flex_row().gap(px(18.0)).child(kv("↓ 下载", monitor::fmt_rate(n.net_rx), c::net())).child(kv("↑ 上传", monitor::fmt_rate(n.net_tx), c::mem()))))
        .child(mirror_graph("net-g", ("↓ 下载", &m.rx_h, c::net()), ("↑ 上传", &m.tx_h, c::mem()), top, (CARD_GRAPH_H - 1.0) / 2.0))
        .child(foot(note(format!("负载 {:.2}  {:.2}  {:.2}", n.load[0], n.load[1], n.load[2]))))
}

/// 明细行三张卡的内容区:统一最小高度,内容在里面上下均匀分布
const DETAIL_BODY_H: f32 = 100.0;

fn body() -> Div {
    div().min_h(px(DETAIL_BODY_H)).flex().flex_col().justify_between().gap(px(4.0))
}

/// 核心:8 个以内一核一行(名字 + 占用条 + 数值);更多时换成热力格(亮度 = 占用),格子随核心数变小,底部给最高的核心和过载个数
fn cores_tile(m: &MonSnapshot) -> Stateful<Div> {
    let n = &m.now;
    let tone = c::cpu();
    let cnt = n.cores.len();
    let avg = if cnt == 0 { 0.0 } else { n.cores.iter().sum::<f32>() / cnt as f32 };
    let t0 = tile("hud-cores", tone).child(tag("CORES", "核心", tone, format!("{cnt} 核 · 平均 {avg:.0}%")));
    if cnt <= 8 {
        let mut rows = body().justify_center().gap(px(14.0));
        for (i, v) in n.cores.iter().enumerate() {
            let col = alert(tone, *v);
            rows = rows.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(div().w(px(24.0)).flex_shrink_0().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::muted()).child(format!("C{i}")))
                    .child(div().flex_1().min_w_0().child(segbar(8.0, vec![(*v, col), (100.0 - v, c::field_border())])))
                    .child(div().w(px(40.0)).flex_shrink_0().flex().justify_end().text_size(px(t::TYPE_CONTROL)).font_family(MONO).font_weight(FontWeight::SEMIBOLD).text_color(if *v >= WARN_AT { col } else { c::text() }).child(format!("{v:.0}%"))),
            );
        }
        return t0.child(rows);
    }
    // 格子边长:16 核以内 30(带数字),64 核以内 20,128 核以内 12,再多 8;网格最高 DETAIL_BODY_H-20,超出在卡内滚动
    let cell = if cnt <= 16 { 30.0 } else if cnt <= 64 { 20.0 } else if cnt <= 128 { 12.0 } else { 8.0 };
    let mut grid = div().flex().flex_row().flex_wrap().gap(px(3.0));
    for (i, v) in n.cores.iter().enumerate() {
        let f = (v / 100.0).clamp(0.0, 1.0);
        let col = alert(tone, *v);
        grid = grid.child(
            div()
                .id(("hud-core", i))
                .size(px(cell))
                .flex()
                .items_center()
                .justify_center()
                                .border_1()
                .border_color(col.opacity(0.25 + 0.5 * f))
                .bg(col.opacity(0.08 + 0.7 * f))
                .text_size(px(t::TYPE_MICRO))
                .font_family(MONO)
                .text_color(if f > 0.5 { c::text() } else { c::muted() })
                .when(cell >= 30.0, |d| d.child(format!("{v:.0}"))),
        );
    }
    let (mi, mv) = n.cores.iter().enumerate().fold((0, 0.0_f32), |a, (i, v)| if *v > a.1 { (i, *v) } else { a });
    let hot = n.cores.iter().filter(|v| **v >= WARN_AT).count();
    t0.child(body().child(div().id("hud-cores-grid").max_h(px(DETAIL_BODY_H - 20.0)).overflow_y_scroll().child(grid)).child(note(format!("最高 C{mi} {mv:.0}% · {hot} 个核心 ≥{WARN_AT:.0}%"))))
}

/// 内存构成:一条按占比切开的总条,下面每段一行(色点、名字、占比条、用量),Swap 单独一行
fn mem_tile(m: &MonSnapshot) -> Stateful<Div> {
    let n = &m.now;
    let tone = alert(c::mem(), pct_of(n.mem_used, n.mem_total));
    let tot = n.mem_total.max(1) as f32;
    let mc = mem_colors(tone);
    let row = |name: &'static str, v: u64, total: f32, col: Rgba| {
        let f = (v as f32 / total).clamp(0.0, 1.0);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .child(div().size(px(8.0)).flex_shrink_0().rounded(px(2.0)).bg(col))
            .child(div().w(px(36.0)).flex_shrink_0().text_size(px(t::TYPE_CAPTION)).text_color(c::muted()).child(name))
            .child(div().flex_1().min_w_0().child(segbar(6.0, vec![(f, col), (1.0 - f, c::field_border())])))
            .child(div().w(px(64.0)).flex_shrink_0().flex().justify_end().text_size(px(t::TYPE_CONTROL)).font_family(MONO).font_weight(FontWeight::SEMIBOLD).text_color(c::text()).child(monitor::fmt_bytes(v as f64)))
            .child(div().w(px(36.0)).flex_shrink_0().flex().justify_end().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::muted()).child(format!("{:.0}%", f * 100.0)))
    };
    tile("hud-mem2", tone)
        .child(tag("MEM·MAP", "内存构成", tone, format!("共 {}", monitor::fmt_bytes(n.mem_total as f64))))
        .child(
            body()
                .child(segbar(10.0, vec![(n.mem_used as f32 / tot, mc[0]), (n.mem_cache as f32 / tot, mc[1]), (n.mem_free as f32 / tot, mc[2])]))
                .child(row("已用", n.mem_used, tot, mc[0]))
                .child(row("缓存", n.mem_cache, tot, mc[1]))
                .child(row("空闲", n.mem_free, tot, mc[2]))
                .child(if n.swap_total > 0 {
                    let sp = pct_of(n.swap_used, n.swap_total);
                    row("Swap", n.swap_used, n.swap_total as f32, alert(c::warn(), sp))
                } else {
                    div().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::ghost()).child("Swap 未启用")
                }),
        )
}

/// 磁盘空间:所有挂载点合并成一条总占用
fn disks_tile(m: &MonSnapshot) -> Stateful<Div> {
    let n = &m.now;
    let tone = c::disk();
    let used: u64 = n.disks.iter().map(|d| d.used).sum();
    let total: u64 = n.disks.iter().map(|d| d.total).sum();
    let p = pct_of(used, total);
    let col = alert(tone, p);
    let t0 = tile("hud-disks", col).child(tag("VOLUMES", "磁盘空间", col, format!("{} 个挂载点", n.disks.len())));
    if n.disks.is_empty() {
        return t0.child(note("读不到磁盘用量".into()));
    }
    t0.child(body().child(big(format!("{p:.0}"), "%", col)).child(segbar(8.0, vec![(p, col), (100.0 - p, c::field_border())])).child(note(format!("已用 {} · 可用 {} · 共 {}", monitor::fmt_bytes(used as f64), monitor::fmt_bytes(total.saturating_sub(used) as f64), monitor::fmt_bytes(total as f64)))))
}

impl Home {
    /// 页头副标题行:LIVE 点、主机名 · 系统 · 架构、运行时长
    pub(crate) fn overview_header_info(&self) -> Option<Div> {
        let m = self.dock.mon.as_ref()?;
        let h = &m.now.host;
        let who = [h.name.as_str(), h.os.as_str(), h.arch.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(t::TYPE_MICRO))
                        .font_family(MONO)
                        .font_weight(FontWeight::BOLD)
                        .text_color(c::online())
                                                // 不用循环动画:动画会让窗口每帧(60/120Hz)整页重绘,CPU 白白占 20%;改成跟着 2 秒一次的采样明暗交替
                        .child(div().size(px(6.0)).rounded_full().bg(c::online()).opacity(if (m.now.uptime as u64 / 2) % 2 == 0 { 1.0 } else { 0.35 }))
                        .child("LIVE · 2s"),
                )
                .child(div().min_w_0().truncate().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::secondary()).child(who))
                .child(note(format!("运行 {}", monitor::fmt_uptime(m.now.uptime)))),
        )
    }

    pub(crate) fn overview_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(m) = self.dock.mon.clone() else {
            let Some(msg) = self.dock.mon_err.clone() else { return crate::ui::loading("正在读取系统信息…").into_any_element() };
            return div().flex_1().flex().items_center().justify_center().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(msg).into_any_element();
        };
        // 卡片行:窄了自动换行,每张至少 260 宽
        let row = |kids: Vec<AnyElement>| div().flex().flex_row().flex_wrap().gap(px(10.0)).children(kids.into_iter().map(|k| div().flex_1().min_w(px(260.0)).flex().flex_col().child(k)));
        let cards = row(vec![cpu_card(&m).flex_1().into_any_element(), mem_card(&m).flex_1().into_any_element(), disk_card(&m).flex_1().into_any_element(), net_card(&m).flex_1().into_any_element()]);
        let details = row(vec![cores_tile(&m).flex_1().into_any_element(), mem_tile(&m).flex_1().into_any_element(), disks_tile(&m).flex_1().into_any_element()]);
        let body = div().relative().flex().flex_col().gap(px(10.0)).pb(px(24.0)).child(cards).child(details).child(self.proc_card(&m, cx));
        div()
            .relative()
            .child(
                gpui::canvas(
                    |_, _, _| (),
                    |b, _, window, _| {
                        // 底纹:淡淡的网格(同空间透镜)
                        let line = gpui::hsla(0.0, 0.0, 1.0, HUD_GRID_ALPHA);
                        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
                        let mut x = 0.0;
                        while x <= w {
                            window.paint_quad(gpui::fill(Bounds::new(point(b.origin.x + px(x), b.origin.y), size(px(1.0), px(h))), line));
                            x += HUD_GRID_STEP;
                        }
                        let mut y = 0.0;
                        while y <= h {
                            window.paint_quad(gpui::fill(Bounds::new(point(b.origin.x, b.origin.y + px(y)), size(px(w), px(1.0))), line));
                            y += HUD_GRID_STEP;
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(body)
            .into_any_element()
    }

    /// 进程:按 CPU 或内存排,只读;行内一条色相占比条代替小图
    fn proc_card(&self, m: &MonSnapshot, cx: &mut Context<Self>) -> Stateful<Div> {
        let by_mem = self.dock.proc_by_mem;
        let tone = c::accent();
        let mut procs = m.now.procs.clone();
        if by_mem {
            procs.sort_by(|a, b| b.mem.cmp(&a.mem));
        }
        let procs: Vec<_> = procs.into_iter().take(20).collect();
        let max_cpu = procs.iter().map(|p| p.cpu).fold(1.0_f32, f32::max);
        let max_mem = procs.iter().map(|p| p.mem).max().unwrap_or(1).max(1) as f32;
        let tab = |id: &'static str, label: &'static str, on: bool| {
            div()
                .id(id)
                .px(px(8.0))
                .h(px(20.0))
                .flex()
                .items_center()
                .rounded(px(t::RADIUS_XS))
                .text_size(px(t::TYPE_CAPTION))
                .cursor_pointer()
                .when(on, |d| d.bg(c::selected()).text_color(c::text()))
                .when(!on, |d| d.text_color(c::muted()).hover(|s| s.bg(c::hover())))
                .child(label)
        };
        let tabs = div()
            .flex()
            .flex_row()
            .gap(px(2.0))
            .child(tab("proc-cpu", "CPU", !by_mem).on_click(cx.listener(|this, _, _, cx| {
                this.dock.proc_by_mem = false;
                cx.notify();
            })))
            .child(tab("proc-mem", "内存", by_mem).on_click(cx.listener(|this, _, _, cx| {
                this.dock.proc_by_mem = true;
                cx.notify();
            })));
        // 列:PID | 程序 | 命令(撑满) | 用户 | 内存 | CPU%;窗口窄了先收用户,再收命令
        let w = self.dock.view_w;
        let (show_user, show_cmd) = (w >= 760.0, w >= 600.0);
        let cell = |w: f32, right: bool| {
            let d = div().w(px(w)).flex_shrink_0().truncate();
            if right { d.flex().justify_end() } else { d }
        };
        let head = div()
            .flex()
            .flex_row()
            .gap(px(8.0))
            .px(px(8.0))
            .text_size(px(t::TYPE_CAPTION))
            .font_family(MONO)
            .text_color(c::ghost())
            .child(cell(52.0, false).child("PID"))
            .child(cell(110.0, false).child("程序"))
            .child(div().flex_1().min_w_0().child(if show_cmd { "命令" } else { "" }))
            .when(show_user, |d| d.child(cell(64.0, false).child("用户")))
            .child(cell(64.0, true).child("内存"))
            .child(cell(48.0, true).child("CPU%"));
        let mut col = tile("hud-proc", tone).child(tag("PROCESS", "进程", tone, String::new()).child(tabs)).child(head);
        for p in procs.iter() {
            let idle = p.cpu < 0.05;
            let dim = |col: Rgba| if idle { col.opacity(0.55) } else { col };
            let frac = if by_mem { p.mem as f32 / max_mem } else { p.cpu / max_cpu };
            col = col.child(
                div()
                    .relative()
                    .h(px(22.0))
                    .overflow_hidden()
                    .child(div().absolute().left_0().top_0().bottom_0().w(relative(frac.clamp(0.0, 1.0))).bg(gpui::linear_gradient(90.0, gpui::linear_color_stop(hsl(tone, 0.7, 0.62, 0.0), 0.0), gpui::linear_color_stop(hsl(tone, 0.7, 0.62, 0.22), 1.0))))
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .px(px(8.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(px(t::TYPE_CONTROL))
                            .child(cell(52.0, false).font_family(MONO).text_color(c::ghost()).child(p.pid.to_string()))
                            .child(cell(110.0, false).font_weight(FontWeight::MEDIUM).text_color(dim(c::text())).child(p.name.clone()))
                            .child(div().flex_1().min_w_0().truncate().font_family(MONO).text_size(px(t::TYPE_CAPTION)).text_color(dim(c::muted())).child(if show_cmd { SharedString::from(p.cmd.clone()) } else { SharedString::from("") }))
                            .when(show_user, |d| d.child(cell(64.0, false).text_color(if p.user == "root" { dim(c::warn().opacity(0.8)) } else { dim(c::secondary()) }).child(p.user.clone())))
                            .child(cell(64.0, true).font_family(MONO).text_color(dim(c::secondary())).child(monitor::fmt_bytes(p.mem as f64)))
                            .child(cell(48.0, true).font_family(MONO).font_weight(FontWeight::SEMIBOLD).text_color(dim(c::text())).child(format!("{:.1}", p.cpu))),
                    ),
            );
        }
        col
    }
}

/// 上下对称的双向图:上半 `up`、下半 `down`,共用一层悬浮,悬停时同时显示这一刻的两个值
fn mirror_graph(id: &'static str, up: (&'static str, &Vec<f32>, Rgba), down: (&'static str, &Vec<f32>, Rgba), top: f32, half: f32) -> Div {
    let u = up.1[up.1.len().saturating_sub(POINTS)..].to_vec();
    let d = down.1[down.1.len().saturating_sub(POINTS)..].to_vec();
    let tips = u.iter().zip(d.iter()).map(|(a, b)| format!("{} {}\n{} {}", up.0, monitor::fmt_rate(*a), down.0, monitor::fmt_rate(*b))).collect();
    let canvases = div()
        .absolute()
        .size_full()
        .flex()
        .flex_col()
        .child(div().relative().w_full().h(px(half)).child(block_canvas(u, Some(top), top, Palette::fixed(up.2), false)))
        .child(div().h(px(1.0)))
        .child(div().relative().w_full().h(px(half)).child(block_canvas(d, Some(top), top, Palette::fixed(down.2), true)));
    graph_box(id, tips, half * 2.0 + 1.0, canvases)
}
