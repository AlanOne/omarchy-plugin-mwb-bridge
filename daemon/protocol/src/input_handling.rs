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

// "Move mouse relatively" (a PowerToys MWB setting): Windows sends each move as
// a delta instead of an absolute position, with both values pushed this far
// from zero so the receiver can tell them apart (dx = X - 100000 for X > 0,
// X + 100000 for X < 0; a zero delta arrives as +100000). Event.cs's
// MOVE_MOUSE_RELATIVE in PowerToys' source.
pub const MOVE_MOUSE_RELATIVE: i32 = 100_000;

// In relative mode Windows stops checking screen edges for us: the receiving
// machine has to notice the cursor being pushed off the edge that faces
// Windows and send a NextMachine package back (Receiver.cs's relative branch
// -> MoveToMyNeighbourIfNeeded -> SendNextMachine). These are the universal
// (0..65535) coordinates the cursor reappears at on Windows: just inside the
// edge facing this machine, like MWB's own JUMP_PIXELS offset.
const SWITCH_BACK_INSET: u32 = 300;
const SWITCH_BACK_MIN_INTERVAL: Duration = Duration::from_millis(300);
// An absolute move this close to an edge (universal units, ~2%) right after a
// switch is the entry point, and tells us which edge leads back to Windows.
const ENTRY_EDGE_MARGIN: u32 = 1400;

/// A side of this machine's screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScreenEdge {
    Left,
    Right,
    Top,
    Bottom,
}

impl ScreenEdge {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "top" | "up" => Some(Self::Top),
            "bottom" | "down" => Some(Self::Bottom),
            _ => None,
        }
    }
}

/// A relative move that tried to carry the cursor past a screen edge.
/// `along` is the cursor's position along that edge, 0..=1 (top-to-bottom for
/// Left/Right, left-to-right for Top/Bottom).
#[derive(Clone, Copy, Debug)]
pub struct EdgeExit {
    pub edge: ScreenEdge,
    pub along: f64,
}

/// Where Windows should put its cursor when control switches back to it
/// (universal 0..65535 coordinates): the payload of a NextMachine package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwitchBack {
    pub x: u32,
    pub y: u32,
}

/// Per-connection mouse state for relative mode: which edge leads back to
/// Windows (configured, or learned from where the cursor enters), and when we
/// last asked to switch back.
pub struct MouseState {
    configured_edge: Option<ScreenEdge>,
    learned_edge: Option<ScreenEdge>,
    relative_seen: bool,
    last_packet: Option<Instant>,
    last_switch_back: Option<Instant>,
}

impl MouseState {
    /// `configured_edge`: config.json's `windows_side`, if set; otherwise it's
    /// learned from the first switch into this machine.
    pub fn new(configured_edge: Option<ScreenEdge>) -> Self {
        Self { configured_edge, learned_edge: None, relative_seen: false, last_packet: None, last_switch_back: None }
    }

    pub fn windows_edge(&self) -> Option<ScreenEdge> {
        self.configured_edge.or(self.learned_edge)
    }

