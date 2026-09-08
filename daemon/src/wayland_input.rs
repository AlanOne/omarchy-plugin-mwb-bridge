// Reusable Wayland virtual-pointer/virtual-keyboard injector, factored out
// of the Phase-1 PoC (see the crate root's src/main.rs for the original,
// verified-working standalone version). No portal/EIS involved — this is a
// native (non-sandboxed) client talking directly to Hyprland's own exposed
// wlroots protocols.

use std::os::fd::AsFd;
use std::time::{SystemTime, UNIX_EPOCH};

use wayland_client::{
    protocol::{wl_pointer, wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

struct AppState {
    seat: Option<wl_seat::WlSeat>,
    pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, .. } = event {
            match interface.as_str() {
                "wl_seat" => {
                    state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, 1, qh, ()));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.pointer_manager =
                        Some(registry.bind::<ZwlrVirtualPointerManagerV1, _, _>(name, 2, qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.keyboard_manager =
                        Some(registry.bind::<ZwpVirtualKeyboardManagerV1, _, _>(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for AppState {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn now_ms() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u32
}

// Builds the virtual keyboard's XKB keymap for the given layout/variant
// (e.g. "si"/"" for Slovenian, "us"/"" for US, "de"/"nodeadkeys", ...) — must
// match whatever layout this machine's compositor is actually using, so
// Windows' physical-key-position VK codes land on the same character
// mapping the user's muscle memory expects here. The caller (main.rs reads
// this from Config; daemon.rs's Config comes from the Omarchy plugin, which
// detects it via `hyprctl getoption input:kb_layout`) is responsible for
// supplying the right value — this function just compiles whatever it's given.
fn build_keymap_fd(layout: &str, variant: &str) -> (std::os::fd::OwnedFd, u32) {
    use xkbcommon::xkb;

    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &context, "", "", layout, variant, None, xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .unwrap_or_else(|| panic!("failed to compile XKB keymap for layout {layout:?} variant {variant:?}"));

    let keymap_string = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
    let bytes = keymap_string.as_bytes();

    let memfd = memfd::MemfdOptions::default()
        .create("mwb-bridge-keymap")
        .expect("memfd create failed");
    {
        use std::io::Write;
        let mut file = memfd.as_file();
        file.write_all(bytes).unwrap();
        file.write_all(b"\0").unwrap();
    }

    let size = bytes.len() as u32 + 1;
    (memfd.into_file().into(), size)
}

pub struct WaylandInput {
    event_queue: EventQueue<AppState>,
    state: AppState,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
}

impl WaylandInput {
    /// Connects to the Wayland display, binds the virtual pointer/keyboard
    /// managers, and creates one virtual pointer + one virtual keyboard
    /// (with the given XKB layout/variant loaded — pass this machine's own
    /// active layout, not a hardcoded one, or typed characters won't match
    /// what's expected). Panics if the compositor doesn't expose these
    /// protocols (not wlroots-based, or they're disabled), or if the
    /// layout/variant name isn't valid.
    pub fn new(xkb_layout: &str, xkb_variant: &str) -> Self {
        let conn = Connection::connect_to_env().expect("failed to connect to Wayland display");
        let display = conn.display();

        let mut event_queue: EventQueue<AppState> = conn.new_event_queue();
        let qh = event_queue.handle();
        let _registry = display.get_registry(&qh, ());

        let mut state = AppState { seat: None, pointer_manager: None, keyboard_manager: None };
        event_queue.roundtrip(&mut state).unwrap();
        event_queue.roundtrip(&mut state).unwrap();

        let seat = state.seat.clone().expect("no wl_seat found");
        let pointer_manager = state
            .pointer_manager
            .clone()
            .expect("compositor has no zwlr_virtual_pointer_manager_v1");
        let keyboard_manager = state
            .keyboard_manager
            .clone()
            .expect("compositor has no zwp_virtual_keyboard_manager_v1");

        let pointer = pointer_manager.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());

        let (keymap_fd, keymap_size) = build_keymap_fd(xkb_layout, xkb_variant);
        keyboard.keymap(1 /* XKB_V1 */, keymap_fd.as_fd(), keymap_size);
        event_queue.roundtrip(&mut state).unwrap();

        Self { event_queue, state, pointer, keyboard }
    }

    fn flush(&mut self) {
        self.event_queue.roundtrip(&mut self.state).unwrap();
    }

    /// Absolute pointer move. `x`/`y` range 0..=x_extent/y_extent — pass MWB's
    /// own 0..=65535 values straight through, no rescaling needed.
    pub fn move_absolute(&mut self, x: u32, y: u32, x_extent: u32, y_extent: u32) {
        self.pointer.motion_absolute(now_ms(), x, y, x_extent, y_extent);
        self.pointer.frame();
        self.flush();
    }

    pub fn move_relative(&mut self, dx: f64, dy: f64) {
        self.pointer.motion(now_ms(), dx, dy);
        self.pointer.frame();
        self.flush();
    }

    /// `button_code` is the raw evdev/Wayland button code (e.g. 0x110 =
    /// BTN_LEFT, 0x111 = BTN_RIGHT, 0x112 = BTN_MIDDLE).
    pub fn button(&mut self, button_code: u32, pressed: bool) {
        let state = if pressed { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
        self.pointer.button(now_ms(), button_code, state);
        self.pointer.frame();
        self.flush();
    }

    /// Vertical scroll. `value` follows wl_pointer.axis semantics (positive =
    /// down/right, in "120ths of a click" style units — callers translate
    /// from whatever their source protocol's wheel delta uses).
    pub fn scroll_vertical(&mut self, value: f64) {
        self.pointer.axis(now_ms(), wl_pointer::Axis::VerticalScroll, value);
        self.pointer.frame();
        self.flush();
    }

    /// `evdev_code` is the Linux evdev keycode (e.g. KEY_A = 30) — NOT the
    /// XKB keycode (which is evdev + 8).
    pub fn key(&mut self, evdev_code: u32, pressed: bool) {
        let state = if pressed { 1 } else { 0 };
        self.keyboard.key(now_ms(), evdev_code, state);
        self.flush();
    }

    /// Sets the full modifier mask directly (depressed/latched/locked groups
    /// per the virtual-keyboard protocol's modifiers() request). Callers
    /// track their own modifier state and call this before key() when it
    /// changes, mirroring how a real compositor tracks Shift/Ctrl/Alt/Super.
    pub fn modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.keyboard.modifiers(depressed, latched, locked, group);
        self.flush();
    }
}
