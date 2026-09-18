mod args;

use anyhow::{Context, Result};
use clap::Parser;
use args::{Cli, Command, DelegateTarget, OutputFormat};
use stillo_core::{
    document::{DelegationTarget, SpaDetection},
    ContentExtractor, ExtractorConfig, MarkdownConfig, MarkdownSerializer,
};
use stillo_fetcher::{HttpConfig, HttpFetcher, SpaDelegationChain};
use stillo_renderer::{TuiBrowser, TuiResult};
use stillo_llm::{LlmProvider, CompletionConfig, prompts};
use stillo_mcp::McpServer;
use url::Url;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = if cli.verbose { "debug" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level)),
        )
        .with_writer(std::io::stderr)
        .init();

    match cli.command {
        Some(Command::Dump { url, format, delegate, no_delegate }) => {
            let fmt = format.unwrap_or(cli.format);
            let del = delegate.or(cli.delegate);
            let no_del = no_delegate || cli.no_delegate;
            dump(&url, &fmt, cli.timeout, del.as_ref(), no_del).await?;
        }
        Some(Command::Browse { url }) => {
            let del = cli.delegate.clone();
            let no_del = cli.no_delegate;
            browse(&url, cli.timeout, del.as_ref(), no_del).await?;
        }
        Some(Command::Qa { question, url }) => {
            let del = cli.delegate.clone();
            let no_del = cli.no_delegate;
            qa(&question, &url, cli.timeout, del.as_ref(), no_del).await?;
        }
        Some(Command::Summarize { url }) => {
            let del = cli.delegate.clone();
            let no_del = cli.no_delegate;
            summarize(&url, cli.timeout, del.as_ref(), no_del).await?;
        }
        Some(Command::Extract { fields, url, format }) => {
            let fmt = format.unwrap_or(cli.format);
            let del = cli.delegate.clone();
            let no_del = cli.no_delegate;
            extract_fields(&fields, &url, &fmt, cli.timeout, del.as_ref(), no_del).await?;
        }
        Some(Command::Search { query }) => {
            let q = query.join(" ");
            let page = run_web_search(&q).await?;
            browse_page(page, cli.timeout, cli.delegate.as_ref(), cli.no_delegate).await?;
        }
        Some(Command::Mcp) => {
            McpServer::new().run_stdio().await?;
        }
        None => {
            if let Some(url) = cli.url {
                browse(&url, cli.timeout, cli.delegate.as_ref(), cli.no_delegate).await?;
            } else {
                use clap::CommandFactory;
                Cli::command().print_help()?;
            }
        }
    }

    Ok(())
}

async fn browse(
    start_url: &Url,
    timeout: u64,
    delegate: Option<&DelegateTarget>,
    no_delegate: bool,
) -> Result<()> {
    let raw = fetch_raw(start_url, timeout, delegate, no_delegate).await?;
    let page = raw_to_browse_page(raw).map_err(|e| anyhow::anyhow!("{}", e))?;
    browse_page(page, timeout, delegate, no_delegate).await
}

/// 取得済みの BrowsePage からTUIループを開始する。
/// Web検索結果など「URLフェッチ以外から得たページ」を扱うための入口。
async fn browse_page(
    page: stillo_core::BrowsePage,
    timeout: u64,
    delegate: Option<&DelegateTarget>,
    no_delegate: bool,
) -> Result<()> {
    let mut browser = TuiBrowser::new(page);

    loop {
        match browser.run()? {
            TuiResult::Navigate(next_url) => {
                let raw = fetch_raw(&next_url, timeout, delegate, no_delegate).await?;
                let page = raw_to_browse_page(raw).map_err(|e| anyhow::anyhow!("{}", e))?;
                browser.load_page(page);
            }
            TuiResult::Reload => {
                let reload_url = browser.current_url().clone();
                let raw = fetch_raw(&reload_url, timeout, delegate, no_delegate).await?;
                let page = raw_to_browse_page(raw).map_err(|e| anyhow::anyhow!("{}", e))?;
                browser.reload_page(page);
            }
            TuiResult::WebSearch(query) => {
                // 検索バックエンドの全滅は TUI を殺さずに報知して続行する
                match run_web_search(&query).await {
                    Ok(page) => browser.load_page(page),
                    Err(e) => eprintln!("search failed: {}", e),
                }
            }
            TuiResult::Dump => {
                print!("{}", browser.markdown());
                break;
            }
            TuiResult::Quit => break,
        }
    }
    Ok(())
}

