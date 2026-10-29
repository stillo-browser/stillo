use crate::document::{ExtractedContent, MarkdownDocument};
use crate::html_to_ast::{collapse_whitespace, is_generic_block, is_tag_start};
use crate::sanitize::is_navigable_url;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct MarkdownConfig {
    pub max_line_width: usize,
    pub include_links: bool,
    pub include_images: bool,
    pub heading_style: HeadingStyle,
}

impl Default for MarkdownConfig {
    fn default() -> Self {
        Self {
            max_line_width: 80,
            include_links: true,
            include_images: false,
            heading_style: HeadingStyle::Atx,
        }
    }
}

#[derive(Debug, Clone)]
pub enum HeadingStyle {
    Atx,
    Setext,
}

pub struct MarkdownSerializer {
    config: MarkdownConfig,
}

impl MarkdownSerializer {
    pub fn new(config: MarkdownConfig) -> Self {
        Self { config }
    }

    /// ExtractedContent → MarkdownDocument（純粋関数）
    ///
    /// 取得時刻は呼び出し側から受け取る。core 内で時計を読むと同じ入力から異なる出力になり、
    /// 純粋関数でなくなるため。
    pub fn serialize(
        &self,
        content: &ExtractedContent,
        extracted_at: DateTime<Utc>,
    ) -> MarkdownDocument {
        let mut out = String::new();

        out.push_str(&format!("# {}\n\n", content.title));
        if let Some(byline) = &content.byline {
            out.push_str(&format!("*{}*\n\n", byline));
        }
        out.push_str(&format!("> Source: {}\n\n", content.url));

        let body_md = self.html_to_markdown(&content.body_html, &content.url);
        out.push_str(&body_md);

        if self.config.include_links && !content.links.is_empty() {
            out.push_str("\n\n---\n\n## Links\n\n");
            for (i, link) in content.links.iter().enumerate() {
                out.push_str(&format!(
                    "{}. [{}]({})\n",
                    i + 1,
                    escape_link_text(&link.text),
                    link.href
                ));
            }
        }

        MarkdownDocument {
            content: out,
            source_url: content.url.clone(),
            extracted_at,
        }
    }

    fn html_to_markdown(&self, html: &str, base_url: &url::Url) -> String {
        let mut converter = HtmlToMarkdown::new(self.config.include_links, base_url.clone());
        converter.convert(html);
        normalize_blank_lines(&converter.finish())
    }
}

/// 出力先を切り替えるための入れ子のバッファ。
/// 引用は閉じたときに全行へ `> ` を付け、リンクは閉じたときに `[text](href)` へ整形するため、
/// 内容をいったん別バッファに溜める。
enum Frame {
    Quote(String),
    Link { href: String, text: String },
}

struct HtmlToMarkdown {
    output: String,
    include_links: bool,
    base_url: url::Url,
    frames: Vec<Frame>,
    /// 開いているリスト。None は ul、Some(n) は ol で直前に出力した番号
    lists: Vec<Option<usize>>,
    /// pre の内側では空白・改行をそのまま残す
    pre_depth: usize,
    /// 現在の表の行で開いたセル数。2 つ目以降のセルの前に区切りを入れる
    cell_count: usize,
}

impl HtmlToMarkdown {
    fn new(include_links: bool, base_url: url::Url) -> Self {
        Self {
            output: String::new(),
            include_links,
            base_url,
            frames: Vec::new(),
            lists: Vec::new(),
            pre_depth: 0,
            cell_count: 0,
        }
    }

    fn convert(&mut self, html: &str) {
        let mut pos = 0;
        let bytes = html.as_bytes();

        while pos < html.len() {
            let is_lt = bytes[pos] == b'<';
            if is_lt && is_tag_start(bytes.get(pos + 1)) {
                let Some(close_offset) = html[pos..].find('>') else {
                    // 閉じる `>` がないので以降はタグではない。テキストとして読んで終える
                    self.push_text(&html_decode(&html[pos..]));
                    break;
                };
                let inner = &html[pos + 1..pos + close_offset];
                let (tag, attrs_str, is_closing, is_self_closing) = parse_tag(inner);
                self.handle_tag(&tag, attrs_str, is_closing, is_self_closing);
                pos += close_offset + 1;
                continue;
            }

            // テキストノード。タグにならない `<` は文字として扱い、1 文字先から次の `<` を探す
            // （pos から探すと同じ位置に留まり無限ループになる）
            let search_from = if is_lt { pos + 1 } else { pos };
            let next = html[search_from..]
                .find('<')
                .map(|i| search_from + i)
                .unwrap_or(html.len());
            let text = html_decode(&html[pos..next]);
            self.push_text(&text);
            pos = next;
        }
    }

