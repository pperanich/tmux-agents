#!/bin/sh
# Print one release's CHANGELOG.md section, without its heading.
#
#   scripts/changelog-section.sh 0.2.0
#   scripts/changelog-section.sh Unreleased
#
# The release workflow feeds this to `gh release create --notes-file`, so the notes on GitHub and
# the section in the file are the same bytes by construction. Exits 1 when the section is missing or
# holds nothing but whitespace, which is what makes "tagged without a changelog entry" a build
# failure rather than an empty release page.
set -eu

[ $# -eq 1 ] || {
	echo "usage: changelog-section.sh <version|Unreleased>" >&2
	exit 2
}
version=${1#v}
file="$(git rev-parse --show-toplevel)/CHANGELOG.md"

# Everything between this version's `## [x]` heading and the next `## ` heading, minus the link
# definitions the footer carries.
section=$(awk -v want="$version" '
	/^## \[/ {
		# `## [0.2.0] - 2026-08-17` or `## [Unreleased]`: the id is between the brackets.
		id = $0; sub(/^## \[/, "", id); sub(/\].*$/, "", id)
		in_section = (id == want)
		next
	}
	/^\[/ { next }   # trailing link definitions belong to no section
	in_section { print }
' "$file")

# Trim leading and trailing blank lines, then refuse an empty section.
section=$(printf '%s\n' "$section" | sed -e '/./,$!d' | sed -e :a -e '/^\n*$/{$d;N;};/\n$/ba')
[ -n "$section" ] || {
	echo "changelog-section.sh: CHANGELOG.md has no entries under [$version]" >&2
	exit 1
}
printf '%s\n' "$section"
