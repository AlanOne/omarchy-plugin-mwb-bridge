#!/usr/bin/env python3
"""Generates "Slovenian (Windows).keylayout" from Microsoft's own definition of
the Windows Slovenian layout (kbdcr.klc, from kbdlayout.info, generated from
kbdcr.dll's KBDTABLES).

Apple's built-in "Slovenian" layout is a different layout altogether
(QWERTY, different punctuation and Option characters), so with it the Windows
keyboard typed the wrong characters through the bridge. This layout puts
exactly the Windows characters, AltGr layer and dead keys on the Mac key codes
the bridge posts for each Windows key (scancode == evdev code for every key
here, then `keycode_macos.rs`'s evdev -> CGKeyCode table).

Run from this directory:  python3 gen_keylayout.py
"""

import re
from pathlib import Path

HERE = Path(__file__).resolve().parent
KLC = HERE / "kbdcr.klc"
KEYCODES_RS = HERE.parent.parent / "src" / "keycode_macos.rs"
OUT = HERE / "Slovenian (Windows).keylayout"

NAME = "Slovenian (Windows)"
LAYOUT_ID = -28524  # any negative id not used by another installed layout


def evdev_to_cgkeycode():
    src = KEYCODES_RS.read_text()
    table = {}
    for a, b in re.findall(r"(\d+)\s*=>\s*0x([0-9A-Fa-f]+)", src):
        table[int(a)] = int(b, 16)
    return table


def klc_char(field):
    """KLC cell -> (char, is_dead) or None."""
    if field in ("", "-1"):
        return None
    dead = field.endswith("@")
    field = field.rstrip("@")
    char = field if len(field) == 1 else chr(int(field, 16))
    return char, dead


def parse_klc():
    text = KLC.read_text()
    keys = []
    layout = text.split("\nLAYOUT", 1)[1].split("\nDEADKEY", 1)[0]
    for line in layout.splitlines():
        m = re.match(r"^([0-9a-f]{2})\t(\S+)\t+(\d)\t(.*)$", line)
        if not m:
            continue
        cols = re.split(r"\t+", m.group(4).split("//")[0].strip())
        cols += [""] * (5 - len(cols))
        keys.append({
            "scancode": int(m.group(1), 16),
            "vk": m.group(2),
            "caps": m.group(3) == "1",
            "base": klc_char(cols[0]),
            "shift": klc_char(cols[1]),
            "altgr": klc_char(cols[2]),
            "ctrl": klc_char(cols[3]),
        })
    # The numpad decimal key is listed with the main block; it types a comma.
    deadkeys = {}
    for block in text.split("\nDEADKEY")[1:]:
        lines = block.strip().splitlines()
        dead = chr(int(lines[0].split()[0], 16))
        table = {}
        for line in lines[1:]:
            m = re.match(r"^([0-9a-f]{4})\t([0-9a-f]{4})", line)
            if m:
                table[chr(int(m.group(1), 16))] = chr(int(m.group(2), 16))
            elif line.strip() and not line.startswith("//"):
                break
        deadkeys[dead] = table
    return keys, deadkeys


# Keys the KLC doesn't describe as characters, same outputs Apple's own layouts use.
SPECIAL = {
    0x24: "\r", 0x30: "\t", 0x33: "\x08", 0x35: "\x1b", 0x47: "\x1b", 0x4C: "\x03", 0x34: "\x03",
    0x72: "\x05", 0x73: "\x01", 0x74: "\x0b", 0x75: "\x7f", 0x77: "\x04", 0x79: "\x0c",
    0x7B: "\x1c", 0x7C: "\x1d", 0x7D: "\x1f", 0x7E: "\x1e",
    0x52: "0", 0x53: "1", 0x54: "2", 0x55: "3", 0x56: "4", 0x57: "5", 0x58: "6", 0x59: "7",
    0x5B: "8", 0x5C: "9", 0x43: "*", 0x45: "+", 0x4E: "-", 0x4B: "/", 0x51: "=",
}
FUNCTION_KEYS = [0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F,
                 0x69, 0x6B, 0x71, 0x6A, 0x40, 0x4F, 0x50, 0x5A]
for code in FUNCTION_KEYS:
    SPECIAL[code] = "\x10"

# keyMap indices
BASE, SHIFT, CAPS, SHIFT_CAPS, ALTGR, ALTGR_SHIFT, CONTROL = range(7)
MODIFIER_MAP = [
    (BASE, "command?"),
    (SHIFT, "anyShift command?"),
    (CAPS, "caps command?"),
    (SHIFT_CAPS, "anyShift caps command?"),
    (ALTGR, "caps? anyOption"),
    (ALTGR_SHIFT, "anyShift caps? anyOption"),
    (CONTROL, "anyShift? caps? anyOption? command? control"),
]


