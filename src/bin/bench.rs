//! dockyard 滚动性能原型。
//!
//! 一次性验证用:模拟 DeskPanel 的堆栈页(变高卡片列表),量 GPUI + gpui-kit 的滚动表现。
//! 不是正式产品代码。
//!
//! 用法:
//!   cargo run --release                 交互模式(默认 300 个堆栈)
//!   cargo run --release -- 600          600 个堆栈
//!   cargo run --release -- 300 --bench  自动压测三个场景,打印报告后退出

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::{VirtualListScrollHandle, v_virtual_list};
use gpui_kit::*;

// ---- 尺寸(像素)-------------------------------------------------------------
const HEADER_H: f32 = 64.0;
const ROW_H: f32 = 40.0;
const BODY_PAD: f32 = 8.0;
const GAP: f32 = 10.0;
const SPARK_BARS: usize = 12;

// ---- 数据 --------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq)]
enum Health {
    Running,
    Degraded,
    Exited,
}

impl Health {
    fn color(self) -> Rgba {
        match self {
            Health::Running => rgb(0x3FB950),
            Health::Degraded => rgb(0xD29922),
            Health::Exited => rgb(0xF85149),
        }
    }
}

struct Service {
    name: String,
    image: String,
    health: Health,
    spark: [f32; SPARK_BARS],
}

struct Stack {
    name: String,
    node: String,
    health: Health,
    cpu: f32,
    mem: f32,
    spark: [f32; SPARK_BARS],
    services: Vec<Service>,
    expanded: bool,
}

impl Stack {
    fn height(&self) -> f32 {
        let body = if self.expanded {
            self.services.len() as f32 * ROW_H + BODY_PAD * 2.0
        } else {
            0.0
        };
        HEADER_H + body + GAP
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn f(&mut self) -> f32 {
        (self.next() % 10_000) as f32 / 10_000.0
    }
    fn health(&mut self) -> Health {
        match self.next() % 10 {
            0 => Health::Exited,
            1 | 2 => Health::Degraded,
            _ => Health::Running,
        }
    }
    fn spark(&mut self) -> [f32; SPARK_BARS] {
        let mut s = [0.0; SPARK_BARS];
        for v in &mut s {
            *v = 0.15 + self.f() * 0.85;
        }
        s
    }
}

fn make_stacks(n: usize) -> Vec<Stack> {
    let mut r = Rng(0x9E3779B97F4A7C15);
    (0..n)
        .map(|i| {
            let count = 2 + (r.next() % 7) as usize; // 2..=8 个服务
            Stack {
                name: format!("project-{i:03}"),
                node: format!("node-{:02}", i % 12),
                health: r.health(),
                cpu: r.f() * 100.0,
                mem: r.f() * 100.0,
                spark: r.spark(),
                services: (0..count)
                    .map(|j| Service {
                        name: format!("svc-{j}"),
                        image: format!("registry.local/app/svc-{j}:1.{}", r.next() % 40),
                        health: r.health(),
                        spark: r.spark(),
                    })
                    .collect(),
                expanded: false,
            }
        })
        .collect()
}

// ---- 压测 --------------------------------------------------------------------
#[derive(Clone, Copy)]
enum Mode {
    Collapsed,
    Mixed,
    Expanded,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Collapsed => "全折叠",
            Mode::Mixed => "混合(每 3 个展开 1 个)",
            Mode::Expanded => "全展开",
        }
    }
}

const BENCH_SCENES: [Mode; 3] = [Mode::Collapsed, Mode::Mixed, Mode::Expanded];
const SCENE_SECS: u64 = 5;
/// 滚动速度,像素/秒。约等于用力划触控板。
const SCROLL_SPEED: f32 = 3000.0;

struct Bench {
    scene: usize,
    scene_start: Instant,
    gaps: Vec<f32>,
    render_us: Vec<f32>,
}

// ---- 视图 --------------------------------------------------------------------
struct Yard {
    stacks: Vec<Stack>,
    sizes: Rc<Vec<Size<Pixels>>>,
    scroll: VirtualListScrollHandle,
    autoscroll: bool,
    direction: f32,
    y: f32,
    last_frame: Option<Instant>,
    recent_gaps: Vec<f32>,
    last_render_us: f32,
    bench: Option<Bench>,
}

impl Yard {
    fn new(n: usize, bench: bool) -> Self {
        let mut this = Self {
            stacks: make_stacks(n),
            sizes: Rc::new(vec![]),
            scroll: VirtualListScrollHandle::new(),
            autoscroll: bench,
            direction: 1.0,
            y: 0.0,
            last_frame: None,
            recent_gaps: vec![],
            last_render_us: 0.0,
            bench: bench.then(|| Bench {
                scene: 0,
                scene_start: Instant::now(),
                gaps: vec![],
                render_us: vec![],
            }),
        };
        if bench {
            // 调试用:DY_SCENE=2 从指定场景开始
            if let Some(i) = std::env::var("DY_SCENE").ok().and_then(|v| v.parse().ok()) {
                this.bench.as_mut().unwrap().scene = i;
            }
            let i = this.bench.as_ref().unwrap().scene;
            this.apply_mode(BENCH_SCENES[i]);
        } else {
            this.rebuild_sizes();
        }
        this
    }

