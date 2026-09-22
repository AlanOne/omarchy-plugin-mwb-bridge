// Linux evdev keycode -> macOS CGKeyCode translation, for standard ANSI
// physical key positions.
//
// Both evdev and CGKeyCode are *positional* (hardware-scancode-style)
// identifiers, independent of whatever software keyboard layout is active —
// exactly like evdev, CGKeyCode 0x00 is always "the key physically where A
// sits on a US ANSI keyboard," regardless of whether the active input
// source is US, Slovenian, or anything else; the OS's own selected input
// source is what turns that physical position into an actual character,
// same division of responsibility XKB has on the Linux side. This means
// `vk_to_evdev` (shared with Linux, in `mwb_protocol::vk_keycode`) — which
// already encodes the Windows-VK-is-printed-intent quirks (the QWERTZ Y/Z
// swap, VK_OEM_2's per-layout reassignment, etc.) as physical-position
// evdev codes — is fully reusable as-is: this table only needs to be a
// static, layout-independent remap from evdev's physical-position numbering
// to Apple's, not its own copy of the Windows-layout research.
//
// Values are the standard Carbon/HIToolbox `kVK_*` constants (stable across
// macOS releases, unchanged since Mac OS X's introduction) — not yet
// verified against this specific keyboard/live traffic, only cross-checked
// against the well-known table; treat non-letter/digit/modifier/arrow keys
// as needing the same live-verification pass the Linux build's punctuation
// mapping got if something looks wrong.
pub fn evdev_to_cgkeycode(evdev: u32) -> Option<u16> {
    Some(match evdev {
        14 => 0x33, // KEY_BACKSPACE -> kVK_Delete
        15 => 0x30, // KEY_TAB
        28 => 0x24, // KEY_ENTER -> kVK_Return
        42 => 0x38, // KEY_LEFTSHIFT -> kVK_Shift
        54 => 0x3C, // KEY_RIGHTSHIFT
        29 => 0x3B, // KEY_LEFTCTRL -> kVK_Control
        97 => 0x3E, // KEY_RIGHTCTRL
        56 => 0x3A, // KEY_LEFTALT -> kVK_Option
        100 => 0x3D, // KEY_RIGHTALT
        1 => 0x35,  // KEY_ESC -> kVK_Escape
        57 => 0x31, // KEY_SPACE
        104 => 0x74, // KEY_PAGEUP
        109 => 0x79, // KEY_PAGEDOWN
        107 => 0x77, // KEY_END
        102 => 0x73, // KEY_HOME
        105 => 0x7B, // KEY_LEFT
        103 => 0x7E, // KEY_UP
        106 => 0x7C, // KEY_RIGHT
        108 => 0x7D, // KEY_DOWN
        // No true Insert key on Mac keyboards; kVK_Help occupies that
        // physical position on extended keyboards. Best available match.
        110 => 0x72, // KEY_INSERT -> kVK_Help
        111 => 0x75, // KEY_DELETE -> kVK_ForwardDelete
        58 => 0x39,  // KEY_CAPSLOCK

        // digits (main row)
        11 => 0x1D, // 0
        2 => 0x12,  // 1
        3 => 0x13,  // 2
        4 => 0x14,  // 3
        5 => 0x15,  // 4
        6 => 0x17,  // 5
        7 => 0x16,  // 6
        8 => 0x1A,  // 7
        9 => 0x1C,  // 8
        10 => 0x19, // 9

        // letters
        30 => 0x00, // A
        48 => 0x0B, // B
        46 => 0x08, // C
        32 => 0x02, // D
        18 => 0x0E, // E
        33 => 0x03, // F
        34 => 0x05, // G
        35 => 0x04, // H
        23 => 0x22, // I
        36 => 0x26, // J
        37 => 0x28, // K
        38 => 0x25, // L
        50 => 0x2E, // M
        49 => 0x2D, // N
        24 => 0x1F, // O
        25 => 0x23, // P
        16 => 0x0C, // Q
        19 => 0x0F, // R
        31 => 0x01, // S
        20 => 0x11, // T
        22 => 0x20, // U
        47 => 0x09, // V
        17 => 0x0D, // W
        45 => 0x07, // X
        21 => 0x10, // Y
        44 => 0x06, // Z

        125 => 0x37, // KEY_LEFTMETA -> kVK_Command
        126 => 0x36, // KEY_RIGHTMETA -> kVK_RightCommand

        // F1-F12
        59 => 0x7A, 60 => 0x78, 61 => 0x63, 62 => 0x76,
        63 => 0x60, 64 => 0x61, 65 => 0x62, 66 => 0x64,
        67 => 0x65, 68 => 0x6D, 87 => 0x67, 88 => 0x6F,

        // numpad
        82 => 0x52, // KP0
        79 => 0x53, // KP1
        80 => 0x54, // KP2
        81 => 0x55, // KP3
        75 => 0x56, // KP4
        76 => 0x57, // KP5
        77 => 0x58, // KP6
        71 => 0x59, // KP7
        72 => 0x5B, // KP8
        73 => 0x5C, // KP9
        55 => 0x43, // KP*
        78 => 0x45, // KP+
        74 => 0x4E, // KP-
        83 => 0x41, // KP.
        98 => 0x4B, // KP/
        // Mac keypads have no NumLock; kVK_ANSI_KeypadClear occupies that
        // physical position. Dead in practice — handle_keyboard intercepts
        // VK_NUMLOCK before it ever reaches InputSink::key — kept only for
        // table completeness.
        69 => 0x47,

        // punctuation (US physical positions, matching what vk_to_evdev
        // already resolves Windows' per-layout OEM VK reassignment to)
        39 => 0x29, // ; :
        13 => 0x18, // = +
        51 => 0x2B, // , <
        12 => 0x1B, // - _
        52 => 0x2F, // . >
        41 => 0x32, // ` ~
        26 => 0x21, // [ {
        43 => 0x2A, // \ |
        27 => 0x1E, // ] }
        40 => 0x27, // ' "
        53 => 0x2C, // / ?  (not currently produced by vk_to_evdev for this
                     // user's layout, kept for robustness)

        _ => return None,
    })
}
