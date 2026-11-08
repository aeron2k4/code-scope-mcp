# astlens

A small, fast MCP server in Rust for AST-aware code navigation. It uses
tree-sitter to find symbols and trims file contents to fit a token budget, so
AI coding agents can explore large codebases without reading whole files.

Supported languages: Rust, Python, JavaScript, TypeScript/TSX, Go, C, C++, Java,
C#, GDScript. `.h` headers are parsed as C, or as C++ when C fails.

## Performance

On the Go 1.27.3 standard library (8,236 source files, 82 MB), with a warm cache:

- `search_symbols`: 15 ms; after a restart, 72 ms from the on-disk store
  versus 2.6 s cold.
- `find_references`: 14 ms. It skips 49% of the lines a whole-word
  `grep -w` matches: comments, strings, docs, and files in languages it does
  not parse (assembly, ...), which can hold real uses.
- `search_text` over every text file: 42 ms median for a whole-word query,
  34 ms for the regex `TODO|FIXME|XXX`.
- Outlines use 81% fewer tokens than full source; reading one function costs
  a median 0.9% of its file.
- The server answers `initialize` in ~3 ms. After searches over the whole
  corpus it holds ~240 MB resident with the on-disk store (~410 MB without).

On the Linux kernel (66,075 files, 1 GB), a warm `search_symbols` over the
whole tree takes 108 ms and `find_references` about 1 s. The server walks
20,000 files by default; raise `ASTLENS_MAX_FILES` for a tree that large.

Full results for Go, CPython, tokio, the Linux kernel and VS Code are in
[`BENCHMARKS.md`](BENCHMARKS.md).

## Installation

Installing takes two steps, on Linux x86_64, macOS (Apple silicon and Intel)
and Windows x86_64 (run it in Git Bash on Windows). First the binary, which
needs `curl`, not `sudo`:

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
| `codebase_map`     | The symbols the rest of the code uses most, by file, within `max_tokens`         |
| `get_file_outline` | Symbols in a file with signatures and line ranges                                |
| `search_symbols`   | Ranked name search across the workspace (exact > prefix > substring)             |
| `read_symbol`      | Exact source of one definition, line-numbered                                    |
| `read_lines`       | A line range of a file, line-numbered, at most 400 lines per call                |
| `replace_symbol`   | Rewrite or delete one definition without reading the file; rejects syntax errors |
| `replace_text`     | Exact-text edits, each matching once, applied all or none; rejects syntax errors |
| `replace_lines`    | Replace/delete a line range, guarded by the lines as they are now                |
| `insert_symbol`    | Add code after/before a symbol or at the end of a file; rejects syntax errors    |
| `read_file_pruned` | File text within `max_tokens`: full → bodies elided → outline → truncated        |
| `find_references`  | Uses of a symbol from identifier tokens (comments and strings skipped)           |
| `search_text`      | Literal or regex search over files, honouring `.gitignore`, skipping hidden ones |
| `list_files`       | Files under a directory, honouring `.gitignore`                                  |
| `changed_symbols`  | Symbols added/modified/removed by uncommitted changes or since a revision        |
| `git_status`       | Working-tree status                                                              |

Files in other languages are plain text: `read_lines`, `read_file_pruned` (the
text, cut at `max_tokens`), `replace_text`, `replace_lines` and `insert_symbol`
at the end of the file work on them, without the syntax check.

`codebase_map` ranks files by PageRank over the names they mention and the
files defining them, then prints the best symbols of the best files, each under
the symbols enclosing it. A mention only counts where the language lets it
reach the definition: another file of the same language family, outside the
definition's function, past its `static`, `private` or unexported scope, and
qualified or imported the way the language needs (a Go package, a Python
module, a JavaScript import path, the owner of a method). `focus` takes files or
identifiers and ranks around them; `dir` limits the map to a directory.

Paths are relative to the workspace root. The tools that only read
(`get_file_outline`, `read_symbol`, `read_lines`, `read_file_pruned`, and
`search_text`, `list_files`, `find_references` through `dir`) also take the
absolute or `~/` path of a dependency source. They look under these
directories, when they exist:
- `~/.cargo/registry/src`, `~/.cargo/git/checkouts` (`CARGO_HOME`)
- `~/.rustup/toolchains` (`RUSTUP_HOME`)
- the Go module cache (`GOMODCACHE`, `GOPATH`)
- `/usr/include`, `/usr/local/include`
- `node_modules` and the `python3*` directories in `/usr/lib` and
  `/usr/local/lib`, and the `python3*` directories in `~/.local/lib`

`ASTLENS_LIBRARY_PATHS` adds more directories, separated like `PATH`. The tools
that write stay inside the workspace.

## Build from source

This needs Rust 1.90 or later. Use it on platforms without a prebuilt binary,
such as Linux ARM.

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
