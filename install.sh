#!/bin/sh
# webseek installer for Linux and macOS.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Aero123421/WebSeek-CLI/main/install.sh | sh
#
# Environment overrides:
#   WEBSEEK_INSTALL_DIR  where to place the binary (default: /usr/local/bin,
#                        falling back to ~/.local/bin when not writable)
#   WEBSEEK_TAG          release tag to install (default: latest)
#
# The script downloads the release archive and SHA256SUMS.txt, verifies the
# checksum, and puts `webseek` on your PATH.

set -eu

REPO="Aero123421/WebSeek-CLI"
API="https://api.github.com/repos/$REPO/releases"

note() { printf '%s\n' "$*" >&2; }
fail() { note "install.sh: error: $*"; exit 1; }

# --- HTTP helper (curl or wget) ---------------------------------------------
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO- "$1"; }
else
    fail "need curl or wget to download files"
fi

# --- Detect platform ---------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux) os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *) fail "unsupported OS: $os (only Linux and macOS are supported)" ;;
esac
case "$arch" in
    x86_64 | amd64) arch_part="x86_64" ;;
    arm64 | aarch64) arch_part="aarch64" ;;
    *) fail "unsupported architecture: $arch (only x86_64 and arm64 are supported)" ;;
esac
target="${arch_part}-${os_part}"

# --- Resolve release tag -----------------------------------------------------
if [ -n "${WEBSEEK_TAG:-}" ]; then
    tag="$WEBSEEK_TAG"
else
    tag="$(fetch "$API/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
    [ -n "$tag" ] || fail "could not determine the latest release tag (API unreachable?)"
fi
note "Installing webseek $tag ($target)"

asset="webseek-$tag-$target.tar.gz"
base="https://github.com/$REPO/releases/download/$tag"

# --- Download ----------------------------------------------------------------
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT INT TERM

note "Downloading $asset"
fetch "$base/$asset" >"$tmpdir/$asset" || fail "download failed: $base/$asset"
fetch "$base/SHA256SUMS.txt" >"$tmpdir/SHA256SUMS.txt" || fail "download failed: $base/SHA256SUMS.txt"

# --- Verify checksum -----------------------------------------------------------
expected="$(grep -F "$asset" "$tmpdir/SHA256SUMS.txt" | head -n 1 | awk '{print $1}')"
[ -n "$expected" ] || fail "no checksum for $asset in SHA256SUMS.txt"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmpdir/$asset" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmpdir/$asset" | awk '{print $1}')"
else
    fail "need sha256sum or shasum to verify the download"
fi
[ "$expected" = "$actual" ] || fail "checksum mismatch for $asset (expected $expected, got $actual)"
note "Checksum OK"

# --- Extract -------------------------------------------------------------------
tar -xzf "$tmpdir/$asset" -C "$tmpdir" || fail "failed to extract $asset"
[ -f "$tmpdir/webseek" ] || fail "archive did not contain a 'webseek' binary"

# --- Install -------------------------------------------------------------------
install_dir="${WEBSEEK_INSTALL_DIR:-/usr/local/bin}"
if [ ! -d "$install_dir" ] || [ ! -w "$install_dir" ]; then
    if [ -n "${WEBSEEK_INSTALL_DIR:-}" ]; then
        mkdir -p "$install_dir" 2>/dev/null || fail "cannot create $install_dir"
    else
        install_dir="$HOME/.local/bin"
        mkdir -p "$install_dir"
    fi
fi

# Stage inside the destination directory so the final rename stays atomic even
# when the download temp directory is on another filesystem.
staged="$install_dir/.webseek.tmp.$$"
if [ -w "$install_dir" ]; then
    cp "$tmpdir/webseek" "$staged"
    chmod +x "$staged"
    mv -f "$staged" "$install_dir/webseek"
else
    note "Installing to $install_dir (requires sudo)"
    sudo cp "$tmpdir/webseek" "$staged"
    sudo chmod +x "$staged"
    sudo mv -f "$staged" "$install_dir/webseek"
fi

case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) note "Note: $install_dir is not on your PATH. Add it with:"
       note "  export PATH=\"$install_dir:\$PATH\"" ;;
esac

note "Installed webseek $tag to $install_dir/webseek"
note "Try it: webseek search \"rust async runtime\""
