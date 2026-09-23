# Mouse Without Borders (Windows PowerToys) Omarchy Bridge

An Omarchy bar-widget plugin, with its own bundled daemon, that lets a
Windows PC's physical keyboard/mouse (via Microsoft PowerToys' **Mouse
Without Borders**) control an Omarchy (Hyprland) machine directly — a
software-KVM setup, without going through any of the mainstream tools
(Synergy/Barrier/Input Leap/Deskflow), all of which currently fail on
Hyprland because `xdg-desktop-portal-hyprland` doesn't implement the
`RemoteDesktop` portal they all depend on.

**Status: working end-to-end.** Mouse (movement, left/right/middle clicks,
extra side buttons like a Logitech MX Master's back/forward, vertical and
horizontal scroll) and keyboard both forward, via either edge-crossing or
the `Ctrl+Alt+F1`-style hotkey switch, plain-text and image clipboard
content syncs both ways automatically, copying a file on Windows makes it
pasteable here too,
double-tapping Windows' own lock hotkey locks this machine too, and it runs
as an auto-starting, auto-reconnecting systemd user service. See **Known
bugs** below for current rough edges.

## Known bugs

- **Keyboard layout issues.** VK→evdev translation (`daemon/protocol/src/
  vk_keycode.rs`, shared by both the Linux and macOS builds) has only been verified against a Slovenian (QWERTZ)
  layout, where it now covers letters, digits, common punctuation, the
  numpad, and AltGr/Level-3 characters (e.g. `<`/`>`). Other layouts are
  likely to hit wrong characters somewhere, especially punctuation/OEM
  keys — Windows can reassign those to different physical keys per layout
  in ways that aren't derivable from the VK code alone (see
  `daemon/PROTOCOL.md`'s keyboard-layout section for how to work out a fix
  for your own layout empirically).
