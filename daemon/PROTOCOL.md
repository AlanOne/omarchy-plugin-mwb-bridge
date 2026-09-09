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
- **Clipboard sync, both directions, text only** (small-path, see below).
- **File copy/paste, Windows -> Omarchy only** (big-path, see below) —
  copying a file on Windows lands as a real, pasteable clipboard entry
  here. The reverse direction is a confirmed architectural dead end
  against a real, unmodified PowerToys install, not a bug — see below.
- **Locking both machines** via MWB's own `HotKeyLockMachine` double-tap
  (see below) — no new packet type, just recognizing a distinctively fast
  Keyboard-packet burst.

## Locking both machines (`HotKeyLockMachine`)

Real MWB's own lock-both-machines feature, confirmed from source
(`InputHook.cs:539-575`) and live-tested end-to-end. A **single** press of
the configured combo (`Setting.Values.HotKeyLockMachine`, default
`Ctrl+Alt+Win+L`) just forwards normally — no lock, no broadcast. A
**double-tap** (within 500ms) does this instead:

```
MachineStuff.SwitchToMultipleMode(true, true);  // Des = ID.ALL for what follows
foreach key in combo: KeyboardEvent(key, down)  // all down, back-to-back
foreach key in combo: KeyboardEvent(key, up)    // all up, back-to-back
MachineStuff.SwitchToMultipleMode(false, true);
LockWorkStation();                              // then locks itself
```

`KeyboardEvent` is the *exact same function* that sends real forwarded
keystrokes as ordinary `PackageType.Keyboard` packets — there's no
dedicated lock packet type at all (the full enum was re-checked; nothing
was missed). The distinguishing feature is timing: this synthetic burst
has near-zero gaps between each key, something no natural chord press can
produce (a human pressing even a 2-key combo has real, non-zero timing
between the keys). The physical keystrokes of the double-tap itself never
reach the wire at all — `ProcessHotKeys` returns `false` for the hotkey
match, meaning Windows' global low-level hook consumes them before the
normal per-key forwarding path ever sees them; only the synthetic burst
above is what actually arrives.

