// InputSink impl for macOS: posts synthetic mouse/keyboard events via Core
// Graphics (CGEventPost), the same mechanism screen-control/automation apps
// (e.g. Karabiner, Hammerspoon) use — no portal/EIS equivalent needed here,
// but unlike the Linux build this requires the running process to be
// manually granted Accessibility permission (System Settings > Privacy &
// Security > Accessibility) before CGEventPost has any effect; posting
// without it silently does nothing (no error), which is worth remembering
// if input forwarding looks like it's "not working" — check that grant
// first, same category of "convincing false positive" the MWB Windows-side
// toasts turned out to be on the Linux build.

use std::time::{Duration, Instant};

use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGPoint, CGRect};

use mwb_protocol::input_handling::{InputSink, ModifierState};

use crate::keycode_macos::evdev_to_cgkeycode;

// Real MWB/CGEventPost mouse-button index convention: 0=left, 1=right,
// 2=center/middle, and (matching the de facto standard most apps/browsers
// use for NSEvent.buttonNumber, same as X11/evdev's BTN_SIDE/BTN_EXTRA
// ordering) 3=back/side, 4=forward/extra.
const CG_BUTTON_CENTER: i64 = 2;
const CG_BUTTON_SIDE: i64 = 3;
const CG_BUTTON_EXTRA: i64 = 4;

// Apple's own default double-click interval is user-configurable (System
// Settings > Trackpad/Mouse), but there's no simple way to read that
// preference from here — this fixed value matches Apple's own default.
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
// A second click further than this from the first doesn't count as the
// same click for click-count purposes — keeps a click, a small hand-jitter
// move, then a second click nearby still registering as a double-click,
// while a click somewhere else entirely doesn't.
const DOUBLE_CLICK_DISTANCE: f64 = 5.0;

// Picked for a comfortable, trackpad-like feel at the shared protocol's
// default scroll_speed=1.0 (one Windows wheel click -> ~45px here); tune
// alongside config.json's scroll_speed multiplier if it still feels off.
const PIXELS_PER_SCROLL_UNIT: f64 = 6.0;

/// The bounding rectangle across every active display, in Quartz's global
/// coordinate space (origin top-left of the main display; a display to its
/// left/below has negative origin coordinates) — what MWB's 0..=65535
/// absolute coordinates should map fractionally onto so the cursor can
/// actually reach every monitor, not just the main one. Fixed live,
/// 2026-09-22: mapping onto `CGDisplay::main().bounds()` alone left the
/// second monitor completely unreachable, since any fraction beyond the
/// main display's own width/height was clamped right back inside it. Real
/// MWB/Windows has the equivalent concept (SendInput's absolute coordinates
/// span the sending machine's *own* full virtual screen, multi-monitor
/// included) — this is the receiving-side counterpart.
fn virtual_desktop_bounds() -> CGRect {
    let Ok(ids) = CGDisplay::active_displays() else {
        return CGDisplay::main().bounds();
    };
    let mut bounds_iter = ids.into_iter().map(|id| CGDisplay::new(id).bounds());
    let Some(first) = bounds_iter.next() else {
        return CGDisplay::main().bounds();
    };
    let (mut min_x, mut min_y) = (first.origin.x, first.origin.y);
    let (mut max_x, mut max_y) = (first.origin.x + first.size.width, first.origin.y + first.size.height);
    for b in bounds_iter {
        min_x = min_x.min(b.origin.x);
        min_y = min_y.min(b.origin.y);
        max_x = max_x.max(b.origin.x + b.size.width);
        max_y = max_y.max(b.origin.y + b.size.height);
    }
    CGRect::new(&CGPoint::new(min_x, min_y), &core_graphics::geometry::CGSize::new(max_x - min_x, max_y - min_y))
}

pub struct CgInput {
    source: CGEventSource,
    flags: CGEventFlags,
    // Tracked ourselves rather than re-queried from the system before every
    // event: a null CGEventCreate() to read back the live cursor position
    // works too, but keeping our own last-posted position avoids an extra
    // syscall per event and matches what we just told the OS to do anyway.
    pos: CGPoint,
    // Which button is currently held, if any — determines whether a move
    // posts as MouseMoved or a *Dragged variant (see `move_absolute`'s doc
    // comment for why this matters: window/text-selection drag tracking
    // specifically listens for the Dragged event types, not MouseMoved with
    // a button field set).
    held_button: Option<(CGEventType, Option<i64>)>,
    // Double/triple-click tracking (see `button`'s doc comment) — macOS
    // apps use the click-count field to distinguish a single click from a
    // double-click (e.g. title-bar zoom), which CGEventCreateMouseEvent
    // doesn't infer on its own the way a real hardware click stream does.
    last_click_button: Option<u32>,
    last_click_time: Instant,
    last_click_pos: CGPoint,
    click_count: i64,
}

