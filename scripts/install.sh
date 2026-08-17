#!/bin/sh
# Install the tma binary from a GitHub release tarball.
#
#   curl -fsSL https://raw.githubusercontent.com/pperanich/tmux-agents/main/scripts/install.sh | sh
#
# Honours TMA_VERSION (a tag, default: the latest release) and TMA_INSTALL_DIR (default:
# ~/.local/bin). Never uses sudo: if the install directory needs root, pick another one.
set -eu

REPO="pperanich/tmux-agents"
BASE_URL="${TMA_BASE_URL:-https://github.com/$REPO/releases/download}"
API_URL="${TMA_API_URL:-https://api.github.com/repos/$REPO/releases/latest}"
INSTALL_DIR="${TMA_INSTALL_DIR:-$HOME/.local/bin}"

die() {
	printf 'install.sh: %s\n' "$*" >&2
	exit 1
}

say() {
	printf '%s\n' "$*"
}

# uname pairs to release-tarball targets. Linux ships musl builds, which are static and so
# do not care which libc the distro has.
detect_target() {
	os=$(uname -s)
	arch=$(uname -m)
	case "$os $arch" in
	'Darwin arm64' | 'Darwin aarch64') echo aarch64-apple-darwin ;;
	'Darwin x86_64') echo x86_64-apple-darwin ;;
	'Linux aarch64' | 'Linux arm64') echo aarch64-unknown-linux-musl ;;
	'Linux x86_64' | 'Linux amd64') echo x86_64-unknown-linux-musl ;;
	*) die "no prebuilt binary for $os $arch. Build one with: cargo install --git https://github.com/$REPO tma" ;;
	esac
}

download() {
	case "$downloader" in
	curl) curl -fsSL "$1" -o "$2" ;;
	wget) wget -q -O "$2" "$1" ;;
	esac
}

# The tag_name out of the releases/latest payload, without depending on jq. Splitting on
# commas first keeps the match off any other quoted field on the same line.
latest_tag() {
	download "$API_URL" "$workdir/latest.json" ||
		die "could not reach $API_URL. Set TMA_VERSION to install a specific tag."
	tr ',' '\n' <"$workdir/latest.json" |
		sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
		head -n 1
}

checksum() {
	case "$sha_cmd" in
	sha256sum) sha256sum "$1" ;;
	shasum) shasum -a 256 "$1" ;;
	esac | cut -d' ' -f1
}

# Copy one completion script into place. Never fatal: the binary is already installed by the time
# these run, and an unwritable completion directory is not a reason to fail the install.
install_completion() {
	[ -f "$1" ] || return 0
	if mkdir -p "$(dirname "$2")" 2>/dev/null && cp "$1" "$2" 2>/dev/null; then
		say "Installed completions to $2"
	else
		say "Could not write $2 (skipped)"
	fi
}

# Completions for the shells that are actually on this machine, in each one's per-user directory.
# Tarballs cut before completions existed carry none, so an absent directory is a silent no-op.
install_completions() {
	[ -z "${TMA_NO_COMPLETIONS:-}" ] || return 0
	src="$workdir/$name/completions"
	[ -d "$src" ] || return 0
	data="${XDG_DATA_HOME:-$HOME/.local/share}"
	if command -v bash >/dev/null 2>&1; then
		install_completion "$src/tma.bash" "$data/bash-completion/completions/tma"
	fi
	if command -v fish >/dev/null 2>&1; then
		install_completion "$src/tma.fish" "${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions/tma.fish"
	fi
	command -v zsh >/dev/null 2>&1 || return 0
	fdir="$data/zsh/site-functions"
	install_completion "$src/_tma" "$fdir/_tma"
	# zsh reads a completion file only from a directory on $fpath, and this one is not there by
	# default. Say so rather than leaving a file that silently does nothing.
	zsh -c 'print -l -- $fpath' 2>/dev/null | grep -qxF "$fdir" ||
		say "zsh: for these to load, add to ~/.zshrc above compinit: fpath=($fdir \$fpath)"
}

command -v tar >/dev/null 2>&1 || die "tar is required but not on PATH"

if command -v curl >/dev/null 2>&1; then
	downloader=curl
elif command -v wget >/dev/null 2>&1; then
	downloader=wget
else
	die "curl or wget is required but neither is on PATH"
fi

if command -v sha256sum >/dev/null 2>&1; then
	sha_cmd=sha256sum
elif command -v shasum >/dev/null 2>&1; then
	sha_cmd=shasum
else
	die "sha256sum or shasum is required but neither is on PATH"
fi

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT INT TERM

target=$(detect_target)
tag="${TMA_VERSION:-$(latest_tag)}"
[ -n "$tag" ] || die "could not determine the latest release tag. Set TMA_VERSION."

name="tma-$tag-$target"
say "Installing $name"

download "$BASE_URL/$tag/$name.tar.gz" "$workdir/$name.tar.gz" ||
	die "could not download $BASE_URL/$tag/$name.tar.gz"
download "$BASE_URL/$tag/SHA256SUMS" "$workdir/SHA256SUMS" ||
	die "could not download $BASE_URL/$tag/SHA256SUMS"

expected=$(awk -v file="$name.tar.gz" '$2 == file { print $1 }' "$workdir/SHA256SUMS")
[ -n "$expected" ] || die "SHA256SUMS has no entry for $name.tar.gz"
actual=$(checksum "$workdir/$name.tar.gz")
[ "$expected" = "$actual" ] || die "checksum mismatch for $name.tar.gz: expected $expected, got $actual"

tar xzf "$workdir/$name.tar.gz" -C "$workdir"
[ -f "$workdir/$name/tma" ] || die "$name.tar.gz does not contain $name/tma"

mkdir -p "$INSTALL_DIR"
# Staged then renamed: overwriting the binary in place fails while a tma is running.
cp "$workdir/$name/tma" "$INSTALL_DIR/tma.new"
chmod +x "$INSTALL_DIR/tma.new"
mv -f "$INSTALL_DIR/tma.new" "$INSTALL_DIR/tma"

say "Installed $("$INSTALL_DIR/tma" --version) to $INSTALL_DIR/tma"
install_completions
case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*) say "$INSTALL_DIR is not on your PATH. Add: export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
say "Next: run 'tma init' to wire up your agents."
