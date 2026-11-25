//! 存储页的「空间透镜」:色块图(treemap),看一个目录里各级子目录各占多少。
//!
//! 数据来自 `du` 的流式扫描(`docker::lens_run`),`LensStore` 把一行行结果拼成目录树;
//! 演示节点用 mock 树。设计见 `docs/modules/spacelens.md`。
//! 一个色块是一个目录,面积按大小。顶层色块里铺它的子目录;点一个有子目录的小块,
//! 就在**它自己的空间里**展开它的子目录(别的块不动),可以一直往下点;再点它或它的「‹」收起。

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use ::gpui::{MouseButton, canvas, hsla, relative};

use super::super::*;
use super::dockerpages::{icon_btn, small};
use crate::sysops::{LensEv, fmt_kb};

/// 目录树的一个节点(KiB)
#[derive(Clone)]
pub(crate) struct Node {
    pub name: String,
    /// 完整路径(挂载点是 `/`,其下是 `/var`、`/var/lib`……),也是展开、悬停状态的键
    pub path: String,
    pub kb: u64,
    pub kids: Vec<Node>,
    /// 下面还有没统计到的子目录(点开时再扫)
    pub lazy: bool,
    /// 是文件(不能往下点)
    pub file: bool,
}

const GB: f64 = 1024.0 * 1024.0;

fn n(name: &str, gb: f64, kids: Vec<Node>) -> Node {
    Node { name: name.into(), path: String::new(), kb: (gb * GB) as u64, kids, lazy: false, file: false }
}

/// 外观用的假数据:`/` 的两层是手写的,更深的和没写的目录由 `fill` 按名字生成
pub(crate) fn mock_root() -> Node {
    let mut root = n(
        "/",
        77.1,
        vec![
            n("var", 38.2, vec![n("lib", 30.0, vec![n("docker", 22.4, vec![]), n("postgresql", 5.1, vec![]), n("containerd", 2.0, vec![])]), n("log", 6.1, vec![]), n("cache", 2.1, vec![])]),
            n("home", 14.0, vec![n("chen", 9.8, vec![]), n("deploy", 4.2, vec![])]),
            n("usr", 9.8, vec![n("lib", 5.1, vec![]), n("share", 2.9, vec![]), n("bin", 1.8, vec![])]),
            n("opt", 6.4, vec![n("app", 4.9, vec![]), n("tools", 1.5, vec![])]),
            n("root", 3.2, vec![n(".cache", 2.2, vec![]), n(".npm", 1.0, vec![])]),
            n("snap", 2.0, vec![]),
            n("etc", 0.4, vec![]),
        ],
    );
    fill(&mut root, 0);
    paths(&mut root, "");
    root
}

const POOL: [&str; 10] = ["data", "cache", "lib", "log", "tmp", "backup", "images", "share", "node_modules", "releases"];
const WEIGHT: [f64; 5] = [0.46, 0.24, 0.13, 0.07, 0.04];

/// 没有子目录的节点(够大才有)按名字生成几个:同一个名字永远生成一样的,方便反复点
fn fill(node: &mut Node, depth: usize) {
    if node.kids.is_empty() && node.kb > (0.3 * GB) as u64 && depth < 5 {
        let seed = node.name.bytes().fold(7usize, |a, b| a.wrapping_mul(31).wrapping_add(b as usize)).wrapping_add(depth);
        let count = 3 + seed % 3;
        let sum: f64 = WEIGHT[..count].iter().sum();
        for k in 0..count {
            let name = POOL[(seed / 3 + k * 3) % POOL.len()];
            node.kids.push(Node { name: name.into(), path: String::new(), kb: (node.kb as f64 * 0.93 * WEIGHT[k] / sum) as u64, kids: vec![], lazy: false, file: false });
        }
    }
    for k in &mut node.kids {
        fill(k, depth + 1);
    }
}

/// 给整棵树填上完整路径
fn paths(node: &mut Node, parent: &str) {
    node.path = if parent.is_empty() { "/".into() } else if parent == "/" { format!("/{}", node.name) } else { format!("{parent}/{}", node.name) };
    let me = node.path.clone();
    for k in &mut node.kids {
        paths(k, &me);
    }
}


