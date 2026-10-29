use std::time::Duration;
use stillo_core::document::{FetchError, RawHtml};
use url::Url;

/// CDP の HTTP エンドポイント（/json/*）呼び出しのタイムアウト。
/// ポートを別プロセスが握っていて応答しない場合に委譲チェーン全体を止めないため
const CDP_HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// loadEventFired を待つ上限。超えたら読み込み途中の DOM を取得する。
/// 委譲チェーンの期限（CLI 既定 --timeout 30 秒）より短くしないと、
/// 長時間ポーリングするページで HTML 取得前にチェーンごと打ち切られるため
#[cfg(feature = "cdp")]
const LOAD_WAIT: Duration = Duration::from_secs(15);

/// /json/version / /json/new / /json/close 用の HTTP クライアント
fn cdp_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(CDP_HTTP_TIMEOUT)
        .build()
        .expect("failed to build CDP HTTP client")
}

/// Chrome が CDP ポートでリッスンしているか確認する
pub async fn is_chrome_available(port: u16) -> bool {
    cdp_http_client()
        .get(format!("http://localhost:{}/json/version", port))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Chrome DevTools Protocol 経由でSPAのHTMLを取得する。
/// Chrome が --remote-debugging-port={port} で起動済みである必要がある。
///
/// 作成したタブは成否にかかわらず閉じる。呼び出し側の timeout で future ごと
/// 破棄された場合も `TabGuard` の Drop で閉じる（ユーザーの Chrome にタブが溜まるのを防ぐ）。
#[cfg(feature = "cdp")]
pub async fn fetch_via_cdp(port: u16, url: &Url) -> Result<RawHtml, FetchError> {
    let client = cdp_http_client();
    let new_tab: serde_json::Value = client
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
    let mut guard = TabGuard {
        client,
        port,
        target_id: Some(target_id),
    };

    let result = match new_tab["webSocketDebuggerUrl"].as_str() {
        Some(ws_url) => fetch_in_tab(ws_url, url).await,
        None => Err(FetchError::DelegationFailed("CDP: no WebSocket URL".into())),
    };

    guard.close().await;
    result
}

/// 作成したタブを確実に閉じるためのガード。
#[cfg(feature = "cdp")]
struct TabGuard {
    client: reqwest::Client,
    port: u16,
    /// close 済みなら None
    target_id: Option<String>,
}

#[cfg(feature = "cdp")]
impl TabGuard {
    async fn close(&mut self) {
        if let Some(id) = self.target_id.take() {
            close_tab(&self.client, self.port, &id).await;
        }
    }
}

#[cfg(feature = "cdp")]
impl Drop for TabGuard {
    fn drop(&mut self) {
        // ここに来るのは future がキャンセルされた場合のみ。Drop では await できないため
        // 実行中のランタイムに close を投げる（ランタイム終了中なら諦める）
        if let Some(id) = self.target_id.take() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let client = self.client.clone();
                let port = self.port;
                handle.spawn(async move { close_tab(&client, port, &id).await });
            }
        }
    }
}

#[cfg(feature = "cdp")]
async fn close_tab(client: &reqwest::Client, port: u16, target_id: &str) {
    let _ = client
        .get(format!(
            "http://localhost:{}/json/close/{}",
            port, target_id
        ))
        .send()
        .await;
}

