use crate::{
    ast::{Block, Document, Inline},
    document::{BrowsePage, ExtractedLink},
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use url::Url;

/// Markdown テキストを BrowsePage に変換する。
pub fn parse_markdown_to_ast(text: &str, url: &Url) -> BrowsePage {
    let options = Options::all();
    let parser = Parser::new_ext(text, options);

    let mut blocks: Vec<Block> = Vec::new();
    let mut links: Vec<ExtractedLink> = Vec::new();
    let mut first_h1: Option<String> = None;

    // ネストした状態を管理するためのスタック
    let mut context = ParseContext::new();

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                context.enter_heading(level);
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(block) = context.exit_heading() {
                    // 最初の H1 をページタイトルとして記録する
                    if let Block::Heading {
                        level: 1,
                        ref inlines,
                    } = block
                    {
                        if first_h1.is_none() {
                            first_h1 = Some(inlines_to_plain(inlines));
                        }
                    }
                    blocks.push(block);
                }
            }

            Event::Start(Tag::Paragraph) => {
                context.enter_paragraph();
            }
            Event::End(TagEnd::Paragraph) => {
                if let Some(block) = context.exit_paragraph() {
                    blocks.push(block);
                }
            }

            Event::Start(Tag::List(ordered)) => {
                context.enter_list(ordered.is_some());
            }
            Event::End(TagEnd::List(_)) => {
                context.exit_list();
            }

            Event::Start(Tag::Item) => {
                context.enter_item();
            }
            Event::End(TagEnd::Item) => {
                if let Some(block) = context.exit_item() {
                    blocks.push(block);
                }
            }

            Event::Start(Tag::CodeBlock(kind)) => {
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
                context.enter_code_block(lang);
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(block) = context.exit_code_block() {
                    blocks.push(block);
                }
            }

            Event::Start(Tag::BlockQuote(_)) => {
                context.enter_blockquote();
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                if let Some(block) = context.exit_blockquote() {
                    blocks.push(block);
                }
            }

            Event::Start(Tag::Strong) => {
                context.enter_strong();
            }
            Event::End(TagEnd::Strong) => {
                context.exit_strong();
            }

            Event::Start(Tag::Emphasis) => {
                context.enter_emphasis();
            }
            Event::End(TagEnd::Emphasis) => {
                context.exit_emphasis();
            }

            Event::Start(Tag::Link { dest_url, .. }) => {
                context.enter_link(dest_url.into_string());
            }
            Event::End(TagEnd::Link) => {
                if let Some((inline, extracted)) = context.exit_link(url) {
                    if let Some(link) = extracted {
                        links.push(link);
                    }
                    context.push_inline(inline);
                }
            }

            Event::Code(t) => {
                context.push_inline(Inline::Code(t.into_string()));
            }

            Event::Text(t) => {
                let s = t.into_string();
                if context.in_code_block() {
                    context.append_code(&s);
                } else if context.in_strong() && context.in_emphasis() {
                    context.push_inline(Inline::BoldItalic(s));
                } else if context.in_strong() {
                    context.push_inline(Inline::Bold(s));
                } else if context.in_emphasis() {
                    context.push_inline(Inline::Italic(s));
                } else if context.in_link() {
                    context.append_link_text(&s);
                } else {
                    context.push_inline(Inline::Text(s));
                }
            }

            Event::SoftBreak => {
                context.push_inline(Inline::SoftBreak);
            }
            Event::HardBreak => {
                context.push_inline(Inline::SoftBreak);
            }

            Event::Rule => {
                blocks.push(Block::Rule);
            }

            _ => {}
        }
    }

    // H1 がなければ URL パス部分をタイトルとして使う
    let title = first_h1.unwrap_or_else(|| url.path().trim_matches('/').to_string());

    BrowsePage {
        title,
        url: url.clone(),
        doc: Document { blocks },
        links,
        markdown: text.to_string(),
    }
}

// ---------------------------------------------------------------------------
// パース状態管理
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum ContextKind {
    Heading(HeadingLevel),
    Paragraph,
    ListItem {
        depth: usize,
        ordered: bool,
        number: usize,
    },
    CodeBlock {
        lang: Option<String>,
    },
    Blockquote,
}

struct ParseContext {
    stack: Vec<ContextKind>,
    /// 現在のインラインを蓄積するバッファ
    inline_buf: Vec<Inline>,
    /// ネストしたリストの深さとordered状態を管理する
    list_stack: Vec<(bool, usize)>,
    /// コードブロックの内容バッファ
    code_buf: String,
    strong_depth: usize,
    emphasis_depth: usize,
    link_dest: Option<String>,
    link_text_buf: String,
}

