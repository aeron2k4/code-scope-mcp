#!/usr/bin/env bash
# PreToolUse guard installed by code-scope-mcp (scripts/install-claude.sh), so
# Claude uses the code-scope MCP tools for source code:
# - Read: deny whole-file reads of source files. A Read with a limit of at
#   most $CODE_SCOPE_MAX_READ_LINES lines (default 300) is still allowed,
#   since Edit needs a prior Read.
# - Grep, and grep/egrep/fgrep/ugrep/rg/git grep in Bash: deny searches that may
#   cover source files inside the session's working directory. Searches over
#   non-code files (a non-code path, or a glob/type/include naming no source
#   extension), over piped input, or outside the working directory are allowed.
# Bash parsing is best effort: commands it cannot see into (eval, scripts,
# find -exec, xargs) are allowed.
input=$(cat)
tool=$(jq -r '.tool_name // empty' <<<"$input")
cwd=$(jq -r '.cwd // empty' <<<"$input")
[ -n "$cwd" ] || cwd=$PWD

EXTS='rs|py|pyi|js|jsx|mjs|cjs|ts|mts|cts|tsx|go|c|h|cc|cpp|cxx|hh|hpp|hxx|java|cs'
CODE_EXT="\\.($EXTS)\$"
# A glob or extension list naming a source extension: *.rs, *.{ts,tsx}, rs,py
CODE_GLOB="(^|[.,{])($EXTS)([,}]|\$)"
CODE_TYPES='^(rust|py|python|js|jsx|ts|tsx|typescript|go|c|cpp|java|cs|csharp)$'

GREP_HINT='Use code-scope MCP tools instead (load with ToolSearch "select:mcp__code-scope__search_text,mcp__code-scope__find_references"): search_text for a literal or regex (.gitignore-aware, shows the enclosing symbol; takes dir), find_references for uses of a symbol. Searching non-code files stays allowed: pass a non-code path, or a filter such as --include="*.md" / -g "*.md" / type md.'

deny() {
  jq -n --arg r "$1" '{hookSpecificOutput: {hookEventName: "PreToolUse",
    permissionDecision: "deny", permissionDecisionReason: $r}}'
  exit 0
}

# Absolute path of $1 relative to directory $2.
absolute() {
  case "$1" in
    /*) printf '%s' "$1" ;;
    "~"*) printf '%s' "$HOME${1#\~}" ;;
    *) printf '%s/%s' "$2" "$1" ;;
  esac
}

inside_workspace() {
  case "$1/" in "$cwd"/*) return 0 ;; *) return 1 ;; esac
}

# 0 when searching path $1 (relative to dir $2) may touch source files.
path_has_code() {
  local abs
  abs=$(absolute "$1" "$2")
  inside_workspace "$abs" || return 1
  if [[ "$1" == *[*?[]* ]]; then
    # Unexpanded glob: code if it names a source extension or none at all.
    [[ "$1" =~ $CODE_EXT || ! "${1##*/}" =~ \. ]]
    return
  fi
  if [ -d "$abs" ]; then
    return 0
  fi
  [ -f "$abs" ] && [[ "$abs" =~ $CODE_EXT ]]
}

# Split a shell command into words. Unquoted | becomes the word "|", other
# unquoted separators (; & ( ) ` newline, $( ) become ";". Quotes are removed.
lex() {
  local s=$1 i c q='' w='' n=${#1} has=0
  WORDS=()
  for ((i = 0; i < n; i++)); do
    c=${s:i:1}
    if [ -n "$q" ]; then
      if [ "$c" = "$q" ]; then
        q=''
      elif [ "$c" = '\' ] && [ "$q" = '"' ] && ((i + 1 < n)); then
        ((i++)); w+=${s:i:1}
      else
        w+=$c
      fi
      continue
    fi
    case "$c" in
      "'" | '"') q=$c; has=1 ;;
      '\') ((i + 1 < n)) && { ((i++)); w+=${s:i:1}; has=1; } ;;
      ' ' | $'\t') ((has)) && WORDS+=("$w"); w=''; has=0 ;;
      '|' | ';' | '&' | '(' | ')' | '`' | $'\n')
        ((has)) && WORDS+=("$w"); w=''; has=0
        if [ "$c" = '|' ] && [ "${s:i+1:1}" != '|' ] && [ "${s:i-1:1}" != '|' ]; then
          WORDS+=('|')
        else
          WORDS+=(';')
        fi
        ;;
      '$')
        if [ "${s:i+1:1}" = '(' ]; then
          ((has)) && WORDS+=("$w"); w=''; has=0; WORDS+=(';'); ((i++))
        else
          w+=$c; has=1
        fi
        ;;
      *) w+=$c; has=1 ;;
    esac
  done
  ((has)) && WORDS+=("$w")
}

