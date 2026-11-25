//! 概览页:整机资源监控(CPU、内存、磁盘、网络、进程),有 Docker 时再加一块容器。
//! 数据来自 `crate::monitor`(经 `Docker` 连接采样),界面只读 `MonSnapshot`。

use std::time::Duration;

use super::super::*;
use crate::monitor::{self, MonSnapshot};

/// 图里画最近多少个点(2 秒一个,3 分钟)
const POINTS: usize = 90;

fn pt(x: f32, y: f32) -> Point<Pixels> {
    point(px(x), px(y))
}

const GREEN: u32 = 0x46D9A0;
const YELLOW: u32 = 0xF2C94C;
const RED: u32 = 0xFF5C7A;

fn mix(a: u32, b: u32, t: f32) -> Rgba {
    let ch = |s: u32| ((a >> s & 255) as f32 * (1.0 - t) + (b >> s & 255) as f32 * t) as u32;
    rgb(ch(16) << 16 | ch(8) << 8 | ch(0))
}

/// btop 式的占用色阶:0 绿 → 0.6 黄 → 1 红
fn heat(f: f32) -> Rgba {
    let f = f.clamp(0.0, 1.0);
    if f < 0.6 { mix(GREEN, YELLOW, f / 0.6) } else { mix(YELLOW, RED, (f - 0.6) / 0.4) }
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
        let g: SharedString = format!("{id}-{i}").into();
        let label = div()
            .absolute()
            .bottom_full()
            .mb(px(4.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .when(i * 2 < n, |d| d.left_0())
            .when(i * 2 >= n, |d| d.right_0())
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
fn mem_graph(hist: Vec<[f32; 3]>, total: f32, h: f32) -> impl IntoElement {
    let samples = hist[hist.len().saturating_sub(POINTS)..].to_vec();
    let cols = mem_colors();
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
                    let mut q = gpui::fill(Bounds::new(pt(x, y0), size(px(bw), px(avail_h))), c::online().opacity(0.8));
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
fn mem_colors() -> [Rgba; 3] {
    [c::mem(), c::warn().opacity(0.85), c::online().opacity(0.5)]
}

/// 竖条图的配色:`Heat` 随数值绿→黄→红;`Fixed` 用一种颜色的深浅
#[derive(Clone, Copy)]
struct Palette {
    low: Rgba,
    high: Rgba,
    heat: bool,
}

impl Palette {
    fn heat() -> Self {
        Self { low: heat(0.0).opacity(0.55), high: heat(1.0), heat: true }
    }
    fn fixed(c0: Rgba) -> Self {
        Self { low: c0.opacity(0.35), high: c0, heat: false }
    }
    fn at(&self, f: f32) -> Rgba {
        if self.heat { heat(f) } else { self.high }
    }
}

/// 分格进度条:一排小方块,按位置从绿到红,亮到 `frac`,其余暗着;`n` 格
fn cells(frac: f32, n: usize, fixed: Option<Rgba>) -> Div {
    let lit = (frac.clamp(0.0, 1.0) * n as f32).round() as usize;
    div().w_full().h(px(8.0)).flex().flex_row().gap(px(1.0)).children((0..n).map(move |i| {
        let on = i < lit;
        let col = fixed.unwrap_or_else(|| heat(i as f32 / (n - 1).max(1) as f32));
        div().flex_1().h_full().rounded(px(0.5)).bg(if on { col } else { c::field() })
    }))
}

/// 带标题的边框面板:标题是等宽小写,主色;右上角放一小段说明
fn panel(title: &'static str, accent: Rgba, right: Div) -> Div {
    let glow = |c: Rgba, blur: f32| gpui::BoxShadow { color: gpui::Hsla::from(c), offset: gpui::point(px(0.0), px(0.0)), blur_radius: px(blur), spread_radius: px(0.0), inset: false };
    // 不用底色:只在顶上压一条主题色线,标题前一小段发光竖条
    let base = div().min_w_0().flex().flex_col().gap(px(6.0)).px(px(10.0)).py(px(8.0)).rounded(px(t::RADIUS_SM)).border_t_2().border_color(accent.opacity(0.55));
    base.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(div().w(px(3.0)).h(px(12.0)).flex_shrink_0().rounded_full().bg(accent).shadow(vec![glow(accent.opacity(0.9), 6.0)]))
            .child(div().text_size(px(t::TYPE_CONTROL)).font_family(MONO).font_weight(FontWeight::BOLD).text_color(accent).child(title))
            .child(div().flex_1().h(px(1.0)).bg(accent.opacity(0.18)))
            .child(right),
    )
}

/// 面板标题右侧的灰色说明文字(不是标签胶囊)
fn panel_note(s: String) -> Div {
    div().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::muted()).child(s)
}

fn kv(k: &'static str, v: String, color: Rgba) -> Div {
    div().flex().flex_row().items_baseline().gap(px(5.0)).child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(k)).child(div().text_size(px(t::TYPE_CONTROL)).font_family(MONO).font_weight(FontWeight::SEMIBOLD).text_color(color).child(v))
}

fn pct_of(a: u64, b: u64) -> f32 {
    if b == 0 { 0.0 } else { a as f32 / b as f32 * 100.0 }
}

/// 一行「名字  分格条  数值」
fn meter_row(name: String, frac: f32, value: String, fixed: Option<Rgba>, name_w: f32, val_w: f32) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(div().w(px(name_w)).flex_shrink_0().truncate().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::muted()).child(name))
        .child(div().flex_1().min_w_0().child(cells(frac, 56, fixed)))
        .child(div().w(px(val_w)).flex_shrink_0().flex().justify_end().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::secondary()).child(value))
}

