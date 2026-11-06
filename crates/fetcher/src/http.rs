use crate::net_guard::{self, BlockedTarget, GuardedResolver};
use reqwest::{redirect, Client, ClientBuilder, Response};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use stillo_core::document::{FetchError, RawHtml};
use url::{Host, Url};

/// レスポンスボディ上限の既定値（10MB）。
pub const DEFAULT_MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// 全 HTTP リクエストで使う User-Agent。クレートのバージョンに追従させる。
pub const USER_AGENT: &str = concat!("stillo/", env!("CARGO_PKG_VERSION"));

/// プライベートネットワーク宛ての取得をどこまで許すかの方針。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PrivateNetworkPolicy {
    /// すべて許可する（ローカル開発用。`--allow-private` / `STILLO_ALLOW_PRIVATE_NETWORK=1`）。
    Allow,
    /// 起点 URL が公開アドレスなら、そこから派生する取得も公開アドレスに限定する（既定）。
    ///
    /// 公開ページのリダイレクト先や `<frame src>` を内部ネットワークへ飛ばし、その本文を
    /// 外部 LLM へ送らせる経路を塞ぐ。ローカルの開発サーバーを直接読む使い方は起点が
    /// 非公開なので、これまでどおり通る。
    #[default]
    GuardPublicOrigin,
    /// 常に拒否する（MCP 経路）。
    DenyAlways,
}

/// 1 回の取得について、宛先の検査を行うかどうか。
///
/// 起点の判定結果を、同じページから派生する取得（フレーム・JS 委譲）へ引き継ぐために使う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DerivedGuard {
    Enforce,
    Allow,
}

pub struct HttpConfig {
    pub timeout_secs: u64,
    pub follow_redirects: bool,
    pub max_redirects: usize,
    pub user_agent: String,
    pub accept_language: String,
    pub cookie_store: bool,
    /// レスポンスボディの上限バイト数。超えたら `FetchError::BodyTooLarge`
    pub max_body_bytes: usize,
    /// プライベートネットワーク宛て取得の扱い（SSRF 対策）。
    /// URL を外部入力（LLM・ページ内容）が決める経路では拒否寄りにする
    pub private_network: PrivateNetworkPolicy,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            follow_redirects: true,
            max_redirects: 10,
            user_agent: USER_AGENT.to_owned(),
            accept_language: "ja,en;q=0.9".to_owned(),
            cookie_store: true,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            private_network: PrivateNetworkPolicy::default(),
        }
    }
}

#[derive(Clone)]
pub struct HttpFetcher {
    /// 検査しないクライアント。非公開の起点（ローカル開発サーバー等）を直接読むときに使う
    client_open: Client,
    /// resolver とリダイレクトの各ホップで宛先を検査するクライアント
    client_guarded: Client,
    policy: PrivateNetworkPolicy,
    timeout_secs: u64,
    max_body_bytes: usize,
}

impl HttpFetcher {
    pub fn new(config: HttpConfig) -> Self {
        let policy = config.private_network;
        // 方針が Allow でも検査用クライアントは作る。呼び出し側が明示的に
        // DerivedGuard::Enforce を渡したときに、それを黙って無視しないため
        let client_open = build_client(&config, false);
        let client_guarded = build_client(&config, true);

        Self {
            client_open,
            client_guarded,
            policy,
            timeout_secs: config.timeout_secs,
            max_body_bytes: config.max_body_bytes,
        }
    }

    /// この起点 URL から派生する取得を検査すべきかを返す。
    ///
    /// 起点自身の取得は `fetch` が同じ規則で判定するため、呼び出し側が明示的に
    /// 判定結果を使いたい場合（フレーム・委譲）だけ呼べばよい。
    pub async fn derived_guard(&self, url: &Url) -> DerivedGuard {
        match self.policy {
            PrivateNetworkPolicy::Allow => DerivedGuard::Allow,
            PrivateNetworkPolicy::DenyAlways => DerivedGuard::Enforce,
            PrivateNetworkPolicy::GuardPublicOrigin => {
                if origin_is_public(url).await {
                    DerivedGuard::Enforce
                } else {
                    DerivedGuard::Allow
                }
            }
        }
    }

    pub async fn fetch(&self, url: &Url) -> Result<RawHtml, FetchError> {
        let guard = self.derived_guard(url).await;
        self.fetch_with_guard(url, guard).await
    }

    /// 追加ヘッダーを付けて GET する（検索API等の認証ヘッダー用）。
    pub async fn fetch_with_headers(
        &self,
        url: &Url,
        headers: &[(&str, &str)],
    ) -> Result<RawHtml, FetchError> {
        let guard = self.derived_guard(url).await;
        self.fetch_inner(url, headers, guard).await
    }

    /// 起点の判定結果を明示して取得する（フレーム等の派生取得用）。
    pub async fn fetch_with_guard(
        &self,
        url: &Url,
        guard: DerivedGuard,
    ) -> Result<RawHtml, FetchError> {
        self.fetch_inner(url, &[], guard).await
    }

