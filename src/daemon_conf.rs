//! Docker 守护进程配置(`/etc/docker/daemon.json`)的表单模型。
//!
//! 界面只编辑常用的几项;文件里其他的键原样保留(`render` 在原来的 JSON 上改,不重写整个文件)。

use serde_json::{Map, Value, json};

/// 服务器上的配置文件路径
pub const PATH: &str = "/etc/docker/daemon.json";

/// 表单里的各项。列表项用空格、逗号或换行分隔(镜像加速和 HTTP 仓库显示时一行一个),空串表示「不设置」。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Form {
    pub mirrors: String,
    pub insecure: String,
    pub data_root: String,
    /// 日志驱动,空串是 Docker 默认(json-file)
    pub driver: String,
    pub max_size: String,
    pub max_file: String,
    pub live_restore: bool,
}

fn list_of(v: &Value) -> String {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")).unwrap_or_default()
}

fn str_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// 解析文件内容:返回完整的 JSON(渲染时在它上面改)和表单。内容为空白按「还没有配置」处理。
pub fn parse(text: &str) -> Result<(Value, Form), String> {
    let base: Value = if text.trim().is_empty() { json!({}) } else { serde_json::from_str(text).map_err(|e| format!("daemon.json 不是合法的 JSON:{e}"))? };
    let obj = base.as_object().ok_or("daemon.json 的最外层必须是 JSON 对象")?;
    let opts = obj.get("log-opts");
    let form = Form {
        mirrors: obj.get("registry-mirrors").map(list_of).unwrap_or_default().replace(' ', "\n"),
        insecure: obj.get("insecure-registries").map(list_of).unwrap_or_default().replace(' ', "\n"),
        data_root: str_of(obj.get("data-root")),
        driver: str_of(obj.get("log-driver")),
        max_size: str_of(opts.and_then(|o| o.get("max-size"))),
        max_file: str_of(opts.and_then(|o| o.get("max-file"))),
        live_restore: obj.get("live-restore").and_then(Value::as_bool).unwrap_or(false),
    };
    Ok((base, form))
}

fn items(s: &str) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || c == ',' || c == ',').filter(|x| !x.is_empty()).map(str::to_string).collect()
}

/// 校验表单,返回第一处不合法的原因
pub fn validate(f: &Form) -> Result<(), String> {
    for m in items(&f.mirrors) {
        if !(m.starts_with("http://") || m.starts_with("https://")) {
            return Err(format!("镜像加速地址要以 http:// 或 https:// 开头:{m}"));
        }
    }
    for r in items(&f.insecure) {
        if r.contains("://") || r.contains('"') {
            return Err(format!("HTTP 仓库写成 主机:端口 的形式,不要带协议:{r}"));
        }
    }
    let root = f.data_root.trim();
    if !root.is_empty() && !root.starts_with('/') {
        return Err("数据目录必须是绝对路径".into());
    }
    let size = f.max_size.trim();
    if !size.is_empty() {
        let (num, unit) = size.split_at(size.len() - 1);
        if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) || !matches!(unit, "k" | "m" | "g" | "K" | "M" | "G") {
            return Err("单个日志文件大小写成 10m、100k、1g 这样".into());
        }
    }
    let file = f.max_file.trim();
    if !file.is_empty() && file.parse::<u32>().map_or(true, |n| n == 0) {
        return Err("日志文件个数必须是正整数".into());
    }
    Ok(())
}

fn set_or_remove(obj: &mut Map<String, Value>, key: &str, v: Option<Value>) {
    match v {
        Some(v) => {
            obj.insert(key.to_string(), v);
        }
        None => {
            obj.remove(key);
        }
    }
}

/// 把表单合并进 `base`,得到要写回的文件内容(带缩进和结尾换行)
pub fn render(base: &Value, f: &Form) -> Result<String, String> {
    validate(f)?;
    let mut obj = base.as_object().cloned().unwrap_or_default();
    let list = |s: &str| (!items(s).is_empty()).then(|| json!(items(s)));
    set_or_remove(&mut obj, "registry-mirrors", list(&f.mirrors));
    set_or_remove(&mut obj, "insecure-registries", list(&f.insecure));
    let root = f.data_root.trim();
    set_or_remove(&mut obj, "data-root", (!root.is_empty()).then(|| json!(root)));
    set_or_remove(&mut obj, "log-driver", (!f.driver.is_empty()).then(|| json!(f.driver)));
    let mut opts = obj.get("log-opts").and_then(Value::as_object).cloned().unwrap_or_default();
    set_or_remove(&mut opts, "max-size", (!f.max_size.trim().is_empty()).then(|| json!(f.max_size.trim())));
    set_or_remove(&mut opts, "max-file", (!f.max_file.trim().is_empty()).then(|| json!(f.max_file.trim())));
    set_or_remove(&mut obj, "log-opts", (!opts.is_empty()).then(|| Value::Object(opts)));
    set_or_remove(&mut obj, "live-restore", f.live_restore.then(|| json!(true)));
    let text = serde_json::to_string_pretty(&Value::Object(obj)).map_err(|e| e.to_string())?;
    Ok(if text == "{}" { "{}\n".into() } else { text + "\n" })
}