# Decide one search command. $1 = dir it runs in, $2 = 1 when fed by a pipe,
# remaining args = argv starting at the program name.
check_search() {
  local dir=$1 piped=$2
  shift 2
  local prog=${1##*/}
  shift
  local kind=grep
  case "$prog" in
    rg) kind=rg ;;
    git) kind=git; shift ;; # skip "grep"
  esac

  local -a filters=() types=() operands=()
  local pattern_given=0 recursive=0 a v opts=1
  # Short options that take a value, per tool.
  local short_vals='efmABCdDgtO'
  [ "$kind" = rg ] && short_vals='efgtTmABCMjrEd'
  [ "$kind" = git ] && short_vals='efmABCO'

  while (($#)); do
    a=$1
    shift
    if ((opts)) && [ "$a" = -- ]; then
      opts=0
      continue
    fi
    if ((opts)) && [[ "$a" == --* ]]; then
      local name=${a%%=*} val=
      if [[ "$a" == *=* ]]; then
        val=${a#*=}
      fi
      case "$name" in
        --include | --glob | --iglob | --file-extensions | --type | --type-not | \
          --regexp | --file | --exclude | --exclude-dir | --include-dir | --max-count | \
          --context | --after-context | --before-context | --max-depth | --replace | \
          --type-add | --sort | --sortr | --color | --colors | --encoding | --threads | \
          --max-columns | --directories | --devices | --label | --binary-files)
          if [ -z "$val" ] && [[ "$a" != *=* ]]; then
            val=$1
            shift
          fi
          ;;
      esac
      case "$name" in
        --include | --glob | --iglob | --file-extensions) filters+=("$val") ;;
        --type) types+=("$val") ;;
        --regexp | --file) pattern_given=1 ;;
        --recursive | --dereference-recursive) recursive=1 ;;
        --directories) [ "$val" = recurse ] && recursive=1 ;;
      esac
      continue
    fi
    if ((opts)) && [[ "$a" == -?* ]]; then
      local k ch rest
      for ((k = 1; k < ${#a}; k++)); do
        ch=${a:k:1}
        if [[ "$short_vals" == *"$ch"* ]]; then
          rest=${a:k+1}
          if [ -n "$rest" ]; then
            v=$rest
          else
            v=$1
            shift
          fi
          case "$ch" in
            e | f) pattern_given=1 ;;
            g) filters+=("$v") ;;
            O) [ "$kind" != git ] && filters+=("$v") ;;
            t) types+=("$v") ;;
            d) [ "$kind" = grep ] && [ "$v" = recurse ] && recursive=1 ;;
          esac
          break
        fi
        case "$ch" in r | R) [ "$kind" = grep ] && recursive=1 ;; esac
      done
      continue
    fi
    operands+=("$a")
  done

  local -a paths
  if ((pattern_given)); then
    paths=("${operands[@]}")
  else
    paths=("${operands[@]:1}")
  fi
  if ((${#paths[@]} == 0)); then
    case "$kind" in
      git) paths=(.) ;;
      rg) ((piped)) && return 1; paths=(.) ;;
      grep) { ((piped)) || ((!recursive)); } && return 1; paths=(.) ;;
    esac
  fi

  # Searches entirely outside the workspace are never blocked.
  local f inside=0
  for f in "${paths[@]}"; do
    inside_workspace "$(absolute "$f" "$dir")" && inside=1
  done
  ((inside)) || return 1

  # Filters decide on their own: any source filter denies, otherwise allow.
  local has_filter=0
  for f in "${filters[@]}"; do
    [[ "$f" == '!'* ]] && continue # exclusion glob
    has_filter=1
    [[ "$f" =~ $CODE_GLOB || "$f" =~ $CODE_EXT ]] && return 0
  done
  for f in "${types[@]}"; do
    has_filter=1
    [[ "$f" =~ $CODE_TYPES || "$f" =~ $CODE_GLOB ]] && return 0
  done
  ((has_filter)) && return 1

  for f in "${paths[@]}"; do
    path_has_code "$f" "$dir" && return 0
  done
  return 1
}

