# WinImage research (for the SectorZero project)

Researched 2026-11-22. Sources:

- The local install at `C:\Program Files\WinImage`: WinImage 11.00 x64, `winimage.exe` dated 2023-01-07.
- Its help file, `winimage.chm`. It was last updated for v9.00, so the v10 and v11 changes come from elsewhere.
- The menu, dialog and file-filter strings and the C++ class names in `winimage.exe`.
- winimage.com, and GitHub metadata for the projects listed below.

WinImage here is the **disk image utility by Gilles Vollant** (1993–2023). It is not the bitmap editor of the same name. It is closed-source shareware: Standard costs $30, Professional $60, and a Developer Edition adds an SDK DLL. It runs on Windows only, with x86, x64 and ARM64 builds.

---

## 1. Formats WinImage supports

### Image containers

| Format | Ext. | Read | Write/Create | Notes |
|---|---|---|---|---|
| Raw sector image | .IMA .IMG .DSK | ✔ | ✔ | Native format. Uses a plain BPB. |
| Virtual floppy | .VFD .FLP | ✔ | ✔ | Raw. Used by Virtual PC and PowerQuest. |
| WinImage compressed image | .IMZ | ✔ | ✔ | A ZIP containing one .IMA (zlib). Can be encrypted. |
| WinImage list | .WIL | ✔ | ✔ | Text list of image files plus a comment. Used for multi-disk sets. |
| WinImage compressed list | .WLZ | ✔ | ✔ | A ZIP holding a .WIL and several .IMA files. Can be encrypted. |
| Batch config | .BWZ | ✔ | ✔ | A text file holding saved Batch Assistant settings. |
| Self-extractor | .EXE | ✔ | ✔ | A Win32 stub with embedded IMZ/WLZ (see §2.8). |
| ISO-9660 CD/DVD | .ISO | ✔ | ✔ created from a CD, not from files | Edit is limited to replacing a file with one of the same name and size. |
| Easy CD Creator image | .CIF | ✔ | ✗ | |
| BIN / NRG / MDF | | ? | ? | Listed in an ISO-related dialog filter. Role unconfirmed. |
| Microsoft VHD | .VHD | ✔ | ✔ fixed and dynamic | Read/write for FAT. NTFS and ext are read-only. |
| Virtual PC undo disk | .VUD | ✔ | ? | Listed alongside VHD. |
| VMware VMDK | .VMDK, -flat.vmdk | ✔ | ✔ dynamic (`VMDKWRITEDYNAMIC`) | |
| Paraben Forensic Replicator | .PFR | ✔ | ✔ | Proprietary forensic format. Its physical-drive features are licensed from Paraben. |
| Boot sector file | .BIN | ✔ | ✔ | 512-byte boot sector import and export. |
| GPT/GUID partitioned images | | ✔ | | Added in v11. |

**Not supported** (gaps an open version could fill): VHDX, VDI, QCOW2, DMG, exFAT, ext4, HFS+ file browsing, UDF, Joliet (not mentioned anywhere), and writing to NTFS or ext.

### File systems

| FS | Read | Write |
|---|---|---|
| FAT12 / FAT16 / FAT32 (+ LFN) | ✔ | ✔ |
| DMF 1024/2048, 1.68 MB, 1.72 MB, XDF-like oversize floppies | ✔ | ✔ also formats them on physical floppy drives |
| NTFS | ✔ | ✗ |
| ext2 / ext3 | ✔ | ✗ |
| ISO-9660 | ✔ | Replaces a file only if the new one has the same size |
| HFS (Mac floppies) | Sector copy only (`HFSDISK`); files cannot be viewed | — |

---

## 2. Feature list

Features marked **[Pro]** need the Professional edition.

### 2.1 Image creation and opening
- New empty image from a preset floppy format (160K to 2.88M, DMF, and others).
- New image from a custom geometry **[Pro]**: bytes/sector, sectors/cluster, total sectors, sectors/FAT, sectors/track, heads, reserved and hidden sectors, media descriptor, drive number.
- Import geometry from an existing image, a .BIN boot sector, or a selected physical partition.
- Open modes: read-only, create backup, save-as, or no-backup (edit in place).
- Small images load into RAM (the threshold is configurable). Large images are edited directly on disk **[Pro]**.

### 2.2 File operations inside an image
- **Inject** files and folders, including by drag-and-drop from Explorer.
- **Extract** selected files or everything, with or without paths. Also by drag-out to Explorer, or by double-click to open with the associated app.
- Delete files, including recursive delete of folders.
- Create folders. Move files between folders by dragging them onto the tree view.
- File properties: rename, attributes, and dates **[Pro]**.
- Change the volume label.
- Sort by name, type, size or date. Explorer-style views and a folder tree pane.

