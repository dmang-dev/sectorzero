# Third-party notices

SectorZero is MIT-licensed. This file credits third-party code that is reused, ported,
or shipped with the program.

## Ported or adapted code

None yet. When code is ported from another project (for example DiscUtils or TotalImage,
both MIT), add an entry here with the project, its URL, its license and copyright line,
and the files affected. Also keep the original copyright notice in those files.

## Direct dependencies

| Crate | Used for | License |
|---|---|---|
| [fatfs](https://github.com/rafalh/rust-fatfs) 0.3 | FAT12/16/32 engine | MIT |
| [chrono](https://github.com/chronotope/chrono) | Timestamp conversion | MIT OR Apache-2.0 |
| [thiserror](https://github.com/dtolnay/thiserror) | Error types | MIT OR Apache-2.0 |
| [serde](https://github.com/serde-rs/serde) / serde_json | Data passed to the CLI's JSON output and the UI | MIT OR Apache-2.0 |
| [clap](https://github.com/clap-rs/clap) | CLI argument parsing | MIT OR Apache-2.0 |
| [anyhow](https://github.com/dtolnay/anyhow) | CLI error reporting | MIT OR Apache-2.0 |
| [tauri](https://github.com/tauri-apps/tauri) 2, tauri-plugin-dialog | Desktop app shell, native dialogs | MIT OR Apache-2.0 |

## Transitive dependencies

As of 2026-11-22, the full dependency tree has no GPL, LGPL or AGPL crates. Most crates are
MIT and/or Apache-2.0. The others:

- **MPL-2.0** (cssparser, selectors, dtoa-short, option-ext). These are pulled in by Tauri.
  MPL-2.0 is file-level copyleft. It allows use in an MIT application, as long as the
  MPL files themselves are not modified.
- **Unicode-3.0, Zlib, BSD-3-Clause, ISC, Unlicense, CC0**. All are permissive.

Run `cargo metadata` (or `cargo deny check licenses` once cargo-deny is set up) to re-check
after changing dependencies.
