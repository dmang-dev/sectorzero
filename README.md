# SectorZero

An open-source (MIT) disk image utility: create floppy, hard disk and CD images, browse them,
and inject or extract files without mounting anything. It reads and writes the formats WinImage
uses (IMA, IMZ, WLZ, VHD, VMDK) and aims to be a free alternative to it.

SectorZero is an independent project. It is not affiliated with or endorsed by WinImage or
Gilles Vollant Software; WinImage is their trademark and is named here only to describe
file-format compatibility.

See [RESEARCH.md](RESEARCH.md) for WinImage's full feature set and how this project maps onto it.

## Status

Windows first. Linux builds come from CI; macOS and other platforms come later.

| Area | Works now |
|---|---|
| Containers | Raw sector images (`.ima` `.img` `.vfd` `.flp` `.dsk`, short "truncated" images read as zero-padded), split raw sets (`.001` `.002` …). WinImage IMZ (zip) and WLZ (zip holding several images), also password-encrypted the way WinImage encrypts them (AES-128): open, create, convert, and set, change or remove the password. VHD fixed, dynamic and differencing, including Virtual PC undo disks (`.vud`). VHDX (dynamic and fixed, with log replay). VMDK monolithicSparse, monolithicFlat and descriptor + extents. VirtualBox VDI (fixed and dynamic). QCOW2 v2/v3 (compressed clusters and backing files read; writes refused for encrypted, snapshotted or zstd images). PC-98 NHD, HDI and FDI. All read/write. CD images: ISO, BIN/CUE (MODE1/MODE2, 2048–2448-byte sectors), Nero NRG and Alcohol MDS/MDF, read-only. |
| Layout | Bare FAT volumes, MBR disks (primary and logical partitions), GPT disks, Apple partition maps (Macintosh disks and Mac-formatted Zip, Jaz and SyQuest cartridges), ISO 9660 CD/DVD images |
| Devices | Physical drives (USB sticks, memory cards, floppy drives, disks) open like raw images: `\\.\PhysicalDriveN` or `\\.\A:` on Windows, `/dev/sdb` or `/dev/fd0` on Linux. List them (`s0 drives`), browse them, read a whole drive into any container, write an image back onto a drive with read-back verification, copy a whole drive onto another, wipe a drive (zeros, optionally random data first, verified), and eject removable media. Zip, Jaz, SuperDisk and magneto-optical drives (including 2048-byte-sector MO) work like any other drive when the system sees them as disks (USB, ATAPI, SCSI); parallel-port Zip drives need Linux, since current Windows has no driver for them. Writing locks and dismounts the drive's volumes on Windows and refuses mounted drives on Linux; the disk holding the running system is never written, and fixed disks need an explicit confirmation. Needs administrator rights (root on Linux). |
| File systems | FAT12 / FAT16 / FAT32 with long file names (read/write). exFAT (read/write; volumes with two FATs (TexFAT) or that were not cleanly unmounted open read-only). ext2, ext3 and ext4 (read/write: extents, 64-bit, flex_bg, metadata checksums, uninitialized groups, hashed folders; the journal must be clean and stays clean; inline data, encryption, bigalloc, quotas and casefolding open read-only). NTFS (read-only, through the `ntfs` crate; compressed and encrypted files are refused). HFS (Mac OS Standard) and HFS+ / HFSX (Mac OS Extended) (read/write, with resource forks, Finder type/creator/flags, hard links and Mac names; journaled HFS+ and HFS+ inside an HFS wrapper open read-only). ISO 9660 with Joliet and Rock Ridge names (read-only), El Torito boot catalog |
| Create | Removable media at their exact capacity and laid out as their makers formatted them for PCs: Iomega Zip 100/250 and Jaz 1/2 GB (one FAT16 partition in MBR slot 4 at sector 32), LS-120 SuperDisk and 3.5" magneto-optical 128/230/640 MB and 1.3 GB (no partition table; 640 MB and 1.3 GB with 2048-byte sectors); images of those sizes are labelled with the medium. exFAT from 1 MiB up (Windows' default cluster sizes; MBR type 0x07 / GPT basic data when partitioned). ext2, ext3 or ext4 on any floppy or hard disk size (MBR type 0x83 / GPT Linux filesystem when partitioned). HFS from 400 KB and HFS+ (no journal), bare like Mac floppies or on an Apple partition map like Mac-formatted cartridges. Every standard floppy format (160 KB to 2.88 MB, including DMF and 1.68/1.72 MB); custom layouts (total sectors, sectors per track, heads, cluster size, root entries, FAT count and type, media byte, hidden sectors, drive number), or the layout copied from another image or boot sector; FAT volumes and MBR or GPT hard disk images of any size, in any of the writable containers above; differencing VHDs over an existing VHD |
| Edit | Inject files and folders, extract, delete, create folders, rename/move. Host file timestamps are kept. File attributes (read-only, hidden, system, archive) and modified/created/accessed dates, also recursively (FAT and exFAT); on ext, read-only and the three times (read-only clears the write permission bits); on HFS/HFS+, locked, hidden and the created/modified dates. Mac resource forks and Finder information go in and out as AppleDouble (`._name` files) or MacBinary, or are left out. NTFS and ISO properties are shown read-only. Replace a file inside an ISO in place when the new file fits in the old one's sectors. |
| Tools | Volume label; boot sector view/edit (OEM name, serial, geometry, hidden sectors, drive number), export, and boot-code import that keeps the BPB; MBR export/import; defragment (with optional free-space clearing); change format / resize, keeping files, dates, attributes, label, serial and boot code; resize MBR and GPT disks (the last FAT partition grows or shrinks with the disk, or only the disk and partition table change); save as another container; truncate unused space off raw images; checksums (CRC-32, Adler-32, MD5, SHA-1, SHA-256) of the disk inside an image (the same for every container), of the image file, or of a file inside it; file lists as text, CSV or HTML; self-extracting images (`s0 sfx`); build ISO images (optionally bootable, BIOS or UEFI); save an ISO's boot image |
| Desktop app | Everything above (drives through the Drives dialog: open, read to image, write image, copy to another drive, wipe, eject), plus double-click to open a file with its default program, and dragging files out to Explorer or the desktop |
| Languages | English, Čeština, Deutsch, Español, Français, Italiano, Magyar, Nederlands, Polski, Português (Brasil), Türkçe, Русский, Українська, 日本語, 한국어, 简体中文 and 繁體中文: the desktop app (following the system language; picker in the status bar), the engine's messages (errors, read-only reasons) and the `s0` CLI's output and help (`--lang`, else `SECTORZERO_LANG`, `LANG` or the system language). Catalogs: `app/ui/locales/` (checked by `node app/i18n-check.mjs`) and `crates/sectorzero-i18n/locales/` (checked by `cargo test -p sectorzero-cli`). Clap's own parse errors stay English |

