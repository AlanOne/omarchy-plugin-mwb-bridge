// Windows virtual-key code -> Linux evdev keycode translation. Covers
// letters, digits, common punctuation, navigation, function keys, and
// modifiers — enough for real typing, not yet a complete VK table (numpad
// and less-common OEM keys are missing; add them here if/when needed).
//
// MWB's Kd.wVk carries the raw Windows VK code (winuser.h); the Wayland
// virtual-keyboard protocol wants the raw Linux evdev code (from
// linux/input-event-codes.h) — NOT the XKB keycode, which is evdev + 8.

pub fn vk_to_evdev(vk: u32) -> Option<u32> {
    Some(match vk {
        0x08 => 14,  // VK_BACK -> KEY_BACKSPACE
        0x09 => 15,  // VK_TAB
        0x0D => 28,  // VK_RETURN -> KEY_ENTER
        0x10 | 0xA0 => 42, // VK_SHIFT / VK_LSHIFT
        0xA1 => 54,  // VK_RSHIFT
        0x11 | 0xA2 => 29, // VK_CONTROL / VK_LCONTROL
        0xA3 => 97,  // VK_RCONTROL
        0x12 | 0xA4 => 56, // VK_MENU (Alt) / VK_LMENU
        0xA5 => 100, // VK_RMENU
        0x1B => 1,   // VK_ESCAPE
        0x20 => 57,  // VK_SPACE
        0x21 => 104, // VK_PRIOR -> KEY_PAGEUP
        0x22 => 109, // VK_NEXT -> KEY_PAGEDOWN
        0x23 => 107, // VK_END
        0x24 => 102, // VK_HOME
        0x25 => 105, // VK_LEFT
        0x26 => 103, // VK_UP
        0x27 => 106, // VK_RIGHT
        0x28 => 108, // VK_DOWN
        0x2D => 110, // VK_INSERT
        0x2E => 111, // VK_DELETE
        0x14 => 58,  // VK_CAPITAL -> KEY_CAPSLOCK

        // '0'-'9' (VK codes match ASCII digits)
        0x30 => 11, 0x31 => 2, 0x32 => 3, 0x33 => 4, 0x34 => 5,
        0x35 => 6, 0x36 => 7, 0x37 => 8, 0x38 => 9, 0x39 => 10,

        // 'A'-'Z' (VK codes match ASCII uppercase letters). Windows reports
        // VK by printed-letter intent (via its active layout), not raw
        // physical position, so on a QWERTZ layout (e.g. Slovenian, matching
        // this machine's XKB layout) Y and Z must map to each other's
        // US-QWERTY physical evdev slot for the two layout-driven swaps to
        // cancel out correctly.
        0x41 => 30, 0x42 => 48, 0x43 => 46, 0x44 => 32, 0x45 => 18,
        0x46 => 33, 0x47 => 34, 0x48 => 35, 0x49 => 23, 0x4A => 36,
        0x4B => 37, 0x4C => 38, 0x4D => 50, 0x4E => 49, 0x4F => 24,
        0x50 => 25, 0x51 => 16, 0x52 => 19, 0x53 => 31, 0x54 => 20,
        0x55 => 22, 0x56 => 47, 0x57 => 17, 0x58 => 45, 0x59 => 44,
        0x5A => 21,

        0x5B => 125, // VK_LWIN -> KEY_LEFTMETA
        0x5C => 126, // VK_RWIN -> KEY_RIGHTMETA

        // F1-F12
        0x70 => 59, 0x71 => 60, 0x72 => 61, 0x73 => 62, 0x74 => 63,
        0x75 => 64, 0x76 => 65, 0x77 => 66, 0x78 => 67, 0x79 => 68,
        0x7A => 87, 0x7B => 88,

        // Common OEM/punctuation keys (US layout)
        0xBA => 39, // VK_OEM_1 ; :
        0xBB => 13, // VK_OEM_PLUS = +
        0xBC => 51, // VK_OEM_COMMA , <
        0xBD => 12, // VK_OEM_MINUS - _
        0xBE => 52, // VK_OEM_PERIOD . >
        // VK_OEM_2: on this user's Slovenian Windows layout, the driver
        // assigns this VK to the physical key that produces apostrophe
        // under the matching "si" XKB layout (evdev 12), not the US "/" key
        // (evdev 53) — confirmed empirically, not from the US-centric VK
        // name. Windows layout DLLs can reassign OEM VK<->scancode per
        // layout, unlike the base A-Z/0-9 keys.
        0xBF => 12,
        0xC0 => 41, // VK_OEM_3 ` ~
        0xDB => 26, // VK_OEM_4 [ {
        0xDC => 43, // VK_OEM_5 \ |
        0xDD => 27, // VK_OEM_6 ] }
        0xDE => 40, // VK_OEM_7 ' "

        _ => return None,
    })
}
