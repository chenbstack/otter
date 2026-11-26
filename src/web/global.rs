//! OpenResty 全局设置(`http {}` 里的性能类指令):以服务器上的 `nginx.conf` 为事实源,不另存。
//! 读取时解析 http 块里的这几条指令(没写就是 nginx 默认值);保存时就地改写,
//! 其余内容(手写指令、注释)原样保留。单站覆盖在站点配置里,这里的值是默认值。

use super::patch::{line_end, line_start, parse_tree};
use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// 大小:`512`、`32k`、`50m`
    Size,
    /// 时间:`60`、`60s`、`5m`
    Time,
    /// 整数范围
    Int(u32, u32),
    OnOff,
}

pub struct Item {
    pub name: &'static str,
    pub label: &'static str,
    pub desc: &'static str,
    /// nginx 没写这条指令时的默认值(界面里显示、留空时沿用)
    pub default: &'static str,
    pub kind: Kind,
}

/// 顺序就是界面顺序;`parse`/`apply` 的值数组和它一一对应
pub const ITEMS: [Item; 10] = [
    Item { name: "client_max_body_size", label: "最大上传大小", desc: "请求体上限,超过返回 413;站点里可单独覆盖", default: "1m", kind: Kind::Size },
    Item { name: "keepalive_timeout", label: "长连接空闲保持时间", desc: "客户端长连接空闲多久后关闭", default: "75s", kind: Kind::Time },
    Item { name: "proxy_connect_timeout", label: "连接后端超时", desc: "与后端建立连接的超时;反向代理站点里可单独覆盖", default: "60s", kind: Kind::Time },
    Item { name: "proxy_read_timeout", label: "读取后端响应超时", desc: "等待后端响应的超时;长任务、流式接口常要调大", default: "60s", kind: Kind::Time },
    Item { name: "proxy_send_timeout", label: "向后端发送超时", desc: "向后端发送请求的超时", default: "60s", kind: Kind::Time },
    Item { name: "gzip", label: "压缩", desc: "是否对文本类响应启用 gzip;站点里可单独沿用/开/关", default: "off", kind: Kind::OnOff },
    Item { name: "gzip_min_length", label: "最小压缩大小", desc: "小于它的响应不压缩", default: "20", kind: Kind::Size },
    Item { name: "gzip_comp_level", label: "压缩级别", desc: "1–9,越大越省带宽、越费 CPU", default: "1", kind: Kind::Int(1, 9) },
    Item { name: "client_header_buffer_size", label: "请求头缓冲", desc: "cookie 很大、请求头过长报 400 时才需要调", default: "1k", kind: Kind::Size },
    Item { name: "server_names_hash_bucket_size", label: "域名哈希桶大小", desc: "域名很长、nginx 报 server_names_hash 错误时才需要调", default: "64", kind: Kind::Int(1, 65536) },
];

pub type Vals = Vec<Option<String>>;

