use crate::ast::{Block, Document, Inline};

/// HTML 文字列を意味的 AST に変換する（純粋関数）。
/// 副作用なし・I/O なし・HTTP なし。
pub fn parse_html_to_ast(html: &str, base_url: &url::Url) -> Document {
    let mut parser = Parser::new(base_url.clone());
    parser.parse(html);
    parser.finish()
}

struct Parser {
    doc: Document,
    base_url: url::Url,
    // ブロックコンテキスト
    heading_level: Option<u8>,
    in_para: bool,
    in_blockquote: bool,
    in_pre: bool,
    pre_lang: Option<String>,
    pre_text: String,
    // リストスタック
    list_depth: usize,
    list_ordered: Vec<bool>,
    list_counters: Vec<usize>,
    in_list_item: bool,
    // インライン収集
    current_inlines: Vec<Inline>,
    current_text: String,
    // インラインフォーマット
    bold: bool,
    italic: bool,
    in_inline_code: bool,
    // リンク: (href, accumulated_text)
    link_stack: Vec<(String, String)>,
    // script/style スキップ用
    skip_tag: Option<String>,
    skip_depth: usize,
}

impl Parser {
    fn new(base_url: url::Url) -> Self {
        Self {
            doc: Document::default(),
            base_url,
            heading_level: None,
            in_para: false,
            in_blockquote: false,
            in_pre: false,
            pre_lang: None,
            pre_text: String::new(),
            list_depth: 0,
            list_ordered: Vec::new(),
            list_counters: Vec::new(),
            in_list_item: false,
            current_inlines: Vec::new(),
            current_text: String::new(),
            bold: false,
            italic: false,
            in_inline_code: false,
            link_stack: Vec::new(),
            skip_tag: None,
            skip_depth: 0,
        }
    }

    fn parse(&mut self, html: &str) {
        let mut pos = 0;
        let bytes = html.as_bytes();

        while pos < html.len() {
            if bytes[pos] == b'<' {
                if let Some(close_offset) = html[pos..].find('>') {
                    let inner = &html[pos + 1..pos + close_offset];
                    let (tag, attrs, is_closing, is_self_closing) = parse_tag_inner(inner);
                    if is_closing {
                        self.handle_close_tag(&tag);
                    } else {
                        self.handle_open_tag(&tag, attrs, is_self_closing);
                    }
                    pos += close_offset + 1;
                    continue;
                }
            }
            let next = html[pos..].find('<').map(|i| pos + i).unwrap_or(html.len());
            let text = html_decode(&html[pos..next]);
            if !text.is_empty() {
                self.handle_text(&text);
            }
            pos = next;
        }
    }

    fn handle_open_tag(&mut self, tag: &str, attrs: &str, _is_self_closing: bool) {
        // スキップ中は深さ管理のみ
        if let Some(ref skip) = self.skip_tag.clone() {
            if tag == skip {
                self.skip_depth += 1;
            }
            return;
        }

        match tag {
            "script" | "style" | "noscript" => {
                self.skip_tag = Some(tag.to_owned());
                self.skip_depth = 1;
            }
            "h1" => {
                self.push_current_block();
                self.heading_level = Some(1);
                self.in_para = true;
            }
            "h2" => {
                self.push_current_block();
                self.heading_level = Some(2);
                self.in_para = true;
            }
            "h3" => {
                self.push_current_block();
                self.heading_level = Some(3);
                self.in_para = true;
            }
            "h4" => {
                self.push_current_block();
                self.heading_level = Some(4);
                self.in_para = true;
            }
            "h5" => {
                self.push_current_block();
                self.heading_level = Some(5);
                self.in_para = true;
            }
            "h6" => {
                self.push_current_block();
                self.heading_level = Some(6);
                self.in_para = true;
            }
            "p" => {
                self.push_current_block();
                self.in_para = true;
            }
            "br" => {
                self.flush_text();
                self.current_inlines.push(Inline::SoftBreak);
            }
            "hr" => {
                self.push_current_block();
                self.doc.blocks.push(Block::Rule);
            }
            "ul" => {
                self.push_current_block();
                self.list_depth += 1;
                self.list_ordered.push(false);
                self.list_counters.push(0);
            }
            "ol" => {
                self.push_current_block();
                self.list_depth += 1;
                self.list_ordered.push(true);
                self.list_counters.push(0);
            }
            "li" => {
                self.push_current_block();
                if let Some(c) = self.list_counters.last_mut() {
                    *c += 1;
                }
                self.in_list_item = true;
                self.in_para = true;
            }
            "pre" => {
                self.push_current_block();
                self.in_pre = true;
                self.pre_text = String::new();
            }
            "code" if self.in_pre => {
                // pre 内 code の lang を class 属性から取得する
                if let Some(class) = extract_attr(attrs, "class") {
                    // "language-xxx" または "lang-xxx" 形式に対応
                    for part in class.split_whitespace() {
                        if let Some(lang) = part.strip_prefix("language-") {
                            self.pre_lang = Some(lang.to_owned());
                            break;
                        } else if let Some(lang) = part.strip_prefix("lang-") {
                            self.pre_lang = Some(lang.to_owned());
                            break;
                        }
                    }
                }
            }
            "code" => {
                self.flush_text();
                self.in_inline_code = true;
            }
            "blockquote" => {
                self.push_current_block();
                self.in_blockquote = true;
                self.in_para = true;
            }
            "strong" | "b" => {
                self.flush_text();
                self.bold = true;
            }
            "em" | "i" => {
                self.flush_text();
                self.italic = true;
            }
            "a" => {
                self.flush_text();
                let raw_href = extract_attr(attrs, "href").unwrap_or_default();
                // ページ内アンカー（#section / 同一ページの fragment）はリンクとして扱わない。
                // 空 href をセンチネルとして積み、</a> でテキストを平文に戻す。
                let href = if raw_href.is_empty() || self.is_page_anchor(&raw_href) {
                    String::new()
                } else {
                    self.base_url
                        .join(&raw_href)
                        .map(|u| u.to_string())
                        .unwrap_or(raw_href)
                };
                self.link_stack.push((href, String::new()));
            }
            _ => {}
        }
    }

