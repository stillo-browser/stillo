/// Playwright デーモン経由の SPA フェッチ統合テスト
///
/// 実行方法:
///   cargo test -p stillo-fetcher --test playwright_integration
///
/// テスト内でデーモンプロセスを自動起動・停止する。
///
/// playwright-daemon/node_modules が存在しない場合はスキップされる。
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn daemon_js() -> PathBuf {
    workspace_root().join("playwright-daemon").join("daemon.js")
}

fn daemon_installed() -> bool {
    workspace_root()
        .join("playwright-daemon")
        .join("node_modules")
        .exists()
}

/// テスト用 Unix ソケットパスを一意に生成する（並列テスト対策: PID + スレッドID）
fn test_socket_path() -> PathBuf {
    let pid = std::process::id();
    let tid = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>();
    PathBuf::from(format!("/tmp/stillo-playwright-test-{}-{}.sock", pid, tid))
}

/// テスト用 SPA ページを返す最小 HTTP サーバーを起動し、ポートを返す。
///
/// ページは初期状態で "Loading..." を表示し、DOMContentLoaded 後に
/// JS で "Hello from JavaScript!" へ書き換える。
async fn start_spa_test_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        // 1リクエストだけ受け付けて終了
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                // リクエストの先頭行だけ読めば十分（UI 側の判定に使わない）。切断は許容。
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let _ = n;

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

/// デーモンプロセスを起動し、ソケットが現れるまで待機する。
async fn start_daemon(socket_path: &Path) -> tokio::process::Child {
    let child = tokio::process::Command::new("node")
        .arg(daemon_js())
        .env("STILLO_PLAYWRIGHT_SOCK", socket_path)
        .kill_on_drop(true)
        .spawn()
        .expect("node コマンドが見つかりません");

    // ソケットファイルが作成されるまで最大 30 秒待機
    for _ in 0..300 {
        if socket_path.exists() {
            return child;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "Playwright デーモンがソケットを作成しませんでした: {:?}",
        socket_path
    );
}

#[tokio::test]
async fn test_playwright_fetches_js_rendered_content() {
    if !daemon_installed() {
        eprintln!(
            "playwright-daemon/node_modules が見つかりません。npm install を実行してください。"
        );
        return;
    }

    let socket_path = test_socket_path();

    // テスト用 SPA サーバーとデーモンを起動
    let port = start_spa_test_server().await;
    let _daemon = start_daemon(&socket_path).await;

    let url = format!("http://127.0.0.1:{}/", port).parse().unwrap();
    let result = stillo_fetcher::spa::playwright::fetch_via_playwright(&socket_path, &url).await;

    // ソケット削除
    let _ = std::fs::remove_file(&socket_path);

    let raw = result.expect("fetch_via_playwright が失敗しました");

    let html = String::from_utf8_lossy(&raw.bytes);
    assert!(
        html.contains("Hello from JavaScript!"),
        "JS レンダリング結果が含まれていません。HTML:\n{}",
        &html[..html.len().min(500)]
    );
    assert_eq!(raw.status, 200);
}

#[tokio::test]
async fn test_playwright_returns_error_for_unreachable_url() {
    if !daemon_installed() {
        return;
    }

    let socket_path = test_socket_path();
    let _daemon = start_daemon(&socket_path).await;

    // 存在しないポートへのリクエスト
    let url = "http://127.0.0.1:1/".parse().unwrap();
    let result = stillo_fetcher::spa::playwright::fetch_via_playwright(&socket_path, &url).await;

    let _ = std::fs::remove_file(&socket_path);

    assert!(result.is_err(), "到達不能 URL でエラーになるべきです");
}

#[tokio::test]
async fn test_playwright_error_when_daemon_not_running() {
    let socket_path = PathBuf::from("/tmp/stillo-playwright-nonexistent.sock");
    let url = "https://example.com".parse().unwrap();
    let result = stillo_fetcher::spa::playwright::fetch_via_playwright(&socket_path, &url).await;

    assert!(result.is_err(), "デーモン未起動時はエラーになるべきです");
}
