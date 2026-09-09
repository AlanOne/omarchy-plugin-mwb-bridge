// The real persistent daemon: holds a long-lived connection to Mouse
// Without Borders' message server, reconnecting automatically on drop, and
// feeds decoded Mouse/Keyboard packets into the Wayland injector. See
// PROTOCOL.md for the wire protocol this implements.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

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
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
// LLKHF_UP: bit 7 of a low-level-keyboard-hook's flags marks a key-up event.
const LLKHF_UP: u32 = 0x80;

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
        WM_MOUSEMOVE => wl.move_absolute(m1, m2, 65535, 65535),
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

/// Runs one connection's lifecycle over an already-connected/accepted
/// stream: prime, handshake, then receive forever until the socket errors or
/// closes. Shared by both the outbound client connection (which injects
/// decoded Mouse/Keyboard into Wayland, `wl = Some(..)`) and the inbound
/// listener side (which only completes the protocol so Windows sees a
/// healthy reverse connection, `wl = None` — nothing it sends there needs
/// forwarding anywhere). Returns (normally via `?`) on any I/O error so the
/// caller can reconnect/re-accept.
fn run_session(mut stream: TcpStream, mut wl: Option<&mut WaylandInput>, role: &str, cfg: &Config) -> std::io::Result<()> {
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
    let mut hi_count = 0u64;

    loop {
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
                if let Some(w) = wl.as_deref_mut() {
                    println!(">>> KEYBOARD vk=0x{vk:x} flags=0x{flags:x}");
                    handle_keyboard(w, &mut mods, vk, flags);
                }
            }
            PACKAGE_TYPE_HI => {
                hi_count += 1;
                if hi_count % 500 == 1 {
                    println!("[{role}] (Hi liveness pings received so far: {hi_count})");
                }
            }
            PACKAGE_TYPE_HELLO | PACKAGE_TYPE_BYEBYE | PACKAGE_TYPE_HEARTBEAT => {}
            _ => {}
        }
    }
}

/// Outbound leg: connects to Windows' message server and injects decoded
/// Mouse/Keyboard into Wayland. This is the connection real input forwarding
/// runs over.
fn run_client(cfg: &Config, wl: &mut WaylandInput) -> std::io::Result<()> {
    println!("Connecting to {}...", cfg.windows_ip);
    config::write_status(false, "", "Connecting...");
    let stream = TcpStream::connect(&cfg.windows_ip)?;
    run_session(stream, Some(wl), "client", cfg)
}

/// Inbound leg: accepts Windows' own outbound connection back to us (the
/// reverse half of the pair it expects between two machines). Runs forever
/// in its own thread; each accepted connection gets its own thread since
/// nothing here touches the single-threaded Wayland event queue.
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
            Ok(stream) => {
                println!("[server] Accepted connection from {:?}", stream.peer_addr());
                let cfg = cfg.clone();
                std::thread::spawn(move || {
                    if let Err(e) = run_session(stream, None, "server", &cfg) {
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

    {
        let cfg = cfg.clone();
        std::thread::spawn(move || run_server_listener(cfg));
    }

    loop {
        if let Err(e) = run_client(&cfg, &mut wl) {
            eprintln!("Connection ended: {e}. Reconnecting in 3s...");
            config::write_status(false, "", &format!("Disconnected: {e}. Reconnecting..."));
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}
