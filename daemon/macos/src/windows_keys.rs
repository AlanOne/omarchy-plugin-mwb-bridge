// "Windows-style keys": makes the Windows keyboard behave on this Mac the way
// it does on Windows, instead of the plain physical-position forwarding
// `keycode_macos.rs` does on its own (Ctrl -> Control, Win -> Command).
//
// - Ctrl and Win swap roles, so Ctrl+C/V/X/Z/A/S/F/T/W/... hit the Command
//   shortcuts. Terminals are the exception: there Ctrl stays a real Control
//   key (Ctrl+C interrupts, Ctrl+R searches history), and Ctrl+Shift+C/V copy
//   and paste the way Windows Terminal does.
// - Text navigation: Home/End (line), Ctrl+Home/End (document), Ctrl+arrows
//   (word/paragraph), Ctrl+Backspace/Delete (word).
// - Alt+Tab app switching, Alt+F4 quit, a lone Win tap opens Spotlight,
//   Win+E opens a Finder window, Win+Shift+S snips a region to the clipboard.
// - Finder: Enter opens, Backspace goes up a folder, Delete moves to Trash,
//   F2 renames.
//
// The rules themselves (`rewrite`) are a pure function so they can be unit
// tested; `CgInput` owns the per-keypress state and does the posting.
// Toggled live from the menu bar (`ENABLED`), persisted as `windows_keys` in
// config.json.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::event::CGEventFlags;

use mwb_protocol::input_handling::ModifierState;

pub static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

// evdev codes (the space `InputSink::key` works in)
pub const EV_BACKSPACE: u32 = 14;
pub const EV_TAB: u32 = 15;
pub const EV_ENTER: u32 = 28;
pub const EV_LEFTCTRL: u32 = 29;
pub const EV_RIGHTCTRL: u32 = 97;
pub const EV_LEFTMETA: u32 = 125;
pub const EV_RIGHTMETA: u32 = 126;
pub const EV_LEFTALT: u32 = 56;
pub const EV_RIGHTALT: u32 = 100;
pub const EV_LEFTSHIFT: u32 = 42;
pub const EV_RIGHTSHIFT: u32 = 54;
pub const EV_CAPSLOCK: u32 = 58;
const EV_C: u32 = 46;
const EV_V: u32 = 47;
const EV_E: u32 = 18;
const EV_S: u32 = 31;
const EV_F2: u32 = 60;
const EV_F4: u32 = 62;
const EV_HOME: u32 = 102;
const EV_END: u32 = 107;
const EV_UP: u32 = 103;
const EV_LEFT: u32 = 105;
const EV_RIGHT: u32 = 106;
const EV_DOWN: u32 = 108;
const EV_DELETE: u32 = 111;

// CGKeyCodes (kVK_*)
pub const VK_COMMAND: u16 = 0x37;
pub const VK_RIGHT_COMMAND: u16 = 0x36;
pub const VK_CONTROL: u16 = 0x3B;
pub const VK_RIGHT_CONTROL: u16 = 0x3E;
pub const VK_TAB: u16 = 0x30;
pub const VK_SPACE: u16 = 0x31;
const VK_RETURN: u16 = 0x24;
const VK_DELETE: u16 = 0x33; // Backspace
const VK_FORWARD_DELETE: u16 = 0x75;
const VK_LEFT: u16 = 0x7B;
const VK_RIGHT: u16 = 0x7C;
const VK_DOWN: u16 = 0x7D;
const VK_UP: u16 = 0x7E;
const VK_C: u16 = 0x08;
const VK_V: u16 = 0x09;
const VK_Q: u16 = 0x0C;
const VK_4: u16 = 0x15;

