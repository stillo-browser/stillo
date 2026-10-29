// Chrome DevTools Protocol 経由の SPA フェッチ統合テスト
//
// 実行方法:
//   cargo test -p stillo-fetcher --test cdp_integration --features cdp
//
// テスト内で使い捨ての headless Chrome プロセスを自動起動・停止する。
// google-chrome/chromium 系バイナリが見つからない場合はスキップされる。
#![cfg(feature = "cdp")]

use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CHROME_CANDIDATES: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "chrome",
];

/// 実行可能な Chrome バイナリを探す。
///
/// spawn できても異常終了するラッパー（未インストール時に案内を出すだけのスクリプト等）を
/// 掴まないよう、終了ステータスまで確認する。
fn find_chrome_binary() -> Option<&'static str> {
    CHROME_CANDIDATES.iter().copied().find(|candidate| {
        std::process::Command::new(candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

const SPA_BODY: &str = r#"<!DOCTYPE html>
<html>
<head><title>SPA Test</title></head>
<body>
<div id="app">Loading...</div>
<script>
document.addEventListener('DOMContentLoaded', function() {
  document.getElementById('app').innerHTML = '<h1>Hello from JavaScript!</h1>';
});
</script>
</body>
</html>"#;

/// テスト用 HTTP サーバーを起動し、ポートを返す。
///
/// `/redirect` は `/final` への 302、それ以外は SPA ページを返す。
/// SPA ページは初期状態で "Loading..." を表示し、DOMContentLoaded 後に
/// JS で "Hello from JavaScript!" へ書き換える。
async fn start_spa_test_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let resp = if request.starts_with("GET /redirect ") {
                    "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        SPA_BODY.len(),
                        SPA_BODY
                    )
                };
                stream.write_all(resp.as_bytes()).await.ok();
            });
        }
    });

    port
}

/// 接続を受け付けるが一切応答しないサーバー。ナビゲーションを終わらせないために使う。
async fn start_hanging_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    port
}

/// 空きポートを1つ確保する。TOCTOU上の小さな競合リスクは許容する。
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// 使い捨ての headless Chrome プロセスを起動し、CDP ポートで応答するまで待機する。
///
/// 本番コード(cdp.rs)は「ユーザー環境で起動済みの Chrome」に接続するだけで起動はしない。
/// このハーネスはその起動済み Chrome を模すため、tokio::process::Command で直接起動する。
async fn start_headless_chrome(
    port: u16,
    user_data_dir: &std::path::Path,
    binary: &str,
) -> tokio::process::Child {
    let child = tokio::process::Command::new(binary)
        .arg("--headless=new")
        .arg(format!("--remote-debugging-port={}", port))
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("--no-sandbox")
        .arg("--disable-gpu")
        .kill_on_drop(true)
        .spawn()
        .expect("chrome コマンドの起動に失敗しました");

    for _ in 0..100 {
        if stillo_fetcher::spa::cdp::is_chrome_available(port).await {
            return child;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "Chrome が --remote-debugging-port={} でリッスンしませんでした",
        port
    );
}

fn unique_user_data_dir(label: &str) -> PathBuf {
    let pid = std::process::id();
    let tid = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>();
    std::env::temp_dir().join(format!("stillo-cdp-test-{}-{}-{}", label, pid, tid))
}

/// Chrome の page ターゲット（タブ）数を返す
async fn page_count(port: u16) -> usize {
    let list: serde_json::Value = reqwest::get(format!("http://localhost:{}/json/list", port))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    list.as_array()
        .unwrap()
        .iter()
        .filter(|t| t["type"].as_str() == Some("page"))
        .count()
}

/// タブ数が `expected` に戻るまで最大 5 秒待つ。close は非同期に反映されるため
async fn wait_page_count(port: u16, expected: usize) -> usize {
    let mut count = page_count(port).await;
    for _ in 0..50 {
        if count == expected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        count = page_count(port).await;
    }
    count
}

#[tokio::test]
async fn test_cdp_fetches_js_rendered_content() {
    let Some(binary) = find_chrome_binary() else {
        eprintln!("Chrome/Chromiumバイナリが見つかりません。このテストをスキップします。");
        return;
    };

    let port = free_port().await;
    let user_data_dir = unique_user_data_dir("content");
    let _chrome = start_headless_chrome(port, &user_data_dir, binary).await;

    let server_port = start_spa_test_server().await;
    let url = format!("http://127.0.0.1:{}/", server_port)
        .parse()
        .unwrap();

    let result = stillo_fetcher::spa::cdp::fetch_via_cdp(port, &url).await;
    let _ = std::fs::remove_dir_all(&user_data_dir);

    let raw = result.expect("fetch_via_cdp が失敗しました");
    let html = String::from_utf8_lossy(&raw.bytes);
    assert!(
        html.contains("Hello from JavaScript!"),
        "JS レンダリング結果が含まれていません。HTML:\n{}",
        &html[..html.len().min(500)]
    );
    assert_eq!(raw.status, 200);
}

#[tokio::test]
async fn test_cdp_error_when_chrome_not_running() {
    // 何もリッスンしていないポート。Chromeバイナリの有無に関わらず実行できる。
    let port = free_port().await;
    let url = "https://example.com".parse().unwrap();

    let result = stillo_fetcher::spa::cdp::fetch_via_cdp(port, &url).await;

    assert!(result.is_err(), "Chrome未起動時はエラーになるべきです");
}

/// フェッチ後も共有 Chrome が生きていることを確認する回帰テスト。
///
/// cdp.rs はユーザーが起動した Chrome にタブを1つ作って使うだけで、
/// Browser.close のようにブラウザ全体を終了させる操作をしてはならない。
#[tokio::test]
async fn test_cdp_leaves_chrome_running_after_fetch() {
    let Some(binary) = find_chrome_binary() else {
        eprintln!("Chrome/Chromiumバイナリが見つかりません。このテストをスキップします。");
        return;
    };

    let port = free_port().await;
    let user_data_dir = unique_user_data_dir("survive");
    let _chrome = start_headless_chrome(port, &user_data_dir, binary).await;

    let server_port = start_spa_test_server().await;
    let url = format!("http://127.0.0.1:{}/", server_port)
        .parse()
        .unwrap();

    let _ = stillo_fetcher::spa::cdp::fetch_via_cdp(port, &url).await;

    assert!(
        stillo_fetcher::spa::cdp::is_chrome_available(port).await,
        "fetch_via_cdp 実行後もChromeは起動したままであるべきです"
    );

    let _ = std::fs::remove_dir_all(&user_data_dir);
}

/// 到達不能 URL では Page.navigate の errorText を検出して Err を返し、タブも閉じること。
///
/// errorText を見ないと Chrome のエラーページ（chrome-error://）の HTML が
/// Ok として返り、委譲チェーンが「成功」と誤認する。
#[tokio::test]
async fn test_cdp_errors_for_unreachable_target() {
    let Some(binary) = find_chrome_binary() else {
        eprintln!("Chrome/Chromiumバイナリが見つかりません。このテストをスキップします。");
        return;
    };

    let port = free_port().await;
    let user_data_dir = unique_user_data_dir("unreachable");
    let _chrome = start_headless_chrome(port, &user_data_dir, binary).await;
    let baseline = page_count(port).await;

    // 何もリッスンしていないローカルポート（接続拒否）
    let dead_port = free_port().await;
    let url = format!("http://127.0.0.1:{}/", dead_port).parse().unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(60),
        stillo_fetcher::spa::cdp::fetch_via_cdp(port, &url),
    )
    .await
    .expect("到達不能ターゲットに対してタイムアウト予算内に完了しませんでした(ハングの可能性)");

    let after = wait_page_count(port, baseline).await;
    let _ = std::fs::remove_dir_all(&user_data_dir);

    let err = result.expect_err("到達不能 URL は Err になるべきです");
    assert!(
        err.to_string().contains("ERR_"),
        "errorText が含まれていません: {err}"
    );
    assert_eq!(after, baseline, "失敗時にタブが閉じられていません");
}

