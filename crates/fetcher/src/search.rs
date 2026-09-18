//! Web検索のフェッチチェーン。
//!
//! バックエンドの優先順位は環境変数で決まる:
//! 1. `STILLO_SEARCH_BACKEND=ddg|searxng|brave` — 指定した1つだけを使う
//! 2. `SEARXNG_URL` が設定されていれば先頭に SearXNG を追加
//! 3. `BRAVE_API_KEY` が設定されていれば Brave Search API を追加
//! 4. DuckDuckGo HTML は常に末尾のフォールバック
//!
//! ブロックページは「空結果」ではなくエラーとして伝播させ、
//! 次のバックエンドへフォールバックする。全てブロックされた場合は
//! 何が起きたか（各バックエンドの失敗理由）をまとめて返す。

use std::time::Duration;
use thiserror::Error;
use url::Url;

use stillo_core::search::{
    detect_blocked_page, parse_brave_results, parse_ddg_results, parse_searxng_results,
    render_results_markdown, SearchResult,
};

use crate::{HttpConfig, HttpFetcher};

const DDG_HTML_ENDPOINT: &str = "https://html.duckduckgo.com/html/";

#[derive(Debug, Clone)]
pub enum SearchBackend {
    DuckDuckGo,
    Searxng { base_url: Url },
    Brave { api_key: String },
}

impl SearchBackend {
    /// 環境変数からバックエンドチェーンを構築する（優先順・重複なし）。
    ///
    /// `STILLO_SEARCH_BACKEND` で単独指定された場合はその1つのみ返す。
    pub fn from_env() -> Vec<SearchBackend> {
        if let Ok(single) = std::env::var("STILLO_SEARCH_BACKEND") {
            match single.trim().to_lowercase().as_str() {
                "ddg" | "duckduckgo" => return vec![SearchBackend::DuckDuckGo],
                "searxng" => {
                    if let Some(base_url) = searxng_base_url() {
                        return vec![SearchBackend::Searxng { base_url }];
                    }
                    tracing::warn!("STILLO_SEARCH_BACKEND=searxng だが SEARXNG_URL が未設定。デフォルトチェーンにフォールバック");
                }
                "brave" => {
                    if let Some(key) = std::env::var("BRAVE_API_KEY").ok().filter(|k| !k.is_empty()) {
                        return vec![SearchBackend::Brave { api_key: key }];
                    }
                    tracing::warn!("STILLO_SEARCH_BACKEND=brave だが BRAVE_API_KEY が未設定。デフォルトチェーンにフォールバック");
                }
                other => {
                    tracing::warn!("未知の STILLO_SEARCH_BACKEND={:?}。デフォルトチェーンを使用", other);
                }
            }
        }

        let mut backends = Vec::new();
        if let Some(base_url) = searxng_base_url() {
            backends.push(SearchBackend::Searxng { base_url });
        }
        if let Some(key) = std::env::var("BRAVE_API_KEY").ok().filter(|k| !k.is_empty()) {
            backends.push(SearchBackend::Brave { api_key: key });
        }
        backends.push(SearchBackend::DuckDuckGo);
        backends
    }

    fn name(&self) -> &'static str {
        match self {
            SearchBackend::DuckDuckGo => "duckduckgo",
            SearchBackend::Searxng { .. } => "searxng",
            SearchBackend::Brave { .. } => "brave",
        }
    }
}

/// SEARXNG_URL の末尾スラッシュを正規化して返す。
fn searxng_base_url() -> Option<Url> {
    std::env::var("SEARXNG_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| {
            let normalized = if s.ends_with('/') { s } else { format!("{}/", s) };
            normalized.parse::<Url>().ok()
        })
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("all search backends failed: {0}")]
    AllBlocked(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("invalid query URL")]
    InvalidQuery,
}

