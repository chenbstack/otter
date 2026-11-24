//! 给代码编辑器用的 JSON 着色:手写的小词法扫描,不依赖 tree-sitter。
//! 键、字符串、数字、true/false/null 各一种颜色,其余(括号、逗号、冒号)保持默认。

use std::ops::Range;

use gpui_kit::base::input::{EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, Rope};
use gpui_kit::{Context, HighlightStyle, Rgba, SharedString, Window};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Key,
    Str,
    Num,
    Lit,
}

/// 扫描整段文本,返回按位置排好、互不重叠的着色区间(字节偏移)
pub fn tokens(text: &str) -> Vec<(Range<usize>, Kind)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(b.len());
                // 后面(跳过空白)紧跟冒号的是键
                let mut j = i;
                while j < b.len() && matches!(b[j], b' ' | b'\t') {
                    j += 1;
                }
                out.push((start..i, if b.get(j) == Some(&b':') { Kind::Key } else { Kind::Str }));
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < b.len() && matches!(b[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                    i += 1;
                }
                out.push((start..i, Kind::Num));
            }
            b'a'..=b'z' => {
                let start = i;
                while i < b.len() && b[i].is_ascii_lowercase() {
                    i += 1;
                }
                if matches!(&text[start..i], "true" | "false" | "null") {
                    out.push((start..i, Kind::Lit));
                }
            }
            _ => i += 1,
        }
    }
    out
}

fn style(color: Rgba) -> HighlightStyle {
    HighlightStyle { color: Some(color.into()), ..Default::default() }
}

/// 编辑器用的 JSON 着色器。文本改动时整段重新扫描(daemon.json 很小)。
pub struct JsonHighlighter {
    toks: Vec<(Range<usize>, Kind)>,
    colors: [Rgba; 4],
}

impl JsonHighlighter {
    pub fn new(colors: [Rgba; 4]) -> Self {
        Self { toks: Vec::new(), colors }
    }

    fn color(&self, k: Kind) -> Rgba {
        self.colors[k as usize]
    }
}

impl InputHighlighter for JsonHighlighter {
    fn language(&self) -> SharedString {
        "json".into()
    }

    fn update(&mut self, _edit: Option<InputEdit>, text: &Rope, _folding: bool, _window: &mut Window, _cx: &mut Context<EditorState>) {
        self.toks = tokens(&text.to_string());
    }

    fn styles(&self, range: &Range<usize>, _resolver: &dyn HighlightStyleResolver) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut out = Vec::new();
        let mut at = range.start;
        for (r, k) in self.toks.iter().filter(|(r, _)| r.end > range.start && r.start < range.end) {
            let (s, e) = (r.start.max(range.start), r.end.min(range.end));
            if s > at {
                out.push((at..s, HighlightStyle::default()));
            }
            out.push((s..e, style(self.color(*k))));
            at = e;
        }
        if at < range.end {
            out.push((at..range.end, HighlightStyle::default()));
        }
        out
    }

    fn fold_ranges(&self, _text: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_keys_strings_numbers_literals() {
        let text = r#"{"a": "x", "n": -1.5e3, "t": true, "z": null}"#;
        let k: Vec<(&str, Kind)> = tokens(text).into_iter().map(|(r, k)| (&text[r], k)).collect();
        assert_eq!(
            k,
            vec![("\"a\"", Kind::Key), ("\"x\"", Kind::Str), ("\"n\"", Kind::Key), ("-1.5e3", Kind::Num), ("\"t\"", Kind::Key), ("true", Kind::Lit), ("\"z\"", Kind::Key), ("null", Kind::Lit)]
        );
    }

    #[test]
    fn escaped_quote_and_unterminated_string() {
        let t = r#""a\"b": 1"#;
        assert_eq!(tokens(t)[0], (0..6, Kind::Key));
        // 没闭合的字符串到行尾为止,不会吞掉后面的行
        let t2 = "\"abc\n7";
        let k = tokens(t2);
        assert_eq!(k.last().map(|x| x.1), Some(Kind::Num));
    }
}
