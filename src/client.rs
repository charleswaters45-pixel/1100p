use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use serde_json::{json, Value};
use tokio::{
    sync::{mpsc, oneshot, Mutex},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    transport::{self, Sender},
    types::{
        parse_incoming, IncomingMessage, ProgressUpdate, Prompt, Resource,
        RpcNotification, RpcRequest, ServerCapabilities, Tool, ToolEvent, ToolResult,
        PROTOCOL_VERSION,
    },
    Error, Result,
};

// ── Pending request registry ──────────────────────────────────────────────────

struct PendingResponse {
    result_tx: oneshot::Sender<Result<Value>>,
    /// Present when the caller requested progress events.
    progress_tx: Option<mpsc::Sender<ProgressUpdate>>,
}

impl std::fmt::Debug for PendingResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingResponse")
            .field("has_progress", &self.progress_tx.is_some())
            .finish()
    }
}

type PendingMap = Arc<Mutex<HashMap<u64, PendingResponse>>>;

// ── McpClient ────────────────────────────────────────────────────────────────

/// An async MCP client. Clone-cheap: all state is behind `Arc`.
#[derive(Clone, Debug)]
pub struct McpClient {
    sender: Arc<Sender>,
    next_id: Arc<AtomicU64>,
    pending: PendingMap,
    pub capabilities: Arc<ServerCapabilities>,
}

impl McpClient {
    // ── Constructors ──────────────────────────────────────────────────────────

    /// Spawn a local process and connect over stdio.
    #[cfg(feature = "stdio")]
    pub async fn stdio(program: &str, args: &[&str]) -> Result<Self> {
        let (sender, receiver) = transport::connect_stdio(program, args).await?;
        Self::handshake(sender, receiver).await
    }

    /// Connect to a remote MCP server over HTTP/SSE.
    #[cfg(feature = "http")]
    pub async fn http(sse_url: &str) -> Result<Self> {
        let (sender, receiver) = transport::connect_http(sse_url).await?;
        Self::handshake(sender, receiver).await
    }

    // ── Tools ─────────────────────────────────────────────────────────────────

    /// List all tools the server exposes.
    pub async fn list_tools(&self) -> Result<Vec<Tool>> {
        let result = self.request("tools/list", None).await?;
        let tools = result
            .get("tools")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        Ok(serde_json::from_value(tools)?)
    }

    /// Call a tool and wait for the final result (no progress events).
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<ToolResult> {
        let result = self
            .request("tools/call", Some(json!({ "name": name, "arguments": args })))
            .await?;
        ToolResult::from_value(result)
    }

    /// Call a tool and stream progress events as they arrive.
    /// The final `ToolEvent::Result` closes the stream.
    pub async fn call_tool_streaming(
        &self,
        name: &str,
        args: Value,
    ) -> Result<impl futures_core::Stream<Item = Result<ToolEvent>>> {
        let id = self.next_id();
        let (result_tx, result_rx) = oneshot::channel();
        let (progress_tx, progress_rx) = mpsc::channel(32);

        self.pending.lock().await.insert(
            id,
            PendingResponse {
                result_tx,
                progress_tx: Some(progress_tx),
            },
        );

        self.send_request(
            id,
            "tools/call",
            Some(json!({
                "name": name,
                "arguments": args,
                "_meta": { "progressToken": id }
            })),
        )
        .await?;

        Ok(ToolStream { progress_rx, result_rx: Some(result_rx) })
    }

