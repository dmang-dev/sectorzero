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

Linux and Windows file systems (2026-11-23): ext2, ext3 and ext4 read/write and create
(`ext.rs`, written from the kernel's ext4 on-disk documentation); NTFS read-only through the
`ntfs` crate. ext3/ext4 journals are handled like an offline tool handles them: writes only
when the journal is clean, and it is left clean. ext4 writing covers mke2fs's default feature
set: new files get minimal-depth extent trees, 64-bit descriptors, flex_bg, uninitialized
block/inode groups (the bitmap of a BLOCK_UNINIT group is rebuilt from the layout and checked
against its free count), and crc32c metadata checksums on the superblock, descriptors,
bitmaps, inodes, extent blocks, directory blocks and shared xattr blocks (crc16 descriptor
checksums for uninit_bg). A hashed (dir_index) folder that gets modified becomes a linear one
(with metadata_csum, its index blocks are rewritten as empty, checksummed leaves), which Linux
and e2fsck accept. Inline data, encryption, bigalloc, quotas, casefolding, meta_bg and
verity stay read-only. The Properties dialog edits ext read-only (the write permission bits)
and modified/accessed/created times. The tests run a built-in consistency check
(`Volume::ext_check`: checksums, bitmaps against inode usage, free counts, link counts) after
every change, on s0's own volumes and on a mkfs.ext4 image. NTFS writing means
MFT record allocation, $Bitmap, index B+tree inserts and splits, and leaving $LogFile clean;
ntfs-3g is GPL, so it would be written from the format documentation. Cross-checked in an
Alpine guest under QEMU (e2fsprogs from the ISO): `e2fsck -fn` passes on SectorZero's
ext2/ext3/ext4 images and on mke2fs ext2/ext3/ext4 images after SectorZero writes to them, and
the kernel sees SectorZero's file contents, renames, deletions and attribute/time changes.

Physical drives (2026-11-23, `device.rs`): `s0 drives` and the app's Drives dialog list
`\\.\PhysicalDriveN` disks (size, bus, model, removable, mounted volumes) and floppy drive
letters on Windows, `/sys/block` on Linux; listing only queries the devices, so it works
without administrator rights. A drive path opens anywhere an image does. Windows only accepts
whole, aligned sectors on raw disks, so every transfer goes through a sector-aligned buffer
that is 4 KiB-aligned in memory, and partial sectors are read, patched and written back.
Writing an image (`write_image_to_drive`, verified by reading everything back) first locks and
dismounts every volume on the disk (FSCTL_LOCK_VOLUME, FSCTL_DISMOUNT_VOLUME), and afterwards
asks Windows to re-read the partition table (IOCTL_DISK_UPDATE_PROPERTIES). The disk holding
the Windows folder (or `/` on Linux) is always refused, fixed disks need an explicit
"allow fixed" flag, and Linux refuses drives with mounted partitions. Opening a drive needs
administrator rights (root on Linux). Checked on real block devices in the Alpine guest
(floppy drive, USB stick, virtio disk); on Windows, only listing and the refusals have been
tried so far, since this machine's only disk is the system disk.

exFAT (2026-11-23, `exfat.rs`, written from Microsoft's published exFAT specification): read,
write and create. Mounting verifies the boot region checksum and the up-case table checksum;
entry sets whose checksum does not match are skipped. Names compare through the volume's
up-case table, so lookups are case-insensitive beyond ASCII. New files are stored as one
contiguous run without FAT entries (NoFatChain) when a run is free, otherwise through the FAT;
folders that outgrow their clusters gain one more (and switch to a FAT chain if the next
cluster is taken). Writing marks the volume dirty first and clears the flag (and updates
PercentInUse) when done; volumes that are already dirty, have media failures, two FATs (TexFAT)
or a revision other than 1.x open read-only. `format` uses Windows' default cluster sizes
(4 KiB to 256 MiB, 32 KiB to 32 GiB, 128 KiB above), puts the FAT at 1 MiB on volumes of 64 MiB
or more, and writes an up-case table generated from Rust's Unicode case mapping (one-to-one
mappings in the Basic Multilingual Plane, identity runs compressed). The tests run
`Volume::exfat_check` (boot checksums in both regions, entry set checksums, name hashes,
cluster ownership against the bitmap) after every change. Cross-checked in the Alpine guest:
the Alpine ISO has no exfatprogs, so the oracle is the kernel's exFAT driver (6.18), which
verifies those checksums as it reads. It mounts SectorZero's exFAT floppy (with a FAT-chained,
fragmented file), USB stick (MBR) and 300 MiB GPT disk, every file's md5 matches, s0's times,
read-only attribute and labels show up, and the kernel logs no errors; SectorZero's check
passes on the volumes after the kernel wrote to them, SectorZero reads the kernel's files and
writes more, and the kernel reads those. `fsck.exfat` and Windows' `chkdsk` have not been run
on them yet.

Drive copy, drive wiping and partitioned resize (2026-11-23). `write_image_to_drive` accepts a
drive as its source, which gives drive-to-drive copies with the same locking and read-back
verification; a source and target that are the same disk (including a Linux partition of the
target disk) are refused. `wipe_drive` zeroes the whole drive, optionally after a pass of
random data, and verifies that every byte reads back as zero; the same system-disk and
fixed-disk rules apply. `Volume::resize_disk` resizes MBR and GPT disk images in any writable
container: the last partition (by end sector) grows or shrinks with the disk and its FAT volume
is rebuilt like "Change format" does (files, dates, attributes, label, serial, boot code; the
FAT type follows the new size and the MBR type with it), or, keeping the partitions, only the
container and the table change. GPT gets both headers, both entry arrays, CRCs, the last usable
LBA and the protective MBR rewritten; shrinking below a partition's end is refused, as are
growing a logical partition and rebuilding a non-FAT file system. Checked in the Alpine guest:
the copy and wipe on its floppy and virtio disks (`cmp`, all-zero reads), and on resized MBR and
GPT disks `sfdisk --verify` is clean, the kernel reports no GPT problems, the rebuilt FAT
partitions mount with every md5 matching, and `e2fsck` is clean on an ext4 disk resized with
its partitions kept.

