#!/usr/bin/env bash
# PreToolUse guard installed by astlens (`astlens --install`), so
# Claude uses the astlens MCP tools for source code:
# - Read: deny whole-file reads of source files inside the working directory.
#   Files elsewhere, such as dependency sources, can be read. A Read with a limit of at
#   most $ASTLENS_MAX_READ_LINES lines (default 300) is still allowed,
#   since Edit needs a prior Read.
# - Grep, and grep/egrep/fgrep/ugrep/rg/git grep in Bash: deny searches that may
#   cover source files inside the session's working directory. Searches over
#   non-code files (a non-code path, or a glob/type/include naming no source
#   extension), over piped input, or outside the working directory are allowed.
# - cat/tac/nl/more/less/most/bat/head/tail/sed/awk and rtk read/smart in Bash:
#   deny reading source files inside the working directory, the same as Read.
#   Output limited to at most $ASTLENS_MAX_READ_LINES lines (head -n 50,
#   sed -n 1,50p, rtk read --head-lines 50), sent to a file, counted or hashed
#   (cat x.rs | wc -l), and in-place edits (sed -i) are allowed.
# Bash parsing is best effort: commands it cannot see into (eval, scripts,
# find -exec, xargs, "$(cat x.rs)" inside quotes) and other programs that can
# print a file (python, xxd, cut) are allowed.

# Byte semantics: in a UTF-8 locale ${s:i:1} is O(i), which makes lex quadratic.
export LC_ALL=C

# One jq for all fields, NUL-separated since a command can span lines.
# workdir is the directory a command starts in, when not the workspace root
# (OpenCode's shell tool); cwd always stays the workspace root.
for field in tool cwd cmd workdir file limit path glob type; do
  IFS= read -r -d '' "$field"
