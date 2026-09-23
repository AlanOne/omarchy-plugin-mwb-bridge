// Windows <-> macOS clipboard sync: text and images, both small- and
// big-path (mirroring the Linux build's "Clipboard images" milestone).
// Real MWB applies the same ~1MB small/big-path threshold to images as
// text, and real screenshots routinely exceed it — small-path-only would
// silently fail for most actual screenshots, so both paths are ported
// together rather than incrementally like text was. Plain file copy/paste
// (arbitrary files, not images) is *not* ported — a separate, later
// milestone on the Linux build too, not attempted here.
//
// Uses `arboard` (NSPasteboard) for clipboard access and the `image` crate
// for PNG<->RGBA conversion — real MWB's own wire payload for both text and
// images is exactly what the Linux build already worked out (raw-DEFLATE-
// wrapped UTF-16LE text; a plain, uncompressed PNG file for images, no
// wrapper at all), all in the shared `mwb_protocol` crate already; only
// clipboard *access* and codec conversion are new macOS-specific code.

use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arboard::{Clipboard, ImageData};
use flate2::read::DeflateDecoder;
use image::ExtendedColorType;
use image::codecs::png::PngEncoder;
use image::ImageEncoder;
use mwb_protocol::config::Config;
use mwb_protocol::mwb_protocol::*;

// The separate "clipboard server" port (BASE_PORT) that big-path bytes
// travel over — a distinct TCP connection from the message server, opened
// on demand in whichever direction a big-path transfer is actually
// happening. Same port Linux's daemon.rs uses.
pub const FILE_LISTEN_PORT: u16 = 15100;

pub enum ClipboardEvent {
    Text(String),
    Image(Vec<u8>),    // PNG bytes, small-path (<1MB)
    BigImage(PathBuf),  // PNG bytes already written to a temp file, big-path
}

/// Shared state between the client connection's receive loop, the file-
/// server listener (port 15100), and the local clipboard-watcher thread —
/// mirrors the Linux build's `ClipboardShared`, minus the file-transfer
/// fields that milestone doesn't apply to macOS (no plain file copy/paste
/// here).
#[derive(Clone)]
pub struct ClipboardShared {
    last_applied_text: Arc<Mutex<Option<String>>>,
    // Decoded RGBA pixels, *not* PNG container bytes — see
    // `apply_incoming_clipboard_image`'s doc comment for why the comparison
    // has to happen on pixels rather than the outer PNG encoding.
    last_applied_image_rgba: Arc<Mutex<Option<(usize, usize, Vec<u8>)>>>,
    pending_outbound_image: Arc<Mutex<Option<PathBuf>>>,
    // Windows' own machine ID, learned from the Src field of anything it's
    // sent us — needed to address a MachineSwitched package directly to it
    // (see `announce_big_image`).
    peer_machine_id: Arc<Mutex<Option<u32>>>,
}

impl ClipboardShared {
    pub fn new() -> Self {
        Self {
            last_applied_text: Arc::new(Mutex::new(None)),
            last_applied_image_rgba: Arc::new(Mutex::new(None)),
            pending_outbound_image: Arc::new(Mutex::new(None)),
            peer_machine_id: Arc::new(Mutex::new(None)),
        }
    }

    /// Called from the client connection's receive loop for every package
    /// carrying a `Src`, so a later big-path announcement can address
    /// Windows directly (see `announce_big_image`'s doc comment on why).
    pub fn note_peer_machine_id(&self, src_id: u32) {
        if src_id != 0 && src_id != ID_ALL {
            *self.peer_machine_id.lock().unwrap() = Some(src_id);
        }
    }
}

impl Default for ClipboardShared {
    fn default() -> Self {
        Self::new()
    }
}

fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join("Library/Caches/mwb-mac-bridge")
}

/// Applies incoming ClipboardText bytes (already-accumulated small-path
/// chunks) to the local clipboard: raw-DEFLATE-inflate, UTF-16LE-decode,
/// strip the `"TXT" + text + SEP` wrapper real MWB sends — see
/// PROTOCOL.md's clipboard section. Records what was applied so the
/// outbound watcher can recognize its own echo and not immediately bounce
/// it right back to Windows.
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

