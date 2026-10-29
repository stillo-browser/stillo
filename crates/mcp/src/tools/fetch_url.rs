use anyhow::Result;
use serde_json::Value;
use stillo_core::{ContentExtractor, ExtractorConfig, MarkdownConfig, MarkdownSerializer};

pub async fn run(args: &Value) -> Result<String> {
    let url = super::parse_http_url(args)?;

    let format = args["format"].as_str().unwrap_or("markdown");

    let fetcher = super::mcp_fetcher();
    let extractor = ContentExtractor::new(ExtractorConfig::default());
    let raw = fetcher.fetch(&url).await?;
    let content = extractor.extract(&raw)?;

    let output = match format {
        "plain" => content.body_text.clone(),
        "json" => serde_json::to_string_pretty(&serde_json::json!({
            "url": content.url.as_str(),
            "title": content.title,
            "byline": content.byline,
            "body_text": content.body_text,
        }))?,
        _ => {
            let serializer = MarkdownSerializer::new(MarkdownConfig::default());
            serializer.serialize(&content).content
        }
    };

    Ok(output)
}
