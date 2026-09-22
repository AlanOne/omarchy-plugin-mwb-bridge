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
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

/// A plain filled square — a placeholder, not a designed icon. Good enough
/// to have *something* in the menu bar for milestone 1; revisit once the
/// bridge itself works.
fn placeholder_icon() -> Icon {
    const SIZE: u32 = 22;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for _ in 0..(SIZE * SIZE) {
        rgba.extend_from_slice(&[255u8, 255, 255, 255]);
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("valid fixed-size RGBA buffer")
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
    let event_loop = EventLoopBuilder::new().build();

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
        .with_icon(placeholder_icon())
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
