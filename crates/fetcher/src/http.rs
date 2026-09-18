use std::time::Duration;
use reqwest::{Client, ClientBuilder, redirect};
use url::Url;
use stillo_core::document::{FetchError, RawHtml};

pub struct HttpConfig {
    pub timeout_secs: u64,
    pub follow_redirects: bool,
    pub max_redirects: usize,
    pub user_agent: String,
    pub accept_language: String,
    pub cookie_store: bool,
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
        }
    }
}

#[derive(Clone)]
pub struct HttpFetcher {
    client: Client,
}

impl HttpFetcher {
    pub fn new(config: HttpConfig) -> Self {
        let redirect_policy = if config.follow_redirects {
            redirect::Policy::limited(config.max_redirects)
        } else {
            redirect::Policy::none()
        };

        let client = ClientBuilder::new()
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
            })
            .build()
            .expect("failed to build HTTP client");

        Self { client }
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

    async fn fetch_inner(&self, url: &Url, extra_headers: &[(&str, &str)]) -> Result<RawHtml, FetchError> {
        let mut request = self.client.get(url.as_str());
        for (name, value) in extra_headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.map_err(|e| {
            if e.is_timeout() {
                FetchError::Timeout { seconds: 30 }
            } else {
                FetchError::Tls(e.to_string())
            }
        })?;

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

        let bytes = response.bytes().await.map_err(|e| FetchError::Tls(e.to_string()))?.to_vec();

        Ok(RawHtml {
            bytes,
            url: final_url,
            content_type,
            status,
        })
    }
}