### 2.3 Image-level operations
- **Defragment** the current image. Works on in-memory images and on large on-disk images.
- **Change format / resize**: to another floppy size, a custom geometry, or one imported from another image or disk. The image must still be larger than its contents.
- Truncate unused space when saving.
- Convert a boot floppy image into a bootable USB-stick or hard-disk image.
- **Export with MBR**: wrap a bare partition image in an MBR (for CD burning tools).
- Image information dialog, with an editable comment.
- Export the directory listing to TXT or HTML, or print it **[Pro]**.

### 2.4 Boot sector and MBR
- Boot sector properties **[Pro]**: OEM string, serial number, custom boot-message text. Can load a stock boot sector (WinImage, MS-DOS 6.22, Win95/98, or NT/2000/XP) or one from a .BIN file. Works on FAT32 too.
- Bootable ISO: view the El Torito boot properties and export the boot image.
- Master Boot Record editor: reset, back up and restore the MBR of removable drives, and of fixed disks **[Pro]**.

### 2.5 Physical floppy drives
- Read disk to image, write image to disk, format then write, and compare disk against image.
- Format blank floppies, including DMF and oversize formats.
- Options: ignore empty tracks on read or write, verify after write, warn when the target is not empty, adjust the DMF gap.

### 2.6 Physical drives (hard disk, USB, removable): imaging and forensics
- **Create physical drive image** as raw, PFR (optionally compressed and encrypted), or fixed or dynamic VHD.
- **Restore** an image onto a physical drive, then verify by checksum.
- **Duplicate** one physical drive to another.
- **Convert** between physical and virtual image formats (PFR ↔ VHD ↔ raw).
- **Checksum** a drive or an image file with Adler-32, CRC32, MD5 and SHA-1 together. Compares the result with the checksum stored in the image.
- **Erase/wipe** a drive by filling it with a byte, either as a standard wipe or a DoD 5220.22-M multi-pass wipe. Verifies the drive is blank afterwards. Option to wipe only the end of the disk.
- **Erase and reset** a drive as a bootable FAT16/32 partition, optionally populated with the current image's contents. This rebuilds the partition table.
- **Connect** to a live FAT32 or ext2/3 partition to browse it (a legacy NT-era feature).
- Create an ISO image from a physical CD or DVD.

### 2.7 Compression and encryption
- ZIP-compatible compression (zlib) for .IMZ and .WLZ, with a selectable level.
- Password encryption of IMZ and WLZ: MD5(password) produces a 128-bit key for Rijndael (AES-128). The container layout is undocumented and would need reverse engineering for compatibility.

### 2.8 Self-extracting executables
- Package one or more images into a Win32 .EXE, optionally encrypted. A **[Pro]** license includes unlimited redistribution.
- The SFX command line can write to a floppy (`/F` format, `/N:xx` sets) or extract files (`/X dir`, `/NI`). Other options: `/W:file` writes an image file, `/H` hides the UI, `/R` resizes to fit a USB stick. There are also flags for verify, ignore empty tracks, and overwrite prompts. Default options can be embedded in the EXE.

### 2.9 Batch and automation
- **Batch Assistant**, as a single dialog or a step-by-step wizard.
  - Sources: a set of floppies, a set of image files (.WIL or .WLZ), or a directory injected into images. A directory can be split across several images, and numbered folders auto-increment.
  - Destinations: floppies (optionally formatted), image files (auto-numbered, optionally with a .WIL or .WLZ), or extraction to folders.
  - Settings can be saved and loaded as a .BWZ file.
- Command line:
  - `winimage src [dst] [opts]`
  - `/E` and `/X` extract; `/I` injects; `/Nfmt` creates a new image; `/Tfmt` transforms the image size; `/F` formats; `/Q` and `/NX` set exit behavior; `/H` hides the window; `/MAKESFX`.
  - `/PreferenceFile x.xml` and `/ReadOnlyPreference` support portable use.
- Auto-incrementing file names in Open and Save. Notification sounds. A taskbar icon that blinks when input is needed.

### 2.10 Mounting
- Mounts uncompressed FAT images, ISOs and fixed VHDs as drives through the bundled **FileDisk** driver by Bo Brantén. That driver dates from 2013 and may not load on current Windows with driver-signature enforcement. I did not test it.

### 2.11 Configuration
- Settings live in the registry, or in `WinImagePreference.xml` next to the exe for portable mode.
- The UI is localized through resource DLLs.

### 2.12 SDK (Developer Edition)
- A Win32 DLL with C, VB and Delphi bindings. It covers reading and writing floppies and image files, injecting, extracting, and similar operations. Its license allows distribution to 10 computers.

---

