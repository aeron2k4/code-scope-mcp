# Changelog

All notable changes to astlens. Versions follow [Semantic Versioning](https://semver.org/).

## Unreleased

### Removed

- Dependabot no longer opens update pull requests.

## 0.5.1 (2026-11-08)

### Changed

- `codebase_map` weighs private symbols down in full: a private helper used as
  much as a public function no longer outranks it, whatever language it is in
  (except C and C++, where `static` already keeps it out of other files).

### Added

- The read tools open the Go standard library (`GOROOT/src`, else
  `/usr/local/go`, `/usr/lib/go`, Homebrew's or `~/sdk/go*`).

### Fixed

- A Go method with an unnamed generic receiver, such as `func (Pair[K, V]) Len()`,
  belongs to its type.
- A Go name defined once per platform in one directory counts once, for its
  most common build, instead of for `js`, `plan9` or `windows`.

## 0.5.0 (2026-11-08)

### Added

- `codebase_map`: the symbols the rest of the code uses most, grouped by file,
  within a token budget. Files are ranked by PageRank over the names they
  mention and the files defining them. `focus` ranks around given files or
  identifiers; `dir` limits the map to a directory.
- It maps trees larger than the file limit (the shallowest files first), C tags
  and headers, and links a mention to the nearest definition when several
  exist.
- Private symbols score below public ones used as much.

### Fixed

- Go and Python names are qualified by package or module, so two packages
  defining the same name are told apart. Go methods belong to their receiver
  type.
- JavaScript and TypeScript imports are told apart by path, package and
  declared module.

## 0.4.0 (2026-11-08)

### Added

- GDScript support.
- Top-level constants and variables are indexed as symbols.
- Files in other languages are handled as plain text by `read_lines`,
  `read_file_pruned`, `replace_text`, `replace_lines` and `insert_symbol` at the
  end of the file, without the syntax check.
- `search_text` takes `context` to show lines around each hit.
- `dir` may name a single file.
- A refused edit reports where its first syntax error is.

### Changed

- Warm symbol search keeps names apart from the rest of a parse, so it fits
  trees as large as the Linux kernel in memory.

## 0.3.1 (2026-11-08)

### Changed

- macOS gets one universal binary for Apple silicon and Intel.

### Fixed

- Windows paths are shown without the `\\?\` prefix.

## 0.3.0 (2026-11-08)

### Added

- `read_lines`: a line range of a file, at most 400 lines per call.
- `replace_text`: exact-text edits, each matching once, applied all or none.
- The read tools open dependency sources by absolute or `~/` path: the cargo
  registry, rustup toolchains, the Go module cache, system headers,
  `node_modules` and Python's library directories. `ASTLENS_LIBRARY_PATHS` adds
  more.

## 0.2.2 (2026-11-08)

### Fixed

- OpenCode: the guard no longer blocks the session, times out on its own, and
  denies whatever the user's permission rules say. The server entry goes into
  the config file whose `mcp.servers` takes effect, and the workspace root is
  kept when OpenCode's shell starts elsewhere.
- `replace_lines` keeps blank lines as written in `expect` and in the new text.
- The on-disk store checks the blob hash before copying an entry, and stores
  strings length-first instead of NUL-terminated.

## 0.2.1 (2026-11-07)

### Added

- `replace_lines`: replace or delete a line range, guarded by the lines as they
  are now.

### Changed

- Parses are stored in a compact binary form, and files are parsed in chunks
  per worker: a faster cold search and restart.

### Fixed

- `search_symbols` returns the best matches, not the first files to match.
- Text and file searches stay inside the workspace cache.

## 0.2.0 (2026-11-06)

### Added

- OpenCode support: `astlens --install` registers the server in OpenCode and
  installs a guard plugin. `--client` picks Claude Code, OpenCode or both.

## 0.1.1 (2026-11-03)

### Fixed

- A `.h` file that fails to parse as C is parsed as C++.
- Symbol queries stop when they run out of time and say the list is cut off.
- `read_symbol`, `replace_symbol` and `insert_symbol` keep other code on the
  symbol's first and last lines.
- Functions sharing one declaration are separate symbols.
- `search_text` uses bounded memory, matches CRLF line ends and reports skipped
  files. `changed_symbols` stops at 2 MB of diff.
- `read_file_pruned` never returns nothing.
- Edits refuse read-only files and FIFOs.
- `git_status` is correct in subdirectories, and diff paths with spaces are read
  correctly.
- A panic in a search worker becomes a tool error instead of a crash.
- A failed install leaves the client registration and user files as they were;
  `--install --no-enforce` removes an earlier hook.
- The hook treats the same file extensions as source as the parser does.

## 0.1.0 (2026-11-03)

First release.

- Tools: `get_file_outline`, `search_symbols`, `read_symbol`,
  `read_file_pruned`, `replace_symbol`, `insert_symbol`, `find_references`,
  `search_text`, `list_files`, `changed_symbols` and `git_status`.
- Languages: Rust, Python, JavaScript, TypeScript/TSX, Go, C, C++, Java and C#.
- Lazy parsing with an in-memory cache bounded by memory, and an LMDB store on
  disk validated by git blob hash, so a restart does not re-parse.
- `astlens --install` registers the server with Claude Code and installs a
  guard hook that sends source reads and searches to the astlens tools.
- Prebuilt binaries for Linux, macOS and Windows with `SHA256SUMS`, and
  `scripts/install.sh` to install them.
