#!/bin/sh
# Install a prebuilt code-scope-mcp release binary.
#
#   VERSION=v0.1.0 INSTALL_DIR=~/.local/bin sh install.sh
#
# Verifies the archive against the release's SHA256SUMS and, when the GitHub
# CLI is available, its build provenance attestation. Never touches Claude's
# configuration: it prints the `claude mcp add` command to run instead.
set -eu

REPO=aeron2k4/code-scope-mcp
INSTALL_DIR=${INSTALL_DIR:-$HOME/.local/bin}

die() {
	echo "error: $*" >&2
	exit 1
}

need() {
	command -v "$1" >/dev/null 2>&1 || die "'$1' is required"
}

need curl
need tar

case "$(uname -s)" in
Linux) os=unknown-linux-musl ;;
Darwin) os=apple-darwin ;;
*) die "unsupported OS $(uname -s); download a release manually from https://github.com/$REPO/releases" ;;
esac

case "$(uname -m)" in
x86_64 | amd64) arch=x86_64 ;;
aarch64 | arm64) arch=aarch64 ;;
*) die "unsupported architecture $(uname -m)" ;;
esac

target="$arch-$os"

if [ -z "${VERSION:-}" ]; then
	latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")
	VERSION=${latest##*/}
	case "$VERSION" in
	v*) ;;
	*) die "could not resolve the latest release" ;;
	esac
fi

name="code-scope-mcp-$VERSION-$target"
base="https://github.com/$REPO/releases/download/$VERSION"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $name.tar.gz"
curl -fsSL -o "$tmp/$name.tar.gz" "$base/$name.tar.gz"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"

expected=$(awk -v f="$name.tar.gz" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || die "$name.tar.gz is not listed in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
	actual=$(sha256sum "$tmp/$name.tar.gz" | awk '{ print $1 }')
else
	actual=$(shasum -a 256 "$tmp/$name.tar.gz" | awk '{ print $1 }')
fi
[ "$expected" = "$actual" ] || die "checksum mismatch for $name.tar.gz"
echo "Checksum OK"

if command -v gh >/dev/null 2>&1; then
	gh attestation verify "$tmp/$name.tar.gz" --repo "$REPO" >/dev/null ||
		die "build provenance attestation failed for $name.tar.gz"
	echo "Attestation OK"
else
	echo "Skipping attestation check (GitHub CLI 'gh' not installed)"
fi

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$INSTALL_DIR"
cp "$tmp/$name/code-scope-mcp" "$INSTALL_DIR/code-scope-mcp.tmp"
chmod 755 "$INSTALL_DIR/code-scope-mcp.tmp"
mv "$INSTALL_DIR/code-scope-mcp.tmp" "$INSTALL_DIR/code-scope-mcp"

echo "Installed $INSTALL_DIR/code-scope-mcp ($VERSION)"
echo
echo "Register it with Claude Code for every project:"
echo "  claude mcp add --scope user code-scope -- $INSTALL_DIR/code-scope-mcp"
