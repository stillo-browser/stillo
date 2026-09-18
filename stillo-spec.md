# stillo 実装仕様書

**バージョン**: 0.1.15  
**言語**: Rust (edition 2021)  
**策定日**: 2026-05-14（最終更新: 2026-08-11）

---

## 1. プロジェクト概要

### 1.1 コンセプト

stillo は「意味抽出ブラウザ」として、以下の2軸を統合するCLIツールである。

- **軸1**: w3m互換のターミナルブラウジング体験
- **軸2**: LLMパイプラインのフロントエンド・MCPサーバー

JavaScript非対応を欠点として補うのではなく、「JSノイズを排除して構造化コンテンツを抽出できる」という差別化として位置づける。

### 1.2 設計原則

1. **パイプライン哲学**: `curl | jq` の延長として機能すること
2. **プロセス分離**: JS実行は外部委譲。コアはピュアHTTP+HTML処理に集中
3. **フォールバックチェーン**: 静的HTML → ローカルCDP → 外部API の優先順位付き委譲
4. **副作用の境界明示**: ドメインロジックとI/O（HTTP・ファイル・LLM API）を型レベルで分離
5. **AI-Friendly設計**: 型と命名でドメイン意図を表現し、Claude Codeとの協働を前提とする

---

## 2. アーキテクチャ

### 2.1 クレート構成

```
stillo/
├── Cargo.toml                  # workspace root
├── crates/
│   ├── cli/                    # エントリポイント・引数解析
│   │   └── src/
│   │       ├── main.rs
│   │       └── args.rs
│   ├── core/                   # ドメインロジック（副作用ゼロ）
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── ast.rs          # Document / Block / Inline AST型
│   │       ├── document.rs     # ドメイン型（RawHtml, ExtractedContent 等）
│   │       ├── extractor.rs    # コンテンツ抽出ロジック
│   │       ├── extractor/
│   │       │   ├── readability.rs
│   │       │   └── spa_detection.rs
│   │       ├── html_to_ast.rs  # HTML → Document 変換
│   │       ├── rss_to_ast.rs   # RSS/Atom/RDF → BrowsePage 変換
│   │       ├── markdown_to_ast.rs # Markdown → BrowsePage 変換
│   │       └── markdown.rs     # ExtractedContent → MarkdownDocument シリアライズ
│   ├── fetcher/                # HTTP取得レイヤー
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── http.rs         # reqwest ラッパー
│   │       └── spa/
│   │           ├── mod.rs      # SpaDelegationChain
│   │           ├── cdp.rs      # Chrome DevTools Protocol（cdp feature）
│   │           ├── playwright.rs
│   │           ├── jina.rs
│   │           └── firecrawl.rs
│   ├── renderer/               # TUI表示
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── tui.rs          # TuiBrowser メインループ
│   │       └── widgets/
│   │           ├── content_view.rs
│   │           ├── link_bar.rs
│   │           └── status_bar.rs
│   ├── llm/                    # LLMブリッジ
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── client.rs       # LlmProvider enum + AnthropicClient / OpenAiCompatClient
│   │       └── prompts.rs      # プロンプトテンプレート
│   └── mcp/                    # MCPサーバー
│       └── src/
│           ├── lib.rs
│           ├── server.rs       # stdio transport（JSON-RPC 2.0）
│           └── tools/
│               ├── mod.rs
│               ├── fetch_url.rs
│               ├── read_links.rs
│               └── extract_structured.rs
├── config/
│   └── default.toml            # 設定リファレンス（現時点では実行時に読み込まれない）
└── CLAUDE.md                   # AI協働ガイド
```

### 2.2 依存方向

```
cli → fetcher → core
cli → renderer → core
cli → llm → core
cli → mcp → fetcher → core
         └→ llm → core
```

`core` は他クレートに依存しない。副作用は `fetcher` / `llm` / `mcp` に閉じる。

---

## 3. ドメインモデル（`crates/core`）

### 3.1 ユビキタス言語