done < <(jq -j '[.tool_name, .cwd] + (.tool_input | [.command, .workdir, .file_path, .limit, .path, .glob, .type])
  | map(((. // "") | tostring) + "\u0000") | add')
[ -n "$cwd" ] || cwd=$PWD

# Same list as Lang::from_path, which ignores case: rs becomes [rR][sS]. Not
# nocasematch, which would also make GREP count as grep.
EXTS='rs|py|pyi|js|jsx|mjs|cjs|ts|mts|cts|tsx|go|c|h|cc|cpp|cxx|c\+\+|hh|hpp|hxx|h\+\+|ipp|tpp|java|cs|gd|rb|rbw|rake|gemspec|ru|jbuilder|thor'
for pair in aA bB cC dD eE gG hH iI jJ kK lL mM oO pP rR sS tT uU vV wW xX yY; do
  EXTS=${EXTS//${pair:0:1}/[$pair]}
done
CODE_EXT="\\.($EXTS)\$"
# A glob or extension list naming a source extension: *.rs, *.{ts,tsx}, rs,py
CODE_GLOB="(^|[.,{])($EXTS)([,}]|\$)"
CODE_TYPES='^(rust|py|python|js|jsx|ts|tsx|typescript|go|c|cpp|java|cs|csharp|gdscript|ruby)$'

# Quick checks that spare most commands the word-by-word parse: a search or
# reader program as a whole word and, for readers, a source path or a glob.
BOUND='[^[:alnum:]_.-]'
SEARCH_CMD="(^|$BOUND)(grep|egrep|fgrep|ugrep|rg)($BOUND|\$)"
READ_CMD="(^|$BOUND)(cat|tac|nl|more|less|most|bat|batcat|head|tail|sed|awk|gawk|mawk|nawk|read|smart)($BOUND|\$)"
CODE_HINT="[*?[{]|\\.($EXTS)($BOUND|\$)"

MAX_LINES=${ASTLENS_MAX_READ_LINES:-300}
[[ "$MAX_LINES" =~ ^[0-9]+$ ]] || MAX_LINES=300

READ_HINT='Use astlens MCP tools instead (load with ToolSearch "select:mcp__astlens__get_file_outline,mcp__astlens__read_symbol,mcp__astlens__read_lines,mcp__astlens__read_file_pruned,mcp__astlens__replace_text"): get_file_outline / read_symbol / read_lines (a line range) / read_file_pruned. To rewrite a whole function or class use replace_symbol, to add one use insert_symbol, and for any other change use replace_text (exact old text and its replacement; no read needed) or replace_lines.'

GREP_HINT='Use astlens MCP tools instead (load with ToolSearch "select:mcp__astlens__search_text,mcp__astlens__find_references"): search_text for a literal or regex (.gitignore-aware, shows the enclosing symbol; takes dir), find_references for uses of a symbol. Searching non-code files stays allowed: pass a non-code path, or a filter such as --include="*.md" / -g "*.md" / type md.'

# OpenCode reaches the same tools as astlens_<tool>, without ToolSearch.
if [ "${ASTLENS_OPENCODE:-}" = 1 ]; then
  READ_HINT='Use astlens MCP tools instead: astlens_get_file_outline / astlens_read_symbol / astlens_read_lines (a line range) / astlens_read_file_pruned. To rewrite a whole function or class use astlens_replace_symbol, to add one use astlens_insert_symbol, and for any other change use astlens_replace_text (exact old text and its replacement; no read needed) or astlens_replace_lines.'
  GREP_HINT='Use astlens MCP tools instead: astlens_search_text for a literal or regex (.gitignore-aware, shows the enclosing symbol; takes dir), astlens_find_references for uses of a symbol. Searching non-code files stays allowed: pass a non-code path, or a filter such as include "*.md".'
fi

deny() {
  jq -n --arg r "$1" '{hookSpecificOutput: {hookEventName: "PreToolUse",
    permissionDecision: "deny", permissionDecisionReason: $r}}'
  exit 0
}

# Absolute path $1 without . and .. components or repeated slashes, so that
# src/../../etc is not taken for a path inside the workspace.
normalize() {
  local rest=$1/ part out=''
  while [ -n "$rest" ]; do
    part=${rest%%/*}
    rest=${rest#*/}
    case "$part" in
      '' | .) ;;
      ..) out=${out%/*} ;;
      *) out+=/$part ;;
    esac
  done
  printf '%s' "${out:-/}"
}

# Absolute path of $1 relative to directory $2.
absolute() {
  case "$1" in
    /*) normalize "$1" ;;
    "~"*) normalize "$HOME${1#\~}" ;;
    *) normalize "$2/$1" ;;
  esac
}

cwd=$(normalize "$cwd")

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
  local s=$1 i c q='' w='' n=${#1} has=0 d line
  local -a heredocs=()
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
        # Heredoc bodies start after this newline; they are data, not commands.
        if [ "$c" = $'\n' ] && ((${#heredocs[@]})); then
          {
            for d in "${heredocs[@]}"; do
              while IFS= read -r line; do
                ((i += ${#line} + 1))
                [ "${line#"${line%%[!$'\t']*}"}" = "$d" ] && break
              done
            done
          } <<<"${s:i+1}"
          heredocs=()
        fi
        ;;
      '<')
        if [ "${s:i:3}" = '<<<' ]; then
          w+='<<<'; has=1; ((i += 2))
        elif [ "${s:i:2}" = '<<' ]; then
          ((has)) && WORDS+=("$w"); w=''; has=0
          ((i += 2))
          [ "${s:i:1}" = - ] && ((i++))
          while [ "${s:i:1}" = ' ' ] || [ "${s:i:1}" = $'\t' ]; do ((i++)); done
          d=''
          while ((i < n)); do
            case "${s:i:1}" in
              ' ' | $'\t' | $'\n' | ';' | '|' | '&' | ')') break ;;
              "'" | '"' | '\') ;;
              *) d+=${s:i:1} ;;
            esac
            ((i++))
          done
          heredocs+=("$d")
          ((i--))
        else
          w+=$c; has=1
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

# 0 when reading path $1 (relative to dir $2) reads a source file. Unlike
# path_has_code, a directory does not count and a glob is expanded.
is_code_file() {
  local abs m
  abs=$(absolute "$1" "$2")
  inside_workspace "$abs" || return 1
  if [[ "$1" == *[*?[{]* ]]; then
    [[ "$1" =~ $CODE_EXT || "$1" =~ $CODE_GLOB ]] && return 0
    while IFS= read -r m; do
      [[ "$m" =~ $CODE_EXT ]] && [ -f "$m" ] && return 0
    done < <(compgen -G "$abs")
    return 1
  fi
  [ -f "$abs" ] && [[ "$abs" =~ $CODE_EXT ]]
}

# Split redirections off an argument list: ARGS gets the rest, INPUT the file
# after <, and STDOUT_GONE is 1 when standard output goes to a file.
parse_redirects() {
  ARGS=(); INPUT=''; STDOUT_GONE=0
  while (($#)); do
    case "$1" in
      '>' | '>>' | 1'>' | 1'>>') STDOUT_GONE=1; shift ;;
      2'>' | 2'>>' | '<<<') shift ;;
      '<') INPUT=${2-}; shift ;;
      '>'* | 1'>'*) STDOUT_GONE=1 ;;
      2'>'* | '<<<'*) ;;
      '<'*) INPUT=${1#<} ;;
      *) ARGS+=("$1") ;;
    esac
    (($#)) && shift
  done
}

small() { [[ "$1" =~ ^[0-9]+$ ]] && ((10#$1 <= MAX_LINES)); }

# Parse sed arguments (from ARGS): FILES gets the input files, SED_QUIET,
# SED_INPLACE and SCRIPTS the rest.
parse_sed() {
  FILES=(); SCRIPTS=(); SED_QUIET=0; SED_INPLACE=0
  local -a words=()
  local a k opts=1
  set -- "${ARGS[@]}"
  while (($#)); do
    a=$1; shift
    if ((opts)) && [ "$a" = -- ]; then
      opts=0
    elif ((opts)) && [[ "$a" == --* ]]; then
      case "$a" in
        --quiet | --silent) SED_QUIET=1 ;;
        --in-place*) SED_INPLACE=1 ;;
        --expression=*) SCRIPTS+=("${a#*=}") ;;
        --expression) SCRIPTS+=("${1-}"); shift ;;
        --file=*) SCRIPTS+=('') ;;
        --file) SCRIPTS+=(''); shift ;;
        --line-length) shift ;;
      esac
    elif ((opts)) && [[ "$a" == -?* ]]; then
      for ((k = 1; k < ${#a}; k++)); do
        case "${a:k:1}" in
          n) SED_QUIET=1 ;;
          i) SED_INPLACE=1; break ;;
          e) if ((k + 1 < ${#a})); then SCRIPTS+=("${a:k+1}"); else SCRIPTS+=("${1-}"); shift; fi; break ;;
          f) SCRIPTS+=(''); ((k + 1 < ${#a})) || shift; break ;;
          l) ((k + 1 < ${#a})) || shift; break ;;
        esac
      done
    else
      words+=("$a")
    fi
  done
  if ((${#SCRIPTS[@]} == 0)) && ((${#words[@]})); then
    SCRIPTS=("${words[0]}")
    FILES=("${words[@]:1}")
  else
    FILES=("${words[@]}")
  fi
}

# 0 when program $1 with arguments ARGS prints at most MAX_LINES lines per input.
limited_output() {
  local prog=$1 a
  case "$prog" in
    head | tail)
      set -- "${ARGS[@]}"
      while (($#)); do
        a=$1; shift
        case "$a" in
          -n | --lines) small "${1-}" || return 1; shift ;;
          --lines=*) small "${a#*=}" || return 1 ;;
          -n*) small "${a#-n}" || return 1 ;;
          -c* | --bytes*) return 1 ;;
          -[0-9]*) small "${a#-}" || return 1 ;;
        esac
      done
      return 0
      ;;
    sed)
      parse_sed
      ((${#SCRIPTS[@]} == 1)) || return 1
      # sed 10q prints the first 10 lines.
      [[ "${SCRIPTS[0]}" =~ ^([0-9]+)q$ ]] && { small "${BASH_REMATCH[1]}"; return; }
      ((SED_QUIET)) || return 1
      [[ "${SCRIPTS[0]}" =~ ^([0-9]+)(,([0-9]+))?p$ ]] || return 1
      local first=$((10#${BASH_REMATCH[1]})) last=${BASH_REMATCH[3]:-${BASH_REMATCH[1]}}
      ((10#$last >= first)) && small $((10#$last - first + 1))
      ;;
    read) # rtk read
      set -- "${ARGS[@]}"
      while (($#)); do
        a=$1; shift
        case "$a" in
          -m | --max-lines | --head-lines | --tail-lines) small "${1-}" && return 0; shift ;;
          --max-lines=* | --head-lines=* | --tail-lines=*) small "${a#*=}" && return 0 ;;
        esac
      done
      return 1
      ;;
    *) return 1 ;;
  esac
}

# 0 when reader argv $3... must be denied. $1 = dir it runs in, $2 = index of
# the word ending its segment in WORDS.
check_read() {
  local dir=$1 end=$2 prog=$3 a code=0 k
  shift 3
  parse_redirects "$@"
  ((STDOUT_GONE)) && return 1
  local -a inputs=()
  case "$prog" in
    sed)
      parse_sed
      ((SED_INPLACE)) && return 1
      inputs=("${FILES[@]}")
      ;;
    awk | gawk | mawk | nawk)
      local script=1
      set -- "${ARGS[@]}"
      while (($#)); do
        a=$1; shift
        case "$a" in
          -f | --file) script=0; shift ;;
          -i | --include) [ "${1-}" = inplace ] && return 1; shift ;;
          -v | -F | --assign | --field-separator) shift ;;
          -*) ;;
          *) if ((script)); then script=0; else inputs+=("$a"); fi ;;
        esac
      done
      ;;
    *)
      for a in "${ARGS[@]}"; do
        [[ "$a" == -* ]] || inputs+=("$a")
      done
      ;;
  esac
  [ -n "$INPUT" ] && inputs+=("$INPUT")
  for a in "${inputs[@]}"; do
    is_code_file "$a" "$dir" && code=1
  done
  ((code)) || return 1

  limited_output "$prog" && return 1

  # Piped: fine when the next command only counts, hashes or limits the output.
  [ "${WORDS[end]-}" = '|' ] || return 0
  local -a next=()
  for ((k = end + 1; k < ${#WORDS[@]}; k++)); do
    [ "${WORDS[k]}" = ';' ] || [ "${WORDS[k]}" = '|' ] && break
    next+=("${WORDS[k]}")
  done
  command_argv "${next[@]}"
  ((${#ARGV[@]})) || return 0
  case "${ARGV[0]##*/}" in
    wc | sha1sum | sha224sum | sha256sum | sha384sum | sha512sum | md5sum | b2sum | shasum | cksum) return 1 ;;
  esac
  parse_redirects "${ARGV[@]:1}"
  case "${ARGV[0]##*/}" in
    head | tail | sed) limited_output "${ARGV[0]##*/}" && return 1 ;;
  esac
  return 0
}

