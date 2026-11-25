# SectorZero

An open-source (MIT) disk image utility: create floppy, hard disk and CD images, browse them,
and inject or extract files without mounting anything. It reads and writes the common floppy,
hard disk, virtual machine and CD image formats (see Status below).

SectorZero is an independent project. It is not affiliated with, endorsed by or sponsored by
any of the companies or projects whose products, file formats or hardware it names or works
with, including Gilles Vollant Software (WinImage), Microsoft (Windows, Virtual PC, Hyper-V),
VMware, Oracle (VirtualBox), Apple, Iomega, Imation, SyQuest, NEC, Nero and Alcohol Soft. All
product names and trademarks belong to their owners and are used only to identify the file
formats, media and devices SectorZero works with.

## Disclaimer

SectorZero reads and writes disk images and physical media (floppy disks, USB sticks, memory
cards, hard disks) at the sector level. Writing, wiping, resizing or editing can destroy data,
and reading old or damaged media can stress it further. Use it at your own risk: keep backups,
and check the target drive before writing to it. SectorZero is provided "as is", without
warranty of any kind, and its authors and contributors are not liable for any data loss,
hardware damage or other harm from using it. The [MIT license](LICENSE) has the full terms.

See [RESEARCH.md](RESEARCH.md) for the feature survey, format notes and roadmap.

## Status

Windows first. Linux builds come from CI; macOS and other platforms come later.

✅ yes · ⚠️ partly (see notes) · ❌ not yet · — does not apply

### Image files

| Format | Extensions | Open | Create | Save changes | Save as | Notes |
|---|---|:-:|:-:|:-:|:-:|---|
| Raw sector image | `.ima` `.img` `.vfd` `.flp` `.dsk` | ✅ | ✅ | ✅ | ✅ | Short ("truncated") images open as if padded with zeros |
| Split raw image | `.001` `.002` … | ✅ | ❌ | ✅ | ❌ | |
| WinImage IMZ | `.imz` | ✅ | ✅ | ✅ | ✅ | Optional password (AES-128): open, create, set, change or remove |
| WLZ (several images in one zip) | `.wlz` | ✅ | ❌ | ✅ | ❌ | Optional password; `--member` picks the image |
| VHD (Virtual PC, Hyper-V) | `.vhd` `.vud` | ✅ | ✅ | ✅ | ✅ | Fixed, dynamic and differencing (new ones with `s0 diff`); Virtual PC undo disks |
| VHDX (Hyper-V) | `.vhdx` `.avhdx` | ✅ | ✅ | ✅ | ✅ | Fixed, dynamic and differencing (Hyper-V checkpoints, new ones with `s0 diff`); the whole parent chain is followed |
| VMDK (VMware) | `.vmdk` | ✅ | ✅ | ✅ | ✅ | Monolithic sparse and flat, descriptor + extents, snapshots (new ones with `s0 diff`); stream-optimized ones open read-only |
| VDI (VirtualBox) | `.vdi` | ✅ | ✅ | ✅ | ✅ | Fixed, dynamic and differencing (snapshots; the parent is found by UUID next to it, in the folder above or through the `.vbox` file) |
| QCOW2 (QEMU) | `.qcow2` | ✅ | ✅ | ⚠️ | ✅ | v2/v3, deflate and zstd compressed clusters, backing files; images with internal snapshots open read-only; encrypted ones ❌ |
| PC-98 | `.nhd` `.hdi` `.fdi` | ✅ | ✅ | ✅ | ✅ | |
| ISO 9660 CD/DVD | `.iso` | ✅ | ✅ | ⚠️ | ❌ | Built from a folder, optionally bootable (BIOS or UEFI); a file can be replaced in place if the new one fits |
| CD images | `.cue` + `.bin`, `.nrg`, `.mds` + `.mdf` | ✅ | ❌ | ❌ | ❌ | MODE1/MODE2, 2048–2448-byte sectors |
| OVA (virtual appliance) | `.ova` | ✅ | ❌ | ❌ | ❌ | The disk inside opens (usually a stream-optimized VMDK); `--member` picks one of several |
| Windows Backup system image | a `WindowsImageBackup` folder | ✅ | ❌ | ❌ | ❌ | Open the drive, the `WindowsImageBackup` folder or a backup folder; the system disk of the newest backup by default, `--member` picks another disk or backup |
| Macrium Reflect X backup | `.mrimgx` | ✅ | ❌ | ❌ | ❌ | Full, incremental and differential images (the rest of the backup set is found in the same folder), zstd-compressed and password-encrypted ones; `--member` picks a disk of a multi-disk image. Images from Reflect 8 and older (`.mrimg`) ❌ |
| Clonezilla / Rescuezilla image | the image folder | ✅ | ❌ | ❌ | ❌ | Whole-disk backups open as the disk (MBR or GPT, put back together from the saved tables), partition backups as the partition; partclone images, compressed (gzip, zstd, xz, bzip2) and split (`.aa`, `.ab`, …); `--member` picks a disk or partition. ntfsclone and partimage images (old backups), lz4 and lzop ❌ |
| Partclone image | `.ptcl-img`, `-ptcl-img.gz.aa`, … | ✅ | ❌ | ❌ | ❌ | Formats 0001 and 0002, a single piece (split ones open through the Clonezilla folder) |
| Apple disk image | `.dmg` (+ `.dmgpart` segments) | ✅ | ❌ | ❌ | ❌ | Uncompressed (UDRO), zlib (UDZO), bzip2 (UDBZ), LZFSE (ULFO), xz (ULMO) and ADC (UDCO) images, segmented ones, old images with a resource fork. The volume inside is usually HFS+ (read) or APFS (❌). Encrypted images ❌ |
| Apple sparse image, sparse bundle | `.sparseimage`, a `.sparsebundle` folder | ✅ | ❌ | ❌ | ❌ | Encrypted ones ❌ |
| Compressed images | `.gz` `.xz` `.zst` `.bz2`, or a `.zip` / `.7z` holding the image | ✅ | ✅ | ✅ | ✅ | Any format above inside (`disk.img.xz`, `disk.vhd.gz`, a Raspberry Pi `.zip`); unpacked to a temporary folder while open; a `.7z` that holds other files too opens read-only |