| 用語 | 定義 |
|------|------|
| `RawHtml` | HTTP取得した生のHTMLバイト列 |
| `ExtractedContent` | Readabilityロジックで抽出したメインコンテンツ |
| `MarkdownDocument` | LLMに渡すMarkdown文字列 |
| `Document` | HTML/RSS/Markdownから変換したセマンティックAST |
| `BrowsePage` | TUIに渡すフォーマット非依存のページ表現 |
| `DelegationTarget` | SPA描画委譲先の選択肢 |

### 3.2 コアドメイン型

```rust
// crates/core/src/document.rs

/// HTTP取得した生のHTML
#[derive(Debug, Clone)]
pub struct RawHtml {
    pub bytes: Vec<u8>,
    pub url: Url,
    pub content_type: String,
    pub status: u16,
}

/// 抽出済みコンテンツ（副作用ゼロの純粋データ）
#[derive(Debug, Clone)]
pub struct ExtractedContent {
    pub url: Url,
    pub title: String,
    pub byline: Option<String>,    // 著者・日付情報
    pub body_text: String,         // プレーンテキスト
    pub body_html: String,         // クリーンなHTML
    pub links: Vec<ExtractedLink>,
    pub metadata: PageMetadata,
}

#[derive(Debug, Clone)]
pub struct ExtractedLink {
    pub text: String,
    pub href: Url,
    pub rel: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PageMetadata {
    pub description: Option<String>,
    pub og_title: Option<String>,
    pub og_image: Option<String>,
    pub canonical: Option<Url>,
    pub published_at: Option<DateTime<Utc>>,
}

/// LLMに渡すMarkdown
#[derive(Debug, Clone)]
pub struct MarkdownDocument {
    pub content: String,
    pub source_url: Url,
    pub extracted_at: DateTime<Utc>,
}

/// フォーマット非依存のブラウズ用ページ表現。
/// HTML / RSS / Markdown など各入力から変換して TuiBrowser に渡す。
#[derive(Debug, Clone)]
pub struct BrowsePage {
    pub title: String,
    pub url: Url,
    pub doc: Document,
    pub links: Vec<ExtractedLink>,
    pub markdown: String,           // TUI の 'd' キー dump 用
}
```

### 3.3 セマンティックAST

```rust
// crates/core/src/ast.rs

/// ページのセマンティック構造を表す中間表現。
/// body_html → html_to_ast がビルドし、renderer が消費する。
#[derive(Debug, Clone, Default)]
pub struct Document {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone)]
pub enum Block {
    Heading { level: u8, inlines: Vec<Inline> },
    Paragraph(Vec<Inline>),
    ListItem { depth: usize, ordered: bool, number: usize, inlines: Vec<Inline> },
    CodeBlock { lang: Option<String>, content: String },
    Blockquote(Vec<Inline>),
    Rule,
}

#[derive(Debug, Clone)]
pub enum Inline {
    Text(String),
    Bold(String),
    Italic(String),
    BoldItalic(String),
    Code(String),
    Link { text: String, href: String },
    SoftBreak,
}
```

### 3.4 SPA検出と委譲の状態モデル

```rust
// crates/core/src/document.rs

/// SPA判定結果（網羅的列挙）
#[derive(Debug, Clone, PartialEq)]
pub enum SpaDetection {
    Static,
    SuspectedSpa { text_length: usize },
    FrameworkDetected { framework: JsFramework },
}

#[derive(Debug, Clone, PartialEq)]
pub enum JsFramework {
    React,
    Vue,
    Angular,
    Next,
    Nuxt,
    Unknown(String),
}

/// 委譲先（優先順位順）
#[derive(Debug, Clone, PartialEq)]
pub enum DelegationTarget {
    LocalCdp { port: u16 },
    PlaywrightDaemon { socket_path: PathBuf },
    JinaReader { api_key: Option<String> },
    Firecrawl { base_url: Url, api_key: String },
    Unavailable { reason: String },
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("HTTP error: {status} {url}")]
    Http { status: u16, url: Url },
    #[error("TLS error: {0}")]
    Tls(String),
    #[error("Timeout after {seconds}s")]
    Timeout { seconds: u64 },
    #[error("Delegation failed: {0}")]
    DelegationFailed(String),
    #[error("All delegation targets unavailable")]
    NoDelegationAvailable,
}
```

---

## 4. フェッチレイヤー（`crates/fetcher`）

### 4.1 HTTPクライアント

