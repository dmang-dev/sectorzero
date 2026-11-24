//! Collect the translation keys from the Rust sources.
//!
//! Engine messages are not marked: in the engine and the desktop app's backend every string
//! literal that reads like text (letters and a space, some lowercase) is a key, except
//! outside the code proper (comments, tests, attributes other than `#[error(…)]`) and in
//! `expect(…)`, `panic!(…)` and `unreachable!(…)`, which are not for users. In the command
//! line only the first argument of each `t!(…)` counts; its help text comes from clap.

use std::path::{Path, PathBuf};

/// How literals in a file become keys.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Every literal that reads like a message.
    Messages,
    /// Only `t!("…")`.
    Macro,
}

/// The keys of the SectorZero workspace at `root`, with where each first appears, in source
/// order and without repeats.
pub fn workspace_keys(root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    for (dir, mode) in [
        ("crates/sectorzero-core/src", Mode::Messages),
        ("app/src-tauri/src", Mode::Messages),
        ("crates/sectorzero-cli/src", Mode::Macro),
    ] {
        let mut found = Vec::new();
        rust_files(&root.join(dir), &mut found);
        found.sort();
        files.extend(found.into_iter().map(|f| (f, mode)));
    }
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (file, mode) in files {
        let Ok(src) = std::fs::read_to_string(&file) else {
            continue;
        };
        let rel = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .display()
            .to_string();
        for (line, key) in keys(&src, mode) {
            if seen.insert(key.clone()) {
                out.push((key, format!("{}:{line}", rel.replace('\\', "/"))));
            }
        }
    }
    out
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The keys in one source file, with their line numbers.
pub fn keys(src: &str, mode: Mode) -> Vec<(usize, String)> {
    let src = match test_module(src) {
        Some(at) => &src[..at],
        None => src,
    };
    let mut out = Vec::new();
    for (at, text) in literals(src) {
        let before = src[..at].trim_end();
        let line_start = src[..at].rfind('\n').map_or(0, |i| i + 1);
        let line = src[..at].matches('\n').count() + 1;
        let wanted = match mode {
            Mode::Macro => {
                before.ends_with("t!(")
                    && !before[..before.len() - 3]
                        .ends_with(|c: char| c.is_alphanumeric() || c == '_')
            }
            Mode::Messages => {
                let this_line = src[line_start..at].trim_start();
                reads_like_text(&text)
                    && !["expect(", "panic!(", "unreachable!(", "todo!("]
                        .iter()
                        .any(|p| before.ends_with(p))
                    && !(this_line.starts_with("#[") && !this_line.starts_with("#[error("))
            }
        };
        if wanted {
            out.push((line, crate::normalize(&text)));
        }
    }
    out
}

fn reads_like_text(s: &str) -> bool {
    let b = s.as_bytes();
    // Multi-line literals are file contents (descriptors, cue sheets, HTML), not messages.
    s.trim().contains(' ')
        && !s.contains(['\n', '\r', '\0', '<'])
        && !s.starts_with('#')
        && b.windows(2)
            .any(|w| w[0].is_ascii_alphabetic() && w[1].is_ascii_alphabetic())
        && s.chars().any(|c| c.is_lowercase())
}

/// Where `#[cfg(test)] mod tests` starts, if the file has one (it is always last).
fn test_module(src: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = src[from..].find("#[cfg(test)]") {
        let at = from + i;
        let rest = src[at + "#[cfg(test)]".len()..].trim_start();
        let rest = rest.strip_prefix("pub(crate) ").unwrap_or(rest);
        if rest.starts_with("mod tests") {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// The string literals in Rust source (not byte strings, not in comments), with the byte
/// offset of each and its value after escapes.
fn literals(src: &str) -> Vec<(usize, String)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let ident = |j: usize| j > 0 && (b[j - 1].is_ascii_alphanumeric() || b[j - 1] == b'_');
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                i = src[i..].find('\n').map_or(b.len(), |n| i + n);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = src[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2);
            }
            b'r' if !ident(i) && matches!(b.get(i + 1), Some(b'"' | b'#')) => {
                let hashes = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
                if b.get(i + 1 + hashes) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let start = i + 2 + hashes;
                let close = format!("\"{}", "#".repeat(hashes));
                let end = src[start..].find(&close).map_or(b.len(), |n| start + n);
                out.push((i, src[start..end].to_string()));
                i = end + close.len();
            }
            b'b' if !ident(i) && matches!(b.get(i + 1), Some(b'"' | b'\'')) => {
                // Byte string or byte character: skip it.
                i = skip_quoted(b, i + 1);
            }
            b'\'' => {
                // A character literal, or a lifetime.
                if b.get(i + 1) == Some(&b'\\') {
                    i = skip_quoted(b, i);
                } else {
                    let ch = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                    i += if b.get(i + 1 + ch) == Some(&b'\'') {
                        ch + 2
                    } else {
                        1
                    };
                }
            }
            b'"' => {
                let end = skip_quoted(b, i);
                out.push((i, unescape(&src[i + 1..end - 1])));
                i = end;
            }
            _ => i += 1,
        }
    }
    out
}

/// Past the closing quote of the quoted literal that opens at `open`.
fn skip_quoted(b: &[u8], open: usize) -> usize {
    let quote = b[open];
    let mut j = open + 1;
    while j < b.len() && b[j] != quote {
        j += if b[j] == b'\\' { 2 } else { 1 };
    }
    j + 1
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                if let Ok(v) = u8::from_str_radix(&hex, 16) {
                    out.push(v as char);
                }
            }
            Some('u') => {
                let code: String = chars
                    .by_ref()
                    .skip_while(|&c| c == '{')
                    .take_while(|&c| c != '}')
                    .collect();
                if let Some(ch) = u32::from_str_radix(&code, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some('\n') => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            Some('\r') => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_messages_and_skips_the_rest() {
        let src = r#####"
// "a comment with words"
#[derive(Debug)]
enum E {
    #[error("not found in image: {0}")]
    NotFound(String),
}
fn f(x: u32) -> Result<()> {
    let c = 'a'; let q = '"'; let life: &'static str = "two words";
    Err(Error::Invalid(format!("bad value {x:?} at {}", 3)))?;
    let r = r#"raw "quoted" text"#;
    let b = b"byte string here";
    let s = "line one \
             continues";
    let v = something.expect("this never fails");
    t!("cli text {}", 1);
    Ok(())
}
#[cfg(test)]
mod tests { fn g() { let _ = "test only text"; } }
"#####;
        let got: Vec<String> = keys(src, Mode::Messages).into_iter().map(|k| k.1).collect();
        assert_eq!(
            got,
            [
                "not found in image: {0}",
                "two words",
                "bad value {0} at {1}",
                "raw \"quoted\" text",
                "line one continues",
                "cli text {0}",
            ]
        );
        let got: Vec<String> = keys(src, Mode::Macro).into_iter().map(|k| k.1).collect();
        assert_eq!(got, ["cli text {0}"]);
    }
}
