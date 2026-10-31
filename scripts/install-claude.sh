#!/usr/bin/env bash
# Install code-scope-mcp into Claude Code and make Claude use it for source code.
#
#   scripts/install-claude.sh              build, register, enforce
#   scripts/install-claude.sh --no-enforce build and register only
#   scripts/install-claude.sh --uninstall  undo everything this script did
#
# Enforcement = a PreToolUse hook that denies whole-file Read of source files
# (Read with offset/limit stays allowed, since Edit needs a prior Read) and Grep
# over source files in the workspace, plus a short instruction block in
# ~/.claude/CLAUDE.md.
set -euo pipefail

REPO_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CLAUDE_DIR=${CLAUDE_CONFIG_DIR:-$HOME/.claude}
CARGO_BIN=${CARGO_HOME:-$HOME/.cargo}/bin
BIN=$CARGO_BIN/code-scope-mcp
HOOK=$CLAUDE_DIR/hooks/code-scope-guard.sh
SETTINGS=$CLAUDE_DIR/settings.json
MEMORY=$CLAUDE_DIR/CLAUDE.md
BEGIN='<!-- code-scope:begin -->'
END='<!-- code-scope:end -->'

log() { printf '==> %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' not found in PATH"; }

mode=install
enforce=1
for arg in "$@"; do
  case "$arg" in
    --uninstall) mode=uninstall ;;
    --no-enforce) enforce=0 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) die "unknown option: $arg" ;;
  esac
done

need claude
need jq

# Drop the code-scope block from CLAUDE.md, if present.
strip_memory() {
  [ -f "$MEMORY" ] || return 0
  local tmp
  tmp=$(mktemp)
  awk -v b="$BEGIN" -v e="$END" '$0==b{skip=1;next} $0==e{skip=0;next} !skip' "$MEMORY" >"$tmp"
  mv "$tmp" "$MEMORY"
}

# Drop our hook entry from settings.json, leaving other hooks untouched.
strip_hook() {
  [ -f "$SETTINGS" ] || return 0
  local tmp
  tmp=$(mktemp)
  jq --arg cmd "$HOOK" '
    if .hooks.PreToolUse then
      .hooks.PreToolUse |= map(.hooks |= map(select(.command != $cmd)) | select(.hooks | length > 0))
      | if (.hooks.PreToolUse | length) == 0 then del(.hooks.PreToolUse) else . end
      | if (.hooks | length) == 0 then del(.hooks) else . end
    else . end' "$SETTINGS" >"$tmp"
  mv "$tmp" "$SETTINGS"
}

if [ "$mode" = uninstall ]; then
  log "Removing MCP server registration"
  claude mcp remove --scope user code-scope >/dev/null 2>&1 || true
  log "Removing hook and CLAUDE.md block"
  strip_hook
  rm -f "$HOOK"
  strip_memory
  log "Done. The binary is still at $BIN; remove it with: cargo uninstall code-scope-mcp"
  exit 0
fi

need cargo

log "Building and installing $BIN"
cargo install --path "$REPO_DIR" --locked --force

log "Registering MCP server 'code-scope' (user scope)"
claude mcp remove --scope user code-scope >/dev/null 2>&1 || true
# alwaysLoad keeps the tools in the model's manifest; without it Claude Code
# defers them behind ToolSearch and the model falls back to Read/Grep.
claude mcp add-json --scope user code-scope \
  "$(jq -n --arg cmd "$BIN" '{type: "stdio", command: $cmd, alwaysLoad: true}')"

if [ "$enforce" = 1 ]; then
  mkdir -p "$CLAUDE_DIR/hooks"

  log "Writing hook $HOOK"
  install -m 755 "$REPO_DIR/scripts/code-scope-guard.sh" "$HOOK"

  log "Adding hook to $SETTINGS"
  [ -f "$SETTINGS" ] || echo '{}' >"$SETTINGS"
  cp "$SETTINGS" "$SETTINGS.bak"
  strip_hook
  tmp=$(mktemp)
  jq --arg cmd "$HOOK" '
    .hooks.PreToolUse = ((.hooks.PreToolUse // []) +
      [{matcher: "Read|Grep|Bash", hooks: [{type: "command", command: $cmd}]}])' "$SETTINGS" >"$tmp"
  mv "$tmp" "$SETTINGS"

  log "Adding instructions to $MEMORY"
  touch "$MEMORY"
  strip_memory
  cat >>"$MEMORY" <<EOF
$BEGIN
# Code navigation
When exploring or reading source code, prefer the code-scope MCP tools over Read/Grep:
- \`get_file_outline\` before opening a file; \`search_symbols\` to find a definition.
- \`read_symbol\` to read one function/class instead of the whole file.
- \`read_file_pruned\` when a whole file is needed but may be large.
- \`find_references\` to see where a symbol is used; \`search_text\` instead of Grep.
Use Read/Grep for non-code files or when code-scope fails.
Load the tools with ToolSearch ("select:mcp__code-scope__...") if they are deferred.
$END
EOF
fi

log "Done. Restart Claude Code, then check with: claude mcp list"
