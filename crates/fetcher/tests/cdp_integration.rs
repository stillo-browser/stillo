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
use tokio::net::TcpListener;

const CHROME_CANDIDATES: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "chrome",
];

fn find_chrome_binary() -> Option<&'static str> {
    CHROME_CANDIDATES.iter().copied().find(|candidate| {
        std::process::Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok()
    })
}

/// テスト用 SPA ページを返す最小 HTTP サーバーを起動し、ポートを返す。
///
/// ページは初期状態で "Loading..." を表示し、DOMContentLoaded 後に
/// JS で "Hello from JavaScript!" へ書き換える。
async fn start_spa_test_server() -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                // リクエストの内容は見ないため読み込みバイト数チェックは不要
                #[allow(clippy::unused_io_amount)]
                stream.read(&mut buf).await.ok();

                let body = r#"<!DOCTYPE html>
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
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(resp.as_bytes()).await.ok();
            });
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
/// chromiumoxide::Browser::launch は本番コード(cdp.rs)では使用禁止だが、
/// このテストハーネスは「ユーザー環境で起動済みの Chrome」を模しているだけなので
/// tokio::process::Command で直接起動してよい。
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

/// Browser::close() を誤って呼んでいないかを検証する回帰テスト。
///
/// chromiumoxide の Browser::close() は connect() 由来であっても
/// Browser.close CDP コマンドを送信し、ユーザーが起動している共有 Chrome
/// インスタンス全体を終了させてしまう。フェッチ成功後も同じ port で
/// Chrome が生き続けていることを確認する。
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
        "fetch_via_cdp 実行後もChromeは起動したままであるべきです(Browser::close()を呼んでいないか確認)"
    );

    let _ = std::fs::remove_dir_all(&user_data_dir);
}

/// 到達不能ターゲットへの挙動確認。
///
/// CDPの Target.createTarget は Page.navigate と異なりエラーテキストを
/// 返さないため、到達不能URLでも new_page 自体は成功し、Chromeが表示する
/// インターステイシャルページのHTMLが Ok として返る可能性がある。
/// そのため Ok/Err どちらも許容し、タイムアウト予算内でハングせず
/// 完了することのみを検証する。
#[tokio::test]
async fn test_cdp_completes_for_unreachable_target() {
    let Some(binary) = find_chrome_binary() else {
        eprintln!("Chrome/Chromiumバイナリが見つかりません。このテストをスキップします。");
        return;
    };

    let port = free_port().await;
    let user_data_dir = unique_user_data_dir("unreachable");
    let _chrome = start_headless_chrome(port, &user_data_dir, binary).await;

    // ローカルの到達不能ポート(何もリッスンしていない)
    let url = "http://127.0.0.1:1/".parse().unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(60),
        stillo_fetcher::spa::cdp::fetch_via_cdp(port, &url),
    )
    .await;

    let _ = std::fs::remove_dir_all(&user_data_dir);

    assert!(
        result.is_ok(),
        "到達不能ターゲットに対してタイムアウト予算内に完了しませんでした(ハングの可能性)"
    );
}
