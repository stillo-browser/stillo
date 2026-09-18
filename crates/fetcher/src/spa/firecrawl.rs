use reqwest::Client;
use serde::Deserialize;
use stillo_core::document::{FetchError, RawHtml};
use url::Url;

#[derive(Deserialize)]
struct FirecrawlResponse {
    data: Option<FirecrawlData>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct FirecrawlData {
    html: Option<String>,
    markdown: Option<String>,
    metadata: Option<FirecrawlMeta>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FirecrawlMeta {
    title: Option<String>,
}

/// Firecrawl API 経由でSPAのHTMLを取得する。
/// FIRECRAWL_URL + FIRECRAWL_API_KEY 環境変数が必要。
pub async fn fetch_via_firecrawl(
    client: &Client,
    base_url: &Url,
    api_key: &str,
    url: &Url,
) -> Result<RawHtml, FetchError> {
    let endpoint = base_url
        .join("v1/scrape")
        .map_err(|e| FetchError::DelegationFailed(format!("invalid firecrawl url: {}", e)))?;

    let body = serde_json::json!({
        "url": url.as_str(),
        "formats": ["html", "markdown"],
    });

    let resp = client
        .post(endpoint.as_str())
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| FetchError::DelegationFailed(format!("firecrawl request failed: {}", e)))?;

    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(FetchError::Http {
            status,
            url: url.clone(),
        });
    }

    let result: FirecrawlResponse = resp
        .json()
        .await
        .map_err(|e| FetchError::DelegationFailed(format!("firecrawl parse failed: {}", e)))?;

    if let Some(err) = result.error {
        return Err(FetchError::DelegationFailed(format!(
            "firecrawl error: {}",
            err
        )));
    }

    let data = result
        .data
        .ok_or_else(|| FetchError::DelegationFailed("firecrawl returned empty data".into()))?;

    // html フィールドがあればそれを使い、なければ markdown を <pre> で包む
    let html = if let Some(h) = data.html.filter(|h| !h.is_empty()) {
        h
    } else if let Some(md) = data.markdown {
        let title = data.metadata.and_then(|m| m.title).unwrap_or_default();
        format!(
            "<html><head><title>{}</title></head><body><article><pre>{}</pre></article></body></html>",
            title, md
        )
    } else {
        return Err(FetchError::DelegationFailed(
            "firecrawl returned no content".into(),
        ));
    };

    Ok(RawHtml {
        bytes: html.into_bytes(),
        url: url.clone(),
        content_type: "text/html; charset=utf-8".to_owned(),
        status: 200,
    })
}