pub fn is_modifier(evdev: u32) -> bool {
    matches!(
        evdev,
        EV_LEFTCTRL | EV_RIGHTCTRL | EV_LEFTMETA | EV_RIGHTMETA | EV_LEFTALT | EV_RIGHTALT | EV_LEFTSHIFT
            | EV_RIGHTSHIFT | EV_CAPSLOCK
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppKind {
    Terminal,
    Finder,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Forward unchanged (with the usual Ctrl/Win-swapped flags).
    Raw,
    /// Post this key with exactly these flags instead.
    Key(u16, CGEventFlags),
    /// Alt+Tab: Command+Tab, held open while Alt stays down.
    AppSwitch { reverse: bool },
    /// Post this key as a system-wide shortcut (Session tap level — see
    /// `cg_input::lock_screen` for why those need it).
    SystemShortcut(u16, CGEventFlags),
    OpenFinderWindow,
}

/// The keys a rule can apply to with no modifiers held. Everything else with
/// no modifiers held is forwarded as-is without looking up the focused app.
pub fn needs_context(evdev: u32, mods: ModifierState) -> bool {
    mods.ctrl
        || mods.alt
        || mods.super_
        || matches!(evdev, EV_HOME | EV_END | EV_ENTER | EV_BACKSPACE | EV_DELETE | EV_F2)
}

/// Decides what a non-modifier keypress becomes. `in_text` is asked only
/// when a rule depends on whether the caret is in an editable text field
/// (it costs an Accessibility round trip to the focused app).
pub fn rewrite(evdev: u32, mods: ModifierState, app: AppKind, in_text: &mut dyn FnMut() -> bool) -> Action {
    let shift = if mods.shift { CGEventFlags::CGEventFlagShift } else { CGEventFlags::CGEventFlagNull };
    let cmd = CGEventFlags::CGEventFlagCommand;
    let opt = CGEventFlags::CGEventFlagAlternate;
    let only_ctrl = mods.ctrl && !mods.alt && !mods.super_ && !mods.level3;
    let only_alt = mods.alt && !mods.ctrl && !mods.super_ && !mods.level3;
    let only_win = mods.super_ && !mods.ctrl && !mods.alt && !mods.level3;
    let none = !mods.ctrl && !mods.alt && !mods.super_ && !mods.level3;

    // Everywhere, terminals included
    if only_alt && evdev == EV_TAB {
        return Action::AppSwitch { reverse: mods.shift };
    }
    if only_alt && evdev == EV_F4 && !mods.shift {
        return Action::Key(VK_Q, cmd);
    }
    if only_win && evdev == EV_E && !mods.shift {
        return Action::OpenFinderWindow;
    }
    if only_win && evdev == EV_S && mods.shift {
        return Action::SystemShortcut(
            VK_4,
            cmd | CGEventFlags::CGEventFlagShift | CGEventFlags::CGEventFlagControl,
        );
    }

    if app == AppKind::Terminal {
        if only_ctrl && mods.shift && evdev == EV_C {
            return Action::Key(VK_C, cmd);
        }
        if only_ctrl && mods.shift && evdev == EV_V {
            return Action::Key(VK_V, cmd);
        }
        return Action::Raw;
    }

    if only_ctrl {
        match evdev {
            EV_LEFT => return Action::Key(VK_LEFT, opt | shift),
            EV_RIGHT => return Action::Key(VK_RIGHT, opt | shift),
            EV_UP => return Action::Key(VK_UP, opt | shift),
            EV_DOWN => return Action::Key(VK_DOWN, opt | shift),
            EV_BACKSPACE => return Action::Key(VK_DELETE, opt),
            EV_DELETE => return Action::Key(VK_FORWARD_DELETE, opt),
            EV_HOME => return Action::Key(VK_UP, cmd | shift),
            EV_END => return Action::Key(VK_DOWN, cmd | shift),
            _ => {}
        }
    }

    if none && matches!(evdev, EV_HOME | EV_END) {
        // Outside a text field, Mac Home/End already scroll to the top/bottom
        // the way Windows does — and Command+Left in a browser page is Back.
        if in_text() {
            let key = if evdev == EV_HOME { VK_LEFT } else { VK_RIGHT };
            return Action::Key(key, cmd | shift);
        }
        return Action::Raw;
    }

    if app == AppKind::Finder && none && !mods.shift {
        let finder_key = match evdev {
            EV_ENTER => Some((VK_DOWN, cmd)),
            EV_BACKSPACE => Some((VK_UP, cmd)),
            EV_DELETE => Some((VK_DELETE, cmd)),
            EV_F2 => Some((VK_RETURN, CGEventFlags::CGEventFlagNull)),
            _ => None,
        };
        // In the rename field or the search box these keys edit text.
        if let Some((key, flags)) = finder_key {
            if !in_text() {
                return Action::Key(key, flags);
            }
        }
    }

    Action::Raw
}

// --- Focused app / focused element, via the Accessibility API -------------
// The bridge already holds the Accessibility grant (CGEventPost needs it), so
// these queries need no extra permission.

type AXUIElementRef = *const c_void;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(element: AXUIElementRef, attribute: CFStringRef, value: *mut CFTypeRef) -> i32;
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> i32;
    fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, timeout_seconds: f32) -> i32;
}

