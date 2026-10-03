// SPDX-License-Identifier: GPL-3.0-or-later
//! Local, unverified JWT inspection: expiry, subject and audience only.
//! Steam validates signatures; the worker just avoids sending dead tokens.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;

const MAX_TOKEN_BYTES: usize = 8 * 1024;
/// A token that expires within this many seconds counts as expired.
const SKEW_SECONDS: u64 = 60;

#[derive(Debug, Clone)]
pub struct Claims {
    exp: u64,
    pub sub: String,
    aud: Vec<String>,
}

impl Claims {
    pub fn parse(token: &str) -> Option<Claims> {
        if token.len() > MAX_TOKEN_BYTES {
            return None;
        }
        let mut parts = token.split('.');
        let (_, payload, _, None) = (parts.next()?, parts.next()?, parts.next()?, parts.next())
        else {
            return None;
        };
        let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        let object = value.as_object()?;
        let exp = match object.get("exp")? {
            Value::Number(n) => n
                .as_u64()
                .or_else(|| n.as_f64().map(|f| f.max(0.0) as u64))?,
            _ => return None,
        };
        let sub = object
            .get("sub")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let aud = match object.get("aud") {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .take(16)
                .collect(),
            _ => Vec::new(),
        };
        Some(Claims { exp, sub, aud })
    }

    /// Not expired (with skew) and, when given, issued for `steam_id`.
    pub fn live(&self, now: u64, steam_id: Option<&str>) -> bool {
        self.exp > now.saturating_add(SKEW_SECONDS) && steam_id.is_none_or(|id| self.sub == id)
    }

    pub fn has_aud(&self, audience: &str) -> bool {
        self.aud.iter().any(|a| a == audience)
    }
}

#[cfg(test)]
pub fn fake_jwt(exp: u64, sub: &str, aud: &[&str]) -> String {
    let payload = serde_json::json!({"exp": exp, "sub": sub, "aud": aud}).to_string();
    format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
        URL_SAFE_NO_PAD.encode(payload),
        "sig"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_expiry_subject_audience() {
        let claims = Claims::parse(&fake_jwt(1000, "42", &["web", "derive"])).unwrap();
        assert!(claims.live(900, Some("42")));
        assert!(!claims.live(950, Some("42")), "inside the skew window");
        assert!(!claims.live(900, Some("43")));
        assert!(claims.has_aud("derive") && !claims.has_aud("client"));
    }

    #[test]
    fn rejects_malformed() {
        for bad in ["", "a.b", "a.b.c.d", "a.!!!.c", "a.e30.c"] {
            assert!(Claims::parse(bad).is_none(), "{bad}");
        }
    }
}