/// 单个值的格式检查
pub fn check_value(it: &Item, v: &str) -> Result<(), String> {
    let digits = |s: &str| !s.is_empty() && s.len() <= 9 && s.chars().all(|c| c.is_ascii_digit());
    let ok = match it.kind {
        Kind::Size => v.strip_suffix(['k', 'K', 'm', 'M', 'g', 'G']).unwrap_or(v).pipe(digits),
        Kind::Time => ["ms", "s", "m", "h", "d"].iter().find_map(|u| v.strip_suffix(u)).unwrap_or(v).pipe(digits),
        Kind::Int(lo, hi) => digits(v) && v.parse::<u32>().is_ok_and(|n| (lo..=hi).contains(&n)),
        Kind::OnOff => v == "on" || v == "off",
    };
    if ok {
        return Ok(());
    }
    Err(format!(
        "{}的格式不对:{}",
        it.label,
        match it.kind {
            Kind::Size => "如 512、32k、50m".to_string(),
            Kind::Time => "如 60、60s、5m".to_string(),
            Kind::Int(lo, hi) => format!("要填 {lo}–{hi} 的整数"),
            Kind::OnOff => "要选开或关".to_string(),
        }
    ))
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

fn http_block(tree: &[patch::PNode]) -> Option<&patch::PNode> {
    tree.iter().find(|n| n.words.first().map(String::as_str) == Some("http") && n.kids.is_some())
}

/// 读出 http 块里每项当前写的值;没写是 None
pub fn parse(conf: &str) -> Vals {
    let tree = parse_tree(conf);
    let kids = http_block(&tree).and_then(|h| h.kids.as_deref()).unwrap_or(&[]);
    ITEMS.iter().map(|it| kids.iter().filter(|k| k.kids.is_none() && k.words.first().map(String::as_str) == Some(it.name)).next_back().and_then(|k| k.words.get(1).cloned())).collect()
}

/// 把 `vals` 套到原文上:已有的改值,没有的补在 http 块末尾,值为 None 的删掉那一行。其余原样保留
pub fn apply(conf: &str, vals: &[Option<String>]) -> Result<String, String> {
    if vals.len() != ITEMS.len() {
        return Err("设置项数量不对".into());
    }
    for (it, v) in ITEMS.iter().zip(vals) {
        if let Some(v) = v {
            check_value(it, v)?;
        }
    }
    let tree = parse_tree(conf);
    let http = http_block(&tree).ok_or("nginx.conf 里没找到 http 块")?;
    let kids = http.kids.as_deref().unwrap_or(&[]);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut add = String::new();
    for (it, want) in ITEMS.iter().zip(vals) {
        let found: Vec<&patch::PNode> = kids.iter().filter(|k| k.kids.is_none() && k.words.first().map(String::as_str) == Some(it.name)).collect();
        match (found.last(), want) {
            (Some(n), Some(v)) => {
                if n.words.get(1) != Some(v) || n.words.len() != 2 {
                    edits.push((n.start, n.end, format!("{} {v};", it.name)));
                }
                // 重复写的旧行去掉(nginx 本来就会报重复,这里顺手清掉)
                for d in &found[..found.len() - 1] {
                    edits.push(remove_range(conf, d));
                }
            }
            (Some(_), None) => {
                for d in &found {
                    edits.push(remove_range(conf, d));
                }
            }
            (None, Some(v)) => add += &format!("    {} {v};\n", it.name),
            (None, None) => {}
        }
    }
    if !add.is_empty() {
        let close = http.end - 1;
        let ls = line_start(conf, close);
        if conf[ls..close].trim().is_empty() {
            edits.push((ls, ls, add));
        } else {
            edits.push((close, close, format!("\n{add}")));
        }
    }
    edits.sort_by_key(|e| std::cmp::Reverse((e.0, e.1)));
    let mut out = conf.to_string();
    for (a, b, t) in edits {
        out.replace_range(a..b, &t);
    }
    // 套完再读一遍,必须和想要的一致
    if parse(&out) != vals {
        return Err("改写后和期望的设置不一致,没有保存".into());
    }
    Ok(out)
}

/// 删一条指令:它独占一行就连行一起删,否则只删指令本身
fn remove_range(conf: &str, n: &patch::PNode) -> (usize, usize, String) {
    let (ls, le) = (line_start(conf, n.start), line_end(conf, n.end));
    if conf[ls..n.start].trim().is_empty() && conf[n.end..le].trim().is_empty() {
        (ls, (le + 1).min(conf.len()), String::new())
    } else {
        (n.start, n.end, String::new())
    }
}

/// 保存脚本:备份 → 原地写入(`nginx.conf` 是单文件挂载,不能 mv 换文件)→ `nginx -t` → reload;失败把原内容写回。输出 `OK` 或 `ERR:…`
pub fn save_script(conf: &str, vals: &[Option<String>], t: &Target) -> Result<String, String> {
    if t.adopted {
        return Err("接管的 nginx 不在这里改全局设置".into());
    }
    Ok(write_script(&apply(conf, vals)?))
}

/// 把完整的新 `nginx.conf` 原地写入并检查、重载(失败还原)
pub fn write_script(new: &str) -> String {
    format!(
        "{}\n{}\n",
        srvdata::lock_prelude(),
        fill(
            r#"@PRE@; F=$W/conf/nginx.conf
[ -f "$F" ] || { echo 'ERR:没找到 nginx.conf'; exit 0; }
docker inspect -f '{{.State.Running}}' $N 2>/dev/null | grep -q true || { echo 'ERR:OpenResty/nginx 容器没有在运行'; exit 0; }
mkdir -p @T@ $W/conf/backup
O=$(mktemp @T@/ng.XXXXXX) || { echo 'ERR:建不了临时文件'; exit 0; }
cp -p "$F" "$O"; cp -p "$F" "$W/conf/backup/nginx.conf.$(date +%s)"
printf %s @B64@ | base64 -d > "$F" || { cat "$O" > "$F"; rm -f "$O"; echo 'ERR:写入失败'; exit 0; }
o=$(docker exec $N $B -t 2>&1) || { cat "$O" > "$F"; rm -f "$O"; echo "ERR:$o"; exit 0; }
o=$(docker exec $N $B -s reload 2>&1) || { cat "$O" > "$F"; rm -f "$O"; docker exec $N $B -s reload >/dev/null 2>&1; echo "ERR:$o"; exit 0; }
rm -f "$O"
echo OK
"#
        )
        .replace("@B64@", &quote(&b64(new.as_bytes())))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(f: impl Fn(&str) -> Option<&'static str>) -> Vals {
        ITEMS.iter().map(|i| f(i.name).map(str::to_string)).collect()
    }

    #[test]
    fn parses_template() {
        let v = parse(&nginx_conf());
        let get = |n: &str| v[ITEMS.iter().position(|i| i.name == n).unwrap()].clone();
        assert_eq!(get("client_max_body_size").as_deref(), Some("100m"));
        assert_eq!(get("keepalive_timeout").as_deref(), Some("65"));
        assert_eq!(get("gzip").as_deref(), Some("on"));
        assert_eq!(get("gzip_comp_level").as_deref(), Some("5"));
        assert_eq!(get("proxy_read_timeout").as_deref(), Some("300s"));
        assert_eq!(get("proxy_send_timeout"), None);
    }

    #[test]
    fn apply_changes_adds_removes_and_keeps_rest() {
        let conf = "# 我的注释\nworker_processes auto;\nhttp {\n    # 手写\n    client_max_body_size 100m;\n    keepalive_timeout 65;\n    gzip on;\n    custom_thing yes;\n    server {\n        gzip off;\n        client_max_body_size 1k;\n    }\n    include /etc/nginx/conf.d/*.conf;\n}\n";
        let v = vals(|n| match n {
            "client_max_body_size" => Some("2g"),
            "proxy_read_timeout" => Some("300s"),
            "gzip" => Some("on"),
            _ => None,
        });
        let out = apply(conf, &v).unwrap();
        assert!(out.contains("# 我的注释") && out.contains("# 手写") && out.contains("custom_thing yes;") && out.contains("include /etc/nginx/conf.d/*.conf;"));
        assert!(out.contains("    client_max_body_size 2g;\n") && out.contains("    proxy_read_timeout 300s;\n"));
        assert!(!out.contains("keepalive_timeout"), "None 的删掉");
        // server 块里的同名指令不动
        assert!(out.contains("        gzip off;\n        client_max_body_size 1k;\n"));
        assert_eq!(parse(&out), v);
        // 再套一次不变
        assert_eq!(apply(&out, &v).unwrap(), out);
    }

    #[test]
    fn apply_to_template_round_trips() {
        let t = nginx_conf();
        let same = parse(&t);
        assert_eq!(apply(&t, &same).unwrap(), t, "没改就原样");
        let mut v = same.clone();
        v[0] = Some("500m".into());
        let out = apply(&t, &v).unwrap();
        assert!(out.contains("client_max_body_size 500m;") && !out.contains("100m"));
        assert_eq!(out.len(), t.len());
    }

    #[test]
    fn validation() {
        let it = |n: &str| ITEMS.iter().find(|i| i.name == n).unwrap();
        for (n, v, ok) in [
            ("client_max_body_size", "50m", true),
            ("client_max_body_size", "0", true),
            ("client_max_body_size", "50mb", false),
            ("client_max_body_size", "", false),
            ("client_max_body_size", "1;rm", false),
            ("keepalive_timeout", "60s", true),
            ("keepalive_timeout", "60", true),
            ("keepalive_timeout", "1.5s", false),
            ("gzip", "on", true),
            ("gzip", "yes", false),
            ("gzip_comp_level", "9", true),
            ("gzip_comp_level", "10", false),
            ("gzip_comp_level", "0", false),
        ] {
            assert_eq!(check_value(it(n), v).is_ok(), ok, "{n} {v}");
        }
        assert!(apply(&nginx_conf(), &vals(|n| (n == "gzip").then_some("maybe"))).is_err());
        assert!(apply("worker_processes 1;", &vals(|_| None)).is_err(), "没有 http 块");
    }

    #[test]
    fn save_script_is_valid_shell_and_in_place() {
        let t = Target::own();
        let s = save_script(&nginx_conf(), &vals(|n| (n == "gzip").then_some("on")), &t).unwrap();
        let o = std::process::Command::new("bash").arg("-n").arg("-c").arg(&s).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(s.contains("> \"$F\"") && !s.contains("mv "), "单文件挂载:必须原地写,不能换文件");
        let mut ad = Target::own();
        ad.adopted = true;
        assert!(save_script(&nginx_conf(), &vals(|_| None), &ad).is_err());
    }
}