impl CgInput {
    pub fn new() -> Self {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .expect("failed to create CGEventSource — is this process sandboxed?");
        let bounds = virtual_desktop_bounds();
        let pos = CGPoint::new(bounds.origin.x + bounds.size.width / 2.0, bounds.origin.y + bounds.size.height / 2.0);
        Self {
            source,
            flags: CGEventFlags::CGEventFlagNull,
            pos,
            held_button: None,
            last_click_button: None,
            last_click_time: Instant::now(),
            last_click_pos: pos,
            click_count: 1,
        }
    }

    fn post_mouse(&self, event_type: CGEventType, button_field: Option<i64>, click_count: Option<i64>) {
        let Ok(event) = CGEvent::new_mouse_event(self.source.clone(), event_type, self.pos, CGMouseButton::Left) else {
            eprintln!("(cg_input: failed to create mouse event)");
            return;
        };
        event.set_flags(self.flags);
        if let Some(n) = button_field {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, n);
        }
        if let Some(n) = click_count {
            event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, n);
        }
        event.post(CGEventTapLocation::HID);
    }
}

impl Default for CgInput {
    fn default() -> Self {
        Self::new()
    }
}

impl InputSink for CgInput {
    /// `x`/`y` are MWB's 0..=x_extent/0..=y_extent absolute values (always
    /// 0..=65535 in practice — see `handle_mouse`), fractions of the
    /// *receiving* machine's own full screen, same convention Windows'
    /// SendInput(MOUSEEVENTF_ABSOLUTE) uses. Mapped onto the bounding
    /// rectangle across every active display — see `virtual_desktop_bounds`.
    ///
    /// Posts a `*MouseDragged` event instead of plain `MouseMoved` whenever
    /// a button is currently held (`held_button`, set by `button` below) —
    /// confirmed live, 2026-09-22, this is required, not cosmetic: window
    /// dragging and title-bar double-click-to-zoom both rely on the
    /// WindowServer's live drag-tracking, which specifically watches for
    /// the Dragged event *types*, not just a MouseMoved event with a button
    /// field set. Posting plain MouseMoved while a button was held meant a
    /// dragged window never visually followed the cursor, only snapping to
    /// its final position on mouse-up.
    fn move_absolute(&mut self, x: u32, y: u32, x_extent: u32, y_extent: u32) {
        let bounds = virtual_desktop_bounds();
        let fx = x as f64 / x_extent as f64;
        let fy = y as f64 / y_extent as f64;
        self.pos = CGPoint::new(bounds.origin.x + fx * bounds.size.width, bounds.origin.y + fy * bounds.size.height);
        let (event_type, button_field) = match self.held_button {
            Some((CGEventType::LeftMouseDown, _)) => (CGEventType::LeftMouseDragged, None),
            Some((CGEventType::RightMouseDown, _)) => (CGEventType::RightMouseDragged, None),
            Some((CGEventType::OtherMouseDown, field)) => (CGEventType::OtherMouseDragged, field),
            _ => (CGEventType::MouseMoved, None),
        };
        self.post_mouse(event_type, button_field, None);
    }

