//! Password-encrypted IMZ/WLZ members as WinImage writes them.
//!
//! The format is not published; it was worked out from files the WinImage application
//! saved with known passwords and known contents (its help file only says that a 128-bit
//! key is built from the password with MD5 and used with Rijndael). The member is an
//! ordinary ZIP entry (method 8, general-purpose flag bit 0) whose stored bytes are:
//!
//! - when the deflate stream spans more than one chunk, 8 bytes first: the CRC-32 of the
//!   first chunk's plaintext, then the CRC-32 of its ciphertext (WinImage refuses the file
//!   when they do not match);
//! - the raw deflate stream cut into 32 KiB chunks, each zero-padded to a multiple of 16
//!   bytes and encrypted with AES-128 in CBC mode with a zero IV, stored as a little-endian
//!   u32 length followed by that many bytes;
//! - a u32 giving the deflate bytes in the last chunk.
//!
//! The key is the MD5 digest of the password in UTF-16LE. The entry carries two extra
//! fields: `WI` (u32 1, u32 image size) and `SI` (eight fixed bytes that mark the
//! encryption). The ZIP CRC is that of the decompressed image.

use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use md5::{Digest, Md5};

use crate::error::{Error, Result};

const CHUNK: usize = 32768;
/// Extra field ID `SI`.
pub(crate) const SI_TAG: u16 = 0x4953;
/// Extra field ID `WI`.
pub(crate) const WI_TAG: u16 = 0x4957;
/// The body of the `SI` extra field in every encrypted file seen.
const SI_BODY: [u8; 8] = [0x80, 0x4D, 0x49, 0x52, 0x10, 0x80, 0x00, 0x00];

/// Whether a ZIP extra field block has WinImage's encryption marker.
pub(crate) fn has_marker(extra: &[u8]) -> bool {
    let mut p = 0;
    while p + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[p], extra[p + 1]]);
        let len = u16::from_le_bytes([extra[p + 2], extra[p + 3]]) as usize;
        if id == SI_TAG {
            return true;
        }
        p += 4 + len;
    }
    false
}

/// The extra fields for an encrypted member holding an image of `size` bytes.
pub(crate) fn extra_fields(size: u64) -> Vec<u8> {
    let mut e = Vec::with_capacity(24);
    e.extend_from_slice(&WI_TAG.to_le_bytes());
    e.extend_from_slice(&8u16.to_le_bytes());
    e.extend_from_slice(&1u32.to_le_bytes());
    e.extend_from_slice(&(size as u32).to_le_bytes());
    e.extend_from_slice(&SI_TAG.to_le_bytes());
    e.extend_from_slice(&8u16.to_le_bytes());
    e.extend_from_slice(&SI_BODY);
    e
}

fn key(password: &str) -> Aes128 {
    let units: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let digest: [u8; 16] = Md5::digest(&units).into();
    Aes128::new(&digest)
}

/// Split the stored bytes into chunk ciphertexts and the deflate length of the last one.
fn frames(data: &[u8], start: usize) -> Option<(Vec<&[u8]>, usize)> {
    let mut p = start;
    let mut out = Vec::new();
    loop {
        let n = u32::from_le_bytes(data.get(p..p + 4)?.try_into().ok()?) as usize;
        if n == 0 || !n.is_multiple_of(16) || n > CHUNK + 16 {
            return None;
        }
        out.push(data.get(p + 4..p + 4 + n)?);
        p += 4 + n;
        if n < CHUNK || data.len() == p + 4 {
            break;
        }
    }
    let last = u32::from_le_bytes(data.get(p..p + 4)?.try_into().ok()?) as usize;
    (p + 4 == data.len() && last <= out.last()?.len()).then_some((out, last))
}

/// Decrypt and decompress a member's stored bytes. `crc` and `size` come from the ZIP
/// header and tell a wrong password from a right one.
pub(crate) fn decrypt(data: &[u8], password: &str, crc: u32, size: u64) -> Result<Vec<u8>> {
    let (chunks, last) = frames(data, 0).or_else(|| frames(data, 8)).ok_or_else(|| {
        Error::Invalid("the encrypted image is damaged (bad chunk layout)".into())
    })?;
    let aes = key(password);
    let mut deflated = Vec::with_capacity(data.len());
    let count = chunks.len();
    for (i, c) in chunks.into_iter().enumerate() {
        let mut plain = c.to_vec();
        aes.cbc_decrypt(&mut plain);
        if i + 1 == count {
            plain.truncate(last);
        }
        deflated.extend_from_slice(&plain);
    }
    let mut out = Vec::with_capacity(size as usize);
    let wrong = || Error::WrongPassword;
    DeflateDecoder::new(&deflated[..])
        .read_to_end(&mut out)
        .map_err(|_| wrong())?;
    if out.len() as u64 != size || crc32fast_hash(&out) != crc {
        return Err(wrong());
    }
    Ok(out)
}

