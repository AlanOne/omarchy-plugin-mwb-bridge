// Runtime config + live status, both read/written as JSON files under a
// per-OS data dir (Linux: ~/.local/share/omarchy-mwb-bridge/, macOS:
// ~/Library/Application Support/mwb-mac-bridge/) so each platform's
// front-end (the Omarchy bar widget on Linux, the menu bar app on macOS) can
// configure the daemon and show its connection status without either side
// needing to know about the other's internals. The daemon only reads
// config.json at startup (no live-reload) — the front-end restarts the
// daemon after saving a change, matching how Cameras' go2rtc container is
// restarted after config edits rather than hot-reloaded.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Deserialize, Clone)]
pub struct Config {
    pub security_key: String,
    pub windows_ip: String,
    pub machine_name: String,
    pub machine_id: u32,
    // Which XKB layout/variant the virtual keyboard is compiled with — must
    // match this machine's actual active layout (the Omarchy plugin detects
    // it via `hyprctl getoption input:kb_layout` when saving, rather than
    // this ever being hardcoded to one layout). Defaulted for configs
    // written before these fields existed; "us"/"" is a reasonable fallback,
    // not a real detection.
    #[serde(default = "default_xkb_layout")]
    pub xkb_layout: String,
    #[serde(default)]
    pub xkb_variant: String,
    // Multiplier applied to every scroll-wheel event (vertical and
    // horizontal) before forwarding — see handle_mouse's own comment for why
    // one's needed at all: Wayland's virtual-pointer axis value isn't in the
    // same units as Windows' WHEEL_DELTA, so a fixed baseline conversion is
    // applied first, then this multiplier on top for the user to taste.
    #[serde(default = "default_scroll_speed")]
    pub scroll_speed: f64,
}

fn default_xkb_layout() -> String {
    "us".to_string()
}

fn default_scroll_speed() -> f64 {
    1.0
}

#[derive(Serialize, Deserialize)]
pub struct Status {
    pub connected: bool,
    pub peer: String,
    pub detail: String,
    pub updated_epoch: u64,
}

#[cfg(target_os = "linux")]
fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join(".local/share/omarchy-mwb-bridge")
}

#[cfg(target_os = "macos")]
fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join("Library/Application Support/mwb-mac-bridge")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

fn status_path() -> PathBuf {
    data_dir().join("status.json")
}

/// Returns `None` if the config file is missing or malformed (not yet set up
/// via the plugin's settings form, or mid-write) — the caller should treat
/// this as "not configured yet" and retry, not as a fatal error.
pub fn load_config() -> Option<Config> {
    let text = std::fs::read_to_string(config_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Reads back whatever `write_status` last wrote — `None` if the daemon
/// hasn't written a status yet (not yet configured, or this is the very
/// first tick after starting). Best-effort in the same spirit as
/// `write_status`: a missing/malformed file just means "nothing to show
/// yet," not an error worth surfacing to the front-end.
pub fn read_status() -> Option<Status> {
    let text = std::fs::read_to_string(status_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Best-effort: a failed status write shouldn't ever take down the
/// connection loop, so errors are silently swallowed.
pub fn write_status(connected: bool, peer: &str, detail: &str) {
    let dir = data_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let status = Status {
        connected,
        peer: peer.to_string(),
        detail: detail.to_string(),
        updated_epoch: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    };
    if let Ok(body) = serde_json::to_string(&status) {
        let _ = std::fs::write(status_path(), body);
    }
}
