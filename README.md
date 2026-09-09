# Mouse Without Borders (Windows PowerToys) Omarchy Bridge

An Omarchy bar-widget plugin, with its own bundled daemon, that lets a
Windows PC's physical keyboard/mouse (via Microsoft PowerToys' **Mouse
Without Borders**) control an Omarchy (Hyprland) machine directly — a
software-KVM setup, without going through any of the mainstream tools
(Synergy/Barrier/Input Leap/Deskflow), all of which currently fail on
Hyprland because `xdg-desktop-portal-hyprland` doesn't implement the
`RemoteDesktop` portal they all depend on.

**Status: working end-to-end.** Mouse (movement, clicks, scroll) and
keyboard both forward, via either edge-crossing or the `Ctrl+Alt+F1`-style
hotkey switch, plain-text clipboard content syncs both ways automatically,
copying a file on Windows makes it pasteable here too, double-tapping
Windows' own lock hotkey locks this machine too, and it runs as an
auto-starting, auto-reconnecting systemd user service. See **Known bugs**
below for current rough edges.

## Known bugs

- **Keyboard layout issues.** VK→evdev translation (`daemon/src/
  vk_keycode.rs`) has only been verified against a Slovenian (QWERTZ)
  layout, where it now covers letters, digits, common punctuation, the
  numpad, and AltGr/Level-3 characters (e.g. `<`/`>`). Other layouts are
  likely to hit wrong characters somewhere, especially punctuation/OEM
  keys — Windows can reassign those to different physical keys per layout
  in ways that aren't derivable from the VK code alone (see
  `daemon/PROTOCOL.md`'s keyboard-layout section for how to work out a fix
  for your own layout empirically).
- **Typing sometimes registers too many keystrokes** — occasional extra/
  duplicate key events land on the Omarchy side for a single physical
  keypress. Not yet root-caused: live-tested a single keypress and a full
  sentence while logging the raw wire traffic and saw a clean 1:1 key-down/
  key-up pair for every key, no duplicates — so it's rarer than normal
  typing triggers, or tied to something specific (fast typing, a key combo,
  a reconnect race) not hit in that test. Needs catching in the wild: next
  time it happens, check `journalctl --user -u mwb-omarchy-bridge --since
  "2 min ago"` right away for the real packet trace.

## Ideas for later

- **Clipboard images.** Not implemented, text/files only for now — see
  `daemon/PROTOCOL.md`'s clipboard section for why it's a separable,
  independently-addable code path whenever it's worth doing.
- **File copy/paste back to Windows** (Omarchy -> Windows direction).
  Copying a file here does get announced to Windows correctly, but a real,
  unmodified PowerToys install's own file-pull only ever triggers on its
  internal "machine switched" event — and since this bridge never
  participates in that (it's a simple one-way input-forwarding design, not
  a full peer in Mouse Without Borders' multi-machine switching protocol),
  Windows likely never even sees a switch to trigger on. Confirmed live:
  the announcement sends fine, Windows never connects to pull it. Making
  this direction work would mean implementing a real slice of that
  switching protocol — a bigger, more uncertain project than file transfer
  itself, not a quick fix.

## How it's put together

This repo is two things in one:

- **The daemon** (`daemon/`) — a Rust program that reimplements Mouse
  Without Borders' wire protocol and talks straight to Hyprland's own
  exposed wlroots protocols (`zwlr_virtual_pointer_v1` for the mouse,
  `zwp_virtual_keyboard_v1` for the keyboard) to actually move the cursor
  and type. Runs as a systemd `--user` service. See `daemon/PROTOCOL.md`
  for everything reverse-engineered about the wire protocol, and several
  Windows-side PowerToys state bugs that turned out to matter far more
  than the protocol itself.
- **The plugin** (`manifest.json`, `BarWidget.qml`, at the repo root — the
  standard Omarchy plugin layout) — a thin QML front-end giving the daemon
  a status icon plus a popup for start/stop/restart and settings (shared
  security key, Windows PC address, this machine's name). It doesn't
  implement any of the actual input-forwarding logic itself, and it builds
  and installs the daemon for you the first time it loads (see below) —
  there's nothing to build or install by hand.

They're split this way because plugins run unsandboxed inside the
long-running Omarchy shell process — real protocol/crypto/Wayland-protocol
code doesn't belong in there, so it's a separate daemon the plugin only
supervises over `systemctl` and a couple of JSON files.

## Clipboard

Plain text syncs both ways automatically — copy on Windows and it's applied
to this machine's clipboard (via `wl-copy`), copy here and it's sent to
Windows (detected by polling `wl-paste` every 500ms; both need
`wl-clipboard` installed, which is standard on Omarchy). Applying one
side's copy to the other doesn't bounce straight back — each side tracks
what it just applied from its peer and skips re-sending an exact echo of
it. See **Ideas for later** above for what's not covered yet (images).

## Files

Copying a file on Windows makes it show up here as a real, pasteable
clipboard entry (a `text/uri-list` pointing at a copy of it under
`~/.cache/omarchy-mwb-bridge-files/`) — tested end-to-end including an
actual paste into a file manager. The reverse direction (copying a file
here so it pastes on Windows) only gets halfway: it's correctly announced
to Windows, but a real PowerToys install won't actually come fetch it — see
**Ideas for later** above for why. Multiple files at once aren't supported
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
  "xkb_variant": ""
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