"Save as" means another image can be converted to this format; every format that opens can be
converted from.

### File systems

| File system | Browse & extract | Add, delete, rename | Format new | Attributes & dates | Notes |
|---|:-:|:-:|:-:|:-:|---|
| FAT12 / FAT16 / FAT32 (long names) | ✅ | ✅ | ✅ | ✅ | Also defragment, resize keeping files, boot sector editing |
| exFAT | ✅ | ✅ | ✅ | ✅ | From 1 MiB; TexFAT volumes and volumes not cleanly unmounted open read-only |
| ext2 / ext3 / ext4 | ✅ | ✅ | ✅ | ✅ | Read-only flag and times; inline data, encryption, bigalloc, quotas and casefolding open read-only |
| HFS / HFS+ / HFSX (Mac) | ✅ | ✅ | ✅ | ✅ | Resource forks and Finder info (as AppleDouble or MacBinary), hard links; journaled HFS+ opens read-only |
| NTFS | ✅ | ❌ | ❌ | view only | Compressed and encrypted files are refused |
| ISO 9660 + Joliet + Rock Ridge | ✅ | ⚠️ | ✅ | view only | Replace a file in place; El Torito boot images |

### Disk layouts

| Layout | Open | Create | Resize | Notes |
|---|:-:|:-:|:-:|---|
| Single volume (floppies, partition images) | ✅ | ✅ | ⚠️ | Resizing keeps files on FAT |
| MBR | ✅ | ✅ | ⚠️ | Primary and logical partitions; resizing grows or shrinks the last FAT partition; MBR export/import |
| GPT | ✅ | ✅ | ⚠️ | Resizing as for MBR |
| Apple partition map | ✅ | ✅ | ❌ | Mac disks and Mac-formatted Zip, Jaz and SyQuest cartridges (HFS/HFS+) |
| El Torito (bootable CD) | ✅ | ✅ | — | Save an ISO's boot image |

New images come in every standard floppy format (160 KB to 2.88 MB, including DMF and
1.68/1.72 MB), custom floppy layouts, any hard disk size, and removable media at their exact
size, laid out the way their makers formatted them: Iomega Zip 100/250 and Jaz 1/2 GB, LS-120
SuperDisk, and 3.5" magneto-optical 128 MB to 1.3 GB.

### Physical drives

| | Windows | Linux |
|---|:-:|:-:|
| List drives (USB sticks, memory cards, floppy drives, disks) | ✅ | ✅ |
| Browse and edit the files on a drive | ✅ | ✅ |
| Read a drive into an image file | ✅ | ✅ |
| Write an image onto a drive, verified | ✅ | ✅ |
| Copy a drive onto another drive, verified | ✅ | ✅ |
| Wipe a drive (zeros, or random data then zeros) | ✅ | ✅ |
| Eject removable media | ✅ | ✅ |
| Zip, Jaz, SuperDisk and magneto-optical drives | ✅ | ✅ |
| Parallel-port Zip drives | ❌ | ✅ |

Drive paths are `\\.\PhysicalDriveN` or `\\.\A:` on Windows and `/dev/sdb` or `/dev/fd0` on
Linux. Working with drives needs administrator rights (root on Linux). The disk holding the
running system is never written, and fixed disks need an extra confirmation.

### Tools

- Checksums (CRC-32, Adler-32, MD5, SHA-1, SHA-256) of the disk inside an image, of the image
  file, or of a file inside it.
