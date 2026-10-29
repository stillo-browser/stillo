pub mod cdp;
pub mod firecrawl;
pub mod jina;
pub mod playwright;

use reqwest::{Client, ClientBuilder};
use std::time::Duration;
use stillo_core::document::{DelegationTarget, FetchError, RawHtml};
use url::Url;

/// Jina / Firecrawl 呼び出しのタイムアウト秒数
pub(crate) const SPA_HTTP_TIMEOUT_SECS: u64 = 60;

pub struct SpaDelegationChain {
    targets: Vec<DelegationTarget>,
    http: Client,
    /// チェーン全体の期限。個々のターゲットのタイムアウトとは別に、合計時間を `--timeout` に収めるため
    timeout: Option<Duration>,
}

impl SpaDelegationChain {
    /// 環境変数とChromeの有無からチェーンを構築する。
    /// 呼び出し時に Chrome の到達確認は行わない（fetch_with_js 時に判定）。
    pub fn from_env(cdp_port: u16) -> Self {
        let mut targets = Vec::new();

        // 1. LocalCdp — 常にリストに入れる（起動確認は fetch 時）
        targets.push(DelegationTarget::LocalCdp { port: cdp_port });

        // 2. PlaywrightDaemon — 自ユーザー所有のソケットがあれば追加
        if let Some(socket_path) = playwright::discover_socket() {
            targets.push(DelegationTarget::PlaywrightDaemon { socket_path });
        }

        // 3. JinaReader — 閲覧 URL を外部サービスへ送るためオプトイン
        if let Some(jina) = jina_target_from(
            std::env::var("JINA_API_KEY").ok(),
            std::env::var("STILLO_ENABLE_JINA").ok(),
        ) {
            targets.push(jina);
        }

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

        Self::with_targets(targets)
    }

    /// 特定のターゲットのみを使うチェーンを構築する
    pub fn with_single_target(target: DelegationTarget) -> Self {
        Self::with_targets(vec![target])
    }

    fn with_targets(targets: Vec<DelegationTarget>) -> Self {
        let http = ClientBuilder::new()
            .use_rustls_tls()
            .timeout(Duration::from_secs(SPA_HTTP_TIMEOUT_SECS))
            .user_agent("stillo/0.1")
            .build()
            .expect("failed to build SPA HTTP client");

        Self {
            targets,
            http,
            timeout: None,
        }
    }

    /// チェーン全体の期限を設定する。期限を過ぎると実行中のターゲットを打ち切る。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// フォールバックチェーンを実行し、最初に成功したターゲットの結果を返す。
    ///
    /// 全滅時は最後のエラーだけでなく全ターゲットの失敗理由を返す。
    /// 先頭の CDP が「Chrome 未起動」で落ちたのか、Jina が 4xx を返したのかを区別できないと
    /// 利用者が対処を判断できないため。
    pub async fn fetch_with_js(&self, url: &Url) -> Result<RawHtml, FetchError> {
        if self.targets.is_empty() {
            return Err(FetchError::NoDelegationAvailable);
        }

        let deadline = self.timeout.map(|t| tokio::time::Instant::now() + t);
        let mut failures = Vec::new();

        for target in &self.targets {
            tracing::debug!("trying delegation target: {:?}", target);
            let attempt = self.try_target(target, url);
            let result = match deadline {
                Some(d) => match tokio::time::timeout_at(d, attempt).await {
                    Ok(r) => r,
                    Err(_) => {
                        let seconds = self.timeout.map_or(0, |t| t.as_secs());
                        failures.push(format!(
                            "{}: {}",
                            target_name(target),
                            FetchError::Timeout { seconds }
                        ));
                        // 期限切れ後に残りのターゲットを試しても即座に打ち切られるだけなので終える
                        break;
                    }
                },
                None => attempt.await,
            };
            match result {
                Ok(html) => {
                    tracing::info!("delegation succeeded via {:?}", target);
                    return Ok(html);
                }
                Err(e) => {
                    tracing::warn!("delegation target {:?} failed: {}", target, e);
                    failures.push(format!("{}: {}", target_name(target), e));
                }
            }
        }

        Err(FetchError::DelegationChainFailed(failures))
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

/// 失敗理由の集約で使うターゲットの短い名前
fn target_name(target: &DelegationTarget) -> &'static str {
    match target {
        DelegationTarget::LocalCdp { .. } => "cdp",
        DelegationTarget::PlaywrightDaemon { .. } => "playwright",
        DelegationTarget::JinaReader { .. } => "jina",
        DelegationTarget::Firecrawl { .. } => "firecrawl",
        DelegationTarget::Unavailable { .. } => "unavailable",
    }
}

/// Jina Reader をチェーンに入れるか判定する。
///
/// API キーが設定されているか `STILLO_ENABLE_JINA=1` のときだけ有効にする。
/// Jina は閲覧先 URL を第三者サービスに送信するため、明示的な設定なしに使わない。
fn jina_target_from(
    api_key: Option<String>,
    enable_flag: Option<String>,
) -> Option<DelegationTarget> {
    let api_key = api_key.filter(|k| !k.is_empty());
    let enabled = enable_flag.as_deref().map(str::trim) == Some("1");
    if api_key.is_some() || enabled {
        Some(DelegationTarget::JinaReader { api_key })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jina_is_opt_in() {
        assert_eq!(jina_target_from(None, None), None);
        assert_eq!(jina_target_from(Some(String::new()), None), None);
        assert_eq!(jina_target_from(None, Some("0".into())), None);
        assert_eq!(
            jina_target_from(None, Some("1".into())),
            Some(DelegationTarget::JinaReader { api_key: None })
        );
        assert_eq!(
            jina_target_from(Some("k".into()), None),
            Some(DelegationTarget::JinaReader {
                api_key: Some("k".into())
            })
        );
    }

    #[tokio::test]
    async fn chain_failure_reports_every_target() {
        let chain = SpaDelegationChain::with_targets(vec![
            DelegationTarget::Unavailable {
                reason: "first reason".into(),
            },
            DelegationTarget::Unavailable {
                reason: "second reason".into(),
            },
        ]);
        let url = Url::parse("https://example.com/").unwrap();
        let err = chain.fetch_with_js(&url).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("first reason"), "{msg}");
        assert!(msg.contains("second reason"), "{msg}");
    }

    #[tokio::test]
    async fn chain_timeout_cuts_off_slow_target() {
        // 応答しない Playwright デーモンを模した Unix ソケット
        let dir = std::env::temp_dir().join(format!("stillo-chain-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("pw.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                held.push(s);
            }
        });

        let chain = SpaDelegationChain::with_single_target(DelegationTarget::PlaywrightDaemon {
            socket_path: sock.clone(),
        })
        .with_timeout(Duration::from_secs(1));
        let url = Url::parse("https://example.com/").unwrap();
        let started = std::time::Instant::now();
        let err = chain.fetch_with_js(&url).await.unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(err.to_string().contains("Timeout"), "{err}");
    }
}