Verified against other software: Linux (Alpine, in QEMU) checks SectorZero's ext2/ext3/ext4
images clean with `e2fsck -fn`, mounts them and reads every file, and SectorZero reads
ext2/ext3/ext4 volumes made by mke2fs and filled by the kernel (large, sparse, hard-linked and
symlinked files, hashed folders), writes to them (including attribute and date changes), and
`e2fsck` still finds them clean. Linux's exFAT driver mounts SectorZero's exFAT floppies, USB
sticks (MBR) and GPT disks, reads every file, label, time and read-only attribute, and writes to
them; SectorZero then reads the kernel's changes and writes more, and the kernel reads those. The same round trip runs for HFS and HFS+ (floppies and Zip 100 cartridges with an Apple partition map): the Linux hfs and hfsplus drivers mount SectorZero's volumes, read every file and name, write to them, and SectorZero's consistency check and file reads agree with what the kernel wrote. Real Macs have not been tried yet. The WinImage SDK reads images made here (IMA, IMZ, VHD fixed/dynamic,
VMDK sparse/flat, MBR and GPT disks, defragmented and resized images) with byte-identical files,
and SectorZero reads the SDK's VHD and VMDK output. WinImage itself opens SectorZero's
password-encrypted IMZ images, and SectorZero opens the ones WinImage encrypted.
Windows mounts the ISO images. VHDX is
checked by this project's own tests only so far.

