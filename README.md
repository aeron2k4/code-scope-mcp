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

Files are parsed lazily the first time they are requested. The cache is
checked against each file's modification time and size, so nothing is
indexed at startup.

## Usage

```sh
cargo build --release
./target/release/code-scope-mcp /path/to/project   # or $CODE_SCOPE_ROOT, or the cwd
```

Register it with Claude Code:

```sh
claude mcp add code-scope -- /abs/path/to/code-scope-mcp /path/to/project
```

Logs go to stderr only. Set the level with `CODE_SCOPE_LOG`, e.g. `CODE_SCOPE_LOG=debug`.

## Development

```sh
cargo test
cargo fmt
cargo clippy --all-targets -- -D warnings
```

See `CLAUDE.md` for architecture and contribution rules.
