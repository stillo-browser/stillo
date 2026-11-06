use reqwest::Client;
use serde::Deserialize;
use url::Url;

/// LLM API 呼び出し全体の上限。
/// 無期限に待つと応答しない LLM 1 件で CLI や MCP の該当リクエストが永久に返らなくなる。
/// max_tokens 分の生成が完了する程度の余裕を持たせた値にしている
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Ollama 等のローカルエンドポイントが起動していない場合に早く失敗させるための接続上限
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("API error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("No LLM provider configured. Set one of ANTHROPIC_API_KEY, ANTHROPIC_BASE_URL, OPENAI_API_KEY, LLAMA_CPP_BASE_URL or OLLAMA_BASE_URL.")]
    NoApiKey,
    /// 生成が max_tokens に達して打ち切られた。
    /// 抽出用途では途中で切れた JSON を正常結果として扱うと壊れたデータが下流に流れるため成功と区別する。
    /// 要約等で部分出力を使いたい呼び出し側のために `partial` を保持する
    #[error("LLM output was truncated at max_tokens={max_tokens}")]
    Truncated { max_tokens: u32, partial: String },
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompletionConfig {
    pub max_tokens: u32,
    pub temperature: f32,
}

impl Default for CompletionConfig {
    fn default() -> Self {
        Self {
            max_tokens: 1024,
            temperature: 0.3,
        }
    }
}

impl CompletionConfig {
    /// 構造化抽出用の設定。
    /// フィールド数が多いと JSON が 1024 トークンを超えて途中で切れるため上限を広げ、
    /// 同じ入力に対して同じ JSON を返すよう temperature を 0 にする
    pub fn extraction() -> Self {
        Self {
            max_tokens: 4096,
            temperature: 0.0,
        }
    }
}

/// 利用可能な LLM プロバイダーを保持する enum。
/// enum ディスパッチにより async trait の複雑さを回避する。
pub enum LlmProvider {
    Anthropic(AnthropicClient),
    OpenAiCompat(OpenAiCompatClient),
}

impl LlmProvider {
    /// 環境変数から自動的にプロバイダーを選択する（上から優先）。
    /// ANTHROPIC_API_KEY  → Anthropic
    /// ANTHROPIC_BASE_URL → Anthropic（カスタムエンドポイント、プロキシ経由の場合はキー不要）
    /// OPENAI_API_KEY     → OpenAI 互換
    /// LLAMA_CPP_BASE_URL → llama.cpp サーバー（OpenAI 互換、API キー不要）
    /// OLLAMA_BASE_URL    → Ollama（OpenAI 互換、API キー不要）
    /// いずれも未設定     → `LlmError::NoApiKey`
    pub fn from_env() -> Result<Self, LlmError> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// `from_env` の本体。環境変数の参照を引数で受けるのは、
    /// テストでプロセス全体の環境変数を書き換えると並列実行される他テストと競合するため
    fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, LlmError> {
        let base_url = var("ANTHROPIC_BASE_URL");
        let api_key = var("ANTHROPIC_API_KEY");

        if api_key.is_some() || base_url.is_some() {
            // ANTHROPIC_BASE_URL で Anthropic 互換 API のカスタムプロバイダを指定し、API キーを設定しない構成向けのフォールバック。
            // 認証が不要なプロバイダでも x-api-key フィールド自体は必須のものがあるため、ヘッダを省かずダミー値を送る。
            // プロバイダ側で無視される前提の値であり、秘密情報ではない
            let key = api_key.unwrap_or_else(|| "proxy-managed".to_owned());
            let model = var("ANTHROPIC_MODEL").unwrap_or_else(|| "claude-sonnet-4-5".to_owned());
            let url = base_url.unwrap_or_else(|| "https://api.anthropic.com".to_owned());
            return Ok(Self::Anthropic(AnthropicClient::new(key, model, url)));
        }

        if let Some(api_key) = var("OPENAI_API_KEY") {
            // 不正な OPENAI_BASE_URL を黙って api.openai.com に置き換えると、
            // 別プロバイダー向けのキーを OpenAI へ送ってしまうためエラーにする
            let base_url = match var("OPENAI_BASE_URL") {
                Some(u) => parse_base_url("OPENAI_BASE_URL", &u)?,
                None => "https://api.openai.com/"
                    .parse()
                    .expect("default URL is valid"),
            };
            let model = var("OPENAI_MODEL").unwrap_or_else(|| "gpt-4o-mini".to_owned());
            return Ok(Self::OpenAiCompat(OpenAiCompatClient::new(
                base_url,
                Some(api_key),
                model,
            )));
        }

        // llama.cpp サーバー: OpenAI 互換 API を持つがキー不要
        if let Some(url_str) = var("LLAMA_CPP_BASE_URL") {
            let base_url = parse_base_url("LLAMA_CPP_BASE_URL", &url_str)?;
            let model = var("LLAMA_CPP_MODEL").unwrap_or_else(|| "default".to_owned());
            return Ok(Self::OpenAiCompat(OpenAiCompatClient::new(
                base_url, None, model,
            )));
        }

        // 何も設定がないときに localhost:11434 へ暗黙に接続すると、
        // 設定漏れが「接続失敗」として報告され原因が分かりにくい。明示設定時のみ使う
        if let Some(url_str) = var("OLLAMA_BASE_URL") {
            let base_url = parse_base_url("OLLAMA_BASE_URL", &url_str)?;
            let model = var("OLLAMA_MODEL").unwrap_or_else(|| "llama3".to_owned());
            return Ok(Self::OpenAiCompat(OpenAiCompatClient::new(
                base_url, None, model,
            )));
        }

        Err(LlmError::NoApiKey)
    }

    pub async fn complete(
        &self,
        messages: Vec<Message>,
        config: &CompletionConfig,
    ) -> Result<String, LlmError> {
        match self {
            Self::Anthropic(c) => c.complete(messages, config).await,
            Self::OpenAiCompat(c) => c.complete(messages, config).await,
        }
    }
}

/// OpenAI 互換 API の chat completions エンドポイントを組み立てる。
///
/// `Url::join` は末尾スラッシュのない base URL の最後のセグメントを置き換えるため、
/// `http://proxy/llm` が `http://proxy/v1/chat/completions` になりパスが落ちる。常にパスを連結する。
/// OpenAI SDK の慣習（`https://api.openai.com/v1` のように /v1 まで含める）と、
/// 本ツールの従来の書き方（/v1 を含めない）の両方を受け付けるため、末尾が /v1 なら /v1 を重ねない
fn chat_completions_endpoint(base: &Url) -> Url {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    let suffix = if path.ends_with("/v1") {
        "/chat/completions"
    } else {
        "/v1/chat/completions"
    };
    url.set_path(&format!("{}{}", path, suffix));
    url
}

/// 環境変数で与えられたベース URL を解釈する。エラーに変数名を含めて設定箇所を特定できるようにする
fn parse_base_url(name: &str, value: &str) -> Result<Url, LlmError> {
    value
        .parse()
        .map_err(|_| LlmError::Http(format!("invalid {}: {}", name, value)))
}

// ── Anthropic ────────────────────────────────────────────────────────────────

pub struct AnthropicClient {
    api_key: String,
    model: String,
    base_url: String,
    http: Client,
}

impl AnthropicClient {
    pub fn new(api_key: String, model: String, base_url: String) -> Self {
        Self {
            api_key,
            model,
            base_url,
            http: Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    pub async fn complete(
        &self,
        messages: Vec<Message>,
        config: &CompletionConfig,
    ) -> Result<String, LlmError> {
        // Anthropic は system メッセージをトップレベルに分離する
        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.as_str())
            .unwrap_or("");
        let user_messages: Vec<_> = messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
            .collect();

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": config.max_tokens,
            // 抽出用途の temperature=0.0 を効かせるため Anthropic 経路でも明示的に送る
            "temperature": config.temperature,
            "system": system,
            "messages": user_messages,
        });

        let endpoint = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        if status >= 400 {
            return Err(LlmError::Api {
                status,
                message: text,
            });
        }

        parse_anthropic_response(&text, config.max_tokens)
    }
}

