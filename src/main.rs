//! code-scope-mcp: AST-aware code navigation MCP server over stdio.
//!
//! `stdout` is reserved for JSON-RPC traffic. All diagnostics go to `stderr`.
//!
//! Usage: `code-scope-mcp [ROOT]`. The workspace root defaults to
//! `$CODE_SCOPE_ROOT`, then the current directory.

use std::path::PathBuf;

use anyhow::Result;
use code_scope_mcp::mcp::{CodeScope, Workspace};
use rmcp::ServiceExt;
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

fn workspace_root() -> Result<PathBuf> {
    if let Some(arg) = std::env::args_os().nth(1) {
        return Ok(arg.into());
    }
    if let Some(env) = std::env::var_os("CODE_SCOPE_ROOT") {
        return Ok(env.into());
    }
    Ok(std::env::current_dir()?)
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let workspace = Workspace::new(workspace_root()?)?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        root = %workspace.root().display(),
        "code-scope-mcp starting"
    );

    let service = CodeScope::new(workspace)
        .serve(rmcp::transport::stdio())
        .await
        .inspect_err(|err| tracing::error!(%err, "failed to start MCP server"))?;
    service.waiting().await?;
    tracing::info!("client disconnected, shutting down");
    Ok(())
}
