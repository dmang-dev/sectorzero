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

- **Macrium Reflect X file layout** (https://github.com/macrium/mrimgx_file_layout), MIT,
  Copyright (c) 2024 Paramount Software UK Limited. `crates/sectorzero-core/src/container/mrimgx.rs`
  follows its documentation (`docs/FILE_LAYOUT.md`, `docs/ENCRYPTION.md`, the JSON schema) and
  the reading steps of its reference reader and restore code (`src/libs/file_reader`,
  `src/libs/restore`, `src/libs/encryption`): the footer and metadata block headers, the
  packed block index records, delta indexes over the full image, where restored blocks go
  on the disk, and the key derivation and IVs of encrypted images. The Rust code is new; no
  C++ was copied. The MIT license asks that its copyright notice be kept with substantial
  portions; it is repeated in that file's header.

Otherwise none. Containers, partition tables, FAT tools (defrag, boot sector, resize), ISO 9660 and
ext2/3/4 are written from the published specifications: Microsoft's FAT specification, the
VHD and VHDX specifications (MS-VHDX), VMware's Virtual Disk Format 1.1, UEFI (GPT), ECMA-119
(ISO 9660), Joliet, Rock Ridge (IEEE P1282), El Torito, the ext4 on-disk layout in the
Linux kernel documentation (Documentation/filesystems/ext4), partclone's description of
its image formats (IMAGE_FORMATS.md; no partclone code was read or used), and the Keramics
format notes for Apple disk images (`docs/formats/src/udif.md`, `sparseimage.md`,
`sparsebundle.md`, `adc.md`; https://github.com/keramics/keramics, Apache-2.0; only the
documentation was read, no Keramics code), and libyal's "Expert Witness Compression Format
(EWF)" document for EnCase evidence files (`documentation/` in https://github.com/libyal/libewf;
only that document was read, no libewf code, which is LGPL; ewfacquire from libewf serves only
as a black-box test oracle in CI). No code from e2fsprogs or Linux
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
| [flate2](https://github.com/rust-lang/flate2-rs) | Compressed QCOW2 clusters, .gz images | MIT OR Apache-2.0 |
| [lzma-rust2](https://github.com/hasenbanck/lzma-rust2) | .xz images | Apache-2.0 |
| [sevenz-rust2](https://github.com/hasenbanck/sevenz-rust2) | .7z archives holding an image | Apache-2.0 |
| [zstd](https://github.com/gyscos/zstd-rs) (with the zstd C library) | .zst images, zstd QCOW2 clusters | BSD-3-Clause (zstd-sys: BSD-3-Clause AND BSD-2-Clause) |
| [bzip2](https://github.com/trifectatechfoundation/bzip2-rs) (with libbz2-rs-sys) | .bz2 images | MIT OR Apache-2.0; bzip2-1.0.6 (libbz2-rs-sys) |
| [aes](https://github.com/RustCrypto/block-ciphers) | Encrypted Macrium Reflect X images | MIT OR Apache-2.0 |
| [plist](https://github.com/ebarnard/rust-plist) | Property lists in Apple disk images and sparse bundles | MIT |
| [lzfse_rust](https://github.com/shampoofactory/lzfse_rust) | LZFSE blocks in Apple disk images | MIT OR Apache-2.0 |
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

- `crates/sectorzero-core/tests/data/qemu-zstd.qcow2` is a 1.44 MB FAT12 floppy made by
  SectorZero (one README.TXT), converted by `qemu-img convert -c -o compression_type=zstd`
  (QEMU 11.1). It contains only that data, no QEMU code.
- `crates/sectorzero-core/tests/data/qemu-stream.vmdk` is a 1.44 MB FAT12 floppy made by
  SectorZero (one NOTE.TXT), converted by `qemu-img convert -O vmdk -o subformat=streamOptimized`
  (QEMU 11.1). It contains only that data, no QEMU code.

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
