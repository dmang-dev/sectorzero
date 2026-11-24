//! Directory listings as text, CSV or HTML (WinImage's "Export directory listing").

use serde::{Deserialize, Serialize};

use crate::volume::Entry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListingFormat {
    Text,
    Csv,
    Html,
}

impl ListingFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ListingFormat::Text => "txt",
            ListingFormat::Csv => "csv",
            ListingFormat::Html => "html",
        }
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const HTML_HEAD: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{TITLE}</title>
<style>
body { font: 14px system-ui, sans-serif; margin: 24px; color: #1b1f24; background: #fff; }
table { border-collapse: collapse; }
th, td { padding: 3px 12px; text-align: left; border-bottom: 1px solid #ddd; }
td.n { text-align: right; font-variant-numeric: tabular-nums; }
@media (prefers-color-scheme: dark) {
  body { color: #e6e6e6; background: #16181c; }
  th, td { border-color: #333; }
}
</style>
</head>
<body>
<h1>{TITLE}</h1>
<table>
<tr><th>Path</th><th>Size</th><th>Modified</th><th>Attributes</th></tr>
"#;

/// Render `entries` (typically from `Volume::walk`) under `title` (e.g. the image name).
pub fn render_listing(title: &str, entries: &[Entry], format: ListingFormat) -> String {
    let files = entries.iter().filter(|e| !e.is_dir).count();
    let dirs = entries.len() - files;
    let bytes: u64 = entries.iter().map(|e| e.size).sum();
    let summary = format!("{files} file(s), {dirs} folder(s), {bytes} bytes");
    match format {
        ListingFormat::Text => {
            let mut out = format!("{title}\n\n");
            let width = entries
                .iter()
                .map(|e| e.size.to_string().len())
                .max()
                .unwrap_or(1)
                .max(5);
            for e in entries {
                let size = if e.is_dir {
                    "<DIR>".to_string()
                } else {
                    e.size.to_string()
                };
                let slash = if e.is_dir { "/" } else { "" };
                out.push_str(&format!(
                    "{size:>width$}  {:<19}  {}  {}{slash}\n",
                    e.modified,
                    e.attributes.flags(),
                    e.path
                ));
            }
            out.push_str(&format!("\n{summary}\n"));
            out
        }
        ListingFormat::Csv => {
            let mut out = String::from("path,type,size,modified,created,attributes\r\n");
            for e in entries {
                out.push_str(&format!(
                    "{},{},{},{},{},{}\r\n",
                    csv_field(&e.path),
                    if e.is_dir { "folder" } else { "file" },
                    if e.is_dir { 0 } else { e.size },
                    e.modified,
                    e.created,
                    e.attributes.flags()
                ));
            }
            out
        }
        ListingFormat::Html => {
            let mut out = HTML_HEAD.replace("{TITLE}", &html(title));
            for e in entries {
                let size = if e.is_dir {
                    String::new()
                } else {
                    e.size.to_string()
                };
                out.push_str(&format!(
                    "<tr><td>{}{}</td><td class=\"n\">{size}</td><td>{}</td><td>{}</td></tr>\n",
                    html(&e.path),
                    if e.is_dir { "/" } else { "" },
                    html(&e.modified),
                    e.attributes.flags()
                ));
            }
            out.push_str(&format!("</table>\n<p>{summary}</p>\n</body>\n</html>\n"));
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::Attributes;

    fn entry(path: &str, is_dir: bool, size: u64) -> Entry {
        Entry {
            name: path.rsplit('/').next().unwrap().into(),
            short_name: String::new(),
            path: path.into(),
            is_dir,
            size,
            modified: "2026-11-22 10:00:00".into(),
            created: String::new(),
            accessed: String::new(),
            attributes: Attributes {
                read_only: false,
                hidden: false,
                system: false,
                archive: !is_dir,
            },
            mac: None,
        }
    }

    #[test]
    fn formats() {
        let e = [entry("DOCS", true, 0), entry("DOCS/a,b <x>.txt", false, 12)];
        let t = render_listing("disk.ima", &e, ListingFormat::Text);
        assert!(t.contains("<DIR>") && t.contains("DOCS/a,b <x>.txt") && t.contains("1 file(s)"));
        let c = render_listing("disk.ima", &e, ListingFormat::Csv);
        assert!(c.contains("\"DOCS/a,b <x>.txt\",file,12,"));
        let h = render_listing("disk.ima", &e, ListingFormat::Html);
        assert!(h.contains("DOCS/a,b &lt;x&gt;.txt") && !h.contains("<x>"));
    }
}
