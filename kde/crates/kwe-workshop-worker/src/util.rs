// SPDX-License-Identifier: GPL-3.0-or-later
//! Small shared helpers: form encoding, a strict https URL splitter, clock,
//! randomness, and the bounded `event=` stderr record.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// One short diagnostic line. Callers pass only fixed words and numbers,
/// never URLs, tokens, cookies or request ids.
pub fn log(event: &str, detail: &str) {
    let detail: String = detail
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(120)
        .collect();
    eprintln!("event={event} {detail}");
}

/// application/x-www-form-urlencoded value encoding (RFC 3986 unreserved
/// bytes kept, everything else %XX).
pub fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn encode_pairs(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Truncates to at most `max` characters.
pub fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// Returns the lower-cased host of a plain `https://host[:443]/...` URL, or
/// None for anything else (other schemes, userinfo, odd ports, control
/// characters, backslashes, non-ASCII hosts).
pub fn https_host(url: &str) -> Option<String> {
    if url.len() > 2048 || url.bytes().any(|b| b <= 0x20 || b == 0x7f || b == b'\\') {
        return None;
    }
    let rest = url.strip_prefix("https://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains('@') {
        return None;
    }
    let host = match authority.rsplit_once(':') {
        Some((host, "443")) => host,
        Some(_) => return None,
        None => authority,
    };
    let host = host.to_ascii_lowercase();
    let valid = !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('.')
        && !host.ends_with('.')
        && !host.contains("..")
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.');
    valid.then_some(host)
}

/// 12 random bytes as hex (the web `sessionid` cookie value).
pub fn random_hex12() -> String {
    let mut buf = [0u8; 12];
    // SAFETY: the buffer is valid for `buf.len()` bytes.
    let got = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
    if got != buf.len() as isize {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(format!("{}-{}", unix_nanos(), std::process::id()));
        buf.copy_from_slice(&digest[..12]);
    }
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes() {
        assert_eq!(form_encode("a b/c+d=="), "a%20b%2Fc%2Bd%3D%3D");
        assert_eq!(percent_decode("1%7C%7Cx").as_deref(), Some("1||x"));
        assert_eq!(percent_decode("%zz"), None);
    }

    #[test]
    fn https_host_is_strict() {
        assert_eq!(
            https_host("https://Images.Example.com/a.jpg").as_deref(),
            Some("images.example.com")
        );
        assert_eq!(
            https_host("https://a.example.com:443/x").as_deref(),
            Some("a.example.com")
        );
        for bad in [
            "http://a.example.com/",
            "https://u@a.example.com/",
            "https://a.example.com:8443/",
            "https://a.example.com\\@evil/",
            "https://a b.example.com/",
            "https:///x",
            "https://a..b/",
            "ftp://a.example.com/",
        ] {
            assert_eq!(https_host(bad), None, "{bad}");
        }
    }
}
