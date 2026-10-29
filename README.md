# stillo

AIネイティブなターミナルブラウザ。Rustで実装。

**stillo** は *distill*（蒸留する）から作った造語です。Webページを蒸留して情報だけを抽出するというコンセプトを名前に込めています。

## 概要

stilloはターミナル上で動作するブラウザです。静的なHTMLページはそのまま取得し、React/Vue等のSPAはChrome CDP・Jina Reader・Firecrawl等へ自動委譲してコンテンツを取得します。取得したページはMarkdown/テキスト/JSONとして出力するか、TUIで対話的にブラウズできます。LLMと連携してQ&A・要約・構造化抽出を行う機能や、MCPサーバーとして他のAIエージェントに機能を提供する機能も備えています。

## インストール

### Nix (flake)

```bash
# ビルドして result/bin/stillo を使う
nix build github:stillo-browser/stillo

# ユーザープロファイルにインストール
nix profile install github:stillo-browser/stillo

# インストールせず直接実行
nix run github:stillo-browser/stillo -- dump https://example.com
```

自分の nixpkgs に生やして home-manager / NixOS の設定に取り込む場合:

```nix
{
  inputs.stillo.url = "github:stillo-browser/stillo";

  # 方法1: overlay 経由（pkgs.stillo が使えるようになる）
  nixpkgs.overlays = [ inputs.stillo.overlays.default ];
  # home.packages = [ pkgs.stillo ];

  # 方法2: package を直接渡す
  # home.packages = [ inputs.stillo.packages.${pkgs.system}.default ];
}
```

ローカルのチェックアウトを参照するときは `path:/path/to/stillo`（`git+file:` でも可）。開発シェルは `nix develop` で入ります。

### Cargo

```bash
cargo build --release
# バイナリは target/release/stillo に生成されます
```

## 使い方

```bash
# TUIブラウザとして開く
stillo https://example.com

# Markdownとして標準出力に出力
stillo dump https://example.com

# 出力形式を指定
stillo dump --format json https://example.com
stillo dump --format plain https://example.com

# SPA委譲先を明示
stillo dump --delegate jina https://example.com

# JS委譲を無効化（静的HTMLのみ処理）
stillo dump --no-delegate https://example.com

# タイムアウト秒数を指定（デフォルト30秒）
stillo dump --timeout 60 https://example.com

# 詳細ログを出力（stderrに出力）
stillo dump -v https://example.com
```

## サブコマンド

| コマンド | 説明 |
|----------|------|
| `dump`      | ページをMarkdown/テキスト/JSONとして stdout に出力 |
| `browse`    | TUIブラウザとして起動 |
| `search`    | Web検索して結果をTUIで表示（`stillo search <query>`） |
| `qa`        | ページについてLLMに質問（LLMプロバイダー設定が必要） |
| `summarize` | ページをLLMで要約（LLMプロバイダー設定が必要） |
| `extract`   | 指定フィールドをLLMで抽出（LLMプロバイダー設定が必要） |
| `mcp`       | MCPサーバーとして起動（stdio transport、JSON-RPC 2.0） |

## SPA委譲

SPAと判定されたページは以下の優先順位で委譲を試みます（いずれかが成功すれば以降はスキップ）。

1. **Chrome CDP** — `localhost:9222` でChromeが起動していれば使用
2. **Playwright Daemon** — 自ユーザー所有のソケットが存在すれば使用（探索順: `STILLO_PLAYWRIGHT_SOCK` → `$XDG_RUNTIME_DIR/stillo-playwright.sock` → `/tmp/stillo-playwright.sock`）
3. **Jina Reader** — `JINA_API_KEY` が設定されているか `STILLO_ENABLE_JINA=1` のときのみ使用（閲覧URLを外部サービスへ送信するためオプトイン）
4. **Firecrawl** — `FIRECRAWL_URL` と `FIRECRAWL_API_KEY` が両方設定されていれば使用

委譲チェーン全体の所要時間は `--timeout` で打ち切られます。全ターゲットが失敗した場合は静的HTMLにフォールバックします。

### Chrome CDP を使う場合

