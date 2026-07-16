#!/usr/bin/env bash
# Client installer — prefers server-hosted install when DOMAIN is known.
# Usage: curl -sSL https://<domain>/install | bash
# Fallback (GitHub): curl -sSL .../scripts/install.sh | bash

set -euo pipefail

REPO="phamvanquyit/ZoTunnel"
INSTALL_DIR="${HOME}/.zotunnel/bin"
mkdir -p "$INSTALL_DIR"

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$OS" in
  linux) OS_LABEL=linux ;;
  darwin) OS_LABEL=darwin ;;
  *) echo "Unsupported OS: $OS"; exit 1 ;;
esac
case "$ARCH" in
  x86_64|amd64) ARCH_LABEL=amd64 ;;
  aarch64|arm64) ARCH_LABEL=arm64 ;;
  *) echo "Unsupported arch: $ARCH"; exit 1 ;;
esac

FILE="zotunnel-${OS_LABEL}-${ARCH_LABEL}"

if [ -n "${ZOTUNNEL_BASE:-}" ]; then
  URL="${ZOTUNNEL_BASE%/}/download/${FILE}"
else
  TAG=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
  if [ -z "$TAG" ]; then
    echo "Could not resolve latest release"
    exit 1
  fi
  URL="https://github.com/${REPO}/releases/download/${TAG}/zotunnel-${TAG}-${OS_LABEL}-${ARCH_LABEL}.tar.gz"
fi

echo "▸ Downloading ${FILE}..."
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

if [[ "$URL" == *.tar.gz ]]; then
  curl -fsSL "$URL" -o "$TMP/zotunnel.tar.gz"
  tar -xzf "$TMP/zotunnel.tar.gz" -C "$TMP"
  BIN=$(find "$TMP" -type f -name 'zotunnel' | head -1)
  cp "$BIN" "$INSTALL_DIR/zotunnel"
else
  curl -fsSL "$URL" -o "$INSTALL_DIR/zotunnel"
fi
chmod +x "$INSTALL_DIR/zotunnel"

SHELL_RC=""
case "${SHELL:-}" in
  */zsh) SHELL_RC="$HOME/.zshrc" ;;
  */bash) SHELL_RC="$HOME/.bashrc" ;;
  *) SHELL_RC="$HOME/.profile" ;;
esac
if [ -n "$SHELL_RC" ] && ! grep -q '.zotunnel/bin' "$SHELL_RC" 2>/dev/null; then
  echo 'export PATH="$HOME/.zotunnel/bin:$PATH"' >> "$SHELL_RC"
fi
export PATH="$INSTALL_DIR:$PATH"

echo "✅ zotunnel installed to $INSTALL_DIR/zotunnel"
echo
echo "Next:"
echo "  zotunnel config set --server HOST:6200 --token TOKEN"
echo "  zotunnel http 3000"
