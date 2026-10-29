pub mod readability;
pub mod spa_detection;

use html5ever::parse_document;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{NodeData, RcDom};
use url::Url;

use self::readability::ReadabilityExtractor;
use self::spa_detection::{detect_spa, extract_text_length};
use crate::document::{ExtractedContent, ExtractedLink, RawHtml, SpaDetection};

#[derive(Debug, Clone)]
pub struct ExtractorConfig {
    pub min_content_length: usize,
    pub noise_selectors: Vec<String>,
    pub preserve_links: bool,
}

impl Default for ExtractorConfig {
    fn default() -> Self {
        Self {
            min_content_length: 500,
            noise_selectors: vec![],
            preserve_links: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExtractionError {
    #[error("Failed to decode HTML: {0}")]
    Decode(String),
    #[error("No content found")]
    NoContent,
}

pub struct ContentExtractor {
    config: ExtractorConfig,
}

impl ContentExtractor {
    pub fn new(config: ExtractorConfig) -> Self {
        Self { config }
    }

    /// RawHtml → ExtractedContent（純粋関数）
    pub fn extract(&self, raw: &RawHtml) -> Result<ExtractedContent, ExtractionError> {
        let html_str = decode_bytes(raw);
        let dom = parse_html(&html_str);
        let root = dom.document.clone();

        let extractor = ReadabilityExtractor {
            preserve_links: self.config.preserve_links,
        };
        let content = extractor.extract(&root, &raw.url);

        Ok(content)
    }

    pub fn detect_spa_for(&self, raw: &RawHtml) -> Result<SpaDetection, ExtractionError> {
        let html_str = decode_bytes(raw);
        let dom = parse_html(&html_str);
        let root = dom.document.clone();
        let text_len = extract_text_length(&root);
        Ok(detect_spa(&root, text_len, self.config.min_content_length))
    }

    /// frameset ページのフレーム URL 一覧を返す。空なら通常ページ。
    pub fn detect_frames(&self, raw: &RawHtml) -> Vec<Url> {
        let html_str = decode_bytes(raw);
        let dom = parse_html(&html_str);
        collect_frame_srcs(&dom.document, &raw.url)
    }

    /// 文書全体から <a href> を収集する。readability による本文絞り込みを経ないため、
    /// ナビゲーション等本文外のリンクも含む。MCP read_links 用。
    pub fn extract_links(&self, raw: &RawHtml) -> Vec<ExtractedLink> {
        let html_str = decode_bytes(raw);
        let dom = parse_html(&html_str);
        let mut links = Vec::new();
        let mut seen = std::collections::HashSet::new();
        collect_anchor_links(&dom.document, &raw.url, &mut links, &mut seen);
        links
    }
}

/// バイト列のエンコードを判定して UTF-8 に変換する。
///
/// 優先順は WHATWG Encoding 標準に合わせて BOM → Content-Type の charset → meta charset。
/// 判定できない場合は UTF-8 として読み、不正なら windows-1252 で読む。
fn decode_bytes(raw: &RawHtml) -> String {
    // BOM はサーバー設定より確実にファイル実体を表すため、Content-Type より優先する
    if let Some((enc, bom_len)) = encoding_rs::Encoding::for_bom(&raw.bytes) {
        let (cow, _) = enc.decode_without_bom_handling(&raw.bytes[bom_len..]);
        return cow.into_owned();
    }

    let charset = extract_charset_from_content_type(&raw.content_type)
        .or_else(|| sniff_charset_from_bytes(&raw.bytes));

    if let Some(label) = charset {
        if let Some(enc) = encoding_rs::Encoding::for_label(label.as_bytes()) {
            let (cow, _) = enc.decode_without_bom_handling(&raw.bytes);
            return cow.into_owned();
        }
    }

    if let Ok(s) = std::str::from_utf8(&raw.bytes) {
        return s.to_owned();
    }

    // ブラウザは宣言のない非 UTF-8 文書を windows-1252 として読む。latin1 だと 0x80-0x9F が
    // 制御文字になり、“” や € などが消える
    let (cow, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(&raw.bytes);
    cow.into_owned()
}

/// DOM の最大深さ。これより深い部分木は切り捨てる。
///
/// readability・SPA 判定・リンク収集などの DOM 走査は再帰で実装されている。
/// html5ever はネスト深さを制限しないため、<div> を数千段ネストした HTML で
/// tokio ワーカースレッド（2MB スタック）がスタックオーバーフローし、プロセスごと abort する。
/// 実在するページの DOM 深さは通常 100 未満なので、512 で切っても本文は失われない。
const MAX_DOM_DEPTH: usize = 512;

/// HTML をパースし、MAX_DOM_DEPTH を超える部分木を切り捨てた DOM を返す。
/// core 内で html5ever の DOM を走査する処理は、この関数を経由してパースすること。
pub(crate) fn parse_html(html: &str) -> RcDom {
    let dom = parse_document(RcDom::default(), Default::default())
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .unwrap_or_default();
    truncate_deep_nodes(&dom.document, MAX_DOM_DEPTH);
    dom
}

/// 深さ max_depth に達したノードの子をすべて外す。
/// この関数自体が深い DOM を処理するため、再帰ではなく明示スタックで走査する。
/// 外した部分木の解放は markup5ever_rcdom の Node::drop が反復的に行うため、解放時にもスタックは消費しない。
fn truncate_deep_nodes(root: &markup5ever_rcdom::Handle, max_depth: usize) {
    let mut stack = vec![(root.clone(), 0usize)];
    while let Some((node, depth)) = stack.pop() {
        if depth >= max_depth {
            node.children.borrow_mut().clear();
            continue;
        }
        for child in node.children.borrow().iter() {
            stack.push((child.clone(), depth + 1));
        }
    }
}

/// "text/html; charset=Shift_JIS" → Some("Shift_JIS")
/// パラメータ名は RFC 9110 で大文字小文字を区別しないため `Charset=` も受け付ける。
fn extract_charset_from_content_type(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|part| {
        let (key, val) = part.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| val.trim().trim_matches('"').to_owned())
            .filter(|v| !v.is_empty())
    })
}

/// HTML バイト列の先頭 4096 バイトをバイト列のまま走査し、
/// `charset=` 属性値を ASCII レベルで抽出する。
/// Shift_JIS 等の非 UTF-8 バイト列でも meta タグ部分は ASCII のため動作する。
fn sniff_charset_from_bytes(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(4096)];
    let needle = b"charset=";
    let pos = head
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))?;
    let after = &head[pos + needle.len()..];
    // 引用符を読み飛ばす
    let after = after
        .strip_prefix(b"\"")
        .or_else(|| after.strip_prefix(b"'"))
        .unwrap_or(after);
    let val: Vec<u8> = after
        .iter()
        .copied()
        .take_while(|&b| !matches!(b, b'"' | b'\'' | b';' | b' ' | b'>' | b'\n' | b'\r'))
        .collect();
    if val.is_empty() {
        return None;
    }
    String::from_utf8(val).ok().filter(|s| !s.is_empty())
}

