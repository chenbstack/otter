//! 终端关键字高亮:按正则给屏幕上的 IP、URL、error/warn/success 等词换前景色。
//! 只影响显示,不改终端缓冲区;渲染层只会把它用在默认前景色的文字上(见 `vendor/gpui_xterm` 的 `Highlighter`)。

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use gpui_xterm::Highlighter;


/// 全局开关,设置里切换后下一次重绘生效
static ENABLED: AtomicBool = AtomicBool::new(true);

/// 持久化用的 settings 键,值 `on` / `off`,缺省为开
pub const SETTING_KEY: &str = "term_highlight";

/// 启动时按记住的设置恢复开关
pub fn restore(store: &crate::store::Store) {
    set_enabled(store.get_setting(SETTING_KEY).as_deref() != Some("off"));
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Url,
    Ip,
    Error,
    Warn,
    Ok,
}

impl Kind {
    /// 颜色取自当前主题的 ANSI 色(`theme::current()`,默认主题见 tokens.md 1.9)
    fn ansi(self) -> usize {
        match self {
            Kind::Error => 1,  // 红
            Kind::Ok => 2,     // 绿
            Kind::Warn => 3,   // 黄
            Kind::Url => 12,   // 明亮蓝
            Kind::Ip => 13,    // 明亮品红
        }
    }
}

/// 顺序就是优先级:regex 同一位置先匹配到的分支胜出,URL 在前,所以 URL 里的 IP 不会被单独染色
static RULES: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(concat!(
        r#"(?P<url>https?://[^\s<>"'\)\]]+)"#,
        r"|(?P<ip>\b(?:\d{1,3}\.){3}\d{1,3}(?::\d{1,5})?\b)",
        r"|(?P<error>(?i:\b(?:error|errors|fail|failed|failure|fatal|denied|refused|critical|panic|exception)\b))",
        r"|(?P<warn>(?i:\b(?:warn|warning|deprecated)\b))",
        r"|(?P<ok>(?i:\b(?:ok|done|success|successful|successfully|running|started|active)\b))",
    ))
    .expect("高亮规则是常量,必须能编译")
});

fn valid_ip(s: &str) -> bool {
    let host = s.split(':').next().unwrap_or(s);
    host.split('.').all(|p| p.parse::<u16>().is_ok_and(|n| n <= 255))
}

/// 一行文字里的匹配,范围按**字符下标**
fn find(line: &str) -> Vec<(Range<usize>, Kind)> {
    let ascii = line.is_ascii();
    let char_idx = |byte: usize| if ascii { byte } else { line[..byte].chars().count() };
    let mut out = Vec::new();
    for caps in RULES.captures_iter(line) {
        let (m, kind) = if let Some(m) = caps.name("url") {
            (m, Kind::Url)
        } else if let Some(m) = caps.name("ip") {
            if !valid_ip(m.as_str()) {
                continue;
            }
            (m, Kind::Ip)
        } else if let Some(m) = caps.name("error") {
            (m, Kind::Error)
        } else if let Some(m) = caps.name("warn") {
            (m, Kind::Warn)
        } else if let Some(m) = caps.name("ok") {
            (m, Kind::Ok)
        } else {
            continue;
        };
        out.push((char_idx(m.start())..char_idx(m.end()), kind));
    }
    out
}

fn color(kind: Kind) -> gpui_kit::Hsla {
    let (r, g, b) = super::theme::current().ansi[kind.ansi()];
    gpui_kit::Rgba { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }.into()
}

/// 交给 `TerminalConfig::highlighter` 的高亮器。内部按行内容缓存结果,滚动、光标闪烁时同一行不重复匹配。
pub fn highlighter() -> Highlighter {
    let cache: Mutex<HashMap<String, Arc<Vec<(Range<usize>, Kind)>>>> = Mutex::new(HashMap::new());
    Highlighter(Arc::new(move |line: &str| {
        if !enabled() {
            return Vec::new();
        }
        let hits = {
            let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.len() > 1024 {
                cache.clear();
            }
            cache.entry(line.to_string()).or_insert_with(|| Arc::new(find(line))).clone()
        };
        hits.iter().map(|(r, k)| (r.clone(), color(*k))).collect()
    }))
}

#[cfg(test)]
mod tests {
    use super::{Kind, find};

    fn kinds(line: &str) -> Vec<(String, Kind)> {
        find(line)
            .into_iter()
            .map(|(r, k)| (line.chars().skip(r.start).take(r.end - r.start).collect(), k))
            .collect()
    }

    #[::core::prelude::v1::test]
    fn matches_ip_with_port_and_rejects_invalid() {
        assert_eq!(kinds("from 203.0.113.7 port"), vec![("203.0.113.7".into(), Kind::Ip)]);
        assert_eq!(kinds("listen 0.0.0.0:8080"), vec![("0.0.0.0:8080".into(), Kind::Ip)]);
        assert!(kinds("version 1.2.3.999").is_empty());
    }

    #[::core::prelude::v1::test]
    fn url_wins_over_ip_inside_it() {
        assert_eq!(kinds("see http://10.0.0.1:9000/x?a=1 now"), vec![("http://10.0.0.1:9000/x?a=1".into(), Kind::Url)]);
    }

    #[::core::prelude::v1::test]
    fn keywords_respect_word_boundaries_and_case() {
        assert_eq!(kinds("ERROR: Failed to start"), vec![("ERROR".into(), Kind::Error), ("Failed".into(), Kind::Error)]);
        assert!(kinds("okay token errored brokenness").is_empty());
        assert_eq!(kinds("warning: deprecated"), vec![("warning".into(), Kind::Warn), ("deprecated".into(), Kind::Warn)]);
        assert_eq!(kinds("Active: active (running)"), vec![("Active".into(), Kind::Ok), ("active".into(), Kind::Ok), ("running".into(), Kind::Ok)]);
    }

    #[::core::prelude::v1::test]
    fn ranges_are_char_indices_after_wide_chars() {
        // 「六六云」3 个字符,error 从字符下标 4 开始
        let hits = find("六六云 error");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 4..9);
    }
}
