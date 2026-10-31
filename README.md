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
| `find_references`  | Uses of a symbol (identifier tokens, so comments/strings are skipped), with the enclosing symbol |
| `search_text`      | Literal or regex search over files, honouring `.gitignore`, with the enclosing symbol |
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

### Claude Code

One command builds the binary, registers it and makes Claude use it
(requires `cargo`, `claude` and `jq`):

```sh
scripts/install-claude.sh
```

It does three things, all at user scope so they apply to every project:

1. `cargo install` the binary to `~/.cargo/bin/code-scope-mcp` and register it
   with `claude mcp add --scope user code-scope`. With no root argument the
   server uses the session's working directory.
2. Add a `PreToolUse` hook (`~/.claude/hooks/code-scope-guard.sh`, wired in
   `~/.claude/settings.json`) that points Claude to the code-scope tools:
   - denies whole-file `Read` of `.rs .py .js .jsx .ts .tsx .go` files; `Read`
     with `offset`/`limit` is still allowed, because `Edit` needs a prior `Read`;
   - denies `Grep` that may cover those files inside the session's workspace
     (use `search_text` / `find_references`). `Grep` on a non-code path, with a
     non-code `glob`/`type` such as `*.md`, or outside the workspace is allowed.
3. Add a short instruction block to `~/.claude/CLAUDE.md`.

Restart Claude Code afterwards. Re-running the script is safe and updates the
binary. Options:

```sh
scripts/install-claude.sh --no-enforce   # step 1 only, no hook or CLAUDE.md block
scripts/install-claude.sh --uninstall    # undo steps 1–3 (keeps the binary)
```

To register manually instead:

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

