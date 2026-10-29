use crate::ast::{Block, Document, Inline};
use crate::sanitize::is_navigable_url;

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
    /// 直前に出力した文字が空白（またはブロック先頭）か。空白の連続と行頭の空白を畳むために使う
    last_was_space: bool,
    /// 現在の表の行で開いたセル数。2 つ目以降のセルの前に区切りを入れる
    cell_count: usize,
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
            last_was_space: true,
            cell_count: 0,
        }
    }

    fn parse(&mut self, html: &str) {
        let mut pos = 0;
        let bytes = html.as_bytes();

        while pos < html.len() {
            let is_lt = bytes[pos] == b'<';
            if is_lt && is_tag_start(bytes.get(pos + 1)) {
                let Some(close_offset) = html[pos..].find('>') else {
                    // 閉じる `>` がないので以降はタグではない。テキストとして読んで終える
                    self.handle_text(&html_decode(&html[pos..]));
                    break;
                };
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
            // タグにならない `<`（`a < b` など）は文字として読み進める。
            // 次の `<` を pos から探すと同じ位置に留まり無限ループになるため 1 文字先から探す
            let search_from = if is_lt { pos + 1 } else { pos };
            let next = html[search_from..]
                .find('<')
                .map(|i| search_from + i)
                .unwrap_or(html.len());
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
                self.last_was_space = true;
            }
            "tr" => {
                self.push_current_block();
                self.cell_count = 0;
            }
            "td" | "th" => {
                self.flush_text();
                self.trim_trailing_space();
                // 空セルの後に区切りだけが並ばないよう、行に内容があるときだけ区切る
                if self.cell_count > 0 && !self.current_inlines.is_empty() {
                    self.current_inlines.push(Inline::Text(" | ".to_owned()));
                }
                self.cell_count += 1;
                self.last_was_space = true;
            }
            _ if is_generic_block(tag) => self.push_current_block(),
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
                // javascript: / data: 等も同じく平文扱いにする（辿れず、開くとスクリプトが動きうるため）
                let href = if raw_href.is_empty() || self.is_page_anchor(&raw_href) {
                    String::new()
                } else {
                    match self.base_url.join(&raw_href) {
                        Ok(u) if is_navigable_url(&u) => u.to_string(),
                        _ => String::new(),
                    }
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
            "tr" => self.push_current_block(),
            "td" | "th" => {
                self.flush_text();
                self.trim_trailing_space();
            }
            _ if is_generic_block(tag) => self.push_current_block(),
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
        // HTML の空白の扱いに合わせ、連続する空白（改行含む）は 1 つに畳み、行頭の空白は捨てる
        let collapsed = collapse_whitespace(text);
        let text = if self.last_was_space {
            collapsed.trim_start()
        } else {
            collapsed.as_str()
        };
        if text.is_empty() {
            return;
        }
        self.last_was_space = text.ends_with(' ');
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

    /// 行末の空白を除く。ブロック末尾・セル末尾に畳み残した空白が出力に残らないようにする
    fn trim_trailing_space(&mut self) {
        while let Some(last) = self.current_inlines.last_mut() {
            let s = match last {
                Inline::Text(s) | Inline::Bold(s) | Inline::Italic(s) | Inline::BoldItalic(s) => s,
                _ => return,
            };
            let trimmed_len = s.trim_end().len();
            if trimmed_len > 0 {
                s.truncate(trimmed_len);
                return;
            }
            self.current_inlines.pop();
        }
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
        self.trim_trailing_space();
        self.last_was_space = true;
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

/// `<` の次の文字がタグ・コメント・宣言の開始になりうるか
pub(crate) fn is_tag_start(next: Option<&u8>) -> bool {
    matches!(next, Some(c) if c.is_ascii_alphabetic() || matches!(c, b'/' | b'!' | b'?'))
}

/// 段落の区切りとして扱うブロック要素（専用の処理を持つ p/li/h*/pre/blockquote/表要素以外）。
/// 区切らないと `<div>alpha</div><div>beta</div>` が `alphabeta` になる
pub(crate) fn is_generic_block(tag: &str) -> bool {
    matches!(
        tag,
        "div"
            | "section"
            | "article"
            | "main"
            | "header"
            | "footer"
            | "aside"
            | "nav"
            | "figure"
            | "figcaption"
            | "dl"
            | "dt"
            | "dd"
            | "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "caption"
            | "address"
            | "details"
            | "summary"
    )
}

/// 連続する空白文字（改行・タブを含む）を半角空白 1 つに畳む
pub(crate) fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() && c != '\u{a0}' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn base() -> url::Url {
        url::Url::parse("https://example.com/page").unwrap()
    }

    fn plain(inlines: &[Inline]) -> String {
        inlines
            .iter()
            .map(|i| match i {
                Inline::Text(s)
                | Inline::Bold(s)
                | Inline::Italic(s)
                | Inline::BoldItalic(s)
                | Inline::Code(s) => s.as_str(),
                Inline::Link { text, .. } => text.as_str(),
                Inline::SoftBreak => "\n",
            })
            .collect()
    }

    fn block_texts(doc: &Document) -> Vec<String> {
        doc.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph(i) | Block::Blockquote(i) => Some(plain(i)),
                Block::Heading { inlines, .. } | Block::ListItem { inlines, .. } => {
                    Some(plain(inlines))
                }
                _ => None,
            })
            .collect()
    }

    /// 無限ループ時にテスト全体を止めないよう、別スレッドで実行して時間制限を設ける
    fn parse_with_timeout(html: &'static str) -> Document {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(parse_html_to_ast(html, &base()));
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("parse_html_to_ast が 5 秒以内に終わらない")
    }

    #[test]
    fn test_unclosed_lt_terminates() {
        let doc = parse_with_timeout("a < b");
        assert_eq!(block_texts(&doc), ["a < b"]);
        let doc = parse_with_timeout("<p>x</p><");
        assert_eq!(block_texts(&doc), ["x", "<"]);
    }

    #[test]
    fn test_whitespace_between_inline_elements_is_kept() {
        let doc = parse_html_to_ast("<p><b>foo</b> <i>bar</i></p>", &base());
        assert_eq!(block_texts(&doc), ["foo bar"]);
    }

    #[test]
    fn test_div_is_block_boundary() {
        let doc = parse_html_to_ast("<div>alpha</div><div>beta</div>", &base());
        assert_eq!(block_texts(&doc), ["alpha", "beta"]);
    }

    #[test]
    fn test_table_rows_become_paragraphs() {
        let doc = parse_html_to_ast(
            "<table><tbody><tr><td>a</td><td>b</td></tr><tr><th>c</th><td>d</td></tr></tbody></table>",
            &base(),
        );
        assert_eq!(block_texts(&doc), ["a | b", "c | d"]);
    }

    #[test]
    fn test_unsafe_scheme_link_is_plain_text() {
        let doc = parse_html_to_ast(
            r#"<p><a href="javascript:alert(1)">js</a> <a href="data:text/html,x">data</a> <a href="mailto:a@example.com">mail</a></p>"#,
            &base(),
        );
        let Block::Paragraph(inlines) = &doc.blocks[0] else {
            panic!("{:?}", doc.blocks)
        };
        let hrefs: Vec<&str> = inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Link { href, .. } => Some(href.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(hrefs, ["mailto:a@example.com"]);
        assert_eq!(plain(inlines), "js data mail");
    }
}
