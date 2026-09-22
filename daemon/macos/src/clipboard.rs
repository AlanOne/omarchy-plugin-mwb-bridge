// Windows <-> macOS clipboard TEXT sync, small-path only (payloads under
// MAX_SMALL_PATH_SIZE — see mwb_protocol::mwb_protocol for the size split
// and PROTOCOL.md for the wire format). Mirrors the Linux build's very
// first clipboard milestone (text-only, before images/files/big-path were
// added) — same incremental scope here; the packet-building/parsing side is
// the same shared `mwb_protocol` code, only clipboard *access* differs.
//
// Uses `arboard` (NSPasteboard under the hood) rather than shelling out to
// pbcopy/pbpaste the way the Linux build pipes through wl-copy/wl-paste:
// arboard gives a typed Rust API without spawning a process per read/write.

use std::io::Read;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arboard::Clipboard;
use flate2::read::DeflateDecoder;
use mwb_protocol::mwb_protocol::CLIPBOARD_SEP;

pub enum ClipboardEvent {
    Text(String),
}

/// Shared echo-prevention state between the client connection's receive
/// loop (which applies Windows' clipboard here) and the watcher thread
/// (which polls this machine's clipboard for outbound changes) — mirrors
/// the Linux build's `ClipboardShared`, minus the file/image fields this
/// milestone doesn't implement yet.
#[derive(Clone)]
pub struct ClipboardShared {
    last_applied_text: Arc<Mutex<Option<String>>>,
}

impl ClipboardShared {
    pub fn new() -> Self {
        Self { last_applied_text: Arc::new(Mutex::new(None)) }
    }
}

impl Default for ClipboardShared {
    fn default() -> Self {
        Self::new()
    }
}

/// Applies incoming ClipboardText bytes (already-accumulated small-path
/// chunks) to the local clipboard: raw-DEFLATE-inflate, UTF-16LE-decode,
/// strip the `"TXT" + text + SEP` wrapper real MWB sends — see
/// PROTOCOL.md's clipboard section for the exact recipe (originally
/// reverse-engineered for the Linux build, unchanged here since this is
/// Windows' own wire format, not anything OS-specific on the receiving
/// end). Records what was applied so the outbound watcher can recognize
/// its own echo and not immediately bounce it right back to Windows.
pub fn apply_incoming_clipboard_text(compressed: &[u8], shared: &ClipboardShared) {
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

    match Clipboard::new().and_then(|mut cb| cb.set_text(text.to_string())) {
        Ok(()) => {
            *shared.last_applied_text.lock().unwrap() = Some(text.to_string());
            println!("(clipboard: applied {} chars from Windows)", text.chars().count());
        }
        Err(e) => eprintln!("(clipboard: failed to set local clipboard: {e})"),
    }
}

/// macOS -> Windows clipboard sync: polls this machine's clipboard (no
/// simple blocking "notify me on change" primitive here either, same as
/// `wl-clipboard`'s CLI tools on the Linux side, so polling is again the
/// simple option) and forwards genuinely new content into `clip_tx` for
/// whichever connection is currently live to pick up and send. Seeds its
/// baseline from whatever's already on the clipboard at startup without
/// sending it — only actual *changes* get synced.
pub fn run_clipboard_watcher(clip_tx: Sender<ClipboardEvent>, shared: ClipboardShared) {
    let mut clipboard = match Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("(clipboard: failed to open the local clipboard, outbound sync disabled: {e})");
            return;
        }
    };
    let mut last_seen_text = clipboard.get_text().ok();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let Ok(current) = clipboard.get_text() else { continue };
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