/// Compress and encrypt an image; returns the stored bytes for the ZIP entry.
pub(crate) fn encrypt(image: &[u8], password: &str) -> std::io::Result<Vec<u8>> {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
    enc.write_all(image)?;
    let deflated = enc.finish()?;
    let aes = key(password);
    let pieces: Vec<&[u8]> = if deflated.is_empty() {
        vec![&[][..]]
    } else {
        deflated.chunks(CHUNK).collect()
    };
    let mut out = Vec::with_capacity(deflated.len() + pieces.len() * 20 + 12);
    if pieces.len() > 1 {
        out.extend_from_slice(&[0; 8]);
    }
    for (i, p) in pieces.iter().enumerate() {
        let mut block = p.to_vec();
        block.resize(p.len().div_ceil(16).max(1) * 16, 0);
        let plain_crc = crc32fast_hash(&block);
        aes.cbc_encrypt(&mut block);
        if i == 0 && pieces.len() > 1 {
            out[..4].copy_from_slice(&plain_crc.to_le_bytes());
            out[4..8].copy_from_slice(&crc32fast_hash(&block).to_le_bytes());
        }
        out.extend_from_slice(&(block.len() as u32).to_le_bytes());
        out.extend_from_slice(&block);
    }
    out.extend_from_slice(&(pieces.last().map_or(0, |p| p.len()) as u32).to_le_bytes());
    Ok(out)
}

pub(crate) fn crc32fast_hash(data: &[u8]) -> u32 {
    let mut h = flate2::Crc::new();
    h.update(data);
    h.sum()
}

/// AES-128 (FIPS-197), enough for CBC with a zero IV.
pub(crate) struct Aes128 {
    rk: [[u8; 16]; 11],
}

const fn xtime(a: u8) -> u8 {
    (a << 1) ^ if a & 0x80 != 0 { 0x1B } else { 0 }
}

const fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut r = 0;
    while b != 0 {
        if b & 1 != 0 {
            r ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    r
}

const fn sboxes() -> ([u8; 256], [u8; 256]) {
    let mut s = [0u8; 256];
    let mut inv = [0u8; 256];
    let mut x = 0usize;
    while x < 256 {
        // Multiplicative inverse in GF(2^8) by exponentiation (x^254), then the affine map.
        let mut y = 1u8;
        let mut base = x as u8;
        let mut e = 254u32;
        while e != 0 {
            if e & 1 != 0 {
                y = gmul(y, base);
            }
            base = gmul(base, base);
            e >>= 1;
        }
        if x == 0 {
            y = 0;
        }
        let b =
            y ^ y.rotate_left(1) ^ y.rotate_left(2) ^ y.rotate_left(3) ^ y.rotate_left(4) ^ 0x63;
        s[x] = b;
        inv[b as usize] = x as u8;
        x += 1;
    }
    (s, inv)
}

const BOXES: ([u8; 256], [u8; 256]) = sboxes();
const SBOX: [u8; 256] = BOXES.0;
const INV_SBOX: [u8; 256] = BOXES.1;

impl Aes128 {
    pub(crate) fn new(key: &[u8; 16]) -> Self {
        let mut w = [[0u8; 4]; 44];
        for i in 0..4 {
            w[i].copy_from_slice(&key[i * 4..i * 4 + 4]);
        }
        let mut rcon = 1u8;
        for i in 4..44 {
            let mut t = w[i - 1];
            if i % 4 == 0 {
                t = [
                    SBOX[t[1] as usize],
                    SBOX[t[2] as usize],
                    SBOX[t[3] as usize],
                    SBOX[t[0] as usize],
                ];
                t[0] ^= rcon;
                rcon = xtime(rcon);
            }
            for j in 0..4 {
                w[i][j] = w[i - 4][j] ^ t[j];
            }
        }
        let mut rk = [[0u8; 16]; 11];
        for (r, k) in rk.iter_mut().enumerate() {
            for c in 0..4 {
                k[c * 4..c * 4 + 4].copy_from_slice(&w[r * 4 + c]);
            }
        }
        Aes128 { rk }
    }

    fn add(s: &mut [u8; 16], k: &[u8; 16]) {
        for (a, b) in s.iter_mut().zip(k) {
            *a ^= b;
        }
    }

    pub(crate) fn encrypt_block(&self, s: &mut [u8; 16]) {
        Self::add(s, &self.rk[0]);
        for (r, key) in self.rk.iter().enumerate().skip(1) {
            let mut t = [0u8; 16];
            for c in 0..4 {
                for row in 0..4 {
                    t[c * 4 + row] = SBOX[s[((c + row) % 4) * 4 + row] as usize];
                }
            }
            if r < 10 {
                for c in 0..4 {
                    let a = [t[c * 4], t[c * 4 + 1], t[c * 4 + 2], t[c * 4 + 3]];
                    for row in 0..4 {
                        t[c * 4 + row] = xtime(a[row])
                            ^ xtime(a[(row + 1) % 4])
                            ^ a[(row + 1) % 4]
                            ^ a[(row + 2) % 4]
                            ^ a[(row + 3) % 4];
                    }
                }
            }
            *s = t;
            Self::add(s, key);
        }
    }

    pub(crate) fn decrypt_block(&self, s: &mut [u8; 16]) {
        Self::add(s, &self.rk[10]);
        for r in (0..10).rev() {
            let mut t = [0u8; 16];
            for c in 0..4 {
                for row in 0..4 {
                    t[((c + row) % 4) * 4 + row] = INV_SBOX[s[c * 4 + row] as usize];
                }
            }
            Self::add(&mut t, &self.rk[r]);
            if r > 0 {
                for c in 0..4 {
                    let a = [t[c * 4], t[c * 4 + 1], t[c * 4 + 2], t[c * 4 + 3]];
                    for row in 0..4 {
                        t[c * 4 + row] = gmul(a[row], 14)
                            ^ gmul(a[(row + 1) % 4], 11)
                            ^ gmul(a[(row + 2) % 4], 13)
                            ^ gmul(a[(row + 3) % 4], 9);
                    }
                }
            }
            *s = t;
        }
    }

    /// CBC with a zero IV over whole blocks.
    pub(crate) fn cbc_encrypt(&self, data: &mut [u8]) {
        let mut prev = [0u8; 16];
        for b in data.as_chunks_mut::<16>().0 {
            Self::add(b, &prev);
            self.encrypt_block(b);
            prev = *b;
        }
    }

    pub(crate) fn cbc_decrypt(&self, data: &mut [u8]) {
        let mut prev = [0u8; 16];
        for b in data.as_chunks_mut::<16>().0 {
            let c = *b;
            self.decrypt_block(b);
            Self::add(b, &prev);
            prev = c;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn fips_197_example() {
        let k: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let aes = Aes128::new(&k);
        let mut b: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().unwrap();
        aes.encrypt_block(&mut b);
        assert_eq!(b.to_vec(), hex("69c4e0d86a7b0430d8cdb78070b4c55a"));
        aes.decrypt_block(&mut b);
        assert_eq!(b.to_vec(), hex("00112233445566778899aabbccddeeff"));
        // SP 800-38A F.2.1 (CBC-AES128), first block, with its IV folded into the plaintext.
        let k: [u8; 16] = hex("2b7e151628aed2a6abf7158809cf4f3c").try_into().unwrap();
        let mut p = hex("6bc1bee22e409f96e93d7e117393172a");
        for (a, b) in p.iter_mut().zip(hex("000102030405060708090a0b0c0d0e0f")) {
            *a ^= b;
        }
        Aes128::new(&k).cbc_encrypt(&mut p);
        assert_eq!(p, hex("7649abac8119b246cee98e9b12e9197d"));
    }

    #[test]
    fn key_is_md5_of_utf16() {
        // MD5("s\0e\0c\0r\0e\0t\0"), the key WinImage used for the password "secret".
        let units: Vec<u8> = "secret".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let d: [u8; 16] = Md5::digest(&units).into();
        assert_eq!(d.to_vec(), hex("09433e1853385270b51511571e35eeca"));
    }

    #[test]
    fn round_trip_one_and_many_chunks() {
        for size in [512usize, 163_840, 300_000] {
            let mut img = vec![0u8; size];
            let mut x = 12345u32;
            // Incompressible bytes in the larger images so the stream spans several chunks.
            for (i, b) in img.iter_mut().enumerate() {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                if size > 200_000 || i < 64 {
                    *b = (x >> 16) as u8;
                }
            }
            let stored = encrypt(&img, "pässwörd").unwrap();
            let crc = crc32fast_hash(&img);
            assert_eq!(decrypt(&stored, "pässwörd", crc, size as u64).unwrap(), img);
            assert!(matches!(
                decrypt(&stored, "password", crc, size as u64),
                Err(Error::WrongPassword)
            ));
            let multi = frames(&stored, 0).is_none();
            assert_eq!(multi, size > 200_000, "prefix only with several chunks");
            if multi {
                // The prefix holds the CRCs of the first chunk's plaintext and ciphertext.
                let first = &stored[12..12 + CHUNK];
                let mut plain = first.to_vec();
                key("pässwörd").cbc_decrypt(&mut plain);
                assert_eq!(stored[0..4], crc32fast_hash(&plain).to_le_bytes());
                assert_eq!(stored[4..8], crc32fast_hash(first).to_le_bytes());
            }
        }
    }
}