```rust
// crates/fetcher/src/http.rs

pub struct HttpConfig {
    pub timeout_secs: u64,          // default: 30
    pub follow_redirects: bool,     // default: true
    pub max_redirects: usize,       // default: 10
    pub user_agent: String,         // default: "stillo/0.1"
    pub accept_language: String,    // default: "ja,en;q=0.9"
    pub cookie_store: bool,         // default: true
}

pub struct HttpFetcher {
    client: Client,
}

impl HttpFetcher {
    pub fn new(config: HttpConfig) -> Self {
        let client = ClientBuilder::new()
            .use_rustls_tls()
            .redirect(redirect_policy)
            .cookie_store(config.cookie_store)
            .timeout(Duration::from_secs(config.timeout_secs))
            .user_agent(&config.user_agent)
            .build()
            .expect("failed to build HTTP client");
        Self { client }
    }

    pub async fn fetch(&self, url: &Url) -> Result<RawHtml, FetchError> { ... }
}
```

**採用クレート**:
- `reqwest` with `rustls-tls`, `cookies`, `json` features

### 4.2 SPA委譲チェーン

```rust
// crates/fetcher/src/spa/mod.rs

pub struct SpaDelegationChain {
    targets: Vec<DelegationTarget>,
    http: Client,
}

impl SpaDelegationChain {
    /// 環境変数とファイルシステムから利用可能なターゲットを構築する
    pub fn from_env(cdp_port: u16) -> Self { ... }

    /// 特定のターゲットのみを使うチェーンを構築する
    pub fn with_single_target(target: DelegationTarget) -> Self { ... }

    /// フォールバックチェーンを実行し、最初に成功したターゲットの結果を返す
    pub async fn fetch_with_js(&self, url: &Url) -> Result<RawHtml, FetchError> { ... }
}
```

**デフォルト優先順位**（`from_env` での構築順）:
1. `LocalCdp` — 常にリストに追加（到達確認は fetch 時）
2. `PlaywrightDaemon` — `/tmp/stillo-playwright.sock` が存在すれば追加
3. `JinaReader` — 常に追加（`JINA_API_KEY` があれば認証付き）
4. `Firecrawl` — `FIRECRAWL_URL` + `FIRECRAWL_API_KEY` が両方設定されていれば追加

全ターゲット失敗時は静的HTMLにフォールバック。

**CDPについて**: Chrome DevTools Protocol 実装は `cdp` feature フラグで有効化。`tokio-tungstenite` を使ったWebSocket接続で実装（`chromiumoxide` 等の外部クレートは使用しない）。

---

## 5. コンテンツ抽出エンジン（`crates/core/src/extractor.rs`）

```rust
pub struct ContentExtractor {
    config: ExtractorConfig,
}

pub struct ExtractorConfig {
    pub min_content_length: usize,       // default: 500
    pub noise_selectors: Vec<String>,
    pub preserve_links: bool,            // default: true
}

impl ContentExtractor {
    /// RawHtml → ExtractedContent（純粋関数）
    pub fn extract(&self, raw: &RawHtml) -> Result<ExtractedContent, ExtractionError> { ... }

    /// SPA判定（副作用ゼロ）
    pub fn detect_spa_for(&self, raw: &RawHtml) -> Result<SpaDetection, ExtractionError> { ... }

    /// frameset ページのフレームURL一覧を返す（空ならframeset非検出）
    pub fn detect_frames(&self, raw: &RawHtml) -> Vec<Url> { ... }
}
```

### 5.1 コンテンツ変換パイプライン

入力の Content-Type とボディ内容に応じてパイプラインを切り替える。

| 入力形式 | 変換先 | 実装 |
|----------|--------|------|
| HTML（デフォルト） | `ExtractedContent` → `Document` | `extractor.extract()` + `html_to_ast` |
| RSS / Atom / RDF | `BrowsePage` | `rss_to_ast` (`roxmltree`) |
| Markdown / plain text | `BrowsePage` | `markdown_to_ast` (`pulldown-cmark`) |

### 5.2 Readabilityロジック

Mozilla Readability.jsのRust移植として以下のアルゴリズムを実装。

