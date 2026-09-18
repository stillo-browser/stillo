use std::path::PathBuf;
use chrono::{DateTime, Utc};
use url::Url;
use crate::ast::Document;

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

#[derive(Debug, Clone, PartialEq)]
pub enum DelegationTarget {
    LocalCdp { port: u16 },
    PlaywrightDaemon { socket_path: PathBuf },
    JinaReader { api_key: Option<String> },
    Firecrawl { base_url: Url, api_key: String },
    Unavailable { reason: String },
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
    #[error("TLS error: {0}")]
    Tls(String),
    #[error("Timeout after {seconds}s")]
    Timeout { seconds: u64 },
    #[error("Delegation failed: {0}")]
    DelegationFailed(String),
    #[error("All delegation targets unavailable")]
    NoDelegationAvailable,
}
