use crate::{
    ast::{Block, Document, Inline},
    document::{BrowsePage, ExtractedLink},
    sanitize::is_navigable_url,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use url::Url;

/// Markdown テキストを BrowsePage に変換する。
pub fn parse_markdown_to_ast(text: &str, url: &Url) -> BrowsePage {
    let parser = Parser::new_ext(text, Options::all());
    let mut builder = AstBuilder::new(url);
    for event in parser {
        builder.event(event);
    }
    builder.flush_pending();

    // H1 がなければ URL パス部分をタイトルとして使う
    let title = builder
        .first_h1
        .unwrap_or_else(|| url.path().trim_matches('/').to_string());

    BrowsePage {
        title,
        url: url.clone(),
        doc: Document {
            blocks: builder.blocks,
        },
        links: builder.links,
        markdown: text.to_string(),
    }
}

// ---------------------------------------------------------------------------
// パース状態管理
// ---------------------------------------------------------------------------

/// 段落の出力先を決めるコンテナ。AST はフラットなので、段落を
/// 「リスト項目の本文」「引用」「通常の段落」のどれとして出すかをここで決める。
#[derive(Debug)]
enum Container {
    Item {
        depth: usize,
        ordered: bool,
        number: usize,
        /// 項目の 1 段落目を ListItem として出力済みか。2 段落目以降は Paragraph にする
        emitted: bool,
    },
    Quote,
}

/// pulldown-cmark のイベント列からフラットな Block 列を組み立てる。
///
/// インラインは段落・見出し・リスト項目などの終端（または子ブロックの開始）でまとめて出力する。
/// 子ブロックの開始時に溜まったインラインを先に出さないと、ネストしたリストで親項目のテキストが
/// 子項目に混入する。
struct AstBuilder<'a> {
    base_url: &'a Url,
    blocks: Vec<Block>,
    links: Vec<ExtractedLink>,
    first_h1: Option<String>,
    containers: Vec<Container>,
    /// 開いているリスト: (ordered, 次の番号)
    lists: Vec<(bool, usize)>,
    inlines: Vec<Inline>,
    heading: Option<u8>,
    /// コードブロック内: (言語, 内容)
    code: Option<(Option<String>, String)>,
    strong_depth: usize,
    emphasis_depth: usize,
    /// リンク内: (リンク先, テキスト)。Inline::Link は装飾を持てないので平文で集める
    link: Option<(String, String)>,
    /// 表の現在行で開いたセル数
    table_cells: usize,
}