    async fn fetch_inner(
        &self,
        url: &Url,
        extra_headers: &[(&str, &str)],
        guard: DerivedGuard,
    ) -> Result<RawHtml, FetchError> {
        let deny = guard == DerivedGuard::Enforce;
        net_guard::check_url(url, deny).map_err(|reason| FetchError::Blocked {
            url: url.clone(),
            reason,
        })?;
        let client = if deny {
            &self.client_guarded
        } else {
            &self.client_open
        };

        let mut request = client.get(url.as_str());
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

/// リダイレクトの1ホップ先を検査する。`guarded` が false でもスキームは必ず検査する
/// （`ftp:` 等へ落とすと、その本文がそのまま取得結果になる）。
fn check_redirect_target(next: &Url, guarded: bool) -> Result<(), String> {
    net_guard::check_url(next, guarded)
        .map_err(|reason| format!("redirect to {}: {}", next, reason))
}

/// 起点 URL が公開アドレスだけを指すかを返す。解決できないホストは安全側（検査する）に倒す。
async fn origin_is_public(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(v4)) => !net_guard::is_blocked_ip(IpAddr::V4(v4)),
        Some(Host::Ipv6(v6)) => !net_guard::is_blocked_ip(IpAddr::V6(v6)),
        Some(Host::Domain(host)) => match tokio::net::lookup_host((host, 0)).await {
            Ok(addrs) => {
                let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
                net_guard::all_public(&ips)
            }
            Err(_) => true,
        },
        None => true,
    }
}

fn build_client(config: &HttpConfig, guarded: bool) -> Client {
    let redirect_policy = if config.follow_redirects {
        let max = config.max_redirects;
        // 既定の Policy::limited ではホップ先を検査できないため custom にする。
        // スキーム検査は常に行い、検査モードでは IP リテラルのホップも弾く
        // （ホスト名は resolver が解決結果で弾く）
        redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= max {
                return attempt.error(format!("too many redirects (max {})", max));
            }
            match check_redirect_target(attempt.url(), guarded) {
                Ok(()) => attempt.follow(),
                Err(reason) => attempt.error(BlockedTarget(reason)),
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
    if guarded {
        // プロキシ経由だと宛先の名前解決はプロキシ側で行われ、resolver の検査を素通りする。
        // また社内プロキシ自体がプライベート IP なので、検査モードではプロキシを使わない
        builder = builder.dns_resolver(Arc::new(GuardedResolver)).no_proxy();
    }
    builder.build().expect("failed to build HTTP client")
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
            private_network: PrivateNetworkPolicy::DenyAlways,
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

    // --- 起点の公開性による派生取得の検査（SSRF） ---

    #[tokio::test]
    async fn guard_public_origin_allows_private_origin() {
        // ローカルの開発サーバーを直接読む使い方は従来どおり通す
        let port = serve(ok_html).await;
        let fetcher = HttpFetcher::new(HttpConfig::default());
        let url = Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap();
        assert_eq!(fetcher.derived_guard(&url).await, DerivedGuard::Allow);
        assert!(fetcher.fetch(&url).await.is_ok());
    }

    #[tokio::test]
    async fn deny_always_guards_every_origin() {
        let fetcher = HttpFetcher::new(deny_config());
        let url = Url::parse("http://127.0.0.1/").unwrap();
        assert_eq!(fetcher.derived_guard(&url).await, DerivedGuard::Enforce);
    }

    #[tokio::test]
    async fn allow_policy_never_guards_but_enforce_is_still_honored() {
        let port = serve(ok_html).await;
        let fetcher = HttpFetcher::new(HttpConfig {
            timeout_secs: 5,
            private_network: PrivateNetworkPolicy::Allow,
            ..Default::default()
        });
        let url = Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap();
        assert_eq!(fetcher.derived_guard(&url).await, DerivedGuard::Allow);
        // 明示的な Enforce は方針より優先する（黙って無視しない）
        let err = fetcher
            .fetch_with_guard(&url, DerivedGuard::Enforce)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Blocked { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn derived_targets_are_blocked_when_the_origin_is_public() {
        // 公開ページのフレームが内部アドレスを指す場合、その取得は拒否される
        let port = serve(ok_html).await;
        let fetcher = HttpFetcher::new(HttpConfig {
            timeout_secs: 5,
            ..Default::default()
        });
        let internal = Url::parse(&format!("http://127.0.0.1:{}/", port)).unwrap();
        let err = fetcher
            .fetch_with_guard(&internal, DerivedGuard::Enforce)
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Blocked { .. }), "{err:?}");
    }

    #[test]
    fn redirect_hops_to_private_targets_are_rejected_only_when_guarded() {
        let hop = |u: &str| Url::parse(u).unwrap();
        // 検査モードでは内部アドレスへのホップを拒否する
        let err = check_redirect_target(&hop("http://169.254.169.254/latest/"), true).unwrap_err();
        assert!(err.contains("private network address"), "{err}");
        assert!(check_redirect_target(&hop("http://127.0.0.1:8080/"), true).is_err());
        // 非検査モードでは通す（ローカル開発の意図を尊重する）
        assert!(check_redirect_target(&hop("http://127.0.0.1:8080/"), false).is_ok());
        // スキーム検査はモードに関係なく効く
        assert!(check_redirect_target(&hop("ftp://127.0.0.1/"), false).is_err());
    }
}