/// 核心的迷你柱状图:每根竖条 0.5px、间距 1px(横向尽量密),高度分 9 档,没有网格线
const SPARK_W: f32 = 64.0;
const SPARK_SLOTS: usize = 32;

/// 进程的 CPU 小图:固定 32 列、每列 2px(竖条 1px),历史不满时从右边起;颜色随占用从绿到红
fn proc_spark(hist: Vec<f32>, max: f32, ncores: f32) -> impl IntoElement {
    gpui::canvas(
        |_, _, _| (),
        move |b, _, window, _| {
            let (x0, y0) = (f32::from(b.origin.x), f32::from(b.origin.y));
            let (w, hh) = (f32::from(b.size.width), f32::from(b.size.height));
            let slot = w / SPARK_SLOTS as f32;
            let v = &hist[hist.len().saturating_sub(SPARK_SLOTS)..];
            let n = v.len();
            for (i, val) in v.iter().enumerate() {
                let ch = (hh * val / max).clamp(1.0, hh);
                let x = x0 + w - (n - i) as f32 * slot;
                window.paint_quad(gpui::fill(Bounds::new(pt(x, y0 + hh - ch), size(px((slot - 1.0).max(1.0)), px(ch))), heat(val / 100.0 / ncores * 2.0)));
            }
        },
    )
    .w(px(SPARK_W))
    .flex_shrink_0()
    .h(px(14.0))
}

fn core_canvas(vals: Vec<f32>) -> impl IntoElement {
    gpui::canvas(
        |_, _, _| (),
        move |b, _, window, _| {
            let (x0, y0) = (f32::from(b.origin.x), f32::from(b.origin.y));
            let (w, hh) = (f32::from(b.size.width), f32::from(b.size.height));
            let slots = w.floor().max(1.0) as usize;
            let v = &vals[vals.len().saturating_sub(slots)..];
            let n = v.len();
            let rows = 9.0;
            for (i, val) in v.iter().enumerate() {
                let f = (val / 100.0).clamp(0.0, 1.0);
                let ch = ((f * rows).ceil() / rows * hh).max(if f > 0.0 { hh / rows } else { 1.0 });
                let x = x0 + w - (n - i) as f32;
                window.paint_quad(gpui::fill(Bounds::new(pt(x, y0 + hh - ch), size(px(0.5), px(ch))), heat(f)));
            }
        },
    )
    .absolute()
    .size_full()
}

/// 一个核心一行:名字 + 细密的历史柱状图 + 当前占用
fn core_row(i: usize, now: f32, hist: Vec<f32>) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(div().w(px(26.0)).flex_shrink_0().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::muted()).child(format!("C{i}")))
        .child(div().relative().flex_1().h(px(18.0)).child(core_canvas(hist)))
        .child(div().w(px(34.0)).flex_shrink_0().flex().justify_end().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::secondary()).child(format!("{now:.0}%")))
}