    /// An absolute move arriving as the entry point of a switch (any absolute
    /// move once relative mode is in use, or the first move after a pause),
    /// close enough to an edge to say which side Windows is on.
    fn learn_entry(&mut self, x: u32, y: u32, after_pause: bool) {
        if !(self.relative_seen || after_pause) {
            return;
        }
        let edge = if x <= ENTRY_EDGE_MARGIN {
            Some(ScreenEdge::Left)
        } else if x >= 65535 - ENTRY_EDGE_MARGIN {
            Some(ScreenEdge::Right)
        } else if y <= ENTRY_EDGE_MARGIN {
            Some(ScreenEdge::Top)
        } else if y >= 65535 - ENTRY_EDGE_MARGIN {
            Some(ScreenEdge::Bottom)
        } else {
            None
        };
        if edge.is_some() && edge != self.learned_edge {
            self.learned_edge = edge;
            println!("[mouse] Windows is on the {:?} side (learned from where the cursor entered).", edge.unwrap());
        }
    }
}

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
    /// Moves the cursor by (dx, dy) pixels (relative mode), keeping it on
    /// screen. Returns where it was pushed past an edge, if it was, so the
    /// caller can hand control back to Windows. An impl may return `None`
    /// while switching away would be wrong (e.g. an app has captured the
    /// cursor for mouse-look).
    fn move_relative(&mut self, dx: i32, dy: i32) -> Option<EdgeExit>;
    fn button(&mut self, button_code: u32, pressed: bool);
    fn scroll_vertical(&mut self, value: f64);
    fn scroll_horizontal(&mut self, value: f64);
    fn key(&mut self, evdev_code: u32, pressed: bool);
    /// Called for each of Windows' auto-repeat "down" packets for a key
    /// that's already held (never for modifiers). Default: ignore — right
    /// for a receiver whose own OS generates repeat for a held injected key
    /// (Wayland compositors do). An impl whose OS *doesn't* repeat injected
    /// keys (macOS: key repeat comes from the HID layer for real hardware
    /// only, a posted CGEvent key-down never repeats) forwards these instead.
    fn key_repeat(&mut self, _evdev_code: u32) {}
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
/// Returns `Some` when a relative move pushed the cursor off the edge facing
/// Windows: the caller sends Windows a NextMachine package with it.
pub fn handle_mouse(sink: &mut impl InputSink, state: &mut MouseState, m1: u32, m2: u32, m3: u32, flags: u32, scroll_speed: f64) -> Option<SwitchBack> {
    let now = Instant::now();
    let after_pause = state.last_packet.is_none_or(|t| now.duration_since(t) > Duration::from_secs(1));
    state.last_packet = Some(now);

    let (xi, yi) = (m1 as i32, m2 as i32);
    let relative = xi.abs() >= MOVE_MOUSE_RELATIVE && yi.abs() >= MOVE_MOUSE_RELATIVE;
    if relative {
        state.relative_seen = true;
    }

    match flags {
        WM_MOUSEMOVE if relative => {
            let decode = |v: i32| if v < 0 { v + MOVE_MOUSE_RELATIVE } else { v - MOVE_MOUSE_RELATIVE };
            let exit = sink.move_relative(decode(xi), decode(yi))?;
            if Some(exit.edge) != state.windows_edge() {
                return None;
            }
            if state.last_switch_back.is_some_and(|t| now.duration_since(t) < SWITCH_BACK_MIN_INTERVAL) {
                return None;
            }
            state.last_switch_back = Some(now);
            let along = (exit.along.clamp(0.0, 1.0) * 65535.0) as u32;
            // Reappear on Windows just inside its edge that faces us.
            return Some(match exit.edge {
                ScreenEdge::Left => SwitchBack { x: 65535 - SWITCH_BACK_INSET, y: along },
                ScreenEdge::Right => SwitchBack { x: SWITCH_BACK_INSET, y: along },
                ScreenEdge::Top => SwitchBack { x: along, y: 65535 - SWITCH_BACK_INSET },
                ScreenEdge::Bottom => SwitchBack { x: along, y: SWITCH_BACK_INSET },
            });
        }
        WM_MOUSEMOVE => {
            // Observed in real traffic (likely an edge-crossing overshoot):
            // an occasional out-of-range value that's actually small and
            // negative, wrapping to a huge u32 (e.g. 4294967271 = -25 as
            // i32) when read as one. Clamp back into the valid 0..=65535
            // absolute-coordinate range rather than forwarding it verbatim,
            // which would otherwise send the cursor somewhere nonsensical.
            let x = xi.clamp(0, 65535) as u32;
            let y = yi.clamp(0, 65535) as u32;
            state.learn_entry(x, y, after_pause);
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
    None
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
    // its end. Only the actual press/release *transition* goes to `key()`;
    // repeats go to `key_repeat()` instead, which each sink handles per its
    // OS. On Linux a held key generating repeat characters is the
    // compositor's own repeat timer's job once it sees the first press
    // (forwarding Windows' repeats there as fresh presses compounds with
    // it, producing far more characters than intended), so that sink
    // ignores them; macOS never repeats an injected key on its own, so that
    // sink forwards them. Releases always forward regardless of tracked
    // state, so a missed/out-of-order press can never leave a key stuck.
    if pressed {
        if !state.pressed_keys.insert(evdev_code) {
            if !is_mod {
                sink.key_repeat(evdev_code);
            }
            return;
        }
    } else {
        state.pressed_keys.remove(&evdev_code);
    }
    sink.key(evdev_code, pressed);
}

#[cfg(test)]
mod relative_mode_tests {
    use super::*;

    /// Records calls; `exit` is what the next move_relative reports.
    #[derive(Default)]
    struct FakeSink {
        relative: Vec<(i32, i32)>,
        absolute: Vec<(u32, u32)>,
        exit: Option<EdgeExit>,
    }

    impl InputSink for FakeSink {
        fn move_absolute(&mut self, x: u32, y: u32, _: u32, _: u32) { self.absolute.push((x, y)); }
        fn move_relative(&mut self, dx: i32, dy: i32) -> Option<EdgeExit> {
            self.relative.push((dx, dy));
            self.exit.take()
        }
        fn button(&mut self, _: u32, _: bool) {}
        fn scroll_vertical(&mut self, _: f64) {}
        fn scroll_horizontal(&mut self, _: f64) {}
        fn key(&mut self, _: u32, _: bool) {}
        fn modifiers(&mut self, _: ModifierState, _: bool) {}
    }

    fn rel(v: i32) -> u32 {
        (if v < 0 { v - MOVE_MOUSE_RELATIVE } else { v + MOVE_MOUSE_RELATIVE }) as u32
    }

    #[test]
    fn decodes_relative_deltas_including_zero_and_negative() {
        let (mut sink, mut st) = (FakeSink::default(), MouseState::new(None));
        handle_mouse(&mut sink, &mut st, rel(5), rel(-3), 0, WM_MOUSEMOVE, 1.0);
        handle_mouse(&mut sink, &mut st, rel(0), rel(0), 0, WM_MOUSEMOVE, 1.0);
        assert_eq!(sink.relative, vec![(5, -3), (0, 0)]);
        assert!(sink.absolute.is_empty());
    }

    #[test]
    fn absolute_moves_still_work() {
        let (mut sink, mut st) = (FakeSink::default(), MouseState::new(None));
        handle_mouse(&mut sink, &mut st, 30000, 20000, 0, WM_MOUSEMOVE, 1.0);
        assert_eq!(sink.absolute, vec![(30000, 20000)]);
    }

    #[test]
    fn learns_windows_side_from_entry_and_switches_back_there_only() {
        let (mut sink, mut st) = (FakeSink::default(), MouseState::new(None));
        assert_eq!(st.windows_edge(), None);
        // Entry right at the left edge after a pause: Windows is to the left.
        handle_mouse(&mut sink, &mut st, 200, 30000, 0, WM_MOUSEMOVE, 1.0);
        assert_eq!(st.windows_edge(), Some(ScreenEdge::Left));

        // Pushing off the right edge does nothing...
        sink.exit = Some(EdgeExit { edge: ScreenEdge::Right, along: 0.5 });
        assert_eq!(handle_mouse(&mut sink, &mut st, rel(9), rel(0), 0, WM_MOUSEMOVE, 1.0), None);
        // ...pushing off the left edge hands control back, reappearing at Windows' right edge.
        sink.exit = Some(EdgeExit { edge: ScreenEdge::Left, along: 0.25 });
        let back = handle_mouse(&mut sink, &mut st, rel(-9), rel(0), 0, WM_MOUSEMOVE, 1.0).expect("switch back");
        assert_eq!(back, SwitchBack { x: 65535 - SWITCH_BACK_INSET, y: (0.25 * 65535.0) as u32 });
        // A second push right away is throttled.
        sink.exit = Some(EdgeExit { edge: ScreenEdge::Left, along: 0.25 });
        assert_eq!(handle_mouse(&mut sink, &mut st, rel(-9), rel(0), 0, WM_MOUSEMOVE, 1.0), None);
    }

    #[test]
    fn configured_side_wins_and_mid_screen_entry_teaches_nothing() {
        let (mut sink, mut st) = (FakeSink::default(), MouseState::new(ScreenEdge::parse("right")));
        handle_mouse(&mut sink, &mut st, 200, 30000, 0, WM_MOUSEMOVE, 1.0);
        assert_eq!(st.windows_edge(), Some(ScreenEdge::Right));

        let mut st2 = MouseState::new(None);
        handle_mouse(&mut sink, &mut st2, 30000, 30000, 0, WM_MOUSEMOVE, 1.0);
        assert_eq!(st2.windows_edge(), None);
    }

    #[test]
    fn clicks_in_relative_mode_are_not_moves() {
        let (mut sink, mut st) = (FakeSink::default(), MouseState::new(None));
        handle_mouse(&mut sink, &mut st, rel(0), rel(0), 0, WM_LBUTTONDOWN, 1.0);
        assert!(sink.relative.is_empty() && sink.absolute.is_empty());
    }
}
