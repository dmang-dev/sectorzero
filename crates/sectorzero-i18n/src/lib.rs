//! Translations of SectorZero's engine messages and command line.
//!
//! The English text is the key, as in the desktop app's catalogs (`app/ui/locales`). A
//! catalog (`locales/<code>.json`) maps each key to its translation; anything missing stays
//! English. Keys are Rust format strings with every placeholder renumbered in order of
//! appearance: `format!("cannot open {}: {e}", path)` gives the key `cannot open {0}: {1}`,
//! and a translation may use `{0}` and `{1}` in any order. Literal braces are doubled.
//!
//! Two ways in:
//!
//! - [`t!`] formats a key with arguments, for text the command line prints.
//! - [`tr`] translates a finished message, such as an engine error: the message is matched
//!   against the keys and the text in each placeholder is translated in turn, so
//!   `unsupported: {0}` around `writing {0} file systems with {1}` comes out translated as a
//!   whole. A message that matches no key, or matches one in more than one way, stays
//!   English rather than come out garbled.
//!
//! `cargo test -p sectorzero-cli` checks that every catalog translates every key; the keys
//! come from the sources (see [`extract`]) and from the command line's help.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::{OnceLock, RwLock};

pub mod extract;

/// Supported languages: code and name in that language, English first. The same set as the
/// desktop app.
pub const LANGUAGES: [(&str, &str); 17] = [
    ("en", "English"),
    ("cs", "Čeština"),
    ("de", "Deutsch"),
    ("es", "Español"),
    ("fr", "Français"),
    ("it", "Italiano"),
    ("hu", "Magyar"),
    ("nl", "Nederlands"),
    ("pl", "Polski"),
    ("pt-BR", "Português (Brasil)"),
    ("tr", "Türkçe"),
    ("ru", "Русский"),
    ("uk", "Українська"),
    ("ja", "日本語"),
    ("ko", "한국어"),
    ("zh-CN", "简体中文"),
    ("zh-TW", "繁體中文"),
];

const CATALOGS: [(&str, &str); 16] = [
    ("cs", include_str!("../locales/cs.json")),
    ("de", include_str!("../locales/de.json")),
    ("es", include_str!("../locales/es.json")),
    ("fr", include_str!("../locales/fr.json")),
    ("it", include_str!("../locales/it.json")),
    ("hu", include_str!("../locales/hu.json")),
    ("nl", include_str!("../locales/nl.json")),
    ("pl", include_str!("../locales/pl.json")),
    ("pt-BR", include_str!("../locales/pt-BR.json")),
    ("tr", include_str!("../locales/tr.json")),
    ("ru", include_str!("../locales/ru.json")),
    ("uk", include_str!("../locales/uk.json")),
    ("ja", include_str!("../locales/ja.json")),
    ("ko", include_str!("../locales/ko.json")),
    ("zh-CN", include_str!("../locales/zh-CN.json")),
    ("zh-TW", include_str!("../locales/zh-TW.json")),
];

static CURRENT: RwLock<&str> = RwLock::new("en");

/// The language messages are translated into.
pub fn language() -> &'static str {
    *CURRENT.read().unwrap_or_else(|e| e.into_inner())
}

/// Switch to the best supported language for a tag such as `de`, `de-AT`, `de_DE.UTF-8` or
/// `zh-Hant`; English when nothing fits. Returns the code chosen.
pub fn set_language(tag: &str) -> &'static str {
    let code = matching(tag).unwrap_or("en");
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = code;
    code
}

/// The supported language for a tag, if any. POSIX locale names (`pt_BR.UTF-8`) work too.
pub fn matching(tag: &str) -> Option<&'static str> {
    let tag = tag.split(['.', '@']).next()?.trim().replace('_', "-");
    let lower = tag.to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    if lower == "c" || lower == "posix" {
        return Some("en");
    }
    if let Some((code, _)) = LANGUAGES
        .iter()
        .find(|(c, _)| c.eq_ignore_ascii_case(&lower))
    {
        return Some(code);
    }
    let base = lower.split('-').next().unwrap_or_default();
    match base {
        "zh" => Some(
            if ["hant", "tw", "hk", "mo"]
                .iter()
                .any(|t| lower.split('-').any(|p| p == *t))
            {
                "zh-TW"
            } else {
                "zh-CN"
            },
        ),
        "pt" => Some("pt-BR"),
        _ => LANGUAGES
            .iter()
            .find(|(c, _)| c.split('-').next() == Some(base))
            .map(|(c, _)| *c),
    }
}

