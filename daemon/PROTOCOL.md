# PowerToys Mouse Without Borders wire protocol (v0.98.1)

Reverse-engineered entirely from Microsoft's own MIT-licensed source
(`microsoft/PowerToys`, module `MouseWithoutBorders`, tag `v0.98.1` — **not**
the `main` branch, which has a materially different, newer crypto scheme;
always check the exact installed version's source, don't assume `main`
applies). Validated end-to-end against a real Windows 11 PC running
PowerToys 0.98.1, hostname `WINPC`, machine ID `123456789`.

## Ports

MWB runs **two separate TCP servers**, not one:

| Port | Purpose |
|---|---|
| `BASE_PORT` (default `15100`) | Clipboard server (`AcceptConnectionAndSendClipboardData`) |
| `BASE_PORT + 1` (`15101`) | **Message server** — Handshake, Mouse, Keyboard, Heartbeat, Matrix, everything else |

This is completely undocumented anywhere and was the single biggest time-sink
in this investigation — every mouse/keyboard/handshake packet lives on
`BASE_PORT + 1`. Confirmed via a PowerToys debug dump showing
`"Tcp Server: AcceptConnectionAndSendClipboardData [::]:15100"` and
`"TCP listening on port: 15101"` as two distinct log lines.

## Encryption (v0.98.1 specific — NOT the same as `main`)

`main`'s `Encryption.cs` uses a random per-connection salt+IV exchanged in
cleartext at the start of the stream (PBKDF2-SHA512, 100k iterations). This
is a **newer, unreleased-as-of-0.98.1 hardening change**. v0.98.1 instead
uses a **fixed, deterministic** key and IV — no per-connection handshake for
the crypto itself at all:

```
InitialIV = "18446744073709551615"          // literal decimal string of ulong.MaxValue

salt = UTF-16LE bytes of InitialIV            // 40 bytes (2 bytes/char)
key  = PBKDF2-HMAC-SHA512(security_key_utf8, salt, iterations=50000, output_len=32)

iv   = ASCII bytes of InitialIV[0..16]        // "1844674407370955", 16 bytes
```

