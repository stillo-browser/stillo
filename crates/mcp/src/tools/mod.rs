pub mod extract_structured;
pub mod fetch_url;
pub mod read_links;
pub mod search;

use serde_json::Value;
use stillo_fetcher::{HttpConfig, HttpFetcher};

/// MCP ツール用の HTTP フェッチャーを作る。
///
/// MCP では取得先 URL を LLM が決めるため、既定でプライベートネットワークへの接続を拒否する
/// （ローカル管理画面やクラウドのメタデータエンドポイントへの SSRF 対策）。
/// ローカルの開発サーバーを読ませたい場合は `STILLO_ALLOW_PRIVATE_NETWORK=1` で解除する。
pub(crate) fn mcp_fetcher() -> HttpFetcher {
    let allow_private =
        std::env::var("STILLO_ALLOW_PRIVATE_NETWORK").is_ok_and(|v| v.trim() == "1");
    HttpFetcher::new(HttpConfig {
        deny_private_network: !allow_private,
        ..Default::default()
    })
}

/// ツール引数の `url` を取り出し、http/https 以外を拒否する。
pub(crate) fn parse_http_url(args: &Value) -> anyhow::Result<url::Url> {
    let url_str = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing 'url'"))?;
    let url: url::Url = url_str.parse()?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err(anyhow::anyhow!(
            "unsupported URL scheme '{}': only http and https are allowed",
            other
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_url_rejects_other_schemes() {
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/",
            "data:text/html,hi",
        ] {
            let args = serde_json::json!({ "url": bad });
            assert!(parse_http_url(&args).is_err(), "{bad}");
        }
        let args = serde_json::json!({ "url": "https://example.com/" });
        assert!(parse_http_url(&args).is_ok());
    }

    #[tokio::test]
    async fn mcp_fetcher_blocks_loopback_by_default() {
        // 環境変数を変更すると並列テストに影響するため、未設定（既定）の挙動だけを確認する
        if std::env::var_os("STILLO_ALLOW_PRIVATE_NETWORK").is_some() {
            return;
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = url::Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap();
        let err = mcp_fetcher().fetch(&url).await.unwrap_err();
        assert!(
            matches!(err, stillo_core::document::FetchError::Blocked { .. }),
            "{err:?}"
        );
    }
}
