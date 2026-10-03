// SPDX-License-Identifier: GPL-3.0-or-later
//! Request envelope parsing, error codes and response rendering
//! (`docs/WORKSHOP_PROTOCOL_V1.md` section 1).

use serde::Deserialize;
use serde_json::{Value, json};

pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Every way an operation can fail, mapped 1:1 onto the contract's codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fail {
    BadRequest(String),
    SignedOut,
    Offline,
    Timeout,
    RateLimited,
    Steam(u16),
    Protocol(&'static str),
    Internal(&'static str),
}

impl Fail {
    pub fn code(&self) -> &'static str {
        match self {
            Fail::BadRequest(_) => "bad_request",
            Fail::SignedOut => "signed_out",
            Fail::Offline => "offline",
            Fail::Timeout => "timeout",
            Fail::RateLimited => "rate_limited",
            Fail::Steam(_) => "steam_error",
            Fail::Protocol(_) => "protocol",
            Fail::Internal(_) => "internal",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Fail::BadRequest(d) => d.clone(),
            Fail::SignedOut => "not signed in to Steam".into(),
            Fail::Offline => "could not reach Steam".into(),
            Fail::Timeout => "Steam did not answer in time".into(),
            Fail::RateLimited => "Steam is rate limiting requests (HTTP 429)".into(),
            Fail::Steam(status) => format!("Steam answered HTTP {status}"),
            Fail::Protocol(d) | Fail::Internal(d) => (*d).into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub version: u32,
    pub op: String,
    #[serde(default)]
    pub params: Value,
}

pub fn parse_envelope(input: &[u8]) -> Result<Envelope, Fail> {
    if input.len() > MAX_REQUEST_BYTES {
        return Err(Fail::BadRequest("request exceeds 64 KiB".into()));
    }
    let text =
        std::str::from_utf8(input).map_err(|_| Fail::BadRequest("request is not UTF-8".into()))?;
    let text = text.trim_end_matches(['\n', '\r']);
    if text.contains('\n') {
        return Err(Fail::BadRequest("request must be a single line".into()));
    }
    let envelope: Envelope = serde_json::from_str(text)
        .map_err(|_| Fail::BadRequest("request is not a valid envelope".into()))?;
    if envelope.version != 1 {
        return Err(Fail::BadRequest("unsupported version".into()));
    }
    Ok(envelope)
}

/// Parses an op's params, rejecting unknown fields (the param structs carry
/// `deny_unknown_fields`). A missing/null params object counts as `{}`.
pub fn parse_params<T: for<'de> Deserialize<'de>>(params: &Value) -> Result<T, Fail> {
    let value = if params.is_null() {
        json!({})
    } else {
        params.clone()
    };
    serde_json::from_value(value).map_err(|_| Fail::BadRequest("invalid params".into()))
}

pub fn render(result: Result<Value, Fail>) -> String {
    let value = match result {
        Ok(result) => json!({"version":1,"ok":true,"result":result}),
        Err(fail) => error_value(&fail),
    };
    let line = value.to_string();
    if line.len() > MAX_RESPONSE_BYTES {
        return error_value(&Fail::Internal("response too large")).to_string();
    }
    line
}

fn error_value(fail: &Fail) -> Value {
    json!({"version":1,"ok":false,"error":fail.code(),"detail":fail.detail()})
}

/// 1–20 ASCII digits, not all zero.
pub fn valid_id(id: &str) -> bool {
    (1..=20).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_digit())
        && id.bytes().any(|b| b != b'0')
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdParams {
    pub id: String,
}

impl IdParams {
    pub fn checked(self) -> Result<String, Fail> {
        if valid_id(&self.id) {
            Ok(self.id)
        } else {
            Err(Fail::BadRequest("id must be 1-20 digits, not 0".into()))
        }
    }
}