1. **ノイズ除去**: `<nav>`, `<header>`, `<footer>`, `<aside>`, `class="sidebar"` 等を除去
2. **スコアリング**: `<p>` タグのテキスト密度、リンク密度比でノードをスコアリング
3. **本文抽出**: 最高スコアのコンテナノード以下をメインコンテンツとして採用
4. **クリーンアップ**: 残存する低スコアノードを除去

**CSSクラスのノイズ判定**: `val.contains(pattern)` ではなく、スペース・ハイフンで分解したコンポーネント単位の完全一致を使用（`"shadow-2xs"` が `"ad"` にヒットするような誤検出を防ぐため）。

---

## 6. Markdownシリアライザ（`crates/core/src/markdown.rs`）

```rust
pub struct MarkdownSerializer {
    config: MarkdownConfig,
}

pub struct MarkdownConfig {
    pub max_line_width: usize,      // default: 80
    pub include_links: bool,        // default: true
    pub include_images: bool,       // default: false
    pub heading_style: HeadingStyle,
}

#[derive(Debug, Clone)]
pub enum HeadingStyle {
    Atx,     // # H1, ## H2 ...（default）
    Setext,  // H1\n===
}

impl MarkdownSerializer {
    /// ExtractedContent → MarkdownDocument（純粋関数）
    pub fn serialize(&self, content: &ExtractedContent) -> MarkdownDocument { ... }
}
```

---

## 7. CLIインターフェース（`crates/cli`）

### 7.1 コマンド体系

```
stillo [OPTIONS] [URL]

SUBCOMMANDS:
  browse      TUIブラウザモード（デフォルト）
  search      Web検索して結果をTUIで表示
  dump        Markdown/テキストをstdoutに出力
  qa          ページについてLLMに質問
  summarize   ページを要約
  extract     指定した情報を抽出
  mcp         MCPサーバーとして起動（stdio）

OPTIONS:
  --format <FORMAT>     出力形式 [markdown|plain|json] (default: markdown)
  --delegate <TARGET>   SPA委譲先を明示 [auto|cdp|playwright|jina|firecrawl]
  --no-delegate         JS委譲を無効化（静的HTMLのみ）
  --timeout <SECS>      タイムアウト秒数 (default: 30)
  -v, --verbose         詳細ログ出力
```

### 7.2 Web検索

`stillo search <query>` と MCP `search_web`、TUI の `s` キーは共通の検索チェーン（`fetcher::search::web_search`）を使う。

**バックエンド優先順位**（`SearchBackend::from_env`）:

1. `STILLO_SEARCH_BACKEND=ddg|searxng|brave` — 単独指定時はその1つだけ
2. `SEARXNG_URL` が設定されていれば SearXNG（JSON API）
3. `BRAVE_API_KEY` が設定されていれば Brave Search API
4. DuckDuckGo HTML（`html.duckduckgo.com/html/`）— 常に末尾フォールバック

**ブロック検出**: `core::search::detect_blocked_page` がHTTPステータス（202/403/429）とHTMLマーカー（DDG anomaly / Cloudflare / Anubis）からボットブロックを検出する。ブロック時は空結果ではなくエラーを返し、次バックエンドへフォールバック。全滅時は各バックエンドの失敗理由をまとめた `SearchError::AllBlocked` を返す。

**責務分離**: パース（DDG/SearXNG/Brave HTML・JSON → `SearchResult`）は `core::search` の純粋関数、HTTP・バックエンド選択は `fetcher::search` が担う。

### 7.3 使用例

```bash
# TUIブラウズ
stillo https://example.com

# Markdown dump → llmへパイプ
stillo dump https://example.com | llm "要約して"

# QA
stillo qa "この記事の著者は誰？" https://example.com

# サマリー
stillo summarize https://example.com

# 構造化抽出（JSON出力）
stillo extract --format json "タイトル,著者,公開日" https://example.com

# MCPサーバーとして起動（Claude Code用）
stillo mcp
```

### 7.3 引数解析

`clap` v4（`derive` feature）を使用。