/// The language the environment asks for: `SECTORZERO_LANG`, then the POSIX variables
/// (`LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG`), then on Windows the user's display
/// languages. English when none of them is supported.
pub fn detect() -> &'static str {
    if let Some(code) = std::env::var("SECTORZERO_LANG")
        .ok()
        .and_then(|v| matching(&v))
    {
        return code;
    }
    for var in ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(var) {
            let first = value.split(':').next().unwrap_or_default();
            if !first.trim().is_empty() {
                return matching(first).unwrap_or("en");
            }
        }
    }
    system_languages()
        .iter()
        .find_map(|l| matching(l))
        .unwrap_or("en")
}

/// Use the language from [`detect`]. Returns the code chosen.
pub fn init() -> &'static str {
    set_language(detect())
}

#[cfg(windows)]
fn system_languages() -> Vec<String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserPreferredUILanguages(
            flags: u32,
            count: *mut u32,
            buffer: *mut u16,
            size: *mut u32,
        ) -> i32;
    }
    const MUI_LANGUAGE_NAME: u32 = 0x8;
    let (mut count, mut size) = (0u32, 0u32);
    // SAFETY: a null buffer asks for the size; the second call gets a buffer of that size.
    if unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            std::ptr::null_mut(),
            &mut size,
        )
    } == 0
    {
        return Vec::new();
    }
    let mut buf = vec![0u16; size as usize];
    // SAFETY: `buf` holds `size` UTF-16 units, as the call above asked for.
    if unsafe {
        GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, buf.as_mut_ptr(), &mut size)
    } == 0
    {
        return Vec::new();
    }
    buf.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

#[cfg(not(windows))]
fn system_languages() -> Vec<String> {
    Vec::new()
}

/// Format a key with arguments in the current language: `t!("wrote {} ({} bytes)", path,
/// size)`. Only plain `{}` placeholders, filled in order; format anything that needs a width
/// or precision first. A key with no arguments is just looked up.
#[macro_export]
macro_rules! t {
    ($key:literal $(,)?) => {
        $crate::fill($key, &[])
    };
    ($key:literal, $($arg:expr),+ $(,)?) => {
        $crate::fill($key, &[$(&$arg as &dyn ::std::fmt::Display),+])
    };
}

/// What [`t!`] calls: `key` is a Rust format string with `{}` placeholders.
pub fn fill(key: &str, args: &[&dyn Display]) -> String {
    let key = normalize(key);
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    match catalog().and_then(|c| c.keys.get(&key)) {
        Some(pieces) => render(pieces, &args),
        None => render(&parse(&key), &args),
    }
}

/// Translate a finished message, such as an engine error, into the current language.
pub fn tr(message: &str) -> String {
    match catalog() {
        Some(c) => c.translate(message, 0),
        None => message.to_string(),
    }
}

/// Whether the catalog for `lang` translates `key` (a normalized key, as in the catalogs).
pub fn has(lang: &str, key: &str) -> bool {
    catalog_for(lang).is_some_and(|c| c.keys.contains_key(key))
}

/// A catalog's translation of `key` (a normalized key), as written in the file.
pub fn translation(lang: &str, key: &str) -> Option<String> {
    catalog_for(lang).and_then(|c| c.raw.get(key).cloned())
}

/// The keys a catalog has, normalized as in the file.
pub fn catalog_keys(lang: &str) -> Vec<String> {
    catalog_for(lang).map_or_else(Vec::new, |c| c.order.clone())
}

