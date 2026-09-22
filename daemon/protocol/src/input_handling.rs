// Platform-agnostic decoding of MWB's raw Mouse/Keyboard wire values (Win32
// message flags, VK codes, WHEEL_DELTA) into semantic input actions, handed
// off to a per-OS `InputSink` impl that does the actual platform injection
// (Wayland virtual-pointer/keyboard on Linux, Core Graphics event posting on
// macOS). Shared verbatim across both daemons — every quirk decoded here
// (repeat-key dedup, mouse-coordinate clamping, scroll-unit conversion,
// NumLock desync avoidance, the lock-combo detector) was found via
// live-testing against real Windows traffic on the Linux build and applies
// identically regardless of what's actually receiving the input; only the
// key-code space (`InputSink::key` takes the evdev code `vk_to_evdev`
// already produces — each impl maps that to its own native code) and
// modifier representation are platform-specific, isolated in `InputSink`.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::vk_keycode::vk_to_evdev;

// Win32 WM_* message constants carried in Mouse/Keyboard dwFlags fields.
pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_RBUTTONDOWN: u32 = 0x0204;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const WM_MBUTTONDOWN: u32 = 0x0207;
pub const WM_MBUTTONUP: u32 = 0x0208;
pub const WM_MOUSEWHEEL: u32 = 0x020A;
pub const WM_XBUTTONDOWN: u32 = 0x020B;
pub const WM_XBUTTONUP: u32 = 0x020C;
pub const WM_MOUSEHWHEEL: u32 = 0x020E;

// Conventional evdev-style button codes — the canonical vocabulary
// `InputSink::button` is called with; each platform maps these to its own
// native button identifier.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;

// LLKHF_UP: bit 7 of a low-level-keyboard-hook's flags marks a key-up event.
pub const LLKHF_UP: u32 = 0x80;

const VK_NUMLOCK: u32 = 0x90;
const VK_L: u32 = 0x4C;

// Windows reports one wheel "click" as WHEEL_DELTA = 120 (WM_MOUSEWHEEL's
// HIWORD), but neither Wayland's wlr-virtual-pointer `axis` value nor Core
// Graphics' scroll-wheel event line-delta use that convention — both are
// closer to the handful-of-units a real wheel's own driver reports per
// click (confirmed empirically on the Wayland side, ~15). Forwarding
// Windows' raw 120 straight through overscrolls by roughly 8x (confirmed by
// Alan actually feeling it: "a single scroll moves the page too much" — see
// the mwb-omarchy-bridge project memory). This baseline converts one
// Windows click into one conventional ~15-unit click; the caller's
// `scroll_speed` (user-tunable) multiplies on top of that for taste.
pub const SCROLL_UNITS_PER_WHEEL_CLICK: f64 = 15.0 / 120.0;

// How long a lock-combo burst (see `LockComboDetector`) may span and still
// count as one. Real MWB's HotKeyLockMachine sends the combo's own keys as
// an ordinary, very-fast Keyboard-packet burst (all down back-to-back, then
// all up back-to-back) right before locking locally — no natural keypress
// produces this pattern (a human pressing even a 2-key chord has real,
// non-zero timing between each key).
const LOCK_COMBO_WINDOW: Duration = Duration::from_millis(150);

/// Semantic modifier state, tracked from raw VK down/up events and handed to
/// `InputSink::modifiers` — each impl converts this into its own native
/// representation (an XKB depressed-modifier bitmask on Linux, `CGEventFlags`
/// on macOS) rather than this module knowing about either.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub struct ModifierState {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_: bool,
    /// AltGr / "Level 3" shift — layouts with AltGr-level characters (e.g.
    /// Slovenian `<`/`>` on the comma/period keys) only resolve to level 3
    /// when this is set, not plain Alt. See the mwb-omarchy-bridge project
    /// memory's "AltGr (Level 3) characters never resolved" entry for how
    /// this was root-caused.
    pub level3: bool,
}