```rust
#[derive(Parser)]
#[command(name = "stillo", version, about = "AI-native terminal browser")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    pub url: Option<Url>,

    #[arg(long, default_value = "markdown", global = true)]
    pub format: OutputFormat,

    #[arg(long, global = true)]
    pub delegate: Option<DelegateTarget>,

    #[arg(long, global = true)]
    pub no_delegate: bool,

    #[arg(long, default_value = "30", global = true)]
    pub timeout: u64,

    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Command {
    Browse { url: Url },
    Dump { url: Url, #[arg(long)] format: Option<OutputFormat>, ... },
    Qa { question: String, url: Url },
    Summarize { url: Url },
    Extract { fields: String, url: Url, #[arg(long)] format: Option<OutputFormat> },
    Mcp,
}

#[derive(ValueEnum)]
pub enum DelegateTarget {
    Auto,       // SPA検出時に自動委譲チェーン
    Cdp,
    Playwright,
    Jina,
    Firecrawl,
}
```

---

## 8. TUIビューア（`crates/renderer`）

### 8.1 採用クレート

`ratatui` + `crossterm`

### 8.2 UIレイアウト

```
┌─────────────────────────────────────────────────────┐
│ stillo │ https://example.com             [q]uit [/]search │
├─────────────────────────────────────────────────────┤
│                                                     │
│  # Article Title                                    │
│                                                     │
│  Lorem ipsum dolor sit amet...                     │
│                                                     │
│  [1] Link to something                              │
│  [2] Another link                                   │
│                                                     │
├─────────────────────────────────────────────────────┤
│ [Enter]follow  [Tab]next-link  [d]dump  [U]url      │
└─────────────────────────────────────────────────────┘
```

### 8.3 キーバインド（w3m互換）

| キー | 動作 |
|------|------|
| `j` / `↓` | 下スクロール |
| `k` / `↑` | 上スクロール |
| マウスホイール | スクロール（3行/イベント） |
| `Enter` | リンクをフォロー |
| `Tab` | 次のリンクへ |
| `B` / `←` | 戻る（スクロール位置復元） |
| `F` / `→` | 進む |
| `r` | リロード（履歴に積まず再取得） |
| `U` | URLを直接入力（現在URLプリフィル） |
| `s` | Web検索（検索チェーン経由） |
| `/` | ページ内検索 |
| `n` / `N` | 次の/前の検索結果 |
| `d` | Markdown dump（stdout出力後終了） |
| `?` | ヘルプ |
| `q` / `Ctrl+C` | 終了 |

---

## 9. LLMブリッジ（`crates/llm`）

### 9.1 API抽象層

async trait の複雑さを避けるため、enum ディスパッチで実装する。

```rust
// crates/llm/src/client.rs

pub struct CompletionConfig {
    pub max_tokens: u32,    // default: 1024
    pub temperature: f32,   // default: 0.3（事実抽出向け）
}

/// 利用可能な LLM プロバイダーを保持する enum。
pub enum LlmProvider {
    Anthropic(AnthropicClient),
    OpenAiCompat(OpenAiCompatClient),
}

impl LlmProvider {
    /// 環境変数から自動的にプロバイダーを選択する。
    /// 優先順位: ANTHROPIC_API_KEY → OPENAI_API_KEY → LLAMA_CPP_BASE_URL → Ollama
    pub fn from_env() -> Result<Self, LlmError> { ... }

    pub async fn complete(
        &self,
        messages: Vec<Message>,
        config: &CompletionConfig,
    ) -> Result<String, LlmError> { ... }
}

pub struct AnthropicClient {
    api_key: String,
    model: String,    // ANTHROPIC_MODEL または "claude-sonnet-4-5"
    http: reqwest::Client,
}

/// OpenAI / Ollama / LM Studio / llama.cpp に対応する汎用クライアント
pub struct OpenAiCompatClient {
    base_url: Url,
    api_key: Option<String>,
    model: String,
    http: reqwest::Client,
}
```

### 9.2 LLMプロバイダー設定

| 優先度 | プロバイダー | 環境変数 |
|--------|-------------|----------|
| 1 | Anthropic | `ANTHROPIC_API_KEY`（モデル: `ANTHROPIC_MODEL`） |
| 2 | OpenAI互換 | `OPENAI_API_KEY`（ベースURL: `OPENAI_BASE_URL`、モデル: `OPENAI_MODEL`） |
| 3 | llama.cpp | `LLAMA_CPP_BASE_URL`（モデル: `LLAMA_CPP_MODEL`、APIキー不要） |
| 4 | Ollama | `OLLAMA_BASE_URL`（デフォルト `http://localhost:11434/`）、`OLLAMA_MODEL` |