/// 一个目录最多画这么多个子块,其余并成「其他 N 项」
const MAX_KIDS: usize = 40;
/// 首次扫描往下几层;点开没统计过的小块时补扫几层
pub(crate) const SCAN_DEPTH: u32 = 3;
pub(crate) const LAZY_DEPTH: u32 = 2;

fn parent_path(p: &str) -> Option<String> {
    if p == "/" {
        return None;
    }
    match p.rsplit_once('/') {
        Some(("", _)) => Some("/".into()),
        Some((a, _)) => Some(a.to_string()),
        None => None,
    }
}

fn depth_of(p: &str) -> usize {
    p.split('/').filter(|x| !x.is_empty()).count()
}

/// 一次透镜会话:挂载点、各目录的大小(`du` 一行一行进来)、扫描进度
pub(crate) struct LensStore {
    pub root: String,
    /// 挂载点已用空间(KiB),扫描没出结果前拿它当总量
    pub total: u64,
    sizes: HashMap<String, u64>,
    /// 子目录已经全部统计过的目录
    complete: HashSet<String>,
    /// 是文件的路径
    files: HashSet<String>,
    /// 当前扫描的 (编号, 起点, 层数);没有就是空闲
    pub running: Option<(u64, String, u32)>,
    /// 等前一次扫完再扫的目录
    pub pending: Vec<String>,
    pub err: Option<String>,
    pub seen: usize,
    pub tree: Node,
    demo: bool,
}

impl LensStore {
    pub fn new(root: &str, total: u64) -> Self {
        let tree = Node { name: root.into(), path: root.into(), kb: total, kids: vec![], lazy: false, file: false };
        Self { root: root.into(), total, sizes: HashMap::new(), complete: HashSet::new(), files: HashSet::new(), running: None, pending: vec![], err: None, seen: 0, tree, demo: false }
    }

    pub fn mock(root: &str) -> Self {
        let mut s = Self::new(root, 0);
        s.tree = mock_root();
        s.demo = true;
        s
    }

    pub fn is_demo(&self) -> bool {
        self.demo
    }

    /// 开始一次扫描(清掉上次的错误)
    pub fn begin(&mut self, scan: u64, path: &str, depth: u32) {
        self.running = Some((scan, path.to_string(), depth));
        self.err = None;
    }

    /// 吃一个后台事件;返回扫描是否刚结束
    pub fn apply(&mut self, ev: LensEv) -> bool {
        match ev {
            LensEv::Lines { scan, lines } => {
                let Some((cur, base, depth)) = self.running.clone() else { return false };
                if cur != scan {
                    return false;
                }
                let bd = depth_of(&base);
                for (kb, p, file) in lines {
                    if file {
                        self.files.insert(p.clone());
                    } else if depth_of(&p).saturating_sub(bd) < depth as usize {
                        self.complete.insert(p.clone());
                    }
                    self.sizes.insert(p, kb);
                    self.seen += 1;
                }
                self.rebuild();
                false
            }
            LensEv::Done { scan, err, stopped } => {
                if self.running.as_ref().is_none_or(|r| r.0 != scan) {
                    return false;
                }
                let (_, base, depth) = self.running.take().unwrap();
                if err.is_none() && !stopped {
                    // 起点自己也算统计完(没有子目录的空目录不会再有行)
                    self.complete.insert(base.clone());
                    let _ = depth;
                }
                self.err = err;
                self.rebuild();
                true
            }
        }
    }

    /// 用已有的大小重拼目录树
    fn rebuild(&mut self) {
        let mut kids: HashMap<&str, Vec<(&str, u64)>> = HashMap::new();
        for (p, kb) in &self.sizes {
            if *p == self.root {
                continue;
            }
            if let Some(par) = parent_path(p) {
                // 借用 sizes 里的 key 当父路径的键:父路径要么在 sizes 里,要么就是 root
                let key = if par == self.root { self.root.as_str() } else { match self.sizes.get_key_value(&par) { Some((k, _)) => k.as_str(), None => continue } };
                kids.entry(key).or_default().push((p.as_str(), *kb));
            }
        }
        let scanning = self.running.is_some();
        let root_kb = self.sizes.get(&self.root).copied().unwrap_or_else(|| {
            let sum: u64 = kids.get(self.root.as_str()).map_or(0, |v| v.iter().map(|x| x.1).sum());
            self.total.max(sum)
        });
        let _ = scanning;
        self.tree = self.node(&self.root, root_kb, &kids);
    }

