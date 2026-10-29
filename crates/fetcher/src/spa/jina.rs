use crate::http::{read_body_limited, DEFAULT_MAX_BODY_BYTES};
use reqwest::Client;
use stillo_core::document::{FetchError, RawHtml};
use url::Url;

const JINA_ENDPOINT: &str = "https://r.jina.ai/";

/// Jina Reader API 経由でページを取得する。
/// Jina はページの Markdown を返す。レスポンスを解析してタイトルと本文を分離し、
/// 下流の HTML パイプラインに渡せる形に変換する。
pub async fn fetch_via_jina(
    client: &Client,
    api_key: Option<&str>,
    url: &Url,
) -> Result<RawHtml, FetchError> {
    fetch_from(JINA_ENDPOINT, client, api_key, url).await
}

/// エンドポイントを差し替え可能にした本体。テストでローカルサーバーを指すため分離している。
async fn fetch_from(
    endpoint: &str,
    client: &Client,
    api_key: Option<&str>,
    url: &Url,
) -> Result<RawHtml, FetchError> {
    let jina_url = format!("{}{}", endpoint, url);

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

    // Jina 自身の 4xx/5xx（認証失敗・レート制限等）を対象サイトの HTTP エラーと
    // 誤認させないため、Http ではなくバックエンド名付きの DelegationFailed にする
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(FetchError::DelegationFailed(format!(
            "jina returned HTTP {}",
            status
        )));
    }

    let bytes = read_body_limited(
        resp,
        DEFAULT_MAX_BODY_BYTES,
        url,
        super::SPA_HTTP_TIMEOUT_SECS,
    )
    .await?;
    let body = String::from_utf8_lossy(&bytes);

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
    // 行末が \r\n でもバイトオフセットがずれないよう、改行込みの実際の長さで数える
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        offset += line.len();
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(rest) = line.strip_prefix("Title: ") {
            title = rest.trim().to_owned();
        } else if line.starts_with("Markdown Content:") {
            // "Markdown Content:" 以降が本文
            content_start = offset;
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

pub(super) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn parse_handles_crlf_line_endings() {
        let body = "Title: Hello\r\n\r\nURL Source: https://example.com\r\n\r\nMarkdown Content:\r\nFirst line\r\nSecond";
        let (title, content) = parse_jina_response(body);
        assert_eq!(title, "Hello");
        assert!(content.starts_with("First line"), "{content:?}");

        let lf = "Title: Hello\n\nMarkdown Content:\nFirst line\n";
        assert_eq!(parse_jina_response(lf).1, "First line");
    }

    #[tokio::test]
    async fn jina_4xx_is_delegation_failure_not_target_http_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            s.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .ok();
        });
        let endpoint = format!("http://127.0.0.1:{}/", port);
        let target = Url::parse("https://example.com/").unwrap();
        let err = fetch_from(&endpoint, &Client::new(), None, &target)
            .await
            .unwrap_err();
        match err {
            FetchError::DelegationFailed(msg) => {
                assert!(msg.contains("jina") && msg.contains("401"), "{msg}")
            }
            other => panic!("expected DelegationFailed, got {other:?}"),
        }
    }
}
