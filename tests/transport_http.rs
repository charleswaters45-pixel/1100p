/// Integration tests for the HTTP/SSE transport.
///
/// Each test spins up a real `axum` server on a random port, connects the
/// production `McpClient::http` path to it, and exercises the full round-trip
/// through hyper/SSE without any mocking.
#[cfg(feature = "http")]
mod http_transport {
    use axum::{
        extract::State,
        http::StatusCode,
        response::sse::{Event, Sse},
        routing::{get, post},
        Router,
    };
    use futures::StreamExt;
    use mcp_client::{ContentBlock, McpClient};
    use serde_json::{json, Value};
    use std::convert::Infallible;
    use tokio::sync::broadcast;
    use tokio_stream::wrappers::BroadcastStream;

    // ── Shared test server ────────────────────────────────────────────────────

    #[derive(Clone)]
    struct Srv {
        /// POST handler sends SSE events through this.
        tx: broadcast::Sender<String>,
    }

    async fn sse_handler(State(srv): State<Srv>) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
        let rx = srv.tx.subscribe();
        let messages = BroadcastStream::new(rx)
            .filter_map(|r| async move { r.ok() })
            .map(|data| Ok(Event::default().event("message").data(data)));

        // Prepend the endpoint event so the client knows where to POST.
        let endpoint = futures::stream::once(async {
            Ok::<Event, Infallible>(Event::default().event("endpoint").data("/messages"))
        });

        Sse::new(endpoint.chain(messages))
    }

    async fn messages_handler(
        State(srv): State<Srv>,
        body: String,
    ) -> StatusCode {
        let req: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => return StatusCode::BAD_REQUEST,
        };

        // Notifications have no `id` — acknowledge without responding.
        let id = match req.get("id").and_then(|v| v.as_u64()) {
            Some(id) => id,
            None => return StatusCode::OK,
        };

        let method = req["method"].as_str().unwrap_or("");
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {}, "resources": {} },
                    "serverInfo": { "name": "axum-test", "version": "0.0.1" }
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "tools": [{
                        "name": "greet",
                        "description": "says hello",
                        "inputSchema": {
                            "type": "object",
                            "properties": { "name": { "type": "string" } }
                        }
                    }]
                }
            }),
            "tools/call" => {
                let tool_name = req["params"]["name"].as_str().unwrap_or("");
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": format!("called {tool_name}")
                        }],
                        "isError": false
                    }
                })
            }
            _ => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": "method not found" }
            }),
        };

        let _ = srv.tx.send(serde_json::to_string(&response).unwrap());
        StatusCode::OK
    }

    /// Bind on a random port, return the URL and a task handle.
    async fn start_server() -> String {
        let (tx, _) = broadcast::channel::<String>(64);
        let state = Srv { tx };
        let app = Router::new()
            .route("/sse", get(sse_handler))
            .route("/messages", post(messages_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{}/sse", addr)
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_http_connect_and_list_tools() {
        let url = start_server().await;
        let client = McpClient::http(&url).await.unwrap();

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "greet");
        assert_eq!(tools[0].description.as_deref(), Some("says hello"));
    }

    #[tokio::test]
    async fn test_http_call_tool_roundtrip() {
        let url = start_server().await;
        let client = McpClient::http(&url).await.unwrap();

        let result = client
            .call_tool("greet", json!({ "name": "world" }))
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        if let ContentBlock::Text { text } = &result.content[0] {
            assert_eq!(text, "called greet");
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn test_http_capabilities_parsed() {
        let url = start_server().await;
        let client = McpClient::http(&url).await.unwrap();
        assert!(client.capabilities.tools.is_some());
        assert!(client.capabilities.resources.is_some());
    }

    #[tokio::test]
    async fn test_http_unknown_method_returns_rpc_error() {
        let url = start_server().await;
        let client = McpClient::http(&url).await.unwrap();

        let err = client.request("unknown/method", None).await.unwrap_err();
        assert!(matches!(err, mcp_client::Error::Rpc { code: -32601, .. }));
    }

    #[tokio::test]
    async fn test_http_concurrent_requests() {
        let url = start_server().await;
        let client = McpClient::http(&url).await.unwrap();
        let c2 = client.clone();

        let (tools, result) = tokio::join!(
            client.list_tools(),
            c2.call_tool("greet", json!({}))
        );

        assert!(tools.is_ok());
        assert!(result.is_ok());
    }
}
