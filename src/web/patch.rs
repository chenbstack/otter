//! 就地修改站点配置:手改过的正文不整体重新生成,只把「旧表单渲染 → 新表单渲染」的差异套到原文上。
//! 原文里 Otter 不管理的指令、注释、顺序、缩进都原样保留。
//!
//! 做法:A = 渲染(旧 spec,从原文解析得到),B = 渲染(新 spec),M = 原文。A、B、M 都解析成带位置的指令树,
//! 递归比较 A→B 的差异(同名单值指令=替换,多值指令按序配对,块按「块头」匹配),把增删改翻译成 M 上的文本编辑。
//! 套完再解析一遍,必须和新 spec 一致,否则拒绝(不会写出一个和表单不符的配置)。
//! 设计见 `docs/plans/active/2026-11-26-config-as-truth.md`。

use super::*;

#[derive(Debug)]
pub(super) struct PNode {
    pub(super) words: Vec<String>,
    pub(super) kids: Option<Vec<PNode>>,
    /// 第一个词的起点、结尾(`;` 或 `}` 之后)
    pub(super) start: usize,
    pub(super) end: usize,
    /// `{` 之后的位置(块才有意义)
    body_start: usize,
}

#[derive(Debug)]
enum Tok {
    Word(String, usize),
    Semi(usize),
    Open(usize),
    Close(usize),
}

fn lex(text: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_start = 0usize;
    let mut q: Option<char> = None;
    let mut it = text.char_indices().peekable();
    let flush = |cur: &mut String, start: usize, out: &mut Vec<Tok>| {
        if !cur.is_empty() {
            out.push(Tok::Word(std::mem::take(cur), start));
        }
    };
    while let Some((i, ch)) = it.next() {
        if let Some(qc) = q {
            if ch == qc {
                q = None;
            } else {
                cur.push(ch);
            }
            continue;
        }
        match ch {
            '#' => {
                flush(&mut cur, cur_start, &mut out);
                for (_, n) in it.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            '"' | '\'' => {
                if cur.is_empty() {
                    cur_start = i;
                }
                q = Some(ch);
            }
            ';' => {
                flush(&mut cur, cur_start, &mut out);
                out.push(Tok::Semi(i));
            }
            '{' => {
                flush(&mut cur, cur_start, &mut out);
                out.push(Tok::Open(i));
            }
            '}' => {
                flush(&mut cur, cur_start, &mut out);
                out.push(Tok::Close(i));
            }
            c if c.is_whitespace() => flush(&mut cur, cur_start, &mut out),
            c => {
                if cur.is_empty() {
                    cur_start = i;
                }
                cur.push(c);
            }
        }
    }
    flush(&mut cur, cur_start, &mut out);
    out
}

fn parse_nodes(toks: &[Tok], i: &mut usize) -> Vec<PNode> {
    let mut nodes = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut start = 0usize;
    while *i < toks.len() {
        let t = &toks[*i];
        *i += 1;
        match t {
            Tok::Word(w, at) => {
                if words.is_empty() {
                    start = *at;
                }
                words.push(w.clone());
            }
            Tok::Semi(at) => {
                if !words.is_empty() {
                    nodes.push(PNode { words: std::mem::take(&mut words), kids: None, start, end: at + 1, body_start: 0 });
                }
            }
            Tok::Open(at) => {
                let kids = parse_nodes(toks, i);
                // 子树解析在遇到 `}` 时返回,`}` 是上一个消费的记号
                let end = match toks.get(*i - 1) {
                    Some(Tok::Close(c)) => c + 1,
                    _ => *at + 1,
                };
                nodes.push(PNode { words: std::mem::take(&mut words), kids: Some(kids), start, end, body_start: at + 1 });
            }
            Tok::Close(_) => return nodes,
        }
    }
    nodes
}

pub(super) fn parse_tree(text: &str) -> Vec<PNode> {
    let toks = lex(text);
    let mut i = 0;
    parse_nodes(&toks, &mut i)
}

fn is_blank(s: &str) -> bool {
    s.chars().all(|c| c == ' ' || c == '\t')
}

pub(super) fn line_start(text: &str, pos: usize) -> usize {
    text[..pos].rfind('\n').map(|p| p + 1).unwrap_or(0)
}

pub(super) fn line_end(text: &str, pos: usize) -> usize {
    text[pos..].find('\n').map(|p| pos + p).unwrap_or(text.len())
}

fn indent_of(text: &str, pos: usize) -> String {
    let ls = line_start(text, pos);
    text[ls..pos].chars().take_while(|c| *c == ' ' || *c == '\t').collect()
}

/// `server` 块用「监听端口 + 是否 ssl + server_name」当身份(IPv6/QUIC 的监听行变化不换身份);其它块用块头
fn key_of(n: &PNode) -> String {
    if n.words.first().map(String::as_str) != Some("server") {
        return n.words.join(" ");
    }
    let kids = n.kids.as_deref().unwrap_or(&[]);
    let listen = kids.iter().filter(|k| k.words.first().map(String::as_str) == Some("listen")).find(|k| !k.words.get(1).is_some_and(|a| a.starts_with("[::]")) && !k.words.iter().any(|w| w == "quic"));
    let (port, ssl) = listen.map(|l| (l.words.get(1).cloned().unwrap_or_default(), l.words.iter().any(|w| w == "ssl"))).unwrap_or_default();
    let names = kids.iter().find(|k| k.words.first().map(String::as_str) == Some("server_name")).map(|k| k.words[1..].join(" ")).unwrap_or_default();
    format!("server {port} {ssl} {names}")
}

enum Edit {
    Replace(usize, usize, String),
    Insert(usize, String),
}

struct Ctx<'a> {
    b_text: &'a str,
    m_text: &'a str,
    edits: Vec<(usize, usize, Edit)>,
    seq: usize,
}

