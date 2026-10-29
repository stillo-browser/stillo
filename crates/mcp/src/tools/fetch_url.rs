use anyhow::Result;
use serde_json::Value;
use stillo_core::document::ExtractedContent;
use stillo_core::{ContentExtractor, ExtractorConfig, MarkdownConfig, MarkdownSerializer};
use stillo_fetcher::{HttpConfig, HttpFetcher};

use super::paging::{positive_usize_arg, slice_chars, usize_arg};

/// 1 回の応答で返す本文の既定上限（文字数）。
/// 一般的な記事は収まり、巨大なページでもクライアント側 LLM のコンテキストを占有しきらない値
pub const DEFAULT_MAX_CHARS: usize = 50_000;

pub async fn run(args: &Value) -> Result<String> {
    let url_str = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing 'url'"))?;
    let url: url::Url = url_str.parse()?;

    let format = args["format"].as_str().unwrap_or("markdown");
    // フェッチ前に検証し、引数の誤りでネットワークアクセスを無駄にしない
    let offset = usize_arg(args, "offset", 0)?;
    let max_chars = positive_usize_arg(args, "max_chars", DEFAULT_MAX_CHARS)?;

    let fetcher = HttpFetcher::new(HttpConfig::default());
    let extractor = ContentExtractor::new(ExtractorConfig::default());
    let raw = fetcher.fetch(&url).await?;
    let content = extractor.extract(&raw)?;

    render(&content, format, offset, max_chars)
}

/// 抽出結果を指定形式に整形し、`offset` から `max_chars` 文字分を返す。
/// 切り詰めた場合は全体長と次の offset を出力に含め、呼び出し側 LLM が続きを要求できるようにする
fn render(
    content: &ExtractedContent,
    format: &str,
    offset: usize,
    max_chars: usize,
) -> Result<String> {
    match format {
        "json" => {
            // JSON 全体を文字数で切ると不正な JSON になるため、本文フィールドだけをページングする
            let page = slice_chars(&content.body_text, offset, max_chars)?;
            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "url": content.url.as_str(),
                "title": content.title,
                "byline": content.byline,
                "body_text": page.text,
                "offset": page.offset,
                "total_chars": page.total,
                "truncated": page.next_offset().is_some(),
                "next_offset": page.next_offset(),
            }))?)
        }
        "plain" => Ok(paginate_text(&content.body_text, offset, max_chars)?),
        _ => {
            let serializer = MarkdownSerializer::new(MarkdownConfig::default());
            let markdown = serializer.serialize(content).content;
            Ok(paginate_text(&markdown, offset, max_chars)?)
        }
    }
}

/// テキストを切り出し、部分出力であれば末尾に範囲と続きの取得方法を追記する
fn paginate_text(text: &str, offset: usize, max_chars: usize) -> Result<String> {
    let page = slice_chars(text, offset, max_chars)?;
    let mut out = page.text.to_owned();
    match page.next_offset() {
        Some(next) => out.push_str(&format!(
            "\n\n[stillo: output truncated. Showing characters {}-{} of {}. Call fetch_url again with offset={} to continue.]",
            page.offset, page.end, page.total, next
        )),
        None if page.offset > 0 => out.push_str(&format!(
            "\n\n[stillo: showing characters {}-{} of {} (end of content).]",
            page.offset, page.end, page.total
        )),
        None => {}
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(body: &str) -> ExtractedContent {
        ExtractedContent {
            url: "https://example.com/".parse().unwrap(),
            title: "t".into(),
            byline: None,
            body_text: body.into(),
            body_html: String::new(),
            links: Vec::new(),
            metadata: stillo_core::document::PageMetadata {
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

    #[test]
    fn short_text_is_returned_without_notice() {
        assert_eq!(paginate_text("hello", 0, 10).unwrap(), "hello");
    }

    #[test]
    fn truncated_text_reports_total_and_next_offset() {
        let out = paginate_text("日本語のテキスト", 0, 3).unwrap();
        assert!(
            out.starts_with("日本語\n\n[stillo: output truncated."),
            "{out}"
        );
        assert!(out.contains("of 8"), "{out}");
        assert!(out.contains("offset=3"), "{out}");
    }

    #[test]
    fn last_page_reports_end_of_content() {
        let out = paginate_text("abcdef", 4, 10).unwrap();
        assert!(out.starts_with("ef\n\n"), "{out}");
        assert!(out.contains("end of content"), "{out}");
    }

    #[test]
    fn json_format_stays_valid_json_when_truncated() {
        let out = render(&content("あいうえお"), "json", 1, 2).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["body_text"], "いう");
        assert_eq!(v["total_chars"], 5);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["next_offset"], 3);
    }

    #[test]
    fn plain_format_is_paginated() {
        let out = render(&content("abcdef"), "plain", 0, 2).unwrap();
        assert!(out.starts_with("ab\n\n[stillo"), "{out}");
    }

    #[test]
    fn offset_beyond_content_is_error() {
        assert!(render(&content("abc"), "plain", 10, 5).is_err());
    }
}