### 9.3 プロンプトテンプレート

```rust
// crates/llm/src/prompts.rs

const MAX_CONTENT_CHARS: usize = 6000;

pub fn summarize_prompt(doc: &MarkdownDocument) -> Vec<Message> { ... }
pub fn qa_prompt(question: &str, doc: &MarkdownDocument) -> Vec<Message> { ... }
pub fn extract_prompt(fields: &str, doc: &MarkdownDocument) -> Vec<Message> { ... }
```

---

## 10. MCPサーバー（`crates/mcp`）

### 10.1 公開ツール定義

```json
{
  "tools": [
    {
      "name": "fetch_url",
      "description": "Fetch a URL and return its content as Markdown. Handles SPAs via delegation chain.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "url": { "type": "string", "description": "URL to fetch" },
          "format": {
            "type": "string",
            "enum": ["markdown", "plain", "json"],
            "default": "markdown"
          }
        },
        "required": ["url"]
      }
    },
    {
      "name": "read_links",
      "description": "Extract all links from a URL with their anchor text.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "url": { "type": "string" }
        },
        "required": ["url"]
      }
    },
    {
      "name": "extract_structured",
      "description": "Extract specific fields from a page as JSON using LLM. Requires ANTHROPIC_API_KEY or OPENAI_API_KEY.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "url": { "type": "string" },
          "fields": {
            "type": "array",
            "items": { "type": "string" },
            "description": "Field names to extract"
          }
        },
        "required": ["url", "fields"]
      }
    },
    {
      "name": "search_web",
      "description": "Search the web via DuckDuckGo and return results with title, URL, and snippet. Use format='links' for structured JSON suitable for follow-up fetch_url calls.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "query": { "type": "string", "description": "Search query" },
          "format": {
            "type": "string",
            "enum": ["markdown", "links"],
            "default": "markdown"
          }
        },
        "required": ["query"]
      }
    }
  ]
}
```

検索バックエンドの選択・ブロック検出・フォールバックは §7.2 に従う。ボットブロック時は `isError: true` のエラー応答を返す（空配列ではない）。

### 10.2 サーバー実装

ユニット構造体として実装。状態は持たず、各リクエストで依存オブジェクトを生成する。

```rust
// crates/mcp/src/server.rs

pub struct McpServer;

impl McpServer {
    pub fn new() -> Self { Self }

    /// stdin から改行区切り JSON-RPC を読み、stdout へレスポンスを書く。
    /// stdout への書き込みは毎回 flush する。
    pub async fn run_stdio(&self) -> Result<()> { ... }
}
```

MCP プロトコルバージョン: `2024-11-05`

### 10.3 起動方法（Claude Code設定）

```json
{
  "mcpServers": {
    "stillo": {
      "command": "stillo",
      "args": ["mcp"]
    }
  }
}
```

---

## 11. 設定ファイル

`config/default.toml` はデフォルト値のリファレンスとして管理する。現時点では実行時に読み込まれず、各設定値はハードコードされたデフォルトと環境変数で供給される。

```toml
# config/default.toml（リファレンス）

[http]
timeout_secs = 30
user_agent = "stillo/0.1"
cookie_store = true

[extractor]
min_content_length = 500
preserve_links = true

[markdown]
max_line_width = 80
include_links = true
include_images = false

[spa]
delegation_chain = ["cdp", "playwright", "jina"]

[spa.cdp]
port = 9222

[spa.playwright]
socket_path = "/tmp/stillo-playwright.sock"

[llm]
provider = "anthropic"
model = "claude-sonnet-4-5"
max_tokens = 1024
```

---

## 12. 採用クレート一覧