impl Ctx<'_> {
    fn push(&mut self, at: usize, e: Edit) {
        self.seq += 1;
        self.edits.push((at, self.seq, e));
    }

    /// 删除一个节点:整行只有它就连行一起删
    fn delete(&mut self, n: &PNode) {
        let t = self.m_text;
        let ls = line_start(t, n.start);
        let le = line_end(t, n.end);
        if is_blank(&t[ls..n.start]) && is_blank(&t[n.end..le]) {
            self.push(ls, Edit::Replace(ls, (le + 1).min(t.len()), String::new()));
        } else {
            self.push(n.start, Edit::Replace(n.start, n.end, String::new()));
        }
    }

    /// B 里这个节点的文本(去掉它自己的缩进,多行按相对缩进保留)
    fn b_block(&self, n: &PNode) -> String {
        let t = self.b_text;
        let ind = indent_of(t, n.start).len();
        let ls = line_start(t, n.start);
        let _ = ls;
        let raw = &t[n.start..n.end];
        raw.lines().enumerate().map(|(i, l)| if i == 0 { l.to_string() } else { l.strip_prefix(&" ".repeat(ind)).unwrap_or(l.trim_start()).to_string() }).collect::<Vec<_>>().join("\n")
    }

    fn insert_after(&mut self, anchor: Option<&PNode>, parent: &Parent, text: &str) {
        let t = self.m_text;
        let (pos, ind) = match anchor {
            Some(a) => (line_end(t, a.end), indent_of(t, a.start)),
            None => (line_end(t, parent.body_start), parent.indent.clone()),
        };
        let body: String = text.lines().map(|l| format!("{ind}{l}")).collect::<Vec<_>>().join("\n");
        self.push(pos, Edit::Insert(pos, format!("\n{body}")));
    }
}

struct Parent {
    /// 新增子节点用的缩进
    indent: String,
    /// 没有锚点时,插在这个位置所在行的末尾
    body_start: usize,
}

