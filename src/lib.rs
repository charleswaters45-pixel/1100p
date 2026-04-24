mod error;
mod types;
mod transport;
mod client;

pub use error::{Error, Result};
pub use client::McpClient;
pub use types::{
    ContentBlock,
    EmbeddedResource,
    Prompt,
    PromptArgument,
    ProgressUpdate,
    Resource,
    ServerCapabilities,
    Tool,
    ToolEvent,
    ToolResult,
};
// Re-export CancellationToken so callers don't need tokio-util directly.
pub use tokio_util::sync::CancellationToken;
