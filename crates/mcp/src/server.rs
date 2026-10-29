use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinError, JoinSet};

use crate::tools;

#[derive(Clone, Copy)]
pub struct McpServer;

impl McpServer {
    pub fn new() -> Self {
        Self
    }

    /// stdin から改行区切り JSON-RPC を読み、stdout へレスポンスを書く。
    pub async fn run_stdio(&self) -> Result<()> {
        let server = *self;
        serve(
            BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
            move |req| async move { server.handle(&req).await },
        )
        .await
    }

    async fn handle(&self, req: &Value) -> Option<Value> {
        let method = req["method"].as_str().unwrap_or("");

        tracing::debug!("MCP request: method={}", method);

        // JSON-RPC 2.0 は id を持たない notification への応答を禁止している。
        // 未知の notification（notifications/cancelled 等）に -32601 を返すと厳密なクライアントがプロトコルエラーにする
        let id = req.get("id").cloned()?;
        // クライアントからの response（method を持たず result/error を持つ）にも応答してはならない
        req.get("method")?;

        match method {
            "initialize" => Some(self.handle_initialize(id, req)),
            "tools/list" => Some(self.handle_tools_list(id)),
            "tools/call" => Some(self.handle_tools_call(id, req).await),
            "ping" => Some(ok_response(id, json!({}))),
            _ => Some(error_response(id, -32601, "Method not found")),
        }
    }

    fn handle_initialize(&self, id: Value, _req: &Value) -> Value {
        ok_response(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "stillo",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )
    }

    fn handle_tools_list(&self, id: Value) -> Value {
        ok_response(
            id,
            json!({
                "tools": [
                    {
                        "name": "fetch_url",
                        "description": format!(
                            "Fetch a URL with a plain HTTP GET and return its main content as Markdown, plain text, or JSON. \
                             JavaScript is not executed, SPAs are not delegated to a headless browser, and frames are not followed, \
                             so client-rendered pages may come back mostly empty. Output is limited to max_chars characters \
                             (default {}); when truncated, the output states the total length and the offset to pass to get the next part.",
                            tools::fetch_url::DEFAULT_MAX_CHARS
                        ),
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "url": { "type": "string", "description": "URL to fetch" },
                                "format": {
                                    "type": "string",
                                    "enum": ["markdown", "plain", "json"],
                                    "default": "markdown"
                                },
                                "max_chars": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "default": tools::fetch_url::DEFAULT_MAX_CHARS,
                                    "description": "Maximum number of characters to return (for format=json, applies to body_text)"
                                },
                                "offset": {
                                    "type": "integer",
                                    "minimum": 0,
                                    "default": 0,
                                    "description": "Character offset to start from; use the next offset reported in a truncated response"
                                }
                            },
                            "required": ["url"]
                        }
                    },
                    {
                        "name": "read_links",
                        "description": format!(
                            "Extract links from a URL (whole document, including navigation) with their anchor text. \
                             Returns a JSON object {{total, offset, returned, truncated, next_offset, links}}; \
                             at most limit links (default {}) are returned per call.",
                            tools::read_links::DEFAULT_LIMIT
                        ),
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "url": { "type": "string", "description": "URL to fetch" },
                                "limit": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "default": tools::read_links::DEFAULT_LIMIT,
                                    "description": "Maximum number of links to return"
                                },
                                "offset": {
                                    "type": "integer",
                                    "minimum": 0,
                                    "default": 0,
                                    "description": "Index of the first link to return; use next_offset from a truncated response"
                                }
                            },
                            "required": ["url"]
                        }
                    },
                    {
                        "name": "extract_structured",
                        "description": "Extract specific fields from a page as JSON using an LLM. The page is fetched with a plain HTTP GET (no JavaScript). \
                                        Requires an LLM provider configured on the server via ANTHROPIC_API_KEY, ANTHROPIC_BASE_URL, OPENAI_API_KEY, \
                                        LLAMA_CPP_BASE_URL or OLLAMA_BASE_URL. Returns an error if the LLM output is not valid JSON or was cut off.",
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
            }),
        )
    }

    async fn handle_tools_call(&self, id: Value, req: &Value) -> Value {
        let name = match req["params"]["name"].as_str() {
            Some(n) => n,
            None => return error_response(id, -32602, "missing tool name"),
        };
        let args = req["params"].get("arguments").unwrap_or(&Value::Null);

        // MCP 仕様では未知のツール名はツール実行エラー（isError）ではなくプロトコルエラー（-32602）とする
        let result = match name {
            "fetch_url" => tools::fetch_url::run(args).await,
            "read_links" => tools::read_links::run(args).await,
            "extract_structured" => tools::extract_structured::run(args).await,
            "search_web" => tools::search::run(args).await,
            _ => return error_response(id, -32602, &format!("Unknown tool: {}", name)),
        };

        match result {
            Ok(text) => ok_response(
                id,
                json!({
                    "content": [{ "type": "text", "text": text }]
                }),
            ),
            Err(e) => ok_response(
                id,
                json!({
                    "content": [{ "type": "text", "text": format!("Error: {}", e) }],
                    "isError": true
                }),
            ),
        }
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

/// 改行区切り JSON-RPC のサーバーループ。
///
/// 1 件ずつ await すると LLM 呼び出し（最大 120 秒）の間 ping や他のツール呼び出しが止まるため、
/// リクエストごとにタスクを spawn する。レスポンスは単一の writer タスクへ mpsc で渡し、
/// 1 レスポンス 1 行・都度 flush で書き出す（複数タスクが直接書くと行が混ざる）。
/// `notifications/cancelled` を受けたら該当リクエストのタスクを abort し、EOF では実行中タスクの完了を待って終了する。
/// `handler` を引数で受けるのはテストで任意の遅延・panic を注入するため
async fn serve<R, W, H, Fut>(reader: R, writer: W, handler: H) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
    H: Fn(Value) -> Fut,
    Fut: Future<Output = Option<Value>> + Send + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    let writer_task = tokio::spawn(write_loop(writer, rx));
    let mut tasks: JoinSet<()> = JoinSet::new();
    let mut in_flight = InFlight::default();
    // read_line は select! で中断されると読みかけのデータを失う。Lines::next_line は cancel safe
    let mut lines = reader.lines();

    loop {
        tokio::select! {
            // 完了タスクを読み取りと並行して回収し、panic したリクエストにも応答を返す
            Some(res) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                in_flight.reap(res, &tx);
            }
            line = lines.next_line() => {
                let Some(line) = line? else {
                    break; // EOF
                };
                dispatch(&line, &handler, &tx, &mut tasks, &mut in_flight);
            }
        }
        // writer が書き込みに失敗して終了した場合、以降の応答は届かないので読み取りを打ち切る
        if tx.is_closed() {
            tasks.abort_all();
            break;
        }
    }

    while let Some(res) = tasks.join_next_with_id().await {
        in_flight.reap(res, &tx);
    }
    drop(tx);
    writer_task.await?
}

