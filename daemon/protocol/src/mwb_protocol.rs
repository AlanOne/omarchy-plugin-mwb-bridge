// Shared MWB v0.98.1 protocol primitives (crypto, framing, packet
// building/parsing) — see PROTOCOL.md for the full spec and why each piece
// is the way it is. Extracted from the original mwb_probe.rs test client so
// the real daemon doesn't duplicate this.

use std::io::Write;

use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use pbkdf2::pbkdf2_hmac;
use sha2::{Digest, Sha512};

pub const PACKAGE_SIZE: usize = 32;
pub const PACKAGE_SIZE_EX: usize = 64;
pub const INITIAL_IV: &str = "18446744073709551615";

pub const PACKAGE_TYPE_HI: u8 = 2;
pub const PACKAGE_TYPE_HELLO: u8 = 3;
pub const PACKAGE_TYPE_BYEBYE: u8 = 4;
pub const PACKAGE_TYPE_HEARTBEAT: u8 = 20;
pub const PACKAGE_TYPE_KEYBOARD: u8 = 122;
pub const PACKAGE_TYPE_MOUSE: u8 = 123;
pub const PACKAGE_TYPE_HANDSHAKE: u8 = 126;
pub const PACKAGE_TYPE_HANDSHAKE_ACK: u8 = 127;
// "You're now the active machine" — this is what actually gates real MWB's
// own file/big-clipboard auto-pull (Receiver.cs's MachineSwitched case),
// not the Clipboard beat itself. A plain 32-byte package (not in the
// is_big_package list), Des = the machine being told it's now active.
// Confirmed harmless to synthesize: its handler only checks timing against
// the last beat and re-triggers a pull — no state it touches is shared
// with Mouse/Keyboard forwarding. See PROTOCOL.md.
pub const PACKAGE_TYPE_MACHINE_SWITCHED: u8 = 77;
// Sent immediately before MachineSwitched in every *real* switch this repo
// has captured live from Windows (Ctrl+Alt+F1-style) — presumably just a
// cursor-visibility toggle for the switch UX, but included here to exactly
// mirror the real sequence rather than a synthesized subset of it.
pub const PACKAGE_TYPE_HIDE_MOUSE: u8 = 50;
pub const PACKAGE_TYPE_CLIPBOARD_DATA_END: u8 = 76;
pub const PACKAGE_TYPE_CLIPBOARD_TEXT: u8 = 124;
pub const PACKAGE_TYPE_CLIPBOARD_IMAGE: u8 = 125;
// "Clipboard" in the C# enum — the small "beat" broadcast over the message
// server announcing big data (here: a file) is available, and also the
// handshake package type used by whichever side of the *separate*
// port-15100 connection is pulling/receiving rather than pushing.
pub const PACKAGE_TYPE_CLIPBOARD: u8 = 69;
// The handshake package type used by whichever side of the port-15100
// connection is about to push/send the actual file bytes.
pub const PACKAGE_TYPE_CLIPBOARD_PUSH: u8 = 79;
pub const ID_ALL: u32 = 255;

// Real MWB's own cap (MAX_CLIPBOARD_FILE_SIZE_CAN_BE_SENT), enforced by the
// sender before ever announcing a file.
pub const MAX_CLIPBOARD_FILE_SIZE: usize = 100 * 1024 * 1024;

// Real MWB's own small-path/big-path threshold
// (MAX_CLIPBOARD_DATA_SIZE_CAN_BE_SENT_INSTANTLY_TCP), applied uniformly to
// both text and image clipboard data — under this, push directly over the
// message-server connection; at or above it, announce a beat and let the
// peer pull it over the separate clipboard-server connection instead.
pub const MAX_SMALL_PATH_SIZE: usize = 1024 * 1024;

// The literal type-tag real MWB uses in the big-path file-transfer header's
// `name` field when the payload is a clipboard image rather than a real
// file — confirmed from source, not a filename to be sanitized/basenamed.
pub const BIG_PATH_IMAGE_NAME: &str = "image";

