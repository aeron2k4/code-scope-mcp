# Contributing

## Development

```sh
cargo test
cargo fmt
cargo clippy --all-targets -- -D warnings
scripts/test-guard.sh   # table test for the hook; needs jq
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

## Releasing

Bump `version` in `Cargo.toml`, rename the `Unreleased` section of
[`CHANGELOG.md`](CHANGELOG.md) to `X.Y.Z (date)`, commit, then push a matching
tag:

```sh
git tag -a v0.1.0 -m "astlens 0.1.0"
git push origin v0.1.0
```

The release workflow checks that the tag matches `Cargo.toml` and that
`CHANGELOG.md` has a section for it, runs the tests on Linux and drafts a
GitHub Release with the Linux binary, `SHA256SUMS` and that section as its
notes (`scripts/release-notes.sh vX.Y.Z` prints them). The Windows and macOS
binaries are then built and added to the draft, and only a release with all
three is published, so `scripts/install.sh` never finds the latest release
without the binary it needs. When one of those builds fails the release stays a
draft: fix the build, then run the workflow by hand with the tag. Each binary
has a build provenance, checked with
`gh attestation verify astlens-linux --repo aeron2k4/code-scope-mcp`.