// A hung app would otherwise block an Accessibility query (and with it, all
// input forwarding) for the system default of several seconds.
const AX_TIMEOUT_SECONDS: f32 = 0.1;

fn system_wide() -> CFType {
    unsafe {
        let el = AXUIElementCreateSystemWide();
        AXUIElementSetMessagingTimeout(el, AX_TIMEOUT_SECONDS);
        CFType::wrap_under_create_rule(el)
    }
}

fn copy_attribute(element: &CFType, name: &str) -> Option<CFType> {
    let attr = CFString::new(name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = unsafe { AXUIElementCopyAttributeValue(element.as_CFTypeRef(), attr.as_concrete_TypeRef(), &mut value) };
    if err != 0 || value.is_null() {
        return None;
    }
    Some(unsafe { CFType::wrap_under_create_rule(value) })
}

fn executable_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    buf.truncate(len as usize);
    String::from_utf8(buf).ok()
}

const TERMINAL_APPS: &[&str] = &[
    "/Ghostty.app/",
    "/Terminal.app/",
    "/iTerm.app/",
    "/WezTerm.app/",
    "/Alacritty.app/",
    "/kitty.app/",
    "/Warp.app/",
];

pub fn classify_app_path(path: &str) -> AppKind {
    if TERMINAL_APPS.iter().any(|t| path.contains(t)) {
        AppKind::Terminal
    } else if path.contains("/Finder.app/") {
        AppKind::Finder
    } else {
        AppKind::Other
    }
}

/// The app that currently has keyboard focus.
pub fn focused_app() -> AppKind {
    let Some(app) = copy_attribute(&system_wide(), "AXFocusedApplication") else {
        return AppKind::Other;
    };
    let mut pid = 0;
    if unsafe { AXUIElementGetPid(app.as_CFTypeRef(), &mut pid) } != 0 {
        return AppKind::Other;
    }
    executable_path(pid).map(|p| classify_app_path(&p)).unwrap_or(AppKind::Other)
}

