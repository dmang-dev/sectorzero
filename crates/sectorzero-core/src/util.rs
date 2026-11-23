//! Small byte-level helpers shared by the container and file system code.

use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
pub(crate) fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
pub(crate) fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
pub(crate) fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
pub(crate) fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}
pub(crate) fn put_le16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn put_le32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn put_le64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn put_be16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_be_bytes());
}
pub(crate) fn put_be32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}
pub(crate) fn put_be64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_be_bytes());
}

/// CRC-32C (Castagnoli), as used by VHDX headers and region tables.
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0x82F6_3B78 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        t
    });
    let mut crc = !0u32;
    for &b in data {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// A new random GUID in on-disk (mixed-endian) byte order.
pub(crate) fn new_guid() -> [u8; 16] {
    uuid::Uuid::new_v4().to_bytes_le()
}

/// Parse a GUID string into on-disk (mixed-endian) byte order.
pub(crate) fn guid(s: &str) -> [u8; 16] {
    uuid::Uuid::parse_str(s)
        .expect("valid GUID literal")
        .to_bytes_le()
}

pub(crate) fn guid_string(b: &[u8]) -> String {
    let bytes: [u8; 16] = b[..16].try_into().unwrap();
    uuid::Uuid::from_bytes_le(bytes)
        .hyphenated()
        .to_string()
        .to_uppercase()
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn round_up(v: u64, to: u64) -> u64 {
    v.div_ceil(to) * to
}

pub(crate) fn is_zero(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_known_vector() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn guid_roundtrip_is_mixed_endian() {
        let g = guid("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(&g[..4], &[0x28, 0x73, 0x2A, 0xC1]);
        assert_eq!(guid_string(&g), "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
    }
}