Not yet (see RESEARCH.md §5): low-level floppy formatting, writing to NTFS,
writing to ISO images beyond in-place replacement, resizing non-FAT partitions.

## Layout

```
crates/sectorzero-core   the engine (library): containers, partitions, file systems, tools
crates/sectorzero-cli    command-line tool, installed as `s0`
crates/sectorzero-i18n   translations of engine messages and the CLI (JSON catalogs)
app/                     Tauri 2 desktop app (Rust shell in src-tauri/, plain HTML/JS UI in ui/)
third_party/fatfs        vendored fatfs 0.3.6 with one fix (see SECTORZERO-PATCH.md there)
```

## Building

`rust-toolchain.toml` asks for stable Rust with clippy and rustfmt. On Windows, use the MSVC
toolchain. If your rustup default is the GNU one, set an override for this folder:

```powershell
rustup override set stable-x86_64-pc-windows-msvc
```

Requirements:

- **Windows:** Rust via rustup, Visual Studio Build Tools with the C++ workload, Node.js (for
  the Tauri CLI only), and WebView2 (it ships with Windows 10 and 11).
- **Linux (Debian/Ubuntu):** Rust via rustup, Node.js, and the Tauri packages:
  `sudo apt install build-essential pkg-config libwebkit2gtk-4.1-dev libgtk-3-dev libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev file`

```powershell
cargo test --workspace
cargo run -p sectorzero-cli -- --help
```

Desktop app:

```powershell
cd app
npm install
npm run dev      # run with live reload of ui/
npm run build    # release build and installer (NSIS on Windows)
```

On Linux, `npx tauri build --bundles deb,appimage` builds the .deb and AppImage.

## CI and releases

`.github/workflows/ci.yml` runs rustfmt, clippy (warnings are errors) and the tests on Windows
(x64 and ARM64), Linux and macOS for every push to `main` and every pull request.

