//! code-scope-mcp: AST-aware code navigation MCP server over stdio.
//!
//! `stdout` is reserved for JSON-RPC traffic. All diagnostics go to `stderr`.
//!
//! Usage: `code-scope-mcp [ROOT]`. The workspace root defaults to
//! `$CODE_SCOPE_ROOT`, then the current directory.
//!
//! Parse results persist in `$CODE_SCOPE_CACHE_DIR`, defaulting to
//! `$XDG_CACHE_HOME/code-scope-mcp` or `~/.cache/code-scope-mcp`. Set
//! `CODE_SCOPE_NO_CACHE=1` to keep the cache in memory only.

use std::path::PathBuf;

use anyhow::Result;
use code_scope_mcp::index::cache::PersistentStore;
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

fn cache_dir() -> Option<PathBuf> {
    if std::env::var_os("CODE_SCOPE_NO_CACHE").is_some_and(|v| !v.is_empty() && v != "0") {
        return None;
    }
    if let Some(dir) = std::env::var_os("CODE_SCOPE_CACHE_DIR") {
        return Some(dir.into());
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("code-scope-mcp"))
}

/// Attach the persistent store when available; a broken or unwritable cache
/// directory only costs re-parsing, so it is never fatal.
/// Drop stored results of deleted files on a background thread, so the
/// server answers `initialize` without waiting for the sweep.
fn prune_in_background(store: PersistentStore, root: PathBuf) {
    std::thread::spawn(move || match store.prune_missing(&root) {
        Ok(0) => {}
        Ok(n) => tracing::info!(removed = n, "pruned parse results of deleted files"),
        Err(err) => tracing::warn!(%err, "failed to prune the parse store"),
    });
}

fn with_persistence(workspace: Workspace) -> Workspace {
    let Some(dir) = cache_dir() else {
        tracing::info!("persistent cache disabled");
        return workspace;
    };
    match PersistentStore::open(&dir) {
        Ok(store) => {
            tracing::info!(dir = %dir.display(), "persistent cache enabled");
            prune_in_background(store.clone(), workspace.root().to_path_buf());
            workspace.with_store(store)
        }
        Err(err) => {
            tracing::warn!(%err, "persistent cache unavailable, using memory only");
            workspace
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let workspace = with_persistence(Workspace::new(workspace_root()?)?);
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
