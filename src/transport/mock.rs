use serde_json::Value;
use tokio::sync::mpsc;

use crate::{types::PROTOCOL_VERSION, Error, Result};

pub(crate) struct MockSender {
    tx: mpsc::Sender<String>,
}

pub(crate) struct MockReceiver {
    rx: mpsc::Receiver<String>,
}

/// The "server" end of the mock connection, held by test code.
pub(crate) struct ServerHandle {
    /// Messages sent by the client arrive here.
    pub rx: mpsc::Receiver<String>,
    /// Messages sent here are delivered to the client.
    pub tx: mpsc::Sender<String>,
}

impl MockSender {
    pub async fn send(&self, json: String) -> Result<()> {
        self.tx.send(json).await.map_err(|_| Error::Closed)
    }
}

impl MockReceiver {
    pub async fn recv(&mut self) -> Result<Option<String>> {
        Ok(self.rx.recv().await)
    }
}

impl ServerHandle {
    pub async fn recv_json(&mut self) -> Value {
        let s = self.rx.recv().await.expect("client closed");
        serde_json::from_str(&s).expect("invalid JSON from client")
    }

    pub async fn send_json(&self, v: Value) {
        self.tx
            .send(serde_json::to_string(&v).unwrap())
            .await
            .expect("client closed");
    }

    /// Consume and respond to the MCP `initialize` / `notifications/initialized` exchange.
    pub async fn complete_handshake(&mut self) {
        let init = self.recv_json().await;
        assert_eq!(init["method"], "initialize");
        self.send_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": init["id"],
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mock", "version": "0.0.0" }
            }
        }))
        .await;

        let notif = self.recv_json().await;
        assert_eq!(notif["method"], "notifications/initialized");
    }

    /// Send a well-formed JSON-RPC response for a pending request.
    pub async fn respond(&self, id: u64, result: Value) {
        self.send_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }))
        .await;
    }

    /// Send a JSON-RPC error response for a pending request.
    pub async fn respond_err(&self, id: u64, code: i64, message: &str) {
        self.send_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }))
        .await;
    }

    /// Send a progress notification for a running request.
    pub async fn send_progress(&self, token: u64, progress: f64, total: f64, message: &str) {
        self.send_json(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {
                "progressToken": token,
                "progress": progress,
                "total": total,
                "message": message
            }
        }))
        .await;
    }
}

/// Returns `(MockSender, MockReceiver, ServerHandle)`.
pub(crate) fn mock_channel() -> (MockSender, MockReceiver, ServerHandle) {
    let (c2s_tx, c2s_rx) = mpsc::channel(64);
    let (s2c_tx, s2c_rx) = mpsc::channel(64);
    (
        MockSender { tx: c2s_tx },
        MockReceiver { rx: s2c_rx },
        ServerHandle { rx: c2s_rx, tx: s2c_tx },
    )
}