    fn node(&self, path: &str, kb: u64, kids: &HashMap<&str, Vec<(&str, u64)>>) -> Node {
        let name = if path == "/" { "/".to_string() } else { path.rsplit('/').next().unwrap_or(path).to_string() };
        let mut list: Vec<(&str, u64)> = kids.get(path).cloned().unwrap_or_default();
        list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let mut out: Vec<Node> = Vec::new();
        let extra = list.split_off(list.len().min(MAX_KIDS));
        for (p, k) in list {
            out.push(self.node(p, k, kids));
        }
        if !extra.is_empty() {
            let sum: u64 = extra.iter().map(|x| x.1).sum();
            let base = path.trim_end_matches('/');
            out.push(Node { name: format!("其他 {} 项", extra.len()), path: format!("{base}/\u{2026}其他"), kb: sum, kids: vec![], lazy: false, file: false });
        }
        let file = self.files.contains(path);
        let lazy = !file && out.is_empty() && kb > 0 && !self.complete.contains(path) && path != self.root;
        Node { name, path: path.to_string(), kb, kids: out, lazy, file }
    }
}

/// 按完整路径找节点
fn find<'a>(node: &'a Node, path: &str) -> Option<&'a Node> {
    if node.path == path {
        return Some(node);
    }
    node.kids.iter().find(|k| path == k.path || path.starts_with(&format!("{}/", k.path))).and_then(|k| find(k, path))
}

fn is_under(path: &str, ancestor: &str) -> bool {
    path.starts_with(&format!("{}/", ancestor.trim_end_matches('/')))
}

fn parent_of(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(p, _)| p).filter(|p| !p.is_empty())
}

fn uid(path: &str) -> usize {
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    h.finish() as usize
}

/// 第 i 个子目录的色相(度)
const HUES: [f32; 7] = [255.0, 188.0, 152.0, 32.0, 330.0, 212.0, 58.0];

fn hue(i: usize) -> f32 {
    HUES[i % HUES.len()]
}

/// 色块排版时假定的画布像素大小(只用来定比例和判断放不放得下文字,真正的位置用占比)
const RW: f32 = 1000.0;
const RH: f32 = 340.0;
/// 外层色块顶部标题条高度
const HEAD: f32 = 24.0;

/// 方块树图(squarified):按面积把 `sizes` 铺进 w×h,返回每项的 [x, y, w, h],顺序和输入一致
fn squarify(sizes: &[f64], w: f32, h: f32) -> Vec<[f32; 4]> {
    let n = sizes.len();
    let mut out = vec![[0f32; 4]; n];
    let total: f64 = sizes.iter().sum();
    if n == 0 || total <= 0.0 || w <= 0.0 || h <= 0.0 {
        return out;
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| sizes[b].partial_cmp(&sizes[a]).unwrap_or(std::cmp::Ordering::Equal));
    let scale = w as f64 * h as f64 / total;
    let areas: Vec<f64> = order.iter().map(|&k| (sizes[k] * scale).max(1e-6)).collect();
    let (mut x, mut y, mut rw, mut rh) = (0f64, 0f64, w as f64, h as f64);
    let mut i = 0;
    while i < n {
        let side = rw.min(rh);
        let (mut j, mut sum, mut best) = (i, 0f64, f64::MAX);
        while j < n {
            let s2 = sum + areas[j];
            let worst = (side * side * areas[i] / (s2 * s2)).max(s2 * s2 / (side * side * areas[j]));
            if worst > best {
                break;
            }
            best = worst;
            sum = s2;
            j += 1;
        }
        if rw >= rh {
            let cw = sum / rh;
            let mut yy = y;
            for k in i..j {
                let hh = areas[k] / cw;
                out[order[k]] = [x as f32, yy as f32, cw as f32, hh as f32];
                yy += hh;
            }
            x += cw;
            rw -= cw;
        } else {
            let rh_row = sum / rw;
            let mut xx = x;
            for k in i..j {
                let ww = areas[k] / rh_row;
                out[order[k]] = [xx as f32, y as f32, ww as f32, rh_row as f32];
                xx += ww;
            }
            y += rh_row;
            rh -= rh_row;
        }
        i = j;
    }
    out
}