```bash
# --remote-debugging-port=9222 でChromeを起動しておく
google-chrome --remote-debugging-port=9222 --headless
stillo dump https://your-spa-app.example.com
```

### Jina Reader を使う場合

```bash
export JINA_API_KEY=your_api_key   # APIキーなしの無料枠を使う場合は STILLO_ENABLE_JINA=1
stillo dump https://your-spa-app.example.com
```

### Firecrawl を使う場合

```bash
export FIRECRAWL_URL=https://api.firecrawl.dev/
export FIRECRAWL_API_KEY=your_api_key
stillo dump https://your-spa-app.example.com
```

## LLM連携

`qa`・`summarize`・`extract` サブコマンドはLLMにアクセスします。以下の環境変数で使用するプロバイダーを選択します（上から優先）。

| 優先度 | プロバイダー | 環境変数 |
|--------|-------------|----------|
| 1 | Anthropic | `ANTHROPIC_API_KEY` または `ANTHROPIC_BASE_URL`（モデル: `ANTHROPIC_MODEL`、デフォルト `claude-sonnet-4-5`）。`ANTHROPIC_BASE_URL` のみの場合は認証をプロキシ側で行う前提で、キーにはダミー値を送る |
| 2 | OpenAI | `OPENAI_API_KEY`（ベースURL: `OPENAI_BASE_URL`、モデル: `OPENAI_MODEL`、デフォルト `gpt-4o-mini`） |
| 3 | llama.cpp | `LLAMA_CPP_BASE_URL`（モデル: `LLAMA_CPP_MODEL`、デフォルト `default`、APIキー不要） |
| 4 | Ollama（ローカル） | `OLLAMA_BASE_URL`（例 `http://localhost:11434/`）、`OLLAMA_MODEL`（デフォルト `llama3`） |

いずれも設定されていない場合はエラーになります（Ollama への暗黙の接続は行いません）。ベースURLが不正な場合もエラーになります。OpenAI互換APIを持つ任意のプロバイダー（LM Studio等）は `OPENAI_BASE_URL` と `OPENAI_API_KEY` で利用できます。OpenAI互換のベースURLは `/v1` を含めても含めなくても構いません（`https://api.groq.com/openai/v1`、`http://proxy.local/llm` のいずれも可）。

ページ本文は `<document>` タグで区切ってLLMに渡し、本文中の指示に従わないよう system プロンプトで指定しています。出力が `max_tokens` に達して途中で切れた場合、`qa`・`summarize` は警告付きで部分出力を表示し、`extract` はエラーにします。

```bash
# Anthropic でQ&A
export ANTHROPIC_API_KEY=your_api_key
stillo qa "この記事の主張は何ですか？" https://example.com/article

# 要約
stillo summarize https://example.com/article

# フィールド抽出（カンマ区切りで複数指定）
stillo extract "title,author,published_at" https://example.com/article
stillo extract "title,author" --format json https://example.com/article

# Ollama でローカル実行
export OLLAMA_BASE_URL=http://localhost:11434/
stillo qa "記事の要点は？" https://example.com/article
```

## MCPサーバー

`stillo mcp` でstdio transportのMCPサーバーとして起動します。Claude DesktopやClaude Codeなど、MCPクライアントからstilloの機能を呼び出せます。

### 利用可能なツール

