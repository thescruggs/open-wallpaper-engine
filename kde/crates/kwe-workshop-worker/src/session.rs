// SPDX-License-Identifier: GPL-3.0-or-later
// Borrowed-From: waywallen/open-wallpaper-engine@b8180ad:waywallen/plugins/org.waywallen.open-wallpaper-engine/wallpaper_engine/session.lua (GPL-2.0; protocol reference — endpoints, field names and state machine; independently implemented)
//! Token lifecycle. The refresh token (WebBrowser platform, audiences `web`
//! and `derive`) is exchanged at `login.steampowered.com/jwt/finalizelogin`
//! for transfer URLs; posting to them yields a `steamLoginSecure` cookie
//! whose JWT (audience `web:community`) is the access token sent to the
//! Workshop API. A 401/403 re-derives once and retries once; a second
//! rejection signs the user out and deletes the credential file.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::{
    envelope::Fail,
    jwt::Claims,
    store::{Credential, CredentialStore, Secret},
    transport::{Body, Request, Response, Transport, check_status},
    util::{log, percent_decode, random_hex12, unix_seconds},
};

const FINALIZE_LOGIN: &str = "https://login.steampowered.com/jwt/finalizelogin";
const MAX_TRANSFERS: usize = 8;
const TRANSFER_ATTEMPTS: usize = 2;
const COMMUNITY_HOST: &str = "steamcommunity.com";
/// Wall clock for one derivation; the daemon kills the worker at 40 s and a
/// single HTTP call may take 15 s.
const DERIVE_BUDGET: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: usize = 3;

pub fn community_headers(request: Request) -> Request {
    request
        .header("Accept", "application/json, text/plain, */*")
        .header("Origin", "https://steamcommunity.com")
        .header("Referer", "https://steamcommunity.com/")
        .header("Sec-Fetch-Site", "cross-site")
        .header("Sec-Fetch-Mode", "cors")
        .header("Sec-Fetch-Dest", "empty")
}

pub struct Session<'a> {
    pub transport: &'a dyn Transport,
    pub store: &'a CredentialStore,
    pub retry_delay: Duration,
}

/// A refresh token we can still use: unexpired and issued to this account
/// for the `web` and `derive` audiences.
pub fn refresh_usable(token: &str, steam_id: Option<&str>, now: u64) -> Option<Claims> {
    let claims = Claims::parse(token)?;
    (claims.live(now, steam_id) && claims.has_aud("web") && claims.has_aud("derive"))
        .then_some(claims)
}

fn community_usable(token: &str, steam_id: &str, now: u64) -> bool {
    Claims::parse(token).is_some_and(|c| c.live(now, Some(steam_id)) && c.has_aud("web:community"))
}

fn host_allowed(host: &str) -> bool {
    ["steampowered.com", "steamcommunity.com"]
        .iter()
        .any(|root| host == *root || host.ends_with(&format!(".{root}")))
}

