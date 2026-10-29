use anyhow::Result;
use serde_json::Value;
use stillo_core::{ContentExtractor, ExtractorConfig, MarkdownConfig, MarkdownSerializer};
use stillo_llm::{prompts, CompletionConfig, LlmProvider};

pub async fn run(args: &Value) -> Result<String> {
    let url = super::parse_http_url(args)?;

    let fields = match &args["fields"] {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        Value::String(s) => s.clone(),
        _ => return Err(anyhow::anyhow!("'fields' must be an array or string")),
    };

    let fetcher = super::mcp_fetcher();
    let extractor = ContentExtractor::new(ExtractorConfig::default());
    let raw = fetcher.fetch(&url).await?;
    let content = extractor.extract(&raw)?;
    let serializer = MarkdownSerializer::new(MarkdownConfig::default());
    let doc = serializer.serialize(&content);

    let llm = LlmProvider::from_env()?;
    let config = CompletionConfig {
        temperature: 0.0,
        ..Default::default()
    };
    let messages = prompts::extract_prompt(&fields, &doc);
    let result = llm.complete(messages, &config).await?;

    // JSON として返せる場合は整形する
    if let Ok(v) = serde_json::from_str::<Value>(&result) {
        return Ok(serde_json::to_string_pretty(&v)?);
    }
    Ok(result)
}
