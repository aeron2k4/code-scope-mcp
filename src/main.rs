//! code-scope-mcp: AST-aware code navigation MCP server over stdio.
//!
//! `stdout` is reserved for JSON-RPC traffic. All diagnostics go to `stderr`.

use anyhow::Result;
use tracing_subscriber::EnvFilter;

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_env("CODE_SCOPE_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "code-scope-mcp starting"
    );
    Ok(())
}
