#!/usr/bin/env bash
# Print the CHANGELOG.md section of one release, without its heading.
#
#   scripts/release-notes.sh v0.5.1 [CHANGELOG.md]
#
# Fails when the changelog has no section for that version, so a release
# cannot go out without its notes.
set -euo pipefail

version=${1:?usage: release-notes.sh VERSION [CHANGELOG]}
version=${version#v}
changelog=${2:-CHANGELOG.md}

notes=$(awk -v v="$version" '
  /^## / { if (found) exit; found = ($2 == v); next }
  found
' "$changelog")

# Drop the blank lines around the section, and join each list item's wrapped
# lines: GitHub shows a release's line breaks as written.
notes=$(printf '%s\n' "$notes" | sed -e '/./,$!d' | awk '
  /^  [^ ]/ && prev != "" { sub(/^ +/, ""); prev = prev " " $0; next }
  { if (started) print prev; prev = $0; started = 1 }
  END { if (started) print prev }
')
notes=${notes%"${notes##*[![:space:]]}"}

if [ -z "$notes" ]; then
  echo "release-notes.sh: no section for $version in $changelog" >&2
  exit 1
fi
printf '%s\n' "$notes"
