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
use crate::sysops::{LensEv, LineKind, fmt_kb};

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
    /// 是另一个文件系统的挂载点(不扫里面,双击进入它自己的透镜)
    pub mount: bool,
    /// 目录名已经知道、大小还在统计(先画的占位卡片)
    pub pending: bool,
}

const GB: f64 = 1024.0 * 1024.0;

fn n(name: &str, gb: f64, kids: Vec<Node>) -> Node {
    Node { name: name.into(), path: String::new(), kb: (gb * GB) as u64, kids, lazy: false, file: false, mount: false, pending: false }
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
            node.kids.push(Node { name: name.into(), path: String::new(), kb: (node.kb as f64 * 0.93 * WEIGHT[k] / sum) as u64, kids: vec![], lazy: false, file: false, mount: false, pending: false });
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
/// 已用量达到这个值的挂载点,点击扫描前要确认(500 GiB)
const BIG_KB: u64 = 500 * 1024 * 1024;
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
    /// 先列出来、大小还没出的子目录
    listed: HashSet<String>,
    /// 根下面直接挂着的其他文件系统:(挂载路径, 已用 KiB)。`du -x` 不进去,大小取自 df
    mounts: Vec<(String, u64)>,
    /// 当前扫描的 (编号, 起点, 层数);没有就是空闲
    pub running: Option<(u64, String, u32)>,
    /// 等前一次扫完再扫的目录
    pub pending: Vec<String>,
    pub err: Option<String>,
    pub seen: usize,
    pub tree: Node,
    demo: bool,
    /// 正在检查访问权限(还没开始扫)
    pub probing: bool,
    /// 登录用户读不了,改用 root(sudo)统计
    pub as_root: bool,
    /// 网络存储(NFS/CIFS…):不额外找大文件,界面提示扫描会慢
    pub network: bool,
}

impl LensStore {
    pub fn new(root: &str, total: u64) -> Self {
        let tree = Node { name: root.into(), path: root.into(), kb: total, kids: vec![], lazy: false, file: false, mount: false, pending: false };
        Self { root: root.into(), total, sizes: HashMap::new(), complete: HashSet::new(), files: HashSet::new(), listed: HashSet::new(), mounts: vec![], running: None, pending: vec![], err: None, seen: 0, tree, demo: false, probing: false, as_root: false, network: false }
    }

    /// 记下根下面直接挂着的文件系统(挂在别的挂载点下面的不算)
    /// 扫描前检查就发现读不了:不扫,直接把原因摆出来
    pub fn blocked(mut self, msg: &str) -> Self {
        self.probing = false;
        self.err = Some(msg.to_string());
        self
    }

    pub fn network(mut self, yes: bool) -> Self {
        self.network = yes;
        self
    }

    pub fn with_mounts(mut self, all: &[(String, u64)]) -> Self {
        let under: Vec<&(String, u64)> = all.iter().filter(|(m, _)| *m != self.root && (self.root == "/" || is_under(m, &self.root)) && m != "/").collect();
        self.mounts = under.iter().filter(|(m, _)| !under.iter().any(|(n, _)| n != m && is_under(m, n))).map(|(m, k)| (m.clone(), *k)).collect();
        self
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
        self.probing = false;
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
                for (kb, p, kind) in lines {
                    if kind == LineKind::Listed {
                        self.listed.insert(p);
                        continue;
                    }
                    if kind == LineKind::File {
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
                // 扫完(或停了)还没出大小的,不再当占位卡片留着
                let sized = &self.sizes;
                self.listed.retain(|p| sized.contains_key(p));
                if err.is_none() && !stopped {
                    // 起点自己也算统计完(没有子目录的空目录不会再有行)
                    self.complete.insert(base.clone());
                    let _ = depth;
                }
                self.err = err;
                self.rebuild();
                // 扫完起点下面一个子目录也没读到:多半是没有读取权限(NFS/CIFS 按用户映射时常见),也可能真是空的
                if self.err.is_none() && !stopped && base == self.root && self.tree.kids.iter().all(|k| k.mount) {
                    self.err = Some("没有读到任何子目录:登录用户可能没有读取权限(网络存储常见),也可能这里本来就是空的".into());
                }
                true
            }
        }
    }