# Drop VAR=value prefixes, shell keywords and wrapper commands from a command:
# ARGV gets the rest, UNDER_RTK is 1 when rtk was one of the wrappers.
command_argv() {
  local -a seg=("$@")
  local j=0
  UNDER_RTK=0
  while ((j < ${#seg[@]})); do
    case "${seg[j]}" in
      rtk) UNDER_RTK=1; ((j++)) ;;
      *=* | sudo | command | exec | env | time | nice | nohup | bypass | \
        if | then | else | elif | do | while | until | '{' | '!') ((j++)) ;;
      *) break ;;
    esac
  done
  ARGV=("${seg[@]:j}")
}

check_bash() {
  [[ "$cmd" =~ $SEARCH_CMD ]] || { [[ "$cmd" =~ $READ_CMD ]] && [[ "$cmd" =~ $CODE_HINT ]]; } || exit 0
  lex "$cmd"

  local dir=$cwd piped=0 i=0 n=${#WORDS[@]}
  [ -n "$workdir" ] && dir=$(absolute "$workdir" "$cwd")
  local -a seg
  while ((i <= n)); do
    local w=${WORDS[i]-;}
    if ((i < n)) && [ "$w" != ';' ] && [ "$w" != '|' ]; then
      seg+=("$w")
      ((i++))
      continue
    fi
    if ((${#seg[@]})); then
      command_argv "${seg[@]}"
      local -a argv=("${ARGV[@]}")
      local under_rtk=$UNDER_RTK
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
        cat | tac | nl | more | less | most | bat | batcat | head | tail | sed | awk | gawk | mawk | nawk)
          check_read "$dir" "$i" "${argv[0]##*/}" "${argv[@]:1}" &&
            deny "Reading source code with ${argv[0]##*/} is blocked. $READ_HINT To see a few lines, limit the output to at most $MAX_LINES lines, e.g. sed -n 10,60p."
          ;;
        read | smart)
          ((under_rtk)) && check_read "$dir" "$i" "${argv[0]}" "${argv[@]:1}" &&
            deny "Reading source code with rtk ${argv[0]} is blocked. $READ_HINT To see a few lines, use rtk read --head-lines with at most $MAX_LINES."
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
    [[ "$file" =~ $CODE_EXT ]] || exit 0
    inside_workspace "$(absolute "$file" "$cwd")" || exit 0
    # Without a limit, Read returns up to 2000 lines even from an offset.
    small "$limit" && exit 0
    deny "Whole-file Read of $file (or a Read without a limit of at most $MAX_LINES lines) is blocked. $READ_HINT For a smaller Edit, Read only the needed lines with offset and a limit of at most $MAX_LINES."
    ;;
  Grep)
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