/// 1 行を解釈し、リクエストならタスクを spawn、キャンセル通知なら該当タスクを abort する
fn dispatch<H, Fut>(
    line: &str,
    handler: &H,
    tx: &mpsc::UnboundedSender<Value>,
    tasks: &mut JoinSet<()>,
    in_flight: &mut InFlight,
) where
    H: Fn(Value) -> Fut,
    Fut: Future<Output = Option<Value>> + Send + 'static,
{
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let req = match serde_json::from_str::<Value>(trimmed) {
        Ok(req) => req,
        Err(e) => {
            tracing::warn!("failed to parse JSON-RPC request: {}", e);
            let _ = tx.send(error_response(Value::Null, -32700, "Parse error"));
            return;
        }
    };

    if req["method"] == "notifications/cancelled" {
        in_flight.cancel(&req["params"]["requestId"]);
        return;
    }
    // notification とクライアントからの response には応答しないため、タスクを作らない
    let Some(id) = req.get("id").cloned() else {
        return;
    };
    if req.get("method").is_none() {
        return;
    }

    let fut = handler(req);
    let tx = tx.clone();
    let handle = tasks.spawn(async move {
        if let Some(resp) = fut.await {
            // 受信側が閉じているのは writer が失敗した場合のみで、その時点で応答は届けられない
            let _ = tx.send(resp);
        }
    });
    in_flight.insert(id, handle);
}

/// 実行中リクエストの管理表。キャンセル通知は request id で、タスク完了は task id で届くため両方向に引けるようにする
#[derive(Default)]
struct InFlight {
    by_request: HashMap<String, AbortHandle>,
    by_task: HashMap<tokio::task::Id, Value>,
}

