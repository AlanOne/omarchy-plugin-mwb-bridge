# MWB Bridge

An Omarchy bar-widget plugin, with its own bundled daemon, that lets a
Windows PC's physical keyboard/mouse (via Microsoft PowerToys' **Mouse
Without Borders**) control an Omarchy (Hyprland) machine directly — a
software-KVM setup, without going through any of the mainstream tools
(Synergy/Barrier/Input Leap/Deskflow), all of which currently fail on
Hyprland because `xdg-desktop-portal-hyprland` doesn't implement the
`RemoteDesktop` portal they all depend on.

**Status: working end-to-end.** Mouse (movement, clicks, scroll) and
keyboard (including modifiers and non-US layouts) both forward correctly,
via either edge-crossing or the `Ctrl+Alt+F1`-style hotkey switch, and it
runs as an auto-starting, auto-reconnecting systemd user service.

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
  implement any of the actual input-forwarding logic itself.

They're split this way because plugins run unsandboxed inside the
long-running Omarchy shell process — real protocol/crypto/Wayland-protocol
code doesn't belong in there, so it's a separate daemon the plugin only
supervises over `systemctl` and a couple of JSON files.

## Install

1. **Build the daemon:**

   ```sh
   cd daemon
   cargo build --release
   mkdir -p ~/.config/systemd/user
   cp systemd/mwb-omarchy-bridge.service ~/.config/systemd/user/
   # Edit ExecStart in the copied unit to point at wherever you checked
   # this repo out (it defaults to ~/Work/mwb-omarchy-bridge/daemon).
   systemctl --user daemon-reload
   systemctl --user enable --now mwb-omarchy-bridge.service
   ```

2. **Install the plugin.** If you checked this repo out somewhere other
   than `~/.config/omarchy/plugins/io.github.alanone.mwb-bridge/`, symlink
   or copy it there, then add it to your bar layout in
   `~/.config/omarchy/shell.json`:

   ```json
   {
     "id": "io.github.alanone.mwb-bridge"
   }
   ```

3. Open the widget's popup (bar icon) and fill in the Security Key,
   Windows PC's IP or hostname, and a name for this machine, then hit
   **Save and restart**. The keyboard layout is detected automatically
   (via `hyprctl`) — no need to set it by hand.

4. **On the Windows side** (see `daemon/PROTOCOL.md` for the full story of
   why PowerToys' own UI isn't enough for this):
   - Fully close PowerToys, hand-edit
     `%LOCALAPPDATA%\Microsoft\PowerToys\MouseWithoutBorders\settings.json`
     to add this machine to both `MachineMatrixString` and `MachinePool`
     (name + the ID shown in the widget's popup), then relaunch.
   - Add a hosts-file entry
     (`C:\Windows\System32\drivers\etc\hosts`) mapping this machine's name
     to its LAN IP, so Windows can resolve it for edge-crossing routing.

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
  exactly match what you add to Windows' `MachinePool` setting (see
  `daemon/PROTOCOL.md`'s "Matrix and MachinePool" section — the classic UI
  can't actually persist this in this PowerToys version, so it has to be
  hand-edited into `settings.json` directly).
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
- `daemon/src/bin/mwb_probe.rs`, `daemon/src/bin/mwb_readonly.rs` — early
  protocol-debugging test clients, superseded by `daemon.rs` and the
  shared `lib.rs` modules; not yet refactored to use them (technical debt,
  not currently needed for anything).
- `daemon/PROTOCOL.md` — the full reverse-engineered wire protocol spec,
  plus everything learned about Windows-side state (the Matrix/MachinePool
  UI persistence bug, DNS/IP-mapping quirks, keyboard-layout translation).
  Read this before changing any of the crypto/framing/handshake/keymap
  code.
- `daemon/systemd/mwb-omarchy-bridge.service` — the service unit template.

## Next steps

- Refactor `daemon/src/bin/mwb_probe.rs`/`mwb_readonly.rs` to use the
  shared `lib.rs` modules, or remove them — they're pure technical debt at
  this point.
