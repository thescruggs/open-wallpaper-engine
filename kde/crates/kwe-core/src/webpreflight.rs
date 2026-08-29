// SPDX-License-Identifier: GPL-3.0-or-later
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

const MAX_HTML_BYTES: u64 = 16 * 1024 * 1024;

/// The entry must be HTML-ish text, not an arbitrary blob — but real
/// Workshop wallpapers routinely ship fragment entries with no `<html>`
/// root at all (HTML5 makes html/head/body optional; Chromium and
/// Wallpaper Engine both load them), and some are UTF-16 encoded. Accept
/// any known top-of-document marker after stripping NULs (the lossy read
/// of UTF-16 text interleaves them), instead of demanding a literal
/// `<html` (which refused e.g. Workshop 1103493745 "Colorful Matrix").
fn looks_like_html(bytes: &[u8]) -> bool {
    let text: String = String::from_utf8_lossy(bytes)
        .chars()
        .filter(|character| *character != '\0')
        .collect::<String>()
        .to_ascii_lowercase();
    [
        "<!doctype html",
        "<html",
        "<head",
        "<body",
        "<script",
        "<style",
        "<link",
        "<meta",
        "<canvas",
        "<div",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebPreflight {
    pub path: PathBuf,
    pub safe: bool,
    pub network_allowed: bool,
    pub permissions: Vec<String>,
    pub reasons: Vec<String>,
}

pub fn preflight_web(root: &Path, permissions: &[String]) -> WebPreflight {
    let mut report = WebPreflight {
        path: root.to_path_buf(),
        safe: false,
        network_allowed: false,
        permissions: permissions
            .iter()
            .filter(|p| matches!(p.as_str(), "pointer" | "audio" | "network"))
            .cloned()
            .collect(),
        reasons: Vec::new(),
    };
    let entry = root.join("index.html");
    let metadata = match fs::symlink_metadata(&entry) {
        Ok(value) => value,
        Err(error) => {
            report
                .reasons
                .push(format!("cannot stat index.html: {error}"));
            return report;
        }
    };
    if metadata.file_type().is_symlink() {
        report
            .reasons
            .push("index.html must not be a symlink".into());
        return report;
    }
    if !metadata.is_file() {
        report
            .reasons
            .push("index.html must be a regular file".into());
        return report;
    }
    if metadata.len() > MAX_HTML_BYTES {
        report
            .reasons
            .push(format!("index.html exceeds {MAX_HTML_BYTES} byte limit"));
        return report;
    }
    let bytes = match fs::read(&entry) {
        Ok(value) => value,
        Err(error) => {
            report
                .reasons
                .push(format!("cannot read index.html: {error}"));
            return report;
        }
    };
    if !looks_like_html(&bytes) {
        report
            .reasons
            .push("index.html does not look like an HTML document".into());
    }
    report.network_allowed = false;
    report.safe = report.reasons.is_empty();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_html_entry_and_keeps_network_disabled() {
        let root = std::env::temp_dir().join(format!("kwe-web-preflight-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("index.html"), "<html><body>ok</body></html>").unwrap();
        let report = preflight_web(&root, &["network".into(), "pointer".into()]);
        assert!(report.safe);
        assert!(!report.network_allowed);
        assert_eq!(report.permissions, ["network", "pointer"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn accepts_fragment_and_utf16_entries_rejects_non_html() {
        let root =
            std::env::temp_dir().join(format!("kwe-web-preflight-fragment-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        // Workshop 1103493745 "Colorful Matrix": a fragment entry with no
        // <html> root — head + canvas + script only. Must pass.
        fs::write(
            root.join("index.html"),
            "<head>\n<canvas id=\"c\"></canvas>\n<script src=\"./index.js\"></script>\n</head>\n",
        )
        .unwrap();
        assert!(
            preflight_web(&root, &[]).safe,
            "a fragment entry without an <html> root must pass"
        );

        // The same fragment in UTF-16LE (lossy read interleaves NULs).
        let utf16: Vec<u8> = "<body><script>go()</script></body>"
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        fs::write(root.join("index.html"), utf16).unwrap();
        assert!(
            preflight_web(&root, &[]).safe,
            "a UTF-16 encoded entry must pass"
        );

        // Non-HTML content still fails closed.
        fs::write(root.join("index.html"), b"\x7fELF not a web page at all").unwrap();
        let report = preflight_web(&root, &[]);
        assert!(!report.safe);
        assert!(
            report.reasons[0].contains("does not look like an HTML document"),
            "{:?}",
            report.reasons
        );
        let _ = fs::remove_dir_all(root);
    }
}