## 3. Programs that can read WinImage's formats

| Format | Compatible programs |
|---|---|
| IMA/IMG/VFD/FLP/DSK (raw) | Almost everything: 7-Zip, mtools, Linux `mount -o loop`, ImDisk, OSFMount, Virtual Floppy Drive, Hyper-V and Virtual PC (as a floppy), VirtualBox, QEMU, 86Box/PCem, DOSBox, dd, Win32 Disk Imager, TotalImage, DiskImageTool |
| IMZ / WLZ (unencrypted) | Any unzip tool (7-Zip, `unzip`), which yields plain .IMA files. TotalImage reads IMZ. DiskImageTool reads IMZ. |
| IMZ / WLZ (encrypted) | WinImage only |
| VHD | Windows built-in (`Mount-DiskImage`, Disk Management), Hyper-V, VirtualBox, qemu-img, 7-Zip, DiscUtils, FATtools, libvhdi |
| VMDK | VMware, VirtualBox, qemu-img, 7-Zip, DiscUtils, FATtools, libvmdk |
| ISO | Everything (Windows Explorer, 7-Zip, xorriso, every burner and VM) |
| CIF | Almost nothing current (IsoBuster, which is commercial) |
| PFR | Paraben tools only |
| WIL / BWZ | Plain text. No other tool uses them. |

---

## 4. Open-source landscape

There is **no open-source program that replaces WinImage end to end.** The pieces exist, but they are spread across command-line tools and libraries, and the two GUI projects each cover only a slice.

### 4.1 GUI programs (closest competitors)