def xml_escape(s):
    out = []
    for c in s:
        if c in "&<>\"'" or ord(c) < 0x20 or ord(c) == 0x7F:
            out.append(f"&#x{ord(c):04X};")
        else:
            out.append(c)
    return "".join(out)


def main():
    keys, deadkeys = parse_klc()
    ev2cg = evdev_to_cgkeycode()
    dead_ids = {d: f"dead_{ord(d):04x}" for d in deadkeys}

    # keyMap index -> CGKeyCode -> (char, is_dead)
    maps = {i: {} for i, _ in MODIFIER_MAP}
    for k in keys:
        cg = ev2cg.get(k["scancode"])
        if cg is None:
            raise SystemExit(f"no CGKeyCode for scancode {k['scancode']:#x} ({k['vk']}) in keycode_macos.rs")
        base, shift = k["base"], k["shift"]
        maps[BASE][cg] = base
        maps[SHIFT][cg] = shift
        maps[CAPS][cg] = shift if k["caps"] else base
        maps[SHIFT_CAPS][cg] = base if k["caps"] else shift
        altgr = k["altgr"]
        maps[ALTGR][cg] = altgr
        maps[ALTGR_SHIFT][cg] = altgr
        ctrl = k["ctrl"]
        if base and not base[1] and "a" <= base[0] <= "z":
            ctrl = (chr(ord(base[0]) - 0x60), False)
        maps[CONTROL][cg] = ctrl or (base if base and not base[1] else None)
    for i in maps:
        for code, out in SPECIAL.items():
            maps[i].setdefault(code, (out, False))

    # Every character some dead key composes with needs an action, so the
    # dead state can replace it with the composed character.
    composes = {}
    for dead, table in deadkeys.items():
        for base_char, composed in table.items():
            composes.setdefault(base_char, {})[dead] = composed

    def key_xml(code, out):
        if out is None:
            return f'\t\t\t<key code="{code}" output=""/>'
        char, dead = out
        if dead:
            return f'\t\t\t<key code="{code}" action="{dead_ids[char]}"/>'
        if char in composes:
            return f'\t\t\t<key code="{code}" action="c_{ord(char):04x}"/>'
        return f'\t\t\t<key code="{code}" output="{xml_escape(char)}"/>'

    lines = [
        '<?xml version="1.1" encoding="UTF-8"?>',
        '<!DOCTYPE keyboard SYSTEM "file://localhost/System/Library/DTDs/KeyboardLayout.dtd">',
        f"<!-- Generated by gen_keylayout.py from kbdcr.klc (Windows Slovenian). Do not edit by hand. -->",
        f'<keyboard group="126" id="{LAYOUT_ID}" name="{NAME}" maxout="2">',
        "\t<layouts>",
        '\t\t<layout first="0" last="255" modifiers="mods" mapSet="maps"/>',
        "\t</layouts>",
        '\t<modifierMap id="mods" defaultIndex="0">',
    ]
    for index, keys_spec in MODIFIER_MAP:
        lines.append(f'\t\t<keyMapSelect mapIndex="{index}"><modifier keys="{keys_spec}"/></keyMapSelect>')
    lines += ["\t</modifierMap>", '\t<keyMapSet id="maps">']
    for index, _ in MODIFIER_MAP:
        lines.append(f'\t\t<keyMap index="{index}">')
        for code in sorted(maps[index]):
            lines.append(key_xml(code, maps[index][code]))
        lines.append("\t\t</keyMap>")
    lines += ["\t</keyMapSet>", "\t<actions>"]
    for dead, ident in dead_ids.items():
        lines.append(f'\t\t<action id="{ident}">')
        lines.append(f'\t\t\t<when state="none" next="{ident}"/>')
        # A dead key pressed after another dead key types the first one's
        # accent and starts over, like Windows.
        lines.append(f'\t\t\t<when state="{ident}" output="{xml_escape(dead)}"/>')
        lines.append("\t\t</action>")
    for char, by_dead in sorted(composes.items()):
        lines.append(f'\t\t<action id="c_{ord(char):04x}">')
        lines.append(f'\t\t\t<when state="none" output="{xml_escape(char)}"/>')
        for dead, composed in by_dead.items():
            lines.append(f'\t\t\t<when state="{dead_ids[dead]}" output="{xml_escape(composed)}"/>')
        lines.append("\t\t</action>")
    lines += ["\t</actions>", "\t<terminators>"]
    for dead, ident in dead_ids.items():
        lines.append(f'\t\t<when state="{ident}" output="{xml_escape(dead)}"/>')
    lines += ["\t</terminators>", "</keyboard>", ""]
    OUT.write_text("\n".join(lines), encoding="utf-8")
    print(f"wrote {OUT.name}: {len(keys)} character keys, {len(deadkeys)} dead keys")


if __name__ == "__main__":
    main()