fn cpu_panel(m: &MonSnapshot) -> Div {
    let n = &m.now;
    // 核心多时分列:每列最多 12 个,先排满第一列,放不下再排第二列;
    // 超过 24 个时固定 12 行高度,两列一起上下滚动
    let ncores = n.cores.len();
    let vis = ncores.clamp(1, 12);
    let scroll = ncores > 24;
    let cols = if ncores <= 12 { 1 } else { 2 };
    let per = if scroll { ncores.div_ceil(2) } else { vis };
    let mut list = div().flex().flex_row().gap(px(18.0));
    for chunk in 0..cols {
        let mut col = div().w(px(190.0)).flex().flex_col().gap(px(2.0));
        for i in (chunk * per)..((chunk + 1) * per).min(ncores) {
            col = col.child(core_row(i, n.cores[i], m.cores_h.get(i).cloned().unwrap_or_default()));
        }
        list = list.child(col);
    }
    let cores = if scroll { div().id("cpu-cores").h(px(vis as f32 * 20.0)).overflow_y_scroll().child(list).into_any_element() } else { list.into_any_element() };
    let load = kv("负载", format!("{:.2}  {:.2}  {:.2}", n.load[0], n.load[1], n.load[2]), c::text());
    let head = div().flex().flex_row().items_baseline().gap(px(8.0)).child(div().text_size(px(34.0)).font_family(MONO).font_weight(FontWeight::BOLD).text_color(heat(n.cpu / 100.0)).child(format!("{:.0}%", n.cpu)));
    panel("cpu", c::cpu(), panel_note(n.host.cpu_model.clone()))
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(16.0))
                .child(div().flex_1().min_w_0().flex().flex_col().gap(px(8.0)).child(head).child(block_graph(m.cpu_h.clone(), ("cpu-g","CPU", pct), Some(100.0), 100.0, (vis as f32 * 20.0 - 70.0).max(132.0), Palette::heat(), false)).child(div().flex().flex_row().gap(px(18.0)).child(kv("用户", format!("{:.0}%", n.cpu_user), c::text())).child(kv("系统", format!("{:.0}%", n.cpu_sys), c::text())).child(kv("等待 IO", format!("{:.0}%", n.cpu_wait), c::text())).children(n.temp_c.map(|t| kv("温度", format!("{t:.0}°C"), heat((t - 40.0) / 50.0))))))
                .child(div().flex_shrink_0().flex().flex_col().justify_between().gap(px(10.0)).child(cores).child(load)),
        )
}

fn mem_panel(m: &MonSnapshot) -> Div {
    let n = &m.now;
    let t = n.mem_total.max(1) as f32;
    let free = n.mem_total.saturating_sub(n.mem_used);
    let mc = mem_colors();
    let mut rows = div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .child(meter_row("已用".into(), n.mem_used as f32 / t, monitor::fmt_bytes(n.mem_used as f64), Some(mc[0]), 34.0, 64.0))
        .child(meter_row("缓存".into(), n.mem_cache as f32 / t, monitor::fmt_bytes(n.mem_cache as f64), Some(mc[1]), 34.0, 64.0))
        .child(meter_row("空闲".into(), n.mem_free as f32 / t, monitor::fmt_bytes(n.mem_free as f64), Some(mc[2]), 34.0, 64.0))
        .child(meter_row("可用".into(), free as f32 / t, monitor::fmt_bytes(free as f64), Some(c::online()), 34.0, 64.0));
    if n.swap_total > 0 {
        rows = rows.child(meter_row("Swap".into(), n.swap_used as f32 / n.swap_total as f32, monitor::fmt_bytes(n.swap_used as f64), Some(c::warn().opacity(0.8)), 34.0, 64.0));
    }
    let p = panel("mem", c::mem(), panel_note(format!("共 {}", monitor::fmt_bytes(n.mem_total as f64)))).child(mem_graph(m.mem_s.clone(), n.mem_total.max(1) as f32, 64.0)).child(rows);
    p
}

