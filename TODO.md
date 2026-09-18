# TODO

## Open

- [ ] [IMPROVE] ソーシャル共有ウィジェットの除外（低優先度） <!-- 2026-05-15 -->

- [ ] [IMPROVE] 大型ニュースサイトの出力サイズ制限 <!-- 2026-05-16 -->
  - `https://www.npr.org/sections/news/` で 60KB の出力が返った（通常の 3〜5 倍）
  - ニュース系インデックスページでは記事リストが大量に展開されるため
  - 対策案: (A) 出力上限 (max_chars) パラメータを MCP ツールに追加 / (B) `read_links` で見出し+URLのみ先に取得してから個別記事を fetch する推奨フローをドキュメント化
  - File: `crates/stillo-mcp/src/tools.rs` または `crates/core/src/markdown.rs`

- [ ] [BUG] Al Jazeera `/tag/` URL → 404 <!-- 2026-05-16 -->
  - `https://www.aljazeera.com/tag/iran/` が 404 で返る
  - `/news/` (`https://www.aljazeera.com/news/`) は正常に取得可能
  - タグページは SPA 完全レンダリングが必要な可能性。または URL 構造が変わった
  - 回避策: `/news/` ページを fetch して iran 関連記事を手動フィルタ
  - File: `crates/stillo-fetcher/src/lib.rs`（SPA判定ロジック）

- [ ] [IMPROVE] 共有ボタンテキストの本文混入（日経新聞等 CSS Modules サイト） <!-- 2026-05-16 -->
  - 「記事を印刷する」「X（旧Twitter）」等の共有ボタンテキストが本文に混入する
  - 誤検出リスクを考慮して対応見送り。方針は2択: (A) NOISE_CLASS_PATTERNS に "share"/"sns" 追加（30分、CSS Modules には効かない）/ (B) `<li>` テキストが SNS 名と完全一致ならスキップ（2〜3時間、CSS Modules も対応可だが本文 SNS 言及を誤除外するリスクあり）
  - File: `crates/core/src/extractor/readability.rs`

## Done

- [x] [BUG] search_web がボットブロック時に空結果を返す <!-- 2026-08-11, fixed 2026-08-11 -->
  - DDG が HTTP 202 の anomaly チャレンジページを返すと、パーサーが空配列を返し「検索結果なし」と誤認されていた
  - 修正: `core::search::detect_blocked_page` でステータス(202/403/429)とHTMLマーカーからブロックを検出し、`fetcher::search::web_search` がエラーとして伝播。`SEARXNG_URL`/`BRAVE_API_KEY` のバックエンドフォールバックも追加
  - File: `crates/core/src/search.rs`, `crates/fetcher/src/search.rs`

- [x] [IMPROVE] readability: 全項目がリンクで包まれたインデックスページの抽出精度向上 <!-- 2026-05-14, fixed 2026-05-14 -->
  - gihyo.jp 等、記事カード全体が `<a>` で囲まれた構造では body フォールバックが動作するがサイドバーノイズが混入する
  - 隣接する同クラス要素の繰り返しパターン検出（シブリング展開）で親コンテナを選ぶヒューリスティックが有効な可能性がある
  - File: `crates/core/src/extractor/readability.rs`

- [x] [BUG] Classmethod dev blog の本文抽出が薄い <!-- 2026-05-15, fixed 2026-05-15 -->
  - 原因: `is_noise` が部分文字列一致していたため、Tailwind の `shadow-2xs` が `"ad"` に誤ヒットしていた
  - 修正: `class_contains_pattern()` でコンポーネント完全一致に変更。回帰テスト追加済み（2026-08-11）
  - File: `crates/core/src/extractor/readability.rs`

- [x] [BUG] fetch_url MCP ツール: Markdown 本文中のリンクが相対 URL のまま返される <!-- 2026-05-14, fixed 2026-05-14 -->
  - `HtmlToMarkdown` に base_url を持たせ `url::Url::join()` で絶対化
  - File: `crates/core/src/markdown.rs`

- [x] [BUG] RSS 1.0 (RDF形式) 未対応 <!-- 2026-05-15, fixed 2026-05-15 -->
  - はてブの RSS は `<rdf:RDF>` をルートとする RSS 1.0 形式
  - File: `crates/core/src/rss_to_ast.rs`