AES-256-CBC, one continuous cipher stream per TCP direction (i.e. CBC
chaining continues across the *entire* connection lifetime, not reset per
packet — this is a single `CryptoStream` wrapping the socket for as long as
it's open). This means: to decrypt/encrypt packet N you need the ciphertext
of packet N-1's last block as the chaining value, not the original IV
(except for the very first block of the connection).

**Every new TCP connection, in both directions, starts with a throwaway
16-byte "priming" block** (`Common.SendOrReceiveARandomDataBlockPerInitialIV`)
sent/received *through the cipher* (not cleartext) immediately after the
socket connects, before any real packet. Its plaintext content is
meaningless — it exists purely to consume the first block. **Miss this and
every subsequent byte you read/write is shifted by 16 bytes and looks like
garbage** (this cost significant time — the framing looked broken when it
was actually just misaligned by exactly one block).

Sequence per direction, immediately on connect:
1. 16-byte priming block (garbage, discard).
2. Real packets begin.

No cleartext bytes are ever sent — both the priming block and everything
after it are ciphertext from byte 0.

## Packet framing

Fixed-size packets: `PACKAGE_SIZE = 32` bytes (most types) or
`PACKAGE_SIZE_EX = 64` bytes ("big" packages — see below). Always a multiple
of the 16-byte AES block size, so `PaddingMode.Zeros` on the .NET side never
actually adds padding in practice.

**Checksum/magic packing** (applies to the first 32 bytes only, even for a
64-byte package):

```
magicNumber = Get24BitHash(security_key)     // see below, computed once at startup

// On send:
byte[3] = (magicNumber >> 24) & 0xFF
byte[2] = (magicNumber >> 16) & 0xFF
byte[1] = sum(byte[2..32)) & 0xFF            // wrapping byte sum, includes byte[2] and byte[3] themselves

// On receive: verify byte[3..4] and byte[1] the same way, THEN zero
// byte[1], byte[2], byte[3] before treating byte[0] as a clean PackageType.
```

`Get24BitHash(key)` (used both for the magic number above, and unrelated to
the AES key derivation — a completely separate hash):

```
bytes[32] = key's characters truncated to a byte each (ASCII-safe: this is
            what CreateRandomKey()'s charset always produces), zero-padded
hash = SHA512(bytes)
repeat 50000 times: hash = SHA512(hash)      // 50001 total SHA512 rounds
magicNumber = (hash[0] << 23) | (hash[1] << 16) | (hash[63] << 8) | hash[2]
```

Verified byte-for-byte against a real PowerToys debug dump's
`magicNumber = 186073743` field (== `0x0b17428f`, exactly what this formula
produces for the real security key).

## DATA struct layout (byte offsets, little-endian)

A C# `[StructLayout(Explicit)]` union. Same layout for both 32- and 64-byte
packages; bytes 32-63 only exist/matter for "big" packages.

| Offset | Field | Notes |
|---|---|---|
| 0 | Type (1 byte used) | `PackageType` enum value; bytes 1-3 are checksum/magic on the wire, zeroed after validation |
| 4 | Id (u32) | sequence id, used for dedup on the receive side |
| 8 | Src (u32) | sender's machine ID — **see gotcha below** |
| 12 | Des (u32) | destination machine ID, or `ID.ALL` (255) to broadcast |
| 16 | Machine1 (u32) | Handshake: random challenge value. Mouse: X. Keyboard: unused (always 0 in observed traffic). |
| 20 | Machine2 (u32) | Handshake: random challenge value. Mouse: Y. Keyboard: unused. |
| 24 | Machine3 (u32) | Handshake: random challenge value. Mouse: WheelDelta. **Keyboard: wVk** (verified against real traffic — NOT offset 16 as originally assumed from reading the struct field order; the union's Keyboard fields apparently line up with Mouse's WheelDelta/dwFlags slots, not its X/Y slots). |
| 28 | Machine4 (u32) | Handshake: random challenge value. Mouse: dwFlags (raw Win32 `WM_*` constant). **Keyboard: dwFlags** (`WM.LLKHF.*` — bit 0x80 = key-up, matching `LLKHF_UP`). |
| 32-63 | MachineName | 32 bytes, space-padded (`PadRight(32, ' ')`), only present for "big" packages |

`ID.NONE = 0`, `ID.ALL = 255`. Any other `u32` is a valid machine ID — real
machines use large effectively-random-looking persisted values (e.g. real
`WINPC` = `123456789`).

### "Big" packages (get the extra 32-byte MachineName half)

`Hello(3)`, `Awake(21)`, `Heartbeat(20)`, `Heartbeat_ex(51)`, `Handshake(126)`,
`HandshakeAck(127)`, `ClipboardPush(79)`, `Clipboard(69)`, `ClipboardAsk(78)`,
`ClipboardImage(125)`, `ClipboardText(124)`, `ClipboardDataEnd(76)`, or any
type with the `Matrix` (128) bit set. Everything else (`Mouse=123`,
`Keyboard=122`, `Hi=2`, `ByeBye=4`, ...) is a plain 32-byte package.

## PackageType enum

```
Invalid=0xFF  Error=0xFE
Hi=2  Hello=3  ByeBye=4
Heartbeat=20  Awake=21  HideMouse=50  Heartbeat_ex=51  Heartbeat_ex_l2=52  Heartbeat_ex_l3=53
Clipboard=69  ClipboardDragDrop=70  ClipboardDragDropEnd=71  ExplorerDragDrop=72
ClipboardCapture=73  CaptureScreenCommand=74  ClipboardDragDropOperation=75
ClipboardDataEnd=76  MachineSwitched=77  ClipboardAsk=78  ClipboardPush=79
NextMachine=121  Keyboard=122  Mouse=123  ClipboardText=124  ClipboardImage=125
Handshake=126  HandshakeAck=127
Matrix=128  MatrixSwapFlag=2  MatrixTwoRowFlag=4   // OR'd with 128, not standalone values
```

## Handshake sequence

Symmetric — **both sides do this to each other independently**, right when
the connection is established (as the very first thing `MainTCPRoutine`
does, before it ever tries to read anything):

1. Build a Handshake packet: `Type=126`, `Src=<your own real machine ID —
   never 0>`, `Des=0`, `Machine1..4=<4 fresh random u32s>`,
   `MachineName=<your name, space-padded>`.
2. Send it **10 times** (verified: real PowerToys does exactly this,
   presumably historical redundancy from a less-reliable transport; harmless
   over TCP, the peer just processes it 10 times and ACKs 10 times).
3. Locally bit-flip your own copy of Machine1-4 (`~Machine1` etc.) — this is
   what you'll compare an incoming ack against.
4. Now enter the receive loop. Two things can arrive:
   - **A `Handshake` from the peer**: reply with `HandshakeAck` — same
     packet, `Type=127`, `Src=<your own ID>`, `Machine1..4` = the *peer's*
     values bit-flipped, `MachineName=<your name>`.
   - **A `HandshakeAck` from the peer**: verify its `Machine1..4` equals
     *your* locally-flipped values from step 3. If they match, the peer is
     now `SocketStatus.Connected` and `TcpSk.MachineId` gets set to
     whatever `Src` was on the packet that first got you to this success
     path.

### Critical gotcha: `Src` must never be `ID.NONE` (0)

The real code has `if (data.Src == ID.NONE) data.Src = Common.MachineID;` as
an automatic fallback in `TcpSend` — meaning the real app *never* actually
sends `Src=0`, even though it's tempting to leave it as a "don't care" value
when building packets by hand. **Sending `Src=0` in the Handshake means
Windows registers your `TcpSk.MachineId` as literally 0** — the handshake
still cryptographically succeeds (magic/checksum/challenge all validate
fine, "Connected to new machine X" toast fires, the machine gets added to
`MachinePool`), but every subsequent lookup that matches by machine ID
(`IsConnectedTo`, routing decisions for Mouse/Keyboard forwarding) will never
find you, since nothing else has ID 0. This produced a very convincing
false "it's basically working" signal — successful handshake, visible toast,
zero actual input forwarding — that took a long time to trace back to this
one field. **Always send a real, non-zero, non-255 machine ID as `Src` in
every packet**, not just the first one.

## What's confirmed working (this repo, tested end-to-end against a real PC)

- Full crypto (key/IV derivation, priming block, continuous CBC chaining).
- Full packet framing (checksum, magic number, big-package second half).
- Full handshake, both directions, byte-for-byte validated including the
  challenge-response match.
- `Common.MachineID`-style real ID usage (after fixing the `Src=0` bug).
- **Real Mouse and Keyboard forwarding, both directions of trust** (edge-
  crossing *and* the `Ctrl+Alt+F1`-style hotkey switch), fully working
  end-to-end: cursor movement, clicks, scroll wheel, and typing (including
  modifier keys) all land correctly on the Linux side. See below for what
  it actually took to get here — none of it was in the wire protocol.

## The real blocker was never the wire protocol — it was Windows-side state

Everything above (crypto/framing/handshake) was correct from early in the
investigation. Mouse/Keyboard packets still never arrived for a long time
after that, and the cause turned out to be entirely on the Windows/PowerToys
side, in state the classic UI is supposed to manage but doesn't reliably:

### The Matrix and the MachinePool are two different lists, and neither one gets updated by the UI in this version

- **`MachineMatrixString`** (the visual 4-slot "Computer Matrix" grid used
  for edge-crossing routing) and **`MachinePool`** (the list `SwitchToMachine`
  actually resolves a name against, used by the `Ctrl+Alt+F1`-style hotkeys)
  are separate settings, both stored in
  `%LOCALAPPDATA%\Microsoft\PowerToys\MouseWithoutBorders\settings.json`
  (this version does **not** use the registry, despite the original
  open-source app historically doing so).
- A successfully connected new machine gets a **toast** ("Connected to new
  machine X") but the source path for a not-yet-pooled machine
  (`SocketStuff.MainTCPRoutine`'s `else` branch when
  `MachinePool.TryFindMachineByName` fails) only shows that toast — it never
  actually calls anything that adds the machine to `MachinePool`. This looks
  like a genuine gap/bug in this version (0.98.1.0).
- The classic "Machine Setup" UI's "Apply" button (`ButtonOK_Click` in
  `frmMatrix.cs`) does read the checked-checkbox + typed name and calls
  `MachineStuff.MachineMatrix = st` (which does persist to
  `Setting.Values.MachineMatrixString` per its property setter — verified by
  reading the source), yet in practice this **never actually persisted**:
  confirmed by re-opening the dialog after Apply (entry reverted to empty),
  confirmed again via a fresh Mini Log dump (`Matrix:` line unchanged), and
  confirmed a third time via direct inspection of `settings.json` on disk
  after a full PowerToys restart. Multiple candidate slots were tried (the
  slot showing a live "Connected" badge, and genuinely blank slots) with the
  same result every time. The `MachinePool` has no UI path to edit it at
  all.
- **The toast lies.** `Switch2()` (the `Ctrl+Alt+Fn` hotkey handler in
  `InputHook.cs`) shows "Control has been switched to X" unconditionally
  whenever the matrix slot has a non-empty name in it — it does **not**
  check whether `SwitchToMachine` (which it calls) actually succeeded.
  `SwitchToMachine` silently no-ops if `MachinePool.ResolveID(name)` returns
  `ID.NONE`. This produced a second very convincing false-positive signal
  (successful-looking toast, zero actual effect — same shape as the earlier
  `Src=0` bug, just one layer up the stack) that cost significant time
  before being traced back to the missing `MachinePool` entry.
- **The actual working fix**: fully close PowerToys, hand-edit
  `settings.json` directly — set `MachineMatrixString` to include your
  machine's name in one slot (position encodes physical left/right layout
  for edge-crossing) and set `MachinePool` to
  `"<ExistingName>:<ExistingID>,<YourName>:<YourMachineId>,:,:"` (comma-
  separated `Name:ID` per slot, matching `MAX_MACHINE`=4) — then relaunch.
  Both changes now persist correctly across restarts once written this way;
  the bug is specifically in the UI's write path, not in settings
  persistence itself. `<YourMachineId>` must exactly match the fixed
  `Src`/machine-ID constant your daemon sends (decimal form of the same
  `u32`).
