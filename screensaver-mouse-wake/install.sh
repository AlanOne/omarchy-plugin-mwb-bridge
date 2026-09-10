#!/bin/bash
# Installs the screensaver mouse-wake patch: makes Omarchy's screensaver
# (org.omarchy.screensaver, the decorative ttfx animation -- NOT the actual
# authenticated lock screen) dismiss on mouse movement, not just keyboard
# input. Useful for KVM setups like this plugin's own daemon, where control
# arrives as a cursor move with no keypress.
#
# Needs sudo since /usr/bin/omarchy-screensaver is a root-owned file from the
# `omarchy` package -- run this yourself, it will prompt for your password.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET=/usr/bin/omarchy-screensaver
HOOK_DIR="$HOME/.config/omarchy/hooks/post-update.d"

if [[ ! -f $TARGET ]]; then
  echo "error: $TARGET not found -- is this Omarchy?" >&2
  exit 1
fi

if grep -q "MWB_MOUSE_WAKE" "$TARGET" 2>/dev/null; then
  echo "Already patched."
elif cmp -s "$DIR/orig.sh" "$TARGET"; then
  echo "Installing patched $TARGET (needs sudo)..."
  sudo install -m 755 "$DIR/patched.sh" "$TARGET"
  echo "Installed."
else
  echo "error: $TARGET doesn't match the upstream version this patch was built" >&2
  echo "against, and isn't already patched. Diff it against $DIR/orig.sh and" >&2
  echo "$DIR/patched.sh yourself before applying -- refusing to overwrite blindly." >&2
  exit 1
fi

mkdir -p "$HOOK_DIR"
ln -sf "$DIR/reapply.sh" "$HOOK_DIR/mwb-screensaver-mouse-wake.hook"
echo "Self-healing hook symlinked into $HOOK_DIR (reapplies the patch after"
echo "future \`omarchy update\` runs, if they revert it)."