check_bash() {
  local cmd
  cmd=$(jq -r '.tool_input.command // empty' <<<"$input")
  [[ "$cmd" =~ (grep|rg) ]] || exit 0
  lex "$cmd"

  local dir=$cwd piped=0 i=0 n=${#WORDS[@]}
  local -a seg
  while ((i <= n)); do
    local w=${WORDS[i]-;}
    if ((i < n)) && [ "$w" != ';' ] && [ "$w" != '|' ]; then
      seg+=("$w")
      ((i++))
      continue
    fi
    if ((${#seg[@]})); then
      # Drop VAR=value prefixes and wrapper commands.
      local j=0
      while ((j < ${#seg[@]})); do
        case "${seg[j]}" in
          *=* | sudo | command | exec | env | time | nice | nohup | rtk | bypass) ((j++)) ;;
          *) break ;;
        esac
      done
      local -a argv=("${seg[@]:j}")
      case "${argv[0]##*/}" in
        cd) dir=$(absolute "${argv[1]:-$HOME}" "$dir") ;;
        grep | egrep | fgrep | ugrep | rg)
          check_search "$dir" "$piped" "${argv[@]}" &&
            deny "Searching source code with ${argv[0]##*/} is blocked. $GREP_HINT"
          ;;
        git)
          [ "${argv[1]-}" = grep ] && check_search "$dir" 0 "${argv[@]}" &&
            deny "Searching source code with git grep is blocked. $GREP_HINT"
          ;;
      esac
    fi
    seg=()
    piped=0
    [ "$w" = '|' ] && piped=1
    ((i++))
  done
}

case "$tool" in
  Read)
    f=$(jq -r '.tool_input.file_path // empty' <<<"$input")
    limit=$(jq -r '.tool_input.limit // empty' <<<"$input")
    [[ "$f" =~ $CODE_EXT ]] || exit 0
    max=${CODE_SCOPE_MAX_READ_LINES:-300}
    [[ "$max" =~ ^[0-9]+$ ]] || max=300
    # Without a limit, Read returns up to 2000 lines even from an offset.
    if [[ "$limit" =~ ^[0-9]+$ ]] && ((limit <= max)); then
      exit 0
    fi
    deny "Whole-file Read of source code (or a Read without a limit of at most $max lines) is blocked. Use code-scope MCP tools instead (load with ToolSearch \"select:mcp__code-scope__get_file_outline,mcp__code-scope__read_symbol,mcp__code-scope__search_symbols,mcp__code-scope__read_file_pruned\"): get_file_outline / read_symbol / read_file_pruned for $f. To rewrite a whole function or class use replace_symbol, to add one use insert_symbol; for a smaller Edit, Read only the needed lines with offset and a limit of at most $max."
    ;;
  Grep)
    path=$(jq -r '.tool_input.path // empty' <<<"$input")
    glob=$(jq -r '.tool_input.glob // empty' <<<"$input")
    type=$(jq -r '.tool_input.type // empty' <<<"$input")
    abs=$(absolute "${path:-.}" "$cwd")
    inside_workspace "$abs" || exit 0
    if [ -f "$abs" ]; then
      [[ "$abs" =~ $CODE_EXT ]] || exit 0
    elif [ -n "$type" ]; then
      [[ "$type" =~ $CODE_TYPES ]] || exit 0
    elif [ -n "$glob" ]; then
      [[ "$glob" =~ $CODE_GLOB || "$glob" =~ $CODE_EXT ]] || exit 0
    fi
    deny "Grep over source code is blocked. $GREP_HINT"
    ;;
  Bash) check_bash ;;
esac
exit 0