| ツール | 説明 |
|--------|------|
| `fetch_url` | URLを取得してMarkdown/テキスト/JSONで返す。`max_chars`（既定 50,000 文字）と `offset` でページング |
| `read_links` | URLからリンク一覧（テキスト・href）を抽出して返す。`limit`（既定 200 件）と `offset` でページング |
| `search_web` | Web検索して結果を返す（format=markdown\|links）。ボットブロック時はエラーを明示 |
| `extract_structured` | LLMを使って指定フィールドをJSONで抽出する（[LLM連携](#llm連携)のプロバイダー設定が必要）。JSONとして解釈できない出力・途中で切れた出力はエラー |

MCP経由のページ取得は静的HTTP取得のみで、`dump` コマンドのようなSPA委譲やフレーム展開は行いません。切り詰めた場合は出力に全体長と次の `offset` を明記します。リクエストは並行処理され、`notifications/cancelled` で実行中のツール呼び出しを中断できます。

`fetch_url`・`read_links`・`extract_structured` は取得先URLをLLMが決めるため、loopback・プライベートアドレス（RFC1918）・リンクローカル（169.254.0.0/16 等）・ULA への接続を既定で拒否します。判定はDNS解決後のIPとリダイレクトの各ホップに対して行います。拒否モードでは `HTTP_PROXY` 等のプロキシ設定を使いません。ローカルの開発サーバー等を読ませる場合は `STILLO_ALLOW_PRIVATE_NETWORK=1` を設定してください。URLスキームは http/https のみ受け付けます。CLI（`stillo dump` 等）にはこの制限はありません。

### Web検索バックエンド

`search_web` と `stillo search` はバックエンドチェーンで検索します。ボットブロック（DDGの202チャレンジ等）を検出した場合は空結果ではなくエラーを返し、次のバックエンドへフォールバックします。

| 環境変数 | 効果 |
|----------|------|
| `SEARXNG_URL` | SearXNG インスタンスを最優先バックエンドに追加（JSON API: `/search?format=json`） |
| `BRAVE_API_KEY` | Brave Search API（無料枠2000回/月）をバックエンドに追加 |
| `STILLO_SEARCH_BACKEND` | `ddg`\|`searxng`\|`brave` で単独バックエンド指定 |

DuckDuckGo HTML エンドポイントは常に末尾のフォールバックとして有効（ただし高頻度利用時はボットブロックされることがある）。

### Claude Codeでの設定例

```json
{
  "mcpServers": {
    "stillo": {
      "command": "/path/to/stillo",
      "args": ["mcp"]
    }
  }
}
```

## クレート構成

| クレート | 役割 |
|---------|------|
| `stillo-core`     | 純粋関数のみ（HTML解析・コンテンツ抽出・Markdown変換） |
| `stillo-fetcher`  | HTTP取得・SPA委譲 |
| `stillo-renderer` | TUI描画 |
| `stillo-llm`      | LLM API呼び出し（Anthropic / OpenAI互換 / Ollama） |
| `stillo-mcp`      | MCPサーバー（JSON-RPC 2.0 stdio transport） |
| `stillo` (cli)    | コマンドライン引数解析・全体オーケストレーション |

## 将来構想

### stilloネイティブモード

サーバとクライアントの主従を逆転させるコンセプト。従来のWebはサーバがHTMLでデザインごとコンテンツを送りつけるが、stilloネイティブモードではサーバはデータとセマンティクスのみを返し、表示・デザインはクライアントが全面的にコントロールする。

既存のWebへのフォールバックを維持しつつ、対応サーバにはよりリッチなセマンティクスを返させるオプトイン型の拡張として設計する想定。`stillo mcp` と組み合わせ、MCPサーバが変換層を担うことも視野に入れる。

## 開発

```bash
# ビルド
cargo build

# テスト
cargo test

# CDPサポートを有効にしてビルド
cargo build --features stillo-fetcher/cdp
```

ログレベルは `RUST_LOG` 環境変数で制御できます（stderrに出力）。

```bash
RUST_LOG=debug stillo dump https://example.com
```

## リリース手順

バージョン番号を上げて crates.io へ publish するには `bump-version.sh` を使います。

```bash
./scripts/bump-version.sh 0.2.0
```

このスクリプトは以下を一括実行します。

1. `Cargo.toml` のバージョンを書き換え
2. `cargo build` でビルド確認（`Cargo.lock` も更新）
3. `git commit`（`chore: bump version to X.Y.Z`）
4. `git tag vX.Y.Z` を作成
5. `git push origin main vX.Y.Z`

タグ push をトリガーに GitHub Actions（`.github/workflows/publish.yml`）が自動で起動し、以下を行います。

- `cargo test --workspace` でテスト
- 6クレートを依存順に crates.io へ publish
- GitHub Release を自動作成

### 事前準備

- リポジトリの Settings → Secrets → `CARGO_REGISTRY_TOKEN` に crates.io の API トークンを登録しておくこと
- 全クレートのオーナーに登録済みのアカウントのトークンを使用すること（`cargo owner --add <user> <crate>` で追加）