    fn handle_close_tag(&mut self, tag: &str) {
        // スキップ中の終了タグ処理
        if let Some(ref skip) = self.skip_tag.clone() {
            if tag == skip {
                self.skip_depth -= 1;
                if self.skip_depth == 0 {
                    self.skip_tag = None;
                }
            }
            return;
        }

        match tag {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.push_current_block();
                self.in_para = false;
            }
            "p" => {
                self.push_current_block();
                self.in_para = false;
            }
            "ul" | "ol" => {
                self.push_current_block();
                if self.list_depth > 0 {
                    self.list_depth -= 1;
                }
                self.list_ordered.pop();
                self.list_counters.pop();
            }
            "li" => {
                self.push_current_block();
                self.in_list_item = false;
                self.in_para = false;
            }
            "pre" => {
                let content = std::mem::take(&mut self.pre_text);
                let lang = self.pre_lang.take();
                self.doc.blocks.push(Block::CodeBlock { lang, content });
                self.in_pre = false;
            }
            "code" if self.in_inline_code => {
                let text = std::mem::take(&mut self.current_text);
                if let Some((_, ref mut link_text)) = self.link_stack.last_mut() {
                    link_text.push_str(&text);
                } else if !text.is_empty() {
                    self.current_inlines.push(Inline::Code(text));
                }
                self.in_inline_code = false;
            }
            "blockquote" => {
                self.push_current_block();
                self.in_blockquote = false;
                self.in_para = false;
            }
            "strong" | "b" => {
                self.flush_text();
                self.bold = false;
            }
            "em" | "i" => {
                self.flush_text();
                self.italic = false;
            }
            "a" => {
                self.flush_text();
                if let Some((href, text)) = self.link_stack.pop() {
                    if href.is_empty() {
                        // ページ内アンカー: テキストを平文として復元する
                        if !text.is_empty() {
                            self.current_inlines.push(Inline::Text(text));
                        }
                    } else if !text.is_empty() {
                        self.current_inlines.push(Inline::Link { text, href });
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_text(&mut self, text: &str) {
        if self.skip_tag.is_some() {
            return;
        }
        if self.in_pre {
            self.pre_text.push_str(text);
            return;
        }
        if self.in_inline_code {
            self.current_text.push_str(text);
            return;
        }
        if let Some((_, ref mut link_text)) = self.link_stack.last_mut() {
            link_text.push_str(text);
            return;
        }
        self.current_text.push_str(text);
    }

    /// current_text を適切な Inline variant に変換して current_inlines に追加する
    fn flush_text(&mut self) {
        let text = std::mem::take(&mut self.current_text);
        if text.is_empty() {
            return;
        }
        let inline = match (self.bold, self.italic) {
            (true, true) => Inline::BoldItalic(text),
            (true, false) => Inline::Bold(text),
            (false, true) => Inline::Italic(text),
            (false, false) => Inline::Text(text),
        };
        self.current_inlines.push(inline);
    }

    /// flush_text 後に current_inlines を適切な Block にしてドキュメントへ追加する
    fn push_current_block(&mut self) {
        self.flush_text();
        let inlines = std::mem::take(&mut self.current_inlines);
        if inlines.is_empty() {
            // heading_level はクリアが必要（インラインなしのヘッダーでも）
            self.heading_level = None;
            return;
        }
        let block = if let Some(level) = self.heading_level.take() {
            Block::Heading { level, inlines }
        } else if self.in_list_item {
            let depth = self.list_depth;
            let ordered = self.list_ordered.last().copied().unwrap_or(false);
            let number = self.list_counters.last().copied().unwrap_or(1);
            self.in_list_item = false;
            Block::ListItem {
                depth,
                ordered,
                number,
                inlines,
            }
        } else if self.in_blockquote {
            Block::Blockquote(inlines)
        } else {
            Block::Paragraph(inlines)
        };
        self.doc.blocks.push(block);
    }

    /// raw_href がページ内アンカーかどうか判定する。
    /// "#section" 形式、または解決後 URL が同一ページのフラグメント差分のみの場合に true。
    fn is_page_anchor(&self, raw_href: &str) -> bool {
        if raw_href.starts_with('#') {
            return true;
        }
        // 絶対 URL に解決してフラグメントの有無と同一ページかを確認する
        if let Ok(resolved) = self.base_url.join(raw_href) {
            if resolved.fragment().is_some() {
                let mut no_frag = resolved.clone();
                no_frag.set_fragment(None);
                return no_frag == self.base_url;
            }
        }
        false
    }

    fn finish(mut self) -> Document {
        self.push_current_block();
        self.doc
    }
}

fn parse_tag_inner(inner: &str) -> (String, &str, bool, bool) {
    let is_self_closing = inner.ends_with('/');
    let trimmed = if is_self_closing {
        &inner[..inner.len() - 1]
    } else {
        inner
    };
    let is_closing = trimmed.starts_with('/');
    let body = if is_closing { &trimmed[1..] } else { trimmed }.trim();
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
