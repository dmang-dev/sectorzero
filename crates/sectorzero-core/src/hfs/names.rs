//! Catalog names: on-disk forms, display forms and the order catalog B-trees keep them in.
//!
//! HFS names are up to 31 Mac Roman bytes, compared through a per-byte weight (case-blind,
//! accent-sensitive). HFS+ names are up to 255 UTF-16 units, stored decomposed; they compare
//! unit by unit after case folding, skipping a few invisible formatting characters. HFSX
//! may instead compare binary. On every volume a '/' in a Mac name is shown as ':' (':' is
//! the Mac path separator and '/' ours). See `tables.rs` for where the tables come from.

use std::cmp::Ordering;

use unicode_normalization::UnicodeNormalization;

use super::tables::{HFS_WEIGHT, PLUS_DECOMPOSE, PLUS_FOLD, PLUS_KEEP};
use super::{mac_roman_to_string, string_to_mac_roman};

/// How a catalog compares names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    /// HFS: Mac Roman weights.
    Hfs,
    /// HFS+ (and HFSX with key compare type 0xCF): case-folded UTF-16.
    Folded,
    /// HFSX with key compare type 0xBC: binary UTF-16.
    Binary,
}

pub(crate) fn hfs_cmp(a: &[u16], b: &[u16]) -> Ordering {
    let w = |c: u16| HFS_WEIGHT[usize::from(c as u8)];
    a.iter().map(|&c| w(c)).cmp(b.iter().map(|&c| w(c)))
}

/// The folded form of one HFS+ name unit (0: ignored when comparing).
pub(crate) fn plus_fold(u: u16) -> u16 {
    match PLUS_FOLD.binary_search_by_key(&u, |&(k, _)| k) {
        Ok(i) => PLUS_FOLD[i].1,
        Err(_) => u,
    }
}

pub(crate) fn plus_cmp(a: &[u16], b: &[u16]) -> Ordering {
    let f = |s: &[u16]| -> Vec<u16> {
        s.iter()
            .map(|&u| plus_fold(u))
            .filter(|&u| u != 0)
            .collect()
    };
    f(a).cmp(&f(b))
}

/// The form of a name the catalog order compares: names with equal sort keys are the same
/// name (one of them can exist per folder).
pub(crate) fn sort_key(order: Order, name: &[u16]) -> Vec<u16> {
    match order {
        Order::Hfs => name
            .iter()
            .map(|&c| HFS_WEIGHT[usize::from(c as u8)])
            .collect(),
        Order::Folded => name
            .iter()
            .map(|&u| plus_fold(u))
            .filter(|&u| u != 0)
            .collect(),
        Order::Binary => name.to_vec(),
    }
}

pub(crate) fn cmp(order: Order, a: &[u16], b: &[u16]) -> Ordering {
    match order {
        Order::Hfs => hfs_cmp(a, b),
        Order::Folded => plus_cmp(a, b),
        Order::Binary => a.cmp(b),
    }
}

