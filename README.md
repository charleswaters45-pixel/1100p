# 1100p

**1100p** is a lightweight, async [Model Context Protocol](https://modelcontextprotocol.io) client in Rust.

Supports stdio and HTTP/SSE transports, long-running tools with progress streaming, and cancellation — with a minimal dependency footprint.

## Features

- **Two transports** — stdio (local processes) and HTTP/SSE (remote servers), both feature-gated
- **Long-running tools** — stream progress events as they arrive; designed for LLM-as-tool patterns
- **Cancellation** — `CancellationToken` sends `notifications/cancelled` per MCP spec
- **Zero trait-object overhead** — enum dispatch on transports, no `async-trait` boxing
- **Clone-cheap client** — all state behind `Arc`, safe to share across tasks

## Installation

Requires Rust 1.75+ ([install via rustup](https://rustup.rs)).

Add to your `Cargo.toml`:

```toml
[dependencies]
eleven100p = { path = "." }
```

To include only the transport(s) you need:

```toml
eleven100p = { path = ".", default-features = false, features = ["stdio"] }
```

## Quick start

```rust
use eleven100p::{McpClient, ToolEvent, CancellationToken};
use serde_json::json;
use futures::StreamExt;

#[tokio::main]
async fn main() -> eleven100p::Result<()> {
    // Connect over stdio (spawns the process for you)
    let client = McpClient::stdio("npx", &["-y", "@modelcontextprotocol/server-filesystem", "."]).await?;

    // Or connect to a remote server over HTTP/SSE
    // let client = McpClient::http("http://localhost:3000/sse").await?;

    // List available tools
    let tools = client.list_tools().await?;
    for tool in &tools {
        println!("{}: {}", tool.name, tool.description.as_deref().unwrap_or(""));
    }

    // Call a tool
    let result = client.call_tool("read_file", json!({ "path": "README.md" })).await?;
    for block in result.content {
        if let eleven100p::ContentBlock::Text { text } = block {
            println!("{text}");
        }
    }

    Ok(())
}
```

## Long-running tools

For tools that take time (e.g. calling an LLM), stream progress events instead of blocking:

```rust
use futures::StreamExt;

let mut stream = client
    .call_tool_streaming("generate_code", json!({ "prompt": "write a binary search" }))
    .await?;

while let Some(event) = stream.next().await {
    match event? {
        ToolEvent::Progress(p) => {
            let pct = p.total.map(|t| p.progress / t * 100.0);
            println!("[{:.0}%] {}", pct.unwrap_or(0.0), p.message.unwrap_or_default());
        }
        ToolEvent::Result(r) => {
            println!("done (is_error={})", r.is_error);
            break;
        }
    }
}
```

## Cancellation

```rust
use eleven100p::CancellationToken;

let token = CancellationToken::new();

// Cancel from another task
let t = token.clone();
tokio::spawn(async move {
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    t.cancel();
});

match client.call_tool_cancellable("slow_tool", json!({}), token).await {
    Ok(result) => println!("completed"),
    Err(eleven100p::Error::Cancelled) => println!("cancelled by token"),
    Err(e) => eprintln!("error: {e}"),
}
```

## Raw requests

For any MCP method not covered by the typed API:

```rust
let response = client.request("sampling/createMessage", Some(json!({
    "messages": [{ "role": "user", "content": { "type": "text", "text": "hello" } }],
    "maxTokens": 256
}))).await?;
```

## Resources and prompts

```rust
// Resources
let resources = client.list_resources().await?;
let content = client.read_resource("file:///path/to/file.txt").await?;

// Prompts
let prompts = client.list_prompts().await?;
let rendered = client.get_prompt("summarise", Some(json!({ "length": "short" }))).await?;
```

## Testing

```bash
cargo test                   # all tests
cargo test --lib             # unit tests only (no I/O)
cargo test --test transport_http  # HTTP/SSE integration tests only
```

### Test layers

**Protocol parsing** (`src/types.rs`) — pure unit tests, no I/O

| Test | Covers |
|------|--------|
| `test_parse_routing` (5 cases, rstest) | Response vs notification dispatch |
| `test_parse_response_fields` | `id`, `result`, `error` deserialisation |
| `test_parse_error_response_fields` | RPC error code + message |
| `test_parse_notification_*` (2) | Method, params, absent params |
| `test_parse_malformed_json_returns_error` | Bad JSON → `Err` |
| `test_parse_empty_string_returns_error` | Empty input → `Err` |
| `test_parse_unknown_shape_returns_error` | Has `id` but no result/method → `Err` |
| `test_tool_result_*` (3) | `ToolResult::from_value` content blocks and defaults |
| `test_rpc_request_*` (2) | Serialisation, `params` omitted when `None` |

**Client behaviour** (`src/client.rs`) — mock in-process transport, no I/O

| Test | Covers |
|------|--------|
| `test_handshake_capabilities_parsed` | Capabilities extracted from init response |
| `test_handshake_server_error_propagates` | Init RPC error → `Error::Rpc` |
| `test_list_tools_*` (2) | Full list + empty server |
| `test_call_tool_success` | Content, `is_error` flag, argument forwarding |
| `test_call_tool_is_error_flag` | `isError: true` surfaces correctly |
| `test_call_tool_rpc_error_becomes_err` | Server error → `Error::Rpc` with correct code |
| `test_concurrent_requests_routed_by_id` | Out-of-order responses reach the right caller |
| `test_progress_events_delivered_before_result` | Two progress events then result, correct order |
| `test_cancellation_returns_cancelled_error` | Token fires → `Error::Cancelled` + wire notification |
| `test_cancellation_after_result_returns_result` | Token unfired → result passes through |
| `test_raw_request_roundtrip` | `client.request()` for arbitrary methods |

**HTTP/SSE transport** (`tests/transport_http.rs`) — real `axum` server, full hyper round-trip

| Test | Covers |
|------|--------|
| `test_http_connect_and_list_tools` | SSE connect + `tools/list` over HTTP |
| `test_http_call_tool_roundtrip` | POST request + SSE response |
| `test_http_capabilities_parsed` | Capabilities from init response |
| `test_http_unknown_method_returns_rpc_error` | Server error over HTTP |
| `test_http_concurrent_requests` | Two simultaneous requests on one SSE stream |

### Adding tests for a new transport

The mock transport (`src/transport/mock.rs`) is available in `#[cfg(test)]` contexts. Use `connect_mock()` to get a `(Sender, Receiver, ServerHandle)` triple and call `McpClient::handshake` directly:

```rust
#[tokio::test]
async fn my_test() {
    let (client, mut server) = mock_client().await; // helper in client.rs tests
    tokio::spawn(async move {
        let req = server.recv_json().await;
        server.respond(req["id"].as_u64().unwrap(), json!({ ... })).await;
    });
    // exercise client ...
}
```

## Architecture

```
McpClient  (Arc-shared, Clone)
│
├── Arc<Sender>          — writes JSON-RPC requests to the transport
│     └── enum { Stdio | Http }
│
├── Arc<PendingMap>      — maps request IDs to oneshot response channels
│                          + optional progress mpsc channels
│
└── background task      — reads all incoming messages, routes to pending map
      ├── Response        → oneshot::Sender in pending map
      └── notifications/progress → mpsc::Sender in pending map
```

The `Sender` and `Receiver` halves of each transport are split at construction time so the background reader holds the only reference to `Receiver` — no lock contention on the read path.

## Feature flags

| Flag | Default | Enables |
|------|---------|---------|
| `stdio` | ✓ | stdio transport (`McpClient::stdio`) |
| `http` | ✓ | HTTP/SSE transport (`McpClient::http`) |

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | async runtime (minimal features) |
| `tokio-util` | `CancellationToken` |
| `serde` + `serde_json` | JSON serialization |
| `thiserror` | error types |
| `futures-core` | `Stream` trait |
| `hyper` + `hyper-util` + `http-body-util` | HTTP/SSE transport (optional) |

## License

MIT