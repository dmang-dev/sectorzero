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
- Password encryption of IMZ and WLZ: MD5(password) produces a 128-bit key for Rijndael (AES-128). The layout is undocumented; SectorZero's reading of it (`container/wicrypt.rs`), worked out from files WinImage saved with known passwords and contents, and checked by WinImage opening SectorZero's files:
  - The member is an ordinary ZIP entry with general-purpose flag bit 0 set, method 8, version 1.0, external attributes 0x20, and two extra fields: `WI` (u32 1, u32 image size) and `SI` (8 bytes, `80 4D 49 52 10 80 00 00` in every file seen). The ZIP CRC is that of the decompressed image; the compressed size is that of the stored bytes.
  - Key: MD5 of the password in UTF-16LE.
  - Stored bytes: the raw deflate stream of the image, cut into 32 KiB chunks. Each chunk is zero-padded to a multiple of 16 bytes, encrypted with AES-128-CBC and a zero IV (per chunk), and stored as a u32 length and the ciphertext. A u32 with the deflate bytes in the last chunk ends the data.
  - When there is more than one chunk, 8 bytes come first: the CRC-32 of the first chunk's plaintext, then the CRC-32 of its ciphertext. WinImage rejects a file whose prefix does not match ("is not image file").
  - A wrong password shows as a deflate error or a CRC mismatch.

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
| IMZ / WLZ (encrypted) | WinImage; SectorZero |
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
5. **Compatibility targets:** IMA/IMG, IMZ and WLZ (easy, since they are just ZIP), WIL, VFD/FLP, VHD and VMDK. PFR is proprietary and would need reverse engineering, so it is low priority. (Encrypted IMZ/WLZ are now supported; see §2.7.)

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
(floppy drive, USB stick, virtio disk).

On Windows (2026-11-24), checked as administrator on two 64 MiB VHDs attached with
`Mount-DiskImage`, which appear as `\\.\PhysicalDriveN` (BusType "File Backed Virtual"). All of
these passed:
- listing;
- writing an image with verify;
- listing, injecting and `info` straight on the drive;
- reading the drive into a VHDX;
- a drive-to-drive copy with verify;
- a wipe (random pass, then zeros), with the drive's checksum equal to that of zeros;
- Windows reading the files back afterwards.

That run found that a mounted source drive changes under a drive-to-drive copy, since Windows
itself writes to it (System Volume Information). The source's volumes are now locked
(FSCTL_LOCK_VOLUME, without dismounting) for the copy and its verify, and, where they can be,
while a drive is read into an image. `s0 checksum` also hashes a disk without a file system (a
wiped drive) instead of refusing it.

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
bootable hard disk image, and the macOS and Android builds.

Password-encrypted IMZ/WLZ (2026-11-24): read, write, and set/change/remove the password, in
the engine, `s0` (`--password`, `--encrypt`, `s0 password`) and the desktop app (password
prompt on open, password in New and Save As, Tools > Password). The format notes are in §2.7.
Checked against WinImage: SectorZero reads WinImage's encrypted 160 KB, 360 KB and 1.44 MB
images (one and several chunks) byte for byte, and WinImage opens SectorZero's (one and
several chunks).

Translations (2026-11-24): engine messages and the CLI are translated into the desktop app's 16
languages (`crates/sectorzero-i18n`). Keys are the English format strings; engine errors are
matched against them at display time (a message that fits a key in more than one way stays
English), and the CLI formats its own text through `t!`.