/// Check one translation against its key: the same placeholders, each used at least once,
/// and nothing else in braces. Returns what is wrong.
pub fn check_translation(key: &str, translation: &str) -> Option<String> {
    let count = |s: &str| -> Result<Vec<usize>, String> {
        let mut out = Vec::new();
        for p in parse_strict(s)? {
            if let Piece::Arg(n) = p {
                out.push(n);
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    };
    let want = match count(key) {
        Ok(v) => v,
        Err(e) => return Some(format!("key: {e}")),
    };
    match count(translation) {
        Ok(have) if have == want => None,
        Ok(have) => Some(format!("placeholders {have:?}, expected {want:?}")),
        Err(e) => Some(e),
    }
}

/// Turn a Rust format string into a key: each `{…}` placeholder becomes `{0}`, `{1}`… in
/// order of appearance; doubled braces stay doubled.
pub fn normalize(format: &str) -> String {
    let mut out = String::with_capacity(format.len());
    let mut n = 0;
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push_str("{{");
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push_str("}}");
            }
            '{' => {
                for d in chars.by_ref() {
                    if d == '}' {
                        break;
                    }
                }
                out.push_str(&format!("{{{n}}}"));
                n += 1;
            }
            _ => out.push(c),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
enum Piece {
    Text(String),
    Arg(usize),
}

/// Split a key or translation into text and `{n}` placeholders; anything else in braces is
/// kept as text.
fn parse(s: &str) -> Vec<Piece> {
    parse_strict(s).unwrap_or_else(|_| vec![Piece::Text(s.to_string())])
}

fn parse_strict(s: &str) -> Result<Vec<Piece>, String> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                text.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                text.push('}');
            }
            '{' => {
                let mut inner = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(d) => inner.push(d),
                        None => return Err("unclosed {".into()),
                    }
                }
                let n = inner
                    .parse::<usize>()
                    .map_err(|_| format!("{{{inner}}} is not a placeholder like {{0}}"))?;
                if !text.is_empty() {
                    out.push(Piece::Text(std::mem::take(&mut text)));
                }
                out.push(Piece::Arg(n));
            }
            '}' => return Err("single } (write }} for a brace)".into()),
            _ => text.push(c),
        }
    }
    if !text.is_empty() {
        out.push(Piece::Text(text));
    }
    Ok(out)
}

fn render(pieces: &[Piece], args: &[String]) -> String {
    let mut out = String::new();
    for p in pieces {
        match p {
            Piece::Text(t) => out.push_str(t),
            Piece::Arg(n) => out.push_str(args.get(*n).map_or("", String::as_str)),
        }
    }
    out
}

struct Template {
    /// The key's text around its placeholders: one more than there are placeholders.
    parts: Vec<String>,
    /// How much literal text the key has: the more, the more specific the match.
    weight: usize,
    translation: Vec<Piece>,
}

impl Template {
    /// The text in each placeholder when `message` fits this key in exactly one way.
    fn captures<'a>(&self, message: &'a str) -> Option<Vec<&'a str>> {
        let (first, last) = (&self.parts[0], &self.parts[self.parts.len() - 1]);
        if message.len() < first.len() + last.len()
            || !message.starts_with(first.as_str())
            || !message.ends_with(last.as_str())
        {
            return None;
        }
        let body = &message[first.len()..message.len() - last.len()];
        let middle = &self.parts[1..self.parts.len() - 1];
        if middle.iter().any(String::is_empty) {
            return None; // two placeholders side by side: no way to tell where one ends
        }
        let mut found = None;
        let mut count = 0;
        solve(body, 0, middle, &mut Vec::new(), &mut found, &mut count);
        if count == 1 { found } else { None }
    }
}

fn solve<'a>(
    body: &'a str,
    start: usize,
    middle: &[String],
    caps: &mut Vec<&'a str>,
    found: &mut Option<Vec<&'a str>>,
    count: &mut usize,
) {
    let Some((part, rest)) = middle.split_first() else {
        caps.push(&body[start..]);
        *count += 1;
        if found.is_none() {
            *found = Some(caps.clone());
        }
        caps.pop();
        return;
    };
    let mut from = start;
    while let Some(i) = body[from..].find(part.as_str()) {
        let at = from + i;
        caps.push(&body[start..at]);
        solve(body, at + part.len(), rest, caps, found, count);
        caps.pop();
        if *count > 1 {
            return;
        }
        from = at + body[at..].chars().next().map_or(1, char::len_utf8);
    }
}

struct Catalog {
    /// Normalized key → translation, for [`fill`].
    keys: HashMap<String, Vec<Piece>>,
    /// Keys in file order.
    order: Vec<String>,
    /// The translations as written.
    raw: HashMap<String, String>,
    /// Messages without placeholders, as they appear at run time.
    exact: HashMap<String, String>,
    /// Keys with placeholders, most specific first.
    templates: Vec<Template>,
}

