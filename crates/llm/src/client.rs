use reqwest::Client;
use serde::Deserialize;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("API error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("No API key configured. Set ANTHROPIC_API_KEY or OPENAI_API_KEY.")]
    NoApiKey,
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

/// 利用可能な LLM プロバイダーを保持する enum。
/// enum ディスパッチにより async trait の複雑さを回避する。
pub enum LlmProvider {
    Anthropic(AnthropicClient),
    OpenAiCompat(OpenAiCompatClient),
}

impl LlmProvider {
    /// 環境変数から自動的にプロバイダーを選択する。
    /// ANTHROPIC_API_KEY → Anthropic
    /// ANTHROPIC_BASE_URL → Anthropic（カスタムエンドポイント、プロキシ経由の場合はキー不要）
    /// OPENAI_API_KEY    → OpenAI 互換
    /// LLAMA_CPP_BASE_URL → llama.cpp サーバー（OpenAI 互換、API キー不要）
    /// 未設定             → Ollama (localhost:11434)
    pub fn from_env() -> Result<Self, LlmError> {
        let base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok();

        if api_key.is_some() || base_url.is_some() {
            let key = api_key.unwrap_or_else(|| "proxy-managed".to_owned());
            let model =
                std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-5".to_owned());
            let url = base_url.unwrap_or_else(|| "https://api.anthropic.com".to_owned());
            return Ok(Self::Anthropic(AnthropicClient::new(key, model, url)));
        }

        if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
            let base_url = std::env::var("OPENAI_BASE_URL")
                .ok()
                .and_then(|u| u.parse().ok())
                .unwrap_or_else(|| "https://api.openai.com/".parse().unwrap());
            let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_owned());
            return Ok(Self::OpenAiCompat(OpenAiCompatClient::new(
                base_url,
                Some(api_key),
                model,
            )));
        }

        // llama.cpp サーバー: OpenAI 互換 API を持つがキー不要
        if let Ok(url_str) = std::env::var("LLAMA_CPP_BASE_URL") {
            let base_url = url_str
                .parse()
                .map_err(|_| LlmError::Http(format!("invalid LLAMA_CPP_BASE_URL: {}", url_str)))?;
            let model = std::env::var("LLAMA_CPP_MODEL").unwrap_or_else(|_| "default".to_owned());
            return Ok(Self::OpenAiCompat(OpenAiCompatClient::new(
                base_url, None, model,
            )));
        }

        // Ollama をローカルフォールバックとして試みる
        let base_url = std::env::var("OLLAMA_BASE_URL")
            .ok()
            .and_then(|u| u.parse().ok())
            .unwrap_or_else(|| "http://localhost:11434/".parse().unwrap());
        let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3".to_owned());
        Ok(Self::OpenAiCompat(OpenAiCompatClient::new(
            base_url, None, model,
        )))
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

        parse_anthropic_response(&text)
    }
}

fn parse_anthropic_response(text: &str) -> Result<String, LlmError> {
    #[derive(Deserialize)]
    struct Resp {
        content: Vec<ContentBlock>,
    }
    #[derive(Deserialize)]
    struct ContentBlock {
        #[serde(rename = "type")]
        kind: String,
        text: Option<String>,
    }

    let resp: Resp =
        serde_json::from_str(text).map_err(|e| LlmError::Parse(format!("{}: {}", e, text)))?;
    resp.content
        .into_iter()
        .find(|b| b.kind == "text")
        .and_then(|b| b.text)
        .ok_or_else(|| LlmError::Parse("no text content in response".into()))
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

        let endpoint = self
            .base_url
            .join("v1/chat/completions")
            .map_err(|e| LlmError::Http(e.to_string()))?;

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

        parse_openai_response(&text)
    }
}

fn parse_openai_response(text: &str) -> Result<String, LlmError> {
    #[derive(Deserialize)]
    struct Resp {
        choices: Vec<Choice>,
    }
    #[derive(Deserialize)]
    struct Choice {
        message: MsgContent,
    }
    #[derive(Deserialize)]
    struct MsgContent {
        content: String,
    }

    let resp: Resp =
        serde_json::from_str(text).map_err(|e| LlmError::Parse(format!("{}: {}", e, text)))?;
    resp.choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .ok_or_else(|| LlmError::Parse("no choices in response".into()))
}
