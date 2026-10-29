//! Web検索結果のパースとボットブロック検出（純粋関数）。
//!
//! I/O（HTTP取得・バックエンド選択）は `stillo-fetcher` が担い、
//! ここではレスポンス本体だけを受け取る。ブロックページと「結果0件」を
//! 区別することで、呼び出し側がブロックを空結果として誤認するのを防ぐ。

use markup5ever_rcdom::{Handle, NodeData};
use std::collections::HashMap;
use url::Url;

/// Web検索の1ヒット
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub title: String,
    pub url: Url,
    pub snippet: String,
    pub display_url: String,
}

/// HTTPレスポンスが検索結果ではなくボットブロック/チャレンジページかを判定する。
///
/// 「ブロックされた」を「結果0件」と区別するのが目的。
/// DDG の anomaly ページ（HTTP 202 + challenge-form）、Cloudflare /
/// Anubis 系のチャレンジ画面をマーカーで検出する。
pub fn detect_blocked_page(status: u16, html: &str) -> bool {
    // 202 は検索レスポンスとして異常（DDG が anomaly 時に返す）。
    // 403/429 は明示的な拒否・レート制限。
    if matches!(status, 202 | 403 | 429) {
        return true;
    }
    // DDG anomaly ページ固有のマーカー
    if html.contains("id=\"anomaly-modal\"") || html.contains("id=\"challenge-form\"") {
        return true;
    }
    // Anubis（"Making sure you're not a bot"）/ Cloudflare / 汎用キャプチャ
    if html.contains("Making sure you&#39;re not a bot")
        || html.contains("Making sure you're not a bot")
        || html.contains("<title>Just a moment")
        || html.contains("<title>Captcha</title>")
    {
        return true;
    }
    false
}

/// DDG の HTML をパースできなかったことを表すエラー。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DdgParseError {
    /// 結果ブロック（class に `result` を含む要素）はあるのに 1 件も取り出せなかった。
    /// DDG のマークアップ変更でパーサーが追随できていない可能性が高く、「0 件」とは区別する
    #[error("DuckDuckGo の結果ブロック {blocks} 件から検索結果を取り出せなかった（HTML 構造の変更の可能性）")]
    UnrecognizedResults { blocks: usize },
}

/// DuckDuckGo HTML エンドポイント（html.duckduckgo.com/html/）の結果をパースする。
///
/// パースできなかった場合も空 Vec を返す。「0 件」とパース失敗を区別したい場合は
/// [`parse_ddg_results_checked`] を使う。
pub fn parse_ddg_results(html: &str) -> Vec<SearchResult> {
    parse_ddg_results_checked(html).unwrap_or_default()
}

/// DuckDuckGo HTML エンドポイントの結果をパースし、パース失敗を「0 件」と区別して返す。
///
/// result__a / result__url / result__snippet の各 `<a>` を uddg= キーで
/// グループ化し、リダイレクトを解決した実URL付きの構造化データを返す。
/// 結果ブロックがあるのに 1 件も取り出せない場合は [`DdgParseError::UnrecognizedResults`]。
pub fn parse_ddg_results_checked(html: &str) -> Result<Vec<SearchResult>, DdgParseError> {
    let dom = crate::extractor::parse_html(html);
    let results = group_ddg_results(&dom.document);
    if results.is_empty() {
        let blocks = count_result_blocks(&dom.document);
        if blocks > 0 {
            return Err(DdgParseError::UnrecognizedResults { blocks });
        }
    }
    Ok(results)
}

fn group_ddg_results(root: &Handle) -> Vec<SearchResult> {
    // (class, href, text) の形式で全対象リンクを収集
    let mut raw: Vec<(String, String, String)> = Vec::new();
    collect_result_links(root, &mut raw);

    // DDGリダイレクトURLの uddg= パラメータをキーにグループ化。
    // order は挿入順を保持するためのキーリスト。
    let mut order: Vec<String> = Vec::new();
    #[derive(Default)]
    struct Group {
        title: Option<String>,
        display_url: Option<String>,
        snippet: Option<String>,
        real_url: Option<Url>,
    }
    let mut groups: HashMap<String, Group> = HashMap::new();

    for (class, href, text) in raw {
        let normalized = if href.starts_with("//") {
            format!("https:{}", href)
        } else {
            href.clone()
        };

        let parsed = match Url::parse(&normalized) {
            Ok(u) => u,
            Err(_) => continue,
        };

        if parsed.host_str() != Some("duckduckgo.com") {
            continue;
        }

        let uddg = match parsed
            .query_pairs()
            .find(|(k, _)| k == "uddg")
            .map(|(_, v)| v.into_owned())
        {
            Some(u) => u,
            None => continue,
        };

        let real_url = uddg.parse::<Url>().ok();

        if !groups.contains_key(&uddg) {
            order.push(uddg.clone());
            groups.insert(uddg.clone(), Group::default());
        }

        let g = groups.get_mut(&uddg).unwrap();
        if real_url.is_some() {
            g.real_url = real_url;
        }

        match class.as_str() {
            "result__a" => g.title = Some(text.trim().to_owned()),
            "result__url" => g.display_url = Some(text.trim().to_owned()),
            "result__snippet" => g.snippet = Some(text.trim().to_owned()),
            _ => {}
        }
    }

    order
        .into_iter()
        .filter_map(|key| {
            let g = groups.remove(&key)?;
            Some(SearchResult {
                title: g.title?,
                url: g.real_url?,
                snippet: g.snippet.unwrap_or_default(),
                display_url: g.display_url.unwrap_or_default(),
            })
        })
        .collect()
}