    fn rebuild_sizes(&mut self) {
        self.sizes = Rc::new(
            self.stacks
                .iter()
                .map(|s| size(px(800.0), px(s.height())))
                .collect(),
        );
    }

    fn apply_mode(&mut self, mode: Mode) {
        for (i, s) in self.stacks.iter_mut().enumerate() {
            s.expanded = match mode {
                Mode::Collapsed => false,
                Mode::Mixed => i % 3 == 0,
                Mode::Expanded => true,
            };
        }
        self.y = 0.0;
        self.rebuild_sizes();
    }

    fn content_h(&self) -> f32 {
        self.sizes.iter().map(|s| f32::from(s.height)).sum()
    }

    /// 每帧推进滚动并统计帧间隔。返回 false 表示压测结束。
    fn tick(&mut self, window: &Window) -> bool {
        let now = Instant::now();
        if let Some(prev) = self.last_frame {
            let gap = now.duration_since(prev);
            let ms = gap.as_secs_f32() * 1000.0;
            self.recent_gaps.push(ms);
            if self.recent_gaps.len() > 60 {
                self.recent_gaps.remove(0);
            }
            if self.autoscroll {
                let viewport = f32::from(window.viewport_size().height);
                let max = (self.content_h() - viewport).max(0.0);
                self.y += self.direction * SCROLL_SPEED * gap.as_secs_f32();
                if self.y >= max {
                    self.y = max;
                    self.direction = -1.0;
                } else if self.y <= 0.0 {
                    self.y = 0.0;
                    self.direction = 1.0;
                }
                self.scroll.set_offset(point(px(0.0), px(-self.y)));
            }
            if let Some(b) = &mut self.bench {
                b.gaps.push(ms);
            }
        }
        self.last_frame = Some(now);

        if let Some(b) = &self.bench {
            if b.scene_start.elapsed() >= Duration::from_secs(SCENE_SECS) {
                return self.advance_scene();
            }
        }
        true
    }

    fn advance_scene(&mut self) -> bool {
        let n = self.stacks.len();
        let b = self.bench.as_mut().unwrap();
        report(BENCH_SCENES[b.scene], &mut b.gaps, &mut b.render_us, n);
        b.scene += 1;
        if b.scene >= BENCH_SCENES.len() {
            return false;
        }
        b.scene_start = Instant::now();
        let mode = BENCH_SCENES[b.scene];
        self.apply_mode(mode);
        self.last_frame = None;
        true
    }

    fn fps(&self) -> f32 {
        if self.recent_gaps.is_empty() {
            return 0.0;
        }
        let avg = self.recent_gaps.iter().sum::<f32>() / self.recent_gaps.len() as f32;
        1000.0 / avg
    }
}

fn pct(v: &mut [f32], p: f32) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f32 * p) as usize]
}

fn report(mode: Mode, gaps: &mut Vec<f32>, render_us: &mut Vec<f32>, n: usize) {
    // 丢掉前 30 帧预热
    let skip = gaps.len().min(30);
    gaps.drain(..skip);
    let rskip = render_us.len().min(30);
    render_us.drain(..rskip);
    let slow = gaps.iter().filter(|g| **g > 12.5).count();
    println!(
        "[{n} 个堆栈] {:<24} 帧数 {:>4} | 帧间隔 中位 {:>5.2}ms p95 {:>5.2}ms p99 {:>5.2}ms max {:>6.2}ms | 掉帧(>12.5ms) {:>3} | render 中位 {:>5.0}µs p99 {:>5.0}µs",
        mode.label(),
        gaps.len(),
        pct(gaps, 0.5),
        pct(gaps, 0.95),
        pct(gaps, 0.99),
        pct(gaps, 1.0),
        slow,
        pct(render_us, 0.5),
        pct(render_us, 0.99),
    );
    gaps.clear();
    render_us.clear();
}

// ---- 绘制 --------------------------------------------------------------------
fn spark(values: &[f32; SPARK_BARS], color: Rgba, h: f32) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_end()
        .gap(px(2.0))
        .h(px(h))
        .children(
            values
                .iter()
                .map(move |v| div().w(px(5.0)).h(px(h * *v)).rounded(px(1.0)).bg(color)),
        )
}