- One extra gotcha while debugging this: **copying a live Windows file/folder
  to a shared location can silently return a stale previous copy** if the
  copy mechanism doesn't force a real overwrite — burned significant time
  cross-referencing an old `settings.json` before catching this. Always
  check the file's own timestamp/mtime before trusting a "fresh" copy, or
  delete the destination first to force a real overwrite.

### DNS/IP resolution for edge-crossing routing

Once a machine is genuinely in the Matrix, Windows still needs to resolve
its **name** to an **IP** to route edge-crossing Mouse packets to it (this is
separate from the already-open inbound TCP connection it's receiving your
Handshake over). Options, in order of what was tried:

1. **MWB's own "IP Mappings" tab** (Settings → Mouse Without Borders →
   IP Mappings): add `<name> <ip>`. Works, but produced a "Cannot resolve
   IP Address of the remote machine" toast on every fresh connection anyway
   (see below) — a resolve-then-fallback race, not an actual failure.
2. **A static Windows hosts-file entry**
   (`C:\Windows\System32\drivers\etc\hosts`: `<ip>  <name>`) — this is the
   one that actually eliminated the toast, because it lets the *first*, raw
   `Dns.GetHostEntry` call succeed immediately instead of failing and only
   then falling back to MWB's own mapping. Confirmed as sufficient on its
   own; the IP Mapping entry was later removed entirely with no regression.

