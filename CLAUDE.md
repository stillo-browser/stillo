# stillo CLAUDE.md

## プロジェクト概要

AIネイティブなターミナルブラウザ。Rust実装（edition 2021）。

## クレート責務

- `stillo-core`: 純粋関数のみ。副作用禁止（I/O・HTTP・LLM呼び出し禁止）
- `stillo-fetcher`: HTTP・SPA委譲のI/O
- `stillo` (cli): コマンドライン引数解析・全体オーケストレーション
- `stillo-renderer`: TUI描画（Phase 3）
- `stillo-llm`: LLM API呼び出し（Phase 4）
- `stillo-mcp`: MCPサーバー（Phase 5）

## 依存方向

```
cli → fetcher → core
cli → renderer → core    (Phase 3)
cli → llm → core         (Phase 4)
cli → mcp → fetcher → core (Phase 5)
```

`core` は他クレートに依存しない。

## 非同期

async/await を使用。.then() チェーン禁止。

## エラー処理

- ライブラリクレート（core, fetcher 等）: `thiserror` で型付きエラー
- アプリケーション層（cli）: `anyhow` で伝播

## 命名規則

- SPA関連型: `Spa` プレフィックス（例: `SpaDetection`, `SpaDelegationChain`）
- 取得結果: `FetchResult` enum（直和型）
- 変換関数: `extract_*`, `serialize_*`, `detect_*`

## コメント規約

関数やクラスなどのヘッダーに概要を記載する。
処理についてのコメントは何をしているかではなく、**なぜそうしたか**を書く。

## lessons

- `RawHtml` はバイト列で保持し、文字コード変換は抽出レイヤーで行う
- SPA かどうかを判定してから委譲先を選ぶ（委譲先の選択をフェッチ前に決めない）
- MCP の stdout 書き込みはバッファリングして都度 flush すること
- `ParsedDocument` は `Rc<Node>` を含むため `Send` 非対応。`extract()` は同期関数として実装し、async 境界をまたがない
- `markup5ever_rcdom::Handle` は `Rc<Node>` のエイリアス。DOM操作は単一スレッドで完結させる
- CSS クラスのノイズ判定は `val.contains(pattern)` ではなく、スペース→ハイフンで分解したコンポーネント単位の完全一致を使う。`"shadow-2xs"` が `"ad"` にヒットするような誤検出を防ぐため
- 検索バックエンド（DDG/SearXNG/Brave）はブロックページ検出が必須。HTTP 202 + HTML マーカー（`id="anomaly-modal"` 等）で判定し、空結果ではなくエラーとして伝播する。ブロック検出がないと「検索結果なし」と誤認される
