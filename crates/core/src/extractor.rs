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

        let text_len = extract_text_length(&root);
        let _spa = detect_spa(&root, text_len, self.config.min_content_length);

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

/// HTTP Content-Type ヘッダーと HTML meta charset を参照してエンコードを検出し UTF-8 に変換する。
/// 判定できない場合は UTF-8 → latin1 の順でフォールバック。
fn decode_bytes(raw: &RawHtml) -> String {
    let charset = extract_charset_from_content_type(&raw.content_type)
        .or_else(|| sniff_charset_from_bytes(&raw.bytes));

    if let Some(label) = charset {
        if let Some(enc) = encoding_rs::Encoding::for_label(label.as_bytes()) {
            let (cow, _, _) = enc.decode(&raw.bytes);
            return cow.into_owned();
        }
    }

    if let Ok(s) = std::str::from_utf8(&raw.bytes) {
        return s.to_owned();
    }

    // latin1 フォールバック（文字化けは甘受する）
    raw.bytes.iter().map(|&b| b as char).collect()
}

fn parse_html(html: &str) -> RcDom {
    parse_document(RcDom::default(), Default::default())
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .unwrap_or_default()
}

/// "text/html; charset=Shift_JIS" → Some("Shift_JIS")
fn extract_charset_from_content_type(content_type: &str) -> Option<String> {
    for part in content_type.split(';') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix("charset=") {
            return Some(val.trim_matches('"').to_owned());
        }
    }
    None
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
                if let Ok(url) = base.join(&h) {
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

/// DOM 中の <frame src="..."> / <iframe src="..."> の URL を収集する。
fn collect_frame_srcs(handle: &markup5ever_rcdom::Handle, base: &Url) -> Vec<Url> {
    let mut result = Vec::new();
    collect_frame_srcs_inner(handle, base, &mut result);
    result
}

fn collect_frame_srcs_inner(handle: &markup5ever_rcdom::Handle, base: &Url, out: &mut Vec<Url>) {
    if let NodeData::Element { name, attrs, .. } = &handle.data {
        let tag = name.local.as_ref();
        if tag == "frame" || tag == "iframe" {
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
}
