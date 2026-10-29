use anyhow::Result;
use serde_json::Value;
use stillo_core::{ContentExtractor, ExtractorConfig};

pub async fn run(args: &Value) -> Result<String> {
    let url = super::parse_http_url(args)?;

    let fetcher = super::mcp_fetcher();
    let extractor = ContentExtractor::new(ExtractorConfig::default());
    let raw = fetcher.fetch(&url).await?;
    // readability の本文絞り込みを経ない文書全体のリンク。ナビ等も含む。
    let all_links = extractor.extract_links(&raw);

    let links: Vec<_> = all_links
        .iter()
        .map(|l| {
            serde_json::json!({
                "text": l.text,
                "href": l.href.as_str(),
                "rel": l.rel,
            })
        })
        .collect();

    Ok(serde_json::to_string_pretty(&links)?)
}
