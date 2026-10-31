# code-scope-mcp

A small, fast MCP server in Rust for AST-aware code navigation. It uses
tree-sitter to find symbols and trims file contents to fit a token budget, so
AI coding agents can explore large codebases without reading whole files.

Supported languages: Rust, Python, JavaScript, TypeScript/TSX, Go.

## Tools

| Tool               | Purpose                                                                          |
| ------------------ | -------------------------------------------------------------------------------- |
| `get_file_outline` | Symbols in a file with signatures and line ranges                                |
| `search_symbols`   | Ranked name search across the workspace (exact > prefix > substring)             |
| `read_symbol`      | Exact source of one definition, line-numbered                                    |
| `read_file_pruned` | File text within `max_tokens`: full → bodies elided → outline → truncated        |
| `find_references`  | Uses of a symbol from identifier tokens (comments and strings skipped)           |
| `search_text`      | Literal or regex search over files, honouring `.gitignore`                       |
| `list_files`       | Files under a directory, honouring `.gitignore`                                  |
| `git_status`       | Working-tree status                                                              |

Hits are one per line. `find_references` prints `path:line:col: text`, with
`[definition]` on definitions when asked for; `search_text` prints
`path:line: text`. Either adds `[in Symbol]` when the hit lies inside a symbol
(top-level code has none):

```text
src/lib.rs:42:9: load();  [in Config::reload]
src/lib.rs:7:12: pub fn load() -> Self {  [definition]  [in Config]
README.md:3: Call load() once at startup.
```

Files are parsed lazily the first time they are requested, so nothing is
indexed at startup. A parse records a file's symbols and the offsets of its
identifier tokens, and is cached at two levels:

- **in memory**, reused while a file's modification time and size are unchanged;
- **on disk**, in an LMDB (memory-mapped) store keyed by the file's git blob
  hash, so unchanged files are not re-parsed after a restart.

| Variable               | Effect                                                                  |
| ---------------------- | ----------------------------------------------------------------------- |
| `CODE_SCOPE_ROOT`      | Workspace root when no argument is given                                |
| `CODE_SCOPE_CACHE_DIR` | Store location (default `$XDG_CACHE_HOME/code-scope-mcp` or `~/.cache/code-scope-mcp`) |
| `CODE_SCOPE_NO_CACHE`  | Set to `1` to keep the cache in memory only                             |
| `CODE_SCOPE_LOG`       | Log filter, e.g. `debug` (stderr only)                                  |

## Performance

On the Go 1.27.3 standard library (8,090 files, 82 MB), with a warm cache:

- `search_symbols`: 24 ms; after a restart, 86 ms from the on-disk store
  versus 2.6 s cold.
- `find_references`: 24 ms. It skips 49% of the lines a whole-word
  `grep -w` matches: comments, strings, docs, and files in languages it does
  not parse (C, assembly, ...), which can hold real uses.
- `search_text` over every text file: 33 ms median for a whole-word query,
  31 ms for the regex `TODO|FIXME|XXX`.
- Outlines use ~92% fewer tokens than full source; reading one function costs
  a median 0.9% of its file.
- The server answers `initialize` in ~2 ms.

Full results for Go, CPython and tokio are in [`BENCHMARKS.md`](BENCHMARKS.md).

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
   at user scope as `code-scope` with `"alwaysLoad": true`, so its tools are in
   the model's tool list from the start instead of deferred behind ToolSearch.
   With no root argument the server uses the session's working directory.
2. Add a `PreToolUse` hook (`~/.claude/hooks/code-scope-guard.sh`, wired in
   `~/.claude/settings.json`) that points Claude to the code-scope tools:
   - denies whole-file `Read` of `.rs .py .js .jsx .ts .tsx .go` files; `Read`
     with `offset`/`limit` is still allowed, because `Edit` needs a prior `Read`;
   - denies searches that may cover those files inside the session's
     workspace, both the `Grep` tool and `grep`/`egrep`/`fgrep`/`ugrep`/`rg`/`git
     grep` in Bash (use `search_text` / `find_references`). Searching a non-code
     path, with a non-code filter such as `--include='*.md'` / `-g '*.md'`,
     piped input (`cargo test | grep FAILED`) or outside the workspace is
     allowed. Bash parsing is best effort: `eval`, scripts, `find -exec` and
     `xargs` are not inspected.

   The hook is [`scripts/code-scope-guard.sh`](scripts/code-scope-guard.sh).
3. Add a short instruction block to `~/.claude/CLAUDE.md`.

Restart Claude Code afterwards. Re-running the script is safe and updates the
binary. Options:

```sh
scripts/install-claude.sh --no-enforce   # step 1 only, no hook or CLAUDE.md block
scripts/install-claude.sh --uninstall    # undo steps 1–3 (keeps the binary)
```

To register manually instead:

```sh
claude mcp add-json code-scope \
  '{"type": "stdio", "command": "/abs/path/to/code-scope-mcp", "args": ["/path/to/project"], "alwaysLoad": true}'
```

## Development

```sh
cargo test
cargo fmt
cargo clippy --all-targets -- -D warnings
```

`cargo bench --bench report` measures this crate's own `src/`. To regenerate
[`BENCHMARKS.md`](BENCHMARKS.md), set `BENCH_CORPORA` to comma-separated
`name=directory` pairs; the bench stops if a directory does not exist. Paths
vary by system, for example:

```sh
BENCH_CORPORA="Go stdlib=$(go env GOROOT)/src,\
CPython stdlib=$(python3 -c 'import sysconfig; print(sysconfig.get_path("stdlib"))'),\
tokio (Rust)=/path/to/tokio" \
BENCH_OUT=BENCHMARKS.md cargo bench --bench report
```

