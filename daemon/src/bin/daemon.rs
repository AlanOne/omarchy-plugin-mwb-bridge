// The real persistent daemon: holds a long-lived connection to Mouse
// Without Borders' message server, reconnecting automatically on drop, and
// feeds decoded Mouse/Keyboard packets into the Wayland injector. See
// PROTOCOL.md for the wire protocol this implements.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flate2::read::DeflateDecoder;
use mwb_omarchy_bridge::config::{self, Config};
use mwb_omarchy_bridge::mwb_protocol::*;
use mwb_omarchy_bridge::vk_keycode::vk_to_evdev;
use mwb_omarchy_bridge::wayland_input::WaylandInput;
use rand::RngExt;

// Same message-server port MWB uses on every machine (BASE_PORT + 1). We
// listen here too so Windows' own outbound connection back to us (it
// expects a symmetric pair of sockets, one per direction, same as between
// two real MWB installs) succeeds instead of timing out — cosmetic only,
// since real Mouse/Keyboard forwarding already works over the connection we
// initiate; this side only needs to complete the handshake and stay open.
const LISTEN_PORT: u16 = 15101;
// The separate "clipboard server" port (BASE_PORT) that file bytes travel
// over — a distinct TCP connection from the message server above, opened
// on demand in whichever direction a file transfer is actually happening.
const FILE_LISTEN_PORT: u16 = 15100;

// How long any single read or write may block before we give up on a
// connection. Without this, a connection that goes silently dead (no RST,
// no FIN — the peer just stops responding, e.g. the Windows PC sleeping,
// a Wi-Fi hiccup, a NAT table eviction) hangs the blocking read forever
// with no error to trigger the existing reconnect loop — confirmed this
// happening live: the main thread sat blocked in a plain socket read
// (`wait_woken` in /proc/<pid>/task/*/status) for 3+ hours after Windows
// simply stopped sending anything, no crash, no reconnect attempt, control
// just silently stopped working. A timed-out read/write returns a normal
// I/O error, which every caller already propagates via `?` into the
// existing "log it, sleep 3s, reconnect" handling — no new error handling
// needed, just making sure a stall can't block forever in the first place.
//
// Originally 30s, raised to 5 minutes after confirming *that* value was
// too aggressive: real MWB apparently doesn't keep an idle connection
// "hot" with any periodic keepalive-style packet independent of actual
// activity — confirmed live, a genuinely healthy connection with nobody
// touching the Windows mouse/keyboard went quiet for 30-70s at a time,
// repeatedly, causing an unwanted reconnect (and a few seconds of KVM
// downtime) roughly every minute during normal idle use. 5 minutes still
// recovers *far* faster than the original no-timeout-at-all hang, while
// being generous enough to essentially never fire during legitimate idle
// gaps between actual use.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(300);

fn set_socket_timeouts(stream: &TcpStream) {
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
}

// Standard XKB "us" layout modifier bit indices (Shift/Ctrl/Alt/Super) —
// matches xkbcommon's default assignment for this layout. Not derived from
// the actual compiled keymap yet; verify against it if modifier behavior
// ever looks wrong for a different layout.
const MOD_SHIFT: u32 = 1 << 0;
const MOD_CTRL: u32 = 1 << 2;
const MOD_ALT: u32 = 1 << 3;
const MOD_SUPER: u32 = 1 << 6;
// Mod5 / "Level3Shift" — what layouts using xkeyboard-config's
// `level3(ralt_switch)` (which ours does, via Right Alt) bind AltGr to. This
// is a distinct real modifier from Alt/Mod1: layouts with AltGr-level
// characters (e.g. this Slovenian layout's `<`/`>` on the comma/period keys)
// only resolve to level 3 when this bit is set, not MOD_ALT.
const MOD_LEVEL3: u32 = 1 << 7;
// Mod2 — XKB's conventional NumLock lock-modifier.
const LOCK_NUMLOCK: u32 = 1 << 4;
const VK_NUMLOCK: u32 = 0x90;

// Win32 WM_* message constants carried in Mouse/Keyboard dwFlags fields.
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_XBUTTONUP: u32 = 0x020C;
const WM_MOUSEHWHEEL: u32 = 0x020E;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
// Standard Linux evdev "side"/"extra" buttons — the conventional back/
// forward mapping used by browsers and file managers alike.
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
// LLKHF_UP: bit 7 of a low-level-keyboard-hook's flags marks a key-up event.
const LLKHF_UP: u32 = 0x80;

const VK_L: u32 = 0x4C;

// Real MWB's own HotKeyLockMachine feature (a double-tap of the configured
// combo — Win+L here, confirmed with Alan, an unmodified default) sends the
// combo's own keys as an ordinary Keyboard-packet burst before locking
// itself: all keys down back-to-back, then all up back-to-back, no new
// package type — see PROTOCOL.md. No natural keypress produces this
// pattern (a human pressing even a 2-key chord has real, non-zero timing
// between each key), so a tight time window reliably distinguishes it.
const LOCK_COMBO_WINDOW: Duration = Duration::from_millis(150);

/// Recognizes that burst arriving as regular Keyboard packets: a small
/// ring buffer of recent (vk, pressed) events with timestamps, firing once
/// LWIN-down, L-down, LWIN-up, and L-up are all present within
/// `LOCK_COMBO_WINDOW` of each other (order doesn't matter beyond that —
/// real MWB sends down-then-up, but matching by presence-in-window is
/// simpler and just as reliable given no natural keypress could produce
/// this set that fast either way).
struct LockComboDetector {
    recent: VecDeque<(u32, bool, Instant)>,
}

impl LockComboDetector {
    fn new() -> Self {
        Self { recent: VecDeque::with_capacity(8) }
    }

    /// Returns true (once) when the combo is detected — clears its buffer
    /// afterward so the same burst can't re-fire on a later call.
    fn observe(&mut self, vk: u32, pressed: bool) -> bool {
        let now = Instant::now();
        self.recent.push_back((vk, pressed, now));
        while self.recent.len() > 8 {
            self.recent.pop_front();
        }
        while self.recent.front().is_some_and(|&(_, _, t)| now.duration_since(t) > LOCK_COMBO_WINDOW) {
            self.recent.pop_front();
        }

        let is_win = |v: u32| v == 0x5B || v == 0x5C;
        let has = |target_win: bool, want_pressed: bool| {
            self.recent.iter().any(|&(v, p, _)| p == want_pressed && (if target_win { is_win(v) } else { v == VK_L }))
        };
        let fired = has(true, true) && has(false, true) && has(true, false) && has(false, false);
        if fired {
            self.recent.clear();
        }
        fired
    }
}

struct ModState {
    depressed: u32,
}

impl ModState {
    fn new() -> Self {
        Self { depressed: 0 }
    }