- **File/big-image transfer back to Windows** (Omarchy -> Windows
  direction) doesn't work, despite a genuine, thorough attempt. Copying a
  file or large image here does get announced to Windows correctly, and
  this repo sends the exact packet sequence real MWB's own machine-switch
  sends (`HideMouse` + `MachineSwitched`, addressed directly to Windows'
  machine ID — confirmed byte-for-byte identical to a real capture from
  Windows' own `Ctrl+Alt+F1`-style switch) — but Windows still never
  connects to pull anything. Root cause not found after several rounds of
  reading the actual PowerToys source; see `daemon/PROTOCOL.md`'s big-path
  section for the full investigation, including a real bug found and fixed
  along the way (a package-ID reuse issue that was silently blocking
  earlier attempts, now fixed, but not sufficient on its own). Documented
  here as an investigated, unresolved limitation rather than a quick fix
  still to come.

## How it's put together

`daemon/` is a Cargo workspace with three crates:

- **`daemon/protocol/`** — the entire MWB wire protocol (crypto, framing,
  handshake, clipboard chunking, file-transfer framing) plus the shared
  Mouse/Keyboard *decoding* logic (`input_handling.rs`: WM_* flag handling,
  VK→evdev translation via `vk_keycode.rs`, repeat-key dedup, the
  lock-combo detector, scroll-unit conversion) and the JSON config/status
  format. Entirely OS-agnostic — used unchanged by both platform builds
  below, which only differ in how they actually inject input. See
  `daemon/PROTOCOL.md` for everything reverse-engineered about the wire
  protocol, and several Windows-side PowerToys state bugs (matrix/pool
  registration, handshake checksums, package-ID dedup) that turned out to
  matter far more than the protocol itself — these apply identically
  regardless of which platform build you're pairing.
- **`daemon/linux/`** — the Omarchy build: talks straight to Hyprland's own
  exposed wlroots protocols (`zwlr_virtual_pointer_v1` for the mouse,
  `zwp_virtual_keyboard_v1` for the keyboard) to actually move the cursor
  and type. Runs as a systemd `--user` service, supervised by the plugin
  below (`manifest.json`, `BarWidget.qml` at the repo root — the standard
  Omarchy plugin layout — a thin QML front-end for status/settings; it
  builds and installs the daemon for you the first time it loads, nothing
  to build or install by hand). They're split this way because plugins run
  unsandboxed inside the long-running Omarchy shell process — real
  protocol/crypto/Wayland-protocol code doesn't belong in there.
- **`daemon/macos/`** — the macOS build (see **macOS** below for status):
  injects input via Core Graphics `CGEventPost` instead, and is its own
  small menu-bar app rather than a plugin hosted by anything else.

## macOS

**Status: mouse + keyboard forwarding (including window dragging and
double-click-to-zoom) and clipboard sync — text and images, both
directions, both size paths (small inline path and the >1MB big-path
connection, confirmed with a real 2MB screenshot) — all working end-to-end,
packaged as a proper autostarting `.app`, all live-tested against a real
Windows PC.** Not yet ported: plain file copy/paste (any file, not just
images), lock-both-machines, suspend/resume handling. Horizontal scroll's
sign isn't independently confirmed yet (only vertical was live-tested).
Big-path image sync is independently confirmed inbound (Windows -> Mac)
but not yet outbound (the Mac -> Windows code path is symmetric but
untested with a genuinely >1MB Mac-side image).

**Note on clipboard content types**: copying an image *file* (Explorer/
Finder "Copy" on a `.png` on disk) is a different clipboard content type
(a file reference) from "copy image" (a screenshot tool, or Preview/an
app's Edit > Copy) — only the latter is what clipboard image sync applies
to. A file-copy test will correctly report "not supported" rather than
silently doing nothing.

Menu bar icon is a simple procedurally-drawn double-headed arrow
(`macos/src/tray.rs`'s `bridge_icon`), rendered as a template image so it
adapts to light/dark menu bar appearance — not a real designed asset, but
no longer the plain placeholder square either. The app is menu-bar-only
(no Dock icon/app-switcher entry) via `Info.plist`'s `LSUIElement=true`
**plus** `tao`'s `EventLoopExtMacOS::set_activation_policy(Accessory)` —
confirmed live that `LSUIElement` alone isn't enough, since tao's own
`NSApplication` setup defaults to the Regular policy regardless.

**Fixed: Accessibility grant now survives rebuilds.** Originally signed
ad-hoc (`codesign --sign -`), which derives its "identity" from the
binary's own hash — confirmed live that this broke mouse/keyboard
forwarding silently (no error) after every single rebuild, and even
toggling the existing Accessibility entry off/on didn't fix it, only fully
removing and re-adding it did. Fixed by generating a stable local
self-signed code-signing certificate (`install.sh` does this automatically
on first run — `security find-certificate`/`openssl req`/`security
import`, entirely scripted, no Keychain Access GUI needed) and signing
with that instead. Confirmed live, 2026-09-22: rebuilt with a real source
change, Accessibility kept working with zero manual steps. Two one-time
exceptions (both already behind you after the first install with this
identity): switching an existing ad-hoc install over needs one manual
re-grant, and the very first time `codesign` uses the new certificate's
private key, macOS may show a one-time keychain password prompt — if it
keeps prompting on every rebuild rather than just once, also run (once,
needs your login password, deliberately not scripted):
```sh
security set-key-partition-list -S apple-tool:,apple:,codesign: -s ~/Library/Keychains/login.keychain-db
```

**Fixed: window dragging and double-click-to-zoom.** `cg_input.rs` always
posted plain `MouseMoved` while a button was held (instead of the
`*MouseDragged` event types macOS's live window-drag-tracking specifically
watches for — a dragged window only snapped to its final position on
mouse-up rather than following the cursor), and never set
`EventField::MOUSE_EVENT_CLICK_STATE` (so no synthesized click was ever
recognized as a double-click, e.g. title-bar zoom silently did nothing).
Both fixed — `move_absolute` now tracks which button is held and posts the
matching Dragged type, and `button` tracks click timing/position to set a
real click count on both the down and up events.

Requires **Accessibility permission** (System Settings > Privacy &
Security > Accessibility) granted to the running binary — `CGEventPost`
silently does nothing without it, no error. Config lives at
`~/Library/Application Support/mwb-mac-bridge/config.json` (same fields as
the Linux build's `config.json`: `security_key`, `windows_ip`,
`machine_name`, `machine_id`, `scroll_speed`; `xkb_layout`/`xkb_variant`
are ignored here — the Mac's own selected input source handles character
mapping instead, same division of responsibility XKB has on Linux). A
template is written on first run if none exists; edit it via the menu bar
icon's "Edit Config..." item, then "Restart App".

**Also requires the Windows PC to actually know about this machine** —
PowerToys' Settings UI has a known bug (see `daemon/PROTOCOL.md`) where
adding a new machine via the UI doesn't reliably persist to
`MachineMatrixString`/`MachinePool`/`Name2IP` in
`%LOCALAPPDATA%\Microsoft\PowerToys\MouseWithoutBorders\settings.json` —
if the handshake succeeds but Windows never sends anything further
afterward (not even periodic Hi pings), this is almost certainly why; quit
PowerToys fully, hand-edit those three fields to match the existing
entries' format, relaunch.

**Install (builds, packages as `~/Applications/MWB Mac Bridge.app`, and
registers a LaunchAgent so it starts at login and auto-restarts on
crash — the counterpart to the Linux build's systemd `--user` service):**

```sh
bash daemon/macos/packaging/install.sh
```

Re-run it any time after pulling changes to rebuild and reinstall in place.
First run needs a one-time manual step (can't be scripted — it's a macOS
security gate): add `MWB Mac Bridge.app` in System Settings > Privacy &
Security > Accessibility. It won't necessarily be listed automatically
until it's actually tried to post an input event — use the **+** button
and browse to `~/Applications/MWB Mac Bridge.app` if it's missing. Ad-hoc
code signing (no paid Apple Developer account needed) means this grant may
need re-confirming after a rebuild, since the signature changes each time
— see the comment in `install.sh` for the stable-signing alternative if
that gets annoying.

For quick iteration without the full install (no autostart, no bundle):

```sh
cargo run -p mwb_mac_bridge
```

## Clipboard

Plain text and images both sync both ways automatically — copy on Windows
and it's applied to this machine's clipboard (via `wl-copy`), copy here and
it's sent to Windows (detected by polling `wl-paste` every 500ms; both need
`wl-clipboard` installed, which is standard on Omarchy). A screenshot or
"copy image" (no file involved) works the same way as text — a small image
sends inline, a larger one (screenshots routinely exceed the small-path
size limit) is announced and pulled over the same connection file transfer
uses. Applying one side's copy to the other doesn't bounce straight back —
each side tracks what it just applied from its peer and skips re-sending an
exact echo of it.

## Files

Copying a file on Windows makes it show up here as a real, pasteable
clipboard entry (a `text/uri-list` pointing at a copy of it under
`~/.cache/omarchy-mwb-bridge-files/`) — tested end-to-end including an
actual paste into a file manager. The reverse direction (copying a file
here so it pastes on Windows) only gets halfway: it's correctly announced
to Windows, but a real PowerToys install won't actually come fetch it — see
**Known bugs** above for why. Multiple files at once aren't supported
(matches a real limitation of Mouse Without Borders' own file-transfer
code, not something narrowed further here) — only the first file of a
multi-file selection is used, the rest silently ignored.

## Locking both machines

Double-tapping Mouse Without Borders' own lock hotkey (in PowerToys'
settings, `HotKeyLockMachine` — not Windows' native `Win+L`, see below) on
Windows locks this machine too, within about half a second. Real MWB sends
that combo's own keys as an ordinary, very fast Keyboard-packet burst (all
down, then all up) right before locking itself — this bridge watches for
that unnaturally-fast timing and runs `omarchy-system-lock` in response, no
new packet type involved.

**Windows' native `Win+L` won't work for this, and can't be made to** — it's
a Windows-reserved shortcut handled by the OS almost instantly, before
PowerToys' own hook gets a chance to register a second press within the
double-tap window. If your `HotKeyLockMachine` is currently set to `Win+L`,
change it in PowerToys' Mouse Without Borders settings to something Windows
doesn't already reserve — the classic default, `Ctrl+Alt+Win+L`, works.
Confirmed live: the detection only checks that `Win` and `L` both appear
(down and up) within the burst, not the full combo, so it doesn't matter
if your configured combo adds other modifiers on top of those two.

## Optional: dismiss the screensaver on mouse movement

By default, Omarchy's screensaver (the decorative `ttfx` terminal animation,
`org.omarchy.screensaver` — **not** the actual authenticated lock screen,
which is unaffected) only dismisses on a keypress or a window-focus change,
not on raw cursor movement. That's a rough edge for this bridge's whole
point: control usually arrives here as a cursor move, with no keyboard
involved at all, so the screensaver could sit there indefinitely even while
you're actively mousing around.

An optional patch for this lives in [`screensaver-mouse-wake/`](screensaver-mouse-wake/)
in this repo. It's not installed automatically (unlike the daemon/systemd
setup above) because it edits a root-owned file from the `omarchy` package
— install it yourself:

```sh
bash screensaver-mouse-wake/install.sh
```

This will ask for your `sudo` password once. What it does:
- Patches `/usr/bin/omarchy-screensaver` to poll the cursor position a few
  times a second and dismiss as soon as it moves, in addition to the
  existing keyboard/focus checks.
- Symlinks `screensaver-mouse-wake/reapply.sh` into
  `~/.config/omarchy/hooks/post-update.d/`, so a future `omarchy update`
  that overwrites this file gets the patch reapplied automatically —
  **but only if the installed file still exactly matches the unpatched
  version this patch was built against** (`screensaver-mouse-wake/orig.sh`).
  If Omarchy has changed the script in some other way by then, it backs off
  and sends you a notification instead of blindly overwriting an unrelated
  upstream change.

### Optional: make the reapply fully automatic (no sudo prompt)

`reapply.sh` runs unattended from an update hook, so it can't prompt for
your password — by default, if an update reverts the file, it'll detect
that correctly but only get as far as sending you a notification asking you
to run the install command yourself. To let it actually reapply itself
silently, grant passwordless `sudo` for **exactly this one command**
(nothing broader):

```sh
echo 'YOUR_USERNAME ALL=(root) NOPASSWD: /usr/bin/install -m 755 /path/to/this/repo/screensaver-mouse-wake/patched.sh /usr/bin/omarchy-screensaver' \
  | sudo tee /etc/sudoers.d/mwb-screensaver-patch
sudo chmod 440 /etc/sudoers.d/mwb-screensaver-patch
sudo visudo -c   # validates syntax before trusting it
```

