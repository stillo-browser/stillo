pub mod cdp;
pub mod firecrawl;
pub mod jina;
pub mod playwright;

use reqwest::{Client, ClientBuilder};
use std::path::PathBuf;
use std::time::Duration;
use stillo_core::document::{DelegationTarget, FetchError, RawHtml};
use url::Url;

pub struct SpaDelegationChain {
    targets: Vec<DelegationTarget>,
    http: Client,
}

impl SpaDelegationChain {
    /// 環境変数とChromeの有無からチェーンを構築する。
    /// 呼び出し時に Chrome の到達確認は行わない（fetch_with_js 時に判定）。
    pub fn from_env(cdp_port: u16) -> Self {
        let mut targets = Vec::new();

        // 1. LocalCdp — 常にリストに入れる（起動確認は fetch 時）
        targets.push(DelegationTarget::LocalCdp { port: cdp_port });

        // 2. PlaywrightDaemon — ソケットが存在すれば追加
        let sock = PathBuf::from("/tmp/stillo-playwright.sock");
        if sock.exists() {
            targets.push(DelegationTarget::PlaywrightDaemon { socket_path: sock });
        }

        // 3. JinaReader — 常に追加（無料tierあり）
        let jina_key = std::env::var("JINA_API_KEY").ok();
        targets.push(DelegationTarget::JinaReader { api_key: jina_key });

        // 4. Firecrawl — 両環境変数があれば追加
        if let (Ok(fc_url), Ok(fc_key)) = (
            std::env::var("FIRECRAWL_URL"),
            std::env::var("FIRECRAWL_API_KEY"),
        ) {
            if let Ok(base_url) = fc_url.parse::<Url>() {
                targets.push(DelegationTarget::Firecrawl {
                    base_url,
                    api_key: fc_key,
                });
            }
        }

        let http = ClientBuilder::new()
            .use_rustls_tls()
            .timeout(Duration::from_secs(60))
            .user_agent("stillo/0.1")
            .build()
            .expect("failed to build SPA HTTP client");

        Self { targets, http }
    }

    /// 特定のターゲットのみを使うチェーンを構築する
    pub fn with_single_target(target: DelegationTarget) -> Self {
        let http = ClientBuilder::new()
            .use_rustls_tls()
            .timeout(Duration::from_secs(60))
            .user_agent("stillo/0.1")
            .build()
            .expect("failed to build SPA HTTP client");

        Self {
            targets: vec![target],
            http,
        }
    }

    /// フォールバックチェーンを実行し、最初に成功したターゲットの結果を返す
    pub async fn fetch_with_js(&self, url: &Url) -> Result<RawHtml, FetchError> {
        if self.targets.is_empty() {
            return Err(FetchError::NoDelegationAvailable);
        }

        let mut last_error = FetchError::NoDelegationAvailable;

        for target in &self.targets {
            tracing::debug!("trying delegation target: {:?}", target);
            match self.try_target(target, url).await {
                Ok(html) => {
                    tracing::info!("delegation succeeded via {:?}", target);
                    return Ok(html);
                }
                Err(e) => {
                    tracing::warn!("delegation target {:?} failed: {}", target, e);
                    last_error = e;
                }
            }
        }

        Err(last_error)
    }

    async fn try_target(
        &self,
        target: &DelegationTarget,
        url: &Url,
    ) -> Result<RawHtml, FetchError> {
        match target {
            DelegationTarget::LocalCdp { port } => {
                // Chrome の到達確認を先に行いスキップコストを下げる
                if !cdp::is_chrome_available(*port).await {
                    return Err(FetchError::DelegationFailed(format!(
                        "Chrome not reachable at localhost:{}",
                        port
                    )));
                }
                cdp::fetch_via_cdp(*port, url).await
            }
            DelegationTarget::PlaywrightDaemon { socket_path } => {
                playwright::fetch_via_playwright(socket_path, url).await
            }
            DelegationTarget::JinaReader { api_key } => {
                jina::fetch_via_jina(&self.http, api_key.as_deref(), url).await
            }
            DelegationTarget::Firecrawl { base_url, api_key } => {
                firecrawl::fetch_via_firecrawl(&self.http, base_url, api_key, url).await
            }
            DelegationTarget::Unavailable { reason } => {
                Err(FetchError::DelegationFailed(reason.clone()))
            }
        }
    }
}