Removable media of 1980-2005 (2026-11-23, phase 1). Zip, Jaz, SuperDisk and magneto-optical
drives on USB, ATAPI or SCSI are ordinary removable disks to the operating system, so the drive
support above already reads and writes them; parallel-port Zip drives only have drivers on
Linux. Drives report their logical sector size, and 640 MB / 1.3 GB MO media use 2048-byte
sectors, which the drive I/O, the partition parser and FAT handle. `MEDIA_FORMATS` holds exact
capacities as the drives report them (from drive logs, manuals and `fdisk` output on mailing
lists; no GPL code was consulted): Zip 100 196,608 and Zip 250 489,532 sectors, Jaz 1 GB
2,091,050 and 2 GB 3,915,600, LS-120 246,528, MO 128 MB 248,826 and 230 MB 446,325 (512-byte
sectors), MO 640 MB 310,352 and GigaMO 1.3 GB 605,846 (2048-byte sectors). Iomega's PC format
is one FAT16 partition in MBR slot 4 at sector 32 (64 heads, 32 sectors per track, type 0x06);
SuperDisk and MO media were formatted like floppies, without a partition table (LS-120: FAT16,
2 KiB clusters, media byte 0xF0). The FAT details of Zip and Jaz cartridges (cluster size, root
entries) follow Windows' defaults for the size because no factory boot sector dump was found,
and the 230 MB MO count is from a single source. Not found reliably, so no presets yet: Zip
750, LS-240, MO 540 MB, SyQuest (44/88/105/200/270, EZ135, EZFlyer, SyJet, SparQ), Bernoulli,
Clik!/PocketZip and Castlewood Orb. `eject_drive` locks and dismounts, allows removal and
ejects (IOCTL_STORAGE_EJECT_MEDIA on Windows; SCSI door unlock and CDROMEJECT, which Linux
accepts on any SCSI disk, or FDEJECT). Checked in the Alpine guest on QEMU removable SCSI disks
(one with 2048-byte logical blocks): Linux finds the Zip partition in slot 4, mounts it and the
2048-byte MO superfloppy and reads the files, the MO drive reads back with the image's disk
checksum, and the Zip drive has no medium after `s0 eject`. Real cartridges have not been tried.
Mac-formatted cartridges (Apple Partition Map, HFS/HFS+) are phase 2.

Mac-formatted media (2026-11-24, phase 2). Written from Apple's documentation only (Inside
Macintosh: Files for HFS, Inside Macintosh: Devices for the partition map, TN1150 for HFS+); no
hfsutils, Linux or xnu code was consulted. The Apple Partition Map is read and written (Driver
Descriptor Record, the map's own entry, then the volume from block 64 as Apple's tools left
it). HFS and HFS+/HFSX are read through the catalog B-tree, forks through the extents overflow
tree, hard links through their iNode files; names are Mac Roman or UTF-16 (decomposed on disk,
shown composed). Writing loads the catalog and extents trees into memory, edits them and writes
each tree back packed, with new data in newly allocated blocks and the bitmap and both volume
headers last; blocks freed by a change are reused only when space would otherwise run out.
New files get thread records and no BSD permissions, as Mac OS 9 made them. Journaled HFS+ and
HFS+ inside an HFS wrapper stay read-only. Resource forks and Finder information go in and out
as AppleDouble (`._name`, entries 2 and 9) or MacBinary II/III (MacBinary dates are local time).
`s0`'s internal consistency check (`hfs_check`: bitmap against every extent, unique CNIDs,
threads, valences, header counts, the next CNID, orphan overflow extents) runs after every write
in the tests.

Checked in the Alpine guest (`sz-vm/hfswrite.py`, two boots, 62 checks): HFS and HFS+ on a bare
1440 KB floppy and on a Zip 100 with an Apple partition map. The kernel mounts SectorZero's
volumes, matches every md5 and name, writes on top and remounts cleanly with nothing in dmesg;
SectorZero then reads the kernel's files and writes a second round, which the kernel checks
again. What the Linux drivers do differently, found on the way (`sz-vm/hfscnid.py`): the hfs
driver stores the last CNID it gave out in drNxtCNID rather than the next free one (Apple's
definition), so a writer that trusts the header reuses an ID; SectorZero takes the larger of
the header and the highest ID in use plus one. The hfs driver does not update drNmFls when a
file moves into or out of the root; SectorZero recomputes every count when it writes. Linux
cannot open resource forks by path (`file/rsrc` and `..namedfork/rsrc` give ENOTDIR), so forks
were verified by SectorZero after the kernel's writes. hfsplus ignores the Finder locked flag
(it only maps the immutable flag), and hfs shows locked files as mode 444. blkid recognises
none of these volumes in the guest. Not tried yet: real Macs, Mac OS 9 or macOS reading the
images, and Mac-made cartridge images.

Still to do, roughly in order: low-level formatting of physical floppies, batch mode, NTFS
writing, resizing ext/exFAT/NTFS partitions, converting a boot floppy into a
bootable hard disk image, encrypted IMZ (proprietary), and the macOS and Android builds.
