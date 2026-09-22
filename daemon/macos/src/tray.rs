// Minimal menu bar shell: a status item showing connection state (polled
// from status.json, the same file the network thread already writes via
// `config::write_status`) plus "Edit Config...", "Restart App", and "Quit".
// No in-app settings form yet — editing config.json directly and
// restarting is the whole config UX for milestone 1, deliberately, matching
// how small this first cut is meant to be.

use std::process::Command;
use std::time::Duration;

use mwb_protocol::config;
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

/// A double-headed arrow (↔) — a simple, unambiguous "bridges two machines"
/// glyph, drawn as plain geometry rather than a real vector asset (no
/// designer/asset pipeline in this project). Rendered as a **template**
/// image (`with_icon_as_template`, macOS-only in tray-icon) so AppKit
/// recolors it to match the menu bar's current light/dark appearance and
/// the selected/highlighted state itself, the way every other menu bar
/// icon behaves — a plain RGBA icon would stay a flat black shape
/// regardless of appearance, which looks visibly wrong next to system
/// icons. Only the alpha channel matters for a template image; RGB is set
/// to black by convention.
fn bridge_icon() -> Icon {
    const SIZE: i32 = 36;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let mut set = |x: i32, y: i32| {
        if x < 0 || y < 0 || x >= SIZE || y >= SIZE {
            return;
        }
        let idx = ((y * SIZE + x) * 4) as usize;
        rgba[idx + 3] = 255; // alpha only — RGB stays 0 (black), per template-image convention
    };

    let center_y = SIZE / 2;
    let shaft_half = 2;
    let margin = 3;
    let head_len = 10;
    let head_half_max = 6;

    // Shaft, between the two arrowheads.
    for x in (margin + head_len)..(SIZE - margin - head_len) {
        for dy in -shaft_half..=shaft_half {
            set(x, center_y + dy);
        }
    }
    // Left arrowhead: tip at the left margin, widening rightward.
    for i in 0..head_len {
        let x = margin + i;
        let half = (i * head_half_max) / head_len;
        for dy in -half..=half {
            set(x, center_y + dy);
        }
    }
    // Right arrowhead: mirror of the left.
    for i in 0..head_len {
        let x = SIZE - margin - 1 - i;
        let half = (i * head_half_max) / head_len;
        for dy in -half..=half {
            set(x, center_y + dy);
        }
    }

    Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).expect("valid fixed-size RGBA buffer")
}

fn status_line() -> String {
    match config::read_status() {
        Some(s) if s.connected => format!("Connected to {}", s.peer),
        Some(s) => format!("Status: {}", s.detail),
        None => "Status: starting...".to_string(),
    }
}

fn open_config() {
    let path = config::config_path();
    let _ = Command::new("open").arg("-t").arg(path).spawn();
}

fn restart_app() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe).spawn();
    }
    std::process::exit(0);
}

pub fn run() {
    let mut event_loop = EventLoopBuilder::new().build();
    // `LSUIElement=true` in Info.plist alone isn't enough — tao's own
    // NSApplication setup defaults to `NSApplicationActivationPolicyRegular`
    // regardless (confirmed live, 2026-09-22: the packaged .app still showed
    // a Dock icon despite the Info.plist setting), overriding it. This is
    // the actual switch that keeps a pure menu-bar utility out of the Dock
    // and app switcher — must be called before `run()`.
    event_loop.set_activation_policy(ActivationPolicy::Accessory);

    let menu = Menu::new();
    let status_item = MenuItem::new(status_line(), false, None);
    let edit_config_item = MenuItem::new("Edit Config...", true, None);
    let restart_item = MenuItem::new("Restart App", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    menu.append(&status_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&edit_config_item).unwrap();
    menu.append(&restart_item).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&quit_item).unwrap();

    let _tray_icon = TrayIconBuilder::new()
        .with_icon(bridge_icon())
        .with_icon_as_template(true)
        .with_tooltip("MWB Mac Bridge")
        .with_menu(Box::new(menu))
        .build()
        .expect("failed to create the menu bar status item");

    let menu_channel = MenuEvent::receiver();
    let edit_config_id = edit_config_item.id().clone();
    let restart_id = restart_item.id().clone();
    let quit_id = quit_item.id().clone();

    event_loop.run(move |event, _elwt, control_flow| {
        *control_flow = ControlFlow::WaitUntil(std::time::Instant::now() + Duration::from_secs(2));

        if let Event::NewEvents(_) = event {
            status_item.set_text(status_line());
        }

        if let Ok(event) = menu_channel.try_recv() {
            if event.id == edit_config_id {
                open_config();
            } else if event.id == restart_id {
                restart_app();
            } else if event.id == quit_id {
                *control_flow = ControlFlow::Exit;
            }
        }
    });
}