fn parse_anthropic_response(text: &str, max_tokens: u32) -> Result<String, LlmError> {
    #[derive(Deserialize)]
    struct Resp {
        content: Vec<ContentBlock>,
        stop_reason: Option<String>,
    }
    #[derive(Deserialize)]
    struct ContentBlock {
        #[serde(rename = "type")]
        kind: String,
        text: Option<String>,
    }

    let resp: Resp =
        serde_json::from_str(text).map_err(|e| LlmError::Parse(format!("{}: {}", e, text)))?;
    let truncated = resp.stop_reason.as_deref() == Some("max_tokens");
    let output = resp
        .content
        .into_iter()
        .find(|b| b.kind == "text")
        .and_then(|b| b.text);
    match (output, truncated) {
        (output, true) => Err(LlmError::Truncated {
            max_tokens,
            partial: output.unwrap_or_default(),
        }),
        (Some(output), false) => Ok(output),
        (None, false) => Err(LlmError::Parse("no text content in response".into())),
    }
}

// ── OpenAI 互換（OpenAI / Ollama / LM Studio）────────────────────────────────

pub struct OpenAiCompatClient {
    base_url: Url,
    api_key: Option<String>,
    model: String,
    http: Client,
}

impl OpenAiCompatClient {
    pub fn new(base_url: Url, api_key: Option<String>, model: String) -> Self {
        Self {
            base_url,
            api_key,
            model,
            http: Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(CONNECT_TIMEOUT)
                .use_rustls_tls()
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    pub async fn complete(
        &self,
        messages: Vec<Message>,
        config: &CompletionConfig,
    ) -> Result<String, LlmError> {
        let msgs: Vec<_> = messages
            .iter()
            .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
            .collect();

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": config.max_tokens,
            "temperature": config.temperature,
            "messages": msgs,
        });

        let endpoint = chat_completions_endpoint(&self.base_url);

        let mut req = self.http.post(endpoint.as_str()).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        if status >= 400 {
            return Err(LlmError::Api {
                status,
                message: text,
            });
        }

        parse_openai_response(&text, config.max_tokens)
    }
}