fn disk_panel(m: &MonSnapshot) -> Div {
    let n = &m.now;
    let mut p = panel("disks", c::disk(), panel_note(format!("IO 繁忙 {:.0}%", n.disk_util)));
    for d in n.disks.iter().take(5) {
        let pct = pct_of(d.used, d.total);
        let free = d.total.saturating_sub(d.used);
        let dev = d.dev.rsplit('/').next().unwrap_or(&d.dev).to_string();
        p = p.child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap(px(8.0))
                        .child(div().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::MEDIUM).text_color(c::text()).child(d.mount.clone()))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::ghost()).child(dev))
                        .child(div().flex_shrink_0().font_family(MONO).font_weight(FontWeight::SEMIBOLD).text_size(px(t::TYPE_CONTROL)).text_color(heat(pct / 100.0)).child(format!("{pct:.0}%"))),
                )
                .child(cells(pct / 100.0, 56, None))
                .child(panel_note(format!("已用 {} · 可用 {} · 共 {}", monitor::fmt_bytes(d.used as f64), monitor::fmt_bytes(free as f64), monitor::fmt_bytes(d.total as f64)))),
        );
    }
    if n.disks.is_empty() {
        p = p.child(panel_note("读不到磁盘用量".into()));
    }
    let peak = |h: &Vec<f32>| h.iter().rev().take(POINTS).cloned().fold(0.0, f32::max);
    let (pr, pw) = (peak(&m.rd_h), peak(&m.wr_h));
    // 上下对称:同一个满刻度,读向上、写向下
    let top = pr.max(pw).max(1_048_576.0);
    let head = |name: &'static str, color: Rgba, now: f32, iops: f32| {
        div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap(px(6.0))
            .child(div().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(name))
            .child(div().text_size(px(t::TYPE_CONTROL)).font_family(MONO).font_weight(FontWeight::SEMIBOLD).text_color(color).child(monitor::fmt_rate(now)))
            .child(panel_note(format!("{iops:.0} IOPS")))
    };
    p.child(div().flex().flex_row().gap(px(24.0)).child(head("↑ 读", c::disk(), n.disk_r, n.disk_riops)).child(head("↓ 写", c::cpu(), n.disk_w, n.disk_wiops)))
        .child(mirror_graph("io-g", ("↑ 读", &m.rd_h, c::disk()), ("↓ 写", &m.wr_h, c::cpu()), top, 30.0))
        .child(panel_note(format!("峰值 读 {}  写 {}", monitor::fmt_rate(pr), monitor::fmt_rate(pw))))
}

fn net_panel(m: &MonSnapshot) -> Div {
    let n = &m.now;
    let peak_rx = m.rx_h.iter().rev().take(POINTS).cloned().fold(0.0, f32::max);
    let peak_tx = m.tx_h.iter().rev().take(POINTS).cloned().fold(0.0, f32::max);
    // 上下对称:同一个满刻度,下载向上、上传向下
    let top = peak_rx.max(peak_tx).max(10_240.0);
    panel("net", c::net(), panel_note(format!("峰值 ↓{} ↑{}", monitor::fmt_rate(peak_rx), monitor::fmt_rate(peak_tx))))
        .child(div().flex().flex_row().gap(px(24.0)).child(kv("↓ 下载", monitor::fmt_rate(n.net_rx), c::net())).child(kv("↑ 上传", monitor::fmt_rate(n.net_tx), c::mem())))
        .child(mirror_graph("net-g", ("↓ 下载", &m.rx_h, c::net()), ("↑ 上传", &m.tx_h, c::mem()), top, 44.0))
}

