# Third-party notices

SectorZero is MIT-licensed. This file credits third-party code that is reused, ported,
or shipped with the program.

## Vendored and modified code

- **fatfs 0.3.6** (https://github.com/rafalh/rust-fatfs), MIT, Copyright (c) 2017 Rafał Harabień.
  A copy lives in `third_party/fatfs` with its original LICENSE.txt and is used through
  `[patch.crates-io]`. It is changed so that new folders start with plain `.` and `..` entries
  (upstream writes long-name entries before them, which WinImage and DOS reject). Details in
  `third_party/fatfs/SECTORZERO-PATCH.md`.

## Ported or adapted code

None. Containers, partition tables, FAT tools (defrag, boot sector, resize), ISO 9660 and
ext2/3/4 are written from the published specifications: Microsoft's FAT specification, the
VHD and VHDX specifications (MS-VHDX), VMware's Virtual Disk Format 1.1, UEFI (GPT), ECMA-119
(ISO 9660), Joliet, Rock Ridge (IEEE P1282), El Torito, and the ext4 on-disk layout in the
Linux kernel documentation (Documentation/filesystems/ext4). No code from e2fsprogs or Linux
(both GPL) was used; Linux and e2fsck serve only as test oracles in a virtual machine. The WinImage SDK was used only as an external
test oracle on a developer machine. None of it is in this repository or linked into the program.

When code is ported from another project (for example DiscUtils or TotalImage,
both MIT), add an entry here with the project, its URL, its license and copyright line,
and the files affected. Also keep the original copyright notice in those files.

## Direct dependencies

| Crate | Used for | License |
|---|---|---|
| [fatfs](https://github.com/rafalh/rust-fatfs) 0.3 (vendored, see above) | FAT12/16/32 engine | MIT |
| [zip](https://github.com/zip-rs/zip2) | IMZ and WLZ containers | MIT |
| [crc32fast](https://github.com/srijs/rust-crc32fast) | GPT checksums, CRC-32 | MIT OR Apache-2.0 |
| [uuid](https://github.com/uuid-rs/uuid) | GPT, VHD and VHDX identifiers | MIT OR Apache-2.0 |
| [tempfile](https://github.com/Stebalien/tempfile) | Scratch images for change format | MIT OR Apache-2.0 |
| [chrono](https://github.com/chronotope/chrono) | Timestamp conversion | MIT OR Apache-2.0 |
| [thiserror](https://github.com/dtolnay/thiserror) | Error types | MIT OR Apache-2.0 |
| [serde](https://github.com/serde-rs/serde) / serde_json | Data passed to the CLI's JSON output and the UI | MIT OR Apache-2.0 |
| [clap](https://github.com/clap-rs/clap) | CLI argument parsing | MIT OR Apache-2.0 |
| [anyhow](https://github.com/dtolnay/anyhow) | CLI error reporting | MIT OR Apache-2.0 |
| [tauri](https://github.com/tauri-apps/tauri) 2, tauri-plugin-dialog | Desktop app shell, native dialogs | MIT OR Apache-2.0 |
| [tauri-plugin-opener](https://github.com/tauri-apps/plugins-workspace) | Opening a copied-out file with its default program | MIT OR Apache-2.0 |
| [drag](https://github.com/crabnebula-dev/drag-rs) | Native drag of files out of the app window | MIT OR Apache-2.0 |
| [md-5, sha1, sha2](https://github.com/RustCrypto/hashes) | Image and file checksums | MIT OR Apache-2.0 |
| [adler2](https://github.com/oyvindln/adler2) | Adler-32 checksums | 0BSD OR MIT OR Apache-2.0 |
| [flate2](https://github.com/rust-lang/flate2-rs) | Compressed QCOW2 clusters | MIT OR Apache-2.0 |
| [ntfs](https://github.com/ColinFinck/ntfs) | Reading NTFS volumes | MIT OR Apache-2.0 |
| [unicode-normalization](https://github.com/unicode-rs/unicode-normalization) | Composing and decomposing HFS+ names (NFC / NFD) | MIT OR Apache-2.0 |

## Test data

- `crates/sectorzero-core/tests/data/ntfs-testfs1.img.gz` is `testdata/testfs1` from the
  ntfs crate 0.4.0 (https://github.com/ColinFinck/ntfs), gzip-compressed. It was made with
  mkntfs by that project's `create-testfs1.sh`. MIT OR Apache-2.0, Copyright 2021 Colin Finck.
  Its file record 255 has a damaged update sequence in the original.
- `crates/sectorzero-core/tests/data/mke2fs-ext4.img.gz` is a 24 MiB ext4 file system made
  for this project by mkfs.ext4 (e2fsprogs 1.47.4, default options) in an Alpine Linux guest
  and filled with test files by the Linux kernel. It contains only that generated data, no
  e2fsprogs code.

## Transitive dependencies

As of 2026-11-23, the full dependency tree has no GPL, LGPL or AGPL-only crates. (r-efi is
offered under MIT OR Apache-2.0 OR LGPL-2.1-or-later; it is used under MIT.) Most crates are
MIT and/or Apache-2.0. The others:

- **MPL-2.0** (cssparser, selectors, dtoa-short, option-ext). These are pulled in by Tauri.
  MPL-2.0 is file-level copyleft. It allows use in an MIT application, as long as the
  MPL files themselves are not modified.
- **Unicode-3.0, Zlib, BSD-3-Clause, ISC, Unlicense, CC0**. All are permissive.

The ntfs crate brings binrw, nt-string, memoffset, derive_more, strum_macros and others; all
are MIT, MIT OR Apache-2.0, or offer one of those (byteorder: Unlicense OR MIT; bytemuck:
Zlib OR Apache-2.0 OR MIT).

Run `cargo metadata` (or `cargo deny check licenses` once cargo-deny is set up) to re-check
after changing dependencies.