/// タブの WebSocket に接続してナビゲーションし、描画後の HTML と最終 URL を取得する。
#[cfg(feature = "cdp")]
async fn fetch_in_tab(ws_url: &str, url: &Url) -> Result<RawHtml, FetchError> {
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(10), connect_async(ws_url))
        .await
        .map_err(|_| FetchError::DelegationFailed("CDP WebSocket connect timed out".into()))?
        .map_err(|e| {
            FetchError::DelegationFailed(format!("CDP WebSocket connect failed: {}", e))
        })?;

    ws.send(Message::Text(
        cdp_command(1, "Page.enable", serde_json::json!({})).into(),
    ))
    .await
    .map_err(|e| FetchError::DelegationFailed(format!("CDP Page.enable failed: {}", e)))?;

    ws.send(Message::Text(
        cdp_command(
            2,
            "Page.navigate",
            serde_json::json!({ "url": url.as_str() }),
        )
        .into(),
    ))
    .await
    .map_err(|e| FetchError::DelegationFailed(format!("CDP navigate failed: {}", e)))?;

    // Page.navigate の応答（id=2）と loadEventFired の両方を待つ。
    // 接続拒否・DNS 失敗などは応答の errorText でしか分からず、見落とすと
    // Chrome のエラーページの HTML を成功として返してしまう
    let mut navigated = false;
    let mut loaded = false;
    let wait = tokio::time::timeout(LOAD_WAIT, async {
        while let Some(msg) = ws.next().await {
            let Ok(Message::Text(text)) = msg else {
                continue;
            };
            let Ok(v): Result<Value, _> = serde_json::from_str(&text) else {
                continue;
            };
            if v["id"].as_u64() == Some(2) {
                if let Some(err) = navigate_error(&v) {
                    return Err(FetchError::DelegationFailed(format!(
                        "CDP navigate to {} failed: {}",
                        url, err
                    )));
                }
                navigated = true;
            } else if v["method"].as_str() == Some("Page.loadEventFired") {
                loaded = true;
            }
            if navigated && loaded {
                break;
            }
        }
        Ok(())
    })
    .await;
    if let Ok(Err(e)) = wait {
        return Err(e);
    }
    if !navigated {
        return Err(FetchError::DelegationFailed(
            "CDP: no response to Page.navigate within timeout".into(),
        ));
    }
    if !loaded {
        tracing::warn!("CDP: page load event not received within timeout");
    }

    // HTML と最終 URL を 1 回の評価でまとめて取る。
    // リダイレクトや JS による遷移後の URL を RawHtml.url にしないと相対リンクの解決がずれる
    ws.send(Message::Text(
        cdp_command(
            3,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": "({ html: document.documentElement.outerHTML, url: location.href })",
                "returnByValue": true,
            }),
        )
        .into(),
    ))
    .await
    .map_err(|e| FetchError::DelegationFailed(format!("CDP evaluate send failed: {}", e)))?;

    let value = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(msg) = ws.next().await {
            let Ok(Message::Text(text)) = msg else {
                continue;
            };
            let Ok(v): Result<Value, _> = serde_json::from_str(&text) else {
                continue;
            };
            if v["id"].as_u64() == Some(3) {
                return Some(v["result"]["result"]["value"].clone());
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
    .ok_or_else(|| FetchError::DelegationFailed("CDP: failed to get page HTML".into()))?;

    let html = value["html"]
        .as_str()
        .ok_or_else(|| FetchError::DelegationFailed("CDP: failed to get page HTML".into()))?
        .to_owned();
    let final_url = value["url"]
        .as_str()
        .and_then(|s| Url::parse(s).ok())
        .unwrap_or_else(|| url.clone());

    let _ = ws.close(None).await;

    Ok(RawHtml {
        bytes: html.into_bytes(),
        url: final_url,
        content_type: "text/html; charset=utf-8".to_owned(),
        status: 200,
    })
}

/// Page.navigate の応答からエラー内容を取り出す。
/// CDP プロトコルエラー（`error`）とナビゲーション失敗（`result.errorText`）の両方を見る。
#[cfg(feature = "cdp")]
fn navigate_error(resp: &serde_json::Value) -> Option<String> {
    if let Some(msg) = resp["error"]["message"].as_str() {
        return Some(msg.to_owned());
    }
    resp["result"]["errorText"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
}

/// CDP コマンドの JSON-RPC メッセージ文字列を組み立てる
#[cfg(feature = "cdp")]
fn cdp_command(id: u64, method: &str, params: serde_json::Value) -> String {
    serde_json::json!({
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string()
}

/// CDP feature が無効な場合は常にエラーを返す
#[cfg(not(feature = "cdp"))]
pub async fn fetch_via_cdp(_port: u16, _url: &Url) -> Result<RawHtml, FetchError> {
    Err(FetchError::DelegationFailed(
        "CDP support not compiled in (build with --features cdp)".into(),
    ))
}

#[cfg(all(test, feature = "cdp"))]
mod tests {
    use super::*;

    #[test]
    fn navigate_error_detects_error_text_and_protocol_error() {
        let failed = serde_json::json!({
            "id": 2,
            "result": { "frameId": "F", "errorText": "net::ERR_CONNECTION_REFUSED" }
        });
        assert_eq!(
            navigate_error(&failed).as_deref(),
            Some("net::ERR_CONNECTION_REFUSED")
        );
        let protocol = serde_json::json!({ "id": 2, "error": { "code": -32000, "message": "Cannot navigate to invalid URL" } });
        assert!(navigate_error(&protocol).is_some());
        let ok = serde_json::json!({ "id": 2, "result": { "frameId": "F", "loaderId": "L" } });
        assert_eq!(navigate_error(&ok), None);
    }
}