/// Applies received clipboard-image bytes directly to the local clipboard.
/// Real MWB's wire payload for `ClipboardImage` is a plain PNG file
/// verbatim, no wrapper/compression (confirmed on the Linux build from
/// source: `image.Save(stream, ImageFormat.Png)`) — decoded here via the
/// `image` crate into the raw RGBA bytes `arboard`'s `set_image` wants
/// (unlike `wl-copy`, which took the PNG bytes directly — NSPasteboard's
/// Rust API here works in decoded pixels, not an arbitrary MIME payload).
///
/// Records the *decoded pixels*, not the PNG bytes, as what was applied —
/// real bug found live, 2026-09-23: `arboard`'s `get_image()` only ever
/// returns decoded RGBA, never the original PNG container, so the outbound
/// watcher re-encodes it from scratch every poll. That re-encoding is not
/// byte-identical to Windows' own PNG encoder even for pixel-identical
/// images (different compression/filter choices) — comparing PNG bytes for
/// echo-detection therefore never matched, and every incoming image
/// immediately bounced straight back to Windows as if it were a fresh local
/// change. Comparing decoded pixels instead is encoder-independent and
/// fixes this.
pub fn apply_incoming_clipboard_image(png_bytes: &[u8], shared: &ClipboardShared) {
    let rgba = match image::load_from_memory(png_bytes) {
        Ok(img) => img.to_rgba8(),
        Err(e) => {
            eprintln!("(clipboard: failed to decode incoming PNG, ignoring: {e})");
            return;
        }
    };
    let (width, height) = (rgba.width() as usize, rgba.height() as usize);
    let raw = rgba.into_raw();
    let image_data = ImageData { width, height, bytes: raw.clone().into() };
    match Clipboard::new().and_then(|mut cb| cb.set_image(image_data)) {
        Ok(()) => {
            *shared.last_applied_image_rgba.lock().unwrap() = Some((width, height, raw));
            println!("(clipboard: applied a {}-byte image from Windows)", png_bytes.len());
        }
        Err(e) => eprintln!("(clipboard: failed to set local clipboard image: {e})"),
    }
}

/// Reads the local clipboard's current image, if any, as decoded RGBA
/// pixels — the comparison unit for change/echo detection (see
/// `apply_incoming_clipboard_image`'s doc comment for why PNG bytes don't
/// work for this). Encoded to PNG only once actually decided to send, in
/// `run_clipboard_watcher`.
fn read_local_clipboard_image_rgba() -> Option<(usize, usize, Vec<u8>)> {
    let image_data = Clipboard::new().ok()?.get_image().ok()?;
    Some((image_data.width, image_data.height, image_data.bytes.into_owned()))
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut png_bytes = Vec::new();
    PngEncoder::new(&mut png_bytes).write_image(rgba, width as u32, height as u32, ExtendedColorType::Rgba8).ok()?;
    Some(png_bytes)
}