/// SearXNG の JSON API（/search?format=json）レスポンスをパースする。
/// JSON として不正・results キー欠如なら None（バックエンド失敗として扱う）。
pub fn parse_searxng_results(json_text: &str) -> Option<Vec<SearchResult>> {
    let v: serde_json::Value = serde_json::from_str(json_text).ok()?;
    let arr = v.get("results").and_then(|r| r.as_array())?.clone();
    let out: Vec<SearchResult> = arr
        .iter()
        .filter_map(|r| {
            let title = r.get("title").and_then(|t| t.as_str())?.trim().to_string();
            if title.is_empty() {
                return None;
            }
            let url: Url = r.get("url").and_then(|u| u.as_str())?.parse().ok()?;
            let snippet = r
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let display_url = url.host_str().unwrap_or("").to_string();
            Some(SearchResult {
                title,
                url,
                snippet,
                display_url,
            })
        })
        .collect();
    Some(out)
}

/// SearXNG レスポンスの `unresponsive_engines`（応答しなかったエンジン名）を返す。
///
/// 要素は `["google", "timeout"]` のような [エンジン名, 理由] の配列。
/// 上流エンジンが全滅した場合も SearXNG は `results: []` を返すため、
/// 「結果0件」とブロック・障害を区別する材料として使う。
pub fn searxng_unresponsive_engines(json_text: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json_text) else {
        return Vec::new();
    };
    v.get("unresponsive_engines")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| match e {
                    serde_json::Value::Array(pair) => {
                        let parts: Vec<&str> = pair.iter().filter_map(|p| p.as_str()).collect();
                        (!parts.is_empty()).then(|| parts.join(": "))
                    }
                    serde_json::Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Brave Search API（/res/v1/web/search）レスポンスをパースする。
pub fn parse_brave_results(json_text: &str) -> Option<Vec<SearchResult>> {
    let v: serde_json::Value = serde_json::from_str(json_text).ok()?;
    let arr = v
        .pointer("/web/results")
        .and_then(|r| r.as_array())?
        .clone();
    let out: Vec<SearchResult> = arr
        .iter()
        .filter_map(|r| {
            let title = r.get("title").and_then(|t| t.as_str())?.trim().to_string();
            if title.is_empty() {
                return None;
            }
            let url: Url = r.get("url").and_then(|u| u.as_str())?.parse().ok()?;
            let snippet = r
                .get("description")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let display_url = url.host_str().unwrap_or("").to_string();
            Some(SearchResult {
                title,
                url,
                snippet,
                display_url,
            })
        })
        .collect();
    Some(out)
}

/// 検索結果をMarkdownに整形する（MCP markdown形式・TUI表示共用）。
pub fn render_results_markdown(query: &str, results: &[SearchResult]) -> String {
    let mut md = format!("# Search: {}\n\n", query);
    for (i, r) in results.iter().enumerate() {
        md.push_str(&format!("## {}. {}\n", i + 1, r.title));
        md.push_str(&format!("*{}*\n\n", r.display_url));
        if !r.snippet.is_empty() {
            md.push_str(&format!("{}\n\n", r.snippet));
        }
        md.push_str(&format!("<{}>\n\n---\n\n", r.url));
    }
    md
}

/// `duckduckgo.com/l/?uddg=<encoded_url>` のリダイレクト URL を実際の URL に解決する。
/// DDG 以外の URL はそのまま返す。
pub fn resolve_ddg_redirect(url: Url) -> Url {
    if url.host_str() != Some("duckduckgo.com") || url.path() != "/l/" {
        return url;
    }
    url.query_pairs()
        .find(|(k, _)| k == "uddg")
        .and_then(|(_, v)| v.parse::<Url>().ok())
        .unwrap_or(url)
}

/// 要素の class 属性をスペース区切りのトークンで返す
fn class_tokens(handle: &Handle) -> Vec<String> {
    match &handle.data {
        NodeData::Element { attrs, .. } => attrs
            .borrow()
            .iter()
            .find(|a| a.name.local.as_ref() == "class")
            .map(|a| a.value.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// class トークンに `result` / `web-result` を持つ要素（DDG の結果 1 件分の枠）の数
fn count_result_blocks(handle: &Handle) -> usize {
    let own = class_tokens(handle)
        .iter()
        .any(|t| t == "result" || t == "web-result") as usize;
    own + handle
        .children
        .borrow()
        .iter()
        .map(count_result_blocks)
        .sum::<usize>()
}

/// DOM を再帰走査して result__a / result__url / result__snippet の <a> を収集する。
///
/// class 属性全体の完全一致で照合すると、DDG がトークンを 1 つ追加しただけで
/// 全件取りこぼすため、スペース区切りのトークン単位で照合する。
fn collect_result_links(handle: &Handle, out: &mut Vec<(String, String, String)>) {
    if let NodeData::Element { name, attrs, .. } = &handle.data {
        if name.local.as_ref() == "a" {
            let class = class_tokens(handle)
                .into_iter()
                .find(|t| matches!(t.as_str(), "result__a" | "result__url" | "result__snippet"));
            let attrs_ref = attrs.borrow();

            if let Some(class) = class {
                let href = attrs_ref
                    .iter()
                    .find(|a| a.name.local.as_ref() == "href")
                    .map(|a| a.value.as_ref().to_owned())
                    .unwrap_or_default();

                let mut text = String::new();
                collect_text(handle, &mut text);
                out.push((class, href, text));
                return; // <a> の子孫は再帰しない
            }
        }
    }

    for child in handle.children.borrow().iter() {
        collect_result_links(child, out);
    }
}

/// テキストノードを再帰的に収集してプレーンテキストを生成する。
fn collect_text(handle: &Handle, out: &mut String) {
    match &handle.data {
        NodeData::Text { contents } => {
            out.push_str(contents.borrow().as_ref());
        }
        _ => {
            for child in handle.children.borrow().iter() {
                collect_text(child, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ddg_html(results: &[(&str, &str, &str)]) -> String {
        // (title, url, snippet) から DDG 風 HTML を生成するヘルパー
        let mut body = String::new();
        for (title, url, snippet) in results {
            let encoded = url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>();
            body.push_str(&format!(
                r#"<div class="result">
<a class="result__a" href="//duckduckgo.com/l/?uddg={encoded}&amp;rut=abc">{title}</a>
<a class="result__url" href="//duckduckgo.com/l/?uddg={encoded}">{url}</a>
<a class="result__snippet" href="//duckduckgo.com/l/?uddg={encoded}">{snippet}</a>
</div>"#
            ));
        }
        format!("<html><body>{}</body></html>", body)
    }

    #[test]
    fn test_parse_ddg_results_groups_and_resolves() {
        let html = ddg_html(&[
            (
                "Example Domain",
                "https://example.com/",
                "An illustrative domain",
            ),
            (
                "Rust Lang",
                "https://www.rust-lang.org/",
                "A language empowering everyone",
            ),
        ]);
        let results = parse_ddg_results(&html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Example Domain");
        assert_eq!(results[0].url.as_str(), "https://example.com/");
        assert_eq!(results[0].snippet, "An illustrative domain");
        assert_eq!(results[1].url.as_str(), "https://www.rust-lang.org/");
    }

    /// DDG が class にトークンを追加しても結果を取りこぼさない
    #[test]
    fn test_parse_ddg_results_with_extra_class_tokens() {
        let html = ddg_html(&[("Example Domain", "https://example.com/", "snippet")])
            .replace(
                "class=\"result__a\"",
                "class=\"result__a js-result-title-link\"",
            )
            .replace(
                "class=\"result__snippet\"",
                "class=\"result__snippet  new\"",
            );
        let results = parse_ddg_results(&html);
        assert_eq!(results.len(), 1, "{html}");
        assert_eq!(results[0].title, "Example Domain");
        assert_eq!(results[0].snippet, "snippet");
    }

    /// 結果ブロックがあるのに 1 件も取れない場合はパース失敗として「0 件」と区別する
    #[test]
    fn test_parse_ddg_results_checked_distinguishes_parse_failure() {
        let changed = ddg_html(&[("Example", "https://example.com/", "s")])
            .replace("result__a", "result-title")
            .replace("result__url", "result-url")
            .replace("result__snippet", "result-snippet");
        assert_eq!(
            parse_ddg_results_checked(&changed),
            Err(DdgParseError::UnrecognizedResults { blocks: 1 })
        );
        assert!(parse_ddg_results(&changed).is_empty());

        // 結果ブロック自体がない「0 件」ページは正常な空結果
        let no_results = r#"<html><body><div class="no-results">No results.</div></body></html>"#;
        assert_eq!(parse_ddg_results_checked(no_results), Ok(vec![]));

        let ok = ddg_html(&[("Example", "https://example.com/", "s")]);
        assert_eq!(parse_ddg_results_checked(&ok).unwrap().len(), 1);
    }

    #[test]
    fn test_parse_ddg_results_empty_on_challenge_page() {
        // anomaly ページには result__a が存在しないため空になる（ブロック検出と併用する）
        let html =
            r#"<html><body><div id="challenge-form">anomaly anomaly anomaly</div></body></html>"#;
        assert!(parse_ddg_results(html).is_empty());
    }

    #[test]
    fn test_detect_blocked_ddg_anomaly() {
        let html = include_str!("../tests/fixtures/ddg_challenge.html");
        assert!(detect_blocked_page(202, html));
        // マーカーがなければ 202 でもブロックとは断定しない…が、202 自体が異常なので true
        assert!(detect_blocked_page(202, "<html><body>ok</body></html>"));
    }

    #[test]
    fn test_detect_blocked_various_markers() {
        assert!(detect_blocked_page(200, "<title>Just a moment...</title>"));
        assert!(detect_blocked_page(
            200,
            "<html>Making sure you&#39;re not a bot</html>"
        ));
        assert!(detect_blocked_page(403, "<html>forbidden</html>"));
        assert!(detect_blocked_page(429, "<html>rate limited</html>"));
    }

    #[test]
    fn test_detect_not_blocked_normal_page() {
        assert!(!detect_blocked_page(
            200,
            "<html><body><p>normal content</p></body></html>"
        ));
        // "challenge" という単語だけではブロックと判定しない（誤検出防止）
        assert!(!detect_blocked_page(
            200,
            "<html><body>security challenge in modern software</body></html>"
        ));
    }

    #[test]
    fn test_parse_searxng_results() {
        let json = r#"{
            "query": "rust",
            "results": [
                {"title": "Rust", "url": "https://www.rust-lang.org/", "content": "A language"},
                {"title": "", "url": "https://empty-title.example/", "content": "skipped"},
                {"title": "No URL"}
            ]
        }"#;
        let results = parse_searxng_results(json).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[0].snippet, "A language");
        assert_eq!(results[0].display_url, "www.rust-lang.org");
    }

    #[test]
    fn test_parse_searxng_results_invalid() {
        assert!(parse_searxng_results("not json").is_none());
        assert!(parse_searxng_results(r#"{"no_results_key": []}"#).is_none());
        // results が空配列 = 正当な「0件」
        assert_eq!(
            parse_searxng_results(r#"{"results": []}"#).unwrap().len(),
            0
        );
    }

    #[test]
    fn test_searxng_unresponsive_engines() {
        let json = r#"{"results": [], "unresponsive_engines": [["google", "timeout"], ["bing", "CAPTCHA"]]}"#;
        assert_eq!(
            searxng_unresponsive_engines(json),
            vec!["google: timeout".to_string(), "bing: CAPTCHA".to_string()]
        );
        assert!(searxng_unresponsive_engines(r#"{"results": []}"#).is_empty());
        assert!(searxng_unresponsive_engines("not json").is_empty());
    }

    #[test]
    fn test_parse_brave_results() {
        let json = r#"{
            "web": {
                "results": [
                    {"title": "Example", "url": "https://example.com/", "description": "desc"}
                ]
            }
        }"#;
        let results = parse_brave_results(json).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example");
        assert_eq!(results[0].snippet, "desc");
        assert!(parse_brave_results("not json").is_none());
        assert!(parse_brave_results(r#"{"web": {}}"#).is_none());
    }

    #[test]
    fn test_render_results_markdown() {
        let results = vec![SearchResult {
            title: "Example".to_owned(),
            url: Url::parse("https://example.com/").unwrap(),
            snippet: "snippet text".to_owned(),
            display_url: "example.com".to_owned(),
        }];
        let md = render_results_markdown("test query", &results);
        assert!(md.contains("# Search: test query"));
        assert!(md.contains("## 1. Example"));
        assert!(md.contains("<https://example.com/>"));
        assert!(md.contains("snippet text"));
    }

    #[test]
    fn test_resolve_ddg_redirect() {
        let ddg = Url::parse("https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F&rut=x")
            .unwrap();
        assert_eq!(resolve_ddg_redirect(ddg).as_str(), "https://example.com/");

        let plain = Url::parse("https://example.com/page").unwrap();
        assert_eq!(resolve_ddg_redirect(plain.clone()), plain);
    }
}
