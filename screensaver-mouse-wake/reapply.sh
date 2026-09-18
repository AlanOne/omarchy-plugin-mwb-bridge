#!/bin/bash
# Reapplies the MWB_MOUSE_WAKE patch to /usr/bin/omarchy-screensaver if an
# omarchy package update reverted it. Safe by construction: only overwrites
# when the installed file still matches the exact upstream version this patch
# was built against (orig.sh); otherwise it backs off and notifies, since
# upstream must have changed in ways this patch doesn't account for.
#
# Symlinked into ~/.config/omarchy/hooks/post-update.d/ by install.sh in this
# same directory, so it runs automatically after every `omarchy update`.
set -euo pipefail

# readlink -f resolves the symlink chain first -- omarchy-hook invokes this
# script via the post-update.d/*.hook symlink pointing here, and plain
# `dirname "${BASH_SOURCE[0]}"` (without resolving that symlink first) would
# otherwise resolve to the *hooks* directory instead of this one, silently
# breaking every check below (confirmed live 2026-09-18: this exact bug was
# why the patch never actually got reapplied after an omarchy update reverted
# it -- $DIR/orig.sh didn't exist, cmp failed, and the "changed upstream in a
# way this patch doesn't recognize" branch fired every time instead of the
# real reapply logic, with nobody noticing because that notification's
# wording didn't point at the actual cause).
DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
TARGET=/usr/bin/omarchy-screensaver

[[ -f $TARGET ]] || exit 0
grep -q "MWB_MOUSE_WAKE" "$TARGET" 2>/dev/null && exit 0

if cmp -s "$DIR/orig.sh" "$TARGET"; then
  if install -m 755 "$DIR/patched.sh" "$TARGET" 2>/dev/null ||
    sudo -n install -m 755 "$DIR/patched.sh" "$TARGET" 2>/dev/null; then
    omarchy-notification-send -g "🖱" "Screensaver mouse-wake patch reapplied" \
      "An Omarchy update reset omarchy-screensaver; the mouse-dismiss patch was reapplied." 2>/dev/null || true
  else
    omarchy-notification-send -u critical -g "🖱" "Screensaver mouse-wake patch needs manual reapply" \
      "Couldn't write to $TARGET (no privileges in this hook context). Run: sudo install -m 755 $DIR/patched.sh $TARGET" 2>/dev/null || true
  fi
else
  omarchy-notification-send -u critical -g "🖱" "Screensaver mouse-wake patch needs manual reapply" \
    "omarchy-screensaver changed upstream in a way this patch doesn't recognize. See $DIR/." 2>/dev/null || true
fi
