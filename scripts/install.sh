#!/usr/bin/env sh
# Install argent into ~/.local/bin (or INSTALL_DIR).
set -eu
SRC=${1:-target/release/argent}
DEST_DIR=${INSTALL_DIR:-$HOME/.local/bin}
mkdir -p "$DEST_DIR"
if [ ! -f "$SRC" ]; then
  echo "missing binary: $SRC" >&2
  exit 1
fi
cp "$SRC" "$DEST_DIR/argent"
chmod +x "$DEST_DIR/argent"
echo "installed $DEST_DIR/argent"
echo "quit and reopen argent for the new binary to take effect."
