# Changelog

All notable changes to astlens. Versions follow [Semantic Versioning](https://semver.org/).
Each entry ends with the commits that made it.

## Unreleased

### Fixed

- `read_symbol`, `replace_symbol` and `insert_symbol` no longer take code above
  a nested symbol for part of it: a line ending in `)` or `]` below a decorated
  class or function, or a line starting with `*` or `[`. Replacing a method
  could delete its class's decorator, header and fields (1371285).
- `changed_symbols` counts the lines of an untracked file in pieces instead of
  reading it whole, so a large untracked data file no longer takes its size in
  memory (3e7a9d9).
- `read_lines` keeps only the lines it returns in memory, and reads a range of
  a file that is not UTF-8 elsewhere. `read_file_pruned` reads only the start
  of a plain file far over its budget (df64fd3).

### Changed

- `replace_symbol`, `replace_text`, `replace_lines` and `insert_symbol` refuse
  a source file over 2 MB, the size the other tools parse (df64fd3).

## 0.5.2 (2026-11-10)

### Changed

- Release notes come from this changelog, and a release stops when its
  version has no section here (16727e6).

### Removed

- Dependabot no longer opens update pull requests (b4e192e).

### Fixed

- `--install` names only the clients it set up in its closing message
  (8244657).

## 0.5.1 (2026-11-08)

### Changed

- `codebase_map`: a private symbol nothing else uses no longer fills a small
  repository's map from its file's own rank. Only a focus file's symbols count
  in full (fb04299).

### Added

- The read tools open the Go standard library: `GOROOT/src`, else
  `/usr/local/go`, `/usr/lib/go`, `/usr/lib/go-*`, Homebrew's or `~/sdk/go*`
  (fb04299).

## 0.5.0 (2026-11-08)

### Added

- `codebase_map`: the symbols the rest of the code uses most, grouped by file,
  within a token budget. Files are ranked by PageRank over the names they
  mention and the files defining them. `focus` ranks around given files or
  identifiers; `dir` limits the map to a directory (e10a618).
- It maps trees larger than the file limit (the shallowest files first), C tags
  and headers, and links a mention to the nearest definition when several
  exist (1b244a5).
- A private symbol (a Go lower-case name, a Python `_name`, a Rust item without
  `pub`) counts 0.4 of its uses, so public symbols lead. C and C++ are left as
  they are, since including files use their header definitions (9ae8b21).

### Fixed

- A Go use of a name from another package counts only when qualified by that
  package, aliases included. A Python use needs the module imported and the
  name taken from it (705cb27).
- A Go method belongs to its receiver type, so a method named `byte` or
  `string` no longer collects every mention of the builtin (705cb27).
- A name built once per platform in one directory (Go build constraints, Rust
  `cfg`) counts once, for its most common build (705cb27).
- JavaScript and TypeScript imports are told apart by path, package and
  declared module (68de958).

## 0.4.0 (2026-11-08)

### Added

- GDScript support (e1876c8).
- Top-level constants and variables are indexed as symbols (b70e232).
- Files in other languages are handled as plain text by `read_lines`,
  `read_file_pruned`, `replace_text`, `replace_lines` and `insert_symbol` at the
  end of the file, without the syntax check (1e6230a).
- `search_text` takes `context` to show lines around each hit (ce4100a).
- `dir` may name a single file (8098096).
- A refused edit reports where its first syntax error is (9a98878).

### Changed

- Warm symbol search keeps names apart from the rest of a parse, so it fits
  trees as large as the Linux kernel in memory (fb878c3).

## 0.3.1 (2026-11-08)

### Changed

- macOS gets one universal binary for Apple silicon and Intel (823db74).

### Fixed

- Windows paths are shown without the `\\?\` prefix (88ffc18).

## 0.3.0 (2026-11-08)

### Added

- `read_lines`: a line range of a file, at most 400 lines per call (91ec3c7).
- `replace_text`: exact-text edits, each matching once, applied all or none
  (91ec3c7).
- The read tools open dependency sources by absolute or `~/` path: the cargo
  registry, rustup toolchains, the Go module cache, system headers,
  `node_modules` and Python's library directories. `ASTLENS_LIBRARY_PATHS` adds
  more (11fd15a).

## 0.2.2 (2026-11-08)

### Fixed

- OpenCode: the guard runs without blocking and times out on its own
  (169de65), and denies whatever the user's permission rules say (77f0349).
  The server entry goes into the config file whose `mcp.servers` takes effect
  (96a5ce4), and the workspace root is kept when OpenCode's shell starts
  elsewhere (15bf452).
- `replace_lines` keeps blank lines as written in `expect` and in the new text
  (52db9fb).
- The on-disk store checks the blob hash before copying an entry (3af599c),
  and stores strings length-first instead of NUL-terminated (2d9ab20).

## 0.2.1 (2026-11-07)

### Added

- `replace_lines`: replace or delete a line range, guarded by the lines as they
  are now (10e7e66).

### Changed

- Parses are stored in a compact binary form (f6351dd), and files are parsed in
  chunks per worker (9d0b925): a faster cold search and restart.

### Fixed

- `search_symbols` returns the best matches, not the first files to match
  (1d5945b).
- Text and file searches stay inside the workspace cache (e23b39b).

## 0.2.0 (2026-11-06)

### Added

- OpenCode support: `astlens --install` registers the server in OpenCode
  (743a056), reusing its existing config file (247e61e), and installs a guard
  plugin (2f37efa, 2f5d959). `--client` picks Claude Code, OpenCode or both.

## 0.1.1 (2026-11-03)

### Fixed

- A `.h` file that fails to parse as C is parsed as C++ (de3d5e2).
- Symbol queries stop when they run out of time (48df26c) and say the list is
  cut off (2cdb3dc).
- `read_symbol`, `replace_symbol` and `insert_symbol` keep other code on the
  symbol's first and last lines (06c9ecb).
- Functions sharing one declaration are separate symbols (9e0c145).
- `search_text` uses bounded memory, matches CRLF line ends and reports skipped
  files (b5c9561). `changed_symbols` stops at 2 MB of diff (0406de5).
- `read_file_pruned` never returns nothing (c2e5232).
- Edits refuse read-only files and FIFOs, and a delete drops the blank lines it
  leaves (cb96c22).
- `git_status` is correct in subdirectories (3121d59), and diff paths with
  spaces are read correctly (930606a).
- A panic in a search worker becomes a tool error instead of a crash (7358f5f).
- A failed install leaves the client registration and user files as they were
  (951a414); `--install --no-enforce` removes an earlier hook (856afee).
- The hook treats the same file extensions as source as the parser does
  (e0d8c00).

## 0.1.0 (2026-11-03)

First release.

- Tools: `get_file_outline`, `search_symbols`, `read_symbol`,
  `read_file_pruned`, `list_files` and `git_status` (cc43fb3),
  `find_references` and `search_text` (80c0f9e), `replace_symbol` (767d814),
  `insert_symbol` (982aa06) and `changed_symbols` (e8ef1c6).
- Languages: Rust, Python, JavaScript, TypeScript/TSX and Go (198c31d); C, C++,
  Java and C# (1f19b56).
- Lazy parsing (48dc98f) with an in-memory cache bounded by memory (9e4d1e1),
  and an LMDB store on disk validated by git blob hash, so a restart does not
  re-parse (d714b18, 13735f8).
- `ASTLENS_MAX_FILES` sets how many files are walked, and the tools say when
  the limit is hit (35db98a).
- `astlens --install` registers the server with Claude Code (e63651d) and
  installs a guard hook that sends source reads and searches to the astlens
  tools (1719ff0, 722c605, 9ffc9f3).
- Prebuilt binaries for Linux, macOS and Windows with `SHA256SUMS` (9a959bd,
  185ba85), and `scripts/install.sh` to install them (b15218b).
