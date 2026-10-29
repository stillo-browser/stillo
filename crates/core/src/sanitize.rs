//! 端末へ出力する文字列から制御文字を取り除く（純粋関数）。
//!
//! 取得したページ由来の文字列に ESC（0x1B）や C1 の CSI（U+009B）が含まれていると、
//! 端末がエスケープシーケンスとして解釈する。OSC 52 によるクリップボード書き換えや
//! 画面内容の偽装が可能になるため、TUI 描画・stdout 出力の前に必ず通す。

use crate::ast::{Block, Document, Inline};
use crate::document::BrowsePage;

/// TUI でタブ 1 文字を置き換える空白数。
/// ratatui はタブを幅 1 のセルとして書き込むが、端末はタブストップまでカーソルを進めるため列がずれる
const TAB_SPACES: &str = "    ";

/// 改行とタブ以外の制御文字（C0・DEL・C1）を除去する。stdout へのパイプ出力向け。
pub fn strip_control_chars(s: &str) -> String {
    if !s.chars().any(is_unsafe_control) {
        return s.to_owned();
    }
    s.chars().filter(|&c| !is_unsafe_control(c)).collect()
}

/// strip_control_chars に加えてタブを空白に展開する。TUI のセル描画向け。
pub fn sanitize_for_tui(s: &str) -> String {
    strip_control_chars(s).replace('\t', TAB_SPACES)
}

fn is_unsafe_control(c: char) -> bool {
    c.is_control() && c != '\n' && c != '\t'
}

/// リンクとして扱ってよい URL か（http / https / mailto のみ許可）。
///
/// `javascript:` や `data:` をリンクとして残すと、MCP 経由で受け取った LLM や
/// リンクを開く外部ブラウザがスクリプトを実行しうる。ターミナルブラウザが辿れるのも
/// HTTP(S) だけなので、許可リスト方式で絞る。
pub fn is_navigable_url(url: &url::Url) -> bool {
    matches!(url.scheme(), "http" | "https" | "mailto")
}

impl BrowsePage {
    /// 端末表示用に、ページ内の表示文字列すべてから制御文字を除去する。
    ///
    /// `markdown` は 'd' キーで stdout にそのまま書き出されるため、タブは残して制御文字だけを除く。
    /// `url` / `links[].href` は url クレートが制御文字をパーセントエンコードするため対象外。
    pub fn sanitize_for_terminal(mut self) -> Self {
        self.title = sanitize_for_tui(&self.title);
        sanitize_document(&mut self.doc);
        for link in &mut self.links {
            link.text = sanitize_for_tui(&link.text);
        }
        self.markdown = strip_control_chars(&self.markdown);
        self
    }
}

fn sanitize_document(doc: &mut Document) {
    for block in &mut doc.blocks {
        match block {
            Block::Heading { inlines, .. }
            | Block::Paragraph(inlines)
            | Block::ListItem { inlines, .. }
            | Block::Blockquote(inlines) => sanitize_inlines(inlines),
            Block::CodeBlock { lang, content } => {
                *content = sanitize_for_tui(content);
                if let Some(lang) = lang {
                    *lang = sanitize_for_tui(lang);
                }
            }
            Block::Rule => {}
        }
    }
}

fn sanitize_inlines(inlines: &mut [Inline]) {
    for inline in inlines {
        match inline {
            Inline::Text(s)
            | Inline::Bold(s)
            | Inline::Italic(s)
            | Inline::BoldItalic(s)
            | Inline::Code(s) => *s = sanitize_for_tui(s),
            Inline::Link { text, href } => {
                *text = sanitize_for_tui(text);
                *href = sanitize_for_tui(href);
            }
            Inline::SoftBreak => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    const OSC52: &str = "\x1b]52;c;SGVsbG8=\x07";

    #[test]
    fn strips_c0_c1_and_del_but_keeps_newline_and_tab() {
        let input = format!("A{OSC52}B\u{9b}31mC\x7fD\r\nE\tF");
        assert_eq!(strip_control_chars(&input), "A]52;c;SGVsbG8=B31mCD\nE\tF");
    }

    #[test]
    fn tui_variant_expands_tabs() {
        assert_eq!(sanitize_for_tui("a\tb"), "a    b");
    }

    #[test]
    fn keeps_non_control_unicode() {
        let s = "日本語 emoji 😀 \u{200b}zwsp";
        assert_eq!(strip_control_chars(s), s);
    }

    #[test]
    fn browse_page_sanitizes_all_display_strings() {
        let url = Url::parse("https://example.com/").unwrap();
        let page = BrowsePage {
            title: format!("T{OSC52}"),
            url: url.clone(),
            doc: Document {
                blocks: vec![
                    Block::Paragraph(vec![
                        Inline::Text(format!("p{OSC52}")),
                        Inline::Link {
                            text: format!("l{OSC52}"),
                            href: "https://example.com/x".into(),
                        },
                    ]),
                    Block::CodeBlock {
                        lang: Some("rs".into()),
                        content: format!("\tfn x() {{}}{OSC52}"),
                    },
                ],
            },
            links: vec![crate::document::ExtractedLink {
                text: format!("lk{OSC52}"),
                href: url,
                rel: None,
            }],
            markdown: format!("# T{OSC52}\n\tcode"),
        }
        .sanitize_for_terminal();

        let dbg = format!("{:?}", page);
        assert!(!dbg.contains("\\u{1b}") && !dbg.contains("\\u{7}"), "{dbg}");
        assert_eq!(page.markdown, "# T]52;c;SGVsbG8=\n\tcode");
        let Block::CodeBlock { content, .. } = &page.doc.blocks[1] else {
            panic!()
        };
        assert!(content.starts_with("    fn x()"));
    }
}
