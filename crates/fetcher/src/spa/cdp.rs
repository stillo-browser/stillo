use stillo_core::document::{FetchError, RawHtml};
use url::Url;

/// Chrome が CDP ポートでリッスンしているか確認する
pub async fn is_chrome_available(port: u16) -> bool {
    reqwest::get(format!("http://localhost:{}/json/version", port))
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Chrome DevTools Protocol 経由でSPAのHTMLを取得する。
/// Chrome が --remote-debugging-port={port} で起動済みである必要がある。
#[cfg(feature = "cdp")]
pub async fn fetch_via_cdp(port: u16, url: &Url) -> Result<RawHtml, FetchError> {
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    // 1. 新しいタブを作成
    let new_tab: Value = reqwest::Client::new()
        .put(format!("http://localhost:{}/json/new", port))
        .send()
        .await
        .map_err(|e| FetchError::DelegationFailed(format!("CDP new tab failed: {}", e)))?
        .json()
        .await
        .map_err(|e| FetchError::DelegationFailed(format!("CDP new tab parse failed: {}", e)))?;

    let target_id = new_tab["id"]
        .as_str()
        .ok_or_else(|| FetchError::DelegationFailed("CDP: no target id".into()))?
        .to_owned();

    let ws_url = new_tab["webSocketDebuggerUrl"]
        .as_str()
        .ok_or_else(|| FetchError::DelegationFailed("CDP: no WebSocket URL".into()))?
        .to_owned();

    // 2. WebSocket 接続
    let (mut ws, _) = connect_async(&ws_url).await.map_err(|e| {
        FetchError::DelegationFailed(format!("CDP WebSocket connect failed: {}", e))
    })?;

    // helper: CDP コマンドを送信
    let send_cmd = |ws: &mut _, id: u64, method: &str, params: Value| {
        let msg = serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        // 非同期クロージャにするためここでは文字列を返す
        msg
    };

    // 3. Page ドメインを有効化
    ws.send(Message::Text(
        send_cmd(&mut ws, 1, "Page.enable", serde_json::json!({})).into(),
    ))
    .await
    .map_err(|e| FetchError::DelegationFailed(format!("CDP Page.enable failed: {}", e)))?;

    // 4. ナビゲーション
    ws.send(Message::Text(
        send_cmd(
            &mut ws,
            2,
            "Page.navigate",
            serde_json::json!({ "url": url.as_str() }),
        )
        .into(),
    ))
    .await
    .map_err(|e| FetchError::DelegationFailed(format!("CDP navigate failed: {}", e)))?;

    // 5. loadEventFired を待機（最大30秒）
    let timeout = tokio::time::Duration::from_secs(30);
    let loaded = tokio::time::timeout(timeout, async {
        while let Some(msg) = ws.next().await {
            let Ok(Message::Text(text)) = msg else {
                continue;
            };
            let Ok(v): Result<Value, _> = serde_json::from_str(&text) else {
                continue;
            };
            if v["method"].as_str() == Some("Page.loadEventFired") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);

    if !loaded {
        tracing::warn!("CDP: page load event not received within timeout");
    }

    // 6. HTML を取得
    ws.send(Message::Text(
        send_cmd(
            &mut ws,
            3,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": "document.documentElement.outerHTML",
                "returnByValue": true,
            }),
        )
        .into(),
    ))
    .await
    .map_err(|e| FetchError::DelegationFailed(format!("CDP evaluate send failed: {}", e)))?;

    let html = tokio::time::timeout(tokio::time::Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            let Ok(Message::Text(text)) = msg else {
                continue;
            };
            let Ok(v): Result<Value, _> = serde_json::from_str(&text) else {
                continue;
            };
            if v["id"].as_u64() == Some(3) {
                return v["result"]["result"]["value"]
                    .as_str()
                    .map(|s| s.to_owned());
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
    .ok_or_else(|| FetchError::DelegationFailed("CDP: failed to get page HTML".into()))?;

    // 7. タブを閉じる
    let _ = reqwest::Client::new()
        .get(format!(
            "http://localhost:{}/json/close/{}",
            port, target_id
        ))
        .send()
        .await;

    Ok(RawHtml {
        bytes: html.into_bytes(),
        url: url.clone(),
        content_type: "text/html; charset=utf-8".to_owned(),
        status: 200,
    })
}

/// CDP feature が無効な場合は常にエラーを返す
#[cfg(not(feature = "cdp"))]
pub async fn fetch_via_cdp(_port: u16, _url: &Url) -> Result<RawHtml, FetchError> {
    Err(FetchError::DelegationFailed(
        "CDP support not compiled in (build with --features cdp)".into(),
    ))
}
