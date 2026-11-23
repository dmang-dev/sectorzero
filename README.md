# SectorZero

An open-source (MIT) disk image utility: create floppy and disk images, browse them, and
inject or extract files without mounting anything. It aims to be a free alternative to WinImage.

SectorZero is an independent project. It is not affiliated with or endorsed by WinImage or
Gilles Vollant Software; WinImage is their trademark and is named here only to describe
file-format compatibility.

See [RESEARCH.md](RESEARCH.md) for WinImage's full feature set and how this project maps onto it.

## Status

Windows first. The other platforms come later.

| Area | Works now |
|---|---|
| Containers | Raw sector images: `.ima` `.img` `.vfd` `.flp` `.dsk`. Images saved short ("truncate unused space") are read as if padded with zeros. |
| Layout | Bare FAT volumes (floppies, WinImage partition images) and MBR disks (primary partitions) |
| File systems | FAT12 / FAT16 / FAT32 with long file names |
| Create | Every standard floppy format (160 KB to 2.88 MB, including DMF and 1.68/1.72 MB), and bare FAT volumes of any size |
| Edit | Inject files and folders, extract, delete, create folders, rename/move. Host file timestamps are kept. |

Next up (see RESEARCH.md §5): IMZ/WLZ, defrag and resize, boot sector editing, VHD/VHDX/VMDK, GPT, ISO, physical drives.

## Layout

```
crates/sectorzero-core   the engine (library): containers, partitions, FAT operations
crates/sectorzero-cli                 command-line tool
app/                       Tauri 2 desktop app (Rust shell in src-tauri/, plain HTML/JS UI in ui/)
```

## Building (Windows)

Requirements:

- Rust via rustup. The repo pins `stable-x86_64-pc-windows-msvc` in `rust-toolchain.toml`.
- Visual Studio Build Tools with the C++ workload.
- Node.js (for the Tauri CLI only).
- WebView2. It ships with Windows 10 and 11.

```powershell
cargo test --workspace
cargo run -p sectorzero-cli -- --help
```

Desktop app:

```powershell
cd app
npm install
npm run dev      # run with live reload of ui/
npm run build    # release build and NSIS installer
```

## CLI

```powershell
s0 formats                                   # list floppy formats
s0 new boot.ima -f 1440 -l MYDISK            # new empty 1.44 MB floppy
s0 new data.img -s 64M                       # new bare FAT16 volume
s0 inject boot.ima .\files\* --to /tools     # copy files in (folders recurse)
s0 ls boot.ima -r                            # list everything
s0 extract boot.ima /tools -o .\out          # copy out (no path = everything)
s0 rm boot.ima /tools -r
s0 mv boot.ima /a.txt /b.txt
s0 info disk.img -p 2                        # partition 2 of an MBR disk
```

Add `--json` to any command for machine-readable output.

## License policy

SectorZero is MIT-licensed. Code from MIT, BSD, Apache-2.0 or zlib-licensed projects can be
reused or ported. Keep the original notice and list the project in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

Do not copy or port code from GPL, LGPL or AGPL projects (for example mtools, FATtools,
DiskImageTool or qemu). Work from the published format specifications instead.