impl Render for Yard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t0 = Instant::now();
        let keep_going = self.tick(window);
        if !keep_going {
            println!("bench 完成");
            cx.quit();
        }
        if self.autoscroll {
            window.request_animation_frame();
        }

        let fps = self.fps();
        let fps_color = if fps >= 100.0 {
            rgb(0x3FB950)
        } else if fps >= 55.0 {
            rgb(0xD29922)
        } else {
            rgb(0xF85149)
        };
        let auto = self.autoscroll;
        let n = self.stacks.len();
        let render_us = self.last_render_us;
        let view = cx.entity();
        let sizes = self.sizes.clone();

        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .h(px(48.0))
            .px(px(16.0))
            .bg(rgb(0x161B22))
            .border_b_1()
            .border_color(rgb(0x30363D))
            .text_color(rgb(0xC9D1D9))
            .child(div().child(format!("dockyard · {n} 个堆栈")))
            .child(
                div()
                    .text_color(fps_color)
                    .child(format!("{fps:.0} fps · render {render_us:.0}µs")),
            )
            .child(button(
                "auto",
                if auto { "■ 停止自动滚动" } else { "▶ 自动滚动" },
                cx.listener(|this, _, _, cx| {
                    this.autoscroll = !this.autoscroll;
                    this.last_frame = None;
                    cx.notify();
                }),
            ))
            .child(button(
                "expand",
                "全部展开",
                cx.listener(|this, _, _, cx| {
                    this.apply_mode(Mode::Expanded);
                    cx.notify();
                }),
            ))
            .child(button(
                "collapse",
                "全部折叠",
                cx.listener(|this, _, _, cx| {
                    this.apply_mode(Mode::Collapsed);
                    cx.notify();
                }),
            ));

        let list = v_virtual_list(view, "stacks", sizes, |this, range, _window, cx| {
            range
                .map(|ix| stack_card(ix, &this.stacks[ix], cx))
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.scroll)
        .px(px(16.0))
        .pt(px(12.0));

        let out = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x0D1117))
            .child(toolbar)
            .child(div().flex_1().min_h_0().child(list));

        let us = t0.elapsed().as_secs_f32() * 1_000_000.0;
        self.last_render_us = us;
        if let Some(b) = &mut self.bench {
            b.render_us.push(us);
        }
        out
    }
}

fn button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .bg(rgb(0x21262D))
        .hover(|s| s.bg(rgb(0x30363D)))
        .cursor_pointer()
        .child(label)
        .on_click(on_click)
}

fn stack_card(ix: usize, s: &Stack, cx: &mut Context<Yard>) -> Div {
    let header = div()
        .id(("head", ix))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .h(px(HEADER_H))
        .px(px(14.0))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.stacks[ix].expanded = !this.stacks[ix].expanded;
            this.rebuild_sizes();
            cx.notify();
        }))
        .child(div().size(px(10.0)).rounded_full().bg(s.health.color()))
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .child(div().text_color(rgb(0xE6EDF3)).child(s.name.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8B949E))
                        .child(format!("{} · {} 个服务", s.node, s.services.len())),
                ),
        )
        .child(spark(&s.spark, rgb(0x58A6FF), 28.0))
        .child(
            div()
                .w(px(120.0))
                .text_xs()
                .text_color(rgb(0x8B949E))
                .child(format!("CPU {:.0}%  MEM {:.0}%", s.cpu, s.mem)),
        )
        .child(
            div()
                .text_color(rgb(0x8B949E))
                .child(if s.expanded { "▾" } else { "▸" }),
        );

    let mut card = div()
        .w_full()
        .bg(rgb(0x161B22))
        .border_1()
        .border_color(rgb(0x30363D))
        .rounded(px(8.0))
        .child(header);

    if s.expanded {
        card = card.child(
            div()
                .flex()
                .flex_col()
                .py(px(BODY_PAD))
                .border_t_1()
                .border_color(rgb(0x30363D))
                .children(s.services.iter().map(|svc| {
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(12.0))
                        .h(px(ROW_H))
                        .px(px(14.0))
                        .child(div().size(px(8.0)).rounded_full().bg(svc.health.color()))
                        .child(
                            div()
                                .w(px(100.0))
                                .text_color(rgb(0xC9D1D9))
                                .child(svc.name.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(rgb(0x8B949E))
                                .child(svc.image.clone()),
                        )
                        .child(spark(&svc.spark, rgb(0x3FB950), 20.0))
                })),
        );
    }

    // 外层固定为 item_sizes 里声明的高度(含 GAP),卡片本身顶部对齐。
    div().w_full().h(px(s.height())).child(card)
}

fn main() {
    let mut n = 300usize;
    let mut bench = false;
    for a in std::env::args().skip(1) {
        if a == "--bench" {
            bench = true;
        } else if let Ok(v) = a.parse() {
            n = v;
        }
    }

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(WindowOptions::default(), cx, move |_window, cx| {
            cx.new(|_| Yard::new(n, bench))
        })
        .expect("failed to open window");
        cx.activate(true);
    });
}
