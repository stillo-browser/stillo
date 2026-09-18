use std::io::Write;
use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::tools;

pub struct McpServer;

impl McpServer {
    pub fn new() -> Self {
        Self
    }

    /// stdin から改行区切り JSON-RPC を読み、stdout へレスポンスを書く。
    /// stdout への書き込みは毎回 flush する（バッファリングで詰まるのを防ぐ）。
    pub async fn run_stdio(&self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                break; // EOF
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match serde_json::from_str::<Value>(trimmed) {
                Ok(req) => {
                    let resp = self.handle(&req).await;
                    if let Some(resp) = resp {
                        self.write_response(&resp)?;
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to parse JSON-RPC request: {}", e);
                    let err = error_response(Value::Null, -32700, "Parse error");
                    self.write_response(&err)?;
                }
            }
        }
        Ok(())
    }

    fn write_response(&self, resp: &Value) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, resp)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
        Ok(())
    }

    async fn handle(&self, req: &Value) -> Option<Value> {
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req["method"].as_str().unwrap_or("");

        tracing::debug!("MCP request: method={}", method);

        match method {
            "initialize" => Some(self.handle_initialize(id, req)),
            "notifications/initialized" => None, // 通知のみ、レスポンス不要
            "tools/list" => Some(self.handle_tools_list(id)),
            "tools/call" => Some(self.handle_tools_call(id, req).await),
            "ping" => Some(ok_response(id, json!({}))),
            _ => Some(error_response(id, -32601, "Method not found")),
        }
    }

    fn handle_initialize(&self, id: Value, _req: &Value) -> Value {
        ok_response(id, json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "stillo",
                "version": env!("CARGO_PKG_VERSION")
            }
        }))
    }

    fn handle_tools_list(&self, id: Value) -> Value {
        ok_response(id, json!({
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
                            "url": { "type": "string", "description": "URL to fetch" }
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
                            "url": { "type": "string", "description": "URL to fetch" },
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
                                "default": "markdown",
                                "description": "'markdown' returns a readable list; 'links' returns a JSON array of {title, url, snippet, display_url}"
                            }
                        },
                        "required": ["query"]
                    }
                }
            ]
        }))
    }

    async fn handle_tools_call(&self, id: Value, req: &Value) -> Value {
        let name = match req["params"]["name"].as_str() {
            Some(n) => n,
            None => return error_response(id, -32602, "missing tool name"),
        };
        let args = req["params"].get("arguments").unwrap_or(&Value::Null);

        let result = match name {
            "fetch_url" => tools::fetch_url::run(args).await,
            "read_links" => tools::read_links::run(args).await,
            "extract_structured" => tools::extract_structured::run(args).await,
            "search_web" => tools::search::run(args).await,
            _ => Err(anyhow::anyhow!("unknown tool: {}", name)),
        };

        match result {
            Ok(text) => ok_response(id, json!({
                "content": [{ "type": "text", "text": text }]
            })),
            Err(e) => ok_response(id, json!({
                "content": [{ "type": "text", "text": format!("Error: {}", e) }],
                "isError": true
            })),
        }
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}
