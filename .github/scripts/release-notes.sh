#!/usr/bin/env bash
# Prints the section of CHANGELOG.md for the version given as $1, without
# its heading. Fails if the section is missing or empty.
set -euo pipefail
version=$1
notes=$(awk -v heading="## [$version]" '
  index($0, heading) == 1 { found = 1; next }
  found && /^## \[/ { exit }
  found && /^\[[^]]+\]: / { next }
  found { print }
' CHANGELOG.md | sed -e '/./,$!d' | sed -e ':a' -e '/^\n*$/{$d;N;ba' -e '}')
if [ -z "$notes" ]; then
  echo "CHANGELOG.md has no notes for $version" >&2
  exit 1
fi
printf '%s\n' "$notes"