impl InFlight {
    /// JSON-RPC の id は数値と文字列のどちらも取りうるため、JSON 表現をキーにして 1 と "1" を区別する
    fn key(id: &Value) -> String {
        id.to_string()
    }

    fn insert(&mut self, id: Value, handle: AbortHandle) {
        self.by_task.insert(handle.id(), id.clone());
        self.by_request.insert(Self::key(&id), handle);
    }

    fn cancel(&mut self, request_id: &Value) {
        if let Some(handle) = self.by_request.remove(&Self::key(request_id)) {
            tracing::debug!("cancelling request {}", request_id);
            // 完了済みタスクへの abort は何もしない。MCP 仕様上キャンセル後の応答は送らなくてよい
            handle.abort();
        }
    }

    fn reap(
        &mut self,
        res: Result<(tokio::task::Id, ()), JoinError>,
        tx: &mpsc::UnboundedSender<Value>,
    ) {
        let (task_id, panicked) = match &res {
            Ok((task_id, ())) => (*task_id, false),
            Err(e) => (e.id(), e.is_panic()),
        };
        let Some(id) = self.by_task.remove(&task_id) else {
            return;
        };
        let key = Self::key(&id);
        // 同じ id が再利用された場合に後発のリクエストのエントリを消さないよう task id を照合する
        if self.by_request.get(&key).map(|h| h.id()) == Some(task_id) {
            self.by_request.remove(&key);
        }
        if panicked {
            tracing::error!("request {} panicked", id);
            // 応答がないとクライアントは永久に待ち続けるため、内部エラーとして返す
            let _ = tx.send(error_response(id, -32603, "Internal error"));
        }
    }
}