/// The platform-specific injection surface every OS backend implements.
/// `key`/`button` take the evdev-style codes this module already works in
/// (`vk_to_evdev`'s output, and the `BTN_*` constants above) — each impl is
/// responsible for its own final translation to native calls.
pub trait InputSink {
    fn move_absolute(&mut self, x: u32, y: u32, x_extent: u32, y_extent: u32);
    fn button(&mut self, button_code: u32, pressed: bool);
    fn scroll_vertical(&mut self, value: f64);
    fn scroll_horizontal(&mut self, value: f64);
    fn key(&mut self, evdev_code: u32, pressed: bool);
    /// Called whenever tracked modifier state changes, before the `key()`
    /// call for the triggering keypress. Also called (with `numlock: true`)
    /// on every numpad key so an impl that needs to force numpad-producing
    /// state can do so without tracking/toggling a real NumLock lock — see
    /// `handle_keyboard`'s doc comment for why.
    fn modifiers(&mut self, state: ModifierState, numlock: bool);
}

struct ModState {
    state: ModifierState,
}

impl ModState {
    fn new() -> Self {
        Self { state: ModifierState::default() }
    }

    /// Updates tracked modifier state for this VK if it's a modifier key,
    /// returns true if it was one (caller still forwards the raw keypress
    /// either way — modifiers are real keys too).
    fn update(&mut self, vk: u32, pressed: bool) -> bool {
        match vk {
            0x10 | 0xA0 | 0xA1 => self.state.shift = pressed,
            0x11 | 0xA2 => self.state.ctrl = pressed,
            0x12 | 0xA4 => self.state.alt = pressed,
            // VK_RMENU: Windows reports AltGr as a synthetic VK_LCONTROL
            // down/up pair immediately around the real VK_RMENU (verified
            // empirically — every AltGr press logs 0xA2 then 0xA5, in that
            // order, on both press and release). Treating that fake Ctrl as
            // real Ctrl and this key as plain Alt never reaches level 3 —
            // clear the fake Ctrl bit and set level3 instead.
            0xA5 => {
                self.state.ctrl = false;
                self.state.level3 = pressed;
            }
            0xA3 => self.state.ctrl = pressed, // VK_RCONTROL
            0x5B | 0x5C => self.state.super_ = pressed,
            _ => return false,
        }
        true
    }
}

/// Recognizes real MWB's own HotKeyLockMachine double-tap burst arriving as
/// regular Keyboard packets: a small ring buffer of recent (vk, pressed)
/// events with timestamps, firing once LWIN-down, L-down, LWIN-up, and L-up
/// are all present within `LOCK_COMBO_WINDOW` of each other (order doesn't
/// matter beyond that — real MWB sends down-then-up, but matching by
/// presence-in-window is simpler and just as reliable given no natural
/// keypress could produce this set that fast either way).
pub struct LockComboDetector {
    recent: VecDeque<(u32, bool, Instant)>,
}

impl LockComboDetector {
    pub fn new() -> Self {
        Self { recent: VecDeque::with_capacity(8) }
    }

