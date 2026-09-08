// Proof of concept: inject synthetic mouse motion + a keypress into Hyprland
// via the wlr virtual-pointer and virtual-keyboard Wayland protocols, with no
// portal/EIS involved at all (this is a native, non-sandboxed client talking
// directly to the compositor's own exposed protocols).

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
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_seat" => {
                    state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, 1, qh, ()));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.pointer_manager = Some(registry.bind::<ZwlrVirtualPointerManagerV1, _, _>(
                        name,
                        2,
                        qh,
                        (),
                    ));
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
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerManagerV1,
        _event: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerV1,
        _event: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardManagerV1,
        _event: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardV1,
        _event: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

fn now_ms() -> u32 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    d.as_millis() as u32
}

// Builds a minimal "us" XKB keymap, writes it to a memfd, and returns the fd
// + its size, ready to hand to zwp_virtual_keyboard_v1.keymap().
fn build_us_keymap_fd() -> (std::os::fd::OwnedFd, u32) {
    use xkbcommon::xkb;

    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        "",     // rules
        "",     // model
        "us",   // layout
        "",     // variant
        None,   // options
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .expect("failed to compile us keymap");

    let keymap_string = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
    let bytes = keymap_string.as_bytes();

    let memfd = memfd::MemfdOptions::default()
        .create("mwb-poc-keymap")
        .expect("memfd create failed");
    {
        use std::io::Write;
        let mut file = memfd.as_file();
        file.write_all(bytes).unwrap();
        file.write_all(b"\0").unwrap(); // NUL-terminated, per protocol
    }

    let size = (bytes.len() + 1) as u32;
    (memfd.into_file().into(), size)
}

fn main() {
    let conn = Connection::connect_to_env().expect("failed to connect to Wayland display");
    let display = conn.display();

    let mut event_queue: EventQueue<AppState> = conn.new_event_queue();
    let qh = event_queue.handle();

    let _registry = display.get_registry(&qh, ());

    let mut state = AppState {
        seat: None,
        pointer_manager: None,
        keyboard_manager: None,
    };

    // Roundtrip so the registry finishes advertising globals.
    event_queue.roundtrip(&mut state).unwrap();
    event_queue.roundtrip(&mut state).unwrap();

    let seat = state.seat.clone().expect("no wl_seat found");
    let pointer_manager = state
        .pointer_manager
        .clone()
        .expect("compositor has no zwlr_virtual_pointer_manager_v1 (not wlroots-based, or protocol disabled)");
    let keyboard_manager = state
        .keyboard_manager
        .clone()
        .expect("compositor has no zwp_virtual_keyboard_manager_v1");

    println!("Bound all required globals. Creating virtual pointer + keyboard...");

    let pointer = pointer_manager.create_virtual_pointer(Some(&seat), &qh, ());

    println!("Moving mouse by (40, 0) x5, with a short pause between each so it's visible...");
    for _ in 0..5 {
        pointer.motion(now_ms(), 40.0, 0.0);
        pointer.frame();
        event_queue.roundtrip(&mut state).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    println!("Sending a left-click (press + release)...");
    const BTN_LEFT: u32 = 0x110;
    pointer.button(
        now_ms(),
        BTN_LEFT,
        wl_pointer::ButtonState::Pressed,
    );
    pointer.frame();
    event_queue.roundtrip(&mut state).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(80));
    pointer.button(
        now_ms(),
        BTN_LEFT,
        wl_pointer::ButtonState::Released,
    );
    pointer.frame();
    event_queue.roundtrip(&mut state).unwrap();

    println!("Setting up virtual keyboard (US layout) and typing 'a'...");
    let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());
    let (keymap_fd, keymap_size) = build_us_keymap_fd();
    keyboard.keymap(1 /* XKB_V1 */, keymap_fd.as_fd(), keymap_size);
    event_queue.roundtrip(&mut state).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));

    // evdev keycode for 'a' is 30; the Wayland virtual-keyboard protocol
    // wants the evdev code, not the XKB keycode (which is evdev + 8).
    const KEY_A: u32 = 30;
    const PRESSED: u32 = 1;
    const RELEASED: u32 = 0;
    keyboard.key(now_ms(), KEY_A, PRESSED);
    event_queue.roundtrip(&mut state).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(80));
    keyboard.key(now_ms(), KEY_A, RELEASED);
    event_queue.roundtrip(&mut state).unwrap();

    println!("Done. Check: did the mouse move, click, and did an 'a' get typed wherever focus was?");
}
