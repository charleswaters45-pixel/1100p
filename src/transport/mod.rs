#[cfg(feature = "stdio")]
mod stdio;
#[cfg(feature = "http")]
mod http;
#[cfg(test)]
pub(crate) mod mock;

use crate::Result;

// ── Split halves ──────────────────────────────────────────────────────────────
//
// The client holds a `Sender` (Arc-shared, used from call sites) and hands
// the `Receiver` to the background reader task. Splitting avoids any runtime
// locking on the hot receive path.

pub(crate) enum Sender {
    #[cfg(feature = "stdio")]
    Stdio(stdio::StdioSender),
    #[cfg(feature = "http")]
    Http(http::HttpSender),
    #[cfg(test)]
    Mock(mock::MockSender),
}

pub(crate) enum Receiver {
    #[cfg(feature = "stdio")]
    Stdio(stdio::StdioReceiver),
    #[cfg(feature = "http")]
    Http(http::HttpReceiver),
    #[cfg(test)]
    Mock(mock::MockReceiver),
}

impl std::fmt::Debug for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "stdio")]
            Self::Stdio(_) => write!(f, "Sender::Stdio"),
            #[cfg(feature = "http")]
            Self::Http(_) => write!(f, "Sender::Http"),
            #[cfg(test)]
            Self::Mock(_) => write!(f, "Sender::Mock"),
        }
    }
}

impl Sender {
    pub async fn send_raw(&self, json: String) -> Result<()> {
        match self {
            #[cfg(feature = "stdio")]
            Self::Stdio(s) => s.send(json).await,
            #[cfg(feature = "http")]
            Self::Http(s) => s.send(json).await,
            #[cfg(test)]
            Self::Mock(s) => s.send(json).await,
        }
    }
}

impl Receiver {
    /// Returns `None` when the connection is closed cleanly.
    pub async fn recv(&mut self) -> Result<Option<String>> {
        match self {
            #[cfg(feature = "stdio")]
            Self::Stdio(r) => r.recv().await,
            #[cfg(feature = "http")]
            Self::Http(r) => r.recv().await,
            #[cfg(test)]
            Self::Mock(r) => r.recv().await,
        }
    }
}

// ── Transport entry points ────────────────────────────────────────────────────

#[cfg(feature = "stdio")]
pub(crate) async fn connect_stdio(program: &str, args: &[&str]) -> Result<(Sender, Receiver)> {
    let (tx, rx) = stdio::StdioTransport::spawn(program, args).await?;
    Ok((Sender::Stdio(tx), Receiver::Stdio(rx)))
}

#[cfg(feature = "http")]
pub(crate) async fn connect_http(url: &str) -> Result<(Sender, Receiver)> {
    let (tx, rx) = http::HttpTransport::connect(url).await?;
    Ok((Sender::Http(tx), Receiver::Http(rx)))
}

#[cfg(test)]
pub(crate) fn connect_mock() -> (Sender, Receiver, mock::ServerHandle) {
    let (s, r, h) = mock::mock_channel();
    (Sender::Mock(s), Receiver::Mock(r), h)
}
