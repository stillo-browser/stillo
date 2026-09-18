use crate::document::{ExtractedContent, MarkdownDocument};
use chrono::Utc;

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
    pub fn serialize(&self, content: &ExtractedContent) -> MarkdownDocument {
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
                out.push_str(&format!("{}. [{}]({})\n", i + 1, link.text, link.href));
            }
        }

        MarkdownDocument {
            content: out,
            source_url: content.url.clone(),
            extracted_at: Utc::now(),
        }
    }

    fn html_to_markdown(&self, html: &str, base_url: &url::Url) -> String {
        let mut converter = HtmlToMarkdown::new(self.config.include_links, base_url.clone());
        converter.convert(html);
        normalize_blank_lines(&converter.output)
    }
}

struct HtmlToMarkdown {
    output: String,
    include_links: bool,
    base_url: url::Url,
    /// リンク処理中: (href, collected_text)
    link_stack: Vec<(String, String)>,
    list_depth: usize,
    ordered_counters: Vec<usize>,
}

impl HtmlToMarkdown {
    fn new(include_links: bool, base_url: url::Url) -> Self {
        Self {
            output: String::new(),
            include_links,
            base_url,
            link_stack: Vec::new(),
            list_depth: 0,
            ordered_counters: Vec::new(),
        }
    }

    fn convert(&mut self, html: &str) {
        let mut pos = 0;
        let bytes = html.as_bytes();

        while pos < html.len() {
            if bytes[pos] == b'<' {
                if let Some(close_offset) = html[pos..].find('>') {
                    let inner = &html[pos + 1..pos + close_offset];
                    let (tag, attrs_str, is_closing, is_self_closing) = parse_tag(inner);
                    self.handle_tag(&tag, attrs_str, is_closing, is_self_closing);
                    pos += close_offset + 1;
                    continue;
                }
            }

            // テキストノード
            let next = html[pos..].find('<').map(|i| pos + i).unwrap_or(html.len());
            let text = html_decode(&html[pos..next]);
            self.push_text(&text);
            pos = next;
        }
    }

    fn handle_tag(&mut self, tag: &str, attrs: &str, is_closing: bool, _is_self_closing: bool) {
        match (tag, is_closing) {
            ("h1", false) => self.push_str("\n# "),
            ("h2", false) => self.push_str("\n## "),
            ("h3", false) => self.push_str("\n### "),
            ("h4", false) => self.push_str("\n#### "),
            ("h5", false) => self.push_str("\n##### "),
            ("h6", false) => self.push_str("\n###### "),
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", true) => self.push_str("\n\n"),

            ("p", false) => self.push_str("\n"),
            ("p", true) => self.push_str("\n\n"),

            ("br", _) => self.push_str("\n"),
            ("hr", _) => self.push_str("\n---\n"),

            ("strong" | "b", false) => self.push_str("**"),
            ("strong" | "b", true) => self.push_str("**"),
            ("em" | "i", false) => self.push_str("*"),
            ("em" | "i", true) => self.push_str("*"),
            ("code", false) => self.push_str("`"),
            ("code", true) => self.push_str("`"),

            ("pre", false) => self.push_str("\n```\n"),
            ("pre", true) => self.push_str("\n```\n\n"),

            ("blockquote", false) => self.push_str("\n> "),
            ("blockquote", true) => self.push_str("\n"),

            ("ul", false) => {
                self.list_depth += 1;
                self.ordered_counters.push(0);
            }
            ("ul", true) => {
                self.list_depth = self.list_depth.saturating_sub(1);
                self.ordered_counters.pop();
                self.push_str("\n");
            }
            ("ol", false) => {
                self.list_depth += 1;
                self.ordered_counters.push(0);
            }
            ("ol", true) => {
                self.list_depth = self.list_depth.saturating_sub(1);
                self.ordered_counters.pop();
                self.push_str("\n");
            }
            ("li", false) => {
                let indent = "  ".repeat(self.list_depth.saturating_sub(1));
                let counter_val = self.ordered_counters.last_mut().map(|c| {
                    *c += 1;
                    *c
                });
                match counter_val {
                    Some(n) => self.push_str(&format!("\n{}{}. ", indent, n)),
                    None => self.push_str(&format!("\n{}- ", indent)),
                }
            }
            ("li", true) => {}

            ("a", false) if self.include_links => {
                let raw_href = extract_attr(attrs, "href").unwrap_or_default();
                // 相対URLは base_url を基に絶対URLへ解決する
                let href = if raw_href.is_empty() {
                    raw_href
                } else {
                    self.base_url
                        .join(&raw_href)
                        .map(|u| u.to_string())
                        .unwrap_or(raw_href)
                };
                self.link_stack.push((href, String::new()));
            }
            ("a", true) if self.include_links => {
                if let Some((href, text)) = self.link_stack.pop() {
                    let md_link = format!("[{}]({})", text.trim(), href);
                    self.push_str(&md_link);
                }
            }

            // ノイズタグは無視
            ("script" | "style" | "noscript" | "iframe", _) => {}

            _ => {}
        }
    }

    fn push_str(&mut self, s: &str) {
        if let Some((_, ref mut text)) = self.link_stack.last_mut() {
            text.push_str(s);
        } else {
            self.output.push_str(s);
        }
    }

    fn push_text(&mut self, text: &str) {
        if let Some((_, ref mut link_text)) = self.link_stack.last_mut() {
            link_text.push_str(text);
        } else {
            self.output.push_str(text);
        }
    }
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
                return Some(attrs[value_start..value_start + end_offset].to_owned());
            }
        }
    }
    None
}

fn html_decode(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
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