    /// 閉じられなかった引用・リンクを外側へ畳み込んで最終出力を返す
    fn finish(mut self) -> String {
        while !self.frames.is_empty() {
            self.close_frame();
        }
        self.output
    }

    fn in_link(&self) -> bool {
        self.frames.iter().any(|f| matches!(f, Frame::Link { .. }))
    }

    fn handle_tag(&mut self, tag: &str, attrs: &str, is_closing: bool, _is_self_closing: bool) {
        // リンク内のブロック要素（カード型リンクの見出しや段落）を Markdown 記法にすると
        // リンクテキストに `#` や改行が入り `[...](...)` が壊れるため、語の区切りだけ残す
        if self.in_link() && is_block_like(tag) {
            self.push_text(" ");
            return;
        }

        match (tag, is_closing) {
            ("h1", false) => self.push_str("\n# "),
            ("h2", false) => self.push_str("\n## "),
            ("h3", false) => self.push_str("\n### "),
            ("h4", false) => self.push_str("\n#### "),
            ("h5", false) => self.push_str("\n##### "),
            ("h6", false) => self.push_str("\n###### "),
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", true) => self.push_str("\n\n"),

            ("p", false) => self.ensure_line_break(),
            ("p", true) => self.push_str("\n\n"),

            ("br", _) => self.push_str("\n"),
            // 前後を空行で囲まないと直前の行が setext 見出しになる
            ("hr", _) => self.push_str("\n\n---\n\n"),

            ("strong" | "b", _) => self.push_str("**"),
            ("em" | "i", _) => self.push_str("*"),
            // pre 内の code はフェンスで囲まれているのでバッククォートを足さない
            ("code", _) if self.pre_depth > 0 => {}
            ("code", _) => self.push_str("`"),

            ("pre", false) => {
                self.ensure_line_break();
                self.push_str("\n```\n");
                self.pre_depth += 1;
            }
            ("pre", true) => {
                self.pre_depth = self.pre_depth.saturating_sub(1);
                if !self.buf().ends_with('\n') {
                    self.push_str("\n");
                }
                self.push_str("```\n\n");
            }

            ("blockquote", false) => {
                self.ensure_line_break();
                self.frames.push(Frame::Quote(String::new()));
            }
            ("blockquote", true) => {
                if self.frames.iter().any(|f| matches!(f, Frame::Quote(_))) {
                    while !matches!(self.frames.last(), Some(Frame::Quote(_))) {
                        self.close_frame();
                    }
                    self.close_frame();
                }
            }

            ("ul", false) => self.lists.push(None),
            ("ol", false) => self.lists.push(Some(0)),
            ("ul" | "ol", true) => {
                self.lists.pop();
                self.push_str("\n");
            }
            ("li", false) => {
                let indent = "  ".repeat(self.lists.len().saturating_sub(1));
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}. ", n)
                    }
                    _ => "- ".to_owned(),
                };
                self.push_str(&format!("\n{}{}", indent, marker));
            }
            ("li", true) => {}

            ("tr", false) => {
                self.ensure_line_break();
                self.cell_count = 0;
            }
            ("tr", true) => self.ensure_line_break(),
            ("td" | "th", false) => {
                // 空セルの後に区切りだけが並ばないよう、行に内容があるときだけ区切る
                let line_has_content = !self
                    .buf()
                    .rsplit('\n')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .is_empty();
                if self.cell_count > 0 && line_has_content {
                    self.trim_line_end();
                    self.push_str(" | ");
                }
                self.cell_count += 1;
            }
            ("td" | "th", true) => {}

            (t, _) if is_generic_block(t) => self.ensure_line_break(),

            ("a", false) if self.include_links => {
                let raw_href = extract_attr(attrs, "href").unwrap_or_default();
                // 相対URLは base_url を基に絶対URLへ解決する。
                // javascript: / data: 等は空 href にして、閉じたときに平文として出す
                let href = match self.base_url.join(&raw_href) {
                    Ok(u) if !raw_href.is_empty() && is_navigable_url(&u) => u.to_string(),
                    _ => String::new(),
                };
                self.frames.push(Frame::Link {
                    href,
                    text: String::new(),
                });
            }
            ("a", true) if self.include_links => {
                if matches!(self.frames.last(), Some(Frame::Link { .. })) {
                    self.close_frame();
                }
            }

            _ => {}
        }
    }

    /// 最も内側のバッファを閉じ、整形した内容を外側へ書き出す
    fn close_frame(&mut self) {
        match self.frames.pop() {
            Some(Frame::Quote(content)) => {
                let quoted = quote_lines(&content);
                if !quoted.is_empty() {
                    self.push_str(&format!("\n{}\n\n", quoted));
                }
            }
            Some(Frame::Link { href, text }) => {
                let text = collapse_whitespace(&text);
                let text = text.trim();
                if text.is_empty() {
                    return;
                }
                if href.is_empty() {
                    self.push_text(text);
                } else {
                    self.push_str(&format!("[{}]({})", escape_link_text(text), href));
                }
            }
            None => {}
        }
    }

    fn buf(&mut self) -> &mut String {
        match self.frames.last_mut() {
            Some(Frame::Quote(s)) | Some(Frame::Link { text: s, .. }) => s,
            None => &mut self.output,
        }
    }

    fn push_str(&mut self, s: &str) {
        if s.starts_with('\n') {
            self.trim_line_end();
        }
        self.buf().push_str(s);
    }

    /// 行末の空白を除く。インライン要素間の空白を残した結果、行末に空白が付くため
    /// （pre 内では空白に意味があるので触らない）
    fn trim_line_end(&mut self) {
        if self.pre_depth > 0 {
            return;
        }
        let buf = self.buf();
        let len = buf.trim_end_matches(' ').len();
        buf.truncate(len);
    }

    fn push_text(&mut self, text: &str) {
        if self.pre_depth > 0 {
            self.push_str(text);
            return;
        }
        // HTML の空白の扱いに合わせて畳む。改行を残すと引用・リスト内で行が記法の外へはみ出す
        let collapsed = collapse_whitespace(text);
        let buf = self.buf();
        let text = if buf.is_empty() || buf.ends_with(char::is_whitespace) {
            collapsed.trim_start()
        } else {
            collapsed.as_str()
        };
        buf.push_str(text);
    }

    /// 現在行に内容があれば改行する。空行やリスト記号だけの行では改行しない
    /// （`<li><p>` で記号と本文が別の行に分かれるのを防ぐ）
    fn ensure_line_break(&mut self) {
        let buf = self.buf();
        let line = buf.rsplit('\n').next().unwrap_or("").trim();
        if line.is_empty() || is_list_marker(line) {
            return;
        }
        self.push_str("\n");
    }
}

