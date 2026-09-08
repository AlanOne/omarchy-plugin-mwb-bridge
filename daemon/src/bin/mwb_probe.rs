// Full round-trip attempt using the confirmed-correct v0.98.1 scheme:
// fixed key/IV (no per-connection header), a 16-byte priming block
// consumed on both directions, and big-package-aware framing (only the
// FIRST 32 bytes of a package get checksum/magic-validated; a second half,
// if present, is raw).

use std::io::{Read, Write};
use std::net::TcpStream;

use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use pbkdf2::pbkdf2_hmac;
use rand::RngExt;
use sha2::{Digest, Sha512};

const SECURITY_KEY: &str = "REPLACE_WITH_YOUR_MWB_SECURITY_KEY";
const WINDOWS_IP: &str = "192.168.1.100:15101";
const PACKAGE_SIZE: usize = 32;
const PACKAGE_SIZE_EX: usize = 64;
const INITIAL_IV: &str = "18446744073709551615";
const PACKAGE_TYPE_HANDSHAKE: u8 = 126;
const PACKAGE_TYPE_HANDSHAKE_ACK: u8 = 127;
// Common.MachineID is just a persisted setting (ID.NONE=0 and ID.ALL=255 are
// the only reserved sentinels) — real machines use large, effectively-random
// values (e.g. a real Windows PC's persisted ID was a 9-digit number). Our
// Handshake packets have been sending Src=0 (ID.NONE) this whole time, which
// is very likely why Windows registers our TcpSk's MachineId as 0 — never
// matching whatever ID the matrix actually expects for this machine, so it
// can never find us as a forwarding target even though the handshake itself
// succeeds.
const OUR_MACHINE_ID: u32 = 0x12345678;

fn utf16le_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

fn derive_key(security_key: &str) -> [u8; 32] {
    let salt = utf16le_bytes(INITIAL_IV);
    let mut key = [0u8; 32];
    pbkdf2_hmac::<Sha512>(security_key.as_bytes(), &salt, 50_000, &mut key);
    key
}

fn derive_iv() -> [u8; 16] {
    INITIAL_IV.as_bytes()[..16].try_into().unwrap()
}