- Volume label; boot sector view and edit, export, and boot code import that keeps the disk
  layout.
- Defragment FAT volumes, optionally clearing free space.
- Change format / resize, keeping files, dates, attributes, label, serial and boot code.
- Truncate unused space off raw images.
- File lists as text, CSV or HTML.
- Self-extracting images (`s0 sfx`).
- Build ISO images from a folder, optionally bootable.

The **desktop app** does all of the above, including drives through its Drives dialog. It can
also open files with their default program and drag files out to Explorer or the desktop.

**Languages:** the desktop app, the engine's messages and the `s0` CLI are in English, Čeština,
Deutsch, Español, Français, Italiano, Magyar, Nederlands, Polski, Português (Brasil), Türkçe,
Русский, Українська, 日本語, 한국어, 简体中文 and 繁體中文. The app follows the system language
(picker in the status bar); `s0` takes `--lang`, else `SECTORZERO_LANG`, `LANG` or the system
language. Catalogs live in `app/ui/locales/` (checked by `node app/i18n-check.mjs`) and
`crates/sectorzero-i18n/locales/` (checked by `cargo test -p sectorzero-cli`).

### Verified against other software

- **ext2/ext3/ext4:** Linux (Alpine, in QEMU) finds SectorZero's images clean with
  `e2fsck -fn`, mounts them and reads every file. SectorZero reads and writes volumes made by
  mke2fs and filled by the kernel, and `e2fsck` still finds them clean.
- **exFAT:** Linux's driver mounts SectorZero's floppies, USB sticks (MBR) and GPT disks, reads
  every file, label, time and attribute, and writes to them; SectorZero then reads the kernel's
  changes, and the round trip repeats.
- **HFS and HFS+:** the same round trip with Linux's hfs and hfsplus drivers, on floppies and
  Zip 100 cartridges with an Apple partition map. Real Macs have not been tried yet.
- **IMZ, VHD, VMDK:** the original IMZ tool's SDK reads images made here (IMA, IMZ, VHD fixed and
  dynamic, VMDK sparse and flat, MBR and GPT disks, defragmented and resized images) with
  byte-identical files, and SectorZero reads the SDK's VHD and VMDK output. That tool opens
  SectorZero's password-encrypted IMZ images, and SectorZero opens the ones it encrypted.
- **ISO:** Windows mounts SectorZero's ISO images.
- **Compressed images:** gzip, xz, bzip2, unzip, libarchive (Windows `tar`) and Node's zstd
  read SectorZero's files, and SectorZero reads theirs (multi-member gzip, multi-block xz).
  QCOW2 images with zstd clusters made by `qemu-img` read byte for byte and pass `qemu-img check`
  after writing.
- **Physical drives:** checked on real block devices under Linux (floppy drive, USB stick, disk)
  and on Windows with VHDs attached as disks (write, verify, read back, drive-to-drive copy, wipe).
- **Snapshots:** `qemu-img` and `qemu-io` read VMDK snapshots made here, and SectorZero reads theirs
  (two-level chains, partial grains, stream-optimized VMDK). Differencing VHDX and VDI and
  Windows Backup folders are checked by this project's own tests only so far, as is VHDX.
- **Apple disk images:** checked against `hdiutil` on macOS 26 in CI (`.github/oracle/hdiutil.sh`):
  UDRO, UDCO, UDZO, UDBZ, ULFO and ULMO images, split `.dmgpart` sets, sparse images (also one
  over 4 GiB with chained headers) and sparse bundles read byte for byte like hdiutil's raw
  conversion, on GPT, APM, MBR and bare disks, and the HFS+, FAT32 and exFAT files inside match.
  Encrypted images are refused. APFS volumes open as disks only.

**Not yet** (see RESEARCH.md §5): low-level floppy formatting, writing to NTFS, writing to ISO
images beyond in-place replacement, resizing non-FAT partitions.

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

The release workflow refuses a tag that does not match those two versions.

The binaries are not code-signed yet, so Windows SmartScreen warns about an unknown publisher.

Dependabot (`.github/dependabot.yml`) opens weekly pull requests for Cargo, npm and GitHub
Actions updates; minor and patch updates come grouped, and CI runs on each.

## CLI

```powershell
s0 formats                                   # list floppy formats
s0 new boot.ima -f 1440 -l MYDISK            # new empty 1.44 MB floppy
s0 new boot.imz -f 1440                      # same, IMZ-compressed
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
s0 diff base.vhdx snap.avhdx                 # also VHDX over VHDX and VMDK snapshots over VMDK
s0 ls E:\WindowsImageBackup -r              # a Windows Backup system image; --member picks the disk
s0 convert disk.img.xz disk.vhdx             # compressed images (.gz .xz .zst .bz2 .zip .7z) open directly
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
