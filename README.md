# astlens

A small, fast MCP server in Rust for AST-aware code navigation. It uses
tree-sitter to find symbols and trims file contents to fit a token budget, so
AI coding agents can explore large codebases without reading whole files.

Supported languages: Rust, Python, JavaScript, TypeScript/TSX, Go, C, C++, Java,
C#. `.h` headers are parsed as C, or as C++ when C fails.

## Performance

On the Go 1.27.3 standard library (8,236 source files, 82 MB), with a warm cache:

- `search_symbols`: 23 ms; after a restart, 86 ms from the on-disk store
  versus 2.8 s cold.
- `find_references`: 24 ms. It skips 49% of the lines a whole-word
  `grep -w` matches: comments, strings, docs, and files in languages it does
  not parse (assembly, ...), which can hold real uses.
- `search_text` over every text file: 35 ms median for a whole-word query,
  31 ms for the regex `TODO|FIXME|XXX`.
- Outlines use ~92% fewer tokens than full source; reading one function costs
  a median 0.9% of its file.
- The server answers `initialize` in ~2 ms. After searches over the whole
  corpus it holds ~200 MB resident with the on-disk store (~315 MB without).

Full results for Go, CPython and tokio are in [`BENCHMARKS.md`](BENCHMARKS.md).

## Installation

Installing takes two steps, on Linux and Windows x86_64 (run it in Git Bash on
Windows). First the binary, which needs `curl`, not `sudo`:

```sh
curl -fsSL https://raw.githubusercontent.com/aeron2k4/code-scope-mcp/main/scripts/install.sh | bash
```

It checks the latest release against `SHA256SUMS`, saves the binary as
`~/.local/bin/astlens` (`ASTLENS_BIN_DIR` to change) and adds that directory to
`PATH` if missing. To install a specific release:

```sh
curl -fsSL https://raw.githubusercontent.com/aeron2k4/code-scope-mcp/main/scripts/install.sh | bash -s -- --version v0.1.0
```

Then set up the client. This needs `jq`, and `bash` for OpenCode's guard (on
Windows, `winget install jqlang.jq`):

```sh
astlens --install
```

It detects Claude Code and OpenCode and sets up each one it finds. Pass
`--client claude`, `--client opencode`, or `--client all` to choose yourself.
Restart the client afterwards. To upgrade, run both steps again.

### What `--install` changes

All of it is at user scope, so it applies to every project.

Claude Code:

- Registers the MCP server as `astlens`, with its tools always loaded.
- Writes the hook `~/.claude/hooks/astlens-guard.sh` and wires it in
  `~/.claude/settings.json`.
- Adds a short instruction block to `~/.claude/CLAUDE.md`.

OpenCode (v2):

- Adds the server to `mcp.servers.astlens` in `opencode.json` or
  `opencode.jsonc` in the OpenCode config directory, usually `~/.config/opencode`.
  Comments and other settings stay as they are; the file is copied to
  `opencode.json.bak` first. Its tools are exposed directly, not through Code
  Mode, as `astlens_<tool>`.
- OpenCode reads a global `opencode.json`, then `opencode.jsonc`, and a file's
  non-empty `mcp.servers` map replaces the earlier one entirely. The entry
  therefore goes into `opencode.jsonc` when that file lists servers, and
  otherwise into whichever file exists (`opencode.json` first). An astlens
  entry left in the other file by an earlier install is removed.
- Writes the plugin `~/.config/opencode/plugins/astlens-guard/` with the shared
  guard script.
- Adds a short instruction block to `~/.config/opencode/AGENTS.md`.

The hook and plugin block the model from reading or searching source files in
the session's workspace the usual way, and point it to the astlens tools
instead. They deny whole-file `Read`/`read`, the `Grep`/`grep` tool, and
commands such as `cat`, `grep` and `rg` on those files. A read or command
output limited to 300 lines (`ASTLENS_MAX_READ_LINES`) is still allowed, as are
non-code files and paths outside the workspace.

`astlens --install --no-enforce` registers the MCP server only, with no hook,
plugin or instruction block.

## Usage

Once installed, the client starts the server itself, using the session's working
directory as the workspace root.

| Command                                     | Effect                                                |
| ------------------------------------------- | ----------------------------------------------------- |
| `astlens -i`, `--install`                   | Set up Claude Code and/or OpenCode                    |
| `astlens --install --client opencode`       | Set up only OpenCode                                  |
| `astlens --install --no-enforce`            | Register the MCP server only                          |
| `astlens -u`, `--uninstall`                 | Undo `--install`, remove the `PATH` entry and the binary |
| `astlens -s`, `--serve [ROOT]`              | Run the server over stdio                             |
| `astlens -h`, `--help`                      | Show the help                                         |
| `astlens -v`, `--version`                   | Show the version                                      |

`--uninstall` keeps the parse cache in `~/.cache/astlens`.

## Tools

| Tool               | Purpose                                                                          |
| ------------------ | -------------------------------------------------------------------------------- |
| `get_file_outline` | Symbols in a file with signatures and line ranges                                |
| `search_symbols`   | Ranked name search across the workspace (exact > prefix > substring)             |
| `read_symbol`      | Exact source of one definition, line-numbered                                    |
| `replace_symbol`   | Rewrite or delete one definition without reading the file; rejects syntax errors |
| `replace_lines`    | Replace/delete a line range, guarded by the lines as they are now               |
| `insert_symbol`    | Add code after/before a symbol or at the end of a file; rejects syntax errors    |
| `read_file_pruned` | File text within `max_tokens`: full → bodies elided → outline → truncated        |
| `find_references`  | Uses of a symbol from identifier tokens (comments and strings skipped)           |
| `search_text`      | Literal or regex search over files, honouring `.gitignore`, skipping hidden ones |
| `list_files`       | Files under a directory, honouring `.gitignore`                                  |
| `changed_symbols`  | Symbols added/modified/removed by uncommitted changes or since a revision        |
| `git_status`       | Working-tree status                                                              |

## Build from source

This needs Rust 1.90 or later. Use it on platforms without a prebuilt binary,
such as macOS and Linux ARM.

```sh
git clone https://github.com/aeron2k4/code-scope-mcp.git
cd code-scope-mcp
cargo build --release
./target/release/astlens --serve /path/to/project
```

To install the build as `~/.cargo/bin/astlens` instead, run
`scripts/install.sh --from-source`, then `astlens --install`.

To contribute, see [`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
