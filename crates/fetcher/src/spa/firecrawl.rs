use super::jina::html_escape;
use crate::http::{read_body_limited, DEFAULT_MAX_BODY_BYTES};
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

    // Firecrawl 自身の 4xx/5xx（認証失敗・クォータ超過等）を対象サイトの HTTP エラーと
    // 誤認させないため、Http ではなくバックエンド名付きの DelegationFailed にする
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(FetchError::DelegationFailed(format!(
            "firecrawl returned HTTP {}",
            status
        )));
    }

    let bytes = read_body_limited(
        resp,
        DEFAULT_MAX_BODY_BYTES,
        url,
        super::SPA_HTTP_TIMEOUT_SECS,
    )
    .await?;
    let result: FirecrawlResponse = serde_json::from_slice(&bytes)
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
        // markdown 中の `<script>` 等がタグとして解釈されないようエスケープする
        format!(
            "<html><head><title>{}</title></head><body><article><pre>{}</pre></article></body></html>",
            html_escape(&title),
            html_escape(&md)
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 1 リクエストだけ `status` と `body` を返す Firecrawl 模擬サーバー
    async fn mock_firecrawl(status: &'static str, body: &'static str) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = s.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status,
                body.len(),
                body
            );
            s.write_all(resp.as_bytes()).await.ok();
        });
        Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap()
    }

    #[tokio::test]
    async fn markdown_fallback_escapes_title_and_body() {
        let base = mock_firecrawl(
            "200 OK",
            r#"{"data":{"markdown":"<script>alert(1)</script>","metadata":{"title":"</title><script>x</script>"}}}"#,
        )
        .await;
        let target = Url::parse("https://example.com/").unwrap();
        let raw = fetch_via_firecrawl(&Client::new(), &base, "k", &target)
            .await
            .unwrap();
        let html = String::from_utf8(raw.bytes).unwrap();
        assert!(!html.contains("<script>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{html}"
        );
    }

    #[tokio::test]
    async fn firecrawl_4xx_is_delegation_failure() {
        let base = mock_firecrawl("402 Payment Required", "{}").await;
        let target = Url::parse("https://example.com/").unwrap();
        let err = fetch_via_firecrawl(&Client::new(), &base, "k", &target)
            .await
            .unwrap_err();
        match err {
            FetchError::DelegationFailed(msg) => {
                assert!(msg.contains("firecrawl") && msg.contains("402"), "{msg}")
            }
            other => panic!("expected DelegationFailed, got {other:?}"),
        }
    }
}
