// macOS port of the mwb-omarchy-bridge daemon. Everything protocol-level
// (crypto, framing, handshake, VK->evdev translation, mouse/keyboard
// decoding) is unchanged, shared code from `mwb_protocol` — this file is
// mostly platform wiring: the TCP connect/reconnect loop and a status-item
// menu bar app. Mouse/keyboard forwarding, clipboard sync (text, and images
// both small- and big-path), and lock-both-machines are ported; plain file
// transfer and suspend/resume handling are deliberately not yet — see the
// mwb-omarchy-bridge project memory's "macOS port" section for the full
// remaining list, being added incrementally the same way the Linux build was.

mod cg_input;
mod clipboard;
mod keycode_macos;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use mwb_protocol::config::{self, Config};
use mwb_protocol::input_handling::{handle_keyboard, handle_mouse, KeyboardState, LockComboDetector, LLKHF_UP};
use mwb_protocol::mwb_protocol::*;
use rand::RngExt;

use cg_input::CgInput;
use clipboard::{ClipboardEvent, ClipboardShared};

// Same 5-minute idle-tolerant timeout the Linux build settled on after
// finding both failure modes live (a no-timeout 3+ hour hang, then an
// over-aggressive 30s timeout that false-triggered during normal idle
// gaps) — see the mwb-omarchy-bridge project memory.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(300);

// Same message-server port MWB uses on every machine. Windows expects a
// symmetric pair of sockets (one per direction) between two paired
// machines — live-tested against the real Windows PC on 2026-09-22:
// without accepting this reverse leg, the handshake completed and matched,
// but Windows then sent *nothing* further at all afterward (not even
// periodic Hi liveness pings), for a brand-new machine being paired for
// the first time. The Linux build calls this leg "cosmetic only," but that
// finding was on an already-established pairing — a first-time pairing
// appears to need it. See the mwb-omarchy-bridge project memory.
const LISTEN_PORT: u16 = 15101;

fn set_socket_timeouts(stream: &TcpStream) {
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
}

