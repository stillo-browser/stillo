use reqwest::Client;
use url::Url;
use stillo_core::document::{FetchError, RawHtml};

/// Jina Reader API 経由でページを取得する。
/// Jina はページの Markdown を返す。レスポンスを解析してタイトルと本文を分離し、
/// 下流の HTML パイプラインに渡せる形に変換する。
pub async fn fetch_via_jina(
    client: &Client,
    api_key: Option<&str>,
    url: &Url,
) -> Result<RawHtml, FetchError> {
    let jina_url = format!("https://r.jina.ai/{}", url);

    let mut builder = client
        .get(&jina_url)
        .header("Accept", "text/plain")
        .header("X-No-Cache", "true");

    if let Some(key) = api_key {
        builder = builder.header("Authorization", format!("Bearer {}", key));
    }

    let resp = builder
        .send()
        .await
        .map_err(|e| FetchError::DelegationFailed(format!("jina request failed: {}", e)))?;

    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(FetchError::Http { status, url: url.clone() });
    }

    let body = resp
        .text()
        .await
        .map_err(|e| FetchError::DelegationFailed(format!("jina response read failed: {}", e)))?;

    // Jina レスポンスのフォーマット:
    //   Title: {title}
    //
    //   URL Source: {url}
    //
    //   Markdown Content:
    //   {content}
    let (title, content) = parse_jina_response(&body);

    // Markdown コンテンツを <article> で包んだ HTML に変換。
    // <pre> ではなく <div> で包むことで Readability が段落を正しく認識できる。
    let html = format!(
        concat!(
            "<html><head><title>{title}</title></head>",
            "<body><article>{content}</article></body></html>"
        ),
        title = html_escape(&title),
        content = markdown_to_html_minimal(&content),
    );

    Ok(RawHtml {
        bytes: html.into_bytes(),
        url: url.clone(),
        content_type: "text/html; charset=utf-8".to_owned(),
        status,
    })
}

/// Jina レスポンスを title と markdown content に分割する
fn parse_jina_response(body: &str) -> (String, String) {
    let mut title = String::new();
    let mut content_start = 0;
    for (i, line) in body.lines().enumerate() {
        if let Some(rest) = line.strip_prefix("Title: ") {
            title = rest.trim().to_owned();
        } else if line.starts_with("Markdown Content:") {
            // "Markdown Content:" 以降が本文
            let byte_offset: usize = body
                .lines()
                .take(i + 1)
                .map(|l| l.len() + 1) // +1 for newline
                .sum();
            content_start = byte_offset;
            break;
        }
    }

    let content = body.get(content_start..).unwrap_or(body).trim().to_owned();
    (title, content)
}

/// Markdown の見出し・段落のみ最小限 HTML に変換する。
/// 完全な変換は core::markdown が担うため、ここでは構造のみ与える。
fn markdown_to_html_minimal(md: &str) -> String {
    let mut html = String::new();
    let mut in_code_block = false;

    for line in md.lines() {
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            if in_code_block {
                html.push_str("<pre><code>");
            } else {
                html.push_str("</code></pre>\n");
            }
            continue;
        }

        if in_code_block {
            html.push_str(&html_escape(line));
            html.push('\n');
            continue;
        }

        if let Some(rest) = line.strip_prefix("# ") {
            html.push_str(&format!("<h1>{}</h1>\n", html_escape(rest)));
        } else if let Some(rest) = line.strip_prefix("## ") {
            html.push_str(&format!("<h2>{}</h2>\n", html_escape(rest)));
        } else if let Some(rest) = line.strip_prefix("### ") {
            html.push_str(&format!("<h3>{}</h3>\n", html_escape(rest)));
        } else if line.trim().is_empty() {
            html.push_str("<br>\n");
        } else {
            html.push_str(&format!("<p>{}</p>\n", html_escape(line)));
        }
    }

    html
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
