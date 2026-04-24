use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── JSON-RPC 2.0 wire types ───────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(crate) struct RpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl RpcRequest {
    pub fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self { jsonrpc: "2.0", id, method: method.into(), params }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RpcNotification {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl RpcNotification {
    pub fn new(method: &'static str, params: Option<Value>) -> Self {
        Self { jsonrpc: "2.0", method, params }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RpcResponse {
    pub id: u64,
    pub result: Option<Value>,
    pub error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct IncomingNotification {
    pub method: String,
    pub params: Option<Value>,
}

/// Parsed form of any message arriving from a server.
pub(crate) enum IncomingMessage {
    Response(RpcResponse),
    Notification(IncomingNotification),
}

pub(crate) fn parse_incoming(raw: &str) -> crate::Result<IncomingMessage> {
    let v: Value = serde_json::from_str(raw)?;
    let has_id = v.get("id").is_some();
    let has_result_or_error = v.get("result").is_some() || v.get("error").is_some();
    let has_method = v.get("method").is_some();

    if has_id && has_result_or_error {
        Ok(IncomingMessage::Response(serde_json::from_value(v)?))
    } else if has_method {
        Ok(IncomingMessage::Notification(serde_json::from_value(v)?))
    } else {
        Err(crate::Error::Protocol(format!("unrecognised message: {raw}")))
    }
}

// ── MCP protocol types ────────────────────────────────────────────────────────

pub const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Clone, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Resource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Prompt {
    pub name: String,
    pub description: Option<String>,
    pub arguments: Option<Vec<PromptArgument>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PromptArgument {
    pub name: String,
    pub description: Option<String>,
    pub required: Option<bool>,
}

/// Result of a `tools/call` invocation.
#[derive(Debug)]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
}

impl ToolResult {
    pub(crate) fn from_value(v: Value) -> crate::Result<Self> {
        let content = v
            .get("content")
            .map(|c| serde_json::from_value(c.clone()))
            .transpose()?
            .unwrap_or_default();
        let is_error = v.get("isError").and_then(|e| e.as_bool()).unwrap_or(false);
        Ok(Self { content, is_error })
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Resource {
        resource: EmbeddedResource,
    },
}

#[derive(Debug, Deserialize)]
pub struct EmbeddedResource {
    pub uri: String,
    pub text: Option<String>,
    pub blob: Option<String>,
}

/// Progress update emitted during a long-running tool call.
#[derive(Debug)]
pub struct ProgressUpdate {
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<String>,
}

/// Events yielded by [`McpClient::call_tool_streaming`].
#[derive(Debug)]
pub enum ToolEvent {
    Progress(ProgressUpdate),
    Result(ToolResult),
}

/// Server capabilities returned during initialization.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerCapabilities {
    pub tools: Option<Value>,
    pub resources: Option<Value>,
    pub prompts: Option<Value>,
    pub logging: Option<Value>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // ── parse_incoming routing ────────────────────────────────────────────────

    #[rstest]
    #[case(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#,         true)]
    #[case(r#"{"jsonrpc":"2.0","id":2,"result":null}"#,                  true)]
    #[case(r#"{"jsonrpc":"2.0","id":3,"error":{"code":-1,"message":"e"}}"#, true)]
    #[case(r#"{"jsonrpc":"2.0","method":"notify/something","params":{}}"#, false)]
    #[case(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,   false)]
    fn test_parse_routing(#[case] input: &str, #[case] expect_response: bool) {
        match parse_incoming(input).unwrap() {
            IncomingMessage::Response(_)     => assert!(expect_response, "expected notification"),
            IncomingMessage::Notification(_) => assert!(!expect_response, "expected response"),
        }
    }

    #[test]
    fn test_parse_response_fields() {
        let raw = r#"{"jsonrpc":"2.0","id":42,"result":{"tools":[{"name":"x"}]}}"#;
        let IncomingMessage::Response(resp) = parse_incoming(raw).unwrap() else {
            panic!("expected response");
        };
        assert_eq!(resp.id, 42);
        assert!(resp.error.is_none());
        assert!(resp.result.is_some());
    }

    #[test]
    fn test_parse_error_response_fields() {
        let raw = r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"not found"}}"#;
        let IncomingMessage::Response(resp) = parse_incoming(raw).unwrap() else {
            panic!("expected response");
        };
        assert_eq!(resp.id, 7);
        assert!(resp.result.is_none());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "not found");
    }

    #[test]
    fn test_parse_notification_method_and_params() {
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":50}}"#;
        let IncomingMessage::Notification(n) = parse_incoming(raw).unwrap() else {
            panic!("expected notification");
        };
        assert_eq!(n.method, "notifications/progress");
        assert_eq!(n.params.unwrap()["progress"], 50);
    }

    #[test]
    fn test_parse_notification_no_params() {
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let IncomingMessage::Notification(n) = parse_incoming(raw).unwrap() else {
            panic!("expected notification");
        };
        assert_eq!(n.method, "notifications/initialized");
        assert!(n.params.is_none());
    }

    // ── Malformed input ───────────────────────────────────────────────────────

    #[test]
    fn test_parse_malformed_json_returns_error() {
        assert!(parse_incoming("{not json}").is_err());
    }

    #[test]
    fn test_parse_empty_string_returns_error() {
        assert!(parse_incoming("").is_err());
    }

    #[test]
    fn test_parse_unknown_shape_returns_error() {
        // Has `id` but no `result`, `error`, or `method` — unrecognisable.
        assert!(parse_incoming(r#"{"jsonrpc":"2.0","id":1}"#).is_err());
    }

    // ── ToolResult ────────────────────────────────────────────────────────────

    #[test]
    fn test_tool_result_from_value_text_block() {
        let v = serde_json::json!({
            "content": [{ "type": "text", "text": "hello" }],
            "isError": false
        });
        let r = ToolResult::from_value(v).unwrap();
        assert!(!r.is_error);
        assert_eq!(r.content.len(), 1);
        assert!(matches!(&r.content[0], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn test_tool_result_is_error_flag() {
        let v = serde_json::json!({
            "content": [{ "type": "text", "text": "fail" }],
            "isError": true
        });
        assert!(ToolResult::from_value(v).unwrap().is_error);
    }

    #[test]
    fn test_tool_result_empty_content_defaults() {
        let v = serde_json::json!({});
        let r = ToolResult::from_value(v).unwrap();
        assert!(r.content.is_empty());
        assert!(!r.is_error);
    }

    // ── RpcRequest serialisation ──────────────────────────────────────────────

    #[test]
    fn test_rpc_request_serialises_correctly() {
        let req = RpcRequest::new(5, "tools/list", None);
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 5);
        assert_eq!(v["method"], "tools/list");
        assert!(v.get("params").is_none(), "params should be omitted when None");
    }

    #[test]
    fn test_rpc_request_includes_params_when_present() {
        let req = RpcRequest::new(1, "tools/call", Some(serde_json::json!({ "name": "x" })));
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(v["params"]["name"], "x");
    }
}