Replace `YOUR_USERNAME` and the repo path with your actual values (both
`reapply.sh`'s own `sudo -n install ...` call and this sudoers rule need to
reference the same real, resolved path this repo is checked out at — if you
ever move the checkout, update both). `visudo -c` at the end catches a typo
before it can break `sudo` entirely.

## Install

1. Install the plugin, either way:

   - **Via Omarchy's own plugin command** (also reachable from its menu —
     search for "Add Plugin"): `omarchy plugin add
     https://github.com/AlanOne/omarchy-plugin-mwb-bridge.git --enable`.
     This clones it, validates the manifest, and adds it to your bar
     layout for you.
   - **Manually**: check this repo out to
     `~/.config/omarchy/plugins/io.github.alanone.mwb-bridge/` (or symlink
     it there from wherever you cloned it), then add it to your bar
     layout in `~/.config/omarchy/shell.json` yourself:

     ```json
     {
       "id": "io.github.alanone.mwb-bridge"
     }
     ```

2. Reload the bar (or just wait for it to notice). The first time the
   widget loads, it automatically builds the daemon (needs a Rust
   toolchain installed; this can take a minute) and installs + enables its
   systemd `--user` service. The popup shows a status line while this is
   happening. Every load after the first just checks these are already in
   place and skips straight to normal status polling — it won't
   second-guess a Stop you clicked yourself.

3. Open the widget's popup (bar icon) and fill in the Security Key,
   Windows PC's IP or hostname, and a name for this machine, then hit
   **Save and restart**. The keyboard layout is detected automatically
   (via `hyprctl`) — no need to set it by hand.

### On the Windows side

