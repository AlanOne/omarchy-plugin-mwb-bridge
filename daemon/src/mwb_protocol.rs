// Shared MWB v0.98.1 protocol primitives (crypto, framing, packet
// building/parsing) — see PROTOCOL.md for the full spec and why each piece
// is the way it is. Extracted from the original mwb_probe.rs test client so
// the real daemon doesn't duplicate this.

use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
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
pub const PACKAGE_TYPE_CLIPBOARD_DATA_END: u8 = 76;
pub const PACKAGE_TYPE_CLIPBOARD_TEXT: u8 = 124;
pub const PACKAGE_TYPE_CLIPBOARD_IMAGE: u8 = 125;
pub const ID_ALL: u32 = 255;

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