    /// Call a tool with a [`CancellationToken`].
    /// Sends `notifications/cancelled` on the MCP channel when the token fires.
    pub async fn call_tool_cancellable(
        &self,
        name: &str,
        args: Value,
        token: CancellationToken,
    ) -> Result<ToolResult> {
        let id = self.next_id();
        let (result_tx, result_rx) = oneshot::channel();

        self.pending.lock().await.insert(
            id,
            PendingResponse { result_tx, progress_tx: None },
        );

        self.send_request(
            id,
            "tools/call",
            Some(json!({ "name": name, "arguments": args })),
        )
        .await?;

        tokio::select! {
            res = result_rx => {
                res.map_err(|_| Error::Closed)?.map(|v| ToolResult::from_value(v))?
            }
            _ = token.cancelled() => {
                self.pending.lock().await.remove(&id);
                // Notify the server per MCP spec.
                let notif = RpcNotification::new(
                    "notifications/cancelled",
                    Some(json!({ "requestId": id, "reason": "client cancelled" })),
                );
                let _ = self.sender.send_raw(serde_json::to_string(&notif)?).await;
                Err(Error::Cancelled)
            }
        }
    }

    // ── Resources ─────────────────────────────────────────────────────────────

    pub async fn list_resources(&self) -> Result<Vec<Resource>> {
        let result = self.request("resources/list", None).await?;
        let resources = result
            .get("resources")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        Ok(serde_json::from_value(resources)?)
    }

    pub async fn read_resource(&self, uri: &str) -> Result<Value> {
        self.request("resources/read", Some(json!({ "uri": uri }))).await
    }

    // ── Prompts ───────────────────────────────────────────────────────────────

    pub async fn list_prompts(&self) -> Result<Vec<Prompt>> {
        let result = self.request("prompts/list", None).await?;
        let prompts = result
            .get("prompts")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        Ok(serde_json::from_value(prompts)?)
    }

    pub async fn get_prompt(&self, name: &str, args: Option<Value>) -> Result<Value> {
        let mut params = json!({ "name": name });
        if let Some(a) = args {
            params["arguments"] = a;
        }
        self.request("prompts/get", Some(params)).await
    }

    // ── Raw request ───────────────────────────────────────────────────────────

    /// Send any JSON-RPC request and await the raw result value.
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id();
        let (result_tx, result_rx) = oneshot::channel();

        self.pending.lock().await.insert(
            id,
            PendingResponse { result_tx, progress_tx: None },
        );

        self.send_request(id, method, params).await?;

        result_rx.await.map_err(|_| Error::Closed)?
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    async fn send_request(&self, id: u64, method: &str, params: Option<Value>) -> Result<()> {
        let msg = serde_json::to_string(&RpcRequest::new(id, method, params))?;
        self.sender.send_raw(msg).await
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) async fn handshake(
        sender: Sender,
        mut receiver: transport::Receiver,
    ) -> Result<Self> {
        let sender = Arc::new(sender);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let next_id = Arc::new(AtomicU64::new(1));

        // Send initialize.
        let init_id = next_id.fetch_add(1, Ordering::Relaxed);
        let init_req = RpcRequest::new(
            init_id,
            "initialize",
            Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "mcp-client", "version": env!("CARGO_PKG_VERSION") }
            })),
        );
        sender.send_raw(serde_json::to_string(&init_req)?).await?;

        // Wait for the initialize response (may arrive before the reader task starts).
        let caps = loop {
            let raw = receiver.recv().await?.ok_or(Error::Closed)?;
            match parse_incoming(&raw)? {
                IncomingMessage::Response(resp) if resp.id == init_id => {
                    if let Some(err) = resp.error {
                        return Err(Error::Rpc { code: err.code, message: err.message });
                    }
                    let caps: ServerCapabilities = resp
                        .result
                        .as_ref()
                        .and_then(|v| v.get("capabilities"))
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    break caps;
                }
                // Ignore any notifications that arrive before the response.
                _ => continue,
            }
        };

        // Send initialized notification.
        let notif = RpcNotification::new("notifications/initialized", None);
        sender.send_raw(serde_json::to_string(&notif)?).await?;

        let client = McpClient {
            sender,
            next_id,
            pending: Arc::clone(&pending),
            capabilities: Arc::new(caps),
        };

        // Spawn background reader.
        spawn_reader(receiver, pending);

        Ok(client)
    }
}

// ── Background reader task ────────────────────────────────────────────────────