fn diff(cx: &mut Ctx, a: &[PNode], b: &[PNode], m: &[PNode], parent: &Parent) -> Result<(), String> {
    let is_leaf = |n: &PNode| n.kids.is_none();
    let mut used = vec![false; m.len()];
    // M 里找第一个还没用过、满足条件的节点
    let find = |used: &mut Vec<bool>, pred: &dyn Fn(&PNode) -> bool| -> Option<usize> {
        let i = m.iter().enumerate().position(|(i, n)| !used[i] && pred(n))?;
        used[i] = true;
        Some(i)
    };

    // 叶子指令:A、B 各自按名字分组,相同的跳过;不同的按序配对替换,多出的删/增
    let names: Vec<&str> = {
        let mut v: Vec<&str> = Vec::new();
        for n in a.iter().chain(b).filter(|n| is_leaf(n)) {
            let k = n.words[0].as_str();
            if !v.contains(&k) {
                v.push(k);
            }
        }
        v
    };
    // 每个 B 叶子的处置
    enum Act {
        Keep(Option<usize>),
        Replace(usize),
        Add,
    }
    let b_leaf_idx: Vec<usize> = (0..b.len()).filter(|i| is_leaf(&b[*i])).collect();
    let mut acts: std::collections::HashMap<usize, Act> = std::collections::HashMap::new();
    for name in names {
        let al: Vec<&PNode> = a.iter().filter(|n| is_leaf(n) && n.words[0] == name).collect();
        let bl: Vec<(usize, &PNode)> = b_leaf_idx.iter().map(|i| (*i, &b[*i])).filter(|(_, n)| n.words[0] == name).collect();
        let mut a_left: Vec<&PNode> = al.clone();
        let mut added: Vec<(usize, &PNode)> = Vec::new();
        for (bi, bn) in &bl {
            match a_left.iter().position(|x| x.words == bn.words) {
                Some(p) => {
                    a_left.remove(p);
                    let mi = find(&mut used, &|n| is_leaf(n) && n.words == bn.words);
                    acts.insert(*bi, Act::Keep(mi));
                }
                None => added.push((*bi, bn)),
            }
        }
        // a_left = 被删除的,added = 新增的:按序配对替换
        let mut a_iter = a_left.into_iter();
        for (bi, _) in &added {
            match a_iter.next() {
                Some(old) => match find(&mut used, &|n| is_leaf(n) && n.words == old.words) {
                    Some(mi) => {
                        acts.insert(*bi, Act::Replace(mi));
                    }
                    None => {
                        acts.insert(*bi, Act::Add);
                    }
                },
                None => {
                    acts.insert(*bi, Act::Add);
                }
            }
        }
        for old in a_iter {
            if let Some(mi) = find(&mut used, &|n| is_leaf(n) && n.words == old.words) {
                cx.delete(&m[mi]);
            }
        }
    }

    // 按 B 的顺序走一遍:保留的、替换的、新增的(新增插在前一个保留/替换的 M 节点后面)
    let mut anchor: Option<usize> = None;
    let mut b_used_m = vec![false; m.len()];
    for (bi, bn) in b.iter().enumerate() {
        if is_leaf(bn) {
            match acts.get(&bi) {
                Some(Act::Keep(mi)) => {
                    if mi.is_some() {
                        anchor = *mi;
                    }
                }
                Some(Act::Replace(mi)) => {
                    let txt = cx.b_block(bn);
                    cx.push(m[*mi].start, Edit::Replace(m[*mi].start, m[*mi].end, txt));
                    b_used_m[*mi] = true;
                    anchor = Some(*mi);
                }
                Some(Act::Add) | None => {
                    let txt = cx.b_block(bn);
                    cx.insert_after(anchor.map(|i| &m[i]), parent, &txt);
                }
            }
        } else {
            let key = key_of(bn);
            let a_blk = a.iter().find(|x| x.kids.is_some() && key_of(x) == key);
            let m_idx = (0..m.len()).find(|i| !b_used_m[*i] && m[*i].kids.is_some() && key_of(&m[*i]) == key);
            match (a_blk, m_idx) {
                (Some(ab), Some(mi)) => {
                    b_used_m[mi] = true;
                    let mn = &m[mi];
                    let sub = Parent { indent: format!("{}    ", indent_of(cx.m_text, mn.start)), body_start: mn.body_start };
                    diff(cx, ab.kids.as_deref().unwrap_or(&[]), bn.kids.as_deref().unwrap_or(&[]), mn.kids.as_deref().unwrap_or(&[]), &sub)?;
                    anchor = Some(mi);
                }
                (Some(_), None) => {} // 原文里这个块被手工删掉了:不补回来
                (None, _) => {
                    let txt = cx.b_block(bn);
                    cx.insert_after(anchor.map(|i| &m[i]), parent, &txt);
                }
            }
        }
    }
    // A 里有、B 里没有的块:从原文删掉
    for ab in a.iter().filter(|x| x.kids.is_some()) {
        let key = key_of(ab);
        if !b.iter().any(|x| x.kids.is_some() && key_of(x) == key)
            && let Some(mi) = (0..m.len()).find(|i| m[*i].kids.is_some() && key_of(&m[*i]) == key && !b_used_m[*i])
        {
            b_used_m[mi] = true;
            cx.delete(&m[mi]);
        }
    }
    Ok(())
}

fn apply(text: &str, mut edits: Vec<(usize, usize, Edit)>) -> String {
    edits.sort_by(|x, y| (y.0, y.1).cmp(&(x.0, x.1)));
    let mut out = text.to_string();
    for (_, _, e) in edits {
        match e {
            Edit::Replace(s, e, t) => out.replace_range(s..e, &t),
            Edit::Insert(at, t) => out.insert_str(at, &t),
        }
    }
    out
}

