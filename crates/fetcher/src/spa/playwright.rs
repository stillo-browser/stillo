use crate::http::DEFAULT_MAX_BODY_BYTES;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use stillo_core::document::{FetchError, RawHtml};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use url::Url;

/// デーモン（playwright-daemon/daemon.js）と共有するソケットファイル名
pub const SOCKET_FILE_NAME: &str = "stillo-playwright.sock";

/// Playwright デーモンのソケットパス候補を優先順に返す（存在確認はしない）。
///
/// daemon.js と同じ規則: `STILLO_PLAYWRIGHT_SOCK` > `$XDG_RUNTIME_DIR` > `/tmp`。
/// `$XDG_RUNTIME_DIR` は所有者のみアクセス可能なディレクトリなので /tmp より優先する。
pub fn socket_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(p) = std::env::var_os("STILLO_PLAYWRIGHT_SOCK").filter(|p| !p.is_empty()) {
        candidates.push(PathBuf::from(p));
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|p| !p.is_empty()) {
        candidates.push(PathBuf::from(dir).join(SOCKET_FILE_NAME));
    }
    candidates.push(PathBuf::from("/tmp").join(SOCKET_FILE_NAME));
    candidates
}

/// 自ユーザーが所有する Playwright デーモンのソケットを探す。
///
/// /tmp は誰でもファイルを作れるため、存在するだけで信頼すると他ユーザーが
/// 偽デーモンを立てて任意の HTML を返せてしまう。所有者 uid が自プロセスと一致するものだけ使う。
pub fn discover_socket() -> Option<PathBuf> {
    let uid = current_uid()?;
    trusted_socket(&socket_candidates(), uid)
}

fn trusted_socket(candidates: &[PathBuf], uid: u32) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.uid() == uid))
        .cloned()
}

/// 自プロセスの実効 uid を返す。
///
/// libc を直接依存に加えないため、Linux では実効 uid で所有される /proc/self の所有者を使う。
/// /proc が無い環境（macOS 等）では $HOME の所有者で代用する。
fn current_uid() -> Option<u32> {
    if let Ok(m) = std::fs::metadata("/proc/self") {
        return Some(m.uid());
    }
    let home = std::env::var_os("HOME")?;
    std::fs::metadata(home).ok().map(|m| m.uid())
}

/// Playwright デーモン（`playwright-daemon/daemon.js`）経由でSPAのHTMLを取得する。
///
/// プロトコル（JSON-lines, Unix socket）:
///   request : `{"url":"https://..."}\n`
///   response: `{"html":"...","url":"https://...","status":200}\n`
///             `{"error":"..."}\n`（失敗時）
pub async fn fetch_via_playwright(socket_path: &Path, url: &Url) -> Result<RawHtml, FetchError> {
    let stream = UnixStream::connect(socket_path).await.map_err(|e| {
        FetchError::DelegationFailed(format!(
            "Playwright daemon not reachable at {}: {}",
            socket_path.display(),
            e
        ))
    })?;

    let (reader, mut writer) = tokio::io::split(stream);

    let req = format!("{}\n", serde_json::json!({ "url": url.as_str() }));
    writer.write_all(req.as_bytes()).await.map_err(|e| {
        FetchError::DelegationFailed(format!("Playwright socket write failed: {}", e))
    })?;

    let buf = tokio::time::timeout(
        tokio::time::Duration::from_secs(60),
        read_line_limited(BufReader::new(reader), DEFAULT_MAX_BODY_BYTES),
    )
    .await
    .map_err(|_| FetchError::DelegationFailed("Playwright response timed out".into()))??;

    let resp: serde_json::Value = serde_json::from_slice(&buf).map_err(|e| {
        FetchError::DelegationFailed(format!("Playwright response parse failed: {}", e))
    })?;

    if let Some(err) = resp["error"].as_str() {
        return Err(FetchError::DelegationFailed(format!("Playwright: {}", err)));
    }

    let html = resp["html"].as_str().ok_or_else(|| {
        FetchError::DelegationFailed("Playwright: response missing html field".into())
    })?;

    let final_url = resp["url"]
        .as_str()
        .and_then(|s| s.parse::<Url>().ok())
        .unwrap_or_else(|| url.clone());

    let status = resp["status"].as_u64().unwrap_or(200) as u16;

    Ok(RawHtml {
        bytes: html.as_bytes().to_vec(),
        url: final_url,
        content_type: "text/html; charset=utf-8".to_owned(),
        status,
    })
}

/// 改行までを最大 `limit` バイト読む。改行なしで上限を超えたら `BodyTooLarge`。
///
/// `read_line` は改行が来るまで際限なくバッファするため、デーモンが巨大な応答や
/// 改行のないデータを返すとメモリを使い切る。
async fn read_line_limited<R: tokio::io::AsyncBufRead + Unpin>(
    reader: R,
    limit: usize,
) -> Result<Vec<u8>, FetchError> {
    let mut buf = Vec::new();
    // 上限ちょうどの行と上限超過を区別するため 1 バイト余分に読む
    reader
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await
        .map_err(|e| {
            FetchError::DelegationFailed(format!("Playwright socket read failed: {}", e))
        })?;
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    if buf.len() > limit {
        return Err(FetchError::BodyTooLarge { limit });
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_line_limited_rejects_oversized_line() {
        let data = [b'a'; 100];
        let err = read_line_limited(&data[..], 10).await.unwrap_err();
        assert!(matches!(err, FetchError::BodyTooLarge { limit: 10 }));

        let ok = read_line_limited(&b"0123456789\nrest"[..], 10)
            .await
            .unwrap();
        assert_eq!(ok, b"0123456789");
    }

    #[test]
    fn socket_owned_by_other_user_is_not_trusted() {
        let dir = std::env::temp_dir().join(format!("stillo-pw-owner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SOCKET_FILE_NAME);
        std::fs::write(&path, b"").unwrap();
        let my_uid = current_uid().unwrap();
        let missing = dir.join("missing.sock");

        let candidates = [missing.clone(), path.clone()];
        assert_eq!(trusted_socket(&candidates, my_uid), Some(path.clone()));
        // 所有者 uid が一致しなければ存在していても使わない
        assert_eq!(trusted_socket(&candidates, my_uid.wrapping_add(1)), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
