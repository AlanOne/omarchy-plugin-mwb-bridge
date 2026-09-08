# MWB Bridge

A status + control widget for [mwb-omarchy-bridge](https://github.com/AlanOne/mwb-omarchy-bridge)
— a software-KVM daemon that lets a Windows PC's physical keyboard/mouse (via Microsoft
PowerToys' **Mouse Without Borders**) control an Omarchy (Hyprland) machine directly,
without going through any of the mainstream tools (Synergy/Barrier/Input Leap/Deskflow),
none of which currently work on Hyprland.

## How it works

This plugin doesn't implement any of the actual bridge — that's a separate Rust daemon,
running as a systemd `--user` service (`mwb-omarchy-bridge.service`). The plugin is a thin
QML front-end for it:

- **Status**: reads the daemon's live connection state from
  `~/.local/share/omarchy-mwb-bridge/status.json` and the systemd unit's own run state, and
  shows both in the bar icon and popup.
- **Control**: Start/Stop/Restart buttons run `systemctl --user <action> mwb-omarchy-bridge.service`.
- **Settings**: a form for the shared security key, the Windows PC's IP or hostname, and
  this machine's name — writes `~/.local/share/omarchy-mwb-bridge/config.json` (the
  daemon's only config source, read once at startup) and restarts the service to apply.

See the daemon repo's `PROTOCOL.md` for the full story of what it took to get Mouse Without
Borders' wire protocol — and, more importantly, several Windows-side PowerToys state bugs —
actually working.

## Prerequisites

The daemon itself: build and install
[mwb-omarchy-bridge](https://github.com/AlanOne/mwb-omarchy-bridge) first (its README
covers building the systemd service). This plugin is only useful once that service exists
on the system — it doesn't build or install the daemon itself.

## Install

Drop this directory into `~/.config/omarchy/plugins/io.github.alanone.mwb-bridge/`, then
add it to your bar layout in `~/.config/omarchy/shell.json`:

```json
{
  "id": "io.github.alanone.mwb-bridge"
}
```

## Settings

- `popupWidth` — popup width in pixels (default `340`).

Everything else (security key, Windows PC address, this machine's name) is edited from the
popup itself, not via plugin settings — see **How it works** above.