Fully close PowerToys, hand-edit
`%LOCALAPPDATA%\Microsoft\PowerToys\MouseWithoutBorders\settings.json`
to add this machine to both `MachineMatrixString` and `MachinePool`
(name + the ID shown in the widget's popup), then relaunch. If your
machine is named `omarchy` with ID `987654321`, and the existing file
already has your Windows PC as `WINPC`/`123456789` in one slot — among
the many other properties already in the file:

```json
{
  "properties": {
    "MachineMatrixString": ["omarchy", "WINPC", "", ""],
    "MachinePool": { "value": "WINPC:123456789,omarchy:987654321,:,:" }
  }
}
```

`MachineMatrixString` position encodes physical left/right layout for
edge-crossing — put your machine on whichever side matches your actual
desk setup. If Windows can't resolve this machine's name for
edge-crossing routing (a "cannot resolve IP address" toast), add either
an IP Mapping entry in PowerToys' own settings UI or a hosts-file entry
(`C:\Windows\System32\drivers\etc\hosts`) — neither was needed on the
network this was built on, but your router/DNS setup may differ.

## Config files (written by the plugin, read by the daemon)

The plugin's popup writes `~/.local/share/omarchy-mwb-bridge/config.json`
and restarts the service to apply it — the daemon only reads this at
startup, there's no live-reload. It's safe for the service to start before
this file exists; the daemon just waits and retries every 5s. Editing it
by hand works just as well as using the popup:

```json
{
  "security_key": "your PowerToys Mouse Without Borders shared security key",
  "windows_ip": "your Windows PC's LAN IP or hostname:15101",
  "machine_name": "pick a name for this machine, e.g. its hostname",
  "machine_id": 987654321,
  "xkb_layout": "the XKB layout this machine's keyboard actually uses, e.g. us",
  "xkb_variant": "",
  "scroll_speed": 1.0
}
```

- `windows_ip` port is `15101` (**not** the default-assumed `15100` — see
  `daemon/PROTOCOL.md`). A hostname works fine instead of a raw IP
  (`TcpStream::connect` resolves it) — e.g. `WINPC.local:15101` if your
  Windows PC answers to mDNS (`getent hosts WINPC.local` to check). Worth
  using over a raw IP since IPs can change on DHCP renewal; a resolvable
  hostname doesn't.
- `machine_id` can be any number — the plugin picks a random one the first
  time it runs and reuses it after that. `machine_name`/`machine_id` must
  exactly match what you add to Windows' `MachinePool` setting (see above).
- `xkb_layout`/`xkb_variant` are filled in automatically by the plugin
  (`hyprctl getoption input:kb_layout -j` / `input:kb_variant -j`) every
  time you save — hand-editing them only matters if you're running the
  daemon without the plugin. See **Known bugs** above for why this alone
  doesn't guarantee correct typing on every layout.
- The file contains the shared key in plaintext — keep it `chmod 600`
  (the plugin does this after every save).
- `scroll_speed` is a plain multiplier on both vertical and horizontal
  scroll, default `1.0` (missing entirely on an older config defaults to
  the same). The baseline it multiplies already corrects for Windows'
  wheel-delta units being ~8x larger than what Wayland's virtual-pointer
  protocol expects — adjust this only to taste on top of that, e.g. `0.8`
  for a bit slower.

The daemon also writes live connection status to
`~/.local/share/omarchy-mwb-bridge/status.json` (`{connected, peer, detail,
updated_epoch}`) — this is what the bar widget polls for display.

## Reboots

The daemon auto-starts and reconnects to Windows with no interaction needed
on a reboot — confirmed via a real reboot's boot log: it started, handshook,
and was forwarding real packets within about 10 seconds of its systemd unit
firing. The one thing that still needs a human at the machine is anything
that has to happen *before* a graphical session exists at all — most
commonly a full-disk-encryption passphrase prompt, if your install uses one
(this one does), which looks similar to a login screen but is unrelated to
it and can't be bypassed by SDDM autologin or anything else software-side.
Past that point, everything is automatic.

## Layout

- `manifest.json`, `BarWidget.qml` — the Omarchy plugin (repo root, so it
  can be checked out directly as `~/.config/omarchy/plugins/<id>/`).
- `daemon/src/lib.rs` — shared library: `mwb_protocol` (crypto/framing/
  handshake primitives), `vk_keycode` (Windows VK → Linux evdev
  translation), `wayland_input` (the Wayland virtual pointer/keyboard
  injector), `config` (reads `config.json`, writes `status.json`).
- `daemon/src/bin/daemon.rs` — **the real thing.** A persistent, auto-
  reconnecting client connection to Windows' message server, decoding
  Mouse/Keyboard packets into Wayland input; plus a listener on the same
  port for Windows' side of the connection pair (see `daemon/PROTOCOL.md`
  for why — it turned out not to be load-bearing, but is harmless to keep).
- `daemon/src/main.rs` — Phase 1 PoC: Wayland virtual pointer/keyboard
  injection, no networking. Kept as a minimal standalone reference.
- `daemon/PROTOCOL.md` — the full reverse-engineered wire protocol spec,
  plus everything learned about Windows-side state (the Matrix/MachinePool
  UI persistence bug, DNS/IP-mapping quirks, keyboard-layout translation).
  Read this before changing any of the crypto/framing/handshake/keymap
  code.
- `daemon/systemd/mwb-omarchy-bridge.service` — a reference unit for
  building and running the daemon manually (`cd daemon && cargo build
  --release`, which lands the binary at `daemon/target/release/daemon` —
  matching this template's `ExecStart`). The plugin's own automatic setup
  doesn't use this file; it generates an equivalent unit itself, pointing
  at wherever it actually built the binary (`~/.cache/omarchy-mwb-bridge-
  build/release/daemon`, kept outside `daemon/` so a build doesn't spam
  Quickshell's file-watcher with the thousands of files a Rust build
  writes).