/// macOS -> Windows clipboard sync: polls the local clipboard (no simple
/// blocking "notify me on change" primitive here either, same as
/// `wl-clipboard`'s CLI tools on Linux) and forwards genuinely new content
/// into `clip_tx`. Checks for an image first each tick (matching the Linux
/// build's file > image > text priority, minus the file case this
/// milestone doesn't implement), falling back to text.
pub fn run_clipboard_watcher(clip_tx: Sender<ClipboardEvent>, shared: ClipboardShared) {
    let mut last_seen_image = read_local_clipboard_image_rgba();
    let mut last_seen_text = Clipboard::new().ok().and_then(|mut cb| cb.get_text().ok());
    loop {
        std::thread::sleep(Duration::from_millis(500));

        let current_image = read_local_clipboard_image_rgba();
        if let Some((width, height, ref raw)) = current_image {
            if last_seen_image != current_image {
                let is_echo = shared.last_applied_image_rgba.lock().unwrap().as_ref() == current_image.as_ref();
                last_seen_image = current_image.clone();
                if !is_echo {
                    if let Some(png_bytes) = encode_png(width, height, raw) {
                        if png_bytes.len() < MAX_SMALL_PATH_SIZE {
                            let _ = clip_tx.send(ClipboardEvent::Image(png_bytes));
                        } else {
                            let dir = cache_dir();
                            if std::fs::create_dir_all(&dir).is_ok() {
                                let path = dir.join("outbound-image.png");
                                if std::fs::write(&path, &png_bytes).is_ok() {
                                    let _ = clip_tx.send(ClipboardEvent::BigImage(path));
                                }
                            }
                        }
                    }
                }
            }
            continue; // an image is present — don't also fall through to text
        }

        let Ok(mut cb) = Clipboard::new() else { continue };
        let Ok(current) = cb.get_text() else { continue };
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

/// Announces big-path image content to the peer: the `Clipboard` beat,
/// then — if we've learned the peer's machine ID from something it's sent
/// us already — a follow-up `MachineSwitched` addressed directly to it.
/// Real MWB's own auto-pull is gated on `MachineSwitched`, not the beat
/// alone (see PROTOCOL.md); without this follow-up, a real PowerToys
/// install never actually connects to fetch what's been announced. Mirrors
/// the Linux build's `announce_big_path` exactly (same protocol, same
/// gotchas — see its doc comment there for the fuller rationale).
pub fn announce_big_image(
    stream: &mut TcpStream,
    cipher: &CbcState,
    write_chain: &mut [u8; 16],
    magic_number: u32,
    src_id: u32,
    peer_machine_id: Option<u32>,
) -> std::io::Result<()> {
    use std::io::Write;
    use rand::RngExt;
    let mut beat = build_clipboard_beat(rand::rng().random(), src_id);
    finalize_send_buf(&mut beat, magic_number);
    stream.write_all(&cipher.encrypt(write_chain, &beat))?;

    if let Some(des_id) = peer_machine_id {
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

fn prime_connection(stream: &mut TcpStream, cipher: &CbcState, write_chain: &mut [u8; 16], read_chain: &mut [u8; 16]) -> std::io::Result<()> {
    use std::io::Write;
    use rand::RngExt;
    let mut priming_out = [0u8; 16];
    rand::rng().fill(&mut priming_out);
    let ct = cipher.encrypt(write_chain, &priming_out);
    stream.write_all(&ct)?;

    let mut priming_in_ct = [0u8; 16];
    stream.read_exact(&mut priming_in_ct)?;
    let _ = cipher.decrypt(read_chain, &priming_in_ct);
    Ok(())
}

/// Returns `addr` with its port changed, preserving an IPv6 address's scope
/// ID/flow info if present — see the Linux build's identical helper for why
/// this matters (a plain `SocketAddr::new` silently drops a link-local
/// IPv6 address's scope ID, breaking the connection).
fn with_port(addr: SocketAddr, port: u16) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) => SocketAddr::new((*v4.ip()).into(), port),
        SocketAddr::V6(v6) => SocketAddr::V6(std::net::SocketAddrV6::new(*v6.ip(), port, v6.flowinfo(), v6.scope_id())),
    }
}

fn read_padded_body(stream: &mut TcpStream, cipher: &CbcState, chain: &mut [u8; 16], size: usize) -> std::io::Result<Vec<u8>> {
    const CHUNK: usize = 1 << 20;
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

fn write_padded_body(stream: &mut TcpStream, cipher: &CbcState, chain: &mut [u8; 16], data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    const CHUNK: usize = 1 << 20;
    let padded_len = data.len().div_ceil(32) * 32;
    let mut padded = vec![0u8; padded_len];
    padded[..data.len()].copy_from_slice(data);
    for chunk in padded.chunks(CHUNK) {
        stream.write_all(&cipher.encrypt(chain, chunk))?;
    }
    Ok(())
}

/// Connects to Windows' separate clipboard-server socket (port 15100) to
/// pull a big-path image it just announced via a beat on the message
/// server, and applies it to the local clipboard once fully received.
/// Mirrors the Linux build's `pull_file_from_windows`, narrowed to the
/// image case only (this milestone doesn't implement plain file
/// copy/paste) — a non-image payload is logged and discarded rather than
/// silently mishandled.
pub fn pull_image_from_windows(cfg: &Config, peer_addr: Option<SocketAddr>, shared: &ClipboardShared) -> std::io::Result<()> {
    use std::io::Write;
    let mut stream = match peer_addr {
        Some(addr) => TcpStream::connect(with_port(addr, FILE_LISTEN_PORT))?,
        None => {
            let host = cfg.windows_ip.rsplit_once(':').map(|(h, _)| h).unwrap_or(&cfg.windows_ip);
            TcpStream::connect((host, FILE_LISTEN_PORT))?
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(300)));

    let cipher = CbcState::new(&derive_key(&cfg.security_key));
    let mut read_chain = derive_iv();
    let mut write_chain = derive_iv();
    prime_connection(&mut stream, &cipher, &mut write_chain, &mut read_chain)?;

    // Pulling/receiving, so our handshake type is Clipboard (not
    // ClipboardPush — that's Windows' side, since it's about to push the
    // actual bytes). No finalize_send_buf here — this handshake has no
    // checksum/magic-number scheme, unlike the message server's.
    let hs = build_clipboard_handshake(PACKAGE_TYPE_CLIPBOARD, cfg.machine_id, &cfg.machine_name);
    stream.write_all(&cipher.encrypt(&mut write_chain, &hs))?;

    let mut peer_hs_ct = [0u8; PACKAGE_SIZE_EX];
    stream.read_exact(&mut peer_hs_ct)?;
    let _peer_hs = cipher.decrypt(&mut read_chain, &peer_hs_ct);

    let mut header_ct = [0u8; CLIPBOARD_FILE_HEADER_SIZE];
    stream.read_exact(&mut header_ct)?;
    let header_pt = cipher.decrypt(&mut read_chain, &header_ct);
    let header_arr: [u8; CLIPBOARD_FILE_HEADER_SIZE] = header_pt.try_into().unwrap();
    let Some((size, name)) = parse_file_header(&header_arr) else {
        eprintln!("[image-pull] Couldn't parse the file header, aborting.");
        return Ok(());
    };
    if size == 0 || size > MAX_CLIPBOARD_FILE_SIZE {
        eprintln!("[image-pull] Rejected: size={size} name={name:?} (likely a sender-side error, not a real payload).");
        return Ok(());
    }

    let body = read_padded_body(&mut stream, &cipher, &mut read_chain, size)?;

    if name == BIG_PATH_IMAGE_NAME {
        apply_incoming_clipboard_image(&body, shared);
        println!("[image-pull] Received a big clipboard image ({size} bytes) from Windows.");
    } else {
        println!("[image-pull] Windows offered {name:?} ({size} bytes) — not an image, and plain file transfer isn't supported on macOS yet, discarding.");
    }
    Ok(())
}

/// Serves the pending big-path image to a peer that just connected to our
/// clipboard-server listener (port 15100) to pull it — mirrors the Linux
/// build's `serve_file_to_peer`, image-only.
fn serve_pending_image(mut stream: TcpStream, cfg: &Config, pending: &Arc<Mutex<Option<PathBuf>>>) -> std::io::Result<()> {
    use std::io::Write;
    let cipher = CbcState::new(&derive_key(&cfg.security_key));
    let mut read_chain = derive_iv();
    let mut write_chain = derive_iv();
    prime_connection(&mut stream, &cipher, &mut write_chain, &mut read_chain)?;

    let hs = build_clipboard_handshake(PACKAGE_TYPE_CLIPBOARD_PUSH, cfg.machine_id, &cfg.machine_name);
    stream.write_all(&cipher.encrypt(&mut write_chain, &hs))?;

    let mut peer_hs_ct = [0u8; PACKAGE_SIZE_EX];
    stream.read_exact(&mut peer_hs_ct)?;
    let _peer_hs = cipher.decrypt(&mut read_chain, &peer_hs_ct);

    let current = pending.lock().unwrap().clone();
    let Some(path) = current else {
        let header = build_file_header(0, "Nothing currently available");
        stream.write_all(&cipher.encrypt(&mut write_chain, &header))?;
        return Ok(());
    };

    let data = std::fs::read(&path)?;
    let header = build_file_header(data.len(), BIG_PATH_IMAGE_NAME);
    stream.write_all(&cipher.encrypt(&mut write_chain, &header))?;
    write_padded_body(&mut stream, &cipher, &mut write_chain, &data)?;
    println!("[image-serve] Sent a {}-byte image to peer.", data.len());
    Ok(())
}

/// Listens on the clipboard-server port for Windows connecting in to pull
/// a pending big-path image. Mirrors the Linux build's
/// `run_file_server_listener`.
pub fn run_file_server_listener(cfg: Config, pending: Arc<Mutex<Option<PathBuf>>>) {
    let listener = match TcpListener::bind(("0.0.0.0", FILE_LISTEN_PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[image-serve] Failed to bind :{FILE_LISTEN_PORT}: {e}");
            return;
        }
    };
    println!("[image-serve] Listening on :{FILE_LISTEN_PORT} for big-path image pulls.");

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(300)));
                let cfg = cfg.clone();
                let pending = pending.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve_pending_image(stream, &cfg, &pending) {
                        eprintln!("[image-serve] Session ended: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[image-serve] Accept error: {e}"),
        }
    }
}

/// Exposes the shared pending-outbound-image slot to `main.rs`'s send loop
/// (set when a `BigImage` event arrives) and the file-server listener
/// (read when Windows connects to pull it).
pub fn pending_outbound_image(shared: &ClipboardShared) -> Arc<Mutex<Option<PathBuf>>> {
    shared.pending_outbound_image.clone()
}

pub fn peer_machine_id(shared: &ClipboardShared) -> Option<u32> {
    *shared.peer_machine_id.lock().unwrap()
}

