use std::path::Path;
use stillo_core::document::{FetchError, RawHtml};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use url::Url;

/// Playwright デーモン（`playwright-daemon/daemon.js`）経由でSPAのHTMLを取得する。
///
/// プロトコル（JSON-lines, Unix socket）:
///   request : `{"url":"https://..."}\n`
///   response: `{"html":"...","url":"https://...","status":200}\n`
///             `{"error":"..."}\n`（失敗時）
pub async fn fetch_via_playwright(socket_path: &Path, url: &Url) -> Result<RawHtml, FetchError> {
    let stream = UnixStream::connect(socket_path).await.map_err(|e| {
        FetchError::DelegationFailed(format!(
            "Playwright daemon not reachable at {}: {}",
            socket_path.display(),
            e
        ))
    })?;

    let (reader, mut writer) = tokio::io::split(stream);

    let req = format!("{}\n", serde_json::json!({ "url": url.as_str() }));
    writer.write_all(req.as_bytes()).await.map_err(|e| {
        FetchError::DelegationFailed(format!("Playwright socket write failed: {}", e))
    })?;

    let mut buf = String::new();
    let mut buf_reader = BufReader::new(reader);

    tokio::time::timeout(
        tokio::time::Duration::from_secs(60),
        buf_reader.read_line(&mut buf),
    )
    .await
    .map_err(|_| FetchError::DelegationFailed("Playwright response timed out".into()))?
    .map_err(|e| FetchError::DelegationFailed(format!("Playwright socket read failed: {}", e)))?;

    let resp: serde_json::Value = serde_json::from_str(buf.trim()).map_err(|e| {
        FetchError::DelegationFailed(format!("Playwright response parse failed: {}", e))
    })?;

    if let Some(err) = resp["error"].as_str() {
        return Err(FetchError::DelegationFailed(format!("Playwright: {}", err)));
    }

    let html = resp["html"].as_str().ok_or_else(|| {
        FetchError::DelegationFailed("Playwright: response missing html field".into())
    })?;

    let final_url = resp["url"]
        .as_str()
        .and_then(|s| s.parse::<Url>().ok())
        .unwrap_or_else(|| url.clone());

    let status = resp["status"].as_u64().unwrap_or(200) as u16;

    Ok(RawHtml {
        bytes: html.as_bytes().to_vec(),
        url: final_url,
        content_type: "text/html; charset=utf-8".to_owned(),
        status,
    })
}