/// 文書全体を走査し <a href> を抽出する。空アンカー（画像リンク等）は除外し、
/// resolved URL の重複は最初の 1 件のみ残す。ドキュメント順を保つ。
fn collect_anchor_links(
    handle: &markup5ever_rcdom::Handle,
    base: &Url,
    out: &mut Vec<ExtractedLink>,
    seen: &mut std::collections::HashSet<Url>,
) {
    if let NodeData::Element { name, attrs, .. } = &handle.data {
        if name.local.as_ref() == "a" {
            let attrs_ref = attrs.borrow();
            let href = attrs_ref
                .iter()
                .find(|a| a.name.local.as_ref() == "href")
                .map(|a| a.value.as_ref().to_owned());
            let rel = attrs_ref
                .iter()
                .find(|a| a.name.local.as_ref() == "rel")
                .map(|a| a.value.as_ref().to_owned());
            drop(attrs_ref);

            if let Some(h) = href {
                // javascript: / data: 等は辿れないうえ、MCP 経由で LLM に渡すと危険なので除外する
                if let Some(url) = base.join(&h).ok().filter(crate::sanitize::is_navigable_url) {
                    let mut text = String::new();
                    readability::collect_text(handle, &mut text);
                    let trimmed = text.trim().to_owned();
                    // 画像リンク等でアンカーテキストが空のものは除外（readability と同じ基準）
                    if !trimmed.is_empty() && seen.insert(url.clone()) {
                        out.push(ExtractedLink {
                            text: trimmed,
                            href: url,
                            rel,
                        });
                    }
                }
            }
        }
    }
    for child in handle.children.borrow().iter() {
        collect_anchor_links(child, base, out, seen);
    }
}