impl ParseContext {
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            inline_buf: Vec::new(),
            list_stack: Vec::new(),
            code_buf: String::new(),
            strong_depth: 0,
            emphasis_depth: 0,
            link_dest: None,
            link_text_buf: String::new(),
        }
    }

    fn in_code_block(&self) -> bool {
        self.stack
            .iter()
            .any(|k| matches!(k, ContextKind::CodeBlock { .. }))
    }

    fn in_strong(&self) -> bool {
        self.strong_depth > 0
    }

    fn in_emphasis(&self) -> bool {
        self.emphasis_depth > 0
    }

    fn in_link(&self) -> bool {
        self.link_dest.is_some()
    }

    fn push_inline(&mut self, inline: Inline) {
        self.inline_buf.push(inline);
    }

    fn append_code(&mut self, s: &str) {
        self.code_buf.push_str(s);
    }

    fn append_link_text(&mut self, s: &str) {
        self.link_text_buf.push_str(s);
    }

    fn enter_heading(&mut self, level: HeadingLevel) {
        self.stack.push(ContextKind::Heading(level));
    }

    fn exit_heading(&mut self) -> Option<Block> {
        if let Some(ContextKind::Heading(level)) = self.stack.pop() {
            let inlines = std::mem::take(&mut self.inline_buf);
            let level_u8 = heading_level_to_u8(level);
            Some(Block::Heading {
                level: level_u8,
                inlines,
            })
        } else {
            None
        }
    }

    fn enter_paragraph(&mut self) {
        self.stack.push(ContextKind::Paragraph);
    }

    fn exit_paragraph(&mut self) -> Option<Block> {
        if let Some(ContextKind::Paragraph) = self.stack.pop() {
            let inlines = std::mem::take(&mut self.inline_buf);
            if inlines.is_empty() {
                None
            } else {
                Some(Block::Paragraph(inlines))
            }
        } else {
            None
        }
    }

    fn enter_list(&mut self, ordered: bool) {
        // リストのネスト深さと番号を管理する
        self.list_stack.push((ordered, 1));
    }

    fn exit_list(&mut self) {
        self.list_stack.pop();
    }

    fn enter_item(&mut self) {
        let depth = self.list_stack.len();
        // リストスタックが空の場合は番号なしアイテムとして扱う
        let (ordered, number) = if let Some(last) = self.list_stack.last_mut() {
            let ordered = last.0;
            let number = last.1;
            if ordered {
                last.1 += 1;
            }
            (ordered, number)
        } else {
            (false, 1)
        };
        self.stack.push(ContextKind::ListItem {
            depth,
            ordered,
            number,
        });
    }

    fn exit_item(&mut self) -> Option<Block> {
        if let Some(ContextKind::ListItem {
            depth,
            ordered,
            number,
        }) = self.stack.pop()
        {
            let inlines = std::mem::take(&mut self.inline_buf);
            Some(Block::ListItem {
                depth,
                ordered,
                number,
                inlines,
            })
        } else {
            None
        }
    }

    fn enter_code_block(&mut self, lang: Option<String>) {
        self.code_buf.clear();
        self.stack.push(ContextKind::CodeBlock { lang });
    }

    fn exit_code_block(&mut self) -> Option<Block> {
        if let Some(ContextKind::CodeBlock { lang }) = self.stack.pop() {
            let content = std::mem::take(&mut self.code_buf);
            // 末尾の改行を除去する（pulldown_cmark が末尾に \n を付けることが多いため）
            let content = content.trim_end_matches('\n').to_string();
            Some(Block::CodeBlock { lang, content })
        } else {
            None
        }
    }

    fn enter_blockquote(&mut self) {
        self.stack.push(ContextKind::Blockquote);
    }

    fn exit_blockquote(&mut self) -> Option<Block> {
        if let Some(ContextKind::Blockquote) = self.stack.pop() {
            let inlines = std::mem::take(&mut self.inline_buf);
            Some(Block::Blockquote(inlines))
        } else {
            None
        }
    }

    fn enter_strong(&mut self) {
        self.strong_depth += 1;
    }

    fn exit_strong(&mut self) {
        self.strong_depth = self.strong_depth.saturating_sub(1);
    }

    fn enter_emphasis(&mut self) {
        self.emphasis_depth += 1;
    }

    fn exit_emphasis(&mut self) {
        self.emphasis_depth = self.emphasis_depth.saturating_sub(1);
    }

    fn enter_link(&mut self, dest: String) {
        self.link_dest = Some(dest);
        self.link_text_buf.clear();
    }

    /// リンク終了時に Inline::Link を生成し、URL を絶対化する。
    fn exit_link(&mut self, base_url: &Url) -> Option<(Inline, Option<ExtractedLink>)> {
        let dest = self.link_dest.take()?;
        let text = std::mem::take(&mut self.link_text_buf);

        let href = match base_url.join(&dest) {
            Ok(u) => u,
            Err(_) => return Some((Inline::Text(text), None)),
        };

        let extracted = ExtractedLink {
            text: text.clone(),
            href: href.clone(),
            rel: None,
        };

        Some((
            Inline::Link {
                text,
                href: href.to_string(),
            },
            Some(extracted),
        ))
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