/// 検索クエリをバックエンドチェーンで実行し、最初の成功結果を返す。
pub async fn web_search(query: &str) -> Result<Vec<SearchResult>, SearchError> {
    let backends = SearchBackend::from_env();
    let config = HttpConfig {
        timeout_secs: 20,
        ..Default::default()
    };
    let fetcher = HttpFetcher::new(config);

    let mut failures: Vec<String> = Vec::new();
    for backend in &backends {
        match search_with(&fetcher, backend, query).await {
            Ok(results) => {
                // 先行バックエンドがブロックされた後で成功した場合、経路をログに残す
                if !failures.is_empty() {
                    tracing::warn!(
                        "search backend fell back to {} after failures: {:?}",
                        backend.name(),
                        failures
                    );
                }
                return Ok(results);
            }
            Err(e) => {
                tracing::debug!("search backend {} failed: {}", backend.name(), e);
                failures.push(format!("{}: {}", backend.name(), e));
            }
        }
    }

    Err(SearchError::AllBlocked(failures.join("; ")))
}

/// 単一バックエンドで検索を実行する。
///
/// ブロック検出・パース失敗は Err として返し、チェーン側のフォールバック判断に委ねる。
/// 「結果0件」は正当な Ok(vec![]) として返す（ブロックとは区別する）。
async fn search_with(
    fetcher: &HttpFetcher,
    backend: &SearchBackend,
    query: &str,
) -> Result<Vec<SearchResult>, SearchError> {
    match backend {
        SearchBackend::DuckDuckGo => {
            let mut url = Url::parse(DDG_HTML_ENDPOINT).map_err(|_| SearchError::InvalidQuery)?;
            url.query_pairs_mut().append_pair("q", query.trim());
            let raw = fetcher
                .fetch(&url)
                .await
                .map_err(|e| SearchError::Http(e.to_string()))?;
            let html = String::from_utf8_lossy(&raw.bytes);
            if detect_blocked_page(raw.status, &html) {
                return Err(SearchError::Http(format!(
                    "blocked (HTTP {} challenge page)",
                    raw.status
                )));
            }
            let results = parse_ddg_results(&html);
            // 非ブロックだが結果0件 + anomaly の残骸がある場合はブロック扱いにする
            if results.is_empty() && html.contains("anomaly") {
                return Err(SearchError::Http("blocked (empty results with anomaly markers)".into()));
            }
            Ok(results)
        }
        SearchBackend::Searxng { base_url } => {
            let mut url = base_url.join("search").map_err(|_| SearchError::InvalidQuery)?;
            url.query_pairs_mut()
                .append_pair("q", query.trim())
                .append_pair("format", "json");
            let raw = fetcher
                .fetch(&url)
                .await
                .map_err(|e| SearchError::Http(e.to_string()))?;
            let text = String::from_utf8_lossy(&raw.bytes);
            if detect_blocked_page(raw.status, &text) {
                return Err(SearchError::Http(format!(
                    "blocked (HTTP {} challenge page)",
                    raw.status
                )));
            }
            parse_searxng_results(&text)
                .ok_or_else(|| SearchError::Http("invalid SearXNG JSON response".into()))
        }
        SearchBackend::Brave { api_key } => {
            let mut url = Url::parse("https://api.search.brave.com/res/v1/web/search")
                .map_err(|_| SearchError::InvalidQuery)?;
            url.query_pairs_mut().append_pair("q", query.trim());
            let raw = fetcher
                .fetch_with_headers(&url, &[("Accept", "application/json"), ("X-Subscription-Token", api_key)])
                .await
                .map_err(|e| SearchError::Http(e.to_string()))?;
            let text = String::from_utf8_lossy(&raw.bytes);
            if detect_blocked_page(raw.status, &text) {
                return Err(SearchError::Http(format!(
                    "blocked (HTTP {})",
                    raw.status
                )));
            }
            parse_brave_results(&text)
                .ok_or_else(|| SearchError::Http("invalid Brave Search JSON response".into()))
        }
    }
}

/// 検索結果をMarkdown文字列として返す（dump/TUI表示用）。
pub fn results_to_markdown(query: &str, results: &[SearchResult]) -> String {
    render_results_markdown(query, results)
}

/// テスト用にタイムアウトを短縮したフェッチャ構成を作る（未使用時はDurationのみ）。
#[allow(dead_code)]
fn short_timeout() -> Duration {
    Duration::from_secs(5)
}