    /// Updates tracked modifier state for this VK if it's a modifier key,
    /// returns true if it was one (caller still forwards the raw keypress
    /// either way — modifiers are real keys too).
    fn update(&mut self, vk: u32, pressed: bool) -> bool {
        let bit = match vk {
            0x10 | 0xA0 | 0xA1 => MOD_SHIFT,
            0x11 | 0xA2 | 0xA3 => MOD_CTRL,
            0x12 | 0xA4 => MOD_ALT,
            // VK_RMENU: Windows reports AltGr as a synthetic VK_LCONTROL
            // down/up pair immediately around the real VK_RMENU (verified
            // empirically — every AltGr press logs 0xA2 then 0xA5, in that
            // order, on both press and release). Forwarding that fake Ctrl
            // as MOD_CTRL and this key as MOD_ALT never reaches level 3 —
            // clear the fake Ctrl bit and use MOD_LEVEL3 instead, matching
            // what `level3(ralt_switch)` actually binds Right Alt to.
            0xA5 => {
                self.depressed &= !MOD_CTRL;
                MOD_LEVEL3
            }
            0x5B | 0x5C => MOD_SUPER,
            _ => return false,
        };
        if pressed {
            self.depressed |= bit;
        } else {
            self.depressed &= !bit;
        }
        true
    }
}

fn handle_mouse(wl: &mut WaylandInput, m1: u32, m2: u32, m3: u32, flags: u32) {
    match flags {
        WM_MOUSEMOVE => {
            // Observed in real traffic (likely an edge-crossing overshoot):
            // an occasional out-of-range value that's actually small and
            // negative, wrapping to a huge u32 (e.g. 4294967271 = -25 as
            // i32) when read as one. Clamp back into the valid 0..=65535
            // absolute-coordinate range rather than forwarding it verbatim,
            // which would otherwise send the cursor somewhere nonsensical.
            let x = (m1 as i32).clamp(0, 65535) as u32;
            let y = (m2 as i32).clamp(0, 65535) as u32;
            wl.move_absolute(x, y, 65535, 65535)
        }
        WM_LBUTTONDOWN => wl.button(BTN_LEFT, true),
        WM_LBUTTONUP => wl.button(BTN_LEFT, false),
        WM_RBUTTONDOWN => wl.button(BTN_RIGHT, true),
        WM_RBUTTONUP => wl.button(BTN_RIGHT, false),
        WM_MBUTTONDOWN => wl.button(BTN_MIDDLE, true),
        WM_MBUTTONUP => wl.button(BTN_MIDDLE, false),
        WM_MOUSEWHEEL => {
            // WheelDelta is a signed 16-bit value in Windows' usual +/-120
            // per notch; wl_pointer.axis wants "120ths of a click"-ish
            // units too, so pass it through with sign flipped (Windows:
            // positive = away from user/up; Wayland vertical-scroll:
            // positive = down) — matches physical scroll-wheel direction.
            let delta = m3 as i32 as i16 as f64;
            wl.scroll_vertical(-delta);
        }
        WM_MOUSEHWHEEL => {
            // Same signed-16-bit-in-m3 shape as WM_MOUSEWHEEL (confirmed
            // from real MWB's own InputHook.cs: WheelDelta is set from the
            // same HIWORD(MouseData) read for every mouse message, not
            // just WM_MOUSEWHEEL). Windows: positive = right; Wayland
            // horizontal-scroll: positive = right too, no sign flip needed.
            let delta = m3 as i32 as i16 as f64;
            wl.scroll_horizontal(delta);
        }
        // Real MWB (per InputHook.cs, confirmed from source): every mouse
        // message's WheelDelta slot (here, m3) is set from HIWORD(MouseData)
        // regardless of message type — for WM_MOUSEWHEEL that's the scroll
        // delta, but for WM_XBUTTONDOWN/UP it's *which* extra button
        // (XBUTTON1=1, XBUTTON2=2), per the Win32 MSLLHOOKSTRUCT contract.
        // These are standard side buttons (e.g. an MX Master's Back/
        // Forward) that a real low-level mouse hook — and so real MWB —
        // captures just fine; this daemon just never had a mapping for
        // them until now.
        WM_XBUTTONDOWN => wl.button(if m3 == 2 { BTN_EXTRA } else { BTN_SIDE }, true),
        WM_XBUTTONUP => wl.button(if m3 == 2 { BTN_EXTRA } else { BTN_SIDE }, false),
        _ => {}
    }
}

fn handle_keyboard(wl: &mut WaylandInput, mods: &mut ModState, vk: u32, flags: u32) {
    // Windows' own NumLock state and this machine's are two independent,
    // unsynchronized locks. Forwarding the raw NumLock keypress toggles our
    // side's lock-state via the compositor's own keymap-driven handling —
    // if the two ever disagree, numpad digit keys silently become
    // navigation keys (Home/End/arrows/etc.) instead, since which one a
    // numpad key produces depends entirely on the *receiving* side's
    // NumLock state. Numpad keys below force NumLock-locked on every press
    // instead, so this never needs tracking or toggling at all.
    if vk == VK_NUMLOCK {
        return;
    }

    let pressed = (flags & LLKHF_UP) == 0;
    let is_mod = mods.update(vk, pressed);
    let Some(evdev_code) = vk_to_evdev(vk) else {
        eprintln!("(no evdev mapping for VK 0x{vk:02x}, ignoring)");
        return;
    };
    if is_mod {
        wl.modifiers(mods.depressed, 0, 0, 0);
    } else if (0x60..=0x6F).contains(&vk) {
        wl.modifiers(mods.depressed, 0, LOCK_NUMLOCK, 0);
    }
    wl.key(evdev_code, pressed);
}

/// Shared state between the client connection's receive loop, the file-
/// server listener (port 15100), and the local clipboard-watcher thread —
/// bundled together since nearly every clipboard-related function needs at
/// least one of these. Cloning is just cloning the inner `Arc`s.
#[derive(Clone)]
struct ClipboardShared {
    // Text most recently applied from Windows, so the outbound watcher can
    // recognize its own echo and not immediately bounce it right back.
    last_applied_text: Arc<Mutex<Option<String>>>,
    // The exact `file://...` URI most recently applied from a received
    // file, for the same echo-prevention purpose.
    last_applied_file_uri: Arc<Mutex<Option<String>>>,
    // The exact PNG bytes most recently applied from a received clipboard
    // image, for the same echo-prevention purpose.
    last_applied_image: Arc<Mutex<Option<Vec<u8>>>>,
    // What's currently available to serve when Windows connects to our
    // file-server listener (big-path only — small-path file/image sync
    // sends inline and never touches this) — set when the local clipboard
    // watcher detects new local big-path content, read by that listener.
    pending_outbound: Arc<Mutex<Option<PendingOutbound>>>,
    // Windows' own machine ID, learned from the Src field of anything it's
    // sent us — needed to address a MachineSwitched package directly to
    // it. Persisted here (not just a per-connection local) so a reconnect
    // doesn't forget it and go a full round without being able to send
    // MachineSwitched — it's the same physical Windows install and this
    // essentially never changes short of a PowerToys reset.
    peer_machine_id: Arc<Mutex<Option<u32>>>,
}

