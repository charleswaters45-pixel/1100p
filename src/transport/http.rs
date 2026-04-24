/// HTTP/SSE transport.
///
/// MCP over HTTP works as follows:
///   1. Client opens a GET /sse connection.
///   2. Server immediately sends an `endpoint` event whose `data` is the URL
///      to POST requests to (often `/messages?sessionId=…`).
///   3. Client POSTs JSON-RPC requests to that URL.
///   4. Responses and notifications arrive as `message` events on the SSE stream.
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, Request};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::{Error, Result};

type HyperClient = Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>>;

pub(crate) struct HttpSender {
    client: Arc<HyperClient>,
    /// Resolved after the server sends the `endpoint` SSE event.
    endpoint: Arc<Mutex<Option<String>>>,
    base_url: Arc<String>,
}

pub(crate) struct HttpReceiver {
    rx: mpsc::Receiver<Result<String>>,
}

pub(crate) struct HttpTransport;

impl HttpTransport {
    pub async fn connect(sse_url: &str) -> Result<(HttpSender, HttpReceiver)> {
        let client: HyperClient = Client::builder(TokioExecutor::new()).build_http();
        let client = Arc::new(client);

        let endpoint: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let (msg_tx, msg_rx) = mpsc::channel::<Result<String>>(64);
        let (endpoint_tx, endpoint_rx) = oneshot::channel::<String>();

        // Parse base URL (scheme + host) for resolving relative endpoint paths.
        let base_url = Arc::new(base_url(sse_url));

        // Spawn the SSE reader task.
        let client_clone = Arc::clone(&client);
        let sse_url = sse_url.to_owned();
        tokio::spawn(async move {
            if let Err(e) = sse_loop(client_clone, &sse_url, msg_tx, endpoint_tx).await {
                // SSE loop ended — channel closes, client will see Closed error.
                let _ = e;
            }
        });

        // Wait until the server sends us the POST endpoint before returning.
        let resolved = endpoint_rx.await.map_err(|_| {
            Error::Protocol("SSE connection closed before endpoint event".into())
        })?;
        *endpoint.lock().await = Some(resolved);

        let sender = HttpSender { client, endpoint, base_url };
        let receiver = HttpReceiver { rx: msg_rx };
        Ok((sender, receiver))
    }
}

impl HttpSender {
    pub async fn send(&self, json: String) -> Result<()> {
        let endpoint = self.endpoint.lock().await;
        let url = endpoint.as_deref().ok_or(Error::NotInitialized)?;

        let url = if url.starts_with("http") {
            url.to_owned()
        } else {
            format!("{}{}", self.base_url, url)
        };

        let req = Request::post(&url)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(json)))
            .map_err(|e| Error::Protocol(e.to_string()))?;

        let resp = self.client.request(req).await?;

        if !resp.status().is_success() {
            return Err(Error::Protocol(format!(
                "POST {} returned {}",
                url,
                resp.status()
            )));
        }

        // Drain the body so the connection can be reused.
        let _ = resp.into_body().collect().await;
        Ok(())
    }
}

impl HttpReceiver {
    pub async fn recv(&mut self) -> Result<Option<String>> {
        match self.rx.recv().await {
            Some(Ok(msg)) => Ok(Some(msg)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }
}

// ── SSE streaming loop ────────────────────────────────────────────────────────

async fn sse_loop(
    client: Arc<HyperClient>,
    url: &str,
    msg_tx: mpsc::Sender<Result<String>>,
    endpoint_tx: oneshot::Sender<String>,
) -> Result<()> {
    let req = Request::get(url)
        .header("accept", "text/event-stream")
        .body(Full::new(Bytes::new()))
        .map_err(|e| Error::Protocol(e.to_string()))?;

    let resp = client.request(req).await?;

    if !resp.status().is_success() {
        return Err(Error::Protocol(format!(
            "SSE GET {} returned {}",
            url,
            resp.status()
        )));
    }

    let mut body = resp.into_body();
    let mut buf = Vec::<u8>::new();
    let mut event_type = String::new();
    let mut endpoint_tx = Some(endpoint_tx);

    loop {
        match body.frame().await {
            None => break,
            Some(Err(e)) => {
                let _ = msg_tx.send(Err(Error::Http(e))).await;
                break;
            }
            Some(Ok(frame)) => {
                if let Some(chunk) = frame.data_ref() {
                    buf.extend_from_slice(chunk);
                }

                // Process complete lines from buf.
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line = buf.drain(..=pos).collect::<Vec<_>>();
                    let line = String::from_utf8_lossy(&line);
                    let line = line.trim_end_matches(['\n', '\r']);

                    if line.is_empty() {
                        // Empty line = dispatch — reset event type.
                        event_type.clear();
                        continue;
                    }

                    if let Some(rest) = line.strip_prefix("event:") {
                        event_type = rest.trim().to_owned();
                    } else if let Some(data) = line.strip_prefix("data:") {
                        let data = data.trim().to_owned();
                        match event_type.as_str() {
                            "endpoint" => {
                                if let Some(tx) = endpoint_tx.take() {
                                    let _ = tx.send(data);
                                }
                            }
                            "message" | "" => {
                                if msg_tx.send(Ok(data)).await.is_err() {
                                    return Ok(());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn base_url(url: &str) -> String {
    // Returns "scheme://host[:port]" from a full URL.
    if let Some(rest) = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) {
        let scheme = if url.starts_with("https") { "https" } else { "http" };
        let host = rest.split('/').next().unwrap_or(rest);
        format!("{scheme}://{host}")
    } else {
        url.to_owned()
    }
}
