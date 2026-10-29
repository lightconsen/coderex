#!/bin/sh
# CodeRex installer — cross-platform shell script.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/lightconsen/coderex/main/scripts/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/lightconsen/coderex/main/scripts/install.sh | sh -s -- v0.1.0

set -e

REPO="lightconsen/coderex"
BINARY="coderex"
RELEASE_BASE_URL="${CODEREX_RELEASE_BASE_URL:-https://github.com/$REPO/releases/download}"
API_URL="${CODEREX_API_BASE_URL:-https://api.github.com/repos/$REPO}"

# Determine version
if [ -n "$1" ]; then
    VERSION="$1"
else
    VERSION="latest"
fi

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    linux)
        case "$ARCH" in
            x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64)
                echo "No prebuilt archive for aarch64 Linux yet; build from source: cargo install --git https://github.com/$REPO.git"
                exit 1 ;;
            *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
        esac
        ;;
    darwin)
        case "$ARCH" in
            x86_64) TARGET="x86_64-apple-darwin" ;;
            aarch64|arm64) TARGET="aarch64-apple-darwin" ;;
            *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
        esac
        ;;
    *)
        echo "Unsupported OS: $OS"
        exit 1
        ;;
esac

# Resolve latest version from GitHub API
if [ "$VERSION" = "latest" ]; then
    VERSION=$(curl -fsSL "$API_URL/releases/latest" | \
        grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
    if [ -z "$VERSION" ]; then
        echo "Failed to determine latest version"
        exit 1
    fi
fi

echo "Installing $BINARY $VERSION for $TARGET..."

# Download URL
ARCHIVE="${BINARY}-${TARGET}.tar.gz"
URL="$RELEASE_BASE_URL/$VERSION/$ARCHIVE"
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

# SHA-256 of a file, whichever tool this machine has.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        openssl dgst -sha256 -r "$1" | awk '{print $1}'
    fi
}

curl -fsSL "$URL" -o "$TMPDIR/${BINARY}.tar.gz"

# The release publishes one SHA256SUMS for every asset. A missing file, a
# missing entry or a mismatch stops the install: an unverified download is a
# download from whatever answered the request.
if ! curl -fsSL "$RELEASE_BASE_URL/$VERSION/SHA256SUMS" -o "$TMPDIR/SHA256SUMS"; then
    echo "Could not download SHA256SUMS for $VERSION; refusing to install an unverified download" >&2
    exit 1
fi
EXPECTED=$(awk -v name="$ARCHIVE" '$2 == name { print $1; exit }' "$TMPDIR/SHA256SUMS")
if [ -z "$EXPECTED" ]; then
    echo "SHA256SUMS has no entry for $ARCHIVE; refusing to install" >&2
    exit 1
fi
ACTUAL=$(sha256_of "$TMPDIR/${BINARY}.tar.gz")
if [ "$ACTUAL" != "$EXPECTED" ]; then
    echo "Checksum mismatch for $ARCHIVE: expected $EXPECTED, got $ACTUAL" >&2
    exit 1
fi

tar xzf "$TMPDIR/${BINARY}.tar.gz" -C "$TMPDIR"

# Install to ~/.local/bin or /usr/local/bin
INSTALL_DIR="${INSTALL_DIR:-}"
if [ -z "$INSTALL_DIR" ]; then
    if [ -d "$HOME/.local/bin" ] && echo "$PATH" | grep -q "$HOME/.local/bin"; then
        INSTALL_DIR="$HOME/.local/bin"
    else
        INSTALL_DIR="/usr/local/bin"
    fi
fi

# Create install dir if needed
mkdir -p "$INSTALL_DIR"

# Move binary
cp "$TMPDIR/${BINARY}-${TARGET}" "$INSTALL_DIR/$BINARY"
chmod +x "$INSTALL_DIR/$BINARY"

echo "Installed $BINARY to $INSTALL_DIR/$BINARY"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo "Note: $INSTALL_DIR is not on your PATH."
        echo "Add it with: export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac
echo "Run '$BINARY --help' to get started"
echo "To uninstall, remove $INSTALL_DIR/$BINARY"