fn get_24bit_hash(key: &str) -> u32 {
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

struct CbcState {
    aes_encrypt: aes::Aes256,
    aes_decrypt: aes::Aes256,
}

impl CbcState {
    fn new(key: &[u8; 32]) -> Self {
        Self {
            aes_encrypt: aes::Aes256::new(key.into()),
            aes_decrypt: aes::Aes256::new(key.into()),
        }
    }

    fn encrypt(&self, chain: &mut [u8; 16], plaintext: &[u8]) -> Vec<u8> {
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

    fn decrypt(&self, chain: &mut [u8; 16], ciphertext: &[u8]) -> Vec<u8> {
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

fn pack_u32_le(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn unpack_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
}

fn build_handshake(id: u32, machine1_4: [u32; 4], machine_name: &str) -> [u8; PACKAGE_SIZE_EX] {
    let mut buf = [0u8; PACKAGE_SIZE_EX];
    buf[0] = PACKAGE_TYPE_HANDSHAKE;
    pack_u32_le(&mut buf, 4, id);
    pack_u32_le(&mut buf, 8, OUR_MACHINE_ID); // Src = our own machine ID
    pack_u32_le(&mut buf, 12, 0); // Des
    for (i, v) in machine1_4.iter().enumerate() {
        pack_u32_le(&mut buf, 16 + i * 4, *v);
    }
    let name_padded = format!("{:<32}", machine_name);
    buf[32..64].copy_from_slice(&name_padded.as_bytes()[..32]);
    buf
}

fn finalize_send_buf(buf: &mut [u8], magic_number: u32) {
    buf[3] = ((magic_number >> 24) & 0xFF) as u8;
    buf[2] = ((magic_number >> 16) & 0xFF) as u8;
    let mut checksum: u8 = 0;
    for &b in &buf[2..PACKAGE_SIZE] {
        checksum = checksum.wrapping_add(b);
    }
    buf[1] = checksum;
}

fn is_big_package(t: u8) -> bool {
    matches!(t, 3 | 21 | 20 | 51 | 126 | 127 | 79 | 69 | 78 | 125 | 124 | 76) || (t & 0x80) == 0x80
}

fn main() {
    let magic_number = get_24bit_hash(SECURITY_KEY);
    println!("magic_number = {magic_number:08x}");

    let key = derive_key(SECURITY_KEY);
    let iv = derive_iv();

    let mut stream = TcpStream::connect(WINDOWS_IP).expect("connect failed");
    let cipher = CbcState::new(&key);
    let mut read_chain = iv;
    let mut write_chain = iv;

    // Priming blocks, both directions, through the fixed-IV cipher.
    let mut priming_out = [0u8; 16];
    rand::rng().fill(&mut priming_out);
    let priming_out_ct = cipher.encrypt(&mut write_chain, &priming_out);
    stream.write_all(&priming_out_ct).expect("write priming failed");

    let mut priming_in_ct = [0u8; 16];
    stream.read_exact(&mut priming_in_ct).expect("read priming failed");
    let _ = cipher.decrypt(&mut read_chain, &priming_in_ct);

    println!("Priming exchanged both ways.\n");

    let our_machine1_4: [u32; 4] = {
        let mut r = rand::rng();
        [r.random(), r.random(), r.random(), r.random()]
    };
    let mut handshake = build_handshake(1, our_machine1_4, "omarchy");
    finalize_send_buf(&mut handshake, magic_number);
    println!(
        "Sending Handshake x10 (Machine1-4 = {:08x} {:08x} {:08x} {:08x})...",
        our_machine1_4[0], our_machine1_4[1], our_machine1_4[2], our_machine1_4[3]
    );
    for _ in 0..10 {
        let ct = cipher.encrypt(&mut write_chain, &handshake);
        stream.write_all(&ct).expect("write handshake failed");
    }
    println!("Sent.\n");

    let our_flipped: [u32; 4] = our_machine1_4.map(|v| !v);

    stream.set_read_timeout(Some(std::time::Duration::from_secs(250))).ok();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
    println!(">>> LISTENING FOR 240 SECONDS — try all four screen edges now <<<\n");
    let mut count = 0;
    let mut mouse_seen = 0u32;
    let mut keyboard_seen = 0u32;
    loop {
        if std::time::Instant::now() >= deadline {
            println!("(75s window elapsed — mouse packets seen: {mouse_seen}, keyboard packets seen: {keyboard_seen})");
            break;
        }
        let mut ct = [0u8; PACKAGE_SIZE];
        if let Err(e) = stream.read_exact(&mut ct) {
            println!("read ended after {count} packages: {e}");
            break;
        }
        let mut pt = cipher.decrypt(&mut read_chain, &ct);
        let package_type = pt[0];

        let magic = ((pt[3] as u32) << 24) + ((pt[2] as u32) << 16);
        let mut checksum: u8 = 0;
        for &b in &pt[2..PACKAGE_SIZE] {
            checksum = checksum.wrapping_add(b);
        }
        let valid = magic == (magic_number & 0xFFFF0000) && pt[1] == checksum;
        pt[1] = 0;
        pt[2] = 0;
        pt[3] = 0;
        count += 1;

        let mut full = pt.clone();
        if is_big_package(package_type) {
            let mut ct2 = [0u8; PACKAGE_SIZE];
            if stream.read_exact(&mut ct2).is_ok() {
                let pt2 = cipher.decrypt(&mut read_chain, &ct2);
                full.extend_from_slice(&pt2);
            }
        }

        let id = unpack_u32_le(&full, 4);
        let src = unpack_u32_le(&full, 8);
        let des = unpack_u32_le(&full, 12);
        let m1 = unpack_u32_le(&full, 16);
        let m2 = unpack_u32_le(&full, 20);
        let m3 = unpack_u32_le(&full, 24);
        let m4 = unpack_u32_le(&full, 28);

        let noisy = matches!(package_type, 2 | 20 | 21 | 51 | 52 | 53); // Hi/Heartbeat/Awake family
        if !noisy || count % 2000 == 0 {
            println!(
                "#{count} type=0x{:02x} valid={} id={} src={} des={} m1-4={:08x} {:08x} {:08x} {:08x}",
                package_type, valid, id, src, des, m1, m2, m3, m4
            );
        }
        if package_type == 123 {
            mouse_seen += 1;
            println!("  >>> MOUSE: x={} y={} wheel={} flags=0x{:x}", m1 as i32, m2 as i32, m3 as i32, m4);
        }
        if package_type == 122 {
            keyboard_seen += 1;
            println!("  >>> KEYBOARD: vk=0x{:x} flags=0x{:x}", m1, m2);
        }
        if full.len() >= 64 && !noisy {
            println!("  name = {:?}", String::from_utf8_lossy(&full[32..64]).trim());
        }
        if package_type == PACKAGE_TYPE_HANDSHAKE_ACK {
            let matches = m1 == our_flipped[0] && m2 == our_flipped[1] && m3 == our_flipped[2] && m4 == our_flipped[3];
            println!("  >>> HandshakeAck challenge match: {matches}");
        }
        if package_type == PACKAGE_TYPE_HANDSHAKE && full.len() >= 64 {
            // MainTCPRoutine's reciprocal side: on receiving a Handshake,
            // reply with HandshakeAck, echoing the SENDER's Machine1-4
            // bit-flipped, Src=ID.NONE, and OUR OWN machine name. We never
            // did this before — Windows was very plausibly waiting on
            // exactly this and giving up when it never arrived.
            println!("  >>> Windows' own Handshake — replying with HandshakeAck");
            let mut ack = full.clone();
            ack[0] = PACKAGE_TYPE_HANDSHAKE_ACK;
            pack_u32_le(&mut ack, 8, OUR_MACHINE_ID); // Src = our own machine ID (was wrongly 0)
            pack_u32_le(&mut ack, 16, !m1);
            pack_u32_le(&mut ack, 20, !m2);
            pack_u32_le(&mut ack, 24, !m3);
            pack_u32_le(&mut ack, 28, !m4);
            let name_padded = format!("{:<32}", "omarchy");
            ack[32..64].copy_from_slice(&name_padded.as_bytes()[..32]);
            finalize_send_buf(&mut ack, magic_number);
            let ct = cipher.encrypt(&mut write_chain, &ack);
            stream.write_all(&ct).expect("write handshake-ack failed");
            println!("  >>> HandshakeAck sent.");
        }

        if count > 200_000 {
            println!("(stopping after 200,000 — that's a lot of Hi packets)");
            break;
        }
    }
}
