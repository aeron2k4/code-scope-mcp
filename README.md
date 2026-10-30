# code-scope-mcp

A small, fast MCP server in Rust for AST-aware code navigation. It uses
tree-sitter to find symbols and trims file contents to fit a token budget, so
AI coding agents can explore large codebases without reading whole files.

Supported languages: Rust, Python, JavaScript, TypeScript/TSX, Go.

## Tools

| Tool               | Purpose                                                                     |
| ------------------ | --------------------------------------------------------------------------- |
| `get_file_outline` | Symbols in a file with signatures and line ranges                           |
| `search_symbols`   | Ranked name search across the workspace (exact > prefix > substring)        |
| `read_symbol`      | Exact source of one definition, line-numbered                               |
| `read_file_pruned` | File text within `max_tokens`: full → bodies elided → outline → truncated   |
| `list_files`       | Files under a directory, honouring `.gitignore`                             |
| `git_status`       | Working-tree status                                                         |

Files are parsed lazily the first time they are requested, so nothing is
indexed at startup. Parse results are cached at two levels:

- **in memory**, reused while a file's modification time and size are unchanged;
- **on disk**, in an LMDB (memory-mapped) store keyed by the file's git blob
  hash, so unchanged files are not re-parsed after a restart.

| Variable               | Effect                                                                  |
| ---------------------- | ----------------------------------------------------------------------- |
| `CODE_SCOPE_ROOT`      | Workspace root when no argument is given                                |
| `CODE_SCOPE_CACHE_DIR` | Store location (default `$XDG_CACHE_HOME/code-scope-mcp` or `~/.cache/code-scope-mcp`) |
| `CODE_SCOPE_NO_CACHE`  | Set to `1` to keep the cache in memory only                             |
| `CODE_SCOPE_LOG`       | Log filter, e.g. `debug` (stderr only)                                  |

## Usage

```sh
cargo build --release
./target/release/code-scope-mcp /path/to/project   # or $CODE_SCOPE_ROOT, or the cwd
```

Register it with Claude Code:

```sh
claude mcp add code-scope -- /abs/path/to/code-scope-mcp /path/to/project
```

## Development

```sh
cargo test
cargo fmt
cargo clippy --all-targets -- -D warnings
```

Benchmarks on the Go, CPython and tokio codebases are in [`BENCHMARKS.md`](BENCHMARKS.md); regenerate them with `cargo bench --bench report` (see the header of `benches/report.rs`).