fn same_spec(a: &SiteSpec, b: &SiteSpec) -> bool {
    let norm = |s: &SiteSpec| {
        let mut s = s.clone();
        if let Some(au) = &mut s.adv.auth {
            for u in &mut au.users {
                u.pass = None;
            }
        }
        s
    };
    norm(a) == norm(b)
}

/// 把 `old`(站点现有的配置正文)按 `new` 就地修改。返回新正文;不能保证和表单一致时返回 Err
pub fn patch_text(old: &str, new: &SiteSpec, t: &Target) -> Result<String, String> {
    let parsed = conf::parse_conf(old, &new.name, Some(new)).ok_or("配置里有 Otter 读不懂的结构")?;
    let (a_text, b_text) = (render_site(&parsed.spec, t), render_site(new, t));
    let (a, b, m) = (parse_tree(&a_text), parse_tree(&b_text), parse_tree(old));
    // 顶层没有锚点时插在 otter:hash 行之后
    let top_pos = old.find("# otter:hash").map(|p| line_end(old, p)).unwrap_or(0);
    let top = Parent { indent: String::new(), body_start: top_pos };
    let mut cx = Ctx { b_text: &b_text, m_text: old, edits: Vec::new(), seq: 0 };
    diff(&mut cx, &a, &b, &m, &top)?;
    if cx.edits.is_empty() {
        return Ok(old.to_string());
    }
    let mut out = apply(old, std::mem::take(&mut cx.edits));
    // 头部和校验值换成新 spec 的(校验值是生成正文的,有手加内容时本来就对不上,继续被识别为「手改过」)
    for key in ["# otter:site v", "# otter:hash "] {
        let new_line = b_text.lines().find(|l| l.starts_with(key)).ok_or("渲染结果缺少头部")?;
        let at = out.find(key).ok_or("原文缺少 Otter 头部")?;
        let le = line_end(&out, at);
        out.replace_range(at..le, new_line);
    }
    // 保险:改完重新读,必须和表单一致
    match conf::parse_conf(&out, &new.name, Some(new)) {
        Some(p) if same_spec(&p.spec, new) => Ok(out),
        _ => Err("就地修改后的配置和表单对不上,已放弃".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SiteSpec {
        let l = |d: &[&str], port: u16, cert: Option<&str>, force: bool| Listener { domains: d.iter().map(|x| x.to_string()).collect(), port, cert: cert.map(str::to_string), force_https: force };
        SiteSpec {
            name: "app".into(),
            kind: SiteKind::Proxy { locations: vec![Location::new("/", Backend::Url("http://127.0.0.1:3000".into()))], upstreams: vec![] },
            listeners: vec![l(&["a.com"], 80, None, true), l(&["a.com"], 443, Some("a.com"), false)],
            ipv6: false,
            hsts: true,
            adv: Advanced::default(),
        }
    }

    /// 手改过:加了不支持的指令和注释,再用表单改别的字段,手改要原样留下
    #[test]
    fn keeps_hand_edits_and_applies_form_changes() {
        let spec = base();
        let t = Target::own();
        let text = render_site(&spec, &t);
        let hand = text.replace("    location / {\n        proxy_pass", "    # 我的注释\n    proxy_buffering off;\n    location / {\n        proxy_pass");
        assert_ne!(hand, text);
        let mut new = spec.clone();
        new.adv.gzip = Some(true);
        new.adv.max_body = Some("10m".into());
        new.adv.headers = vec![("X-A".into(), "1".into())];
        new.adv.tls_protocols = Some(vec!["TLSv1.3".into()]);
        new.adv.read_timeout = Some(90);
        let out = patch_text(&hand, &new, &t).unwrap();
        assert!(out.contains("# 我的注释") && out.contains("proxy_buffering off;"), "{out}");
        assert!(out.contains("gzip on;") && out.contains("client_max_body_size 10m;") && out.contains("add_header X-A \"1\" always;") && out.contains("ssl_protocols TLSv1.3;") && out.contains("proxy_read_timeout 90s;"), "{out}");
        assert!(!out.contains("proxy_read_timeout 300s;"));
        // 读回 = 新表单,手改的仍被识别为「不管理」
        let p = conf::parse_conf(&out, "app", Some(&new)).unwrap();
        assert_eq!(p.spec.adv.gzip, Some(true));
        assert!(p.unknown.iter().any(|u| u.contains("proxy_buffering off")));
    }

    #[test]
    fn removes_and_replaces_in_place() {
        let mut spec = base();
        spec.adv = Advanced { gzip: Some(true), max_body: Some("5m".into()), headers: vec![("X-A".into(), "1".into()), ("X-B".into(), "2".into())], ..Default::default() };
        let t = Target::own();
        let text = render_site(&spec, &t).replace("    gzip on;\n", "    gzip on; # 压缩\n");
        // 改超时、改一个头、删另一个头、关 gzip
        let mut new = spec.clone();
        new.adv.max_body = Some("20m".into());
        new.adv.headers = vec![("X-A".into(), "changed".into())];
        new.adv.gzip = None;
        let out = patch_text(&text, &new, &t).unwrap();
        assert!(out.contains("client_max_body_size 20m;") && !out.contains("5m"));
        assert!(out.contains("add_header X-A \"changed\" always;") && !out.contains("X-B"));
        assert!(!out.contains("gzip"), "{out}");
    }

    #[test]
    fn structural_changes_listener_and_location() {
        let spec = base();
        let t = Target::own();
        let text = render_site(&spec, &t).replace("    server_name a.com;\n    ssl_certificate", "    server_name a.com;\n    keepalive_requests 50;\n    ssl_certificate");
        let mut new = spec.clone();
        new.ipv6 = true;
        new.kind = SiteKind::Proxy {
            locations: vec![Location::new("/", Backend::Url("http://127.0.0.1:3000".into())), Location { timeout: Some(30), ..Location::new("/api/", Backend::Url("http://127.0.0.1:4000".into())) }],
            upstreams: vec![],
        };
        let out = patch_text(&text, &new, &t).unwrap();
        assert!(out.contains("keepalive_requests 50;"));
        assert!(out.contains("listen [::]:443 ssl;") && out.contains("location /api/ {") && out.contains("proxy_read_timeout 30s;"), "{out}");
        // 删掉 location 和监听组
        let mut gone = new.clone();
        gone.kind = SiteKind::Proxy { locations: vec![Location::new("/", Backend::Url("http://127.0.0.1:3000".into()))], upstreams: vec![] };
        gone.listeners.truncate(1);
        gone.listeners[0].force_https = false;
        gone.hsts = false;
        let out2 = patch_text(&out, &gone, &t).unwrap();
        assert!(!out2.contains("/api/") && !out2.contains("443") && !out2.contains("keepalive_requests"), "{out2}");
    }

    #[test]
    fn unchanged_form_leaves_text_untouched_and_unsupported_is_refused() {
        let spec = base();
        let t = Target::own();
        let text = render_site(&spec, &t).replace("proxy_http_version 1.1;", "proxy_http_version 1.1;\n        proxy_buffering off;");
        assert_eq!(patch_text(&text, &spec, &t).unwrap(), text);
        // 认不出的 location 原样保留,表单其他改动照常应用
        let at = text.rfind("    location / {").unwrap();
        let extra = format!("{}    location ~ ^/x {{\n        proxy_pass http://127.0.0.1:9;\n    }}\n{}", &text[..at], &text[at..]);
        assert_eq!(patch_text(&extra, &spec, &t).unwrap(), extra);
        let mut n = spec.clone();
        n.ipv6 = !n.ipv6;
        let out = patch_text(&extra, &n, &t).unwrap();
        assert!(out.contains("location ~ ^/x {") && out.contains("127.0.0.1:9"), "{out}");
        // 只有认不出的 location 时仍拒绝
        let only = text.replace("location / {", "location ~ ^/x {");
        assert!(patch_text(&only, &spec, &t).is_err());
    }

    #[test]
    #[ignore]
    fn dump_patched() {
        let Ok(dir) = std::env::var("OTTER_DUMP_DIR") else { return };
        let spec = base();
        let t = Target::own();
        let hand = render_site(&spec, &t).replace("    location / {\n        proxy_pass", "    # 我的注释\n    proxy_buffering off;\n    location / {\n        proxy_pass");
        let mut new = spec.clone();
        new.ipv6 = true;
        new.adv.gzip = Some(true);
        new.adv.headers = vec![("X-A".into(), "1".into())];
        new.adv.limits = Limits { req_rate: Some(10), burst: 5, conn: Some(20), exempt: vec!["10.0.0.0/8".into()], ..Default::default() };
        new.kind = SiteKind::Proxy { locations: vec![Location::new("/", Backend::Url("http://127.0.0.1:3000".into())), Location::new("/api/", Backend::Url("http://127.0.0.1:4000".into()))], upstreams: vec![] };
        std::fs::write(format!("{dir}/pt.conf"), patch_text(&hand, &new, &t).unwrap()).unwrap();
    }
}
