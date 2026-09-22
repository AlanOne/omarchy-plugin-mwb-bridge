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
CERT_NAME="mwb-mac-bridge-dev"

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

# A stable self-signed code-signing identity, generated once and reused for
# every install/reinstall from then on. This is the actual fix for a real
# gotcha found live (2026-09-22): plain ad-hoc signing (`codesign --sign -`)
# derives its "identity" from the binary's own hash, which changes on every
# rebuild — macOS's Accessibility grant is tied to that identity, so each
# rebuild silently broke CGEventPost (no error) until the grant was fully
# removed and re-added. A real certificate-backed signature keeps the same
# identity across rebuilds (same private key/cert, different binary
# content), so TCC recognizes it as "the same app" every time instead.
if ! security find-certificate -c "$CERT_NAME" >/dev/null 2>&1; then
    echo "==> No local code-signing certificate found — generating one (one-time, local-only, never leaves this Mac)..."
    CERT_TMPDIR="$(mktemp -d)"
    trap 'rm -rf "$CERT_TMPDIR"' EXIT
    openssl req -x509 -newkey rsa:2048 -keyout "$CERT_TMPDIR/key.pem" -out "$CERT_TMPDIR/cert.pem" \
        -days 3650 -nodes -subj "/CN=$CERT_NAME" \
        -addext "extendedKeyUsage=codeSigning" \
        -addext "basicConstraints=critical,CA:false" \
        -addext "keyUsage=critical,digitalSignature"
    openssl pkcs12 -export -out "$CERT_TMPDIR/cert.p12" -inkey "$CERT_TMPDIR/key.pem" \
        -in "$CERT_TMPDIR/cert.pem" -passout pass:temp
    security import "$CERT_TMPDIR/cert.p12" -k ~/Library/Keychains/login.keychain-db \
        -P temp -T /usr/bin/codesign -A
    echo "==> Certificate '$CERT_NAME' generated and imported into your login keychain."

    # The `-T` grant above is not sufficient on its own on modern macOS —
    # confirmed live, 2026-09-22: codesign still prompted for the login
    # keychain password on first use despite it. The actual fix is also
    # granting access via the newer partition-list ACL mechanism, which
    # needs the keychain (login) password once — prompted interactively
    # here rather than passed as a script argument, so it's typed directly
    # into the terminal rather than passing through anything else. This is
    # a one-time step: once set, it applies to every future codesign call
    # using this identity, on every future rebuild.
    echo "==> One more one-time step: macOS needs your login password to let codesign use this certificate without prompting on every future build."
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s ~/Library/Keychains/login.keychain-db
fi

echo "==> Code signing with the stable '$CERT_NAME' identity..."
codesign --force --deep --sign "$CERT_NAME" "$APP_DIR"

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

IMPORTANT — first install needs a manual permission grant (can't be
scripted — it's a macOS security gate): open System Settings > Privacy &
Security > Accessibility, and add "$APP_NAME" (+ button, browse to
$APP_DIR if it's not listed). CGEventPost silently does nothing without
this — no error, no crash, input just won't forward.

Thanks to the stable signing identity above, this grant now persists
across future reinstalls/rebuilds — no more remove-and-re-add dance
(confirmed live, 2026-09-22: rebuilt with a real source change, and
Accessibility kept working with zero manual steps). Two one-time-only
exceptions, both already behind you after your very first install with
this identity:
  - Switching an existing ad-hoc-signed install over to this identity
    needs one manual re-grant (old and new identities are different).
  - The very first time codesign uses the newly-generated certificate's
    private key, macOS may show a one-time keychain password prompt to
    approve access — click Always Allow / enter your password once, and
    it won't ask again for this identity.

Then restart the app:
  launchctl kickstart -k gui/\$(id -u)/$BUNDLE_ID
EOF