impl ClipboardShared {
    fn new() -> Self {
        Self {
            last_applied_text: Arc::new(Mutex::new(None)),
            last_applied_file_uri: Arc::new(Mutex::new(None)),
            last_applied_image: Arc::new(Mutex::new(None)),
            pending_outbound: Arc::new(Mutex::new(None)),
            peer_machine_id: Arc::new(Mutex::new(None)),
        }
    }
}

/// A local file on disk ready to serve over the big-path clipboard-server
/// connection — either a real file (served under its own name) or a
/// clipboard image over the small-path size threshold (written to a temp
/// file, served under the literal `BIG_PATH_IMAGE_NAME` tag real MWB uses
/// for this case instead of a filename).
#[derive(Clone)]
enum PendingOutbound {
    File(PathBuf),
    Image(PathBuf),
}

/// What the local clipboard-watcher thread detected and handed off to the
/// client connection's send loop.
enum ClipboardEvent {
    Text(String),
    File(PathBuf),
    Image(Vec<u8>),
    BigImage(PathBuf),
}

/// Windows -> Omarchy clipboard sync, small-path text only (real MWB's
/// "big path", for clipboard payloads over ~1MB or files, uses an entirely
/// separate socket/framing on port 15100 — not implemented here). Windows'
/// side reverses the same recipe to send: build "TXT" + text (+ "RTF"/"HTM"
/// + content) + SEP, UTF-16LE-encode, then raw-DEFLATE-compress (no zlib/
/// gzip wrapper) before chunking into 48-byte pieces — see PROTOCOL.md.
/// We only care about the "TXT" fragment; RTF/HTM (if present) are ignored.
/// Records the applied text into `last_applied` so the outbound clipboard
/// watcher (which polls the same Linux clipboard we're about to write to)
/// can recognize its own echo and not immediately bounce it right back to
/// Windows as if the Omarchy side had made a fresh local copy.
fn apply_incoming_clipboard_text(compressed: &[u8], last_applied: &Arc<Mutex<Option<String>>>) {
    let mut inflated = Vec::new();
    if let Err(e) = DeflateDecoder::new(compressed).read_to_end(&mut inflated) {
        eprintln!("(clipboard: failed to inflate incoming text, ignoring: {e})");
        return;
    }
    let utf16: Vec<u16> = inflated.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
    let decoded = String::from_utf16_lossy(&utf16);

    let Some(text) = decoded.split(CLIPBOARD_SEP).find_map(|frag| frag.strip_prefix("TXT")) else {
        eprintln!("(clipboard: no TXT fragment in incoming data, ignoring)");
        return;
    };

    let mut child = match Command::new("wl-copy").stdin(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("(clipboard: failed to launch wl-copy: {e})");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let _ = child.wait();
    *last_applied.lock().unwrap() = Some(text.to_string());
    println!("(clipboard: applied {} chars from Windows)", text.chars().count());
}

/// Reads the Linux clipboard's current plain-text content, if any. `wl-paste`
/// exits non-zero (empty stdout) when the clipboard is empty or holds a
/// non-text format (confirmed empirically) — both cases just mean "nothing
/// to sync right now", not an error.
fn read_local_clipboard_text() -> Option<String> {
    let output = Command::new("wl-paste").args(["--no-newline", "--type", "text/plain"]).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reads the Linux clipboard's current `text/uri-list` content (what file
/// managers put there on copy), if any — same non-zero-exit-means-nothing
/// convention as `read_local_clipboard_text`.
fn read_local_clipboard_uri_list() -> Option<String> {
    let output = Command::new("wl-paste").args(["--no-newline", "--type", "text/uri-list"]).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reads the Linux clipboard's current `image/png` content as raw bytes, if
/// any — same non-zero-exit-means-nothing convention as
/// `read_local_clipboard_text`, but binary-safe (no `--no-newline`, which
/// is a text-only concern; PNG bytes are used byte-for-byte as-is).
fn read_local_clipboard_image() -> Option<Vec<u8>> {
    let output = Command::new("wl-paste").args(["--type", "image/png"]).output().ok()?;
    output.status.success().then_some(output.stdout)
}

/// Decodes `%XX` percent-escapes in a `file://` URI's path portion back
/// into raw bytes (interpreted as UTF-8) — just enough of RFC 3986 for
/// local file paths, not a general-purpose URI decoder.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The inverse of `percent_decode`, for building a `file://` URI to hand to
/// `wl-copy` — percent-encodes everything outside RFC 3986's unreserved set
/// (letters/digits/`-_.~/`), which covers spaces and non-ASCII filenames.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parses a `text/uri-list` blob (one URI per line, `#`-prefixed lines are
/// comments — RFC 2483) and returns the first `file://` entry's local path,
/// percent-decoded. Real MWB's own file transfer only ever handles one file
/// per send anyway (see PROTOCOL.md), so a multi-file selection here just
/// takes the first and silently drops the rest, matching that limitation
/// rather than trying to exceed it.
fn parse_first_file_uri(uri_list: &str) -> Option<PathBuf> {
    uri_list
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .and_then(|line| line.strip_prefix("file://"))
        .map(|path| PathBuf::from(percent_decode(path)))
}

/// Where received files get written before being placed on the clipboard —
/// mirrors the plugin's existing `~/.cache/omarchy-mwb-bridge-*` naming.
fn clipboard_files_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join(".cache/omarchy-mwb-bridge-files")
}

/// Strips the `:port` suffix from `windows_ip` (stored as `"host:15101"`)
/// to get the bare host for connecting to Windows' *other* port, 15100.
fn windows_clipboard_host(windows_ip: &str) -> &str {
    windows_ip.rsplit_once(':').map(|(host, _)| host).unwrap_or(windows_ip)
}

/// Returns `addr` with its port changed, preserving an IPv6 address's scope
/// ID and flow info if present. Plain `SocketAddr::new(addr.ip(), port)`
/// silently drops the scope ID — fine for a global IPv6/IPv4 address, but
/// breaks a link-local one (`fe80::...`), which needs it to be routable at
/// all. Confirmed live: a file-pull connection failed with "Invalid
/// argument" (EINVAL) after reconstructing a link-local peer address this
/// way — this network happened to route the message-server connection over
/// global IPv6 in most tests, masking the bug until it didn't.
fn with_port(addr: std::net::SocketAddr, port: u16) -> std::net::SocketAddr {
    match addr {
        std::net::SocketAddr::V4(v4) => std::net::SocketAddr::new((*v4.ip()).into(), port),
        std::net::SocketAddr::V6(v6) => {
            std::net::SocketAddr::V6(std::net::SocketAddrV6::new(*v6.ip(), port, v6.flowinfo(), v6.scope_id()))
        }
    }
}

/// Extracts the filename from a full **Windows** path (e.g.
/// `C:\Users\Alan\Downloads\file.txt`), which the file-transfer header's
/// `name` field always is for a normal file copy. `std::path::Path` isn't
/// safe for this on Linux — it only recognizes `/` as a separator on this
/// platform, so `Path::new(windows_path).file_name()` returns the *entire*
/// backslash-laden string as one component (confirmed live: a received
/// file landed named literally `C:\Users\Alan\Downloads\copytest.txt`).
fn windows_basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Applies a received file to the Linux clipboard as a `text/uri-list`
/// entry (the standard GTK/Wayland equivalent of Windows' `CF_HDROP`), and
/// records the URI for the same echo-prevention purpose as
/// `apply_incoming_clipboard_text`.
fn apply_incoming_clipboard_file(dest: &Path, last_applied_file_uri: &Arc<Mutex<Option<String>>>) {
    let uri = format!("file://{}", percent_encode_path(&dest.to_string_lossy()));
    let mut child = match Command::new("wl-copy").args(["--type", "text/uri-list"]).stdin(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("(clipboard: failed to launch wl-copy for file: {e})");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(uri.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    let _ = child.wait();
    *last_applied_file_uri.lock().unwrap() = Some(uri);
}

/// Applies received clipboard-image bytes directly to the Linux clipboard
/// as `image/png` — no decoding needed, real MWB's wire payload for
/// ClipboardImage is already a plain PNG file verbatim (confirmed from
/// source: `image.Save(stream, ImageFormat.Png)` on the sending side,
/// `Image.FromStream` on receipt), unlike text there's no tag/SEP wrapper
/// or compression to undo.
fn apply_incoming_clipboard_image(png_bytes: &[u8], last_applied_image: &Arc<Mutex<Option<Vec<u8>>>>) {
    let mut child = match Command::new("wl-copy").args(["--type", "image/png"]).stdin(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("(clipboard: failed to launch wl-copy for image: {e})");
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(png_bytes);
    }
    let _ = child.wait();
    *last_applied_image.lock().unwrap() = Some(png_bytes.to_vec());
    println!("(clipboard: applied a {}-byte image from Windows)", png_bytes.len());
}

/// Locks this machine's session in response to Windows' own lock-both-
/// machines double-tap (see `LockComboDetector`) — `omarchy-system-lock`
/// is the same command Omarchy's own idle-service uses for its own
/// timeout-triggered lock (`Service.qml`'s `lockSystem()`), so this landing
/// as a fresh press of it needs no special-casing on the Omarchy side.
fn lock_local_machine() {
    println!("[client] Windows' lock-both-machines combo detected — locking this machine too.");
    match Command::new("omarchy-system-lock").spawn() {
        Ok(mut child) => {
            let _ = child.wait();
        }
        Err(e) => eprintln!("(lock: failed to launch omarchy-system-lock: {e})"),
    }
}

/// Reads `size` real bytes from a clipboard-server connection's raw file
/// stream, decrypting as it goes. Real MWB pads the actual transmitted
/// byte count up to a multiple of `PACKAGE_SIZE` (32) with zero-fill after
/// the real content (`SocketStuff.SendFileEx`) — read that full padded
/// length (always a clean multiple of the AES block size) and truncate the
/// decrypted result back down to `size` real bytes.
fn read_padded_body(stream: &mut TcpStream, cipher: &CbcState, chain: &mut [u8; 16], size: usize) -> std::io::Result<Vec<u8>> {
    const CHUNK: usize = 1 << 20; // matches real MWB's own NETWORK_STREAM_BUF_SIZE
    let padded_len = size.div_ceil(32) * 32;
    let mut decrypted = Vec::with_capacity(padded_len);
    let mut remaining = padded_len;
    while remaining > 0 {
        let this_chunk = remaining.min(CHUNK);
        let mut ct = vec![0u8; this_chunk];
        stream.read_exact(&mut ct)?;
        decrypted.extend_from_slice(&cipher.decrypt(chain, &ct));
        remaining -= this_chunk;
    }
    decrypted.truncate(size);
    Ok(decrypted)
}

/// The write-side counterpart of `read_padded_body`: zero-pads `data` up to
/// a multiple of 32 bytes before encrypting and writing, matching what real
/// MWB's own sender does (and what our own receive side above expects).
fn write_padded_body(stream: &mut TcpStream, cipher: &CbcState, chain: &mut [u8; 16], data: &[u8]) -> std::io::Result<()> {
    const CHUNK: usize = 1 << 20;
    let padded_len = data.len().div_ceil(32) * 32;
    let mut padded = vec![0u8; padded_len];
    padded[..data.len()].copy_from_slice(data);
    for chunk in padded.chunks(CHUNK) {
        stream.write_all(&cipher.encrypt(chain, chunk))?;
    }
    Ok(())
}

/// Omarchy -> Windows clipboard sync: polls the Linux clipboard (there's no
/// simple blocking "notify me on change" primitive over `wl-clipboard`'s CLI
/// tools, so polling is the simple option) and forwards genuinely new local
/// content into `clip_tx` for whichever connection is currently live to pick
/// up and send. Checks for a file first each tick (prioritizing file
/// semantics if a copy happens to offer both `text/uri-list` and
/// `text/plain`), falling back to plain text. Seeds its baseline from
/// whatever's already on the clipboard at startup without sending it — only
/// actual *changes* get synced, so old clipboard content from before the
/// daemon started doesn't get pushed to Windows unexpectedly.
fn run_clipboard_watcher(clip_tx: Sender<ClipboardEvent>, shared: ClipboardShared) {
    let mut last_seen_file = read_local_clipboard_uri_list().as_deref().and_then(parse_first_file_uri);
    let mut last_seen_image = read_local_clipboard_image();
    let mut last_seen_text = read_local_clipboard_text();
    loop {
        std::thread::sleep(Duration::from_millis(500));

        if let Some(path) = read_local_clipboard_uri_list().as_deref().and_then(parse_first_file_uri) {
            if last_seen_file.as_ref() != Some(&path) {
                last_seen_file = Some(path.clone());
                let uri = format!("file://{}", percent_encode_path(&path.to_string_lossy()));
                let is_echo = shared.last_applied_file_uri.lock().unwrap().as_deref() == Some(uri.as_str());
                if !is_echo {
                    let _ = clip_tx.send(ClipboardEvent::File(path));
                }
            }
            continue; // a file is present — don't also fall through to image/text
        }

        if let Some(bytes) = read_local_clipboard_image() {
            if last_seen_image.as_ref() != Some(&bytes) {
                last_seen_image = Some(bytes.clone());
                let is_echo = shared.last_applied_image.lock().unwrap().as_deref() == Some(bytes.as_slice());
                if !is_echo {
                    if bytes.len() < MAX_SMALL_PATH_SIZE {
                        let _ = clip_tx.send(ClipboardEvent::Image(bytes));
                    } else {
                        // Big-path: write to a fixed temp file (only ever
                        // one pending outbound image at a time) and let the
                        // existing file-transfer machinery serve it,
                        // tagged as an image rather than a real filename.
                        let dir = clipboard_files_dir();
                        if std::fs::create_dir_all(&dir).is_ok() {
                            let path = dir.join("outbound-image.png");
                            if std::fs::write(&path, &bytes).is_ok() {
                                let _ = clip_tx.send(ClipboardEvent::BigImage(path));
                            }
                        }
                    }
                }
            }
            continue; // an image is present — don't also fall through to text
        }

        let Some(current) = read_local_clipboard_text() else { continue };
        if last_seen_text.as_deref() == Some(current.as_str()) {
            continue;
        }
        last_seen_text = Some(current.clone());
        if shared.last_applied_text.lock().unwrap().as_deref() == Some(current.as_str()) {
            continue; // our own echo from applying Windows' clipboard moments ago
        }
        let _ = clip_tx.send(ClipboardEvent::Text(current));
    }
}

/// Announces big-path content (a file or a big clipboard image) to the
/// peer: the `Clipboard` beat, then — if we've learned the peer's machine
/// ID from something it's sent us already — a follow-up `MachineSwitched`
/// package addressed directly to it. Real MWB's own auto-pull is gated on
/// `MachineSwitched`, not the beat alone (see PROTOCOL.md); without this
/// follow-up, a real PowerToys install never actually connects to fetch
/// what we've announced.
///
/// **Both packages need a genuinely fresh, never-before-used `Id`, not
/// just one that's unique within this connection.** Windows' own receive
/// dispatcher (`Receiver.PreProcess`) keeps a 50-slot ring buffer of
/// recently-seen `Id` values that persists for the life of the Windows
/// process, silently dropping (no log, no error, switch statement never
/// runs) any repeat — every package type is subject to this *except*
/// `ClipboardText`/`ClipboardImage`/`Handshake`/`HandshakeAck`, which are
/// explicitly exempted. `Clipboard`/`MachineSwitched` are not exempted.
/// Confirmed live: a per-connection counter starting at a fixed value
/// resends the exact same `Id` on every reconnect, which Windows
/// remembers forever (nothing else ever evicts those two slots since nothing
/// else non-exempt is ever sent) — every attempt after the first was
/// silently deduped this way. Random IDs make a repeat astronomically
/// unlikely regardless of how many times this daemon restarts.
fn announce_big_path(
    stream: &mut TcpStream,
    cipher: &CbcState,
    write_chain: &mut [u8; 16],
    magic_number: u32,
    src_id: u32,
    peer_machine_id: Option<u32>,
) -> std::io::Result<()> {
    let mut beat = build_clipboard_beat(rand::rng().random(), src_id);
    finalize_send_buf(&mut beat, magic_number);
    stream.write_all(&cipher.encrypt(write_chain, &beat))?;

    if let Some(des_id) = peer_machine_id {
        // Mirror the exact real sequence captured live from Windows'
        // own Ctrl+Alt+F1-style switch: HideMouse immediately before
        // MachineSwitched, both addressed to the newly-active machine —
        // not just the isolated MachineSwitched case-block in source.
        let mut hide = build_hide_mouse(rand::rng().random(), src_id, des_id);
        finalize_send_buf(&mut hide, magic_number);
        stream.write_all(&cipher.encrypt(write_chain, &hide))?;

        let mut switched = build_machine_switched(rand::rng().random(), src_id, des_id);
        finalize_send_buf(&mut switched, magic_number);
        stream.write_all(&cipher.encrypt(write_chain, &switched))?;
    } else {
        eprintln!("(clipboard: haven't learned the peer's machine ID yet, skipping MachineSwitched — beat sent alone, may not trigger a real pull)");
    }
    Ok(())
}

/// Runs one connection's lifecycle over an already-connected/accepted
/// stream: prime, handshake, then receive forever until the socket errors or
/// closes. Shared by both the outbound client connection (which injects
/// decoded Mouse/Keyboard into Wayland, `wl = Some(..)`) and the inbound
/// listener side (which only completes the protocol so Windows sees a
/// healthy reverse connection, `wl = None` — nothing it sends there needs
/// forwarding anywhere). Returns (normally via `?`) on any I/O error so the
/// caller can reconnect/re-accept.
///
/// `clipboard_rx` is only `Some` on the client role — outbound clipboard
/// sync happens over the one connection real forwarding already runs over,
/// not the cosmetic reverse-listener one, so there's no ambiguity about
/// which of our two sockets a locally-detected clipboard change goes out
/// on. Incoming clipboard (applying Windows' clipboard here) is handled
/// regardless of role, since either connection could plausibly carry it.
fn run_session(
    mut stream: TcpStream,
    mut wl: Option<&mut WaylandInput>,
    role: &str,
    cfg: &Config,
    clipboard_rx: Option<&Receiver<ClipboardEvent>>,
    shared: &ClipboardShared,
) -> std::io::Result<()> {
    let magic_number = get_24bit_hash(&cfg.security_key);
    let key = derive_key(&cfg.security_key);
    let iv = derive_iv();

    let cipher = CbcState::new(&key);
    let mut read_chain = iv;
    let mut write_chain = iv;

    let mut priming_out = [0u8; 16];
    rand::rng().fill(&mut priming_out);
    let ct = cipher.encrypt(&mut write_chain, &priming_out);
    stream.write_all(&ct)?;

    let mut priming_in_ct = [0u8; 16];
    stream.read_exact(&mut priming_in_ct)?;
    let _ = cipher.decrypt(&mut read_chain, &priming_in_ct);

    let our_machine1_4: [u32; 4] = {
        let mut r = rand::rng();
        [r.random(), r.random(), r.random(), r.random()]
    };
    let mut handshake = build_handshake(1, cfg.machine_id, our_machine1_4, &cfg.machine_name);
    finalize_send_buf(&mut handshake, magic_number);
    for _ in 0..10 {
        let ct = cipher.encrypt(&mut write_chain, &handshake);
        stream.write_all(&ct)?;
    }
    let our_flipped: [u32; 4] = our_machine1_4.map(|v| !v);
    println!("[{role}] Handshake sent, entering receive loop.");

    let mut mods = ModState::new();
    let mut lock_combo = LockComboDetector::new();
    let mut hi_count = 0u64;
    let mut clipboard_buf: Vec<u8> = Vec::new();
    let mut clipboard_kind: Option<u8> = None;
    // Arbitrary starting value distinct from the handshake's id=1 above —
    // just needs to be unique per outgoing package within this connection
    // (see build_clipboard_text_packages's doc comment on why).
    let mut next_clip_id: u32 = 1000;

    loop {
        if let Some(rx) = clipboard_rx {
            let mut latest = None;
            while let Ok(ev) = rx.try_recv() {
                latest = Some(ev); // coalesce rapid successive changes to the last one
            }
            match latest {
                Some(ClipboardEvent::Text(text)) => {
                    for mut pkg in build_clipboard_text_packages(&mut next_clip_id, cfg.machine_id, &text) {
                        finalize_send_buf(&mut pkg, magic_number);
                        let ct = cipher.encrypt(&mut write_chain, &pkg);
                        stream.write_all(&ct)?;
                    }
                    println!("[{role}] Sent clipboard text ({} chars) to peer.", text.chars().count());
                }
                Some(ClipboardEvent::Image(png_bytes)) => {
                    let len = png_bytes.len();
                    for mut pkg in build_clipboard_image_packages(&mut next_clip_id, cfg.machine_id, &png_bytes) {
                        finalize_send_buf(&mut pkg, magic_number);
                        let ct = cipher.encrypt(&mut write_chain, &pkg);
                        stream.write_all(&ct)?;
                    }
                    println!("[{role}] Sent clipboard image ({len} bytes) to peer.");
                }
                Some(ClipboardEvent::File(path)) => {
                    let name = path.display();
                    *shared.pending_outbound.lock().unwrap() = Some(PendingOutbound::File(path.clone()));
                    announce_big_path(&mut stream, &cipher, &mut write_chain, magic_number, cfg.machine_id, *shared.peer_machine_id.lock().unwrap())?;
                    println!(
                        "[{role}] Announced file {name} to peer (served if/when it connects to pull it)."
                    );
                }
                Some(ClipboardEvent::BigImage(path)) => {
                    *shared.pending_outbound.lock().unwrap() = Some(PendingOutbound::Image(path));
                    announce_big_path(&mut stream, &cipher, &mut write_chain, magic_number, cfg.machine_id, *shared.peer_machine_id.lock().unwrap())?;
                    println!(
                        "[{role}] Announced a big clipboard image to peer (served if/when it connects to pull it)."
                    );
                }
                None => {}
            }
        }

        let mut ct = [0u8; PACKAGE_SIZE];
        stream.read_exact(&mut ct)?;
        let mut pt = cipher.decrypt(&mut read_chain, &ct);
        let package_type = pt[0];
        let valid = validate_and_clean_recv_buf(&mut pt, magic_number);

        let mut full = pt.clone();
        if is_big_package(package_type) {
            let mut ct2 = [0u8; PACKAGE_SIZE];
            stream.read_exact(&mut ct2)?;
            let pt2 = cipher.decrypt(&mut read_chain, &ct2);
            full.extend_from_slice(&pt2);
        }
        if !valid {
            continue;
        }
        let src_id = unpack_u32_le(&full, 8);
        if src_id != 0 && src_id != ID_ALL {
            *shared.peer_machine_id.lock().unwrap() = Some(src_id);
        }

        match package_type {
            PACKAGE_TYPE_HANDSHAKE => {
                println!("[{role}] Replying to peer's Handshake with HandshakeAck.");
                let mut ack = build_handshake_ack(&full, cfg.machine_id, &cfg.machine_name);
                finalize_send_buf(&mut ack, magic_number);
                let ct = cipher.encrypt(&mut write_chain, &ack);
                stream.write_all(&ct)?;
            }
            PACKAGE_TYPE_HANDSHAKE_ACK => {
                let m1 = unpack_u32_le(&full, 16);
                let m2 = unpack_u32_le(&full, 20);
                let m3 = unpack_u32_le(&full, 24);
                let m4 = unpack_u32_le(&full, 28);
                let matched = m1 == our_flipped[0] && m2 == our_flipped[1] && m3 == our_flipped[2] && m4 == our_flipped[3];
                println!("[{role}] HandshakeAck received, challenge match: {matched}");
                if role == "client" {
                    let peer = String::from_utf8_lossy(&full[32..64]).trim_end().to_string();
                    let detail = if matched { "Connected" } else { "Handshake failed — check the security key" };
                    config::write_status(matched, &peer, detail);
                }
            }
            PACKAGE_TYPE_MOUSE => {
                let x = unpack_u32_le(&full, 16);
                let y = unpack_u32_le(&full, 20);
                let wheel = unpack_u32_le(&full, 24);
                let flags = unpack_u32_le(&full, 28);
                if let Some(w) = wl.as_deref_mut() {
                    println!(">>> MOUSE x={x} y={y} wheel={wheel} flags=0x{flags:x}");
                    handle_mouse(w, x, y, wheel, flags);
                }
            }
            PACKAGE_TYPE_KEYBOARD => {
                // Verified empirically against real traffic: wVk lives at
                // offset 24 and dwFlags at 28 (mirroring Mouse's WheelDelta/
                // dwFlags slots), not 16/20 as originally assumed.
                let vk = unpack_u32_le(&full, 24);
                let flags = unpack_u32_le(&full, 28);
                if lock_combo.observe(vk, (flags & LLKHF_UP) == 0) {
                    lock_local_machine();
                }
                if let Some(w) = wl.as_deref_mut() {
                    println!(">>> KEYBOARD vk=0x{vk:x} flags=0x{flags:x}");
                    handle_keyboard(w, &mut mods, vk, flags);
                }
            }
            PACKAGE_TYPE_CLIPBOARD_TEXT | PACKAGE_TYPE_CLIPBOARD_IMAGE => {
                // Bytes 16-63 of a ClipboardText/ClipboardImage package are
                // one contiguous 48-byte raw-data chunk (not Machine1-4 +
                // MachineName, despite the same package size) — see
                // PROTOCOL.md's clipboard section.
                clipboard_buf.extend_from_slice(&full[16..64]);
                clipboard_kind = Some(package_type);
            }
            PACKAGE_TYPE_CLIPBOARD_DATA_END => {
                match clipboard_kind {
                    Some(PACKAGE_TYPE_CLIPBOARD_TEXT) => {
                        apply_incoming_clipboard_text(&clipboard_buf, &shared.last_applied_text)
                    }
                    Some(PACKAGE_TYPE_CLIPBOARD_IMAGE) => {
                        apply_incoming_clipboard_image(&clipboard_buf, &shared.last_applied_image)
                    }
                    _ => {}
                }
                clipboard_buf.clear();
                clipboard_kind = None;
            }
            PACKAGE_TYPE_CLIPBOARD => {
                // The "beat" announcing a file is available (real MWB's
                // small "big data available" broadcast). Real MWB only
                // auto-pulls this around its own machine-switch event; this
                // bridge's fixed two-machine topology has no equivalent, so
                // treat any beat as "pull immediately" instead — see
                // PROTOCOL.md. Spawned in its own thread since a full file
                // transfer over a fresh connection could take a while and
                // shouldn't block this connection's own receive loop.
                println!("[{role}] Received a file-available beat, pulling now.");
                let cfg = cfg.clone();
                let shared_for_pull = shared.clone();
                // Reuse this already-live connection's peer address (IP,
                // and scope ID if IPv6 link-local — see with_port) rather
                // than re-resolving cfg.windows_ip's hostname independently
                // — a fresh resolution can non-deterministically pick a
                // different address (confirmed live: it picked an IPv6
                // link-local address once, which Windows' clipboard-server
                // rejected outright as "Unknown" since it only has this
                // machine's IPv4 address on file).
                let peer_addr = stream.peer_addr().ok();
                std::thread::spawn(move || {
                    if let Err(e) = pull_file_from_windows(&cfg, peer_addr, &shared_for_pull) {
                        eprintln!("(clipboard: file pull failed: {e})");
                    }
                });
            }
            PACKAGE_TYPE_HI => {
                hi_count += 1;
                if hi_count % 500 == 1 {
                    println!("[{role}] (Hi liveness pings received so far: {hi_count})");
                }
            }
            PACKAGE_TYPE_HELLO | PACKAGE_TYPE_BYEBYE | PACKAGE_TYPE_HEARTBEAT | PACKAGE_TYPE_MACHINE_SWITCHED => {}
            _ => {}
        }
    }
}

/// Runs the priming-block dance that starts every MWB connection (message
/// server or clipboard server alike) — shared so the two clipboard-server
/// functions below don't duplicate `run_session`'s own copy of this.
fn prime_connection(stream: &mut TcpStream, cipher: &CbcState, write_chain: &mut [u8; 16], read_chain: &mut [u8; 16]) -> std::io::Result<()> {
    let mut priming_out = [0u8; 16];
    rand::rng().fill(&mut priming_out);
    let ct = cipher.encrypt(write_chain, &priming_out);
    stream.write_all(&ct)?;

    let mut priming_in_ct = [0u8; 16];
    stream.read_exact(&mut priming_in_ct)?;
    let _ = cipher.decrypt(read_chain, &priming_in_ct);
    Ok(())
}

/// Connects to Windows' *separate* clipboard-server socket (port 15100) to
/// pull a file it just announced via a beat on the message server, and
/// applies it to the local clipboard once fully received. See PROTOCOL.md's
/// clipboard-sync section for the wire format this implements.
fn pull_file_from_windows(cfg: &Config, peer_addr: Option<std::net::SocketAddr>, shared: &ClipboardShared) -> std::io::Result<()> {
    let mut stream = match peer_addr {
        Some(addr) => {
            let addr = with_port(addr, FILE_LISTEN_PORT);
            println!("[file-pull] Connecting to {addr} (same address as the live message-server connection)...");
            TcpStream::connect(addr)?
        }
        None => {
            // Fallback if we somehow couldn't read the live connection's
            // peer address — re-resolves the configured host, which risks
            // the IPv4-vs-IPv6 mismatch documented above.
            let addr = format!("{}:{FILE_LISTEN_PORT}", windows_clipboard_host(&cfg.windows_ip));
            println!("[file-pull] Connecting to {addr} (fallback: re-resolved host)...");
            TcpStream::connect(&addr)?
        }
    };
    set_socket_timeouts(&stream);

    let cipher = CbcState::new(&derive_key(&cfg.security_key));
    let mut read_chain = derive_iv();
    let mut write_chain = derive_iv();
    prime_connection(&mut stream, &cipher, &mut write_chain, &mut read_chain)?;
    println!("[file-pull] Primed.");

    // We're pulling/receiving on this connection, so our handshake type is
    // Clipboard (not ClipboardPush — that's Windows' side, since it's the
    // one about to push the actual bytes). Unlike the message-server's
    // Handshake/HandshakeAck, this handshake has no checksum/magic-number
    // scheme at all — `Type` marshals as a full 4-byte little-endian int on
    // the C# side (confirmed: `Id` sits at FieldOffset(sizeof(PackageType))
    // == 4), so calling `finalize_send_buf` here would write checksum bytes
    // into what Windows reads as the upper 24 bits of the *same* Type
    // field, corrupting it into a value matching neither Clipboard nor
    // ClipboardPush — confirmed live: this is exactly what was producing
    // Windows' "Clipboard connection rejected: Unknown" toast. Send the
    // raw handshake bytes unmodified.
    let hs = build_clipboard_handshake(PACKAGE_TYPE_CLIPBOARD, cfg.machine_id, &cfg.machine_name);
    stream.write_all(&cipher.encrypt(&mut write_chain, &hs))?;
    println!("[file-pull] Sent our handshake, waiting for peer's...");

    let mut peer_hs_ct = [0u8; PACKAGE_SIZE_EX];
    stream.read_exact(&mut peer_hs_ct)?;
    let peer_hs = cipher.decrypt(&mut read_chain, &peer_hs_ct);
    println!("[file-pull] Peer handshake: type={} src={} name={:?}", peer_hs[0], unpack_u32_le(&peer_hs, 8), String::from_utf8_lossy(&peer_hs[32..64]).trim_end());

    let mut header_ct = [0u8; CLIPBOARD_FILE_HEADER_SIZE];
    stream.read_exact(&mut header_ct)?;
    println!("[file-pull] Read 1024-byte header.");
    let header_pt = cipher.decrypt(&mut read_chain, &header_ct);
    let header_arr: [u8; CLIPBOARD_FILE_HEADER_SIZE] = header_pt.try_into().unwrap();
    let Some((size, name)) = parse_file_header(&header_arr) else {
        eprintln!("[file-pull] Couldn't parse the file header, aborting.");
        return Ok(());
    };
    // size==0 (real MWB stuffs an English error message into `name` on
    // failure, e.g. an unsupported folder) and the 100MB cap are both
    // "not a real file" cases, not something to write to disk.
    if size == 0 || size > MAX_CLIPBOARD_FILE_SIZE {
        eprintln!("[file-pull] Rejected: size={size} name={name:?} (likely a sender-side error, not a real file).");
        return Ok(());
    }

    let body = read_padded_body(&mut stream, &cipher, &mut read_chain, size)?;

    if name == BIG_PATH_IMAGE_NAME {
        apply_incoming_clipboard_image(&body, &shared.last_applied_image);
        println!("[file-pull] Received a big clipboard image ({size} bytes) from Windows.");
        return Ok(());
    }

    let safe_name = windows_basename(&name);
    let dir = clipboard_files_dir();
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(&safe_name);
    std::fs::write(&dest, &body)?; // fully flushed before the clipboard is touched below

    apply_incoming_clipboard_file(&dest, &shared.last_applied_file_uri);
    println!("[file-pull] Received {safe_name:?} ({size} bytes) from Windows -> {}", dest.display());
    Ok(())
}

/// Serves whichever file is currently pending (see `ClipboardShared`) to a
/// peer that just connected to our clipboard-server listener (port 15100)
/// to pull it — the mirror image of `pull_file_from_windows`. Whether real
/// MWB actually initiates this connection at all is genuinely uncertain
/// (see PROTOCOL.md): its own auto-pull is gated on a "machine switched"
/// event this bridge's topology doesn't have an equivalent of.
fn serve_file_to_peer(mut stream: TcpStream, cfg: &Config, pending: &Arc<Mutex<Option<PendingOutbound>>>) -> std::io::Result<()> {
    let cipher = CbcState::new(&derive_key(&cfg.security_key));
    let mut read_chain = derive_iv();
    let mut write_chain = derive_iv();
    prime_connection(&mut stream, &cipher, &mut write_chain, &mut read_chain)?;

    // We're about to push the actual bytes, so our handshake type is
    // ClipboardPush. No finalize_send_buf here — see the comment in
    // pull_file_from_windows on why this handshake has no checksum scheme.
    let hs = build_clipboard_handshake(PACKAGE_TYPE_CLIPBOARD_PUSH, cfg.machine_id, &cfg.machine_name);
    stream.write_all(&cipher.encrypt(&mut write_chain, &hs))?;

    let mut peer_hs_ct = [0u8; PACKAGE_SIZE_EX];
    stream.read_exact(&mut peer_hs_ct)?;
    let _peer_hs = cipher.decrypt(&mut read_chain, &peer_hs_ct);

    let current = pending.lock().unwrap().clone();
    let Some(outbound) = current else {
        println!("[file-serve] Peer connected but nothing is currently pending — sending an empty/error header.");
        let header = build_file_header(0, "Nothing currently available");
        stream.write_all(&cipher.encrypt(&mut write_chain, &header))?;
        return Ok(());
    };

    let (path, name) = match &outbound {
        PendingOutbound::File(path) => {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".to_string());
            (path.clone(), name)
        }
        PendingOutbound::Image(path) => (path.clone(), BIG_PATH_IMAGE_NAME.to_string()),
    };
    let data = std::fs::read(&path)?;
    let header = build_file_header(data.len(), &name);
    stream.write_all(&cipher.encrypt(&mut write_chain, &header))?;
    write_padded_body(&mut stream, &cipher, &mut write_chain, &data)?;
    println!("[file-serve] Sent {name:?} ({} bytes) to peer.", data.len());
    Ok(())
}

/// Listens on the clipboard-server port (15100) for Windows connecting in
/// to pull whatever's pending (a file or a big clipboard image). Mirrors
/// `run_server_listener`'s shape.
fn run_file_server_listener(cfg: Config, pending: Arc<Mutex<Option<PendingOutbound>>>) {
    let listener = match TcpListener::bind(("0.0.0.0", FILE_LISTEN_PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[file-serve] Failed to bind :{FILE_LISTEN_PORT}: {e}");
            return;
        }
    };
    println!("[file-serve] Listening on :{FILE_LISTEN_PORT} for file pulls.");

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                println!("[file-serve] Accepted connection from {:?}", stream.peer_addr());
                set_socket_timeouts(&stream);
                let cfg = cfg.clone();
                let pending = pending.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve_file_to_peer(stream, &cfg, &pending) {
                        eprintln!("[file-serve] Session ended: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[file-serve] Accept error: {e}"),
        }
    }
}

/// Outbound leg: connects to Windows' message server and injects decoded
/// Mouse/Keyboard into Wayland. This is the connection real input forwarding
/// (and outbound clipboard sync) runs over.
fn run_client(
    cfg: &Config,
    wl: &mut WaylandInput,
    clipboard_rx: &Receiver<ClipboardEvent>,
    shared: &ClipboardShared,
) -> std::io::Result<()> {
    println!("Connecting to {}...", cfg.windows_ip);
    config::write_status(false, "", "Connecting...");
    let stream = TcpStream::connect(&cfg.windows_ip)?;
    set_socket_timeouts(&stream);
    run_session(stream, Some(wl), "client", cfg, Some(clipboard_rx), shared)
}

/// Inbound leg: accepts Windows' own outbound connection back to us (the
/// reverse half of the pair it expects between two machines). Runs forever
/// in its own thread; each accepted connection gets its own thread since
/// nothing here touches the single-threaded Wayland event queue.
fn run_server_listener(cfg: Config, shared: ClipboardShared) {
    let listener = match TcpListener::bind(("0.0.0.0", LISTEN_PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[server] Failed to bind :{LISTEN_PORT} for the reverse connection: {e}");
            return;
        }
    };
    println!("[server] Listening on :{LISTEN_PORT} for Windows' reverse connection.");

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                println!("[server] Accepted connection from {:?}", stream.peer_addr());
                set_socket_timeouts(&stream);
                let cfg = cfg.clone();
                let shared = shared.clone();
                std::thread::spawn(move || {
                    if let Err(e) = run_session(stream, None, "server", &cfg, None, &shared) {
                        eprintln!("[server] Session ended: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[server] Accept error: {e}"),
        }
    }
}

fn main() {
    let cfg = loop {
        if let Some(cfg) = config::load_config() {
            break cfg;
        }
        config::write_status(
            false,
            "",
            "Not configured — set Security Key, Windows IP, and Machine Name from the Omarchy bar widget, then restart.",
        );
        eprintln!("No valid config at ~/.local/share/omarchy-mwb-bridge/config.json yet — waiting...");
        std::thread::sleep(Duration::from_secs(5));
    };

    println!(
        "mwb-omarchy-bridge daemon starting. Creating Wayland virtual input devices (XKB layout {:?} variant {:?})...",
        cfg.xkb_layout, cfg.xkb_variant
    );
    let mut wl = WaylandInput::new(&cfg.xkb_layout, &cfg.xkb_variant);
    println!("Wayland input ready. Entering connect/reconnect loop.");

    let clipboard_shared = ClipboardShared::new();
    let (clip_tx, clip_rx) = mpsc::channel::<ClipboardEvent>();

    {
        let cfg = cfg.clone();
        let shared = clipboard_shared.clone();
        std::thread::spawn(move || run_server_listener(cfg, shared));
    }
    {
        let cfg = cfg.clone();
        let pending = clipboard_shared.pending_outbound.clone();
        std::thread::spawn(move || run_file_server_listener(cfg, pending));
    }
    {
        let shared = clipboard_shared.clone();
        std::thread::spawn(move || run_clipboard_watcher(clip_tx, shared));
    }

    loop {
        if let Err(e) = run_client(&cfg, &mut wl, &clip_rx, &clipboard_shared) {
            eprintln!("Connection ended: {e}. Reconnecting in 3s...");
            config::write_status(false, "", &format!("Disconnected: {e}. Reconnecting..."));
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}