/// Web検索を実行し、結果を BrowsePage に変換する。
/// バックエンドチェーン・ブロック検出は stillo-fetcher::search が担う。
async fn run_web_search(query: &str) -> Result<stillo_core::BrowsePage> {
    let results = stillo_fetcher::web_search(query)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    let md = stillo_fetcher::results_to_markdown(query, &results);
    let url = Url::parse(&format!("stillo://search?q={}", url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()))?;
    Ok(stillo_core::parse_markdown_to_ast(&md, &url))
}

/// RawHtml を Content-Type とボディの内容に応じてフォーマット判定し BrowsePage に変換する。
/// フォーマット判定をフェッチ後に行うことで、レスポンスの実態に基づいた処理ができる。
fn raw_to_browse_page(raw: stillo_core::document::RawHtml) -> Result<stillo_core::BrowsePage, stillo_core::ExtractionError> {
    let ct = raw.content_type.to_lowercase();

    let is_feed = ct.contains("rss") || ct.contains("atom")
        || (ct.contains("xml") && looks_like_feed(&raw.bytes));
    let is_markdown = ct.contains("markdown") || ct.contains("text/plain");

    if is_feed {
        let text = String::from_utf8_lossy(&raw.bytes);
        if let Some(page) = stillo_core::parse_rss_to_ast(&text, &raw.url) {
            return Ok(page);
        }
        // フィードと判定したがパース失敗の場合は HTML として処理するためフォールスルーする
    }

    if is_markdown {
        let text = String::from_utf8_lossy(&raw.bytes);
        return Ok(stillo_core::parse_markdown_to_ast(&text, &raw.url));
    }

    // HTML パス（デフォルト）
    let extractor = ContentExtractor::new(ExtractorConfig::default());
    let content = extractor.extract(&raw)?;
    let doc = stillo_core::parse_html_to_ast(&content.body_html, &content.url);
    let serializer = MarkdownSerializer::new(MarkdownConfig::default());
    let md_doc = serializer.serialize(&content);
    Ok(stillo_core::BrowsePage {
        title: content.title,
        url: content.url,
        doc,
        links: content.links,
        markdown: md_doc.content,
    })
}

/// XML のバイト列先頭を見て RSS / Atom / RSS 1.0(RDF) らしいかを判定する。
/// Content-Type だけでは application/xml と返すサーバがあるため本文も確認する。
fn looks_like_feed(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    let s = String::from_utf8_lossy(head).to_lowercase();
    s.contains("<rss") || s.contains("<feed") || s.contains("xmlns=\"http://purl.org/rss/")
}

async fn fetch_raw(
    url: &Url,
    timeout: u64,
    delegate: Option<&DelegateTarget>,
    no_delegate: bool,
) -> Result<stillo_core::document::RawHtml> {
    let http_config = HttpConfig {
        timeout_secs: timeout,
        ..Default::default()
    };
    let fetcher = HttpFetcher::new(http_config);
    let extractor = ContentExtractor::new(ExtractorConfig::default());

    tracing::debug!("fetching {}", url);
    let raw = fetcher.fetch(url).await.with_context(|| format!("failed to fetch {}", url))?;
    tracing::debug!("fetched {} bytes (status={})", raw.bytes.len(), raw.status);

    // frameset ページの場合、コンテンツが最も多いフレームを取得する
    let raw = {
        let frames = extractor.detect_frames(&raw);
        if frames.is_empty() {
            raw
        } else {
            tracing::debug!("frameset detected ({} frames), fetching frame contents", frames.len());
            fetch_richest_frame(&fetcher, &extractor, frames).await.unwrap_or(raw)
        }
    };

    if no_delegate {
        return Ok(raw);
    }

    let detection = extractor
        .detect_spa_for(&raw)
        .with_context(|| "SPA detection failed")?;

    match &detection {
        SpaDetection::Static => Ok(raw),
        SpaDetection::SuspectedSpa { text_length } => {
            tracing::warn!("SPA suspected (text_length={}), trying delegation", text_length);
            delegate_or_fallback(url, raw, delegate, timeout).await
        }
        SpaDetection::FrameworkDetected { framework } => {
            tracing::warn!("JS framework detected ({:?}), trying delegation", framework);
            delegate_or_fallback(url, raw, delegate, timeout).await
        }
    }
}