/// DOM 中の <frame src="..."> の URL を収集する。
///
/// <iframe> は対象外。iframe は動画・広告・SNS の埋め込みに使われるため、
/// 拾うと通常の記事ページが埋め込み先の HTML に置き換わってしまう。
/// html5ever は <frameset> 外の <frame> を捨てるので、<frame> だけを見れば frameset ページに限定できる。
fn collect_frame_srcs(handle: &markup5ever_rcdom::Handle, base: &Url) -> Vec<Url> {
    let mut result = Vec::new();
    collect_frame_srcs_inner(handle, base, &mut result);
    result
}

fn collect_frame_srcs_inner(handle: &markup5ever_rcdom::Handle, base: &Url, out: &mut Vec<Url>) {
    if let NodeData::Element { name, attrs, .. } = &handle.data {
        let tag = name.local.as_ref();
        if tag == "frame" {
            if let Some(src) = attrs
                .borrow()
                .iter()
                .find(|a| a.name.local.as_ref() == "src")
                .map(|a| a.value.as_ref().to_owned())
            {
                if let Ok(url) = base.join(&src) {
                    out.push(url);
                }
            }
        }
    }
    for child in handle.children.borrow().iter() {
        collect_frame_srcs_inner(child, base, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_html(html: &str) -> RawHtml {
        RawHtml {
            bytes: html.as_bytes().to_vec(),
            url: Url::parse("https://example.com/docs/").unwrap(),
            content_type: "text/html".to_string(),
            status: 200,
        }
    }

    /// テストスレッドのスタック（既定 2MB）は tokio ワーカーと同じなので、
    /// 深さ制限がないとここで stack overflow により abort する
    #[test]
    fn test_deeply_nested_html_does_not_overflow_stack() {
        // 深さ制限なしでは 5000 段で overflow することを確認済み。html5ever のパースが深さの二乗で遅くなるため必要以上に深くしない
        let depth = 6_000;
        let html = format!(
            "<html><body><p>Shallow text</p>{}<a href=\"/deep\">deep</a>{}</body></html>",
            "<div>".repeat(depth),
            "</div>".repeat(depth)
        );
        let raw = raw_html(&html);
        let extractor = ContentExtractor::new(ExtractorConfig::default());

        let content = extractor.extract(&raw).unwrap();
        assert!(content.body_text.contains("Shallow text"));
        extractor.detect_spa_for(&raw).unwrap();
        assert!(extractor.detect_frames(&raw).is_empty());
        // 深さ上限より深い位置のリンクは切り捨てられる
        assert!(extractor.extract_links(&raw).is_empty());
        crate::search::parse_ddg_results(&html);
    }

    #[test]
    fn test_truncate_deep_nodes_keeps_shallow_content() {
        let html = format!(
            "<html><body>{}x{}</body></html>",
            "<div>".repeat(5),
            "</div>".repeat(5)
        );
        let dom = parse_html(&html);
        let mut text = String::new();
        let mut stack = vec![dom.document.clone()];
        while let Some(n) = stack.pop() {
            if let NodeData::Text { contents } = &n.data {
                text.push_str(&contents.borrow());
            }
            stack.extend(n.children.borrow().iter().cloned());
        }
        assert_eq!(text, "x");
    }

    #[test]
    fn test_detect_frames_frameset() {
        let html = r#"<html><frameset cols="20%,80%">
            <frame src="menu.html"><frame src="body.html">
        </frameset></html>"#;
        let extractor = ContentExtractor::new(ExtractorConfig::default());
        let frames = extractor.detect_frames(&raw_html(html));
        assert_eq!(
            frames.iter().map(Url::as_str).collect::<Vec<_>>(),
            [
                "https://example.com/docs/menu.html",
                "https://example.com/docs/body.html"
            ]
        );
    }

    /// 埋め込み動画の iframe を持つ通常の記事は frameset ページとして扱わない
    #[test]
    fn test_detect_frames_ignores_iframe() {
        let html = r#"<html><body><article><h1>Post</h1><p>Body text.</p>
            <iframe src="https://www.youtube.com/embed/xyz"></iframe>
        </article></body></html>"#;
        let extractor = ContentExtractor::new(ExtractorConfig::default());
        assert!(extractor.detect_frames(&raw_html(html)).is_empty());
    }

    /// クエリ区切りの `&` を含む href が、本文 AST・Markdown のどちらでも `&amp;` のまま残らないこと
    #[test]
    fn test_href_with_ampersand_survives_pipeline() {
        let html = r#"<html><body><article><h1>T</h1>
            <p>See <a href="/s?a=1&amp;b=2">query link</a> for details. Literal &amp;lt;b&amp;gt; text.</p>
        </article></body></html>"#;
        let (content, _) = extract_from(html);

        let doc = crate::html_to_ast::parse_html_to_ast(&content.body_html, &content.url);
        let hrefs: Vec<&str> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                crate::ast::Block::Paragraph(inlines) => Some(inlines),
                _ => None,
            })
            .flatten()
            .filter_map(|i| match i {
                crate::ast::Inline::Link { href, .. } => Some(href.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(hrefs, ["https://example.com/s?a=1&b=2"]);

        let md = crate::markdown::MarkdownSerializer::new(Default::default())
            .serialize(&content, chrono::DateTime::<chrono::Utc>::default());
        assert!(
            md.content.contains("(https://example.com/s?a=1&b=2)"),
            "{}",
            md.content
        );
        assert!(!md.content.contains("&amp;"), "{}", md.content);
        // 元テキストの文字列 `&lt;b&gt;` は 1 段だけデコードされ `<b>` にはならない
        assert!(md.content.contains("&lt;b&gt;"), "{}", md.content);
    }

    fn extract_from(html: &str) -> (ExtractedContent, Vec<ExtractedLink>) {
        let raw = RawHtml {
            bytes: html.as_bytes().to_vec(),
            url: Url::parse("https://example.com/docs/").unwrap(),
            content_type: "text/html".to_string(),
            status: 200,
        };
        let extractor = ContentExtractor::new(ExtractorConfig::default());
        let content = extractor.extract(&raw).unwrap();
        let links = extractor.extract_links(&raw);
        (content, links)
    }

    /// mdBook 等の「本文は短い <main>、リンクは <nav>」構造。
    /// extract()（readability 経由）ではリンクが 0 件になるが、
    /// extract_links() は文書全体から収集できなければならない。
    #[test]
    fn test_extract_links_document_wide() {
        let html = r#"<html><body>
            <nav class="sidebar"><ul>
                <li><a href="title-page.html">Title Page</a></li>
                <li><a href="types.html">Chapter 1</a></li>
            </ul></nav>
            <main><h1>Effective Rust</h1><p>Short intro.</p></main>
        </body></html>"#;
        let (content, links) = extract_from(html);

        // readability 経由のリンクは nav ノイズ除去で 0 件
        assert_eq!(
            content.links.len(),
            0,
            "extract() must not include nav links"
        );
        // 文書全体からは収集できる
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].text, "Title Page");
        assert_eq!(
            links[0].href.as_str(),
            "https://example.com/docs/title-page.html"
        );
        assert_eq!(links[1].text, "Chapter 1");
    }

    /// 重複 href は最初の 1 件、空アンカー（画像リンク等）は除外。
    #[test]
    fn test_extract_links_dedup_and_empty_anchor() {
        let html = r#"<html><body>
            <a href="a.html">First</a>
            <a href="./a.html">Second</a>
            <a href="/img.png"><img src="img.png" alt="logo"></a>
            <a href="https://other.example.org/x">External</a>
        </body></html>"#;
        let (_, links) = extract_from(html);

        let hrefs: Vec<&str> = links.iter().map(|l| l.href.as_str()).collect();
        // a.html と ./a.html は同一 URL に解決され 1 件に。空アンカーは除外。
        assert_eq!(links.len(), 2, "hrefs: {hrefs:?}");
        assert_eq!(links[0].text, "First");
        assert_eq!(links[1].href.as_str(), "https://other.example.org/x");
    }

    #[test]
    fn test_extract_links_drops_unsafe_schemes() {
        let html = r#"<html><body>
            <a href="javascript:alert(1)">js</a>
            <a href="data:text/html,x">data</a>
            <a href="vbscript:x">vb</a>
            <a href="mailto:a@example.com">mail</a>
            <a href="/ok">ok</a>
        </body></html>"#;
        let (_, links) = extract_from(html);
        let hrefs: Vec<&str> = links.iter().map(|l| l.href.as_str()).collect();
        assert_eq!(hrefs, ["mailto:a@example.com", "https://example.com/ok"]);
    }

    fn raw_bytes(bytes: Vec<u8>, content_type: &str) -> RawHtml {
        RawHtml {
            bytes,
            url: Url::parse("https://example.com/").unwrap(),
            content_type: content_type.to_string(),
            status: 200,
        }
    }

    #[test]
    fn test_decode_charset_param_is_case_insensitive() {
        let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode("<p>日本語</p>");
        let raw = raw_bytes(sjis.into_owned(), "text/html; Charset=Shift_JIS");
        assert!(decode_bytes(&raw).contains("日本語"));
        let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode("<p>日本語</p>");
        let raw = raw_bytes(sjis.into_owned(), "text/html;CHARSET=\"shift_jis\"");
        assert!(decode_bytes(&raw).contains("日本語"));
    }

    /// WHATWG Encoding 標準では BOM が Content-Type の charset より優先される
    #[test]
    fn test_decode_bom_overrides_content_type() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("<p>日本語</p>".as_bytes());
        let raw = raw_bytes(bytes, "text/html; charset=Shift_JIS");
        assert_eq!(decode_bytes(&raw), "<p>日本語</p>");

        let mut bytes = vec![0xFF, 0xFE];
        for c in "<p>é</p>".encode_utf16() {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
        let raw = raw_bytes(bytes.clone(), "text/html; charset=iso-8859-1");
        assert_eq!(decode_bytes(&raw), "<p>é</p>");
        // charset 宣言がなくても BOM で判定する
        let raw = raw_bytes(bytes, "text/html");
        assert_eq!(decode_bytes(&raw), "<p>é</p>");
    }

    /// 宣言なし・UTF-8 として不正なバイト列は latin1 ではなく windows-1252 として読む
    #[test]
    fn test_decode_fallback_is_windows_1252() {
        let raw = raw_bytes(b"<p>\x93quoted\x94 \x80</p>".to_vec(), "text/html");
        assert_eq!(decode_bytes(&raw), "<p>\u{201C}quoted\u{201D} \u{20AC}</p>");
    }

    /// readability → html_to_ast / Markdown の経路でブロック間の語が連結しない
    #[test]
    fn test_pipeline_keeps_block_and_word_boundaries() {
        let html = r#"<html><body><article><h1>T</h1>
            <p><b>foo</b> <i>bar</i></p>
            <div>alpha</div><div>beta</div>
            <table><tr><td>c1</td><td>c2</td></tr></table>
        </article></body></html>"#;
        let (content, _) = extract_from(html);
        let doc = crate::html_to_ast::parse_html_to_ast(&content.body_html, &content.url);
        let text: Vec<String> = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                crate::ast::Block::Paragraph(i) => Some(
                    i.iter()
                        .map(|x| match x {
                            crate::ast::Inline::Text(s)
                            | crate::ast::Inline::Bold(s)
                            | crate::ast::Inline::Italic(s) => s.clone(),
                            _ => String::new(),
                        })
                        .collect(),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(text, ["foo bar", "alpha", "beta", "c1 | c2"]);

        let md = crate::markdown::MarkdownSerializer::new(Default::default())
            .serialize(&content, chrono::DateTime::<chrono::Utc>::default());
        // readability はインライン装飾タグを出力しないため、語の間の空白だけを確認する
        assert!(md.content.contains("foo bar"), "{}", md.content);
        assert!(!md.content.contains("alphabeta"), "{}", md.content);
        assert!(md.content.contains("c1 | c2"), "{}", md.content);
    }
}
