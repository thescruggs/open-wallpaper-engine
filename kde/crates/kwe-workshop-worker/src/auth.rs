// SPDX-License-Identifier: GPL-3.0-or-later
// Borrowed-From: waywallen/open-wallpaper-engine@b8180ad:waywallen/plugins/org.waywallen.open-wallpaper-engine/wallpaper_engine/auth.lua (GPL-2.0; protocol reference — endpoints, field names and state machine; independently implemented)
//! QR sign-in: `auth.begin`, one-round-trip `auth.poll`, `auth.status`,
//! `auth.sign_out`.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    envelope::Fail,
    jwt::Claims,
    session::{Session, community_headers, refresh_usable},
    store::{Credential, CredentialStore, Secret},
    transport::{Body, Request, Transport, check_status},
    util::{log, truncate_chars, unix_seconds},
};

const BEGIN: &str = "https://api.steampowered.com/IAuthenticationService/BeginAuthSessionViaQR/v1/";
const POLL: &str = "https://api.steampowered.com/IAuthenticationService/PollAuthSessionStatus/v1/";
const EXPIRES_IN_MS: u64 = 180_000;
const STEAM_ERESULT_EXPIRED: &str = "27";

pub fn status(store: &CredentialStore) -> Value {
    let signed = store.load().filter(|c| {
        refresh_usable(c.refresh_token.expose(), Some(&c.steam_id), unix_seconds()).is_some()
    });
    match signed {
        Some(c) => json!({"signed_in":true,"account_name":c.account_name,"steam_id":c.steam_id}),
        None => json!({"signed_in":false,"account_name":"","steam_id":""}),
    }
}

fn string_or_number(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn challenge_ok(url: &str) -> bool {
    url.len() <= 2048 && url.starts_with("https://") && url.bytes().all(|b| b > 0x20 && b != 0x7f)
}

pub fn begin(transport: &dyn Transport) -> Result<Value, Fail> {
    let input = json!({"device_details":{
        "device_friendly_name":"KDE Wallpaper Engine","platform_type":2}});
    let response = transport.send(&community_headers(Request::post(
        BEGIN,
        Body::Form(vec![("input_json".into(), input.to_string())]),
    )))?;
    check_status(&response)?;
    let body = response.json()?;
    let r = body.get("response").unwrap_or(&Value::Null);
    let client_id = string_or_number(r.get("client_id")).filter(|s| !s.is_empty() && s.len() <= 32);
    let request_id = r
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 512);
    let challenge = r
        .get("challenge_url")
        .and_then(Value::as_str)
        .filter(|s| challenge_ok(s));
    let (Some(client_id), Some(request_id), Some(challenge)) = (client_id, request_id, challenge)
    else {
        return Err(Fail::Protocol("incomplete QR login challenge"));
    };
    let interval = r.get("interval").and_then(Value::as_f64).unwrap_or(5.0);
    let interval_ms = (interval * 1000.0).clamp(1000.0, 30_000.0) as u64;
    Ok(json!({
        "client_id": client_id,
        "request_id": request_id,
        "challenge_url": challenge,
        "poll_interval_ms": interval_ms,
        "expires_in_ms": EXPIRES_IN_MS,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollParams {
    client_id: String,
    request_id: String,
    challenge_url: String,
}

impl PollParams {
    fn check(&self) -> Result<(), Fail> {
        let ok = (1..=32).contains(&self.client_id.len())
            && self.client_id.bytes().all(|b| b.is_ascii_digit())
            && (1..=512).contains(&self.request_id.len())
            && self.request_id.bytes().all(|b| b > 0x20 && b != 0x7f)
            && challenge_ok(&self.challenge_url);
        ok.then_some(())
            .ok_or_else(|| Fail::BadRequest("invalid poll params".into()))
    }
}

fn poll_result(state: &str, client_id: &str, challenge: &str, account: &str) -> Value {
    json!({"state":state,"client_id":client_id,"challenge_url":challenge,"account_name":account})
}

pub fn poll(session: &Session, params: PollParams) -> Result<Value, Fail> {
    params.check()?;
    let response = session.transport.send(&community_headers(Request::post(
        POLL,
        Body::Form(vec![
            ("client_id".into(), params.client_id.clone()),
            ("request_id".into(), params.request_id.clone()),
            ("format".into(), "json".into()),
        ]),
    )))?;
    if response.header("x-eresult") == Some(STEAM_ERESULT_EXPIRED) {
        return Ok(poll_result(
            "expired",
            &params.client_id,
            &params.challenge_url,
            "",
        ));
    }
    check_status(&response)?;
    let body = response.json()?;
    let r = body.get("response").unwrap_or(&Value::Null);

    if let Some(refresh) = r
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let account = truncate_chars(
            r.get("account_name")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            64,
        );
        let access = r
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return complete(session, &account, refresh, access, &params);
    }

    let mut client_id = params.client_id.clone();
    if let Some(new_id) = string_or_number(r.get("new_client_id")).filter(|s| {
        !s.is_empty() && s != "0" && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_digit())
    }) {
        client_id = new_id;
    }
    if let Some(new_url) = r
        .get("new_challenge_url")
        .and_then(Value::as_str)
        .filter(|u| challenge_ok(u) && *u != params.challenge_url)
    {
        return Ok(poll_result("challenge_changed", &client_id, new_url, ""));
    }
    let state = if r.get("had_remote_interaction").and_then(Value::as_bool) == Some(true) {
        "awaiting_confirmation"
    } else {
        "awaiting_scan"
    };
    Ok(poll_result(state, &client_id, &params.challenge_url, ""))
}

fn complete(
    session: &Session,
    account: &str,
    refresh: &str,
    access: &str,
    params: &PollParams,
) -> Result<Value, Fail> {
    let now = unix_seconds();
    let claims = Claims::parse(refresh)
        .filter(|c| {
            !c.sub.is_empty() && c.sub.len() <= 20 && c.sub.bytes().all(|b| b.is_ascii_digit())
        })
        .ok_or(Fail::Protocol("invalid refresh token from Steam"))?;
    refresh_usable(refresh, Some(&claims.sub), now)
        .ok_or(Fail::Protocol("invalid refresh token from Steam"))?;
    // The login access token is kept only when it is well-formed for this account.
    let access = Claims::parse(access)
        .filter(|c| c.live(now, Some(&claims.sub)) && !c.has_aud("derive"))
        .map(|_| access)
        .unwrap_or_default();
    let mut credential = Credential {
        schema_version: 1,
        account_name: account.to_string(),
        steam_id: claims.sub.clone(),
        refresh_token: Secret::new(refresh),
        access_token: Secret::new(access),
        community_access_token: Secret::default(),
    };
    session
        .store
        .save(&credential)
        .map_err(|_| Fail::Internal("could not store the credential"))?;
    // Derive the community token now so the first query is fast. A refusal
    // signs out (the file is removed); a transient failure keeps the login.
    match session.derive(&mut credential) {
        Ok(_) => {}
        Err(Fail::SignedOut) => return Err(Fail::SignedOut),
        Err(error) => log("auth.derive_deferred", error.code()),
    }
    Ok(poll_result(
        "signed_in",
        &params.client_id,
        &params.challenge_url,
        account,
    ))
}

pub fn sign_out(store: &CredentialStore) -> Value {
    // Revocation of the refresh token is not attempted: it needs another
    // endpoint this worker has no reference for. The local credential is gone.
    store.remove();
    json!({"signed_in":false})
}