// Small-path clipboard chunking (Clipboard.cs's DATA_SIZE): a ClipboardText/
// ClipboardImage "big" (64-byte) package repurposes bytes 16-63 — normally
// Machine1-4 + MachineName — as one contiguous 48-byte raw-data region.
// Terminated by one empty ClipboardDataEnd package. Text payloads are
// "TXT" + text + SEP (+ optional "RTF"/"HTM" + content + SEP), UTF-16LE
// encoded, then raw-DEFLATE compressed (no zlib/gzip wrapper) before
// chunking — see PROTOCOL.md's clipboard section.
pub const CLIPBOARD_CHUNK_SIZE: usize = 48;
pub const CLIPBOARD_SEP: &str = "{4CFF57F7-BEDD-43d5-AE8F-27A61E886F2F}";

pub fn utf16le_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

pub fn derive_key(security_key: &str) -> [u8; 32] {
    let salt = utf16le_bytes(INITIAL_IV);
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha512>(security_key.as_bytes(), &salt, 50_000, &mut key);
    key
}

pub fn derive_iv() -> [u8; 16] {
    INITIAL_IV.as_bytes()[..16].try_into().unwrap()
}

/// Encryption.Get24BitHash: SHA-512 the (byte-truncated) key string once,
/// then re-hash the digest 50000 more times, then pack specific bytes of
/// the final digest into a u32. Verified byte-for-byte against a real
/// PowerToys debug dump's logged magicNumber value.
pub fn get_24bit_hash(key: &str) -> u32 {
    let mut bytes = [0u8; PACKAGE_SIZE];
    for (i, ch) in key.chars().take(PACKAGE_SIZE).enumerate() {
        bytes[i] = ch as u8;
    }
    let mut hash = Sha512::digest(bytes);
    for _ in 0..50_000 {
        hash = Sha512::digest(hash);
    }
    ((hash[0] as u32) << 23) + ((hash[1] as u32) << 16) + ((hash[63] as u32) << 8) + (hash[2] as u32)
}

/// AES-256-CBC over a continuous stream (chaining persists across calls,
/// not reset per packet — mirrors .NET's single long-lived CryptoStream per
/// connection direction).
pub struct CbcState {
    aes_encrypt: aes::Aes256,
    aes_decrypt: aes::Aes256,
}

impl CbcState {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            aes_encrypt: aes::Aes256::new(key.into()),
            aes_decrypt: aes::Aes256::new(key.into()),
        }
    }

    pub fn encrypt(&self, chain: &mut [u8; 16], plaintext: &[u8]) -> Vec<u8> {
        let mut ciphertext = vec![0u8; plaintext.len()];
        for (chunk, out) in plaintext.chunks_exact(16).zip(ciphertext.chunks_exact_mut(16)) {
            let mut block: aes::Block = [0u8; 16].into();
            for j in 0..16 {
                block[j] = chunk[j] ^ chain[j];
            }
            self.aes_encrypt.encrypt_block(&mut block);
            out.copy_from_slice(&block);
            chain.copy_from_slice(&block);
        }
        ciphertext
    }

    pub fn decrypt(&self, chain: &mut [u8; 16], ciphertext: &[u8]) -> Vec<u8> {
        let mut plaintext = vec![0u8; ciphertext.len()];
        for (chunk, out) in ciphertext.chunks_exact(16).zip(plaintext.chunks_exact_mut(16)) {
            let mut block: aes::Block = chunk.try_into().unwrap();
            self.aes_decrypt.decrypt_block(&mut block);
            for j in 0..16 {
                out[j] = block[j] ^ chain[j];
            }
            chain.copy_from_slice(chunk);
        }
        plaintext
    }
}

pub fn pack_u32_le(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn unpack_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
}

/// See PROTOCOL.md's "IsBigPackage" list — these get a second, unvalidated
/// 32-byte read appended (the MachineName field).
pub fn is_big_package(t: u8) -> bool {
    matches!(t, 3 | 21 | 20 | 51 | 126 | 127 | 79 | 69 | 78 | 125 | 124 | 76) || (t & 0x80) == 0x80
}