impl Home {
    pub(crate) fn overview_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(m) = self.dock.mon.clone() else {
            let Some(msg) = self.dock.mon_err.clone() else { return crate::ui::loading("正在读取系统信息…").into_any_element() };
            return div().flex_1().flex().items_center().justify_center().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(msg).into_any_element();
        };
        let h = &m.now.host;
        let chip = |label: String, color: Rgba| div().h(px(22.0)).px(px(10.0)).flex().items_center().rounded(px(11.0)).bg(c::field()).border_1().border_color(c::field_border()).text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(color).child(label);
        let mut top = div().flex_shrink_0().pb(px(8.0)).flex().flex_row().flex_wrap().items_center().gap(px(8.0)).child(
            div()
                .h(px(22.0))
                .px(px(10.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .rounded(px(11.0))
                .bg(c::online_soft())
                .text_size(px(t::TYPE_CAPTION))
                .font_family(MONO)
                .font_weight(FontWeight::BOLD)
                .text_color(c::online())
                .child(
                    div().size(px(6.0)).rounded_full().bg(c::online()).with_animation(
                        "live-dot",
                        Animation::new(Duration::from_millis(1400)).repeat().with_easing(ease_in_out),
                        |d, t| d.opacity(0.35 + 0.65 * (1.0 - (2.0 * t - 1.0).abs())),
                    ),
                )
                .child("LIVE · 2s"),
        );
        for s in [&h.name, &h.os] {
            if !s.is_empty() {
                top = top.child(chip(s.clone(), c::secondary()));
            }
        }
        top = top.child(chip(format!("运行 {}", monitor::fmt_uptime(m.now.uptime)), c::secondary()));

        // CPU 独占一整行;下面左侧叠放内存、网络、磁盘,右侧是进程;容器在进程下面
        let left = div().w(px(340.0)).flex_shrink_0().flex().flex_col().gap(px(8.0)).child(mem_panel(&m)).child(net_panel(&m)).child(disk_panel(&m));
        let mid = div().flex().flex_row().items_start().gap(px(8.0)).child(left).child(div().flex_1().min_w(px(0.0)).flex().flex_col().gap(px(8.0)).child(self.proc_card(&m, cx)));

        div().flex().flex_col().gap(px(8.0)).child(top).child(cpu_panel(&m)).child(mid).into_any_element()
    }

    /// 进程:按 CPU 或内存排,只读
    fn proc_card(&self, m: &MonSnapshot, cx: &mut Context<Self>) -> Div {
        let by_mem = self.dock.proc_by_mem;
        let ncores = m.now.cores.len().max(1) as f32;
        let mut procs = m.now.procs.clone();
        if by_mem {
            procs.sort_by(|a, b| b.mem.cmp(&a.mem));
        }
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
        let right = div()
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
        // 列:PID | 程序 | 命令(撑满) | 线程 | 用户 | 内存 | CPU 小图 | CPU%;窗口窄了先收线程和小图,再收用户,最后收命令
        let w = self.dock.view_w - 350.0;
        let (show_thr, show_spark, show_user, show_cmd) = (w >= 760.0, w >= 760.0, w >= 600.0, w >= 480.0);
        let cell = |w: f32, right: bool| {
            let d = div().w(px(w)).flex_shrink_0().truncate();
            if right { d.flex().justify_end() } else { d }
        };
        let head = div()
            .flex()
            .flex_row()
            .gap(px(8.0))
            .text_size(px(t::TYPE_CAPTION))
            .font_family(MONO)
            .text_color(c::ghost())
            .child(cell(52.0, false).child("PID"))
            .child(cell(96.0, false).child("程序"))
            .child(div().flex_1().min_w_0().child(if show_cmd { "命令" } else { "" }))
            .when(show_thr, |d| d.child(cell(34.0, true).child("线程")))
            .when(show_user, |d| d.child(cell(64.0, false).child("用户")))
            .child(cell(58.0, true).child("内存"))
            .when(show_spark, |d| d.child(cell(SPARK_W, false).child("")))
            .child(cell(44.0, true).child("CPU%"));
        let mut col = panel("proc", c::accent(), right).child(head);
        for p in procs.iter().take(20) {
            let idle = p.cpu < 0.05;
            let dim = |col: Rgba| if idle { col.opacity(0.55) } else { col };
            let f = p.cpu / 100.0 / ncores * 2.0;
            let spark_max = p.hist.iter().cloned().fold(5.0_f32, f32::max);
            let spark = proc_spark(p.hist.clone(), spark_max, ncores);
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(20.0))
                    .text_size(px(t::TYPE_CONTROL))
                    .child(cell(52.0, false).font_family(MONO).text_color(c::ghost()).child(p.pid.to_string()))
                    .child(cell(96.0, false).font_weight(FontWeight::MEDIUM).text_color(dim(c::text())).child(p.name.clone()))
                    .child(div().flex_1().min_w_0().truncate().font_family(MONO).text_size(px(t::TYPE_CAPTION)).text_color(dim(c::muted())).child(if show_cmd { SharedString::from(p.cmd.clone()) } else { SharedString::from("") }))
                    .when(show_thr, |d| d.child(cell(34.0, true).font_family(MONO).text_color(dim(c::muted())).child(if p.threads > 0 { p.threads.to_string() } else { String::new() })))
                    .when(show_user, |d| d.child(cell(64.0, false).text_color(if p.user == "root" { dim(c::warn().opacity(0.8)) } else { dim(c::secondary()) }).child(p.user.clone())))
                    .child(cell(58.0, true).font_family(MONO).text_color(dim(heat(p.mem as f32 / m.now.mem_total.max(1) as f32 * 3.0))).child(monitor::fmt_bytes(p.mem as f64)))
                    .when(show_spark, |d| d.child(spark))
                    .child(cell(44.0, true).font_family(MONO).font_weight(FontWeight::SEMIBOLD).text_color(dim(heat(f))).child(format!("{:.1}", p.cpu))),
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