`.github/workflows/release.yml` builds the Windows installers (x64, ARM64 and 32-bit x86), the
Linux .deb and AppImage, and zipped `s0` CLI binaries, with SHA-256 checksums. All Windows builds
need Windows 10 or later (Rust's minimum; the app also needs WebView2). To release, bump `version` in `Cargo.toml` and
`app/src-tauri/tauri.conf.json`, then push a matching tag (`git tag v0.2.0 && git push origin v0.2.0`).
The files land on a draft GitHub release to review and publish. Running the workflow by hand
builds the same files as workflow artifacts without making a release.

The binaries are not code-signed yet, so Windows SmartScreen warns about an unknown publisher.

## CLI

```powershell
s0 formats                                   # list floppy formats
s0 new boot.ima -f 1440 -l MYDISK            # new empty 1.44 MB floppy
s0 new boot.imz -f 1440                      # same, WinImage-compressed
s0 new safe.imz -f 1440 --encrypt -          # encrypted with a password ("-" reads it from stdin)
s0 ls safe.imz --password -                  # open an encrypted IMZ/WLZ
s0 password safe.imz --password old --set new  # change it (--remove stores it unencrypted)
s0 new data.img -s 64M -t none               # bare FAT16 volume
s0 new odd.ima --sectors 1600 --sectors-per-track 10 --heads 2 --fat 12 --root-entries 64
s0 new copy.img --like other.ima             # same layout as another image or a 512-byte boot sector
s0 new disk.vhdx -s 2G -t gpt -l DATA        # dynamic VHDX with a GPT partition (--fixed for fixed size)
s0 new linux.img -s 256M -t mbr --fs ext4     # ext2/ext3/ext4 instead of FAT; inject/extract/rm/mv/attrib work the same
s0 new stick.img -s 8G -t mbr --fs exfat -l DATA   # exFAT
s0 new mac.img -f zip100 --fs hfs -l Zip     # Mac-formatted Zip (Apple partition map); -f 1440 --fs hfs for a Mac floppy
s0 extract mac.img -o .\out --forks appledouble   # Mac resource forks as ._ files (or macbinary); inject takes the same
s0 inject boot.ima .\files\* --to /tools     # copy files in (folders recurse)
s0 ls boot.ima -r                            # list everything
s0 extract boot.ima /tools -o .\out          # copy out (no path = everything)
s0 rm boot.ima /tools -r
s0 mv boot.ima /a.txt /b.txt
s0 info disk.vhd -p 2                        # partition 2 of an MBR or GPT disk
s0 label boot.ima NEWLABEL                   # or --clear
s0 boot boot.ima --serial 1A2B-3C4D --oem MSWIN4.1
s0 boot boot.ima --export bs.bin             # --import copies boot code, keeps the BPB; --mbr for the MBR
s0 defrag boot.ima --zero-free               # --analyze only reports
s0 resize boot.ima -f 2880                   # or -s 32M; files are kept
s0 new zip.img -f zip100                     # removable media presets: zip100 zip250 jaz1g jaz2g ls120 mo128 mo230 mo640 mo1300
s0 resize disk.vhd -s 2G                     # MBR/GPT disk: the last FAT partition grows with it (--keep-partitions: table only)
s0 convert boot.ima boot.vhdx                # container from the extension, or -c raw|imz|vhd|vhdx|vmdk|vdi|qcow2|nhd|hdi|fdi
s0 convert game.cue game.iso                 # CD images (BIN/CUE, NRG, MDS/MDF) to plain ISO
s0 diff base.vhd changes.vud                 # differencing / undo disk; writes go to the new file only
s0 drives                                    # physical drives (as administrator / root to open them)
s0 convert \\.\PhysicalDrive2 stick.vhdx      # read a whole drive into an image, with progress
s0 write-drive stick.vhdx \\.\PhysicalDrive2  # write an image onto a drive and verify it (--allow-fixed for hard disks)
s0 write-drive \\.\PhysicalDrive2 \\.\PhysicalDrive3   # drive-to-drive copy, verified
s0 eject \\.\PhysicalDrive2                  # eject a Zip / Jaz / SuperDisk / MO cartridge
s0 wipe-drive \\.\PhysicalDrive2             # zero the whole drive and verify (--random for a random pass first)
s0 attrib boot.ima /a.txt --set RH --modified "1995-08-24 09:30"   # -r for folders; no options shows them
s0 checksum boot.ima                         # disk inside the image; add a path for one file, --file for the file as stored
s0 ls boot.ima --format html -o list.html    # whole tree; also txt or csv
s0 truncate boot.ima                         # cut free space off the end of a raw image
s0 iso-replace cd.iso /DOCS/README.TXT new.txt
s0 sfx boot.ima -o boot.exe                  # self-extracting image; run boot.exe --help
s0 iso .\cdroot cd.iso -l MYCD -b boot.ima   # bootable ISO (--efi, --no-emulation)
s0 boot-image cd.iso -o boot.ima
```

Add `--json` to any command for machine-readable output. For WLZ files, `--member` picks the image.
`--lang de` (or any of the languages above) translates messages and `--help`; JSON output stays as is.

## License policy

SectorZero is MIT-licensed. Code from MIT, BSD, Apache-2.0 or zlib-licensed projects can be
reused or ported. Keep the original notice and list the project in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

Do not copy or port code from GPL, LGPL or AGPL projects (for example mtools, FATtools,
DiskImageTool or qemu). Work from the published format specifications instead.