/// Builds a 64-byte Handshake packet (Type=126). `src_id` must be a real,
/// non-zero, non-255 machine ID — see PROTOCOL.md's "Src must never be
/// ID.NONE" gotcha.
pub fn build_handshake(id: u32, src_id: u32, machine1_4: [u32; 4], machine_name: &str) -> [u8; PACKAGE_SIZE_EX] {
    let mut buf = [0u8; PACKAGE_SIZE_EX];
    buf[0] = PACKAGE_TYPE_HANDSHAKE;
    pack_u32_le(&mut buf, 4, id);
    pack_u32_le(&mut buf, 8, src_id);
    pack_u32_le(&mut buf, 12, 0); // Des
    for (i, v) in machine1_4.iter().enumerate() {
        pack_u32_le(&mut buf, 16 + i * 4, *v);
    }
    let name_padded = format!("{:<32}", machine_name);
    buf[32..64].copy_from_slice(&name_padded.as_bytes()[..32]);
    buf
}

/// Builds a HandshakeAck (Type=127) replying to an incoming Handshake:
/// echoes the sender's Machine1-4 bit-flipped, per the challenge-response.
pub fn build_handshake_ack(incoming: &[u8], src_id: u32, our_name: &str) -> [u8; PACKAGE_SIZE_EX] {
    let mut ack = [0u8; PACKAGE_SIZE_EX];
    ack.copy_from_slice(&incoming[..PACKAGE_SIZE_EX.min(incoming.len())]);
    ack[0] = PACKAGE_TYPE_HANDSHAKE_ACK;
    pack_u32_le(&mut ack, 8, src_id);
    for i in 0..4 {
        let v = unpack_u32_le(incoming, 16 + i * 4);
        pack_u32_le(&mut ack, 16 + i * 4, !v);
    }
    let name_padded = format!("{:<32}", our_name);
    ack[32..64].copy_from_slice(&name_padded.as_bytes()[..32]);
    ack
}

/// Builds the outgoing ClipboardText chunk sequence (+ trailing
/// ClipboardDataEnd) for Omarchy -> Windows sync: `"TXT" + text + SEP`,
/// UTF-16LE-encoded, raw-DEFLATE-compressed (no zlib/gzip wrapper — matches
/// what real MWB expects to receive), then split into `CLIPBOARD_CHUNK_SIZE`
/// pieces. `next_id` is the caller's running per-connection id counter,
/// incremented once per package built (real MWB's Id field is "used for
/// dedup on the receive side" — a repeated id across packages risks one
/// being silently dropped as a duplicate, so every package here needs its
/// own distinct value, not just a shared placeholder like the 32-byte
/// types use). Caller still owes each returned package a
/// `finalize_send_buf` call before sending — this only fills in the type/
/// id/src/des/data fields.
pub fn build_clipboard_text_packages(next_id: &mut u32, src_id: u32, text: &str) -> Vec<[u8; PACKAGE_SIZE_EX]> {
    let tagged = format!("TXT{text}{CLIPBOARD_SEP}");
    let utf16 = utf16le_bytes(&tagged);

    let mut compressed = Vec::new();
    {
        let mut enc = DeflateEncoder::new(&mut compressed, Compression::default());
        enc.write_all(&utf16).expect("compressing into a Vec<u8> cannot fail");
    }

    build_clipboard_chunk_packages(next_id, src_id, PACKAGE_TYPE_CLIPBOARD_TEXT, &compressed)
}

/// Builds the outgoing ClipboardImage chunk sequence (+ trailing
/// ClipboardDataEnd) for small-path image sync (payload under 1MB — see
/// PROTOCOL.md for the size threshold and the big-path fallback for larger
/// images). Unlike text, real MWB applies **no compression and no
/// text-format tagging** to image data at all — the wire payload is simply
/// the clipboard image's own PNG bytes, verbatim (confirmed from source:
/// `image.Save(stream, ImageFormat.Png)` on the sending side, `Image
/// .FromStream` on receipt — a plain PNG file both ways, nothing custom).
pub fn build_clipboard_image_packages(next_id: &mut u32, src_id: u32, png_bytes: &[u8]) -> Vec<[u8; PACKAGE_SIZE_EX]> {
    build_clipboard_chunk_packages(next_id, src_id, PACKAGE_TYPE_CLIPBOARD_IMAGE, png_bytes)
}

