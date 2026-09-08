// v0.98.1-specific scheme (confirmed different from `main`!): no header
// exchange, no priming block. Fixed deterministic key + IV derived from
// the hardcoded constant string InitialIV = ulong.MaxValue.ToString() =
// "18446744073709551615".
//
// key = PBKDF2-HMAC-SHA512(security_key [utf8], salt=UTF16LE(InitialIV), 50000 iters, 32 bytes)
// iv  = ASCII bytes of InitialIV[..16] = "1844674407370955"

use std::io::Read;
use std::net::TcpStream;

use aes::cipher::{BlockCipherDecrypt, KeyInit};
use pbkdf2::pbkdf2_hmac;
use sha2::{Digest, Sha512};

const SECURITY_KEY: &str = "REPLACE_WITH_YOUR_MWB_SECURITY_KEY";
const WINDOWS_IP: &str = "192.168.1.100:15100";
const PACKAGE_SIZE: usize = 32;
const INITIAL_IV: &str = "18446744073709551615";

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

fn cbc_decrypt(aes: &aes::Aes256, chain: &mut [u8; 16], ciphertext: &[u8]) -> Vec<u8> {
    let mut plaintext = vec![0u8; ciphertext.len()];
    for (chunk, out) in ciphertext.chunks_exact(16).zip(plaintext.chunks_exact_mut(16)) {
        let mut block: aes::Block = chunk.try_into().unwrap();
        aes.decrypt_block(&mut block);
        for j in 0..16 {
            out[j] = block[j] ^ chain[j];
        }
        chain.copy_from_slice(chunk);
    }
    plaintext
}

fn validate(buf: &mut [u8], magic_number: u32) -> bool {
    let magic = ((buf[3] as u32) << 24) + ((buf[2] as u32) << 16);
    let mut checksum: u8 = 0;
    for &b in &buf[2..PACKAGE_SIZE] {
        checksum = checksum.wrapping_add(b);
    }
    let magic_ok = magic == (magic_number & 0xFFFF0000);
    let checksum_ok = buf[1] == checksum;
    println!(
        "  diag: magic got={:08x} want={:08x} ok={}  checksum got={:02x} want={:02x} ok={}",
        magic, magic_number & 0xFFFF0000, magic_ok, buf[1], checksum, checksum_ok
    );
    buf[1] = 0;
    buf[2] = 0;
    buf[3] = 0;
    magic_ok && checksum_ok
}

fn main() {
    let magic_number = get_24bit_hash(SECURITY_KEY);
    println!("magic_number = {magic_number:08x}");

    let key = derive_key(SECURITY_KEY);
    let iv = derive_iv();
    println!("key = {}", hex::encode(key));
    println!("iv  = {}", hex::encode(iv));

    let mut stream = TcpStream::connect(WINDOWS_IP).expect("connect failed");
    let aes = aes::Aes256::new(&key.into());
    let mut chain = iv;

    // Common.SendOrReceiveARandomDataBlockPerInitialIV: even in this
    // fixed-IV scheme, a 16-byte throwaway block is still read first
    // (present in v0.98.1 too, not just the newer per-connection-header
    // version) — missing this shifted every subsequent read by one block.
    let mut priming_ct = [0u8; 16];
    stream.read_exact(&mut priming_ct).expect("read priming failed");
    let priming_pt = cbc_decrypt(&aes, &mut chain, &priming_ct);
    println!("priming (discarded) = {}\n", hex::encode(&priming_pt));

    stream.set_read_timeout(Some(std::time::Duration::from_secs(8))).ok();

    // PackageType.IsBigPackage list (from DATA.cs) — these get a second,
    // UNVALIDATED 32-byte read appended (the MachineName field), unlike the
    // bug in the previous version of this test which ran validate() (and
    // its byte 1-3 zeroing) over that second half too, corrupting real
    // MachineName characters that happened to land there.
    fn is_big_package(t: u8) -> bool {
        matches!(t, 3 | 21 | 20 | 51 | 126 | 127 | 79 | 69 | 78 | 125 | 124 | 76) || (t & 0x80) == 0x80
    }

    let mut count = 0;
    loop {
        let mut ct = [0u8; PACKAGE_SIZE];
        if let Err(e) = stream.read_exact(&mut ct) {
            println!("read ended after {count} packages: {e}");
            break;
        }
        let mut pt = cbc_decrypt(&aes, &mut chain, &ct);
        let package_type = pt[0];
        let ok = validate(&mut pt, magic_number);
        count += 1;

        let mut full = pt.clone();
        if is_big_package(package_type) {
            let mut ct2 = [0u8; PACKAGE_SIZE];
            if stream.read_exact(&mut ct2).is_ok() {
                let pt2 = cbc_decrypt(&aes, &mut chain, &ct2);
                full.extend_from_slice(&pt2); // raw, NOT validated/zeroed
            }
        }

        let ascii: String = full.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
        println!(
            "#{count} type=0x{:02x} valid={} hex={} ascii={}",
            package_type, ok, hex::encode(&full), ascii
        );
        if count > 80 {
            println!("(stopping after 80 for readability)");
            break;
        }
    }
}