Compressed images (2026-11-24, `container/compressed.rs`): `.gz`, `.xz`, `.zst`, `.bz2`, and
`.zip` / `.7z` archives holding one image. They are recognised by signature (a `.zip` is opened
this way unless it holds encrypted members or a `.wil` list, which stay with the IMZ/WLZ code).
The image is unpacked into a temporary folder (`SECTORZERO_TMPDIR`, else the system's) and
opened as whatever it is, so `disk.vhd.gz` is a VHD. Saving recompresses to a temporary file
next to the original and swaps it in. Other `.zip` members are copied unchanged, and so is the
archive comment. A `.7z` that holds other files too opens read-only. New files take the kind of
the inner name (`s0 new disk.vhd.xz`), and xz is written multi-threaded in 16 MiB blocks.
Members that are documentation or checksums (`.txt`, `.md`, `.sha256`, …) are skipped when
choosing the image; `--member` picks another. Random access would need an index (BGZF,
seekable zstd, xz blocks); unpacking keeps it simple, at the cost of temporary disk space.
Checked: gzip, xz, bzip2, unzip, libarchive (Windows `tar`, for .7z) and Node's zstd read
SectorZero's files, and SectorZero reads theirs (multi-member gzip, multi-block xz). QCOW2
images with zstd clusters (compression type 1) now open too; `qemu-img`'s read byte for byte,
and still pass `qemu-img check` after SectorZero writes to them.

Snapshot chains (2026-11-24). Differencing VHDX (`.avhdx`, Hyper-V checkpoints), VMDK snapshots
and differencing VDI open, write and, for VHDX and VMDK, are created by `s0 diff`. All three
follow the parent chain through `open_parent` (at most 32 levels, so a loop is an error, not a
hang) and copy a block or grain up into the child before its first write. Linkage checks keep a
child from being read over a parent that changed after it was made: VHDX compares the
parent_linkage of the parent locator with the parent's DataWriteGuid, VMDK compares parentCID
with the parent's CID, VDI compares the parent UUID with the parent's creation UUID. A VHDX
block that is only partly present is read through its sector bitmap; VMDK grain table entry 1
reads as zeros (not from the parent). The parent is looked for by the stored relative path,
then the absolute one; a VDI parent by UUID next to the child, in the folder above (VirtualBox
keeps snapshots in `Snapshots/`), and in `.vbox` HardDisk locations. Stream-optimized VMDK
(grains as zlib-compressed markers, footer near the end) opens read-only, and an `.ova` is a
tar archive whose disk opens through the compressed-image code. Sources: Microsoft's VHDX
specification ([MS-VHDX]), VMware's "Virtual Disk Format 1.1" and the published description
of the VDI 1.1 header; no VirtualBox or QEMU code was read. Checked: `qemu-img` and `qemu-io` read
and write VMDK snapshots in both directions, also two levels deep. There is no local Hyper-V
or VirtualBox to check differencing VHDX and VDI against yet.

Windows Backup system images (2026-11-24, `container/winbackup.rs`). `wbadmin` and the Windows 7
"Backup and Restore" tool write a `WindowsImageBackup\<PC>\Backup <date>` folder holding one
VHDX (older versions VHD) per backed-up volume, named by GUID, plus XML catalogs. A folder that
looks like this opens as its disks: the drive or folder holding `WindowsImageBackup`, that folder,
the computer's folder or one backup folder. The default is the largest disk (by virtual size) of
the newest backup; the rest are members (`PC/Backup …/{GUID}.vhdx`), picked by number, full
label or file name. Backups open read-only, because the catalogs record them.

Macrium Reflect X images (2026-11-24, `container/mrimgx.rs`). Macrium publishes the `.mrimgx`
layout and an MIT-licensed reference reader (github.com/macrium/mrimgx_file_layout); older
`.mrimg` files (Reflect 8 and before) are a different, unpublished format. A 20-byte footer
(u64 offset, "MACRIUM_FILE") leads to 32-byte metadata block headers (name, u32 length, MD5
of the stored bytes, flags: last of its group, zstd, AES-ECB). `$JSON` describes the disks
and partitions; at `index_file_position` each disk has a group ending in `$TRACK0` (the disk
up to its first partition) and maybe `$EPT` (extended partition boot records, 528-byte
records because the reference struct is not packed), then each partition a group ending in
`$INDEX`: an i32 count of 4 MiB FAT reserved-sector pieces and their records, then an i32
count and packed 30-byte records (i64 position, MD5 of the decoded block, u32 stored length,
u16 file number), or 34-byte delta records with the block number they replace. Block `i`
belongs at `start + (lcn0_offset - fs.start) + i * block_size`; records of length 0 are
blocks that were not imaged. The reference restore adds `start` as bytes although the schema
calls it a sector, so the reader takes bytes unless the partition table in `$TRACK0` says the
value is an LBA. The image opens as the whole disk (`disk_size`), with the unimaged space as
zeros. Incrementals are resolved like the reference: files in the same folder with the same
image ID and an increment number up to the opened one, newest first; a delta index is laid
over the newest full index. Encryption: PBKDF2-HMAC-SHA256 over the password, salt = SHA-256
of the 8-byte image ID, `key_iterations` rounds; the stored `hmac` is HMAC-SHA256 of an empty
message under that key; data blocks are AES-CBC with IV = AES-256-ECB(SHA-256(key), image ID,
disk number, partition number, block number), with the zstd frame padded to 16 bytes.
Checked only against images built in the tests from this description so far; Macrium's
sample image (`demo/demo.zip` in that repository) has not been tried yet.

Clonezilla and Rescuezilla (2026-11-24, `container/clonezilla.rs`, `container/partclone.rs`).
An image is a folder: `parts` and (whole-disk backups) `disk` list the devices; per disk
`sda-pt.sf` (sfdisk dump: partition starts and sizes, sector size, GPT `last-lba`),
`sda-pt.parted`, `sda-mbr`, `sda-hidden-data-after-mbr`, `sda-gpt-1st` / `sda-gpt-2nd`; per
partition one image split into `.aa`, `.ab`, … pieces, named by the tool and compressor
(`sda1.ext4-ptcl-img.gz.aa`, `sda1.dd-img.zst.aa`, `sda1.ntfs-img.*` from ntfsclone,
`sda1.aa` from partimage), and `sda4-ebr` for an extended partition's boot record. The disk
is put back together like a restore: MBR, the gap after it and the first GPT copy at the
start, the backup GPT at the end, each partition at its sfdisk offset. The disk size comes
from the GPT header's backup LBA, else `pt.parted`, else `last-lba`. Compression is found
by signature (the name says it too); pieces are joined and a compressed partition is unpacked
to a temporary file on its first read. Partclone format, from partclone's IMAGE_FORMATS.md:
0001 has a 4160-byte header, a byte per block, `BiTmAgIc`, and a CRC32 after each block;
0002 a 110-byte header (0xC0DE endianness mark, checksum mode/size, blocks per checksum), a
bit per block plus a CRC32, then the used blocks with a checksum after every N. The bit order
of the 0002 bitmap is not documented; least significant bit first is assumed. Checked only
against images built in the tests from those descriptions; no real Clonezilla backup or
partclone binary has been tried (the test guest's ISO has neither). ntfsclone and partimage
have no published description; they wait for a black-box oracle.

Apple disk images (2026-11-24, `container/dmg.rs`), read-only, from the Keramics format
notes (Apache-2.0; documentation only). UDIF: a 512-byte big-endian `koly` trailer at the end
gives the data fork, the XML property list (or, in old images, a resource fork) and the
segment number and count. Each `blkx` resource is a `mish` block table: a start sector, a base
offset in the data fork, and 40-byte runs (type, sector, count, offset, length). Types: 0 and 2
read as zeros (2 is free space), 1 is stored, 0x80000004–8 are ADC, zlib, bzip2, LZFSE and xz,
0x7ffffffe a comment and 0xffffffff the end. Segments continue the data fork in
`name.002.dmgpart`, … (checked by segment number and set ID), and a block may cross into the
next segment. The disk is the runs in sector order, gaps read as zeros, and a compressed run
is decoded whole (cached). zlib, bzip2 and xz blocks are read to their end so their own checks
run; LZFSE, ADC and stored blocks have none, and the CRC-32 checksums in the trailer and the
block tables are not checked yet. Sparse image: `sprs` header, sectors per band at 8, size
in sectors at 16, and from offset 64 a table where entry *i* holds the media band (plus one)
stored at 4096 + *i* × band size. When all 1008 entries are used, hdiutil writes another
4096-byte header right after the last band and links it from offset 20 of the first (a 64-bit
big-endian offset, 0 for none). Those later headers differ: `sprs`, 0 at 4, 1 at 8, the link to
the next one at 12, and a table of 1010 entries from offset 56, their bands stored after them.
Seen on macOS 26 (a 4.5 GB image: second header at 4096 + 1008 × 1 MiB, third 4096 + 1010 MiB
after that). Sparse bundle: `Info.plist` (`band-size`,
`size`, bundle type `com.apple.diskimage.sparsebundle`) and `bands/<hex>` files, missing
or short bands read as zeros; a non-empty `token` means encrypted. Encrypted images
(`encrcdsa`) are refused. Cross-checked in CI against `hdiutil` on macOS 26
(`.github/oracle/hdiutil.sh`, workflow `oracle-dmg.yml`): every UDIF format, 1 MiB
`hdiutil segment` sets, UDSP/UDSB, GPT/APM/MBR/bare layouts with HFS+, FAT32, exFAT and APFS,
and encrypted UDIF, sparse image and sparse bundle (refused), all byte-identical to hdiutil's
UDTO conversion. Still unchecked: the CRC-32 checksums, and images whose blocks only a
resource fork describes (current hdiutil always writes the XML plist).
