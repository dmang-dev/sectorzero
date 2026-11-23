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

See [RESEARCH.md](RESEARCH.md) for the feature survey, format notes and roadmap.

## Status

Windows first. Linux builds come from CI; macOS and other platforms come later.

| Area | Works now |
|---|---|
| Containers | Raw sector images (`.ima` `.img` `.vfd` `.flp` `.dsk`, short "truncated" images read as zero-padded). WinImage IMZ (zip) and WLZ (zip holding several images). VHD fixed and dynamic. VHDX (dynamic and fixed, with log replay). VMDK monolithicSparse, monolithicFlat and descriptor + extents. All read/write. |
| Layout | Bare FAT volumes, MBR disks (primary and logical partitions), GPT disks, ISO 9660 CD/DVD images |
| File systems | FAT12 / FAT16 / FAT32 with long file names (read/write). ISO 9660 with Joliet and Rock Ridge names (read-only), El Torito boot catalog |
| Create | Every standard floppy format (160 KB to 2.88 MB, including DMF and 1.68/1.72 MB); FAT volumes and MBR or GPT hard disk images of any size, in any of the containers above |
| Edit | Inject files and folders, extract, delete, create folders, rename/move. Host file timestamps are kept. |
| Tools | Volume label; boot sector view/edit (OEM name, serial, geometry, hidden sectors, drive number), export, and boot-code import that keeps the BPB; MBR export/import; defragment (with optional free-space clearing); change format / resize, keeping files, dates, attributes, label, serial and boot code; save as another container; build ISO images (optionally bootable, BIOS or UEFI); save an ISO's boot image |

Verified against other software: the original IMZ tool's SDK reads images made here (IMA, IMZ, VHD fixed/dynamic,
VMDK sparse/flat, MBR and GPT disks, defragmented and resized images) with byte-identical files,
and SectorZero reads the SDK's VHD and VMDK output. Windows mounts the ISO images. VHDX is
checked by this project's own tests only so far.

Not yet (see RESEARCH.md §5): physical drives and floppy drives, encrypted IMZ, exFAT/NTFS, VDI/QCOW2,
writing to ISO images, resizing partitioned disks.

## Layout

```
crates/sectorzero-core   the engine (library): containers, partitions, file systems, tools
crates/sectorzero-cli    command-line tool, installed as `s0`
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

`.github/workflows/ci.yml` runs rustfmt, clippy (warnings are errors) and the tests on Windows,
Linux and macOS for every push to `main` and every pull request.

`.github/workflows/release.yml` builds the Windows installer, the Linux .deb and AppImage, and
zipped `s0` CLI binaries, with SHA-256 checksums. To release, bump `version` in `Cargo.toml` and
`app/src-tauri/tauri.conf.json`, then push a matching tag (`git tag v0.2.0 && git push origin v0.2.0`).
The files land on a draft GitHub release to review and publish. Running the workflow by hand
builds the same files as workflow artifacts without making a release.

The binaries are not code-signed yet, so Windows SmartScreen warns about an unknown publisher.

## CLI

```powershell
s0 formats                                   # list floppy formats
s0 new boot.ima -f 1440 -l MYDISK            # new empty 1.44 MB floppy
s0 new boot.imz -f 1440                      # same, IMZ-compressed
s0 new data.img -s 64M -t none               # bare FAT16 volume
s0 new disk.vhdx -s 2G -t gpt -l DATA        # dynamic VHDX with a GPT partition (--fixed for fixed size)
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
s0 convert boot.ima boot.vhdx                # container from the extension, or -c raw|imz|vhd|vhdx|vmdk
s0 iso .\cdroot cd.iso -l MYCD -b boot.ima   # bootable ISO (--efi, --no-emulation)
s0 boot-image cd.iso -o boot.ima
```

Add `--json` to any command for machine-readable output. For WLZ files, `--member` picks the image.

## License policy

SectorZero is MIT-licensed. Code from MIT, BSD, Apache-2.0 or zlib-licensed projects can be
reused or ported. Keep the original notice and list the project in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

Do not copy or port code from GPL, LGPL or AGPL projects (for example mtools, FATtools,
DiskImageTool or qemu). Work from the published format specifications instead.