| Project | License | Status (as of Nov 2026) | What it covers | What it lacks vs WinImage |
|---|---|---|---|---|
| **[TotalImage](https://github.com/TotalImage/TotalImage)** | MIT, C#/WinForms, .NET 10 | Active (pushed 2026-11-20). Alpha; the last release is v1.0.0-alpha1 from 2023. | Read: raw, VHD (except differencing), IMZ, NHD, FDI/HDI. FS read: FAT12/16/32, NTFS, exFAT, ISO, UDF, Joliet, GPT, MBR. Can create FAT12 floppies. | **Almost entirely read-only.** It cannot inject, delete, defrag or resize, has no physical drive or VMDK support, and no batch or SFX. |
| **[DiskImageTool](https://github.com/Digitoxin1/DiskImageTool)** | GPL-3.0, VB.NET | Very active (v2.46, 2026-11-03) | Read/write for **floppy images only**: inject and extract, boot sector editor, FAT editor, hex editor, many floppy formats including IMZ, format conversion, Greaseweazle and KryoFlux hardware | No hard-disk or partitioned images, VHD/VMDK, NTFS, ISO, physical drives, or defrag. Its author calls it a personal, non-production project. |
| MtoolsFM | GPL | Old | A GUI front end for mtools | Floppies or FAT only, and minimal |

### 4.2 Command-line tools and libraries, by WinImage feature

| WinImage feature | Open-source equivalent(s) |
|---|---|
| Inject/extract/delete/mkdir in FAT images | **mtools** (GPL-3; `mcopy`, `mdel`, `mmd`, `mlabel`), **FATtools** (GPL-3, Python; FAT12/16/32 and exFAT read/write inside raw/VHD/VHDX/VDI/VMDK), fat_imgen (FAT12 only), guestfish/libguestfs (Linux) |
| Browse and extract many file systems | 7-Zip (LGPL), TotalImage, fatcat (MIT), The Sleuth Kit |
| Create and format blank images | mkfs.fat (dosfstools), `mformat`, FATtools `mkfat`, DiscUtils |
| Resize / change format | fatresize (libparted). Nothing handles floppy geometry conversion well. |
| **Defragment an image** | **Clear gap.** Options: UltraDefrag on a mounted volume (only older releases were GPL), FATDefrag (SourceForge, GPLv2, dormant), FreeDOS DEFRAG run inside an emulator. FATtools only *measures* fragmentation. |
| Boot sector / MBR edit | ms-sys (GPL-2), DiskImageTool (floppies only), Rufus |
| Virtual disk conversion | qemu-img (GPL-2), VBoxManage, FATtools, DiscUtils |
| Physical drive image/restore | dd, dcfldd, dc3dd, Win32 Disk Imager (GPL-2), tenox7/diskimg (Apache-2, Windows), Rufus (GPL-3; can save a drive as VHD/VHDX), Guymager and ewfacquire (forensic) |
| Drive checksums | dc3dd and dcfldd (hash while imaging) |
| Wipe (including DoD 5220.22-M) | nwipe (GPL-2) |
| Build ISO from files | xorriso and genisoimage (GPL), DiscUtils CDBuilder |
| Mount images as drives | Windows built-in for VHD/VHDX/ISO (no driver needed), ImDisk (source available, custom license), Arsenal Image Mounter, Virtual Floppy Drive (GPL-2), FileDisk |
| Physical floppy read/write | Greaseweazle tools, DiskImageTool, `dd` on Linux |
| Batch Assistant, self-extracting EXE, encrypted IMZ | **None** |

### 4.3 Libraries that could be the engine

| Library | License | Language | Notes |
|---|---|---|---|
| **[LTRData.DiscUtils](https://github.com/LTRData/DiscUtils)** | **MIT** | C# / .NET | Very active fork (pushed 2026-11-18). Read/create/**modify**: FAT12/16/32, exFAT, NTFS, raw, VHD, **VHDX**, VMDK, VDI, MBR, GPT. Read-only: ext2/3/4, HFS+, XFS, Btrfs, DMG, WIM. Builds ISO/Joliet (bootable), UDF and SquashFS. **It covers most of WinImage's backend, plus the formats WinImage lacks.** |
| FATtools | GPL-3 | Python | Very complete FAT/exFAT engine with virtual disk support, but GPL. |
| rust-fatfs | MIT | Rust | FAT only. Would need virtual disk layers added. |
| FatFs (ChaN) | BSD-1-like | C | Embedded FAT, very portable. |
| libyal (libvhdi, libvmdk, libfsntfs, …) | LGPL-3 | C | Read-only parsers. |

---

## 5. Takeaways for SectorZero

1. **An open-source WinImage is worth building.** No open-source project combines writable FAT editing inside partitioned and virtual hard-disk images, defrag and resize, physical drive imaging with verification, and batch processing in one GUI.
2. **Don't write the file-system and container code from scratch.** DiscUtils (MIT) already does FAT, exFAT and NTFS read/write over VHD, VHDX, VMDK, VDI and raw, plus ISO building. That leaves defrag, resize and geometry change, IMZ/WLZ, batch, the physical-drive and floppy layers, and the UI as the real new work.
3. **TotalImage is the closest project** (MIT, C#, active), but it is read-only today. One choice is to contribute write support to it instead of forking the effort. Starting fresh is still reasonable if you want a WinImage-style workflow, or if you want to go cross-platform.
4. **Places to improve on WinImage:** VHDX, VDI and QCOW2; exFAT; NTFS write; cross-platform (Linux/macOS); using Windows' native VHD/VHDX/ISO mounting instead of an unsigned 2013 driver; SHA-256 checksums; scriptable CLI and library APIs.
5. **Compatibility targets:** IMA/IMG, IMZ and WLZ (easy, since they are just ZIP), WIL, VFD/FLP, VHD and VMDK. Encrypted IMZ and PFR are proprietary and would need reverse engineering, so they are low priority.

### 5.1 Implementation status (2026-11-22)

Done, in Rust, from the specifications rather than DiscUtils (so no .NET runtime): IMA/IMG/VFD/FLP,
IMZ, WLZ, VHD (fixed and dynamic), VHDX, VMDK (sparse, flat, descriptor), MBR and GPT, FAT12/16/32
read/write, ISO 9660 + Joliet + Rock Ridge (read), ISO building with El Torito (BIOS and UEFI),
volume label, boot sector edit/import/export, MBR import/export, defragment, change format (resize),
and conversion between containers. Images were cross-checked against the WinImage SDK and Windows'
ISO mounting.

Added the same day: custom geometry (and copying a layout from another image or boot sector), file
attributes and dates, checksums (CRC-32, Adler-32, MD5, SHA-1, SHA-256), file lists as text, CSV
or HTML, truncating unused space, in-place file replacement in ISOs, self-extracting images (the
`s0` executable with the image appended), and in the desktop app, opening a file with its default
program and dragging files out. New containers: VHD differencing and Virtual PC undo disks, VDI,
QCOW2, split raw, PC-98 NHD/HDI/FDI, and read-only BIN/CUE, NRG and MDS/MDF CD images. VDI and
QCOW2 were cross-checked with `qemu-img check` and `qemu-img convert`; NRG and MDS were built from
published format descriptions and have not been tried on files made by Nero or Alcohol yet.
On Linux, block devices open as raw images; this was tested in an Alpine guest under QEMU with a
virtual floppy (`/dev/fd0`) and USB stick (`/dev/sda`).

Still to do, roughly in order: physical drives on Windows (read an image from a USB stick or card,
write one back, with verification), physical floppy drives on Windows, batch mode, NTFS and
ext2/3/4, exFAT, resizing partitioned disks, converting a boot floppy into a bootable hard disk
image, encrypted IMZ (proprietary), and the macOS and Android builds.