    /// 用已有的大小重拼目录树
    fn rebuild(&mut self) {
        let mount_kb: HashMap<&str, u64> = self.mounts.iter().map(|(m, k)| (m.as_str(), *k)).collect();
        let eff = |p: &str, kb: u64| mount_kb.get(p).copied().unwrap_or(kb);
        let mut kids: HashMap<&str, Vec<(&str, u64)>> = HashMap::new();
        for (p, kb) in &self.sizes {
            if *p == self.root {
                continue;
            }
            if let Some(par) = parent_path(p) {
                // 借用 sizes 里的 key 当父路径的键:父路径要么在 sizes 里,要么就是 root
                let key = if par == self.root { self.root.as_str() } else { match self.sizes.get_key_value(&par) { Some((k, _)) => k.as_str(), None => continue } };
                kids.entry(key).or_default().push((p.as_str(), eff(p, *kb)));
            }
        }
        // du 没列出来的挂载点(比如离扫描起点太深)只在父目录已经出现时补上
        for (m, kb) in &self.mounts {
            if self.sizes.contains_key(m) {
                continue;
            }
            if let Some(par) = parent_path(m) {
                let key = if par == self.root { Some(self.root.as_str()) } else { self.sizes.get_key_value(&par).map(|(k, _)| k.as_str()) };
                if let Some(key) = key {
                    kids.entry(key).or_default().push((m.as_str(), *kb));
                }
            }
        }
        let root_kb = match self.sizes.get(&self.root) {
            Some(k) => *k,
            None => {
                let sum: u64 = kids.get(self.root.as_str()).map_or(0, |v| v.iter().map(|x| x.1).sum());
                self.total.max(sum)
            }
        };
        // 先列出来、大小还没出的子目录:画成占位卡片(父目录得已经在树里)
        let mut pend: HashMap<&str, Vec<&str>> = HashMap::new();
        for p in &self.listed {
            if self.sizes.contains_key(p) || mount_kb.contains_key(p.as_str()) || self.files.contains(p) {
                continue;
            }
            if let Some(par) = parent_path(p) {
                let key = if par == self.root { Some(self.root.as_str()) } else { self.sizes.get_key_value(&par).map(|(k, _)| k.as_str()) };
                if let Some(key) = key {
                    pend.entry(key).or_default().push(p.as_str());
                }
            }
        }
        for v in pend.values_mut() {
            v.sort();
        }
        self.tree = self.node(&self.root, root_kb, &kids, &pend);
    }