fn spawn_reader(
    mut receiver: transport::Receiver,
    pending: PendingMap,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let raw = match receiver.recv().await {
                Ok(Some(r)) => r,
                Ok(None) | Err(_) => break,
            };

            let msg = match parse_incoming(&raw) {
                Ok(m) => m,
                Err(_) => continue,
            };

            match msg {
                IncomingMessage::Response(resp) => {
                    let mut map = pending.lock().await;
                    if let Some(pending) = map.remove(&resp.id) {
                        let payload = match resp.error {
                            Some(e) => Err(Error::Rpc { code: e.code, message: e.message }),
                            None => Ok(resp.result.unwrap_or(Value::Null)),
                        };
                        let _ = pending.result_tx.send(payload);
                    }
                }

                IncomingMessage::Notification(notif) => {
                    if notif.method == "notifications/progress" {
                        if let Some(params) = notif.params {
                            route_progress(&pending, params).await;
                        }
                    }
                    // Other notifications (logs, resource changes) are currently
                    // silently dropped. Add a broadcast channel here if needed.
                }
            }
        }
    })
}

async fn route_progress(pending: &PendingMap, params: Value) {
    let token = match params.get("progressToken").and_then(|t| t.as_u64()) {
        Some(t) => t,
        None => return,
    };

    let map = pending.lock().await;
    if let Some(entry) = map.get(&token) {
        if let Some(tx) = &entry.progress_tx {
            let update = ProgressUpdate {
                progress: params.get("progress").and_then(|v| v.as_f64()).unwrap_or(0.0),
                total: params.get("total").and_then(|v| v.as_f64()),
                message: params
                    .get("message")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
            };
            let _ = tx.send(update).await;
        }
    }
}

// ── ToolStream ────────────────────────────────────────────────────────────────

struct ToolStream {
    progress_rx: mpsc::Receiver<ProgressUpdate>,
    result_rx: Option<oneshot::Receiver<Result<Value>>>,
}

impl futures_core::Stream for ToolStream {
    type Item = Result<ToolEvent>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::future::Future as _;
        use std::task::Poll;

        // Drain progress messages first (non-blocking check).
        if let Poll::Ready(Some(update)) = self.progress_rx.poll_recv(cx) {
            return Poll::Ready(Some(Ok(ToolEvent::Progress(update))));
        }