/// Handshake + Mouse/Keyboard/ClipboardText receive loop over an
/// already-connected stream. `cg` is `Some` on the outbound client
/// connection (real forwarding runs there) and `None` on the inbound
/// reverse-listener side, which only needs to complete the protocol so
/// Windows sees a healthy pair — mirrors the Linux build's `role`/
/// `wl: Option<&mut WaylandInput>` split. `clipboard` is only `Some` on the
/// client role too, same reasoning as the Linux build: outbound clipboard
/// sync happens over the one connection real forwarding already runs over.
/// Deliberately narrower than the Linux build otherwise: no image
/// clipboard/lock/file handling yet. Returns (via `?`) on any I/O error so
/// the caller reconnects/re-accepts.
fn run_session(
    stream: &mut TcpStream,
    mut cg: Option<&mut CgInput>,
    cfg: &Config,
    clipboard: Option<(&Receiver<ClipboardEvent>, &ClipboardShared)>,
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
    println!("[client] Handshake sent, entering receive loop.");

    let mut kb_state = KeyboardState::new();
    let mut lock_combo = LockComboDetector::new();
    let mut hi_count = 0u64;
    let mut clipboard_buf: Vec<u8> = Vec::new();
    let mut clipboard_kind: Option<u8> = None;
    // Arbitrary starting value distinct from the handshake's id=1 above —
    // just needs to be unique per outgoing package within this connection
    // (see build_clipboard_text_packages's doc comment on why).
    let mut next_clip_id: u32 = 1000;

    loop {
        if let Some((clip_rx, shared)) = clipboard.as_ref() {
            let mut latest = None;
            while let Ok(ev) = clip_rx.try_recv() {
                latest = Some(ev); // coalesce rapid successive changes to the last one
            }
            match latest {
                Some(ClipboardEvent::Text(text)) => {
                    for mut pkg in build_clipboard_text_packages(&mut next_clip_id, cfg.machine_id, &text) {
                        finalize_send_buf(&mut pkg, magic_number);
                        let ct = cipher.encrypt(&mut write_chain, &pkg);
                        stream.write_all(&ct)?;
                    }
                    println!("[client] Sent clipboard text ({} chars) to peer.", text.chars().count());
                }
                Some(ClipboardEvent::Image(png_bytes)) => {
                    let len = png_bytes.len();
                    for mut pkg in build_clipboard_image_packages(&mut next_clip_id, cfg.machine_id, &png_bytes) {
                        finalize_send_buf(&mut pkg, magic_number);
                        let ct = cipher.encrypt(&mut write_chain, &pkg);
                        stream.write_all(&ct)?;
                    }
                    println!("[client] Sent clipboard image ({len} bytes) to peer.");
                }
                Some(ClipboardEvent::BigImage(path)) => {
                    *clipboard::pending_outbound_image(shared).lock().unwrap() = Some(path);
                    clipboard::announce_big_image(
                        stream,
                        &cipher,
                        &mut write_chain,
                        magic_number,
                        cfg.machine_id,
                        clipboard::peer_machine_id(shared),
                    )?;
                    println!("[client] Announced a big clipboard image to peer (served if/when it connects to pull it).");
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
        if let Some((_, shared)) = clipboard.as_ref() {
            shared.note_peer_machine_id(src_id);
        }

        match package_type {
            PACKAGE_TYPE_HANDSHAKE => {
                println!("[client] Replying to peer's Handshake with HandshakeAck.");
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
                let matched =
                    m1 == our_flipped[0] && m2 == our_flipped[1] && m3 == our_flipped[2] && m4 == our_flipped[3];
                println!("[client] HandshakeAck received, challenge match: {matched}");
                let peer = String::from_utf8_lossy(&full[32..64]).trim_end().to_string();
                let detail = if matched { "Connected" } else { "Handshake failed — check the security key" };
                config::write_status(matched, &peer, detail);
            }
            PACKAGE_TYPE_MOUSE => {
                let x = unpack_u32_le(&full, 16);
                let y = unpack_u32_le(&full, 20);
                let wheel = unpack_u32_le(&full, 24);
                let flags = unpack_u32_le(&full, 28);
                if let Some(cg) = cg.as_deref_mut() {
                    handle_mouse(cg, x, y, wheel, flags, cfg.scroll_speed);
                }
            }
            PACKAGE_TYPE_KEYBOARD => {
                // Offsets verified empirically on the Linux build against
                // real traffic: wVk at 24, dwFlags at 28.
                let vk = unpack_u32_le(&full, 24);
                let flags = unpack_u32_le(&full, 28);
                if lock_combo.observe(vk, (flags & LLKHF_UP) == 0) {
                    println!("[client] Windows' lock-both-machines combo detected — locking this machine too.");
                    cg_input::lock_screen();
                }
                if let Some(cg) = cg.as_deref_mut() {
                    handle_keyboard(cg, &mut kb_state, vk, flags);
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
                if let Some((_, shared)) = clipboard.as_ref() {
                    match clipboard_kind {
                        Some(PACKAGE_TYPE_CLIPBOARD_TEXT) => clipboard::apply_incoming_clipboard_text(&clipboard_buf, shared),
                        Some(PACKAGE_TYPE_CLIPBOARD_IMAGE) => clipboard::apply_incoming_clipboard_image(&clipboard_buf, shared),
                        _ => {}
                    }
                }
                clipboard_buf.clear();
                clipboard_kind = None;
            }
            PACKAGE_TYPE_CLIPBOARD => {
                // The "beat" announcing big-path content is available.
                // Real MWB only auto-pulls this around its own
                // machine-switch event; this bridge's fixed two-machine
                // topology has no equivalent, so treat any beat as "pull
                // immediately" instead — see PROTOCOL.md. Spawned in its
                // own thread since a full pull over a fresh connection
                // could take a while and shouldn't block this connection's
                // own receive loop.
                if let Some((_, shared)) = clipboard.as_ref() {
                    println!("[client] Received a big-path-available beat, pulling now.");
                    let cfg = cfg.clone();
                    let shared = (*shared).clone();
                    let peer_addr = stream.peer_addr().ok();
                    std::thread::spawn(move || {
                        if let Err(e) = clipboard::pull_image_from_windows(&cfg, peer_addr, &shared) {
                            eprintln!("(clipboard: big-path pull failed: {e})");
                        }
                    });
                }
            }
            PACKAGE_TYPE_HI => {
                hi_count += 1;
                if hi_count % 500 == 1 {
                    println!("[client] (Hi liveness pings received so far: {hi_count})");
                }
            }
            PACKAGE_TYPE_HELLO | PACKAGE_TYPE_BYEBYE | PACKAGE_TYPE_HEARTBEAT => {}
            _ => {}
        }
    }
}

fn run_client(
    cfg: &Config,
    cg: &mut CgInput,
    clip_rx: &Receiver<ClipboardEvent>,
    clip_shared: &ClipboardShared,
) -> std::io::Result<()> {
    println!("Connecting to {}...", cfg.windows_ip);
    config::write_status(false, "", "Connecting...");
    let mut stream = TcpStream::connect(&cfg.windows_ip)?;
    set_socket_timeouts(&stream);
    run_session(&mut stream, Some(cg), cfg, Some((clip_rx, clip_shared)))
}

/// Accepts Windows' own reverse connection back to us — see `LISTEN_PORT`'s
/// doc comment for why this turned out to matter live. Runs forever in its
/// own thread; each accepted connection gets its own thread in turn.
fn run_server_listener(cfg: Config) {
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
            Ok(mut stream) => {
                println!("[server] Accepted connection from {:?}", stream.peer_addr());
                set_socket_timeouts(&stream);
                let cfg = cfg.clone();
                std::thread::spawn(move || {
                    if let Err(e) = run_session(&mut stream, None, &cfg, None) {
                        eprintln!("[server] Session ended: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[server] Accept error: {e}"),
        }
    }
}

fn network_thread(cfg: Config) {
    {
        let cfg = cfg.clone();
        std::thread::spawn(move || run_server_listener(cfg));
    }

    let clip_shared = ClipboardShared::new();
    let (clip_tx, clip_rx) = mpsc::channel::<ClipboardEvent>();
    {
        let clip_shared = clip_shared.clone();
        std::thread::spawn(move || clipboard::run_clipboard_watcher(clip_tx, clip_shared));
    }
    {
        let cfg = cfg.clone();
        let pending = clipboard::pending_outbound_image(&clip_shared);
        std::thread::spawn(move || clipboard::run_file_server_listener(cfg, pending));
    }

    // CGEventSource/CGEvent creation doesn't require the main thread —
    // unlike the menu bar UI, which tao/AppKit does require there — so this
    // runs entirely on its own background thread.
    let mut cg = CgInput::new();
    loop {
        if let Err(e) = run_client(&cfg, &mut cg, &clip_rx, &clip_shared) {
            eprintln!("Connection ended: {e}. Reconnecting in 3s...");
            config::write_status(false, "", &format!("Disconnected: {e}. Reconnecting..."));
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Writes a starter config.json (with an obviously-fake security key and
/// Windows host) if none exists yet, so "Edit Config..." always has
/// something to open rather than erroring on a missing file. Never
/// overwrites an existing file, even a malformed one — a parse failure
/// should surface as "fix your edit," not silently discard it.
fn ensure_config_template() {
    let path = config::config_path();
    if path.exists() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let template = r#"{
  "security_key": "CHANGE-ME-must-match-PowerToys-Settings-key",
  "windows_ip": "windows-pc.local:15101",
  "machine_name": "mac",
  "machine_id": 2,
  "scroll_speed": 1.0
}
"#;
    let _ = std::fs::write(&path, template);
}

fn main() {
    ensure_config_template();

    std::thread::spawn(|| {
        let cfg = loop {
            if let Some(cfg) = config::load_config() {
                break cfg;
            }
            config::write_status(
                false,
                "",
                "Not configured — edit config.json from the menu bar icon, then Restart Connection.",
            );
            std::thread::sleep(Duration::from_secs(5));
        };
        network_thread(cfg);
    });

    tray::run();
}

mod tray;
