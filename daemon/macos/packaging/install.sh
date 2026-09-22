#!/bin/bash
# Builds a release binary, bundles it as a proper .app (so Accessibility
# permission grants attach to a stable path/identity instead of a scratch
# target/debug binary that changes every rebuild), installs it to
# ~/Applications/, and registers a LaunchAgent so it starts at login and
# auto-restarts on crash — the macOS counterpart to the Linux build's
# systemd --user service. Re-run this any time after pulling changes to
# rebuild and reinstall in place; it unloads the old LaunchAgent first so a
# reinstall while it's already running doesn't conflict with itself.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DAEMON_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
APP_NAME="MWB Mac Bridge.app"
APP_DIR="$HOME/Applications/$APP_NAME"
BUNDLE_ID="com.alanone.mwb-mac-bridge"
PLIST_PATH="$HOME/Library/LaunchAgents/$BUNDLE_ID.plist"
LOG_DIR="$HOME/Library/Logs/mwb-mac-bridge"

echo "==> Building release binary..."
( cd "$DAEMON_DIR" && cargo build --release -p mwb_mac_bridge )
BIN_PATH="$DAEMON_DIR/target/release/mwb_mac_bridge"

echo "==> Unloading any existing LaunchAgent (ok if this errors — means none was loaded)..."
launchctl unload "$PLIST_PATH" 2>/dev/null || true

echo "==> Assembling $APP_DIR..."
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS"
cp "$SCRIPT_DIR/Info.plist" "$APP_DIR/Contents/Info.plist"
cp "$BIN_PATH" "$APP_DIR/Contents/MacOS/mwb_mac_bridge"

echo "==> Ad-hoc code signing (no Apple Developer account needed for local use)..."
# Ad-hoc (`-`) signing gives the bundle a stable enough identity for one
# install, but its hash changes on every rebuild — macOS may ask you to
# re-grant Accessibility after reinstalling a new build. If that gets
# annoying, replace this with a self-signed code-signing certificate from
# Keychain Access (Certificate Assistant > Create a Certificate > Code
# Signing) and `--sign "<cert name>"` instead, which stays stable across
# rebuilds.
codesign --force --deep --sign - "$APP_DIR"

mkdir -p "$LOG_DIR"
EXECUTABLE_PATH="$APP_DIR/Contents/MacOS/mwb_mac_bridge"
sed -e "s|__EXECUTABLE_PATH__|$EXECUTABLE_PATH|" -e "s|__LOG_DIR__|$LOG_DIR|" \
    "$SCRIPT_DIR/com.alanone.mwb-mac-bridge.plist.template" > "$PLIST_PATH"

echo "==> Loading LaunchAgent..."
launchctl load "$PLIST_PATH"

cat <<EOF

Installed to: $APP_DIR
LaunchAgent:  $PLIST_PATH
Logs:         $LOG_DIR/

IMPORTANT — every install/reinstall needs a manual permission step:
Open System Settings > Privacy & Security > Accessibility. CGEventPost
silently does nothing without this grant — no error, no crash, input just
won't forward.
  - First install: add "$APP_NAME" (+ button, browse to $APP_DIR).
  - Reinstall (after this script rebuilds a new binary): ad-hoc signing
    means the signature changed, and confirmed live, just toggling the
    existing entry off/on does NOT restore trust — remove it entirely
    (− button) and re-add it (+ button) instead.
Then restart the app:
  launchctl kickstart -k gui/\$(id -u)/$BUNDLE_ID
EOF