| カテゴリ | クレート | バージョン | 用途 |
|----------|----------|-----------|------|
| HTTP | `reqwest` | 0.12 | HTTPクライアント（rustls-tls, cookies, json features） |
| TLS | `rustls` | — | reqwest 経由で使用 |
| HTMLパース | `html5ever` | 0.27 | HTML5準拠パーサ |
| DOM操作 | `markup5ever_rcdom` | 0.3 | DOMツリー操作 |
| XMLパース | `roxmltree` | 0.20 | RSS/Atom/RDF フィードパース |
| Markdownパース | `pulldown-cmark` | 0.12 | Markdown入力のパース |
| 文字コード | `encoding_rs` | 0.8 | HTML文書の文字コード変換 |
| WebSocket | `tokio-tungstenite` | 0.26 | CDP接続（`cdp` feature 有効時のみ） |
| URL | `url` | 2 | URL型・バリデーション |
| 非同期 | `tokio` | 1 | async runtime（full features） |
| CLI | `clap` | 4 | 引数解析（derive feature） |
| TUI | `ratatui` | 0.28 | ターミナルUI |
| ターミナル | `crossterm` | 0.28 | クロスプラットフォームターミナル制御 |
| シリアライズ | `serde` + `serde_json` | 1 | JSON / 構造体変換 |
| エラー | `thiserror` | 2 | エラー型定義 |
| エラー伝播 | `anyhow` | 1 | アプリケーション層エラー |
| ログ | `tracing` + `tracing-subscriber` | 0.1 | 構造化ログ |
| 日時 | `chrono` | 0.4 | DateTime型 |

---

## 13. ディレクトリ構造（完全版）

```
stillo/
├── Cargo.toml
├── Cargo.lock
├── CLAUDE.md
├── README.md
├── stillo-spec.md
├── config/
│   └── default.toml
└── crates/
    ├── cli/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── main.rs
    │       └── args.rs
    ├── core/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── ast.rs
    │       ├── document.rs
    │       ├── extractor.rs
    │       ├── extractor/
    │       │   ├── readability.rs
    │       │   └── spa_detection.rs
    │       ├── html_to_ast.rs
    │       ├── rss_to_ast.rs
    │       ├── markdown_to_ast.rs
    │       └── markdown.rs
    ├── fetcher/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── http.rs
    │       └── spa/
    │           ├── mod.rs
    │           ├── cdp.rs
    │           ├── playwright.rs
    │           ├── jina.rs
    │           └── firecrawl.rs
    ├── renderer/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── tui.rs
    │       └── widgets/
    │           ├── content_view.rs
    │           ├── link_bar.rs
    │           └── status_bar.rs
    ├── llm/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── client.rs
    │       └── prompts.rs
    └── mcp/
        ├── Cargo.toml
        └── src/
            ├── lib.rs
            ├── server.rs
            └── tools/
                ├── mod.rs
                ├── fetch_url.rs
                ├── read_links.rs
                └── extract_structured.rs
```

---

## 14. 実装フェーズ

### Phase 1: コアパイプライン（完了）
- `core`: ドメイン型・抽出エンジン・Markdownシリアライザ
- `fetcher`: HTTP取得（静的HTMLのみ）
- `cli`: `dump` サブコマンド

### Phase 2: SPA委譲（完了）
- `fetcher/spa`: CDP / Playwright / Jina Reader / Firecrawl 実装
- SPA検出ロジック
- frameset ページ対応（コンテンツ量が最多のフレームを選択）

### Phase 3: TUIビューア（完了）
- `renderer`: ratatuiベースのTUI
- キーバインド・リンクナビゲーション・検索・URL入力

### Phase 4: LLMブリッジ（完了）
- `llm`: Anthropic / OpenAI互換 / llama.cpp / Ollama クライアント
- `cli`: `qa` / `summarize` / `extract` サブコマンド
- RSS/Atom/RDF フィード対応
- Markdown入力対応

### Phase 5: MCPサーバー（完了）
- `mcp`: stdio transport（JSON-RPC 2.0）
- ツール: `fetch_url` / `read_links` / `extract_structured` / `search_web`

### Phase 6: 検索チェーン・品質基盤（完了、2026-08-11）
- `core/search`: DDG/SearXNG/Brave パーサー + ボットブロック検出（純粋関数）
- `fetcher/search`: バックエンドフォールバックチェーン（SEARXNG_URL / BRAVE_API_KEY / STILLO_SEARCH_BACKEND）
- 単体テスト22件（readability回帰・JSON-LD・検索パース・ブロック検出）
- CI: push/PR で `cargo test` + `clippy -D warnings`（ci.yml）