/// リンク内で Markdown 記法に変換しないブロック要素
fn is_block_like(tag: &str) -> bool {
    matches!(
        tag,
        "h1" | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "p"
            | "br"
            | "hr"
            | "pre"
            | "blockquote"
            | "ul"
            | "ol"
            | "li"
            | "tr"
            | "td"
            | "th"
    ) || is_generic_block(tag)
}

fn is_list_marker(line: &str) -> bool {
    line == "-"
        || line
            .strip_suffix('.')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// 引用の内容の各行に `> ` を付ける。連続する空行は 1 行にまとめ、前後の空行は除く
fn quote_lines(content: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in content.lines() {
        let blank = line.trim().is_empty();
        if blank && lines.last().is_none_or(|l| l.trim().is_empty()) {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                ">".to_owned()
            } else {
                format!("> {}", l.trim_end())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// リンクテキストを `[...]` の中に置けるようにする。
/// `]` や改行がそのままだとリンク記法が途中で閉じたり段落が分かれたりする
fn escape_link_text(text: &str) -> String {
    let collapsed = collapse_whitespace(text);
    let mut out = String::with_capacity(collapsed.len());
    for c in collapsed.trim().chars() {
        if matches!(c, '\\' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn parse_tag(inner: &str) -> (String, &str, bool, bool) {
    let is_self_closing = inner.ends_with('/');
    let trimmed = if is_self_closing {
        &inner[..inner.len() - 1]
    } else {
        inner
    };
    let is_closing = trimmed.starts_with('/');
    let body = if is_closing { &trimmed[1..] } else { trimmed };
    let body = body.trim();

    let (tag_name, attrs) = body
        .split_once(|c: char| c.is_whitespace())
        .unwrap_or((body, ""));
    (
        tag_name.to_lowercase(),
        attrs.trim(),
        is_closing,
        is_self_closing,
    )
}

fn extract_attr(attrs: &str, name: &str) -> Option<String> {
    // href="..." または href='...' の両形式に対応
    for quote in &['"', '\''] {
        let search = format!("{}={}", name, quote);
        if let Some(start_idx) = attrs.find(&search) {
            let value_start = start_idx + search.len();
            if let Some(end_offset) = attrs[value_start..].find(*quote) {
                // readability が属性値を html_escape して出力するため、ここで対になるデコードをしないと
                // href の `&` が `&amp;` のまま URL になる
                return Some(html_decode(&attrs[value_start..value_start + end_offset]));
            }
        }
    }
    None
}

/// `&amp;` は最後に置換する。先に置換すると `&amp;lt;`（元テキストの文字列 `&lt;`）が `<` まで二重にデコードされる
fn html_decode(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

fn normalize_blank_lines(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut blank_count = 0u32;

    for line in s.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(line);
            result.push('\n');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{ExtractedLink, PageMetadata};
    use std::sync::mpsc;
    use std::time::Duration;

    fn content(body_html: &str, links: Vec<ExtractedLink>) -> ExtractedContent {
        ExtractedContent {
            url: url::Url::parse("https://example.com/page").unwrap(),
            title: "T".into(),
            byline: None,
            body_text: String::new(),
            body_html: body_html.into(),
            links,
            metadata: PageMetadata {
                description: None,
                og_title: None,
                og_image: None,
                canonical: None,
                published_at: None,
                author: None,
                date_modified: None,
            },
        }
    }

    fn fixed_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn md_doc(body_html: &str, links: Vec<ExtractedLink>) -> MarkdownDocument {
        MarkdownSerializer::new(MarkdownConfig::default())
            .serialize(&content(body_html, links), fixed_time())
    }

    /// core は時計を読まない。同じ入力からは同じ出力になる
    #[test]
    fn test_serialize_is_deterministic() {
        let a = md_doc("<p>x</p>", vec![]);
        let b = md_doc("<p>x</p>", vec![]);
        assert_eq!(a.extracted_at, fixed_time());
        assert_eq!(a.extracted_at, b.extracted_at);
        assert_eq!(a.content, b.content);
    }

    /// 本文部分だけを返す（タイトル・Source 行は除く）
    fn body_md(body_html: &str) -> String {
        let md = md_doc(body_html, vec![]).content;
        md.split_once("> Source: https://example.com/page\n\n")
            .unwrap()
            .1
            .to_owned()
    }

    #[test]
    fn test_unordered_list_uses_bullets() {
        let md = body_md("<ul><li>a</li><li>b</li></ul><ol><li>c</li></ol>");
        assert!(md.contains("- a\n- b\n"), "{md}");
        assert!(md.contains("1. c"), "{md}");
        assert!(!md.contains("1. a"), "{md}");
    }

    #[test]
    fn test_blockquote_prefixes_every_paragraph() {
        let md = body_md("<blockquote><p>first</p><p>second</p></blockquote><p>after</p>");
        assert!(md.contains("> first\n>\n> second"), "{md}");
        assert!(md.contains("\nafter"), "{md}");
        assert!(!md.contains("> after"), "{md}");
    }

    #[test]
    fn test_link_with_block_children_is_single_line() {
        let md = body_md(r#"<a href="/post"><h2>Title</h2><p>Lead</p></a>"#);
        assert!(
            md.contains("[Title Lead](https://example.com/post)"),
            "{md}"
        );
        assert!(!md.contains("##"), "{md}");
    }

    #[test]
    fn test_link_text_brackets_are_escaped() {
        let link = ExtractedLink {
            text: "a [b]\nc".into(),
            href: url::Url::parse("https://example.com/x").unwrap(),
            rel: None,
        };
        let md = md_doc(r#"<p><a href="/x">see [1]</a></p>"#, vec![link]).content;
        assert!(md.contains(r"[see \[1\]](https://example.com/x)"), "{md}");
        assert!(
            md.contains(r"1. [a \[b\] c](https://example.com/x)"),
            "{md}"
        );
    }

    #[test]
    fn test_unclosed_lt_terminates() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(body_md("a < b"));
        });
        let md = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("html_to_markdown が 5 秒以内に終わらない");
        assert!(md.contains("a < b"), "{md}");
    }

    #[test]
    fn test_div_and_table_cells_are_separated() {
        let md = body_md(
            "<div>alpha</div><div>beta</div><table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>",
        );
        assert!(!md.contains("alphabeta"), "{md}");
        assert!(md.contains("a | b\nc | d"), "{md}");
    }

    #[test]
    fn test_whitespace_between_inline_elements_is_kept() {
        let md = body_md("<p><b>foo</b> <i>bar</i></p>");
        assert!(md.contains("**foo** *bar*"), "{md}");
    }

    #[test]
    fn test_unsafe_scheme_link_is_plain_text() {
        let md = body_md(r#"<p><a href="javascript:alert(1)">click</a></p>"#);
        assert!(!md.contains("javascript:"), "{md}");
        assert!(md.contains("click"), "{md}");
    }
}
