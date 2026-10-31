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
claude mcp add --scope user code-scope -- "$BIN"

if [ "$enforce" = 1 ]; then
  mkdir -p "$CLAUDE_DIR/hooks"

  log "Writing hook $HOOK"
  cat >"$HOOK" <<'EOF'
#!/usr/bin/env bash
# PreToolUse guard installed by code-scope-mcp, so Claude uses the code-scope
# MCP tools for source code:
# - Read: deny whole-file reads of source files. A Read with offset/limit is
#   still allowed (needed before Edit).
# - Grep: deny searches that may cover source files inside the session's
#   workspace. Grep over non-code files (a non-code path, or a glob/type that
#   names no source extension) and outside the workspace stays allowed.
input=$(cat)
tool=$(jq -r '.tool_name // empty' <<<"$input")
cwd=$(jq -r '.cwd // empty' <<<"$input")
CODE_EXT='\.(rs|py|js|jsx|ts|tsx|go)$'
CODE_GLOB='(rs|py|js|jsx|ts|tsx|go)([,}]|$)'
CODE_TYPES='^(rust|py|python|js|jsx|ts|tsx|typescript|go)$'

deny() {
  jq -n --arg r "$1" '{hookSpecificOutput: {hookEventName: "PreToolUse",
    permissionDecision: "deny", permissionDecisionReason: $r}}'
  exit 0
}

case "$tool" in
  Read)
    f=$(jq -r '.tool_input.file_path // empty' <<<"$input")
    ranged=$(jq -r '(.tool_input.offset != null) or (.tool_input.limit != null)' <<<"$input")
    [[ "$f" =~ $CODE_EXT ]] || exit 0
    [ "$ranged" = "true" ] && exit 0
    deny "Whole-file Read of source code is blocked. Use code-scope MCP tools instead (load with ToolSearch \"select:mcp__code-scope__get_file_outline,mcp__code-scope__read_symbol,mcp__code-scope__search_symbols,mcp__code-scope__read_file_pruned\"): get_file_outline / read_symbol / read_file_pruned for $f. If you must Edit, Read only the needed lines with offset+limit."
    ;;
  Grep)
    path=$(jq -r '.tool_input.path // empty' <<<"$input")
    glob=$(jq -r '.tool_input.glob // empty' <<<"$input")
    type=$(jq -r '.tool_input.type // empty' <<<"$input")
    case "$path" in
      "") abs=$cwd ;;
      /*) abs=$path ;;
      *) abs=$cwd/$path ;;
    esac
    # Outside the workspace code-scope cannot help.
    [ -n "$cwd" ] && case "$abs/" in "$cwd"/*) ;; *) exit 0 ;; esac
    if [ -f "$abs" ]; then
      [[ "$abs" =~ $CODE_EXT ]] || exit 0
    elif [ -n "$type" ]; then
      [[ "$type" =~ $CODE_TYPES ]] || exit 0
    elif [ -n "$glob" ]; then
      [[ "$glob" =~ $CODE_GLOB ]] || exit 0
    fi
    deny "Grep over source code is blocked. Use code-scope MCP tools instead (load with ToolSearch \"select:mcp__code-scope__search_text,mcp__code-scope__find_references\"): search_text for a literal or regex (.gitignore-aware, shows the enclosing symbol; takes dir), find_references for uses of a symbol. Grep stays allowed for non-code files: pass a non-code path, or a glob/type such as \"*.md\"."
    ;;
esac
EOF
  chmod +x "$HOOK"

  log "Adding hook to $SETTINGS"
  [ -f "$SETTINGS" ] || echo '{}' >"$SETTINGS"
  cp "$SETTINGS" "$SETTINGS.bak"
  strip_hook
  tmp=$(mktemp)
  jq --arg cmd "$HOOK" '
    .hooks.PreToolUse = ((.hooks.PreToolUse // []) +
      [{matcher: "Read|Grep", hooks: [{type: "command", command: $cmd}]}])' "$SETTINGS" >"$tmp"
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
