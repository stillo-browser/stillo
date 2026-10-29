use crate::ast::Document;
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use url::Url;

#[derive(Debug, Clone)]
pub struct RawHtml {
    pub bytes: Vec<u8>,
    pub url: Url,
    pub content_type: String,
    pub status: u16,
}

#[derive(Debug, Clone)]
pub struct ExtractedContent {
    pub url: Url,
    pub title: String,
    pub byline: Option<String>,
    pub body_text: String,
    pub body_html: String,
    pub links: Vec<ExtractedLink>,
    pub metadata: PageMetadata,
}

#[derive(Debug, Clone)]
pub struct ExtractedLink {
    pub text: String,
    pub href: Url,
    pub rel: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PageMetadata {
    pub description: Option<String>,
    pub og_title: Option<String>,
    pub og_image: Option<String>,
    pub canonical: Option<Url>,
    pub published_at: Option<DateTime<Utc>>,
    pub author: Option<String>,
    pub date_modified: Option<DateTime<Utc>>,
}

/// フォーマット非依存のブラウズ用ページ表現。
/// HTML / RSS / Markdown など各入力から変換して TuiBrowser に渡す。
#[derive(Debug, Clone)]
pub struct BrowsePage {
    pub title: String,
    pub url: Url,
    pub doc: Document,
    pub links: Vec<ExtractedLink>,
    /// TUI の 'd' キー dump 用 Markdown テキスト
    pub markdown: String,
}

#[derive(Debug, Clone)]
pub struct MarkdownDocument {
    pub content: String,
    pub source_url: Url,
    pub extracted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpaDetection {
    Static,
    SuspectedSpa { text_length: usize },
    FrameworkDetected { framework: JsFramework },
}

#[derive(Debug, Clone, PartialEq)]
pub enum JsFramework {
    React,
    Vue,
    Angular,
    Next,
    Nuxt,
    Unknown(String),
}

/// SPA 委譲先。
///
/// `Debug` は手書き実装。derive すると `tracing` の `{:?}` 出力で API キーが平文でログに出るため、
/// キーの値は伏せて有無だけを表示する。
#[derive(Clone, PartialEq)]
pub enum DelegationTarget {
    LocalCdp { port: u16 },
    PlaywrightDaemon { socket_path: PathBuf },
    JinaReader { api_key: Option<String> },
    Firecrawl { base_url: Url, api_key: String },
    Unavailable { reason: String },
}

impl std::fmt::Debug for DelegationTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const REDACTED: &str = "<redacted>";
        match self {
            Self::LocalCdp { port } => f.debug_struct("LocalCdp").field("port", port).finish(),
            Self::PlaywrightDaemon { socket_path } => f
                .debug_struct("PlaywrightDaemon")
                .field("socket_path", socket_path)
                .finish(),
            Self::JinaReader { api_key } => f
                .debug_struct("JinaReader")
                .field("api_key", &api_key.as_ref().map(|_| REDACTED))
                .finish(),
            Self::Firecrawl { base_url, .. } => f
                .debug_struct("Firecrawl")
                .field("base_url", &base_url.as_str())
                .field("api_key", &REDACTED)
                .finish(),
            Self::Unavailable { reason } => f
                .debug_struct("Unavailable")
                .field("reason", reason)
                .finish(),
        }
    }
}

#[derive(Debug)]
pub enum FetchResult {
    Static(RawHtml),
    SpaDelegated {
        detection: SpaDetection,
        target: DelegationTarget,
    },
    DelegatedHtml(RawHtml),
    Failed(FetchError),
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("HTTP error: {status} {url}")]
    Http { status: u16, url: Url },
    /// DNS 解決失敗・接続拒否・TLS ハンドシェイク失敗など、レスポンスを受け取る前の接続段階の失敗
    #[error("Connection failed: {0}")]
    Connect(String),
    #[error("Redirect error: {0}")]
    Redirect(String),
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
    /// 接続後の送受信やボディ読み取りの失敗など、上記に分類できないもの
    #[error("Request failed: {0}")]
    Request(String),
    #[error("Timeout after {seconds}s")]
    Timeout { seconds: u64 },
    /// プライベートネットワーク拒否設定により、接続先 IP やスキームを拒否した
    #[error("Blocked: {url}: {reason}")]
    Blocked { url: Url, reason: String },
    #[error("Response body exceeds limit of {limit} bytes")]
    BodyTooLarge { limit: usize },
    #[error("Delegation failed: {0}")]
    DelegationFailed(String),
    #[error("All delegation targets unavailable")]
    NoDelegationAvailable,
    /// 委譲チェーンの全ターゲットが失敗した。各要素は「ターゲット名: 失敗理由」
    #[error("All delegation targets failed: {}", .0.join("; "))]
    DelegationChainFailed(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegation_target_debug_redacts_api_keys() {
        let jina = DelegationTarget::JinaReader {
            api_key: Some("jina_secret_value".into()),
        };
        let firecrawl = DelegationTarget::Firecrawl {
            base_url: Url::parse("https://api.firecrawl.dev").unwrap(),
            api_key: "fc_secret_value".into(),
        };
        let jina_dbg = format!("{:?}", jina);
        let fc_dbg = format!("{:?}", firecrawl);
        assert!(!jina_dbg.contains("jina_secret_value"), "{jina_dbg}");
        assert!(!fc_dbg.contains("fc_secret_value"), "{fc_dbg}");
        assert!(jina_dbg.contains("<redacted>"));
        assert!(fc_dbg.contains("api.firecrawl.dev"));
    }
}
