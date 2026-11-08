#!/usr/bin/env bash
# Install the astlens binary. Downloads it with curl on Linux x86_64, macOS
# (Apple silicon and Intel) and Windows x86_64 (in Git Bash); elsewhere builds
# it with cargo. Afterwards, `astlens --install` sets up Claude Code and/or
# OpenCode.
#
#   scripts/install.sh                   download the latest release
#   scripts/install.sh --version v0.1.0  download that release instead
#   scripts/install.sh --from-source     build with cargo instead of downloading
#
# Without a clone: curl -fsSL https://raw.githubusercontent.com/aeron2k4/code-scope-mcp/main/scripts/install.sh | bash -s -- [options]
#
# The binary goes to ~/.local/bin (ASTLENS_BIN_DIR to change), no sudo needed.
# A marked block in the rc file of $SHELL adds that directory to PATH if missing.
# `astlens --uninstall` removes it.
set -euo pipefail

REPO=aeron2k4/code-scope-mcp
# Set when the script runs from a clone; empty when piped from curl.
REPO_DIR=
if [ -f "${BASH_SOURCE[0]:-}" ]; then
  dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
  [ -f "$dir/Cargo.toml" ] && REPO_DIR=$dir
fi
BIN_DIR=${ASTLENS_BIN_DIR:-$HOME/.local/bin}
# Must match the markers in src/install.rs.
BEGIN='# astlens:begin'
END='# astlens:end'

log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
need() { have "$1" || die "'$1' not found in PATH${2:+ ($2)}"; }

source_build=0
version=latest
while [ $# -gt 0 ]; do
  case "$1" in
    --from-source) source_build=1 ;;
    --version) [ $# -ge 2 ] || die "--version needs a tag, e.g. v0.1.0"; version=$2; shift ;;
    --version=*) version=${1#--version=} ;;
    -h|--help) sed -n '2,15p' "${BASH_SOURCE[0]:-$0}" 2>/dev/null || true; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
  shift
done

# Release asset for this OS and CPU, or empty when there is none.
os=$(uname -s)
arch=$(uname -m)
exe=
case "$os" in
  Linux) [ "$arch" = x86_64 ] && asset=astlens-linux ;;
  # A universal binary, for both arm64 and x86_64.
  Darwin) asset=astlens-macos ;;
  MINGW*|MSYS*|CYGWIN*) exe=.exe; [ "$arch" = x86_64 ] && asset=astlens-windows.exe ;;
  *) die "unsupported OS: $os" ;;
esac
asset=${asset:-}

# The rc file of the user's shell, or empty for a shell we don't know.
shell_rc() {
  case "$(basename "${SHELL:-}")" in
    bash)
      # A macOS terminal opens login shells, which read only .bash_profile.
      if [ "$os" = Darwin ]; then echo "$HOME/.bash_profile"; else echo "$HOME/.bashrc"; fi ;;
    zsh) echo "${ZDOTDIR:-$HOME}/.zshrc" ;;
    fish) echo "${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish" ;;
  esac
}

# Write our block to the shell's rc file, replacing an earlier one.
add_to_shell() {
  local bin_dir=$1 rc path_line block='' tmp
  rc=$(shell_rc)
  if [ -z "$rc" ]; then
    log "Unknown shell '${SHELL:-}'; add $bin_dir to PATH yourself"
    return 0
  fi
  case "$rc" in
    *.fish)
      path_line="set -gx PATH \"$bin_dir\" \$PATH" ;;
    *)
      path_line="export PATH=\"$bin_dir:\$PATH\"" ;;
  esac
  mkdir -p "$(dirname "$rc")"
  touch "$rc"

  # PATH may hold the directory only because of our own earlier block: keep it then.
  case ":$PATH:" in
    *":$bin_dir:"*) grep -qxF "$path_line" "$rc" && block=$path_line$'\n' ;;
    *) block=$path_line$'\n' ;;
  esac

  tmp=$(mktemp)
  if grep -qxF "$BEGIN" "$rc" && grep -qxF "$END" "$rc"; then
    awk -v b="$BEGIN" -v e="$END" '$0==b{skip=1;next} $0==e{skip=0;next} !skip' "$rc" >"$tmp"
  else
    cat "$rc" >"$tmp"
  fi
  if [ -n "$block" ]; then
    # A last line without a newline must not run into our marker.
    if [ -n "$(tail -c1 "$tmp")" ]; then echo >>"$tmp"; fi
    printf '%s\n%s%s\n' "$BEGIN" "$block" "$END" >>"$tmp"
  fi
  # Rewrite in place, so that an rc file symlinked from a dotfiles repo stays a symlink.
  cat "$tmp" >"$rc"
  rm -f "$tmp"
  if [ -n "$block" ]; then log "Updated $rc"; fi
}

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

if [ "$source_build" = 0 ] && [ -z "$asset" ]; then
  have cargo || die "no prebuilt binary for $os $arch; install Rust (https://rustup.rs) and rerun with --from-source"
  log "No prebuilt binary for $os $arch, building from source"
  source_build=1
fi

if [ "$source_build" = 1 ]; then
  need cargo "https://rustup.rs"
  BIN_DIR=${CARGO_HOME:-$HOME/.cargo}/bin
  BIN=$BIN_DIR/astlens$exe
  if [ -n "$REPO_DIR" ]; then
    log "Building $REPO_DIR and installing $BIN"
    cargo install --path "$REPO_DIR" --locked --force
  elif [ "$version" = latest ]; then
    log "Building $REPO@main and installing $BIN"
    cargo install --git "https://github.com/$REPO" --branch main --locked --force
  else
    log "Building $REPO@$version and installing $BIN"
    cargo install --git "https://github.com/$REPO" --tag "$version" --locked --force
  fi
else
  need curl
  # macOS has shasum, and sha256sum only on recent versions.
  if have sha256sum; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
  else
    need shasum
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
  fi
  if [ "$version" = latest ]; then
    base="https://github.com/$REPO/releases/latest/download"
  else
    base="https://github.com/$REPO/releases/download/$version"
  fi
  log "Downloading $asset ($version)"
  curl -fsSL -o "$tmpdir/$asset" "$base/$asset" || die "download failed: $base/$asset"
  curl -fsSL -o "$tmpdir/SHA256SUMS" "$base/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
  want=$(awk -v f="$asset" '$2==f || $2=="*"f {print $1}' "$tmpdir/SHA256SUMS")
  [ -n "$want" ] || die "$asset is not listed in SHA256SUMS"
  [ "$(sha256 "$tmpdir/$asset")" = "$want" ] || die "checksum mismatch for $asset"
  BIN=$BIN_DIR/astlens$exe
  mkdir -p "$BIN_DIR"
  install -m 755 "$tmpdir/$asset" "$BIN"
  log "Installed $BIN"
fi

add_to_shell "$BIN_DIR"

log "Done. To set up Claude Code and/or OpenCode, or update their guard after an upgrade, run now:"
log "  $BIN --install"