/// 複数フレームを順次取得してテキスト量が最多のものを返す。
/// URL に menu/nav/sidebar を含むフレームは低スコアとして扱う。
async fn fetch_richest_frame(
    fetcher: &HttpFetcher,
    extractor: &ContentExtractor,
    frames: Vec<Url>,
) -> Option<stillo_core::document::RawHtml> {
    let mut best: Option<(stillo_core::document::RawHtml, i64)> = None;

    for url in frames {
        let Ok(raw) = fetcher.fetch(&url).await else { continue };
        // ネストしたフレームセットはスキップ
        if !extractor.detect_frames(&raw).is_empty() {
            continue;
        }
        let url_str = raw.url.as_str().to_lowercase();
        let nav_penalty: i64 = if url_str.contains("menu")
            || url_str.contains("nav")
            || url_str.contains("sidebar")
        {
            -100_000
        } else {
            0
        };
        let score = raw.bytes.len() as i64 + nav_penalty;
        if best.as_ref().is_none_or(|(_, s)| score > *s) {
            best = Some((raw, score));
        }
    }

    best.map(|(raw, _)| raw)
}

async fn dump(
    url: &Url,
    format: &OutputFormat,
    timeout: u64,
    delegate: Option<&DelegateTarget>,
    no_delegate: bool,
) -> Result<()> {
    let raw = fetch_raw(url, timeout, delegate, no_delegate).await?;
    let page = raw_to_browse_page(raw).with_context(|| "failed to extract content")?;

    match format {
        OutputFormat::Markdown => {
            print!("{}", page.markdown);
        }
        OutputFormat::Plain => {
            println!("# {}", page.title);
            println!();
            // Document からプレーンテキストを抽出する
            use stillo_core::Block;
            for block in &page.doc.blocks {
                match block {
                    Block::Heading { inlines, .. } => {
                        let text: String = inlines.iter().map(inline_to_plain).collect();
                        println!("{}\n", text);
                    }
                    Block::Paragraph(inlines) => {
                        let text: String = inlines.iter().map(inline_to_plain).collect();
                        println!("{}\n", text);
                    }
                    Block::ListItem { inlines, .. } => {
                        let text: String = inlines.iter().map(inline_to_plain).collect();
                        println!("- {}", text);
                    }
                    Block::CodeBlock { content, .. } => {
                        println!("{}\n", content);
                    }
                    Block::Blockquote(inlines) => {
                        let text: String = inlines.iter().map(inline_to_plain).collect();
                        println!("> {}\n", text);
                    }
                    Block::Rule => println!("---"),
                }
            }
        }
        OutputFormat::Json => {
            let json = serde_json::json!({
                "url": page.url.as_str(),
                "title": page.title,
                "links": page.links.iter().map(|l| serde_json::json!({
                    "text": l.text,
                    "href": l.href.as_str(),
                    "rel": l.rel,
                })).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&json)?);
        }
    }

    Ok(())
}

fn inline_to_plain(inline: &stillo_core::Inline) -> String {
    use stillo_core::Inline;
    match inline {
        Inline::Text(s) | Inline::Bold(s) | Inline::Italic(s)
        | Inline::BoldItalic(s) | Inline::Code(s) => s.clone(),
        Inline::Link { text, .. } => text.clone(),
        Inline::SoftBreak => "\n".to_owned(),
    }
}

async fn qa(
    question: &str,
    url: &Url,
    timeout: u64,
    delegate: Option<&DelegateTarget>,
    no_delegate: bool,
) -> Result<()> {
    let doc = fetch_as_markdown(url, timeout, delegate, no_delegate).await?;
    let llm = LlmProvider::from_env().context("LLM provider not configured")?;
    let messages = prompts::qa_prompt(question, &doc);
    let answer = llm.complete(messages, &CompletionConfig::default()).await
        .context("LLM request failed")?;
    println!("{}", answer);
    Ok(())
}