fn build_clipboard_chunk_packages(next_id: &mut u32, src_id: u32, package_type: u8, payload: &[u8]) -> Vec<[u8; PACKAGE_SIZE_EX]> {
    let mut packages = Vec::new();
    let mut build_header = |buf: &mut [u8; PACKAGE_SIZE_EX], pt: u8| {
        buf[0] = pt;
        pack_u32_le(buf, 4, *next_id);
        pack_u32_le(buf, 8, src_id);
        pack_u32_le(buf, 12, ID_ALL);
        *next_id += 1;
    };

    if payload.is_empty() {
        // An empty payload still needs at least one chunk package so the
        // receiver has something to accumulate before DataEnd.
        let mut buf = [0u8; PACKAGE_SIZE_EX];
        build_header(&mut buf, package_type);
        packages.push(buf);
    }
    for chunk in payload.chunks(CLIPBOARD_CHUNK_SIZE) {
        let mut buf = [0u8; PACKAGE_SIZE_EX];
        build_header(&mut buf, package_type);
        buf[16..16 + chunk.len()].copy_from_slice(chunk);
        packages.push(buf);
    }
    let mut end = [0u8; PACKAGE_SIZE_EX];
    build_header(&mut end, PACKAGE_TYPE_CLIPBOARD_DATA_END);
    packages.push(end);

    packages
}

/// Builds the "Clipboard" beat package (Type=69) broadcast over the
/// message-server connection to announce that a file is available to pull
/// — mirrors real MWB's `SendClipboardBeat`. Real MWB only auto-pulls this
/// around its own "machine switched" event (this bridge's fixed two-machine
/// topology has no equivalent concept), so this repo's receiver treats any
/// beat as "pull immediately" instead — a deliberate simplification, see
/// PROTOCOL.md.
pub fn build_clipboard_beat(id: u32, src_id: u32) -> [u8; PACKAGE_SIZE_EX] {
    let mut buf = [0u8; PACKAGE_SIZE_EX];
    buf[0] = PACKAGE_TYPE_CLIPBOARD;
    pack_u32_le(&mut buf, 4, id);
    pack_u32_le(&mut buf, 8, src_id);
    pack_u32_le(&mut buf, 12, ID_ALL);
    buf
}

/// Builds the "MachineSwitched" package (Type=77) that actually triggers
/// real MWB's file/big-clipboard auto-pull — a plain 32-byte package
/// (unlike the beat above, `Des` here is the specific machine being told
/// it's now active, not a broadcast). Must arrive within 30s of a
/// `Clipboard` beat to have any effect (`BIG_CLIPBOARD_DATA_TIMEOUT`) —
/// send it right after the beat, same as this repo already does.
pub fn build_machine_switched(id: u32, src_id: u32, des_id: u32) -> [u8; PACKAGE_SIZE] {
    let mut buf = [0u8; PACKAGE_SIZE];
    buf[0] = PACKAGE_TYPE_MACHINE_SWITCHED;
    pack_u32_le(&mut buf, 4, id);
    pack_u32_le(&mut buf, 8, src_id);
    pack_u32_le(&mut buf, 12, des_id);
    buf
}

/// Builds a HideMouse package (Type=50) — see its constant doc comment.
pub fn build_hide_mouse(id: u32, src_id: u32, des_id: u32) -> [u8; PACKAGE_SIZE] {
    let mut buf = [0u8; PACKAGE_SIZE];
    buf[0] = PACKAGE_TYPE_HIDE_MOUSE;
    pack_u32_le(&mut buf, 4, id);
    pack_u32_le(&mut buf, 8, src_id);
    pack_u32_le(&mut buf, 12, des_id);
    buf
}