/// レスポンスを 1 行ずつ書き出す唯一の writer。
/// MCP クライアントは応答を行単位で待つため、バッファに溜めずに都度 flush する
async fn write_loop<W>(mut writer: W, mut rx: mpsc::UnboundedReceiver<Value>) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    while let Some(resp) = rx.recv().await {
        let mut buf = serde_json::to_vec(&resp)?;
        buf.push(b'\n');
        writer.write_all(&buf).await?;
        writer.flush().await?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{DuplexStream, Lines};
    use tokio::sync::Notify;
    use tokio::task::JoinHandle;

    #[tokio::test]
    async fn notifications_get_no_response() {
        let server = McpServer::new();
        for method in [
            "notifications/initialized",
            "notifications/cancelled",
            "unknown/notify",
        ] {
            let req = json!({"jsonrpc": "2.0", "method": method});
            assert!(server.handle(&req).await.is_none(), "{method}");
        }
    }

    #[tokio::test]
    async fn client_responses_get_no_response() {
        let server = McpServer::new();
        let req = json!({"jsonrpc": "2.0", "id": 7, "result": {}});
        assert!(server.handle(&req).await.is_none());
    }

    #[tokio::test]
    async fn unknown_request_still_returns_method_not_found() {
        let server = McpServer::new();
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "nope"});
        let resp = server.handle(&req).await.expect("request needs response");
        assert_eq!(resp["error"]["code"], -32601);
        assert_eq!(resp["id"], 1);
    }

    #[tokio::test]
    async fn unknown_tool_is_invalid_params_error() {
        let server = McpServer::new();
        let req = json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                         "params": {"name": "nope", "arguments": {}}});
        let resp = server.handle(&req).await.expect("request needs response");
        assert_eq!(resp["error"]["code"], -32602);
        assert!(resp.get("result").is_none());
    }

    #[tokio::test]
    async fn tools_list_documents_paging_and_no_spa_delegation() {
        let server = McpServer::new();
        let resp = server
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .await
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let tool = |name: &str| tools.iter().find(|t| t["name"] == name).unwrap().clone();
        let fetch = tool("fetch_url");
        assert!(!fetch["description"]
            .as_str()
            .unwrap()
            .contains("Handles SPAs"));
        assert_eq!(
            fetch["inputSchema"]["properties"]["max_chars"]["default"],
            50_000
        );
        assert!(fetch["inputSchema"]["properties"]["offset"].is_object());
        let links = tool("read_links");
        assert_eq!(links["inputSchema"]["properties"]["limit"]["default"], 200);
        assert!(links["inputSchema"]["properties"]["offset"].is_object());
    }

    /// テスト用ハンドラ: "slow" は gate が開くまで待ち、"boom" は panic し、それ以外は即応答する
    fn test_handler(
        gate: Arc<Notify>,
    ) -> impl Fn(Value) -> std::pin::Pin<Box<dyn Future<Output = Option<Value>> + Send>> {
        move |req: Value| {
            let gate = gate.clone();
            Box::pin(async move {
                let id = req.get("id").cloned()?;
                match req["method"].as_str() {
                    Some("slow") => {
                        gate.notified().await;
                        Some(ok_response(id, json!("slow")))
                    }
                    Some("sleepy") => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Some(ok_response(id, json!("sleepy")))
                    }
                    Some("boom") => panic!("boom"),
                    _ => Some(ok_response(id, json!("fast"))),
                }
            })
        }
    }

    struct Harness {
        input: DuplexStream,
        output: Lines<BufReader<DuplexStream>>,
        server: JoinHandle<Result<()>>,
    }

    impl Harness {
        fn start(gate: Arc<Notify>) -> Self {
            let (input, server_in) = tokio::io::duplex(64 * 1024);
            let (server_out, output) = tokio::io::duplex(64 * 1024);
            let server = tokio::spawn(serve(
                BufReader::new(server_in),
                server_out,
                test_handler(gate),
            ));
            Self {
                input,
                output: BufReader::new(output).lines(),
                server,
            }
        }

        async fn send(&mut self, v: Value) {
            let mut line = v.to_string();
            line.push('\n');
            self.input.write_all(line.as_bytes()).await.unwrap();
        }

        async fn recv(&mut self) -> Option<Value> {
            let line = tokio::time::timeout(Duration::from_secs(5), self.output.next_line())
                .await
                .expect("timed out waiting for response")
                .unwrap()?;
            Some(serde_json::from_str(&line).unwrap())
        }

        /// stdin の EOF に相当する。サーバーが終了するまで待つ
        async fn close(self) -> Lines<BufReader<DuplexStream>> {
            drop(self.input);
            tokio::time::timeout(Duration::from_secs(5), self.server)
                .await
                .expect("server did not finish after EOF")
                .unwrap()
                .unwrap();
            self.output
        }
    }

    fn req(id: i64, method: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method})
    }

    #[tokio::test]
    async fn slow_request_does_not_block_ping() {
        let gate = Arc::new(Notify::new());
        let mut h = Harness::start(gate.clone());
        h.send(req(1, "slow")).await;
        h.send(req(2, "ping")).await;

        let first = h.recv().await.unwrap();
        assert_eq!(first["id"], 2, "ping must not wait for the slow request");

        gate.notify_one();
        let second = h.recv().await.unwrap();
        assert_eq!(second["id"], 1);
        assert_eq!(second["result"], "slow");
        let _ = h.close().await;
    }

    #[tokio::test]
    async fn cancelled_request_is_aborted_without_response() {
        let gate = Arc::new(Notify::new());
        let mut h = Harness::start(gate);
        h.send(req(1, "slow")).await;
        h.send(
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                      "params": {"requestId": 1, "reason": "user"}}),
        )
        .await;
        h.send(req(2, "ping")).await;
        assert_eq!(h.recv().await.unwrap()["id"], 2);

        // gate は開かないので、abort されていなければ EOF 後の待機で close がタイムアウトする
        let mut output = h.close().await;
        assert!(output.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn eof_waits_for_in_flight_requests() {
        let mut h = Harness::start(Arc::new(Notify::new()));
        h.send(req(1, "sleepy")).await;
        let mut output = h.close().await;
        let line = output
            .next_line()
            .await
            .unwrap()
            .expect("response after EOF");
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"], "sleepy");
    }

    #[tokio::test]
    async fn panicking_request_gets_internal_error() {
        let mut h = Harness::start(Arc::new(Notify::new()));
        h.send(req(3, "boom")).await;
        let resp = h.recv().await.unwrap();
        assert_eq!(resp["id"], 3);
        assert_eq!(resp["error"]["code"], -32603);
        let _ = h.close().await;
    }

    #[tokio::test]
    async fn parse_error_and_notifications_are_handled_in_order() {
        let mut h = Harness::start(Arc::new(Notify::new()));
        h.input.write_all(b"{not json\n\n").await.unwrap();
        h.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        h.send(req(5, "ping")).await;
        let parse_err = h.recv().await.unwrap();
        assert_eq!(parse_err["error"]["code"], -32700);
        assert!(parse_err["id"].is_null());
        assert_eq!(h.recv().await.unwrap()["id"], 5);
        let _ = h.close().await;
    }
}