impl Session<'_> {
    /// The stored credential with a usable refresh token, or signed out
    /// (deleting the file when the refresh token is dead).
    pub fn credential(&self) -> Result<Credential, Fail> {
        let credential = self.store.load().ok_or(Fail::SignedOut)?;
        if refresh_usable(
            credential.refresh_token.expose(),
            Some(&credential.steam_id),
            unix_seconds(),
        )
        .is_none()
        {
            log("session.refresh_expired", "");
            self.store.remove();
            return Err(Fail::SignedOut);
        }
        Ok(credential)
    }

    /// Runs an authenticated API call. `build` receives the access token and
    /// returns the request to send.
    pub fn authorized<F>(&self, build: F) -> Result<Response, Fail>
    where
        F: Fn(&str) -> Request,
    {
        let mut credential = self.credential()?;
        let now = unix_seconds();
        let cached = [&credential.community_access_token, &credential.access_token]
            .into_iter()
            .find(|t| community_usable(t.expose(), &credential.steam_id, now))
            .map(|t| t.expose().to_string());
        let token = match cached {
            Some(token) => token,
            None => self.derive(&mut credential)?,
        };
        let response = self.transport.send(&build(&token))?;
        if !matches!(response.status, 401 | 403) {
            return Ok(response);
        }
        log("session.rejected", "rederiving");
        let token = self.derive(&mut credential)?;
        let response = self.transport.send(&build(&token))?;
        if matches!(response.status, 401 | 403) {
            log("session.rejected_twice", "signing out");
            self.remove_if_current(&credential);
            return Err(Fail::SignedOut);
        }
        Ok(response)
    }

    /// Derives a fresh community access token from the refresh token and
    /// stores it. A refusal by Steam deletes the credential (signed out);
    /// transient failures leave it in place.
    pub fn derive(&self, credential: &mut Credential) -> Result<String, Fail> {
        match self.derive_inner(credential) {
            Err(Fail::SignedOut) => {
                self.remove_if_current(credential);
                Err(Fail::SignedOut)
            }
            other => other,
        }
    }

    fn derive_inner(&self, credential: &mut Credential) -> Result<String, Fail> {
        let now = unix_seconds();
        if refresh_usable(
            credential.refresh_token.expose(),
            Some(&credential.steam_id),
            now,
        )
        .is_none()
        {
            return Err(Fail::SignedOut);
        }
        let sessionid = random_hex12();
        let finalize = self.transport.send(&community_headers(Request::post(
            FINALIZE_LOGIN,
            Body::Multipart(vec![
                ("nonce".into(), credential.refresh_token.expose().into()),
                ("sessionid".into(), sessionid),
                (
                    "redir".into(),
                    "https://steamcommunity.com/login/home/?goto=".into(),
                ),
            ]),
        )))?;
        if matches!(finalize.status, 401 | 403) {
            return Err(Fail::SignedOut);
        }
        check_status(&finalize)?;
        let body = finalize.json()?;
        if body.get("error").is_some() {
            return Err(Fail::SignedOut);
        }
        if let Some(id) = body.get("steamID") {
            let id = id
                .as_str()
                .map(str::to_string)
                .or_else(|| id.as_u64().map(|n| n.to_string()));
            if id.as_deref() != Some(credential.steam_id.as_str()) {
                return Err(Fail::Protocol("finalize answered for another account"));
            }
        }
        let transfers = body
            .get("transfer_info")
            .and_then(Value::as_array)
            .filter(|t| !t.is_empty() && t.len() <= MAX_TRANSFERS)
            .ok_or(Fail::Protocol("incomplete finalize response"))?;

        // Only the Community transfer yields the cookie the Workshop API
        // accepts; the other sites' transfers are not needed and are skipped.
        let deadline = Instant::now() + DERIVE_BUDGET;
        let mut raw = None;
        for transfer in transfers {
            let is_community = transfer
                .get("url")
                .and_then(Value::as_str)
                .and_then(crate::util::https_host)
                .is_some_and(|host| host == COMMUNITY_HOST);
            if !is_community {
                continue;
            }
            raw = self.run_transfer(transfer, &credential.steam_id, deadline)?;
            break;
        }
        let raw = raw.ok_or(Fail::Protocol("transfer returned no login cookie"))?;
        let decoded = percent_decode(&raw).ok_or(Fail::Protocol("bad login cookie"))?;
        let (id, token) = decoded
            .split_once("||")
            .ok_or(Fail::Protocol("bad login cookie"))?;
        if id != credential.steam_id || !community_usable(token, id, now) {
            return Err(Fail::Protocol("login cookie is not a community token"));
        }
        credential.community_access_token = Secret::new(token);
        // A sign-out or a new sign-in may have happened while this ran; never
        // write a credential back over that.
        if !self.is_current(credential) {
            return Err(Fail::SignedOut);
        }
        if self.store.save(credential).is_err() {
            log("session.save_error", "");
        }
        Ok(token.to_string())
    }

    /// Whether the credential file still holds the refresh token this run
    /// started from.
    fn is_current(&self, credential: &Credential) -> bool {
        self.store.load().is_some_and(|stored| {
            stored.refresh_token.expose() == credential.refresh_token.expose()
        })
    }

    fn remove_if_current(&self, credential: &Credential) {
        if self.is_current(credential) {
            self.store.remove();
        }
    }

    /// Posts one transfer; returns the `steamLoginSecure` cookie value if
    /// the answer set one.
    fn run_transfer(
        &self,
        transfer: &Value,
        steam_id: &str,
        deadline: Instant,
    ) -> Result<Option<String>, Fail> {
        let url = transfer
            .get("url")
            .and_then(Value::as_str)
            .ok_or(Fail::Protocol("invalid transfer"))?;
        let params = transfer
            .get("params")
            .and_then(Value::as_object)
            .filter(|p| p.len() <= 32)
            .ok_or(Fail::Protocol("invalid transfer"))?;
        crate::util::https_host(url)
            .filter(|h| host_allowed(h))
            .ok_or(Fail::Protocol("transfer to an unexpected host"))?;
        let mut fields = vec![("steamID".to_string(), steam_id.to_string())];
        for (key, value) in params {
            let value = match value {
                Value::String(s) if s.len() <= 4096 => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => return Err(Fail::Protocol("invalid transfer")),
            };
            fields.push((key.clone(), value));
        }
        let mut last = Fail::Protocol("transfer failed");
        for attempt in 0..TRANSFER_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(self.retry_delay);
            }
            if Instant::now() >= deadline {
                return Err(Fail::Timeout);
            }
            match self.post_transfer(url, &fields, deadline) {
                Ok(cookie) => return Ok(cookie),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    fn post_transfer(
        &self,
        url: &str,
        fields: &[(String, String)],
        deadline: Instant,
    ) -> Result<Option<String>, Fail> {
        let mut cookie = None;
        let mut next = Request::post(url, Body::Multipart(fields.to_vec()));
        for _ in 0..=MAX_REDIRECTS {
            if Instant::now() >= deadline {
                return Err(Fail::Timeout);
            }
            let response = self.transport.send(&next)?;
            for header in response.headers_named("set-cookie") {
                if let Some(value) = header.strip_prefix("steamLoginSecure=") {
                    cookie = Some(value.split(';').next().unwrap_or_default().to_string());
                }
            }
            if (300..400).contains(&response.status) {
                let location = response
                    .header("location")
                    .ok_or(Fail::Protocol("redirect without location"))?;
                let absolute = if location.starts_with('/') {
                    format!("{}{location}", url_origin(url))
                } else {
                    location.to_string()
                };
                if !crate::util::https_host(&absolute).is_some_and(|h| host_allowed(&h)) {
                    return Err(Fail::Protocol("transfer redirected to an unexpected host"));
                }
                next = Request::get(absolute);
                continue;
            }
            check_status(&response)?;
            if let Ok(parsed) = response.json()
                && let Some(result) = parsed.get("result")
                && result.as_i64() != Some(1)
            {
                return Err(Fail::Protocol("transfer rejected"));
            }
            return Ok(cookie);
        }
        Err(Fail::Protocol("too many redirects"))
    }
}

fn url_origin(url: &str) -> String {
    let rest = url.strip_prefix("https://").unwrap_or(url);
    let end = rest.find('/').unwrap_or(rest.len());
    format!("https://{}", &rest[..end])
}