fn kept(c: u16) -> bool {
    PLUS_KEEP
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                Ordering::Less
            } else if lo > c {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// The stored (decomposed) UTF-16 of an HFS+ name, character by character.
pub(crate) fn plus_decompose(name: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(name.len());
    for ch in name.chars() {
        let Ok(c) = u16::try_from(u32::from(ch)) else {
            // Outside the BMP: stored as is.
            let mut b = [0u16; 2];
            out.extend_from_slice(ch.encode_utf16(&mut b));
            continue;
        };
        if kept(c) {
            out.push(c);
        } else if let Ok(i) = PLUS_DECOMPOSE.binary_search_by_key(&c, |&(k, _)| k) {
            out.extend_from_slice(PLUS_DECOMPOSE[i].1);
        } else {
            let mut b = [0u16; 2];
            for d in ch.to_string().nfd() {
                out.extend_from_slice(d.encode_utf16(&mut b));
            }
        }
    }
    out
}

/// The Mac name for a displayed name (':' back to '/').
fn mac_form(name: &str) -> String {
    name.chars()
        .map(|c| if c == ':' { '/' } else { c })
        .collect()
}

/// Displayed name of an HFS+ name: composed where that maps back to exactly the stored
/// units (so the displayed name always finds the file again), otherwise as stored.
pub(crate) fn plus_display(units: &[u16]) -> String {
    let raw = String::from_utf16_lossy(units);
    // Undo the decompositions that differ from Unicode's, then compose the rest.
    let mut undone = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let hit = PLUS_DECOMPOSE
            .iter()
            .filter(|(_, s)| units[i..].starts_with(s))
            .max_by_key(|(_, s)| s.len());
        if let Some((c, s)) = hit {
            undone.push(*c);
            i += s.len();
        } else {
            undone.push(units[i]);
            i += 1;
        }
    }
    let composed: String = String::from_utf16_lossy(&undone).nfc().collect();
    let shown = if plus_decompose(&composed) == units {
        composed
    } else {
        raw
    };
    shown
        .chars()
        .map(|c| if c == '/' { ':' } else { c })
        .collect()
}

/// Stored form of a displayed HFS+ name, or an error message.
pub(crate) fn plus_disk(name: &str) -> Result<Vec<u16>, String> {
    let u = plus_decompose(&mac_form(name));
    if u.is_empty() || u.len() > 255 {
        return Err("an HFS+ name is 1-255 UTF-16 units".into());
    }
    Ok(u)
}

/// Displayed name of an HFS name (Mac Roman bytes as u16).
pub(crate) fn hfs_display(units: &[u16]) -> String {
    let b: Vec<u8> = units.iter().map(|&u| u as u8).collect();
    mac_roman_to_string(&b)
        .chars()
        .map(|c| if c == '/' { ':' } else { c })
        .collect()
}