/// Whether the focused element is an editable text field. Apps that don't
/// expose their UI to Accessibility (some Electron apps) read as "not in
/// text", which only means Home/End keep their plain Mac behavior there.
pub fn focus_is_text() -> bool {
    let Some(el) = copy_attribute(&system_wide(), "AXFocusedUIElement") else {
        return false;
    };
    let Some(role) = copy_attribute(&el, "AXRole") else {
        return false;
    };
    let Some(role) = role.downcast::<CFString>() else {
        return false;
    };
    matches!(role.to_string().as_str(), "AXTextField" | "AXTextArea" | "AXComboBox")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(shift: bool, ctrl: bool, alt: bool, win: bool) -> ModifierState {
        ModifierState { shift, ctrl, alt, super_: win, level3: false }
    }
    const NONE: ModifierState = ModifierState { shift: false, ctrl: false, alt: false, super_: false, level3: false };
    const CMD: CGEventFlags = CGEventFlags::CGEventFlagCommand;
    const OPT: CGEventFlags = CGEventFlags::CGEventFlagAlternate;
    const SHIFT: CGEventFlags = CGEventFlags::CGEventFlagShift;

    fn r(evdev: u32, m: ModifierState, app: AppKind, text: bool) -> Action {
        rewrite(evdev, m, app, &mut || text)
    }

    #[test]
    fn plain_letters_and_shortcuts_pass_through() {
        assert_eq!(r(EV_C, NONE, AppKind::Other, true), Action::Raw);
        // Ctrl+C itself is handled by the Ctrl->Command swap, not a rule
        assert_eq!(r(EV_C, mods(false, true, false, false), AppKind::Other, true), Action::Raw);
        assert!(!needs_context(EV_C, NONE));
    }

    #[test]
    fn home_end_only_move_the_caret_in_text_fields() {
        assert_eq!(r(EV_HOME, NONE, AppKind::Other, true), Action::Key(VK_LEFT, CMD));
        assert_eq!(r(EV_END, mods(true, false, false, false), AppKind::Other, true), Action::Key(VK_RIGHT, CMD | SHIFT));
        assert_eq!(r(EV_HOME, NONE, AppKind::Other, false), Action::Raw);
        assert_eq!(r(EV_END, mods(false, true, false, false), AppKind::Other, false), Action::Key(VK_DOWN, CMD));
    }

    #[test]
    fn ctrl_word_navigation_becomes_option() {
        let ctrl = mods(false, true, false, false);
        let ctrl_shift = mods(true, true, false, false);
        assert_eq!(r(EV_LEFT, ctrl, AppKind::Other, true), Action::Key(VK_LEFT, OPT));
        assert_eq!(r(EV_RIGHT, ctrl_shift, AppKind::Other, true), Action::Key(VK_RIGHT, OPT | SHIFT));
        assert_eq!(r(EV_BACKSPACE, ctrl, AppKind::Other, true), Action::Key(VK_DELETE, OPT));
        assert_eq!(r(EV_DELETE, ctrl, AppKind::Other, true), Action::Key(VK_FORWARD_DELETE, OPT));
    }

    #[test]
    fn terminals_keep_ctrl_and_use_ctrl_shift_for_clipboard() {
        let ctrl = mods(false, true, false, false);
        let ctrl_shift = mods(true, true, false, false);
        assert_eq!(r(EV_C, ctrl, AppKind::Terminal, true), Action::Raw);
        assert_eq!(r(EV_LEFT, ctrl, AppKind::Terminal, true), Action::Raw);
        assert_eq!(r(EV_HOME, NONE, AppKind::Terminal, true), Action::Raw);
        assert_eq!(r(EV_C, ctrl_shift, AppKind::Terminal, true), Action::Key(VK_C, CMD));
        assert_eq!(r(EV_V, ctrl_shift, AppKind::Terminal, true), Action::Key(VK_V, CMD));
    }

    #[test]
    fn alt_and_win_shortcuts() {
        assert_eq!(r(EV_TAB, mods(false, false, true, false), AppKind::Other, false), Action::AppSwitch { reverse: false });
        assert_eq!(r(EV_TAB, mods(true, false, true, false), AppKind::Terminal, false), Action::AppSwitch { reverse: true });
        assert_eq!(r(EV_F4, mods(false, false, true, false), AppKind::Other, false), Action::Key(VK_Q, CMD));
        assert_eq!(r(EV_E, mods(false, false, false, true), AppKind::Other, false), Action::OpenFinderWindow);
        assert!(matches!(r(EV_S, mods(true, false, false, true), AppKind::Other, false), Action::SystemShortcut(VK_4, _)));
        // AltGr characters (level3) are never treated as Alt shortcuts
        let altgr = ModifierState { level3: true, ..NONE };
        assert_eq!(r(EV_TAB, altgr, AppKind::Other, false), Action::Raw);
    }

    #[test]
    fn finder_keys_unless_renaming() {
        assert_eq!(r(EV_ENTER, NONE, AppKind::Finder, false), Action::Key(VK_DOWN, CMD));
        assert_eq!(r(EV_BACKSPACE, NONE, AppKind::Finder, false), Action::Key(VK_UP, CMD));
        assert_eq!(r(EV_DELETE, NONE, AppKind::Finder, false), Action::Key(VK_DELETE, CMD));
        assert_eq!(r(EV_F2, NONE, AppKind::Finder, false), Action::Key(VK_RETURN, CGEventFlags::CGEventFlagNull));
        assert_eq!(r(EV_ENTER, NONE, AppKind::Finder, true), Action::Raw);
        assert_eq!(r(EV_ENTER, NONE, AppKind::Other, false), Action::Raw);
    }

    #[test]
    fn app_classification() {
        assert_eq!(classify_app_path("/Applications/Ghostty.app/Contents/MacOS/ghostty"), AppKind::Terminal);
        assert_eq!(classify_app_path("/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder"), AppKind::Finder);
        assert_eq!(classify_app_path("/Applications/Safari.app/Contents/MacOS/Safari"), AppKind::Other);
    }
}