    /// Returns true (once) when the combo is detected — clears its buffer
    /// afterward so the same burst can't re-fire on a later call.
    pub fn observe(&mut self, vk: u32, pressed: bool) -> bool {
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

impl Default for LockComboDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Decodes one Mouse packet's raw (m1, m2, m3, dwFlags) into calls on `sink`.
pub fn handle_mouse(sink: &mut impl InputSink, m1: u32, m2: u32, m3: u32, flags: u32, scroll_speed: f64) {
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
            sink.move_absolute(x, y, 65535, 65535)
        }
        WM_LBUTTONDOWN => sink.button(BTN_LEFT, true),
        WM_LBUTTONUP => sink.button(BTN_LEFT, false),
        WM_RBUTTONDOWN => sink.button(BTN_RIGHT, true),
        WM_RBUTTONUP => sink.button(BTN_RIGHT, false),
        WM_MBUTTONDOWN => sink.button(BTN_MIDDLE, true),
        WM_MBUTTONUP => sink.button(BTN_MIDDLE, false),
        WM_MOUSEWHEEL => {
            // WheelDelta is a signed 16-bit value in Windows' usual +/-120
            // per notch. Sign flipped (Windows: positive = away from user/
            // up; our convention: positive = down) — matches physical
            // scroll-wheel direction.
            let delta = m3 as i32 as i16 as f64;
            sink.scroll_vertical(-delta * SCROLL_UNITS_PER_WHEEL_CLICK * scroll_speed);
        }
        WM_MOUSEHWHEEL => {
            // Same signed-16-bit-in-m3 shape as WM_MOUSEWHEEL (confirmed
            // from real MWB's own InputHook.cs: WheelDelta is set from the
            // same HIWORD(MouseData) read for every mouse message, not
            // just WM_MOUSEWHEEL). Windows: positive = right; our
            // convention: positive = right too, no sign flip needed.
            let delta = m3 as i32 as i16 as f64;
            sink.scroll_horizontal(delta * SCROLL_UNITS_PER_WHEEL_CLICK * scroll_speed);
        }
        // Real MWB (per InputHook.cs, confirmed from source): every mouse
        // message's WheelDelta slot (here, m3) is set from HIWORD(MouseData)
        // regardless of message type — for WM_MOUSEWHEEL that's the scroll
        // delta, but for WM_XBUTTONDOWN/UP it's *which* extra button
        // (XBUTTON1=1, XBUTTON2=2), per the Win32 MSLLHOOKSTRUCT contract.
        WM_XBUTTONDOWN => sink.button(if m3 == 2 { BTN_EXTRA } else { BTN_SIDE }, true),
        WM_XBUTTONUP => sink.button(if m3 == 2 { BTN_EXTRA } else { BTN_SIDE }, false),
        _ => {}
    }
}

/// Per-connection keyboard-handling state (`handle_keyboard` needs both
/// pieces together — bundled so callers only need to carry one value).
pub struct KeyboardState {
    mods: ModState,
    pressed_keys: HashSet<u32>,
}

impl KeyboardState {
    pub fn new() -> Self {
        Self { mods: ModState::new(), pressed_keys: HashSet::new() }
    }
}

impl Default for KeyboardState {
    fn default() -> Self {
        Self::new()
    }
}

/// Decodes one Keyboard packet's raw (wVk, dwFlags) into calls on `sink`.
pub fn handle_keyboard(sink: &mut impl InputSink, state: &mut KeyboardState, vk: u32, flags: u32) {
    // Windows' own NumLock state and the receiving machine's are two
    // independent, unsynchronized locks. Forwarding the raw NumLock
    // keypress would toggle the receiver's own lock-state — if the two
    // ever disagree, numpad digit keys can silently become navigation keys
    // instead. Numpad keys below force numpad-producing state on every
    // press instead (via the `numlock` flag to `sink.modifiers`), so this
    // never needs tracking or toggling at all.
    if vk == VK_NUMLOCK {
        return;
    }

    let pressed = (flags & LLKHF_UP) == 0;
    let is_mod = state.mods.update(vk, pressed);
    let Some(evdev_code) = vk_to_evdev(vk) else {
        eprintln!("(no evdev mapping for VK 0x{vk:02x}, ignoring)");
        return;
    };
    if is_mod {
        sink.modifiers(state.mods.state, false);
    } else if (0x60..=0x6F).contains(&vk) {
        sink.modifiers(state.mods.state, true);
    }

    // Windows forwards its own OS-level auto-repeat "down" messages for a
    // held key — a real, expected part of the wire protocol, not a bug on
    // its end. Only forward the actual press/release *transition*; a held
    // key generating repeat characters is the receiving OS's own repeat
    // timer's job once it sees the first press, same as a real local
    // keyboard (whose driver never re-sends "pressed" for a key that's
    // still down either). Forwarding every one of Windows' repeat packets
    // as a fresh press compounds with the receiver's own repeat, producing
    // far more characters than intended for any key held even slightly too
    // long. Releases always forward regardless of tracked state, so a
    // missed/out-of-order press can never leave a key stuck.
    if pressed {
        if !state.pressed_keys.insert(evdev_code) {
            return;
        }
    } else {
        state.pressed_keys.remove(&evdev_code);
    }
    sink.key(evdev_code, pressed);
}