        // Then check for the final result.
        if let Some(result_rx) = self.result_rx.as_mut() {
            match std::pin::Pin::new(result_rx).poll(cx) {
                Poll::Ready(Ok(Ok(value))) => {
                    self.result_rx = None;
                    match ToolResult::from_value(value) {
                        Ok(r) => return Poll::Ready(Some(Ok(ToolEvent::Result(r)))),
                        Err(e) => return Poll::Ready(Some(Err(e))),
                    }
                }
                Poll::Ready(Ok(Err(e))) => {
                    self.result_rx = None;
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(Err(_)) => {
                    self.result_rx = None;
                    return Poll::Ready(Some(Err(Error::Closed)));
                }
                Poll::Pending => {}
            }
        } else {
            // result_rx consumed and progress drained — stream is done.
            return Poll::Ready(None);
        }

        Poll::Pending
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::connect_mock;
    use futures::StreamExt;
    use serde_json::json;

    /// Builds a connected `McpClient` backed by a mock transport and returns
    /// the `ServerHandle` so individual tests can drive server behaviour.
    async fn mock_client() -> (McpClient, crate::transport::mock::ServerHandle) {
        let (sender, receiver, mut server) = connect_mock();

        // The handshake runs concurrently with the server task.
        let (done_tx, done_rx) = oneshot::channel();
        tokio::spawn(async move {
            server.complete_handshake().await;
            let _ = done_tx.send(server);
        });

        let client = McpClient::handshake(sender, receiver).await.unwrap();
        let server = done_rx.await.unwrap();
        (client, server)
    }

    // ── Handshake ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_handshake_capabilities_parsed() {
        let (_, sender, receiver, mut server) = {
            let (s, r, h) = connect_mock();
            ((), s, r, h)
        };
        let (done_tx, done_rx) = oneshot::channel();
        tokio::spawn(async move {
            let init = server.recv_json().await;
            server
                .send_json(json!({
                    "jsonrpc": "2.0",
                    "id": init["id"],
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {}, "resources": {} },
                        "serverInfo": { "name": "cap-test", "version": "1.0.0" }
                    }
                }))
                .await;
            let _ = server.recv_json().await; // notifications/initialized
            let _ = done_tx.send(());
        });

        let client = McpClient::handshake(sender, receiver).await.unwrap();
        done_rx.await.unwrap();

        assert!(client.capabilities.tools.is_some());
        assert!(client.capabilities.resources.is_some());
        assert!(client.capabilities.prompts.is_none());
    }

    #[tokio::test]
    async fn test_handshake_server_error_propagates() {
        let (sender, receiver, mut server) = connect_mock();
        tokio::spawn(async move {
            let init = server.recv_json().await;
            server
                .respond_err(init["id"].as_u64().unwrap(), -32600, "bad version")
                .await;
        });
        let err = McpClient::handshake(sender, receiver).await.unwrap_err();
        assert!(matches!(err, Error::Rpc { .. }));
    }

    // ── list_tools ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_list_tools_returns_all_tools() {
        let (client, mut server) = mock_client().await;
        tokio::spawn(async move {
            let req = server.recv_json().await;
            assert_eq!(req["method"], "tools/list");
            server
                .respond(
                    req["id"].as_u64().unwrap(),
                    json!({
                        "tools": [
                            { "name": "echo",   "inputSchema": { "type": "object" } },
                            { "name": "search", "inputSchema": { "type": "object" } }
                        ]
                    }),
                )
                .await;
        });

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[1].name, "search");
    }

    #[tokio::test]
    async fn test_list_tools_empty_server() {
        let (client, mut server) = mock_client().await;
        tokio::spawn(async move {
            let req = server.recv_json().await;
            server.respond(req["id"].as_u64().unwrap(), json!({ "tools": [] })).await;
        });
        assert!(client.list_tools().await.unwrap().is_empty());
    }

    // ── call_tool ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_call_tool_success() {
        let (client, mut server) = mock_client().await;
        tokio::spawn(async move {
            let req = server.recv_json().await;
            assert_eq!(req["method"], "tools/call");
            assert_eq!(req["params"]["name"], "echo");
            server
                .respond(
                    req["id"].as_u64().unwrap(),
                    json!({
                        "content": [{ "type": "text", "text": "hello back" }],
                        "isError": false
                    }),
                )
                .await;
        });

        let result = client.call_tool("echo", json!({ "text": "hello" })).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        if let crate::ContentBlock::Text { text } = &result.content[0] {
            assert_eq!(text, "hello back");
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn test_call_tool_is_error_flag() {
        let (client, mut server) = mock_client().await;
        tokio::spawn(async move {
            let req = server.recv_json().await;
            server
                .respond(
                    req["id"].as_u64().unwrap(),
                    json!({
                        "content": [{ "type": "text", "text": "tool failed" }],
                        "isError": true
                    }),
                )
                .await;
        });

        let result = client.call_tool("bad_tool", json!({})).await.unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn test_call_tool_rpc_error_becomes_err() {
        let (client, mut server) = mock_client().await;
        tokio::spawn(async move {
            let req = server.recv_json().await;
            server.respond_err(req["id"].as_u64().unwrap(), -32000, "tool not found").await;
        });

        let err = client.call_tool("missing", json!({})).await.unwrap_err();
        assert!(matches!(err, Error::Rpc { code: -32000, .. }));
    }

    // ── Concurrent requests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_concurrent_requests_routed_by_id() {
        let (client, mut server) = mock_client().await;

        // Fire two tool calls concurrently.
        let c1 = client.clone();
        let c2 = client.clone();
        let h1 = tokio::spawn(async move { c1.call_tool("tool_a", json!({})).await });
        let h2 = tokio::spawn(async move { c2.call_tool("tool_b", json!({})).await });

        // Collect both requests (order non-deterministic).
        let r1 = server.recv_json().await;
        let r2 = server.recv_json().await;
        let id1 = r1["id"].as_u64().unwrap();
        let id2 = r2["id"].as_u64().unwrap();

        // Respond in reverse order — each caller must get its own result.
        server
            .respond(id2, json!({ "content": [{ "type": "text", "text": "b" }], "isError": false }))
            .await;
        server
            .respond(id1, json!({ "content": [{ "type": "text", "text": "a" }], "isError": false }))
            .await;

        // Identify which handle is tool_a and which is tool_b by the request names.
        let (res_a, res_b) = if r1["params"]["name"] == "tool_a" {
            (h1.await.unwrap().unwrap(), h2.await.unwrap().unwrap())
        } else {
            (h2.await.unwrap().unwrap(), h1.await.unwrap().unwrap())
        };

        if let crate::ContentBlock::Text { text } = &res_a.content[0] {
            assert_eq!(text, "a");
        }
        if let crate::ContentBlock::Text { text } = &res_b.content[0] {
            assert_eq!(text, "b");
        }
    }

    // ── Progress streaming ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_progress_events_delivered_before_result() {
        let (client, mut server) = mock_client().await;

        tokio::spawn(async move {
            let req = server.recv_json().await;
            let id = req["id"].as_u64().unwrap();

            // Send two progress notifications, then the result.
            server.send_progress(id, 25.0, 100.0, "quarter done").await;
            server.send_progress(id, 75.0, 100.0, "three quarters").await;
            server
                .respond(
                    id,
                    json!({ "content": [{ "type": "text", "text": "done" }], "isError": false }),
                )
                .await;
        });

        let mut stream = client
            .call_tool_streaming("slow", json!({}))
            .await
            .unwrap();

        let mut progress_count = 0usize;
        let mut got_result = false;

        while let Some(event) = stream.next().await {
            match event.unwrap() {
                ToolEvent::Progress(p) => {
                    progress_count += 1;
                    assert_eq!(p.total, Some(100.0));
                }
                ToolEvent::Result(r) => {
                    got_result = true;
                    assert!(!r.is_error);
                }
            }
        }

        assert_eq!(progress_count, 2);
        assert!(got_result);
    }

    // ── Cancellation ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_cancellation_returns_cancelled_error() {
        let (client, mut server) = mock_client().await;
        let token = CancellationToken::new();
        let t = token.clone();

        let handle = tokio::spawn({
            let client = client.clone();
            async move { client.call_tool_cancellable("slow", json!({}), t).await }
        });

        // Read the tools/call request so the client isn't blocked on the send.
        let _req = server.recv_json().await;

        // Cancel before the server responds.
        token.cancel();

        let err = handle.await.unwrap().unwrap_err();
        assert!(matches!(err, Error::Cancelled));

        // The client must have sent notifications/cancelled.
        let notif = server.recv_json().await;
        assert_eq!(notif["method"], "notifications/cancelled");
    }

    #[tokio::test]
    async fn test_cancellation_after_result_returns_result() {
        let (client, mut server) = mock_client().await;
        let token = CancellationToken::new();

        tokio::spawn(async move {
            let req = server.recv_json().await;
            server
                .respond(
                    req["id"].as_u64().unwrap(),
                    json!({ "content": [{ "type": "text", "text": "fast" }], "isError": false }),
                )
                .await;
        });

        // Token not yet cancelled — result should come through normally.
        let result = client
            .call_tool_cancellable("fast", json!({}), token)
            .await
            .unwrap();
        assert!(!result.is_error);
    }

    // ── Raw request ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_raw_request_roundtrip() {
        let (client, mut server) = mock_client().await;
        tokio::spawn(async move {
            let req = server.recv_json().await;
            assert_eq!(req["method"], "custom/method");
            server.respond(req["id"].as_u64().unwrap(), json!({ "ok": true })).await;
        });

        let result = client.request("custom/method", Some(json!({ "x": 1 }))).await.unwrap();
        assert_eq!(result["ok"], true);
    }
}
