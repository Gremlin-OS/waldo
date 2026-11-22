#!/usr/bin/env bash
# Print one version's section of CHANGELOG.md, without its heading.
#
#   scripts/changelog-section.sh X.Y.Z
#
# It fails when there is no such section or the section is empty. The
# release workflow uses the output as the release notes.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
version=${1:?usage: changelog-section.sh X.Y.Z}
out=$(awk -v v="$version" '
  /^## \[/ { on = (index($0, "## [" v "]") == 1); next }
  /^\[[^]]+\]: / { next }
  on { print }
' "$root/CHANGELOG.md" | sed -e '/./,$!d')
[ -n "$out" ] || { echo "changelog-section.sh: CHANGELOG.md has no section for [$version], or it is empty" >&2; exit 1; }
printf '%s\n' "$out"
