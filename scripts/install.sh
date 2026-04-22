#!/bin/sh
# CodeRex installer — cross-platform shell script.
#
# Usage:
#   curl -fsSL https://coderex.dev/install.sh | sh
#   curl -fsSL https://coderex.dev/install.sh | sh -s -- v0.1.0

set -e

REPO="coderex/coderex"
BINARY="coderex"

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
            aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
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
    VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | \
        grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
    if [ -z "$VERSION" ]; then
        echo "Failed to determine latest version"
        exit 1
    fi
fi

echo "Installing $BINARY $VERSION for $TARGET..."

# Download URL
URL="https://github.com/$REPO/releases/download/$VERSION/${BINARY}-${TARGET}.tar.gz"
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL "$URL" -o "$TMPDIR/${BINARY}.tar.gz"
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
echo "Run '$BINARY --help' to get started"