impl Catalog {
    fn load(json: &str) -> Catalog {
        let map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(json).unwrap_or_default();
        let mut c = Catalog {
            keys: HashMap::new(),
            order: Vec::new(),
            raw: HashMap::new(),
            exact: HashMap::new(),
            templates: Vec::new(),
        };
        for (key, value) in map {
            let Some(value) = value.as_str() else {
                continue;
            };
            let translation = parse(value);
            let pieces = parse(&key);
            let mut parts = vec![String::new()];
            let mut args = 0;
            for p in &pieces {
                match p {
                    Piece::Text(t) => parts.last_mut().unwrap().push_str(t),
                    Piece::Arg(_) => {
                        args += 1;
                        parts.push(String::new());
                    }
                }
            }
            if args == 0 {
                c.exact.insert(parts.remove(0), render(&translation, &[]));
            } else {
                let weight = parts.iter().map(String::len).sum();
                if weight > 0 {
                    c.templates.push(Template {
                        parts,
                        weight,
                        translation: translation.clone(),
                    });
                }
            }
            c.order.push(key.clone());
            c.raw.insert(key.clone(), value.to_string());
            c.keys.insert(key, translation);
        }
        c.templates.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.parts.len().cmp(&b.parts.len()))
        });
        c
    }

    fn translate(&self, message: &str, depth: usize) -> String {
        if let Some(t) = self.exact.get(message) {
            return t.clone();
        }
        if depth < 4 {
            // Inside a placeholder only keys with plenty of text count, so that a file name
            // such as "notes on disks" is not taken for "{0} on {1}".
            for t in self
                .templates
                .iter()
                .filter(|t| depth == 0 || t.weight >= 8)
            {
                if let Some(caps) = t.captures(message) {
                    let args: Vec<String> =
                        caps.iter().map(|c| self.translate(c, depth + 1)).collect();
                    return render(&t.translation, &args);
                }
            }
        }
        message.to_string()
    }
}

fn catalog_for(lang: &str) -> Option<&'static Catalog> {
    static LOADED: [OnceLock<Catalog>; 16] = [const { OnceLock::new() }; 16];
    let i = CATALOGS.iter().position(|(c, _)| *c == lang)?;
    Some(LOADED[i].get_or_init(|| Catalog::load(CATALOGS[i].1)))
}

fn catalog() -> Option<&'static Catalog> {
    catalog_for(language())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Catalog {
        Catalog::load(
            r#"{
              "unsupported: {0}": "nicht unterstützt: {0}",
              "writing {0} file systems with {1}": "Schreiben von {0}-Dateisystemen mit {1}",
              "wrong password": "falsches Kennwort",
              "{0} is encrypted: give its password": "{0} ist verschlüsselt: Kennwort angeben",
              "a {0} b {1}": "{1} B {0} A",
              "{{literal}} {0}": "{{wörtlich}} {0}"
            }"#,
        )
    }

    #[test]
    fn normalize_renumbers_placeholders() {
        assert_eq!(normalize("cannot open {}: {e:#}"), "cannot open {0}: {1}");
        assert_eq!(normalize("{p:?} at {:>8} {{x}}"), "{0} at {1} {{x}}");
    }

    #[test]
    fn nested_messages_translate_piece_by_piece() {
        let c = sample();
        assert_eq!(c.translate("wrong password", 0), "falsches Kennwort");
        assert_eq!(
            c.translate("unsupported: writing ext4 file systems with journals", 0),
            "nicht unterstützt: Schreiben von ext4-Dateisystemen mit journals"
        );
        assert_eq!(
            c.translate("x.imz is encrypted: give its password", 0),
            "x.imz ist verschlüsselt: Kennwort angeben"
        );
        assert_eq!(c.translate("a 1 b 2", 0), "2 B 1 A");
        assert_eq!(c.translate("{literal} z", 0), "{wörtlich} z");
        assert_eq!(c.translate("no such message", 0), "no such message");
    }

    #[test]
    fn ambiguous_messages_stay_english() {
        // "a x b y b z": the first placeholder could end at either " b ".
        assert_eq!(sample().translate("a x b y b z", 0), "a x b y b z");
    }

    #[test]
    fn languages_match_like_the_app() {
        assert_eq!(matching("de-AT"), Some("de"));
        assert_eq!(matching("pt_BR.UTF-8"), Some("pt-BR"));
        assert_eq!(matching("pt-PT"), Some("pt-BR"));
        assert_eq!(matching("zh-Hant-HK"), Some("zh-TW"));
        assert_eq!(matching("zh_CN"), Some("zh-CN"));
        assert_eq!(matching("C"), Some("en"));
        assert_eq!(matching("sv-SE"), None);
    }

    #[test]
    fn translations_are_checked() {
        assert_eq!(check_translation("a {0} b {1}", "{1} {0}"), None);
        assert!(check_translation("a {0} b {1}", "{0}").is_some());
        assert!(check_translation("a {0}", "{name}").is_some());
        assert!(check_translation("a", "b }").is_some());
    }

    #[test]
    fn catalogs_parse() {
        for (code, json) in CATALOGS {
            let parsed: Result<serde_json::Map<String, serde_json::Value>, _> =
                serde_json::from_str(json);
            assert!(parsed.is_ok(), "locales/{code}.json: {:?}", parsed.err());
        }
    }
}
