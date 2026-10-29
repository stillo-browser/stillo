use clap::{Parser, Subcommand, ValueEnum};
use url::Url;

#[derive(Parser, Debug)]
#[command(name = "stillo", version, about = "AI-native terminal browser")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// URLを直接指定した場合は dump として動作
    pub url: Option<Url>,

    /// 出力形式
    #[arg(long, default_value = "markdown", global = true)]
    pub format: OutputFormat,

    /// タイムアウト秒数
    #[arg(long, default_value = "30", global = true)]
    pub timeout: u64,

    /// SPA委譲先を明示指定（デフォルトは auto でSPA検出時に自動委譲）
    #[arg(long, global = true)]
    pub delegate: Option<DelegateTarget>,

    /// JS委譲を無効化し、静的HTMLのみ処理する
    #[arg(long, global = true)]
    pub no_delegate: bool,

    /// 詳細ログ出力
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// URLのコンテンツをMarkdown/テキストとして stdout に出力
    Dump {
        url: Url,
        #[arg(long)]
        format: Option<OutputFormat>,
        #[arg(long)]
        delegate: Option<DelegateTarget>,
        #[arg(long)]
        no_delegate: bool,
    },
    /// TUIブラウザモード
    Browse { url: Url },
    /// ページについてLLMに質問（ANTHROPIC_API_KEY / OPENAI_API_KEY 環境変数が必要）
    Qa { question: String, url: Url },
    /// ページをLLMで要約（ANTHROPIC_API_KEY / OPENAI_API_KEY 環境変数が必要）
    Summarize { url: Url },
    /// 指定したフィールドをLLMで抽出（ANTHROPIC_API_KEY / OPENAI_API_KEY 環境変数が必要）
    Extract {
        fields: String,
        url: Url,
        #[arg(long)]
        format: Option<OutputFormat>,
    },
    /// DuckDuckGoでWeb検索してTUIブラウザで結果を表示
    Search {
        /// 検索クエリ
        query: Vec<String>,
    },
    /// MCPサーバーとして起動（stdio transport、JSON-RPC 2.0）
    Mcp,
}

#[derive(ValueEnum, Clone, Debug, Default)]
pub enum OutputFormat {
    #[default]
    Markdown,
    Plain,
    Json,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum DelegateTarget {
    /// SPA検出時に自動的に委譲チェーンを試みる
    Auto,
    /// Chrome DevTools Protocol（要: Chrome起動済み）
    Cdp,
    /// Playwright デーモン（Phase 3以降）
    Playwright,
    /// Jina Reader API
    Jina,
    /// Firecrawl API
    Firecrawl,
}