async fn summarize(
    url: &Url,
    timeout: u64,
    delegate: Option<&DelegateTarget>,
    no_delegate: bool,
) -> Result<()> {
    let doc = fetch_as_markdown(url, timeout, delegate, no_delegate).await?;
    let llm = LlmProvider::from_env().context("LLM provider not configured")?;
    let messages = prompts::summarize_prompt(&doc);
    let summary = llm.complete(messages, &CompletionConfig::default()).await
        .context("LLM request failed")?;
    println!("{}", summary);
    Ok(())
}

async fn extract_fields(
    fields: &str,
    url: &Url,
    format: &OutputFormat,
    timeout: u64,
    delegate: Option<&DelegateTarget>,
    no_delegate: bool,
) -> Result<()> {
    let doc = fetch_as_markdown(url, timeout, delegate, no_delegate).await?;
    let llm = LlmProvider::from_env().context("LLM provider not configured")?;
    let config = CompletionConfig { temperature: 0.0, ..Default::default() };
    let messages = prompts::extract_prompt(fields, &doc);
    let result = llm.complete(messages, &config).await
        .context("LLM request failed")?;

    // JSON 出力形式の場合は JSON として再フォーマットする
    if matches!(format, OutputFormat::Json) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&result) {
            println!("{}", serde_json::to_string_pretty(&v)?);
            return Ok(());
        }
    }
    println!("{}", result);
    Ok(())
}

/// URL を取得して MarkdownDocument に変換する共通ヘルパー
async fn fetch_as_markdown(
    url: &Url,
    timeout: u64,
    delegate: Option<&DelegateTarget>,
    no_delegate: bool,
) -> Result<stillo_core::document::MarkdownDocument> {
    let extractor = ContentExtractor::new(ExtractorConfig::default());
    let raw = fetch_raw(url, timeout, delegate, no_delegate).await?;
    let content = extractor.extract(&raw).context("failed to extract content")?;
    let serializer = MarkdownSerializer::new(MarkdownConfig::default());
    Ok(serializer.serialize(&content))
}

/// SPA委譲を試み、全ターゲット失敗時は静的HTMLにフォールバックする
async fn delegate_or_fallback(
    url: &Url,
    static_raw: stillo_core::document::RawHtml,
    delegate: Option<&DelegateTarget>,
    _timeout: u64,
) -> Result<stillo_core::document::RawHtml> {
    let chain = build_delegation_chain(delegate);

    match chain.fetch_with_js(url).await {
        Ok(delegated) => Ok(delegated),
        Err(e) => {
            tracing::warn!(
                "all delegation targets failed ({}), falling back to static HTML",
                e
            );
            Ok(static_raw)
        }
    }
}

fn build_delegation_chain(delegate: Option<&DelegateTarget>) -> SpaDelegationChain {
    match delegate {
        None | Some(DelegateTarget::Auto) => SpaDelegationChain::from_env(9222),
        Some(DelegateTarget::Cdp) => SpaDelegationChain::with_single_target(
            DelegationTarget::LocalCdp { port: 9222 },
        ),
        Some(DelegateTarget::Playwright) => SpaDelegationChain::with_single_target(
            DelegationTarget::PlaywrightDaemon {
                socket_path: "/tmp/stillo-playwright.sock".into(),
            },
        ),
        Some(DelegateTarget::Jina) => SpaDelegationChain::with_single_target(
            DelegationTarget::JinaReader {
                api_key: std::env::var("JINA_API_KEY").ok(),
            },
        ),
        Some(DelegateTarget::Firecrawl) => {
            let base_url = std::env::var("FIRECRAWL_URL")
                .ok()
                .and_then(|u| u.parse().ok())
                .unwrap_or_else(|| "https://api.firecrawl.dev/".parse().unwrap());
            let api_key = std::env::var("FIRECRAWL_API_KEY").unwrap_or_default();
            SpaDelegationChain::with_single_target(DelegationTarget::Firecrawl {
                base_url,
                api_key,
            })
        }
    }
}
