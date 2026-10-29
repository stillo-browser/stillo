use crate::net_guard::{self, BlockedTarget, GuardedResolver};
use reqwest::{redirect, Client, ClientBuilder, Response};
use std::sync::Arc;
use std::time::Duration;
use stillo_core::document::{FetchError, RawHtml};
use url::Url;

/// レスポンスボディ上限の既定値（10MB）。
pub const DEFAULT_MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

pub struct HttpConfig {
    pub timeout_secs: u64,
    pub follow_redirects: bool,
    pub max_redirects: usize,
    pub user_agent: String,
    pub accept_language: String,
    pub cookie_store: bool,
    /// レスポンスボディの上限バイト数。超えたら `FetchError::BodyTooLarge`
    pub max_body_bytes: usize,
    /// true のとき loopback / private / link-local 等への接続を拒否する（SSRF 対策）。
    /// URL を外部入力（LLM）が決める MCP 経路で有効にする
    pub deny_private_network: bool,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            follow_redirects: true,
            max_redirects: 10,
            user_agent: "stillo/0.1".to_owned(),
            accept_language: "ja,en;q=0.9".to_owned(),
            cookie_store: true,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            deny_private_network: false,
        }
    }
}

#[derive(Clone)]
pub struct HttpFetcher {
    client: Client,
    timeout_secs: u64,
    max_body_bytes: usize,
    deny_private_network: bool,
}

impl HttpFetcher {
    pub fn new(config: HttpConfig) -> Self {
        let deny_private = config.deny_private_network;
        let redirect_policy = if config.follow_redirects {
            let max = config.max_redirects;
            // 既定の Policy::limited ではホップ先を検査できないため custom にする。
            // ホスト名の宛先は resolver が検査するが、IP リテラルは DNS を経ないのでここで弾く
            redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= max {
                    return attempt.error(format!("too many redirects (max {})", max));
                }
                match net_guard::check_url(attempt.url(), deny_private) {
                    Ok(()) => attempt.follow(),
                    Err(reason) => {
                        let reason = format!("redirect to {}: {}", attempt.url(), reason);
                        attempt.error(BlockedTarget(reason))
                    }
                }
            })
        } else {
            redirect::Policy::none()
        };

        let mut builder = ClientBuilder::new()
            .use_rustls_tls()
            .redirect(redirect_policy)
            .cookie_store(config.cookie_store)
            .timeout(Duration::from_secs(config.timeout_secs))
            .user_agent(&config.user_agent)
            .default_headers({
                let mut headers = reqwest::header::HeaderMap::new();
                if let Ok(val) = config.accept_language.parse() {
                    headers.insert(reqwest::header::ACCEPT_LANGUAGE, val);
                }
                headers.insert(
                    reqwest::header::ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
                        .parse()
                        .unwrap(),
                );
                headers
            });
        if deny_private {
            // プロキシ経由だと宛先の名前解決はプロキシ側で行われ、resolver の検査を素通りする。
            // また社内プロキシ自体がプライベート IP なので、拒否モードではプロキシを使わない
            builder = builder.dns_resolver(Arc::new(GuardedResolver)).no_proxy();
        }
        let client = builder.build().expect("failed to build HTTP client");

        Self {
            client,
            timeout_secs: config.timeout_secs,
            max_body_bytes: config.max_body_bytes,
            deny_private_network: deny_private,
        }
    }

    pub async fn fetch(&self, url: &Url) -> Result<RawHtml, FetchError> {
        self.fetch_inner(url, &[]).await
    }

    /// 追加ヘッダーを付けて GET する（検索API等の認証ヘッダー用）。
    pub async fn fetch_with_headers(
        &self,
        url: &Url,
        headers: &[(&str, &str)],
    ) -> Result<RawHtml, FetchError> {
        self.fetch_inner(url, headers).await
    }

    async fn fetch_inner(
        &self,
        url: &Url,
        extra_headers: &[(&str, &str)],
    ) -> Result<RawHtml, FetchError> {
        net_guard::check_url(url, self.deny_private_network).map_err(|reason| {
            FetchError::Blocked {
                url: url.clone(),
                reason,
            }
        })?;

        let mut request = self.client.get(url.as_str());
        for (name, value) in extra_headers {
            request = request.header(*name, *value);
        }
        let response = request
            .send()
            .await
            .map_err(|e| map_reqwest_error(e, url, self.timeout_secs))?;

        let status = response.status().as_u16();
        if status >= 400 {
            return Err(FetchError::Http {
                status,
                url: url.clone(),
            });
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/html")
            .to_owned();

        // 最終リダイレクト先のURLを保持
        let final_url = response.url().clone();
        let final_url = Url::parse(final_url.as_str()).unwrap_or_else(|_| url.clone());

        let bytes =
            read_body_limited(response, self.max_body_bytes, url, self.timeout_secs).await?;

        Ok(RawHtml {
            bytes,
            url: final_url,
            content_type,
            status,
        })
    }
}

/// reqwest のエラーを FetchError に分類する。
///
/// reqwest の `Display` は外側の文脈しか出さず「error sending request」のように原因が
/// 見えないため、source chain を連結してメッセージにする。
pub(crate) fn map_reqwest_error(e: reqwest::Error, url: &Url, timeout_secs: u64) -> FetchError {
    if let Some(reason) = net_guard::find_blocked(&e) {
        return FetchError::Blocked {
            url: url.clone(),
            reason,
        };
    }
    if e.is_timeout() {
        return FetchError::Timeout {
            seconds: timeout_secs,
        };
    }
    let msg = error_chain(&e);
    if e.is_builder() {
        FetchError::InvalidUrl(msg)
    } else if e.is_redirect() {
        FetchError::Redirect(msg)
    } else if e.is_connect() {
        FetchError::Connect(msg)
    } else {
        FetchError::Request(msg)
    }
}

fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut msg = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        cur = s.source();
    }
    msg
}

/// 上限付きでレスポンスボディを読み切る。
///
/// Content-Length は省略・詐称されうるため事前チェックだけに頼らず、
/// 受信した累積バイト数でも打ち切る。巨大レスポンスでメモリを使い切らないため。
pub(crate) async fn read_body_limited(
    mut response: Response,
    limit: usize,
    url: &Url,
    timeout_secs: u64,
) -> Result<Vec<u8>, FetchError> {
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err(FetchError::BodyTooLarge { limit });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| map_reqwest_error(e, url, timeout_secs))?
    {
        if body.len() + chunk.len() > limit {
            return Err(FetchError::BodyTooLarge { limit });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 受け付けた接続ごとに `respond` が返すバイト列を書き込む HTTP サーバーを起動する。
    /// `respond` にはリクエストの1行目（例: "GET /a HTTP/1.1"）が渡る。
    async fn serve(respond: fn(&str) -> Vec<u8>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let line = req.lines().next().unwrap_or("").to_owned();
                    stream.write_all(&respond(&line)).await.ok();
                    stream.shutdown().await.ok();
                });
            }
        });
        port
    }

    fn ok_html(_: &str) -> Vec<u8> {
        let body = "<html><body>hello</body></html>";
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    }

    fn deny_config() -> HttpConfig {
        HttpConfig {
            timeout_secs: 5,
            deny_private_network: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn deny_mode_blocks_ip_literal_loopback() {
        let port = serve(ok_html).await;
        let fetcher = HttpFetcher::new(deny_config());
        let url = Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap();
        let err = fetcher.fetch(&url).await.unwrap_err();
        assert!(matches!(err, FetchError::Blocked { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn deny_mode_blocks_hostname_resolving_to_loopback() {
        // localhost は /etc/hosts で解決されるためネットワーク不要。
        // URL 文字列ではなく DNS 解決結果の IP で拒否されることを確認する
        let port = serve(ok_html).await;
        let fetcher = HttpFetcher::new(deny_config());
        let url = Url::parse(&format!("http://localhost:{}/", port)).unwrap();
        let err = fetcher.fetch(&url).await.unwrap_err();
        match err {
            FetchError::Blocked { reason, .. } => {
                assert!(reason.contains("localhost"), "{reason}")
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allow_mode_fetches_localhost() {
        let port = serve(ok_html).await;
        let fetcher = HttpFetcher::new(HttpConfig::default());
        let url = Url::parse(&format!("http://localhost:{}/", port)).unwrap();
        let raw = fetcher.fetch(&url).await.unwrap();
        assert!(String::from_utf8_lossy(&raw.bytes).contains("hello"));
    }

    #[tokio::test]
    async fn redirect_to_non_http_scheme_is_rejected() {
        fn redirect_to_ftp(_: &str) -> Vec<u8> {
            b"HTTP/1.1 302 Found\r\nLocation: ftp://127.0.0.1/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
        let port = serve(redirect_to_ftp).await;
        let fetcher = HttpFetcher::new(HttpConfig::default());
        let url = Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap();
        let err = fetcher.fetch(&url).await.unwrap_err();
        assert!(matches!(err, FetchError::Blocked { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn redirect_loop_is_reported_as_redirect_error() {
        fn self_redirect(_: &str) -> Vec<u8> {
            b"HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
        let port = serve(self_redirect).await;
        let fetcher = HttpFetcher::new(HttpConfig {
            max_redirects: 3,
            ..Default::default()
        });
        let url = Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap();
        let err = fetcher.fetch(&url).await.unwrap_err();
        assert!(matches!(err, FetchError::Redirect(_)), "{err:?}");
    }

    #[tokio::test]
    async fn connection_refused_is_connect_error_not_tls() {
        // bind して即 drop したポートには何もリッスンしていない
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let fetcher = HttpFetcher::new(HttpConfig::default());
        let url = Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap();
        let err = fetcher.fetch(&url).await.unwrap_err();
        assert!(matches!(err, FetchError::Connect(_)), "{err:?}");
    }

    #[tokio::test]
    async fn timeout_reports_configured_seconds() {
        // 接続は受け付けるが応答しないサーバー
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                held.push(s);
            }
        });
        let fetcher = HttpFetcher::new(HttpConfig {
            timeout_secs: 1,
            ..Default::default()
        });
        let url = Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap();
        let err = fetcher.fetch(&url).await.unwrap_err();
        assert!(matches!(err, FetchError::Timeout { seconds: 1 }), "{err:?}");
    }

    #[tokio::test]
    async fn body_over_limit_by_content_length_is_rejected() {
        let port = serve(ok_html).await;
        let fetcher = HttpFetcher::new(HttpConfig {
            max_body_bytes: 10,
            ..Default::default()
        });
        let url = Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap();
        let err = fetcher.fetch(&url).await.unwrap_err();
        assert!(
            matches!(err, FetchError::BodyTooLarge { limit: 10 }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn body_over_limit_without_content_length_is_rejected() {
        // Content-Length なし（接続 close まで読む）でも累積量で打ち切る
        fn no_length(_: &str) -> Vec<u8> {
            let mut v =
                b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n".to_vec();
            v.extend(std::iter::repeat_n(b'a', 4096));
            v
        }
        let port = serve(no_length).await;
        let fetcher = HttpFetcher::new(HttpConfig {
            max_body_bytes: 1000,
            ..Default::default()
        });
        let url = Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap();
        let err = fetcher.fetch(&url).await.unwrap_err();
        assert!(
            matches!(err, FetchError::BodyTooLarge { limit: 1000 }),
            "{err:?}"
        );
    }
}
