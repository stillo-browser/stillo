use anyhow::Result;
use serde_json::Value;
use stillo_fetcher::web_search;

/// search_web MCP ツール: Web検索して結果をMarkdownまたはJSONで返す。
///
/// バックエンドは環境変数で切替（SEARXNG_URL / BRAVE_API_KEY / STILLO_SEARCH_BACKEND）。
/// ブロックページは空結果ではなくエラーとして返し、LLM 側の誤認を防ぐ。
pub async fn run(args: &Value) -> Result<String> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing 'query'"))?;
    let format = args["format"].as_str().unwrap_or("markdown");

    let results = web_search(query)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    match format {
        "links" => {
            let json: Vec<_> = results
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "title": r.title,
                        "url": r.url.as_str(),
                        "snippet": r.snippet,
                        "display_url": r.display_url,
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&json)?)
        }
        _ => Ok(stillo_fetcher::results_to_markdown(query, &results)),
    }
}