    /// Posts button down/up, tracking two bits of state real hardware click
    /// streams carry implicitly but `CGEventCreateMouseEvent` doesn't infer
    /// on its own:
    /// - **Held-button state** (`held_button`), consumed by `move_absolute`
    ///   above to post drag events instead of plain moves.
    /// - **Click count** (`EventField::MOUSE_EVENT_CLICK_STATE`): a second
    ///   press of the same button, close in time and position to the first
    ///   (Apple's own double-click interval/distance conventions), needs
    ///   `click_count=2` for apps/the WindowServer to recognize it as a
    ///   double-click (e.g. title-bar zoom) rather than two independent
    ///   single clicks — confirmed live, 2026-09-22, double-click-to-zoom
    ///   silently did nothing without this, no error, matching the general
    ///   "looks fine, does nothing" failure mode this whole module has
    ///   already hit a few times. The same count applies to both the down
    ///   and its matching up event, mirroring real click semantics.
    fn button(&mut self, button_code: u32, pressed: bool) {
        use mwb_protocol::input_handling::{BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE};
        let (down_type, up_type, button_field) = match button_code {
            BTN_LEFT => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, None),
            BTN_RIGHT => (CGEventType::RightMouseDown, CGEventType::RightMouseUp, None),
            BTN_MIDDLE => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, Some(CG_BUTTON_CENTER)),
            BTN_SIDE => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, Some(CG_BUTTON_SIDE)),
            BTN_EXTRA => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, Some(CG_BUTTON_EXTRA)),
            _ => return,
        };

        if pressed {
            let now = Instant::now();
            let dx = self.pos.x - self.last_click_pos.x;
            let dy = self.pos.y - self.last_click_pos.y;
            let same_spot = (dx * dx + dy * dy).sqrt() <= DOUBLE_CLICK_DISTANCE;
            self.click_count = if self.last_click_button == Some(button_code)
                && now.duration_since(self.last_click_time) <= DOUBLE_CLICK_INTERVAL
                && same_spot
            {
                self.click_count + 1
            } else {
                1
            };
            self.last_click_button = Some(button_code);
            self.last_click_time = now;
            self.last_click_pos = self.pos;
            self.held_button = Some((down_type, button_field));
            self.post_mouse(down_type, button_field, Some(self.click_count));
        } else {
            self.held_button = None;
            self.post_mouse(up_type, button_field, Some(self.click_count));
        }
    }

    /// `value` arrives in `handle_mouse`'s shared ~15-units-per-Windows-click
    /// convention (tuned for Wayland's `axis` semantics). `ScrollEventUnit::
    /// LINE` treats each unit as a full line-of-text jump — confirmed live,
    /// 2026-09-22, as large, jumpy scrolling. `PIXEL` units plus a modest
    /// per-unit scale gives smooth, trackpad-like scrolling instead. Sign
    /// also confirmed live as inverted relative to what `handle_mouse`
    /// already flipped Windows' delta to for Wayland's convention —
    /// negated once more here to correct for CGEvent's opposite convention.
    fn scroll_vertical(&mut self, value: f64) {
        let pixels = -(value * PIXELS_PER_SCROLL_UNIT).round() as i32;
        let Ok(event) = CGEvent::new_scroll_event(self.source.clone(), ScrollEventUnit::PIXEL, 1, pixels, 0, 0) else {
            eprintln!("(cg_input: failed to create scroll event)");
            return;
        };
        event.post(CGEventTapLocation::HID);
    }

    /// Same PIXEL-unit smoothing as `scroll_vertical`. Sign not yet
    /// independently live-verified (only vertical scrolling was tested
    /// 2026-09-22) — flip if it turns out backward too.
    fn scroll_horizontal(&mut self, value: f64) {
        let pixels = (value * PIXELS_PER_SCROLL_UNIT).round() as i32;
        let Ok(event) = CGEvent::new_scroll_event(self.source.clone(), ScrollEventUnit::PIXEL, 2, 0, pixels, 0) else {
            eprintln!("(cg_input: failed to create scroll event)");
            return;
        };
        event.post(CGEventTapLocation::HID);
    }

    fn key(&mut self, evdev_code: u32, pressed: bool) {
        let Some(keycode) = evdev_to_cgkeycode(evdev_code) else {
            eprintln!("(cg_input: no CGKeyCode mapping for evdev code {evdev_code}, ignoring)");
            return;
        };
        let Ok(event) = CGEvent::new_keyboard_event(self.source.clone(), keycode, pressed) else {
            eprintln!("(cg_input: failed to create keyboard event)");
            return;
        };
        event.set_flags(self.flags);
        event.post(CGEventTapLocation::HID);
    }

    /// Converts the generic `ModifierState` into `CGEventFlags`, stored and
    /// applied to every subsequent event this sink posts (CGEventPost has no
    /// separate "set modifier state" call the way the Wayland virtual-
    /// keyboard protocol does — each event just carries its own flags
    /// field). `numlock` is ignored: unlike Linux/XKB, Mac keypads have no
    /// NumLock lock-state to desync in the first place — numpad CGKeyCodes
    /// always produce digits regardless of any flag.
    fn modifiers(&mut self, state: ModifierState, _numlock: bool) {
        let mut flags = CGEventFlags::CGEventFlagNull;
        if state.shift {
            flags |= CGEventFlags::CGEventFlagShift;
        }
        if state.ctrl {
            flags |= CGEventFlags::CGEventFlagControl;
        }
        if state.alt {
            flags |= CGEventFlags::CGEventFlagAlternate;
        }
        if state.super_ {
            flags |= CGEventFlags::CGEventFlagCommand;
        }
        // No separate "Level 3 shift" flag on macOS — AltGr-equivalent
        // characters are produced by the input source's own Option-key
        // dead-key/special-character tables once CGEventFlagAlternate is
        // set, so level3 folds into the same Alternate flag here rather
        // than needing its own bit the way Linux/XKB's Mod5 did.
        if state.level3 {
            flags |= CGEventFlags::CGEventFlagAlternate;
        }
        self.flags = flags;
    }
}
