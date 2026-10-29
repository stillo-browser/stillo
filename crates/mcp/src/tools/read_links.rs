use anyhow::{anyhow, Result};
use serde_json::Value;
use stillo_core::document::ExtractedLink;
use stillo_core::{ContentExtractor, ExtractorConfig};
use stillo_fetcher::{HttpConfig, HttpFetcher};

use super::paging::{positive_usize_arg, usize_arg};

/// 1 回の応答で返すリンク数の既定上限。
/// ナビゲーションの多いポータルでは数千件になり、全件返すとクライアント側 LLM のコンテキストを溢れさせる
pub const DEFAULT_LIMIT: usize = 200;

pub async fn run(args: &Value) -> Result<String> {
    let url_str = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing 'url'"))?;
    let url: url::Url = url_str.parse()?;
    // フェッチ前に検証し、引数の誤りでネットワークアクセスを無駄にしない
    let offset = usize_arg(args, "offset", 0)?;
    let limit = positive_usize_arg(args, "limit", DEFAULT_LIMIT)?;

    let fetcher = HttpFetcher::new(HttpConfig::default());
    let extractor = ContentExtractor::new(ExtractorConfig::default());
    let raw = fetcher.fetch(&url).await?;
    // readability の本文絞り込みを経ない文書全体のリンク。ナビ等も含む。
    let all_links = extractor.extract_links(&raw);

    render(&all_links, offset, limit)
}

/// `offset` から最大 `limit` 件のリンクを JSON で返す。
/// 切り詰めの有無で出力の型が変わると呼び出し側が分岐を強いられるため、常に同じオブジェクト形式で
/// 全件数と次の offset を含める
fn render(all_links: &[ExtractedLink], offset: usize, limit: usize) -> Result<String> {
    let total = all_links.len();
    if offset > total || (offset == total && total > 0) {
        return Err(anyhow!(
            "offset {} is out of range (total {} links)",
            offset,
            total
        ));
    }
    let end = offset.saturating_add(limit).min(total);
    let links: Vec<_> = all_links[offset..end]
        .iter()
        .map(|l| {
            serde_json::json!({
                "text": l.text,
                "href": l.href.as_str(),
                "rel": l.rel,
            })
        })
        .collect();
    let next_offset = (end < total).then_some(end);

    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "total": total,
        "offset": offset,
        "returned": links.len(),
        "truncated": next_offset.is_some(),
        "next_offset": next_offset,
        "links": links,
    }))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(n: usize) -> Vec<ExtractedLink> {
        (0..n)
            .map(|i| ExtractedLink {
                text: format!("link{i}"),
                href: format!("https://example.com/{i}").parse().unwrap(),
                rel: None,
            })
            .collect()
    }

    #[test]
    fn truncated_output_has_total_and_next_offset() {
        let v: Value = serde_json::from_str(&render(&links(5), 1, 2).unwrap()).unwrap();
        assert_eq!(v["total"], 5);
        assert_eq!(v["returned"], 2);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["next_offset"], 3);
        assert_eq!(v["links"][0]["text"], "link1");
    }

    #[test]
    fn last_page_has_null_next_offset() {
        let v: Value = serde_json::from_str(&render(&links(5), 3, 10).unwrap()).unwrap();
        assert_eq!(v["returned"], 2);
        assert_eq!(v["truncated"], false);
        assert!(v["next_offset"].is_null());
    }

    #[test]
    fn empty_page_is_valid() {
        let v: Value = serde_json::from_str(&render(&[], 0, 10).unwrap()).unwrap();
        assert_eq!(v["total"], 0);
        assert_eq!(v["links"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn offset_beyond_end_is_error() {
        assert!(render(&links(2), 2, 10).is_err());
    }
}