impl<'a> AstBuilder<'a> {
    fn new(base_url: &'a Url) -> Self {
        Self {
            base_url,
            blocks: Vec::new(),
            links: Vec::new(),
            first_h1: None,
            containers: Vec::new(),
            lists: Vec::new(),
            inlines: Vec::new(),
            heading: None,
            code: None,
            strong_depth: 0,
            emphasis_depth: 0,
            link: None,
            table_cells: 0,
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                self.flush_pending();
                self.heading = Some(heading_level_to_u8(level));
            }
            Event::End(TagEnd::Heading(_)) => {
                let inlines = std::mem::take(&mut self.inlines);
                let level = self.heading.take().unwrap_or(1);
                // 最初の H1 をページタイトルとして記録する
                if level == 1 && self.first_h1.is_none() {
                    self.first_h1 = Some(inlines_to_plain(&inlines));
                }
                self.blocks.push(Block::Heading { level, inlines });
            }

            Event::Start(Tag::Paragraph) => self.flush_pending(),
            Event::End(TagEnd::Paragraph) => self.flush_pending(),

            Event::Start(Tag::List(first)) => {
                self.flush_pending();
                let start = first.map_or(1, |n| n as usize);
                self.lists.push((first.is_some(), start));
            }
            Event::End(TagEnd::List(_)) => {
                self.lists.pop();
            }

            Event::Start(Tag::Item) => {
                self.flush_pending();
                let depth = self.lists.len();
                let (ordered, number) = match self.lists.last_mut() {
                    Some((ordered, next)) => {
                        let n = *next;
                        *next += 1;
                        (*ordered, n)
                    }
                    None => (false, 1),
                };
                self.containers.push(Container::Item {
                    depth,
                    ordered,
                    number,
                    emitted: false,
                });
            }
            Event::End(TagEnd::Item) => {
                self.flush_pending();
                self.containers.pop();
            }

            Event::Start(Tag::CodeBlock(kind)) => {
                self.flush_pending();
                let lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(ref s) => {
                        let s = s.trim();
                        if s.is_empty() {
                            None
                        } else {
                            Some(s.to_string())
                        }
                    }
                    pulldown_cmark::CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, content)) = self.code.take() {
                    // 末尾の改行を除去する（pulldown_cmark が末尾に \n を付けることが多いため）
                    let content = content.trim_end_matches('\n').to_string();
                    self.blocks.push(Block::CodeBlock { lang, content });
                }
            }

            Event::Start(Tag::BlockQuote(_)) => {
                self.flush_pending();
                self.containers.push(Container::Quote);
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.flush_pending();
                self.containers.pop();
            }

            // 表は行単位の段落にする。セル境界で区切らないとセル同士や直後の段落と連結する
            Event::Start(Tag::Table(_)) => self.flush_pending(),
            Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => self.table_cells = 0,
            Event::Start(Tag::TableCell) => {
                if self.table_cells > 0 {
                    self.inlines.push(Inline::Text(" | ".to_owned()));
                }
                self.table_cells += 1;
            }
            Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => self.flush_pending(),

            Event::Start(Tag::Strong) => self.strong_depth += 1,
            Event::End(TagEnd::Strong) => self.strong_depth = self.strong_depth.saturating_sub(1),
            Event::Start(Tag::Emphasis) => self.emphasis_depth += 1,
            Event::End(TagEnd::Emphasis) => {
                self.emphasis_depth = self.emphasis_depth.saturating_sub(1)
            }

            Event::Start(Tag::Link { dest_url, .. }) => {
                self.link = Some((dest_url.into_string(), String::new()));
            }
            Event::End(TagEnd::Link) => self.exit_link(),

            Event::Code(t) => {
                if let Some((_, text)) = self.link.as_mut() {
                    text.push_str(&t);
                } else {
                    self.inlines.push(Inline::Code(t.into_string()));
                }
            }

            Event::Text(t) => {
                let s = t.into_string();
                // リンク判定を装飾判定より先に行う。逆だと `[**bold**](url)` のテキストが
                // リンク外の Bold になり、リンクテキストが空になる
                if let Some((_, code)) = self.code.as_mut() {
                    code.push_str(&s);
                } else if let Some((_, text)) = self.link.as_mut() {
                    text.push_str(&s);
                } else {
                    let inline = match (self.strong_depth > 0, self.emphasis_depth > 0) {
                        (true, true) => Inline::BoldItalic(s),
                        (true, false) => Inline::Bold(s),
                        (false, true) => Inline::Italic(s),
                        (false, false) => Inline::Text(s),
                    };
                    self.inlines.push(inline);
                }
            }

            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, text)) = self.link.as_mut() {
                    text.push(' ');
                } else {
                    self.inlines.push(Inline::SoftBreak);
                }
            }

            Event::Rule => {
                self.flush_pending();
                self.blocks.push(Block::Rule);
            }

            _ => {}
        }
    }

    /// 溜まっているインラインを、最も内側のコンテナに応じたブロックとして出力する
    fn flush_pending(&mut self) {
        if self.inlines.is_empty() {
            return;
        }
        let inlines = std::mem::take(&mut self.inlines);
        let block = match self.containers.last_mut() {
            Some(Container::Item {
                depth,
                ordered,
                number,
                emitted,
            }) if !*emitted => {
                *emitted = true;
                Block::ListItem {
                    depth: *depth,
                    ordered: *ordered,
                    number: *number,
                    inlines,
                }
            }
            Some(Container::Quote) => Block::Blockquote(inlines),
            _ => Block::Paragraph(inlines),
        };
        self.blocks.push(block);
    }

    /// リンク終了時に Inline::Link を生成し、URL を絶対化する。
    /// javascript: / data: 等の辿れない URL はリンクにせず平文として残す。
    fn exit_link(&mut self) {
        let Some((dest, text)) = self.link.take() else {
            return;
        };
        match self.base_url.join(&dest) {
            Ok(href) if is_navigable_url(&href) => {
                self.links.push(ExtractedLink {
                    text: text.clone(),
                    href: href.clone(),
                    rel: None,
                });
                self.inlines.push(Inline::Link {
                    text,
                    href: href.to_string(),
                });
            }
            _ => self.inlines.push(Inline::Text(text)),
        }
    }
}

fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn inlines_to_plain(inlines: &[Inline]) -> String {
    inlines
        .iter()
        .map(|i| match i {
            Inline::Text(s)
            | Inline::Bold(s)
            | Inline::Italic(s)
            | Inline::BoldItalic(s)
            | Inline::Code(s) => s.as_str(),
            Inline::Link { text, .. } => text.as_str(),
            Inline::SoftBreak => " ",
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(md: &str) -> Vec<Block> {
        parse_markdown_to_ast(md, &Url::parse("https://example.com/doc.md").unwrap())
            .doc
            .blocks
    }

    /// テスト比較用にブロックを (種別, 深さ, プレーンテキスト) へ要約する
    fn summary(blocks: &[Block]) -> Vec<(&'static str, usize, String)> {
        blocks
            .iter()
            .map(|b| match b {
                Block::Heading { level, inlines } => {
                    ("h", *level as usize, inlines_to_plain(inlines))
                }
                Block::Paragraph(i) => ("p", 0, inlines_to_plain(i)),
                Block::ListItem { depth, inlines, .. } => ("li", *depth, inlines_to_plain(inlines)),
                Block::CodeBlock { content, .. } => ("code", 0, content.clone()),
                Block::Blockquote(i) => ("quote", 0, inlines_to_plain(i)),
                Block::Rule => ("hr", 0, String::new()),
            })
            .collect()
    }

    fn s(kind: &'static str, depth: usize, text: &str) -> (&'static str, usize, String) {
        (kind, depth, text.to_owned())
    }

    #[test]
    fn test_blockquote_is_single_block() {
        assert_eq!(summary(&blocks("> quoted\n")), [s("quote", 0, "quoted")]);
    }

    #[test]
    fn test_nested_list_items_are_separate() {
        assert_eq!(
            summary(&blocks("- a\n  - b\n- c\n")),
            [s("li", 1, "a"), s("li", 2, "b"), s("li", 1, "c")]
        );
    }

    #[test]
    fn test_loose_list_has_no_empty_items() {
        assert_eq!(
            summary(&blocks("- a\n\n- b\n")),
            [s("li", 1, "a"), s("li", 1, "b")]
        );
    }

    #[test]
    fn test_table_rows_do_not_leak_into_next_paragraph() {
        assert_eq!(
            summary(&blocks("| h1 | h2 |\n|----|----|\n| a | b |\n\nafter\n")),
            [s("p", 0, "h1 | h2"), s("p", 0, "a | b"), s("p", 0, "after")]
        );
    }

    #[test]
    fn test_formatted_link_text_is_kept() {
        let page = parse_markdown_to_ast(
            "[**bold** and `code`](https://example.com/x)\n",
            &Url::parse("https://example.com/").unwrap(),
        );
        let Block::Paragraph(inlines) = &page.doc.blocks[0] else {
            panic!("{:?}", page.doc.blocks)
        };
        assert!(
            matches!(&inlines[..], [Inline::Link { text, .. }] if text == "bold and code"),
            "{inlines:?}"
        );
        assert_eq!(page.links[0].text, "bold and code");
    }

    #[test]
    fn test_unsafe_scheme_link_is_plain_text() {
        let page = parse_markdown_to_ast(
            "[x](javascript:alert(1))\n",
            &Url::parse("https://example.com/").unwrap(),
        );
        assert!(page.links.is_empty());
        assert_eq!(summary(&page.doc.blocks), [s("p", 0, "x")]);
    }
}