/// daemon.json 里的 insecure-registries 是否已有这个仓库
pub fn has_insecure(text: &str, host: &str) -> Result<bool, String> {
    let (_, f) = parse(text)?;
    Ok(items(&f.insecure).iter().any(|h| h == host))
}

/// 把仓库加进 insecure-registries(已有就原样返回),其他键保持不变
pub fn add_insecure(text: &str, host: &str) -> Result<String, String> {
    let (base, mut f) = parse(text)?;
    if !items(&f.insecure).iter().any(|h| h == host) {
        f.insecure = if f.insecure.trim().is_empty() { host.to_string() } else { format!("{}\n{host}", f.insecure) };
    }
    render(&base, &f)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"registry-mirrors":["https://a.example","https://b.example"],"log-driver":"json-file","log-opts":{"max-size":"10m","max-file":"3","labels":"env"},"live-restore":true,"features":{"buildkit":true}}"#;

    #[test]
    fn parses_form_fields() {
        let (_, f) = parse(SAMPLE).unwrap();
        assert_eq!(f.mirrors, "https://a.example\nhttps://b.example");
        assert_eq!(f.driver, "json-file");
        assert_eq!((f.max_size.as_str(), f.max_file.as_str()), ("10m", "3"));
        assert!(f.live_restore);
        assert_eq!(f.data_root, "");
    }

    #[test]
    fn empty_and_broken_text() {
        assert_eq!(parse("  \n").unwrap().1, Form::default());
        assert!(parse("{").is_err());
        assert!(parse("[1]").is_err());
    }

    #[test]
    fn render_keeps_unknown_keys() {
        let (base, mut f) = parse(SAMPLE).unwrap();
        f.mirrors = "https://c.example, https://d.example".into();
        f.max_size = "50m".into();
        let out = render(&base, &f).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["registry-mirrors"], json!(["https://c.example", "https://d.example"]));
        assert_eq!(v["log-opts"]["max-size"], "50m");
        assert_eq!(v["log-opts"]["labels"], "env");
        assert_eq!(v["features"]["buildkit"], true);
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn clearing_fields_removes_keys() {
        let (base, mut f) = parse(SAMPLE).unwrap();
        f.mirrors.clear();
        f.driver.clear();
        f.max_size.clear();
        f.max_file.clear();
        f.live_restore = false;
        let v: Value = serde_json::from_str(&render(&base, &f).unwrap()).unwrap();
        assert!(v.get("registry-mirrors").is_none() && v.get("log-driver").is_none() && v.get("live-restore").is_none());
        assert_eq!(v["log-opts"], json!({"labels": "env"}));
        assert_eq!(render(&json!({}), &Form::default()).unwrap(), "{}\n");
    }

    #[test]
    fn adds_insecure_registry_keeping_other_keys() {
        assert!(!has_insecure(SAMPLE, "10.0.0.2:5000").unwrap());
        let out = add_insecure(SAMPLE, "10.0.0.2:5000").unwrap();
        assert!(has_insecure(&out, "10.0.0.2:5000").unwrap());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["insecure-registries"], json!(["10.0.0.2:5000"]));
        assert_eq!(v["features"]["buildkit"], true);
        // 已有的不重复加;没有文件时从空开始
        assert_eq!(add_insecure(&out, "10.0.0.2:5000").unwrap(), out);
        let v2: Value = serde_json::from_str(&add_insecure("", "h:1").unwrap()).unwrap();
        assert_eq!(v2["insecure-registries"], json!(["h:1"]));
        assert!(add_insecure("{", "h:1").is_err());
    }

    #[test]
    fn rejects_bad_values() {
        let ok = Form::default();
        assert!(validate(&Form { mirrors: "mirror.example".into(), ..ok.clone() }).is_err());
        assert!(validate(&Form { insecure: "http://x:5000".into(), ..ok.clone() }).is_err());
        assert!(validate(&Form { data_root: "data".into(), ..ok.clone() }).is_err());
        assert!(validate(&Form { max_size: "big".into(), ..ok.clone() }).is_err());
        assert!(validate(&Form { max_size: "m".into(), ..ok.clone() }).is_err());
        assert!(validate(&Form { max_file: "0".into(), ..ok.clone() }).is_err());
        assert!(validate(&Form { mirrors: "https://m.example".into(), insecure: "10.0.0.2:5000".into(), data_root: "/data/docker".into(), max_size: "10m".into(), max_file: "3".into(), ..ok }).is_ok());
    }
}
