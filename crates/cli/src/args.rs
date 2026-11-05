use clap::{Parser, Subcommand, ValueEnum};
use url::Url;

#[derive(Parser, Debug)]
#[command(name = "stillo", version, about = "AI-native terminal browser")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// URLを直接指定した場合は browse（端末ではTUI、パイプ時は本文を出力）
    pub url: Option<Url>,

    /// 出力形式（サブコマンドが独自の --format を持つ場合はそちらが優先）
    #[arg(long, default_value = "markdown", global = true)]
    pub format: OutputFormat,

    /// 出力の最大文字数（超過分は切り詰める。0 で無効）
    #[arg(long, global = true)]
    pub max_chars: Option<usize>,

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
        /// 出力形式（既定はグローバルの --format）
        #[arg(long)]
        format: Option<OutputFormat>,
        /// 出力の最大文字数（既定はグローバルの --max-chars）
        #[arg(long)]
        max_chars: Option<usize>,
        /// SPA委譲先を明示指定（既定は auto）
        #[arg(long)]
        delegate: Option<DelegateTarget>,
        /// JS委譲を無効化し、静的HTMLのみ処理する
        #[arg(long)]
        no_delegate: bool,
    },
    /// TUIブラウザモード
    Browse {
        url: Url,
        /// 端末でない場合の出力形式
        #[arg(long)]
        format: Option<OutputFormat>,
        /// 端末でない場合の出力上限文字数
        #[arg(long)]
        max_chars: Option<usize>,
    },
    /// ページについてLLMに質問（LLMプロバイダーの環境変数が必要: ANTHROPIC_API_KEY / ANTHROPIC_BASE_URL / OPENAI_API_KEY / LLAMA_CPP_BASE_URL / OLLAMA_BASE_URL）
    Qa { question: String, url: Url },
    /// ページをLLMで要約（LLMプロバイダーの環境変数が必要: qa と同じ）
    Summarize { url: Url },
    /// 指定したフィールドをLLMで抽出（LLMプロバイダーの環境変数が必要: qa と同じ）
    Extract {
        fields: String,
        url: Url,
        #[arg(long)]
        format: Option<OutputFormat>,
    },
    /// Web検索して結果を表示（端末ではTUI、パイプ・リダイレクト時はテキスト出力）
    Search {
        /// 検索クエリ（1つ以上必要）
        #[arg(required = true, value_name = "QUERY")]
        query: Vec<String>,
        /// 端末でない場合の出力形式
        #[arg(long)]
        format: Option<OutputFormat>,
        /// 端末でない場合の出力上限文字数
        #[arg(long)]
        max_chars: Option<usize>,
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
    /// Playwright デーモン（要: /tmp/stillo-playwright.sock）
    Playwright,
    /// Jina Reader API
    Jina,
    /// Firecrawl API
    Firecrawl,
}
