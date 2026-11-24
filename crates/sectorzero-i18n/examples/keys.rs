//! Print translation keys for translators, as a JSON object of key → where it is used.
//!
//!   cargo run -p sectorzero-i18n --example keys              every source key
//!   cargo run -p sectorzero-i18n --example keys -- de        the keys de.json lacks
//!
//! The command line's help text is listed by `cargo test -p sectorzero-cli` when missing.
use std::path::Path;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lang = std::env::args().nth(1);
    let have = lang
        .as_deref()
        .map(sectorzero_i18n::catalog_keys)
        .unwrap_or_default();
    let mut out = serde_json::Map::new();
    for (key, at) in sectorzero_i18n::extract::workspace_keys(&root) {
        if !have.contains(&key) {
            out.insert(key, at.into());
        }
    }
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
