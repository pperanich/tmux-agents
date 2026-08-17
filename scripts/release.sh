#!/bin/sh
# Cut a release: bump the workspace version, commit it, and tag it.
#
#   mise run release 0.1.2
#
# The tag is what the release workflow builds from, and it refuses a tag that disagrees with the
# workspace version — so the bump and the tag belong in one step. Nothing is pushed: this leaves
# `git push --follow-tags` as the deliberate last move.
#
# Honours RELEASE_SKIP_CHECKS=1 (skip `mise run lint` and `mise run test`) and RELEASE_BRANCH
# (default: main).
set -eu

BRANCH="${RELEASE_BRANCH:-main}"

die() {
	printf 'release.sh: %s\n' "$*" >&2
	exit 1
}

say() {
	printf '%s\n' "$*"
}

# In-place sed without the GNU/BSD -i incompatibility.
sed_i() {
	script=$1
	file=$2
	tmp="$file.release.tmp"
	sed "$script" "$file" >"$tmp" && mv "$tmp" "$file"
}

# Stamp CHANGELOG.md for a release: `## [Unreleased]` becomes `## [<new>] - <today>` with a fresh
# empty `[Unreleased]` above it, and the link definitions at the foot gain a compare line for the new
# tag. awk rather than sed: inserting a line needs `\n` in the replacement, which BSD sed rejects.
stamp_changelog() {
	new=$1
	prev=$2
	tmp=CHANGELOG.md.release.tmp
	awk -v new="$new" -v prev="$prev" -v today="$(date +%Y-%m-%d)" '
		/^## \[Unreleased\]$/ {
			print "## [Unreleased]"
			print ""
			print "## [" new "] - " today
			next
		}
		/^\[Unreleased\]: / {
			base = $2; sub(/\/compare\/.*$/, "", base)
			print "[Unreleased]: " base "/compare/v" new "...HEAD"
			print "[" new "]: " base "/compare/v" prev "...v" new
			next
		}
		{ print }
	' CHANGELOG.md >"$tmp" && mv "$tmp" CHANGELOG.md
	./scripts/changelog-section.sh "$new" >/dev/null || die 'the changelog stamp did not take'
}

# The `version = "…"` of [workspace.package], which is the one the whole workspace inherits.
workspace_version() {
	awk '/^\[workspace\.package\]/ { in_section = 1; next }
	     /^\[/ { in_section = 0 }
	     in_section && /^version = / { gsub(/[",]/, "", $3); print $3; exit }' Cargo.toml
}

[ $# -eq 1 ] || die "usage: release.sh <version>   (e.g. 0.1.2)"
version=${1#v}
case "$version" in
[0-9]*.[0-9]*.[0-9]*) ;;
*) die "'$1' is not a bare semver (0.1.2)" ;;
esac

cd "$(git rev-parse --show-toplevel)" || die 'not a git repository'
[ -z "$(git status --porcelain)" ] || die 'the worktree is dirty; commit or stash first'
current_branch=$(git rev-parse --abbrev-ref HEAD)
[ "$current_branch" = "$BRANCH" ] || die "on '$current_branch', not '$BRANCH' (set RELEASE_BRANCH to override)"
git rev-parse -q --verify "refs/tags/v$version" >/dev/null && die "tag v$version already exists"

old=$(workspace_version)
[ -n "$old" ] || die 'no version found under [workspace.package] in Cargo.toml'
[ "$old" != "$version" ] || die "the workspace is already at $version"

# Before touching anything: the release workflow refuses a tag whose CHANGELOG section is empty, so
# find that out here rather than after the commit and tag are already made.
./scripts/changelog-section.sh Unreleased >/dev/null ||
	die 'CHANGELOG.md has nothing under [Unreleased]; write the entry first'

say "release.sh: $old -> $version"
sed_i "/^\[workspace\.package\]/,/^\[/ s/^version = \"$old\"\$/version = \"$version\"/" Cargo.toml
[ "$(workspace_version)" = "$version" ] || die 'the Cargo.toml bump did not take'

# The docs quote `tma --version` output and the install snippet's tag; a stale sample there reads
# as the current release to anyone following the page.
for doc in docs/tutorial/getting-started.md docs/how-to/install-tma.md; do
	sed_i "s/^tma $old\$/tma $version/; s/TMA_VERSION=v$old/TMA_VERSION=v$version/" "$doc"
done

stamp_changelog "$version" "$old"

# Rewrites Cargo.lock's workspace entries without building anything.
cargo metadata --format-version 1 --no-deps >/dev/null

if [ "${RELEASE_SKIP_CHECKS:-0}" = "1" ]; then
	say 'release.sh: skipping lint and test (RELEASE_SKIP_CHECKS=1)'
else
	mise run lint
	mise run test
fi

git add Cargo.toml Cargo.lock docs CHANGELOG.md
git commit -m "chore(release): v$version"
git tag -a "v$version" -m "v$version"

say ''
say "release.sh: tagged v$version. Push it with:"
say '  git push --follow-tags'