/// リダイレクト後の最終 URL が RawHtml.url に反映されること。
/// 反映されないと相対リンクが元 URL 基準で解決されてしまう。
#[tokio::test]
async fn test_cdp_reports_final_url_after_redirect() {
    let Some(binary) = find_chrome_binary() else {
        eprintln!("Chrome/Chromiumバイナリが見つかりません。このテストをスキップします。");
        return;
    };

    let port = free_port().await;
    let user_data_dir = unique_user_data_dir("redirect");
    let _chrome = start_headless_chrome(port, &user_data_dir, binary).await;

    let server_port = start_spa_test_server().await;
    let url = format!("http://127.0.0.1:{}/redirect", server_port)
        .parse()
        .unwrap();

    let result = stillo_fetcher::spa::cdp::fetch_via_cdp(port, &url).await;
    let _ = std::fs::remove_dir_all(&user_data_dir);

    let raw = result.expect("fetch_via_cdp が失敗しました");
    assert_eq!(raw.url.path(), "/final");
}

/// 呼び出し側の timeout で future がキャンセルされてもタブが閉じられること。
#[tokio::test]
async fn test_cdp_closes_tab_when_cancelled() {
    let Some(binary) = find_chrome_binary() else {
        eprintln!("Chrome/Chromiumバイナリが見つかりません。このテストをスキップします。");
        return;
    };

    let port = free_port().await;
    let user_data_dir = unique_user_data_dir("cancel");
    let _chrome = start_headless_chrome(port, &user_data_dir, binary).await;
    let baseline = page_count(port).await;

    let server_port = start_hanging_server().await;
    let url = format!("http://127.0.0.1:{}/", server_port)
        .parse()
        .unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        stillo_fetcher::spa::cdp::fetch_via_cdp(port, &url),
    )
    .await;
    assert!(
        result.is_err(),
        "応答しないサーバーで完了するはずがありません"
    );

    let after = wait_page_count(port, baseline).await;
    let _ = std::fs::remove_dir_all(&user_data_dir);
    assert_eq!(after, baseline, "キャンセル時にタブが閉じられていません");
}
