# Mouse Without Borders (Windows PowerToys) Omarchy Bridge

An Omarchy bar-widget plugin, with its own bundled daemon, that lets a
Windows PC's physical keyboard/mouse (via Microsoft PowerToys' **Mouse
Without Borders**) control an Omarchy (Hyprland) machine directly — a
software-KVM setup, without going through any of the mainstream tools
(Synergy/Barrier/Input Leap/Deskflow), all of which currently fail on
Hyprland because `xdg-desktop-portal-hyprland` doesn't implement the
`RemoteDesktop` portal they all depend on.

**Status: working end-to-end.** Mouse (movement, clicks, scroll) and
keyboard both forward correctly, via either edge-crossing or the
`Ctrl+Alt+F1`-style hotkey switch, and it runs as an auto-starting,
auto-reconnecting systemd user service. See **Known bugs** below for
current rough edges.

## Known bugs

- **Keyboard layout issues.** VK→evdev translation (`daemon/src/
  vk_keycode.rs`) has only been verified against a Slovenian (QWERTZ)
  layout. Other layouts are likely to hit wrong characters somewhere,
  especially punctuation/OEM keys — Windows can reassign those to
  different physical keys per layout in ways that aren't derivable from
  the VK code alone (see `daemon/PROTOCOL.md`'s keyboard-layout section
  for how to work out a fix for your own layout empirically).
- **Typing sometimes registers too many keystrokes** — occasional extra/
  duplicate key events land on the Omarchy side for a single physical
  keypress. Not yet root-caused.
- **Clipboard sharing isn't implemented.** The real Mouse Without Borders
  also syncs the clipboard between machines; this bridge only reimplements
  mouse/keyboard forwarding, not that part of the protocol.

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

## Install

1. Check this repo out to `~/.config/omarchy/plugins/io.github.alanone.mwb-bridge/`
   (or symlink it there from wherever you cloned it), then add it to your
   bar layout in `~/.config/omarchy/shell.json`:

   ```json
   {
     "id": "io.github.alanone.mwb-bridge"
   }
   ```

2. Reload the bar (or just wait for it to notice). The first time the
   widget loads, it automatically builds the daemon (`cargo build
   --release` in `daemon/` — needs a Rust toolchain installed; this can
   take a minute) and installs + enables its systemd `--user` service.
   The popup shows a status line while this is happening. Every load after
   the first just checks these are already in place and skips straight to
   normal status polling — it won't second-guess a Stop you clicked
   yourself.

3. Open the widget's popup (bar icon) and fill in the Security Key,
   Windows PC's IP or hostname, and a name for this machine, then hit
   **Save and restart**. The keyboard layout is detected automatically
   (via `hyprctl`) — no need to set it by hand.

4. See **On the Windows side** below — a required manual step on the
   Windows PC.

### On the Windows side

See `daemon/PROTOCOL.md` for the full story of why PowerToys' own UI
isn't enough for this: fully close PowerToys, hand-edit
   `%LOCALAPPDATA%\Microsoft\PowerToys\MouseWithoutBorders\settings.json`
   to add this machine to both `MachineMatrixString` and `MachinePool`
   (name + the ID shown in the widget's popup), then relaunch. For example,
   if this machine is named `omarchy` with ID `987654321`, and the existing
   file already has your Windows PC as `WINPC`/`123456789` in one slot —
   among the many other properties already in the file:

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
  "machine_id": 123456789,
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
  daemon without the plugin. See `daemon/PROTOCOL.md`'s keyboard-layout
  section for why letters and punctuation need different per-layout
  handling, and how to work out any additional per-key fixes
  (`daemon/src/vk_keycode.rs`) your own layout needs empirically — this
  repo currently only has fixes verified against a QWERTZ (Slovenian)
  layout; other layouts likely need their own.
- The file contains the shared key in plaintext — keep it `chmod 600`
  (the plugin does this after every save).

The daemon also writes live connection status to
`~/.local/share/omarchy-mwb-bridge/status.json` (`{connected, peer, detail,
updated_epoch}`) — this is what the bar widget polls for display.

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
  port for Windows' side of the connection pair (see `PROTOCOL.md` for
  why — it turned out not to be load-bearing, but is harmless to keep).
- `daemon/src/main.rs` — Phase 1 PoC: Wayland virtual pointer/keyboard
  injection, no networking. Kept as a minimal standalone reference.
- `daemon/PROTOCOL.md` — the full reverse-engineered wire protocol spec,
  plus everything learned about Windows-side state (the Matrix/MachinePool
  UI persistence bug, DNS/IP-mapping quirks, keyboard-layout translation).
  Read this before changing any of the crypto/framing/handshake/keymap
  code.
- `daemon/systemd/mwb-omarchy-bridge.service` — the service unit template
  the plugin generates on first run (with `ExecStart` pointing at wherever
  it actually built the binary) — kept here mainly as a reference/fallback
  for running the daemon without the plugin.