### The "connection timed out" toast, and the reverse-connection theory

After fixing DNS resolution, a *different* toast appeared once per fresh
connection: "connection to `<name>` timed out". The working theory was that
Windows expects a **symmetric pair of sockets** between two real MWB
installs — the peer also opens its own outbound connection back to you, the
same way `MainTCPRoutine` handles both client- and server-role sockets
identically. Since this bridge only ever implemented the outbound leg
(`TcpStream::connect` to Windows' port `15101`), a listener was added
(`run_server_listener` in `daemon.rs`, sharing the same `run_session` logic
via a `wl: Option<&mut WaylandInput>` parameter — `None` for the listener
side, since nothing it might receive needs forwarding anywhere) so Windows'
reverse connection would have somewhere to land.

**This turned out to not actually be the fix** — the listener has never once
logged an accepted connection, even after the timeout toast stopped
appearing (confirmed by grepping the daemon's own logs; real Mouse/Keyboard
traffic keeps flowing the whole time regardless). Removing the hosts-file
entry again afterward and restarting PowerToys *still* didn't bring the
toast back, meaning whatever actually causes it appears to be transient —
possibly tied to the very first time a given machine gets newly registered
in Windows' own state, not something that recurs on ordinary reconnects.
The listener is harmless to keep (in case the reverse connection ever does
get attempted in some other scenario) but shouldn't be assumed necessary.

### Keyboard layout translation is not as simple as "load the matching XKB layout"

Windows' low-level keyboard hook reports a VK code per keypress, and how
that VK maps to evdev/XKB differs by *kind* of key:

- **Letters A-Z and digits 0-9**: Windows assigns the VK by *printed-letter
  intent* via the currently active layout, not raw physical position. On a
  QWERTZ layout (this user's is Slovenian, `si`/no variant — matches
  `localectl`'s `X11 Layout`), the keys physically swapped versus QWERTY
  (Y and Z) get VK codes that already reflect the swap. Loading the matching
  XKB layout on the Linux side (`xkb::Keymap::new_from_names` in
  `wayland_input.rs` — parameterized by layout/variant, read from
  `config.json`'s `xkb_layout`/`xkb_variant`, not hardcoded; the Omarchy
  plugin fills these in from `hyprctl getoption input:kb_layout`) re-applies
  the *same* physical swap a second time,
  so naively mapping `VK_Y -> evdev KEY_Y` and `VK_Z -> evdev KEY_Z` (as if
  VK encoded raw physical position) cancels out wrong. **Fix: swap the
  target evdev codes for VK_Y/VK_Z specifically** (`vk_keycode.rs`) so the
  two layout-driven swaps (Windows' and XKB's) cancel out correctly instead.
- **OEM/punctuation keys (`VK_OEM_1` through `VK_OEM_8`)**: no such clean
  rule — confirmed by testing that Windows' Slovenian keyboard driver
  assigns `VK_OEM_2` (nominally the US "/ ?" key) to the physical key that
  the matching `si` XKB layout puts apostrophe on (evdev 12, not the US "/"
  slot at evdev 53). Windows layout DLLs can and do reassign OEM VK<->
  scancode per layout, unlike the base alphabet. **The only reliable way to
  map an OEM key found to behave unexpectedly is empirically**: log the raw
  `vk` value the daemon actually receives for that physical keypress, cross-
  reference against `xkbcli compile-keymap --layout <layout>`'s output for
  the evdev code that key's *label* should produce, and hardcode that
  mapping — don't trust the VK's US-centric name.

## Running as a persistent service

`daemon.rs` runs as a systemd `--user` service — unit template in
`systemd/mwb-omarchy-bridge.service`, install steps in `README.md` —
auto-restarting and starting on login. `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`
are already present in the systemd user manager's environment on this
Omarchy/UWSM setup, so no explicit environment passing was needed in the
unit.

## Reference: example field values from a working setup

- Hostname: `WINPC`
- Machine ID: `123456789`
- Security key: your own PowerToys Mouse Without Borders shared key — never commit this
- Message port: `15101` (not the default-assumed `15100`)