/// Builds the 64-byte handshake package used on the *separate* clipboard-
/// server connection (port 15100/`BASE_PORT`) that actual file bytes travel
/// over — distinct from the message-server's Handshake/HandshakeAck pair.
/// `package_type` is `PACKAGE_TYPE_CLIPBOARD` (this side is pulling/
/// receiving) or `PACKAGE_TYPE_CLIPBOARD_PUSH` (this side is about to push/
/// send the file). Real MWB leaves Des/Machine1-4 at zero here and only
/// populates Src + MachineName — matched exactly.
pub fn build_clipboard_handshake(package_type: u8, src_id: u32, machine_name: &str) -> [u8; PACKAGE_SIZE_EX] {
    let mut buf = [0u8; PACKAGE_SIZE_EX];
    buf[0] = package_type;
    pack_u32_le(&mut buf, 8, src_id);
    let name_padded = format!("{:<32}", machine_name);
    buf[32..64].copy_from_slice(&name_padded.as_bytes()[..32]);
    buf
}

pub const CLIPBOARD_FILE_HEADER_SIZE: usize = 1024;

/// Builds the 1024-byte "{size}*{name}" header sent (through the same
/// continuing encrypted stream, right before the file's raw bytes) on the
/// clipboard-server connection. UTF-16LE-encoded, **null**-padded to fill
/// the buffer (not space-padded, unlike the small-path MachineName field) —
/// matches real MWB's framing exactly.
pub fn build_file_header(size: usize, name: &str) -> [u8; CLIPBOARD_FILE_HEADER_SIZE] {
    let bytes = utf16le_bytes(&format!("{size}*{name}"));
    let mut buf = [0u8; CLIPBOARD_FILE_HEADER_SIZE];
    let n = bytes.len().min(CLIPBOARD_FILE_HEADER_SIZE);
    buf[..n].copy_from_slice(&bytes[..n]);
    buf
}

/// Parses a file-transfer header back into `(size, name)`. Returns `None`
/// if there's no `*`-separated numeric size prefix at all; callers should
/// *also* treat a successfully-parsed `size == 0` as "no real file" — real
/// MWB stuffs an English error message into this same header shape on
/// failure (e.g. `"0*<path> - Folder is not supported, zip it first!"`)
/// rather than using a distinct error package type.
pub fn parse_file_header(buf: &[u8; CLIPBOARD_FILE_HEADER_SIZE]) -> Option<(usize, String)> {
    let utf16: Vec<u16> = buf.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
    let end = utf16.iter().position(|&u| u == 0).unwrap_or(utf16.len());
    let s = String::from_utf16_lossy(&utf16[..end]);
    let (size_str, name) = s.split_once('*')?;
    Some((size_str.parse().ok()?, name.to_string()))
}

/// SocketStuff.TcpSendData's checksum+magic packing, applied to the first
/// 32 bytes of `buf` regardless of whether it's a 32- or 64-byte package.
pub fn finalize_send_buf(buf: &mut [u8], magic_number: u32) {
    buf[3] = ((magic_number >> 24) & 0xFF) as u8;
    buf[2] = ((magic_number >> 16) & 0xFF) as u8;
    let mut checksum: u8 = 0;
    for &b in &buf[2..PACKAGE_SIZE] {
        checksum = checksum.wrapping_add(b);
    }
    buf[1] = checksum;
}

/// SocketStuff.ProcessReceivedDataEx's validation: checks the magic/checksum
/// over the first 32 bytes, then zeroes bytes 1-3 so the caller reads a
/// clean PackageType out of buf[0]. Returns whether it validated.
pub fn validate_and_clean_recv_buf(buf: &mut [u8], magic_number: u32) -> bool {
    let magic = ((buf[3] as u32) << 24) + ((buf[2] as u32) << 16);
    let mut checksum: u8 = 0;
    for &b in &buf[2..PACKAGE_SIZE] {
        checksum = checksum.wrapping_add(b);
    }
    let ok = magic == (magic_number & 0xFFFF0000) && buf[1] == checksum;
    buf[1] = 0;
    buf[2] = 0;
    buf[3] = 0;
    ok
}
