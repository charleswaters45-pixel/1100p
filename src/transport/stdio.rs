use std::sync::Arc;

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

use crate::{Error, Result};

pub(crate) struct StdioSender {
    stdin: Arc<Mutex<ChildStdin>>,
}

pub(crate) struct StdioReceiver {
    stdout: BufReader<ChildStdout>,
    // Keep the child alive for the lifetime of the receiver.
    _child: Child,
}

pub(crate) struct StdioTransport;

impl StdioTransport {
    pub async fn spawn(program: &str, args: &[&str]) -> Result<(StdioSender, StdioReceiver)> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Silence stderr so server logs don't pollute our stdout.
            .stderr(std::process::Stdio::null())
            .spawn()?;

        let stdin = child.stdin.take().ok_or_else(|| {
            Error::Protocol("failed to capture child stdin".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            Error::Protocol("failed to capture child stdout".into())
        })?;

        let sender = StdioSender {
            stdin: Arc::new(Mutex::new(stdin)),
        };
        let receiver = StdioReceiver {
            stdout: BufReader::new(stdout),
            _child: child,
        };

        Ok((sender, receiver))
    }
}

impl StdioSender {
    /// Write a JSON line (newline-delimited JSON per MCP spec).
    pub async fn send(&self, mut json: String) -> Result<()> {
        json.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(json.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

impl StdioReceiver {
    pub async fn recv(&mut self) -> Result<Option<String>> {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        // Strip the trailing newline before returning.
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        Ok(Some(line))
    }
}