/// 名字里每个字符之间放零宽空格,让长名字能在任意位置换行
fn breakable(name: &str) -> String {
    let mut out = String::with_capacity(name.len() * 4);
    for ch in name.chars() {
        out.push(ch);
        out.push('\u{200B}');
    }
    out
}

fn pct(kb: u64, total: u64) -> String {
    format!("{:.0}%", kb as f32 / total.max(1) as f32 * 100.0)
}

/// 色块的绝对位置:相对父容器的占比
fn place(r: [f32; 4], pw: f32, ph: f32) -> Div {
    div().absolute().left(relative(r[0] / pw)).top(relative(r[1] / ph)).w(relative(r[2] / pw)).h(relative(r[3] / ph))
}

impl Home {
    /// 点一个有子目录的小块:没展开就在它自己的空间里展开,已经展开就收起
    fn lens_toggle(&mut self, path: &str, lazy: bool, cx: &mut Context<Self>) {
        let open = &mut self.dock.sys.lens_open;
        if !open.remove(path) {
            open.insert(path.to_string());
            // 下面还没统计过:补扫两层
            if lazy {
                self.lens_scan_more(path);
            }
        }
        cx.notify();
    }

    /// 双击:把这个目录当顶层放大;没统计过就补扫
    fn lens_zoom_in(&mut self, path: &str, lazy: bool, cx: &mut Context<Self>) {
        self.dock.sys.lens_zoom.push(path.to_string());
        self.dock.sys.lens_hover = None;
        if lazy {
            self.lens_scan_more(path);
        }
        cx.notify();
    }

    /// 返回上一级放大
    fn lens_zoom_out(&mut self, cx: &mut Context<Self>) {
        self.dock.sys.lens_zoom.pop();
        self.dock.sys.lens_hover = None;
        cx.notify();
    }

    /// 补扫一个目录;有扫描在跑就排队
    pub(crate) fn lens_scan_more(&mut self, path: &str) {
        let Some(st) = self.dock.sys.lens.as_mut() else { return };
        if st.is_demo() {
            return;
        }
        if st.running.is_some() {
            if !st.pending.iter().any(|p| p == path) {
                st.pending.push(path.to_string());
            }
            return;
        }
        self.lens_start(path, LAZY_DEPTH);
    }

    /// 发起一次扫描
    pub(crate) fn lens_start(&mut self, path: &str, depth: u32) {
        self.dock.sys.lens_seq += 1;
        let seq = self.dock.sys.lens_seq;
        if let Some(st) = self.dock.sys.lens.as_mut() {
            st.begin(seq, path, depth);
        }
        if let Some(d) = self.sys_docker() {
            d.lens_scan(path, depth, seq);
        }
    }

    /// 取后台事件,扫完一次就接着扫排队的
    pub(crate) fn lens_pump(&mut self) {
        let evs = match self.sys_docker() {
            Some(d) => d.lens_take(),
            None => return,
        };
        let mut finished = false;
        if let Some(st) = self.dock.sys.lens.as_mut() {
            for ev in evs {
                finished |= st.apply(ev);
            }
        }
        if finished {
            let next = self.dock.sys.lens.as_mut().and_then(|st| if st.pending.is_empty() { None } else { Some(st.pending.remove(0)) });
            if let Some(p) = next {
                self.lens_start(&p, LAZY_DEPTH);
            }
        }
    }

    /// 关掉透镜:停掉还在跑的扫描
    pub(crate) fn lens_close(&mut self) {
        if let Some(d) = self.sys_docker() {
            d.lens_stop(self.dock.sys.lens_seq);
        }
        self.dock.sys.lens = None;
        self.dock.sys.lens_hover = None;
        self.dock.sys.lens_open.clear();
        self.dock.sys.lens_zoom.clear();
    }