/// Stored form of a displayed HFS name: 1-`max` Mac Roman characters.
pub(crate) fn hfs_disk(name: &str, max: usize) -> Result<Vec<u16>, String> {
    match string_to_mac_roman(&mac_form(name)) {
        Some(b) if !b.is_empty() && b.len() <= max => Ok(b.into_iter().map(u16::from).collect()),
        Some(_) => Err(format!("an HFS name is 1-{max} characters")),
        None => Err(format!(
            "HFS names use Mac Roman characters only: {name:?} cannot be stored"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn hfs_order_is_case_blind_and_accent_aware() {
        let d = |s: &str| hfs_disk(s, 31).unwrap();
        assert_eq!(hfs_cmp(&d("ReadMe"), &d("README")), Ordering::Equal);
        assert_eq!(hfs_cmp(&d("Äpfel"), &d("äpfel")), Ordering::Equal);
        assert_ne!(hfs_cmp(&d("Äpfel"), &d("Apfel")), Ordering::Equal);
        assert_ne!(hfs_cmp(&d("Ö"), &d("Ä")), Ordering::Equal);
        assert_eq!(hfs_cmp(&d("a"), &d("ab")), Ordering::Less);
        assert_eq!(hfs_cmp(&d("B"), &d("a")), Ordering::Greater);
        assert!(hfs_disk("日本", 31).is_err());
        assert_eq!(hfs_disk("a:b", 31).unwrap(), u("a/b"));
        assert_eq!(hfs_display(&u("a/b")), "a:b");
    }

    #[test]
    fn plus_order_folds_case_and_skips_ignorables() {
        let d = |s: &str| plus_disk(s).unwrap();
        assert_eq!(plus_cmp(&d("Straße"), &d("STRAßE")), Ordering::Equal);
        assert_eq!(plus_cmp(&d("Ωmega"), &d("ωMEGA")), Ordering::Equal);
        assert_eq!(plus_cmp(&d("a\u{200D}b"), &d("ab")), Ordering::Equal);
        assert_eq!(plus_cmp(&d("é"), &d("e\u{301}")), Ordering::Equal);
        assert_ne!(plus_cmp(&d("é"), &d("e")), Ordering::Equal);
        assert_eq!(plus_cmp(&d("a"), &d("B")), Ordering::Less);
        // Unicode 3.0+ case pairs are not folded by HFS+.
        assert_ne!(plus_cmp(&d("\u{218}"), &d("\u{219}")), Ordering::Equal);
    }

    /// Replays the kernel observations (`SZ_HFS_ORDER=<work-order folder>`): every on-disk
    /// order, every name the kernel considered equal to another, and (HFS+) every stored
    /// decomposition must come out the same here.
    #[test]
    #[ignore]
    fn kernel_collation() {
        let dir = std::path::PathBuf::from(std::env::var("SZ_HFS_ORDER").expect("SZ_HFS_ORDER"));
        let hex = |s: &str| u16::from_str_radix(s, 16).unwrap();
        for (set, order) in [("hfs", Order::Hfs), ("plus", Order::Folded)] {
            for folder in ["one", "two"] {
                let text =
                    std::fs::read_to_string(dir.join(format!("{set}.{folder}.txt"))).unwrap();
                let mut ents: Vec<(Vec<u16>, u32)> = Vec::new();
                let mut dups = Vec::new();
                for line in text.lines() {
                    let p: Vec<&str> = line.split(' ').collect();
                    if p[0] == "E" {
                        let id = p[p.len() - 1].parse().unwrap();
                        ents.push((p[1..p.len() - 1].iter().map(|x| hex(x)).collect(), id));
                    } else if p[2] != "ERR" {
                        let linux: Vec<u16> = p[3..].iter().map(|x| hex(x)).collect();
                        dups.push((p[2].parse::<u32>().unwrap(), linux));
                    }
                }
                for w in ents.windows(2) {
                    assert_eq!(
                        cmp(order, &w[0].0, &w[1].0),
                        Ordering::Less,
                        "{set}.{folder} {:x?} {:x?}",
                        w[0].0,
                        w[1].0
                    );
                }
                let by_id: std::collections::HashMap<u32, &Vec<u16>> =
                    ents.iter().map(|(u, i)| (*i, u)).collect();
                for (m, linux) in &dups {
                    // Linux's ':' is the Mac '/'.
                    let linux: Vec<u16> = linux
                        .iter()
                        .map(|&c| if c == 0x3A { 0x2F } else { c })
                        .collect();
                    let mine = if order == Order::Hfs {
                        linux.clone()
                    } else {
                        plus_decompose(&String::from_utf16(&linux).unwrap())
                    };
                    assert_eq!(
                        cmp(order, &mine, by_id[m]),
                        Ordering::Equal,
                        "{set}.{folder} {linux:x?}"
                    );
                }
                if order == Order::Folded && folder == "one" {
                    for (u, id) in &ents {
                        let c = if *id == 0x3A { 0x2F } else { *id };
                        let s = format!("x{}y", char::from_u32(c).unwrap());
                        assert_eq!(&plus_decompose(&s), u, "decomposition of {c:04X}");
                    }
                }
            }
        }
    }

    #[test]
    fn plus_names_decompose_like_the_catalogs() {
        assert_eq!(plus_decompose("é"), u("e\u{301}"));
        assert_eq!(plus_decompose("가"), u("\u{1100}\u{1161}"));
        // Greek tonos is stored with U+030D, and U+2000-U+2FFF stays whole.
        assert_eq!(plus_decompose("ά"), u("\u{3B1}\u{30D}"));
        assert_eq!(plus_decompose("\u{212B}"), u("\u{212B}"));
        for s in ["Café Größe", "ά Ά ΐ", "가나다", "ᾀ", "a:b", "e\u{301}x"] {
            let disk = plus_disk(s).unwrap();
            let shown = plus_display(&disk);
            assert_eq!(plus_disk(&shown).unwrap(), disk, "{s}");
        }
        assert_eq!(plus_display(&plus_disk("Café").unwrap()), "Café");
        assert_eq!(plus_display(&plus_disk("ά").unwrap()), "ά");
        assert_eq!(plus_display(&u("a/b")), "a:b");
    }
}
