mod args;

use anyhow::{Context, Result};
use args::{Cli, Command, DelegateTarget, OutputFormat};
use clap::Parser;
use std::time::Duration;
use stillo_core::{
    document::{DelegationTarget, SpaDetection},
    ContentExtractor, ExtractorConfig, MarkdownConfig, MarkdownSerializer,
};
use stillo_fetcher::{HttpConfig, HttpFetcher, SpaDelegationChain};
use stillo_llm::{prompts, CompletionConfig, LlmError, LlmProvider};
use stillo_mcp::McpServer;
use stillo_renderer::{TuiBrowser, TuiResult, TuiSession};
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
        Some(Command::Dump {
            url,
            format,
            delegate,
            no_delegate,
        }) => {
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
        Some(Command::Extract {
            fields,
            url,
            format,
        }) => {
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
    // 遷移のたびに alternate screen を抜けるとフェッチ中に元のシェル画面のまま固まって見えるため、
    // 端末セッションはブラウズ全体で 1 つ保持し、フェッチ中も TUI 上に読み込み中表示を出す
    let mut session = TuiSession::start()?;

    let dump = loop {
        match browser.run(&mut session)? {
            // 遷移・リロード・検索の失敗で TUI ごと終了すると閲覧中のページと履歴を失うため、
            // エラーはヒントバーに表示してブラウズを続行する
            TuiResult::Navigate(next_url) => {
                let fetch = fetch_browse_page(&next_url, timeout, delegate, no_delegate);
                match load_cancellable(&mut browser, &mut session, &next_url, fetch).await? {
                    Some(Ok(page)) => browser.load_page(page),
                    Some(Err(e)) => browser.show_error(format!("{}: {:#}", next_url, e)),
                    None => browser.show_error(format!("cancelled: {}", next_url)),
                }
            }
            TuiResult::Reload => {
                let reload_url = browser.current_url().clone();
                // 検索結果ページは stillo:// スキームで HTTP 取得できないため、同じクエリで再検索する
                let result = match search_query_from_url(&reload_url) {
                    Some(query) => {
                        let search = run_web_search(&query);
                        load_cancellable(&mut browser, &mut session, &reload_url, search).await?
                    }
                    None => {
                        let fetch = fetch_browse_page(&reload_url, timeout, delegate, no_delegate);
                        load_cancellable(&mut browser, &mut session, &reload_url, fetch).await?
                    }
                };
                match result {
                    Some(Ok(page)) => browser.reload_page(page),
                    Some(Err(e)) => browser.show_error(format!("reload failed: {:#}", e)),
                    None => browser.show_error(format!("cancelled: {}", reload_url)),
                }
            }
            TuiResult::WebSearch(query) => {
                let search = run_web_search(&query);
                match load_cancellable(&mut browser, &mut session, &query, search).await? {
                    Some(Ok(page)) => browser.load_page(page),
                    Some(Err(e)) => browser.show_error(format!("search failed: {:#}", e)),
                    None => browser.show_error(format!("cancelled: {}", query)),
                }
            }
            TuiResult::Dump => break true,
            TuiResult::Quit => break false,
        }
    };
    // Markdown は元の画面（またはリダイレクト先）に出すため、alternate screen を抜けてから書く
    session.end();
    if dump {
        print!("{}", browser.markdown());
    }
    Ok(())
}

/// 読み込み中表示を出したまま `fut` を待つ。Esc / Ctrl-C でキャンセルされたら None を返す。
/// キー入力はブロッキング読み取りできないため、一定間隔で非ブロッキングにポーリングする。
/// キャンセル時は Future を drop してフェッチを中断する。
async fn load_cancellable<T>(
    browser: &mut TuiBrowser,
    session: &mut TuiSession,
    target: impl std::fmt::Display,
    fut: impl std::future::Future<Output = T>,
) -> Result<Option<T>> {
    browser.show_loading(session, format!("Loading {}...", target))?;
    tokio::pin!(fut);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
    loop {
        tokio::select! {
            out = &mut fut => return Ok(Some(out)),
            _ = tick.tick() => {
                if browser.poll_cancel(session)? {
                    return Ok(None);
                }
            }
        }
    }
}

/// Web検索結果ページの URL。リロード時に search_query_from_url でクエリを取り出せる形にする。
fn search_page_url(query: &str) -> Result<Url> {
    Ok(Url::parse(&format!(
        "stillo://search?q={}",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    ))?)
}

/// `stillo://search?q=...` から検索クエリを取り出す。検索結果ページ以外は None。
fn search_query_from_url(url: &Url) -> Option<String> {
    if url.scheme() != "stillo" || url.host_str() != Some("search") {
        return None;
    }
    url.query_pairs()
        .find(|(k, _)| k == "q")
        .map(|(_, v)| v.into_owned())
}

/// URL を取得して BrowsePage に変換する。TUI のナビゲーションループ用。
async fn fetch_browse_page(
    url: &Url,
    timeout: u64,
    delegate: Option<&DelegateTarget>,
    no_delegate: bool,
) -> Result<stillo_core::BrowsePage> {
    let raw = fetch_raw(url, timeout, delegate, no_delegate).await?;
    raw_to_browse_page(raw).map_err(|e| anyhow::anyhow!("{}", e))
}

/// Web検索を実行し、結果を BrowsePage に変換する。
/// バックエンドチェーン・ブロック検出は stillo-fetcher::search が担う。
async fn run_web_search(query: &str) -> Result<stillo_core::BrowsePage> {
    let results = stillo_fetcher::web_search(query)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    let md = stillo_fetcher::results_to_markdown(query, &results);
    let url = search_page_url(query)?;
    Ok(stillo_core::parse_markdown_to_ast(&md, &url))
}

/// RawHtml を Content-Type とボディの内容に応じてフォーマット判定し BrowsePage に変換する。
/// フォーマット判定をフェッチ後に行うことで、レスポンスの実態に基づいた処理ができる。
fn raw_to_browse_page(
    raw: stillo_core::document::RawHtml,
) -> Result<stillo_core::BrowsePage, stillo_core::ExtractionError> {
    let ct = raw.content_type.to_lowercase();

    let is_feed = ct.contains("rss")
        || ct.contains("atom")
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
    let md_doc = serializer.serialize(&content, chrono::Utc::now());
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
    let raw = fetcher
        .fetch(url)
        .await
        .with_context(|| format!("failed to fetch {}", url))?;
    tracing::debug!("fetched {} bytes (status={})", raw.bytes.len(), raw.status);

    // frameset ページの場合、コンテンツが最も多いフレームを取得する
    let raw = {
        let frames = extractor.detect_frames(&raw);
        if frames.is_empty() {
            raw
        } else {
            tracing::debug!(
                "frameset detected ({} frames), fetching frame contents",
                frames.len()
            );
            fetch_richest_frame(&fetcher, &extractor, frames)
                .await
                .unwrap_or(raw)
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
            tracing::warn!(
                "SPA suspected (text_length={}), trying delegation",
                text_length
            );
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
        let Ok(raw) = fetcher.fetch(&url).await else {
            continue;
        };
        // ネストしたフレームセットはスキップ
        if !extractor.detect_frames(&raw).is_empty() {
            continue;
        }
        let url_str = raw.url.as_str().to_lowercase();
        let nav_penalty: i64 =
            if url_str.contains("menu") || url_str.contains("nav") || url_str.contains("sidebar") {
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
    // ページ由来の文字列を stdout に書くため、端末エスケープシーケンスになる制御文字を除去する
    let page = raw_to_browse_page(raw)
        .with_context(|| "failed to extract content")?
        .sanitize_for_terminal();

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
        Inline::Text(s)
        | Inline::Bold(s)
        | Inline::Italic(s)
        | Inline::BoldItalic(s)
        | Inline::Code(s) => s.clone(),
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
    let answer = accept_truncated(llm.complete(messages, &CompletionConfig::default()).await)
        .context("LLM request failed")?;
    // LLM の回答はページ内容を含みうるため、制御文字を除去してから出力する
    println!("{}", stillo_core::strip_control_chars(&answer));
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
    let summary = accept_truncated(llm.complete(messages, &CompletionConfig::default()).await)
        .context("LLM request failed")?;
    println!("{}", stillo_core::strip_control_chars(&summary));
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
    let messages = prompts::extract_prompt(fields, &doc);
    // 抽出結果は途中で切れると JSON として壊れるため、qa/summarize と違い Truncated はエラーのまま扱う
    let result = llm
        .complete(messages, &CompletionConfig::extraction())
        .await
        .context("LLM request failed")?;

    // JSON 出力形式の場合は JSON として再フォーマットする
    if matches!(format, OutputFormat::Json) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&result) {
            println!("{}", serde_json::to_string_pretty(&v)?);
            return Ok(());
        }
    }
    println!("{}", stillo_core::strip_control_chars(&result));
    Ok(())
}

/// max_tokens 到達で打ち切られた自然文の応答を、警告付きで部分出力として受け入れる。
/// qa/summarize では途中までの文章でも利用価値があり、エラーにすると何も表示されなくなるため
fn accept_truncated(result: Result<String, LlmError>) -> Result<String, LlmError> {
    match result {
        Err(LlmError::Truncated {
            max_tokens,
            partial,
        }) => {
            eprintln!(
                "warning: LLM output was truncated at max_tokens={}",
                max_tokens
            );
            Ok(partial)
        }
        other => other,
    }
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
    let content = extractor
        .extract(&raw)
        .context("failed to extract content")?;
    let serializer = MarkdownSerializer::new(MarkdownConfig::default());
    Ok(serializer.serialize(&content, chrono::Utc::now()))
}

/// SPA委譲を試み、全ターゲット失敗時は静的HTMLにフォールバックする
async fn delegate_or_fallback(
    url: &Url,
    static_raw: stillo_core::document::RawHtml,
    delegate: Option<&DelegateTarget>,
    timeout: u64,
) -> Result<stillo_core::document::RawHtml> {
    // 静的取得と同じ --timeout を委譲チェーン全体の期限にする。CDP の読み込み待ち等が
    // 積み重なって指定時間を大きく超えないようにするため
    let chain = build_delegation_chain(delegate).with_timeout(Duration::from_secs(timeout));

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
        Some(DelegateTarget::Cdp) => {
            SpaDelegationChain::with_single_target(DelegationTarget::LocalCdp { port: 9222 })
        }
        Some(DelegateTarget::Playwright) => {
            // 明示指定でも所有者を確認できないソケットには接続しない（他ユーザーのなりすまし対策）
            let target = match stillo_fetcher::spa::playwright::discover_socket() {
                Some(socket_path) => DelegationTarget::PlaywrightDaemon { socket_path },
                None => DelegationTarget::Unavailable {
                    reason: "no Playwright daemon socket owned by the current user".into(),
                },
            };
            SpaDelegationChain::with_single_target(target)
        }
        Some(DelegateTarget::Jina) => {
            SpaDelegationChain::with_single_target(DelegationTarget::JinaReader {
                api_key: std::env::var("JINA_API_KEY").ok(),
            })
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 検索結果ページの URL からクエリを復元でき、リロードで再検索できること
    #[test]
    fn search_query_roundtrips_through_search_page_url() {
        for q in ["rust tui", "日本語 検索", "a+b&c=d/e?"] {
            let url = search_page_url(q).unwrap();
            assert_eq!(search_query_from_url(&url).as_deref(), Some(q));
        }
    }

    #[test]
    fn non_search_urls_have_no_query() {
        let http = Url::parse("https://example.com/search?q=x").unwrap();
        assert_eq!(search_query_from_url(&http), None);
        let other = Url::parse("stillo://other?q=x").unwrap();
        assert_eq!(search_query_from_url(&other), None);
    }
}