    /// 鼠标进入/离开一个块。进入时,若已经指着它里面更深的块就不抢;离开时把悬停退回它的上一级(顶层块的上一级是没有)
    fn lens_hover_move(&mut self, path: &str, enter: bool, cx: &mut Context<Self>) {
        let cur = self.dock.sys.lens_hover.clone();
        let want = if enter {
            if cur.as_deref().is_some_and(|c| is_under(c, path)) {
                return;
            }
            Some(path.to_string())
        } else if cur.as_deref().is_some_and(|c| c == path || is_under(c, path)) {
            {
            let top = self.dock.sys.lens_zoom.last().cloned().or_else(|| self.dock.sys.lens.as_ref().map(|l| l.root.clone())).unwrap_or_default();
            parent_of(path).filter(|p| is_under(p, &top)).map(str::to_string)
        }
        } else {
            return;
        };
        if cur != want {
            self.dock.sys.lens_hover = want;
            cx.notify();
        }
    }

    /// 画一个块(`level` 1 是顶层块,≥2 是里面的小块);展开的块里递归画它的子目录。位置 `r` 是父容器里的像素(只用来定比例)。
    fn lens_tile(&self, cx: &mut Context<Self>, node: &Node, r: [f32; 4], pw: f32, ph: f32, level: usize, h: f32) -> AnyElement {
        let sel = self.dock.sys.lens_hover.clone();
        let (bw, bh) = (r[2], r[3]);
        let lit = sel.as_deref() == Some(node.path.as_str());
        let related = sel.as_deref().is_none_or(|s| s == node.path || is_under(s, &node.path) || is_under(&node.path, s));
        let dim = !related;
        let open = level == 1 || self.dock.sys.lens_open.contains(&node.path);
        let has_kids = !node.kids.is_empty() || node.lazy;
        let lazy = node.lazy;
        let is_file = node.file;
        let show_head = open && bh >= HEAD + 14.0 && bw >= 44.0;
        // 小块只放得下名字和大小
        let label = !show_head && bw >= 52.0 && bh >= 20.0;
        let two = bh >= 40.0 && bw >= 70.0;
        // 名字放不下就分行:高度够几行就最多几行(留一行给大小)
        let name_lines = (((bh - 6.0) / 15.0) as usize).saturating_sub(if two { 2 } else { 0 }).max(1);
        let depth = (level.saturating_sub(1)) as f32;
        let pos = node.path.clone();
        let (path_h, path_c) = (node.path.clone(), node.path.clone());

        let mut inner: Vec<AnyElement> = Vec::new();
        if show_head && !node.kids.is_empty() {
            let (iw, ih) = (bw - 8.0, bh - HEAD - 4.0);
            let sizes: Vec<f64> = node.kids.iter().map(|g| g.kb as f64).collect();
            let rects = squarify(&sizes, iw, ih);
            for (k, g) in node.kids.iter().enumerate() {
                if rects[k][2] < 5.0 || rects[k][3] < 5.0 {
                    continue;
                }
                inner.push(self.lens_tile(cx, g, rects[k], iw, ih, level + 1, h));
            }
        }

        let hl = |a: f32| hsla(h, 0.85, 0.88, a);
        place(r, pw, ph)
            .id(("lt", uid(&pos)))
            .p(px(if level == 1 { 2.0 } else { 1.5 }))
            .when(has_kids, |d| d.cursor_pointer())
            .on_hover(cx.listener(move |this, hv: &bool, _, cx| this.lens_hover_move(&path_h, *hv, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                if level > 1 || ev.click_count >= 2 {
                    cx.stop_propagation();
                }
                if !has_kids {
                    return;
                }
                if ev.click_count >= 2 {
                    this.lens_zoom_in(&path_c, lazy, cx);
                } else if level > 1 {
                    this.lens_toggle(&path_c, lazy, cx);
                }
            }))
            .child(
                div()
                    .relative()
                    .size_full()
                    .rounded(px(if level == 1 { t::RADIUS_MD } else { t::RADIUS_SM }))
                    .border_1()
                    .when(is_file, |d| d.border_dashed())
                    .border_color(hsla(h, 0.72, 0.70, if lit { 1.0 } else if dim { 0.14 } else if level == 1 { 0.55 } else if open { 0.7 } else { 0.38 }))
                    .bg(hsla(h, 0.6, 0.5 + 0.03 * depth, if lit { 0.30 + 0.2 * depth.min(1.0) } else if dim { 0.07 } else { 0.16 + 0.07 * depth.min(3.0) }))
                    .overflow_hidden()
                    .when(show_head, |e| {
                        e.child(
                            div()
                                .h(px(HEAD))
                                .px(px(8.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.0))
                                .when(level > 1, |e| e.child(div().flex_shrink_0().text_size(px(t::TYPE_CONTROL)).text_color(hl(1.0)).child("‹")))
                                .child(div().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).font_weight(FontWeight::SEMIBOLD).text_color(hl(if dim { 0.4 } else { 1.0 })).child(SharedString::from(format!("{}/", node.name))))
                                .when(bw >= 110.0, |e| e.child(div().flex_shrink_0().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(hsla(h, 0.4, 0.85, if dim { 0.25 } else { 0.7 })).child(SharedString::from(fmt_kb(node.kb))))),
                        )
                    })
                    .when(show_head && !node.kids.is_empty(), |e| e.child(div().absolute().left(px(4.0)).right(px(4.0)).top(px(HEAD)).bottom(px(0.0)).children(inner)))
                    .when(show_head && node.kids.is_empty() && node.lazy, |e| e.child(div().absolute().left(px(0.0)).right(px(0.0)).top(px(HEAD)).bottom(px(0.0)).flex().items_center().justify_center().child(crate::ui::dot_spinner(("lt-load", uid(&pos)), c::accent()))))
                    .when(label, |e| {
                        e.px(px(6.0)).py(px(3.0)).flex().flex_col().child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(4.0))
                                .when(is_file && bw >= 70.0, |e| e.child(crate::ui::icon(IconName::File, t::TYPE_CAPTION, if dim { c::ghost() } else { c::muted() })))
                                .child(div().min_w_0().line_clamp(name_lines).text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(hl(if dim { 0.35 } else { 1.0 })).child(SharedString::from(breakable(&format!("{}{}", node.name, if has_kids { "/" } else { "" }))))),
                        ).when(two, |e| {
                            e.child(div().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(hsla(h, 0.5, 0.85, if dim { 0.25 } else { 0.7 })).child(SharedString::from(if is_file { format!("文件 · {}", fmt_kb(node.kb)) } else { fmt_kb(node.kb) })))
                        })
                    }),
            )
            .into_any_element()
    }

    /// 存储页下方的透镜面板(目前是 mock)。
    pub(crate) fn lens_panel(&self, cx: &mut Context<Self>) -> Div {
        let Some(store) = self.dock.sys.lens.as_ref() else { return div() };
        let zoom = self.dock.sys.lens_zoom.last().cloned();
        let root = zoom.as_deref().and_then(|p| find(&store.tree, p)).unwrap_or(&store.tree);
        let zoomed = !std::ptr::eq(root, &store.tree);
        let total = root.kb.max(1);
        let scanning = store.running.is_some();
        // 第一批结果还没到:整块画布显示加载
        let initial = scanning && root.kids.is_empty();
        let err = store.err.clone();
        let seen = store.seen;
        let base = if zoomed { root.path.clone() } else { store.root.clone() };
        let base = base.as_str();
        // 画布实际大小(上一帧量到的);第一帧还没量过,先按默认比例排
        let (mw, mh) = self.dock.sys.lens_size.get();
        let (rw, rh) = if mw > 50.0 && mh > 50.0 { (mw, mh) } else { (RW, RH) };

        // 右上角:指着哪块显示哪块的完整路径、大小和占整个挂载点的比例
        let info = self
            .dock
            .sys
            .lens_hover
            .as_deref()
            .and_then(|p| find(root, p))
            .map(|nd| format!("{}{} · {}{} · {}", nd.path.trim_end_matches('/'), if nd.file { "" } else { "/" }, if nd.file { "文件 · " } else { "" }, fmt_kb(nd.kb), pct(nd.kb, total)))
            .unwrap_or_else(|| {
                if let Some(e) = &err {
                    e.clone()
                } else if scanning {
                    format!("{base} 统计中,已读 {seen} 个目录…")
                } else {
                    format!("{base} 共 {} · 点小块就地展开,双击放大", fmt_kb(root.kb))
                }
            });

        // 顶层色块:挂载点下的子目录加「散落文件」一块,铺满画布
        let kids_sum: u64 = root.kids.iter().map(|d| d.kb).sum();
        let rest = if scanning { 0 } else { root.kb.saturating_sub(kids_sum) };
        let mut sizes: Vec<f64> = root.kids.iter().map(|d| d.kb as f64).collect();
        if rest > 0 {
            sizes.push(rest as f64);
        }
        let rects = squarify(&sizes, rw, rh);
        let mut blocks: Vec<AnyElement> = Vec::new();
        for (i, d) in root.kids.iter().enumerate() {
            blocks.push(self.lens_tile(cx, d, rects[i], rw, rh, 1, hue(i) / 360.0));
        }
        if rest > 0 {
            let r = rects[root.kids.len()];
            blocks.push(
                place(r, rw, rh)
                    .p(px(2.0))
                    .child(
                        div()
                            .size_full()
                            .rounded(px(t::RADIUS_MD))
                            .border_1()
                            .border_color(c::line())
                            .bg(hsla(0.0, 0.0, 1.0, 0.03))
                            .px(px(8.0))
                            .py(px(4.0))
                            .when(r[2] >= 60.0 && r[3] >= 24.0, |d| d.child(div().truncate().text_size(px(t::TYPE_CAPTION)).text_color(c::ghost()).child(SharedString::from(format!("散落文件和读不到的部分 · {}", fmt_kb(rest)))))),
                    )
                    .into_any_element(),
            );
        }

        div()
            .flex_grow(1.0)
            .flex_shrink_0()
            .flex()
            .flex_col()
            .rounded(px(t::RADIUS_XL))
            .border_1()
            .border_color(c::dialog_border())
            .overflow_hidden()
            .child(
                div()
                    .h(px(36.0))
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(16.0))
                    .border_b_1()
                    .border_color(c::border())
                    .when(zoomed, |d| d.child(icon_btn(("lens-back", 0), IconName::ArrowLeft, c::muted()).on_click(cx.listener(|this, _, _, cx| this.lens_zoom_out(cx)))))
                    .child(div().text_size(px(t::TYPE_CONTROL)).font_weight(FontWeight::SEMIBOLD).child("空间透镜"))
                    .child(small("只统计登录用户能读到的部分", c::ghost()))
                    .when(scanning, |d| d.child(crate::ui::dot_spinner(("lens-scan", 0), c::accent())))
                    .child(div().flex_1())
                    .child(div().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(c::muted()).child(SharedString::from(info)))
                    .child(icon_btn(("tp-x", 0), IconName::X, c::muted()).on_click(cx.listener(|this, _, _, cx| {
                        this.lens_close();
                        cx.notify();
                    }))),
            )
            // 画布占面板里剩下的全部高度;下面的 canvas 量出实际大小,和排版用的不一样就重排一次
            .child({
                let (cell, home) = (self.dock.sys.lens_size.clone(), cx.entity());
                div().flex_grow(1.0).flex_shrink_0().min_h(px(RH)).relative().child(
                    div()
                        .absolute()
                        .left(px(10.0))
                        .right(px(10.0))
                        .top(px(10.0))
                        .bottom(px(10.0))
                        .child(
                            canvas(
                                |_, _, _| (),
                                move |b, _, _, cx| {
                                    let now = (f32::from(b.size.width), f32::from(b.size.height));
                                    let old = cell.get();
                                    if (now.0 - old.0).abs() > 1.0 || (now.1 - old.1).abs() > 1.0 {
                                        cell.set(now);
                                        home.update(cx, |_, cx| cx.notify());
                                    }
                                },
                            )
                            .absolute()
                            .inset_0(),
                        )
                        .children(blocks)
                        .when(initial, |d| {
                            d.child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .justify_center()
                                    .gap(px(16.0))
                                    .child(crate::ui::dot_spinner_scaled(("lens-loading", 0), c::accent(), 2.5))
                                    .child(div().text_size(px(t::TYPE_CONTROL)).text_color(c::ghost()).child(SharedString::from(format!("正在统计 {base} 的目录占用…")))),
                            )
                        }),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn feed(st: &mut LensStore, scan: u64, lines: &[(u64, &str)]) {
        st.apply(LensEv::Lines { scan, lines: lines.iter().map(|(k, p)| (*k, p.to_string(), false)).collect() });
    }

    #[test]
    fn builds_tree_and_marks_lazy() {
        let mut st = LensStore::new("/", 9000);
        st.begin(1, "/", 3);
        feed(&mut st, 1, &[(100, "/var/lib/a"), (300, "/var/lib"), (50, "/var/log"), (400, "/var"), (10, "/etc")]);
        // 扫描中:根的大小用挂载点已用量
        assert_eq!(st.tree.kb, 9000);
        assert_eq!(st.tree.kids.len(), 2);
        assert_eq!(st.tree.kids[0].path, "/var");
        let var = &st.tree.kids[0];
        assert_eq!(var.kids[0].path, "/var/lib");
        assert_eq!(var.kids[0].kids[0].path, "/var/lib/a");
        // 第 3 层(/var/lib/a)没统计过它的子目录
        assert!(var.kids[0].kids[0].lazy);
        // 第 1、2 层已完整,/etc 是空目录不 lazy
        assert!(!var.lazy && !var.kids[1].lazy && !st.tree.kids[1].lazy);
        feed(&mut st, 1, &[(500, "/")]);
        assert!(st.apply(LensEv::Done { scan: 1, err: None, stopped: false }));
        assert_eq!(st.tree.kb, 500);
        assert!(st.running.is_none());
    }

    #[test]
    fn ignores_stale_scan_and_merges_lazy_scan() {
        let mut st = LensStore::new("/data", 100);
        st.begin(2, "/data", 3);
        feed(&mut st, 1, &[(5, "/data/x")]);
        assert!(st.tree.kids.is_empty());
        feed(&mut st, 2, &[(30, "/data/a/b/c"), (40, "/data/a/b"), (50, "/data/a"), (60, "/data")]);
        st.apply(LensEv::Done { scan: 2, err: None, stopped: false });
        let c = &st.tree.kids[0].kids[0].kids[0];
        assert_eq!(c.path, "/data/a/b/c");
        assert!(c.lazy);
        // 补扫 c
        st.begin(3, "/data/a/b/c", 2);
        feed(&mut st, 3, &[(10, "/data/a/b/c/d"), (30, "/data/a/b/c")]);
        st.apply(LensEv::Done { scan: 3, err: None, stopped: false });
        let c = &st.tree.kids[0].kids[0].kids[0];
        assert_eq!(c.kids[0].path, "/data/a/b/c/d");
    }

    #[test]
    fn marks_files() {
        let mut st = LensStore::new("/", 0);
        st.begin(1, "/", 3);
        st.apply(LensEv::Lines { scan: 1, lines: vec![(900, "/var/big.log".into(), true), (1000, "/var".into(), false)] });
        let f = &st.tree.kids[0].kids[0];
        assert!(f.file && !f.lazy && f.kids.is_empty());
        assert!(!st.tree.kids[0].file);
    }

    #[test]
    fn merges_small_dirs_into_other() {
        let mut st = LensStore::new("/", 0);
        st.begin(1, "/", 1);
        let lines: Vec<(u64, String, bool)> = (0..50).map(|i| (100 - i, format!("/d{i:02}"), false)).collect();
        st.apply(LensEv::Lines { scan: 1, lines });
        assert_eq!(st.tree.kids.len(), MAX_KIDS + 1);
        let last = st.tree.kids.last().unwrap();
        assert_eq!(last.name, "其他 10 项");
        assert!(!last.lazy);
    }
}