fn parse_openai_response(text: &str, max_tokens: u32) -> Result<String, LlmError> {
    #[derive(Deserialize)]
    struct Resp {
        choices: Vec<Choice>,
    }
    #[derive(Deserialize)]
    struct Choice {
        message: MsgContent,
        finish_reason: Option<String>,
    }
    #[derive(Deserialize)]
    struct MsgContent {
        // tool_calls のみの応答や拒否応答では content が null になる
        content: Option<String>,
        refusal: Option<String>,
    }

    let resp: Resp =
        serde_json::from_str(text).map_err(|e| LlmError::Parse(format!("{}: {}", e, text)))?;
    let choice = resp
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| LlmError::Parse("no choices in response".into()))?;
    let finish_reason = choice.finish_reason.as_deref();
    match (choice.message.content, choice.message.refusal) {
        (content, _) if finish_reason == Some("length") => Err(LlmError::Truncated {
            max_tokens,
            partial: content.unwrap_or_default(),
        }),
        (Some(content), _) => Ok(content),
        (None, Some(refusal)) => Err(LlmError::Parse(format!("model refused: {}", refusal))),
        (None, None) => Err(LlmError::Parse(format!(
            "no text content in response (finish_reason={})",
            finish_reason.unwrap_or("none")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn chat_completions_endpoint_keeps_base_path() {
        for (base, expected) in [
            (
                "https://api.openai.com/",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "https://api.openai.com",
                "https://api.openai.com/v1/chat/completions",
            ),
            // 末尾スラッシュなしのパスが Url::join で落ちていたケース
            (
                "http://proxy.local/llm",
                "http://proxy.local/llm/v1/chat/completions",
            ),
            (
                "http://proxy.local/llm/",
                "http://proxy.local/llm/v1/chat/completions",
            ),
            // OpenAI SDK の慣習どおり /v1 まで含めた base URL
            (
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "https://api.groq.com/openai/v1/",
                "https://api.groq.com/openai/v1/chat/completions",
            ),
            (
                "http://localhost:11434/",
                "http://localhost:11434/v1/chat/completions",
            ),
        ] {
            let url: Url = base.parse().unwrap();
            assert_eq!(chat_completions_endpoint(&url).as_str(), expected, "{base}");
        }
    }

    /// 1 リクエストだけ受けて固定レスポンスを返すローカル HTTP サーバー。
    /// 外部ネットワークに依存せずクライアントの送受信を検証するために使う。
    /// 戻り値の JoinHandle は受信したリクエストボディ（JSON）を返す
    async fn mock_server(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<serde_json::Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (header_end, content_length) = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed before headers");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                    let len = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map(|v| v.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    break (pos + 4, len);
                }
            };
            while buf.len() < header_end + content_length {
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed before body");
                buf.extend_from_slice(&chunk[..n]);
            }
            let resp = format!(
                "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status,
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            serde_json::from_slice(&buf[header_end..header_end + content_length]).unwrap()
        });
        (format!("http://{}/", addr), handle)
    }

    fn anthropic(base: String) -> AnthropicClient {
        AnthropicClient::new("k".into(), "m".into(), base)
    }

    fn openai(base: String) -> OpenAiCompatClient {
        OpenAiCompatClient::new(base.parse().unwrap(), None, "m".into())
    }

    fn msgs() -> Vec<Message> {
        vec![Message::system("sys"), Message::user("hi")]
    }

    #[tokio::test]
    async fn anthropic_sends_temperature() {
        let (base, req) = mock_server(
            200,
            r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}"#,
        )
        .await;
        let out = anthropic(base)
            .complete(msgs(), &CompletionConfig::extraction())
            .await
            .unwrap();
        assert_eq!(out, "ok");
        let body = req.await.unwrap();
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["system"], "sys");
    }

    #[tokio::test]
    async fn anthropic_max_tokens_stop_is_truncated() {
        let (base, _req) = mock_server(
            200,
            r#"{"content":[{"type":"text","text":"{\"a\": "}],"stop_reason":"max_tokens"}"#,
        )
        .await;
        let err = anthropic(base)
            .complete(msgs(), &CompletionConfig::default())
            .await
            .unwrap_err();
        match err {
            LlmError::Truncated {
                max_tokens,
                partial,
            } => {
                assert_eq!(max_tokens, 1024);
                assert_eq!(partial, "{\"a\": ");
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn openai_length_finish_is_truncated() {
        let (base, req) = mock_server(
            200,
            r#"{"choices":[{"message":{"content":"{\"a\""},"finish_reason":"length"}]}"#,
        )
        .await;
        let err = openai(base)
            .complete(msgs(), &CompletionConfig::extraction())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                LlmError::Truncated {
                    max_tokens: 4096,
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(req.await.unwrap()["temperature"], 0.0);
    }

    #[tokio::test]
    async fn openai_null_content_is_parse_error_not_panic() {
        let (base, _req) = mock_server(
            200,
            r#"{"choices":[{"message":{"content":null,"tool_calls":[]},"finish_reason":"tool_calls"}]}"#,
        )
        .await;
        let err = openai(base)
            .complete(msgs(), &CompletionConfig::default())
            .await
            .unwrap_err();
        match err {
            LlmError::Parse(m) => assert!(m.contains("tool_calls"), "{m}"),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn openai_refusal_is_reported() {
        let (base, _req) = mock_server(
            200,
            r#"{"choices":[{"message":{"content":null,"refusal":"no"},"finish_reason":"stop"}]}"#,
        )
        .await;
        let err = openai(base)
            .complete(msgs(), &CompletionConfig::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refused"), "{err}");
    }

    #[tokio::test]
    async fn openai_normal_response() {
        let (base, _req) = mock_server(
            200,
            r#"{"choices":[{"message":{"content":"hello"},"finish_reason":"stop"}]}"#,
        )
        .await;
        let out = openai(base)
            .complete(msgs(), &CompletionConfig::default())
            .await
            .unwrap();
        assert_eq!(out, "hello");
    }

    fn provider_from(pairs: &[(&str, &str)]) -> Result<LlmProvider, LlmError> {
        let vars: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        LlmProvider::from_vars(|k| vars.get(k).cloned())
    }

    #[test]
    fn from_env_without_any_config_is_no_api_key() {
        assert!(matches!(provider_from(&[]), Err(LlmError::NoApiKey)));
        // モデル名だけでは接続先が決まらないので選択しない
        assert!(matches!(
            provider_from(&[("OLLAMA_MODEL", "llama3")]),
            Err(LlmError::NoApiKey)
        ));
    }

    #[test]
    fn from_env_uses_ollama_only_when_base_url_set() {
        let p = provider_from(&[("OLLAMA_BASE_URL", "http://localhost:11434/")]).unwrap();
        match p {
            LlmProvider::OpenAiCompat(c) => {
                assert_eq!(c.base_url.as_str(), "http://localhost:11434/");
                assert!(c.api_key.is_none());
            }
            LlmProvider::Anthropic(_) => panic!("expected OpenAI compat"),
        }
    }

    #[test]
    fn from_env_rejects_invalid_base_urls() {
        assert!(matches!(
            provider_from(&[("OLLAMA_BASE_URL", "not a url")]),
            Err(LlmError::Http(_))
        ));
        assert!(matches!(
            provider_from(&[("OPENAI_API_KEY", "k"), ("OPENAI_BASE_URL", "::bad")]),
            Err(LlmError::Http(_))
        ));
    }

    #[test]
    fn from_env_anthropic_base_url_only_uses_placeholder_key() {
        let p = provider_from(&[("ANTHROPIC_BASE_URL", "http://proxy.local")]).unwrap();
        match p {
            LlmProvider::Anthropic(c) => {
                assert_eq!(c.api_key, "proxy-managed");
                assert_eq!(c.base_url, "http://proxy.local");
            }
            LlmProvider::OpenAiCompat(_) => panic!("expected Anthropic"),
        }
    }

    #[test]
    fn from_env_priority_prefers_anthropic_then_openai() {
        let p = provider_from(&[
            ("OPENAI_API_KEY", "o"),
            ("OLLAMA_BASE_URL", "http://localhost:11434/"),
        ])
        .unwrap();
        match p {
            LlmProvider::OpenAiCompat(c) => assert_eq!(c.api_key.as_deref(), Some("o")),
            LlmProvider::Anthropic(_) => panic!("expected OpenAI"),
        }
        let p = provider_from(&[("ANTHROPIC_API_KEY", "a"), ("OPENAI_API_KEY", "o")]).unwrap();
        assert!(matches!(p, LlmProvider::Anthropic(_)));
    }
}
