# Vendored fatfs 0.3.6

Source: https://crates.io/crates/fatfs/0.3.6 (MIT, Copyright (c) 2017 Rafał Harabień — see LICENSE.txt).
Used through `[patch.crates-io]` in the workspace `Cargo.toml`.

## Change

`src/dir.rs`, `Dir::write_entry`: no long-file-name entries are written for `.` and `..`.
Upstream 0.3.6 puts LFN entries before them, so `.` is not in slot 0 of a new directory;
WinImage, DOS and chkdsk reject such directories. Found by cross-checking with the WinImage SDK.

Drop this copy when moving to a fatfs release that writes plain dot entries.
- `src/lib.rs`: `#![allow(warnings)]` (warnings from compilers newer than this release; no code change).