    fn node(&self, path: &str, kb: u64, kids: &HashMap<&str, Vec<(&str, u64)>>, pend: &HashMap<&str, Vec<&str>>) -> Node {
        let name = if path == "/" { "/".to_string() } else { path.rsplit('/').next().unwrap_or(path).to_string() };
        let mut list: Vec<(&str, u64)> = kids.get(path).cloned().unwrap_or_default();
        list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let mut out: Vec<Node> = Vec::new();
        let extra = list.split_off(list.len().min(MAX_KIDS));
        for (p, k) in list {
            out.push(self.node(p, k, kids, pend));
        }
        if !extra.is_empty() {
            let sum: u64 = extra.iter().map(|x| x.1).sum();
            let base = path.trim_end_matches('/');
            out.push(Node { name: format!("其他 {} 项", extra.len()), path: format!("{base}/\u{2026}其他"), kb: sum, kids: vec![], lazy: false, file: false, mount: false, pending: false });
        }
        if let Some(ps) = pend.get(path) {
            for p in ps {
                let name = p.rsplit('/').next().unwrap_or(p).to_string();
                out.push(Node { name, path: p.to_string(), kb: 0, kids: vec![], lazy: false, file: false, mount: false, pending: true });
            }
        }
        let file = self.files.contains(path);
        let mount = self.mounts.iter().any(|(m, _)| m == path);
        if mount {
            out.clear();
        }
        let lazy = !file && !mount && out.is_empty() && kb > 0 && !self.complete.contains(path) && path != self.root;
        Node { name, path: path.to_string(), kb, kids: out, lazy, file, mount, pending: false }
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
        // 剩下的空间已经没有了(前面的块把它占完),后面的保持零面积,别死循环
        if side < 1e-6 {
            break;
        }
        let (mut j, mut sum, mut best) = (i, 0f64, f64::MAX);
        while j < n {
            let s2 = sum + areas[j];
            let worst = (side * side * areas[i] / (s2 * s2)).max(s2 * s2 / (side * side * areas[j]));
            // 至少收一个,否则 worst 为 inf/NaN 时 j 不前进,外层原地死循环
            if j > i && worst > best {
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

/// 排版用的面积权重:挂载点是别的文件系统(可能是几十 TB 的 NAS),按真实大小排会把本机的目录挤成一条线,
/// 所以最多占本层本机目录总量的 1/10,最少 1/30,让它看得见、点得到;标出来的大小仍是真实值
fn weights(parent_kb: u64, kids: &[Node]) -> Vec<f64> {
    let local: u64 = kids.iter().filter(|k| !k.mount).map(|k| k.kb).sum::<u64>().max(parent_kb.min(1 << 40)).max(1);
    // 还没出大小的占位卡片按已出大小的同级平均值排,免得一出结果整张图大变样;一个都没出就等大
    let known: Vec<u64> = kids.iter().filter(|k| !k.mount && !k.pending).map(|k| k.kb).collect();
    let avg = if known.is_empty() { 1.0 } else { known.iter().sum::<u64>() as f64 / known.len() as f64 };
    kids.iter().map(|k| if k.pending { avg.max(1.0) } else if k.mount { k.kb.min(local / 10).max(local / 30).max(1) as f64 } else { k.kb as f64 }).collect()
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

/// 名字超过 `max` 个字符就留头和尾(尾巴保住扩展名),中间换成省略号
fn shorten(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max || max < 4 {
        return name.to_string();
    }
    let tail = ((max - 1) / 3).min(10);
    let head = max - 1 - tail;
    format!("{}…{}", chars[..head].iter().collect::<String>(), chars[chars.len() - tail..].iter().collect::<String>())
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

    /// 点了一个挂载点:先在服务器上快速检查登录用户读不读得了(最多 5 秒);读不了就直接说原因,不扫。
    /// 读得了再看要不要确认(网络存储、已用量特别大),否则直接扫。
    pub(crate) fn lens_request(&mut self, path: &str, used_kb: u64, cx: &mut Context<Self>) {
        if self.demo {
            self.lens_begin(path, used_kb);
            cx.notify();
            return;
        }
        let network = matches!(&self.dock.sys.storage, Some(Ok(l)) if l.iter().any(|m| m.mount == path && crate::sysops::is_network_fs(&m.fstype)));
        self.lens_close();
        let mut st = LensStore::new(path, used_kb).network(network);
        st.probing = true;
        self.dock.sys.lens = Some(st);
        self.dock.sys.lens_probe = Some((path.to_string(), used_kb, network, false));
        self.sys_ask(crate::sysops::Kind::Probe, &crate::sysops::probe_script(path), false);
        cx.notify();
    }

    /// 访问检查的结果回来了。登录用户读不了就换 root(sudo)再查一次;root 能读就以 root 统计
    pub(crate) fn lens_probed(&mut self, res: Result<String, String>, cx: &mut Context<Self>) {
        use crate::sysops::{Probe, parse_probe};
        let Some((path, used_kb, network, by_root)) = self.dock.sys.lens_probe.take() else { return };
        // 检查期间用户已经关掉或换了别的
        if !self.dock.sys.lens.as_ref().is_some_and(|l| l.root == path && l.probing) {
            return;
        }
        let probe = res.as_ref().ok().map(|t| parse_probe(t));
        // 登录用户读不了:换 root 再查一次(没有 root 权限就明说)
        if !by_root && probe == Some(Probe::Denied) {
            self.dock.sys.lens_probe = Some((path.clone(), used_kb, network, true));
            self.sys_ask(crate::sysops::Kind::Probe, &crate::sysops::probe_script(&path), true);
            cx.notify();
            return;
        }
        let msg = match (by_root, probe, &res) {
            (true, _, Err(_)) => Some("登录用户没有读取权限,而且没法用 sudo 提权(需要免密 sudo,或在节点里保存了密码)"),
            (true, Some(Probe::Denied), _) => Some("连 root 也读不了这个目录(网络存储常见:服务器端把 root 映射成了普通用户,即 root squash)"),
            (_, Some(Probe::Missing), _) => Some("这个目录不存在,或者不是目录"),
            (_, Some(Probe::Timeout), _) => Some("存储 5 秒内没有响应,可能已断开或网络不通"),
            (false, Some(Probe::Denied), _) => Some("登录用户没有读取这个目录的权限"),
            _ => None,
        };
        if let Some(m) = msg {
            if let Some(st) = self.dock.sys.lens.take() {
                self.dock.sys.lens = Some(st.blocked(m));
            }
            cx.notify();
            return;
        }
        if network || used_kb >= BIG_KB {
            if let Some(st) = self.dock.sys.lens.as_mut() {
                st.as_root = by_root;
            }
            self.dock.sys.lens_ask = Some((path, used_kb, network));
        } else {
            self.lens_begin_as(&path, used_kb, by_root);
        }
        cx.notify();
    }

    /// 大存储扫描前的确认框
    pub(crate) fn lens_confirm_dialog(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        if self.page != Page::Storage {
            return None;
        }
        let (path, kb, network) = self.dock.sys.lens_ask.clone()?;
        let mut points = vec![(IconName::Clock, c::warn(), SharedString::from(if network { "统计要经网络遍历整个共享,目录多的话可能要几分钟到几十分钟,期间会给存储服务器加负担" } else { "统计要遍历里面所有目录,数据量大,可能要几分钟甚至更久" }))];
        let by_root = self.dock.sys.lens.as_ref().is_some_and(|l| l.as_root);
        points.push((IconName::Check, if by_root { c::warn() } else { c::muted() }, if by_root { "登录用户读不了这个目录,将用 root(sudo)统计,只读" } else { "已检查:登录用户能读这个目录" }.into()));
        points.push((IconName::Eye, c::muted(), "只读,不会修改任何文件;结果边读边显示,随时可以点 × 停止".into()));
        if network {
            points.push((IconName::Info, c::muted(), "不会跑额外的大文件查找,只做目录大小统计".into()));
        }
        let spec = crate::confirm::Confirm {
            ids: ["lens-cancel", "lens-ok"],
            title: "统计目录占用",
            tile: Self::icon_tile(IconName::HardDrive, c::accent(), c::accent_soft()).into_any_element(),
            name: path.clone().into(),
            detail: format!("已用 {}{}", fmt_kb(kb), if network { " · 网络存储" } else { "" }).into(),
            points,
            ok_label: "开始统计",
            kind: crate::confirm::ConfirmKind::Action,
            extra: None,
        };
        Some(self.delete_confirm("lens-confirm", spec, |this| this.lens_close(), move |this, cx| {
            this.dock.sys.lens_ask = None;
            let root = this.dock.sys.lens.as_ref().is_some_and(|l| l.as_root);
            this.lens_begin_as(&path, kb, root);
            cx.notify();
        }, cx))
    }

    /// 打开一个挂载点的透镜(从表格点行,或在别的透镜里双击挂载点块)
    pub(crate) fn lens_begin(&mut self, path: &str, used_kb: u64) {
        self.lens_begin_as(path, used_kb, false);
    }

    pub(crate) fn lens_begin_as(&mut self, path: &str, used_kb: u64, as_root: bool) {
        self.lens_close();
        if self.demo {
            self.dock.sys.lens = Some(LensStore::mock(path));
            return;
        }
        let all: Vec<(String, u64)> = match &self.dock.sys.storage {
            Some(Ok(l)) => l.iter().map(|m| (m.mount.clone(), m.used_kb)).collect(),
            _ => vec![],
        };
        let network = matches!(&self.dock.sys.storage, Some(Ok(l)) if l.iter().any(|m| m.mount == path && crate::sysops::is_network_fs(&m.fstype)));
        self.dock.sys.lens = Some({
            let mut st = LensStore::new(path, used_kb).with_mounts(&all).network(network);
            st.as_root = as_root;
            st
        });
        self.lens_start(path, SCAN_DEPTH);
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
        let network = self.dock.sys.lens.as_ref().is_some_and(|l| l.network);
        let as_root = self.dock.sys.lens.as_ref().is_some_and(|l| l.as_root);
        if let Some(d) = self.sys_docker() {
            d.lens_scan(path, depth, seq, !network, as_root);
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
        self.dock.sys.lens_ask = None;
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
        let is_mount = node.mount;
        let is_pending = node.pending;
        let mount_kb = node.kb;
        let show_head = open && bh >= HEAD + 14.0 && bw >= 44.0;
        // 小块只放得下名字和大小
        let label = !show_head && bw >= 52.0 && bh >= 20.0;
        let two = bh >= 40.0 && bw >= 70.0 && !is_pending;
        // 名字放不下就分行:高度够几行就最多几行(留一行给大小)
        let name_lines = (((bh - 14.0) / 15.0) as usize).saturating_sub(if two { 2 } else { 0 }).clamp(1, 3);
        // 一行放得下几个字符(等宽字约 0.62 倍字号);超出 行数×每行字符数 的名字取头尾、中间用省略号
        let icon_w = if (is_file || is_mount) && bw >= 70.0 { t::TYPE_CAPTION + 4.0 } else { 0.0 };
        let per_line = (((bw - 14.0 - icon_w) / (t::TYPE_CAPTION * 0.62)) as usize).max(1);
        let shown_name = shorten(&format!("{}{}", node.name, if has_kids || is_mount { "/" } else { "" }), per_line * name_lines);
        let depth = (level.saturating_sub(1)) as f32;
        let pos = node.path.clone();
        let (path_h, path_c) = (node.path.clone(), node.path.clone());

        let mut inner: Vec<AnyElement> = Vec::new();
        if show_head && !node.kids.is_empty() {
            let (iw, ih) = (bw - 8.0, bh - HEAD - 4.0);
            let sizes = weights(node.kb, &node.kids);
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
            .when(has_kids || is_mount, |d| d.cursor_pointer())
            .on_hover(cx.listener(move |this, hv: &bool, _, cx| this.lens_hover_move(&path_h, *hv, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                if level > 1 || ev.click_count >= 2 {
                    cx.stop_propagation();
                }
                if is_mount {
                    if ev.click_count >= 2 {
                        this.lens_request(&path_c, mount_kb, cx);
                    }
                    return;
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
                    .border_1()
                    .when(is_file, |d| d.border_dashed())
                    .border_color(hsla(h, 0.72, 0.70, if lit { 1.0 } else if dim { 0.28 } else if level == 1 { 0.55 } else if open { 0.7 } else { 0.38 }))
                    .bg({
                        // 顶亮底暗的渐变,像有光从上面打下来
                        let a = if lit { 0.26 + 0.1 * depth.min(1.0) } else if dim { 0.13 } else { 0.16 + 0.07 * depth.min(3.0) };
                        let l = 0.5 + 0.03 * depth;
                        let a = if is_pending { a * 0.55 } else { a };
                        ::gpui::linear_gradient(180.0, ::gpui::linear_color_stop(hsla(h, 0.7, l + 0.08, a + if dim { 0.03 } else { 0.10 }), 0.0), ::gpui::linear_color_stop(hsla(h, 0.6, l - 0.04, (a - 0.04).max(0.03)), 1.0))
                    })
                    .when(lit, |d| d.shadow(vec![::gpui::BoxShadow::new(px(0.0), px(0.0), hsla(h, 0.9, 0.65, 0.4)).blur_radius(px(12.0))]))
                    .overflow_hidden()
                    .when(lit && bw >= 40.0 && bh >= 30.0, |e| {
                        // 瞄准框四角
                        let col = hsla(h, 0.9, 0.82, 1.0);
                        let corner = |top: bool, left: bool| {
                            let d = div().absolute().size(px(9.0)).border_color(col);
                            let d = if top { d.top(px(2.0)).border_t_2() } else { d.bottom(px(2.0)).border_b_2() };
                            if left { d.left(px(2.0)).border_l_2() } else { d.right(px(2.0)).border_r_2() }
                        };
                        e.child(corner(true, true)).child(corner(true, false)).child(corner(false, true)).child(corner(false, false))
                    })
                    .when(show_head, |e| {
                        e.child(
                            div()
                                .h(px(HEAD))
                                .px(px(9.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.0))
                                .when(level > 1, |e| e.child(div().flex_shrink_0().text_size(px(t::TYPE_CONTROL)).text_color(hl(1.0)).child("‹")))
                                .child(div().min_w_0().truncate().text_size(px(t::TYPE_CONTROL)).font_family(MONO).font_weight(FontWeight::SEMIBOLD).text_color(hl(if dim { 0.6 } else { 1.0 })).child(SharedString::from(format!("{}/", node.name))))
                                .when(bw >= 110.0, |e| e.child(div().flex_shrink_0().text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(hsla(h, 0.4, 0.85, if dim { 0.45 } else { 0.7 })).child(SharedString::from(fmt_kb(node.kb))))),
                        )
                    })
                    .when(show_head && !node.kids.is_empty(), |e| e.child(div().absolute().left(px(4.0)).right(px(4.0)).top(px(HEAD)).bottom(px(0.0)).children(inner)))
                    .when(is_pending && bw >= 28.0 && bh >= 28.0, |e| e.child(div().absolute().inset_0().flex().items_center().justify_center().child(crate::ui::dot_spinner(("lt-pend", uid(&pos)), c::accent()))))
                    .when(show_head && node.kids.is_empty() && node.lazy, |e| e.child(div().absolute().left(px(0.0)).right(px(0.0)).top(px(HEAD)).bottom(px(0.0)).flex().items_center().justify_center().child(crate::ui::dot_spinner(("lt-load", uid(&pos)), c::accent()))))
                    .when(label, |e| {
                        e.px(px(7.0)).py(px(7.0)).flex().flex_col().child(
                            div()
                                .flex()
                                .flex_row()
                                .items_start()
                                .gap(px(4.0))
                                .when((is_file || is_mount) && bw >= 70.0, |e| e.child(div().h(px(15.0)).flex_shrink_0().flex().items_center().child(crate::ui::icon(if is_mount { IconName::HardDrive } else { IconName::File }, t::TYPE_CAPTION, hsla(h, 0.85, 0.9, if dim { 0.6 } else { 1.0 }).into()))))
                                .child(div().min_w_0().line_clamp(name_lines).line_height(px(15.0)).text_size(px(t::TYPE_CAPTION)).font_family(MONO).text_color(hl(if dim { 0.6 } else { 1.0 })).child(SharedString::from(breakable(&shown_name)))),
                        ).when(two, |e| {
                            e.child(div().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(hsla(h, 0.5, 0.85, if dim { 0.45 } else { 0.7 })).child(SharedString::from(if is_file { format!("文件·{}", fmt_kb(node.kb)) } else if is_mount { format!("挂载点·{}", fmt_kb(node.kb)) } else { fmt_kb(node.kb) })))
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
        let scanning = store.running.is_some() || store.probing;
        // 第一批结果还没到:整块画布显示加载
        let initial = scanning && root.kids.iter().all(|k| k.mount);
        let err = store.err.clone();
        // 扫完了但没有内容可画(出错或没读到任何子目录):画布中间直接说原因
        let failed = !scanning && err.is_some() && root.kids.iter().all(|k| k.mount);
        let network = store.network;
        let as_root = store.as_root;
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
            .map(|nd| if nd.pending { format!("{}/ · 统计中…", nd.path.trim_end_matches('/')) } else { format!("{}{} · {}{} · {}", nd.path.trim_end_matches('/'), if nd.file { "" } else { "/" }, if nd.file { "文件 · " } else if nd.mount { "挂载点,双击进入 · " } else { "" }, fmt_kb(nd.kb), if nd.mount { "不计入总量".to_string() } else { pct(nd.kb, total) }) })
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
        let kids_sum: u64 = root.kids.iter().filter(|d| !d.mount).map(|d| d.kb).sum();
        let rest = if scanning { 0 } else { root.kb.saturating_sub(kids_sum) };
        let mut sizes = weights(root.kb, &root.kids);
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
                    .child(div().text_size(px(t::TYPE_MICRO)).font_family(MONO).text_color(c::accent()).child("SPACE·LENS"))
                    .child(small(if as_root { "以 root(sudo)统计,只读" } else if network { "网络存储:要遍历整个共享,可能很慢,可随时停止" } else { "只统计登录用户能读到的部分" }, if as_root || network { c::warn() } else { c::ghost() }))
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
                        .inset_0()
                        .child(
                            canvas(
                                |_, _, _| (),
                                move |b, _, window, cx| {
                                    // 底纹:淡淡的网格
                                    let line = hsla(0.0, 0.0, 1.0, 0.035);
                                    let step = 28.0;
                                    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
                                    let mut x = 0.0;
                                    while x <= w {
                                        window.paint_quad(::gpui::fill(::gpui::Bounds::new(::gpui::point(b.origin.x + px(x), b.origin.y), ::gpui::size(px(1.0), px(h))), line));
                                        x += step;
                                    }
                                    let mut y = 0.0;
                                    while y <= h {
                                        window.paint_quad(::gpui::fill(::gpui::Bounds::new(::gpui::point(b.origin.x, b.origin.y + px(y)), ::gpui::size(px(w), px(1.0))), line));
                                        y += step;
                                    }
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
                        .when(failed, |d| {
                            d.child(div().absolute().inset_0().flex().items_center().justify_center().px(px(24.0)).child(div().max_w(px(520.0)).text_size(px(t::TYPE_CONTROL)).text_color(c::muted()).text_center().child(SharedString::from(err.clone().unwrap_or_default()))))
                        })
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
        st.apply(LensEv::Lines { scan, lines: lines.iter().map(|(k, p)| (*k, p.to_string(), LineKind::Dir)).collect() });
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
    fn shortens_long_names() {
        assert_eq!(shorten("abc", 10), "abc");
        let s = shorten("system@00065bf3d50753f4-7c9e9bc2871887e6.journal~", 20);
        assert_eq!(s.chars().count(), 20);
        assert!(s.starts_with("system@") && s.ends_with("journal~".get(2..).unwrap()));
    }

    #[test]
    fn shows_mounts_without_scanning_into_them() {
        let all = vec![("/".to_string(), 500), ("/mnt/mac".to_string(), 4000), ("/mnt/mac/x".to_string(), 7), ("/data".to_string(), 100)];
        let mut st = LensStore::new("/", 500).with_mounts(&all);
        // 只留直接挂在根下的:/mnt/mac 和 /data,/mnt/mac/x 在 /mnt/mac 里面,不算
        assert_eq!(st.mounts.len(), 2);
        st.begin(1, "/", 3);
        // du 把挂载点当成一个空目录列出来(很小)
        st.apply(LensEv::Lines { scan: 1, lines: vec![(4, "/mnt/mac".into(), LineKind::Dir), (8, "/mnt".into(), LineKind::Dir), (50, "/etc".into(), LineKind::Dir)] });
        let mnt = st.tree.kids.iter().find(|k| k.path == "/mnt").unwrap();
        // 上层目录只算本文件系统自己的(挂载点的大小不算进去)
        assert_eq!(mnt.kb, 8);
        let mac = &mnt.kids[0];
        assert!(mac.mount && mac.kids.is_empty() && !mac.lazy);
        assert_eq!(mac.kb, 4000);
        // du 没列出来的 /data 也补上
        let data = st.tree.kids.iter().find(|k| k.path == "/data").unwrap();
        assert!(data.mount && data.kb == 100);
    }

    #[test]
    fn squarify_terminates_with_huge_and_tiny_sizes() {
        // 一个比其余大十几个数量级、其余接近 0 的输入曾让它原地死循环
        let r = squarify(&[7.0e16, 1.0, 0.0, 3.0, 0.0, 0.0], 1000.0, 340.0);
        assert_eq!(r.len(), 6);
        let r = squarify(&[0.0, 0.0, 5.0], 10.0, 10.0);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn listed_dirs_become_pending_cards_then_get_sizes() {
        let mut st = LensStore::new("/", 1000);
        st.begin(1, "/", 3);
        st.apply(LensEv::Lines { scan: 1, lines: vec![(0, "/a".into(), LineKind::Listed), (0, "/b".into(), LineKind::Listed), (0, "/c".into(), LineKind::Listed)] });
        assert_eq!(st.tree.kids.len(), 3);
        assert!(st.tree.kids.iter().all(|k| k.pending && k.kb == 0 && !k.lazy));
        // /a 出了大小:变成真卡片,另外两个仍是占位
        st.apply(LensEv::Lines { scan: 1, lines: vec![(300, "/a".into(), LineKind::Dir)] });
        let a = st.tree.kids.iter().find(|k| k.path == "/a").unwrap();
        assert!(!a.pending && a.kb == 300);
        assert_eq!(st.tree.kids.iter().filter(|k| k.pending).count(), 2);
        // 排版权重:占位卡片按已出大小的平均值
        let w = weights(1000, &st.tree.kids);
        assert!(w.iter().all(|x| *x == 300.0));
        // 扫完还没出大小的(读不到)不再留着
        st.apply(LensEv::Done { scan: 1, err: None, stopped: false });
        assert_eq!(st.tree.kids.len(), 1);
    }

    #[test]
    fn marks_files() {
        let mut st = LensStore::new("/", 0);
        st.begin(1, "/", 3);
        st.apply(LensEv::Lines { scan: 1, lines: vec![(900, "/var/big.log".into(), LineKind::File), (1000, "/var".into(), LineKind::Dir)] });
        let f = &st.tree.kids[0].kids[0];
        assert!(f.file && !f.lazy && f.kids.is_empty());
        assert!(!st.tree.kids[0].file);
    }

    #[test]
    fn merges_small_dirs_into_other() {
        let mut st = LensStore::new("/", 0);
        st.begin(1, "/", 1);
        let lines: Vec<(u64, String, LineKind)> = (0..50).map(|i| (100 - i, format!("/d{i:02}"), LineKind::Dir)).collect();
        st.apply(LensEv::Lines { scan: 1, lines });
        assert_eq!(st.tree.kids.len(), MAX_KIDS + 1);
        let last = st.tree.kids.last().unwrap();
        assert_eq!(last.name, "其他 10 项");
        assert!(!last.lazy);
    }
}
