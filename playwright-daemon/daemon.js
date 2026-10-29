/**
 * stillo Playwright daemon
 *
 * Unix ソケット上で JSON-lines プロトコルを話す。
 *   request : {"url": "https://..."}\n
 *   response: {"html": "...", "url": "...", "status": 200}\n
 *             {"error": "..."}\n  (失敗時)
 *
 * ブラウザインスタンスは起動時に1つだけ作成し、リクエストごとに新しいページを開く。
 */

'use strict';

const net = require('net');
const fs = require('fs');
const path = require('path');
const { chromium } = require('playwright');

// stillo 側（crates/fetcher/src/spa/playwright.rs の socket_candidates）と同じ優先順位。
// $XDG_RUNTIME_DIR は所有者のみアクセス可能なため、誰でも書ける /tmp より優先する。
const SOCKET_PATH =
  process.env.STILLO_PLAYWRIGHT_SOCK ||
  (process.env.XDG_RUNTIME_DIR
    ? path.join(process.env.XDG_RUNTIME_DIR, 'stillo-playwright.sock')
    : '/tmp/stillo-playwright.sock');
const NAVIGATION_TIMEOUT_MS = 30_000;
// load 後にバックグラウンド JS の描画を待つ固定遅延（ms）
const POST_LOAD_DELAY_MS = 1_000;

/**
 * SPA がナビゲーション中に別パスへ離脱したかを判定する。
 *
 * 同 origin 内で先頭パスセグメントが変わった場合のみ「離脱」とみなす。
 * origin をまたぐ変化（http→https 等の正規リダイレクト）は除外する。
 *
 * 例:
 *   qiita.com/masakai/items/...  → qiita.com/embed-contents/mermaid  ✓ 離脱
 *   http://example.com/page      → https://example.com/page           ✗ 正規
 *   example.com/page             → example.com/page#section           ✗ 正規
 */
function isNavigatedAway(requestedUrl, actualUrl) {
  try {
    const req = new URL(requestedUrl);
    const act = new URL(actualUrl);
    if (req.origin !== act.origin) return false;
    const reqSeg = req.pathname.split('/').filter(Boolean)[0] ?? '';
    const actSeg = act.pathname.split('/').filter(Boolean)[0] ?? '';
    return reqSeg !== actSeg;
  } catch {
    return false;
  }
}

async function handleRequest(browser, socket, line) {
  let req;
  try {
    req = JSON.parse(line);
  } catch (e) {
    socket.write(JSON.stringify({ error: `invalid JSON: ${e.message}` }) + '\n');
    return;
  }

  if (!req.url) {
    socket.write(JSON.stringify({ error: 'missing url field' }) + '\n');
    return;
  }

  const page = await browser.newPage();
  try {
    // networkidle はバックグラウンドポーリングが続くサイト（Qiita 等）で
    // 永遠に到達しないため、load + 固定遅延を基本戦略とする。
    const response = await page.goto(req.url, {
      waitUntil: 'load',
      timeout: NAVIGATION_TIMEOUT_MS,
    });
    await page.waitForTimeout(POST_LOAD_DELAY_MS);

    // Mermaid embed 等、SPA が意図せず別パスへ遷移した場合は元 URL へ再ナビ。
    if (isNavigatedAway(req.url, page.url())) {
      console.log(`re-navigating: ${page.url()} → ${req.url}`);
      await page.goto(req.url, {
        waitUntil: 'load',
        timeout: Math.floor(NAVIGATION_TIMEOUT_MS / 2),
      });
      await page.waitForTimeout(POST_LOAD_DELAY_MS);
    }

    const html = await page.content();
    const finalUrl = page.url();
    const status = response ? response.status() : 200;

    socket.write(JSON.stringify({ html, url: finalUrl, status }) + '\n');
  } catch (e) {
    socket.write(JSON.stringify({ error: e.message }) + '\n');
  } finally {
    await page.close();
  }
}

async function main() {
  if (fs.existsSync(SOCKET_PATH)) {
    fs.unlinkSync(SOCKET_PATH);
  }

  const browser = await chromium.launch({ headless: true });
  console.log('Browser launched');

  const server = net.createServer((socket) => {
    let buf = '';

    socket.on('data', (chunk) => {
      buf += chunk.toString('utf8');

      // 改行ごとにリクエストを処理する（1接続1リクエスト想定）
      let nl;
      while ((nl = buf.indexOf('\n')) !== -1) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (line.length > 0) {
          handleRequest(browser, socket, line).catch((e) => {
            socket.write(JSON.stringify({ error: String(e) }) + '\n');
          });
        }
      }
    });

    socket.on('error', (e) => {
      console.error('socket error:', e.message);
    });
  });

  server.listen(SOCKET_PATH, () => {
    // /tmp に置いた場合に他ユーザーがデーモンへ接続し、自分の権限でブラウザを操作できないようにする
    fs.chmodSync(SOCKET_PATH, 0o600);
    console.log(`Playwright daemon listening on ${SOCKET_PATH}`);
    // 親プロセスへ起動完了を通知する（テスト用）
    if (process.send) process.send('ready');
  });

  process.on('SIGTERM', async () => {
    server.close();
    await browser.close();
    if (fs.existsSync(SOCKET_PATH)) fs.unlinkSync(SOCKET_PATH);
    process.exit(0);
  });

  process.on('SIGINT', async () => {
    server.close();
    await browser.close();
    if (fs.existsSync(SOCKET_PATH)) fs.unlinkSync(SOCKET_PATH);
    process.exit(0);
  });
}

main().catch((e) => {
  console.error('Failed to start daemon:', e);
  process.exit(1);
});