This repo's `LockComboDetector` (`daemon.rs`) watches the last few Keyboard
packets' `(vk, pressed, arrival-time)` and fires once it's seen `Win`-down,
`L`-down, `Win`-up, and `L`-up all within a 150ms window, running
`omarchy-system-lock` (the same command Omarchy's own idle-service uses)
when it does. Deliberately checks only for `Win`+`L` being present, not the
full configured combo — matches whatever `HotKeyLockMachine` actually is as
long as it includes those two, without needing to know the exact value.

**Windows' own native `Win+L` cannot be used for this at all — confirmed by
testing it directly.** A genuine rapid double-press of `Win+L` produced
*zero* signal: no Keyboard packets for either key showed up on the wire.
`Win+L` is a Windows-reserved shortcut handled by the OS almost instantly,
before any third-party low-level hook (PowerToys' included) gets a chance
to register a second press within the window — the machine is already
locked by the time a "double-tap" could even be counted. `MWB` has zero
other awareness of the local machine's lock state (no
`SessionSwitch`/`WTSRegisterSessionNotification` anywhere in the module) —
this hotkey path is the *only* signal it ever produces, so there's no
other way to detect a `Win+L`-configured lock. Setting
`HotKeyLockMachine` to anything else PowerToys doesn't share with the OS
(`Ctrl+Alt+Win+L`, the classic default, confirmed working) sidesteps this
entirely, since PowerToys' own hook can then fully own the combo.

## Clipboard sync

Researched from the actual `MouseWithoutBorders` module source (`Clipboard.cs`,
`FormHelper.cs`) at the same `v0.98.1` tag everything else here was verified
against — not guessed from the `PackageType` enum names.

### Two completely different paths, chosen by size

- **Small path** (payload under `MAX_CLIPBOARD_DATA_SIZE_CAN_BE_SENT_INSTANTLY_TCP`
  = 1MB): the machine whose clipboard changed immediately broadcasts the data
  as a sequence of `ClipboardText`/`ClipboardImage` packages (`Des = ID.ALL`)
  followed by one `ClipboardDataEnd`, all over the **existing message-server
  connection** (port 15101 — the same one Mouse/Keyboard already use). Pure
  push, no announce/ask step. **This is what's implemented here, both
  directions.**
- **Big path** (≥1MB, or a normal file copy/paste — not drag-and-drop, see
  below): sender broadcasts a small "beat" (`Clipboard=69`, `Des=ID.ALL`, no
  other payload — `Common.SendClipboardBeat`) over the message server
  announcing data is available; the actual bytes go over a **separate,
  fresh clipboard-server connection** (`BASE_PORT`/15100), framed as a
  1024-byte `"{size}*{filename}"` header followed by a raw byte stream —
  not chunked into 32/64-byte DATA packages at all. **Implemented for
  files, Windows -> Omarchy direction only** — see below for both the
  wire details and why the reverse direction doesn't work against a real,
  unmodified PowerToys install.

### Small-path framing

- Package type `ClipboardText = 124` (or `ClipboardImage = 125`), sent as
  64-byte "big" packages, terminated by one 64-byte `ClipboardDataEnd = 76`
  package (empty payload).
- **Chunk size is 48 bytes** (`Clipboard.cs`'s `DATA_SIZE`), not the 32-byte
  `MachineName` region "big package" would suggest — bytes 16-63 of the
  64-byte package are one contiguous 48-byte raw-data region, repurposing what
  would normally be `Machine1`-`Machine4` + `MachineName` for every other big-
  package type. Only bytes 0-15 (Type + checksum/Id/Src/Des) keep their usual
  meaning. Receiver concatenates the 48-byte chunk from every `ClipboardText`/
  `ClipboardImage` package (in arrival order) until `ClipboardDataEnd` arrives.
  A package of any *other* type can legitimately arrive mid-transfer (it's
  still just one shared connection) — don't assume the stream is exclusively
  clipboard packets while accumulating.

### Text encoding — the part most likely to get skipped

The payload is **not** just raw UTF-16 text:

1. Build a string: `"TXT" + <plain text> + SEP`, optionally followed by
   `"RTF" + <rtf text> + SEP` and `"HTM" + <html text> + SEP` if those clipboard
   formats are also present. `SEP` is the **literal string**
   `"{4CFF57F7-BEDD-43d5-AE8F-27A61E886F2F}"` (a GUID-shaped constant, not
   parsed as one, hardcoded identically on both ends).
2. UTF-16LE-encode the whole thing (`ASCIIEncoding.Unicode` in .NET **is**
   UTF-16LE despite the name).
3. Raw-DEFLATE-compress it (`DeflateStream` — **no zlib/gzip header or
   checksum**, matches Rust's `flate2::{read,write}::Deflate{En,De}coder`
   without any wrapper).
4. Chunk the compressed bytes per the framing above.

This repo's `apply_incoming_clipboard_text` (receive) and
`build_clipboard_text_packages` (send, `mwb_protocol.rs`) mirror this exactly,
`"TXT"`-tagged only — `RTF`/`HTM` fragments are neither produced nor parsed,
matching the text-only scope. There's also a 20MB cap on the input string
(measured before compression) on real MWB's sending side, separate from the
1MB small/big-path threshold (measured on compressed bytes) — not enforced
here since nothing this size fits the small path anyway.

### Both directions, and how the reverse one avoids echo loops

Outbound (Omarchy -> Windows) is detection by polling, not a push
notification: `wl-clipboard`'s CLI tools have no simple blocking "notify on
change" primitive, so `run_clipboard_watcher` (`daemon.rs`) just polls
`wl-paste --no-newline --type text/plain` every 500ms and forwards genuinely
new content into an `mpsc` channel. That channel is drained (keeping only the
latest value, coalescing rapid successive changes) once per iteration of the
**client**-role connection's receive loop, right before it blocks on the next
read — not from a separate writer thread, since a second thread encrypting
and writing to the same socket would need to share (and carefully order
around) the single continuous CBC write-chain that connection's outbound
direction already owns; funneling everything through the one thread that
already holds it sidesteps that entirely. Only the client-role connection
checks this channel (the reverse-listener connection is cosmetic — see
"Ports" above), so there's no ambiguity about which of our two sockets a
locally-detected change goes out on. Practical latency is bounded by however
often *some* packet arrives from Windows to unblock that read (Hi/Heartbeat
keep this well under a second in practice, even with an idle mouse).

Applying Windows' clipboard to the Linux clipboard, then having our own poll
loop immediately notice that same content moments later and try to bounce it
right back, is a real risk with two independently-polled clipboards feeding
one one connection. Guarded by recording the exact text just applied
(`last_applied`, shared via `Arc<Mutex<..>>` between the receive and poll
sides) and skipping a send when new local content matches it exactly.

Each outgoing package needs its own distinct, incrementing `Id` — the DATA
struct's own doc comment for that field ("used for dedup on the receive
side") means repeating one value across every chunk (harmless for receiving,
where nothing here dedups) risks the *sender* (real MWB) side dropping
later chunks as duplicates of the first.

### Gotchas

- **Real MWB refuses to apply incoming clipboard data while at its own lock
  screen or screensaver desktop.** This repo deliberately does *not* mirror
  that — matches the project's general stance that KVM forwarding should work
  through an Omarchy lock (see the removed screen-lock "Known bugs" entry).
- **1-second debounce and same-content dedup on real MWB's sending side** —
  not reproduced here; this repo's own 500ms poll interval combined with a
  plain equality check against the last-seen value serves the same practical
  purpose without needing an explicit timer.
- Image handling is a fully separate, independently-switchable code path on
  the sender (same chunking, but `Type = ClipboardImage`, raw bytes, no
  compression, no text-format tagging) — confirmed safe to implement text now
  and images later without touching this logic, if ever needed.

### Big path (file transfer): what's implemented, and two real bugs found live

**Only "copy a file, paste it elsewhere" is implemented — not visual
drag-and-drop.** Real MWB has two genuinely separate trigger paths that
both end up at the same big-path transfer mechanism: a normal file copy
(`Clipboard.cs`'s `CheckClipboardEx`, `isFilePath` branch) and actual
Win32 drag-and-drop (`DragDrop.cs` — a ~200-line state machine involving a
hidden helper window physically repositioned under the cursor every 20ms
to intercept a native `DragEnter` event, IPC to a helper process, and
native window messages). The drag-and-drop path is fundamentally tied to
Win32 APIs and a helper GUI process with no meaningful equivalent for a
headless Linux daemon — deliberately out of scope. Folders aren't
supported either (real MWB itself refuses them, telling the user to zip
first), and only one file at a time (real MWB's own transfer code only
ever handles a single `LastDragDropFile`, never a multi-file selection —
this repo's own `parse_first_file_uri` matches that by only ever looking
at the first line of a `text/uri-list`).

**Clipboard-server handshake, in full**: same AES/priming-block scheme as
the message server, on its own fresh connection with its own fresh
IV/chain state. Then a 64-byte handshake package — but **this one has no
checksum/magic-number scheme at all**, unlike the message server's
Handshake/HandshakeAck. Confirmed the hard way: `finalize_send_buf`
(which writes checksum/magic bytes into positions 1-3) was called on it
in an early version of `pull_file_from_windows`/`serve_file_to_peer`.
Since `PackageType.Type` marshals as a full 4-byte little-endian int on
the C# side (`Id` sits at `FieldOffset(sizeof(PackageType))` == 4, same
struct layout already documented above), those checksum bytes landed in
what Windows reads as the *upper 24 bits of the same Type field* —
corrupting it into a value matching neither `Clipboard` (69, "I'm
pulling") nor `ClipboardPush` (79, "I'm about to push the bytes"). The
connection read fine and Windows replied with its own well-formed
handshake either way, but then closed without ever sending the header,
surfacing as a real Windows toast: `"Clipboard connection rejected:
Unknown: [ip]:port/id"`. **Fix: send the raw handshake bytes unmodified**
— no `finalize_send_buf` call for this specific package type, in either
direction.

**The file-transfer header's filename is a full Windows path**
(`C:\Users\Alan\...\file.txt`), not just a basename. `std::path::Path`
only recognizes `/` as a separator on Linux — `Path::new(windows_path)
.file_name()` returns the *entire* backslash-laden string as one
component, confirmed live (a received file landed named literally the
full Windows path). Split on both `/` and `\` manually instead
(`windows_basename` in `daemon.rs`) rather than relying on `Path`.

**Reuse the live message-server connection's peer IP for the file-pull
connection** rather than re-resolving the configured hostname
independently — a fresh DNS/mDNS resolution can non-deterministically
pick a different address on a dual-stack network. Wasn't the actual root
cause of the "Unknown" rejection above (that was purely the checksum
bug), but a real correctness issue in its own right: worth eliminating
the possibility of the two connections looking like different peers to
Windows.

**Confirmed, via live testing, that the reverse direction (Omarchy ->
Windows) is a genuine architectural dead end, not a bug**: the beat sends
fine and Windows' own Mini Log confirms it sees the connection as
"Connected", but real MWB's own file auto-pull (`Receiver.cs`'s `case
MachineSwitched:` handler) only fires around its internal "machine
switched" event — and this bridge never sends or receives anything
resembling a `MachineSwitched` package (it's a simple one-way input-
forwarding design, not a full peer in MWB's multi-machine switching
protocol). From Windows' own perspective, control likely never
registers as having "switched away" from it in the first place, so there
may be no switch event to trigger on at all, regardless of what hotkey
or edge-crossing the user tries. Confirmed empirically: announced a file,
tried both the hotkey and waiting, Windows never connected to this
repo's file-server listener (port 15100) at all. Making this direction
work would mean implementing a real slice of MWB's `MachineSwitched`
protocol — a bigger, more uncertain project than file transfer itself.

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

### AltGr (Level 3) characters need a separate modifier from Alt, not just a VK mapping

Some layouts put characters on a third "Level 3" shift level reached via
AltGr (Right Alt) instead of Shift — on this Slovenian layout, `<`/`>` live
there on the comma/period keys (confirmed by reading the actual compiled
XKB data: `/usr/share/X11/xkb/symbols/rs`'s `latlevel3` block, which `si`
includes). Two things had to be right before these worked, verified via
raw wire logging:

- **Windows reports AltGr as a synthetic `VK_LCONTROL` immediately
  around the real `VK_RMENU`** (both press and release) — a well-known
  Windows low-level-hook quirk, confirmed empirically: every AltGr press
  logs `vk=0xa2` (Ctrl) then `vk=0xa5` (Right Alt), in that order, on both
  edges. Forwarding that fake Ctrl as a real Ctrl-held signal is mostly
  harmless on its own, but combined with the next point it meant AltGr
  combos could never resolve correctly.
- **The compiled keymap binds Right Alt to Mod5 (the XKB "Level3Shift"
  real modifier), not Mod1 ("Alt")** — this is what `level3(ralt_switch)`
  (pulled in by `si` via `rs(latin)`) actually does to `<RALT>`. The
  daemon originally folded `VK_RMENU` into the same bit as `VK_LMENU`
  (`MOD_ALT`, bit 3), which never engages level 3 at all — AltGr presses
  landed wherever Shift-state happened to leave them (level 1 or 2), never
  level 3. **Fix** (`daemon.rs`): on `VK_RMENU`, clear the fake Ctrl bit
  Windows' companion event set and set a separate `MOD_LEVEL3` bit
  (`1 << 7`) instead — libxkbcommon's legacy 8-modifier ordering
  (Shift/Lock/Control/Mod1/Mod2/Mod3/Mod4/Mod5 = bits 0-7) makes this a
  fixed, reliable bit position for any keymap using `ralt_switch`, no need
  to query the compiled keymap for it.

If a different layout's AltGr level doesn't work even with this fix in
place, check that layout's own XKB data for what it actually binds Right
Alt to (`ralt_switch` is the near-universal xkeyboard-config default, but
not guaranteed) before assuming the bit position is wrong.

### NumLock: don't forward Windows' keypress, don't trust either side's state

Numpad digit/operator keys are dual-function purely based on whichever
*receiving* side currently has NumLock locked — Windows' own NumLock state
is never communicated over the wire at all (Mouse Without Borders' wire
protocol has no field for it; a Keyboard packet is just a raw VK+flags per
keystroke). Forwarding Windows' NumLock keypress as a literal key event
does toggle this machine's own NumLock lock-state (the compiled keymap's
NumLock key has a `LockMods` action that Hyprland's own `xkb_state`
processes automatically on any raw keypress, virtual or real) — but that
lock-state is now unsynchronized with Windows', discovered directly:
testing the NumLock key once flipped this machine's lock-state off,
silently turning every numpad digit into a navigation key (Home/End/
arrows/etc.) instead, with no error anywhere. **Fix** (`daemon.rs`): don't
forward the raw NumLock keypress at all, and instead force NumLock-locked
(`1 << 4`, XKB's conventional Mod2) via the virtual keyboard's
`modifiers()` call on every numpad key event — so the two sides' NumLock
state never needs to be tracked, toggled, or agree at all.

## One environment quirk worth knowing if the systemd unit ever needs debugging

`daemon.rs` runs as a systemd `--user` service (see `../README.md` for
install steps) with no `Environment=` lines, yet it can still reach the
Wayland compositor — `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR` are already
present in the systemd user manager's own environment on an Omarchy/UWSM
setup, so a plain child service inherits them for free.

## Reference: example field values from a working setup

- Hostname: `WINPC`
- Machine ID: `123456789`
- Security key: your own PowerToys Mouse Without Borders shared key — never commit this
- Message port: `15101` (not the default-assumed `15100`)
