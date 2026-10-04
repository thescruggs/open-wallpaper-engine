// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end op tests against scripted fake Steam answers. No network, all
//! ids, names and tokens are obviously fake synthetic values.

use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    time::{Duration, SystemTime},
};

use serde_json::{Value, json};

use crate::{
    app::Worker,
    jwt::fake_jwt,
    previews::PreviewCache,
    store::{
        Credential, CredentialStore, Secret,
        tests::{sample, temp_dir},
    },
    transport::{
        Body, Method, TransportError,
        fake::{Fake, json as jresp, raw},
    },
    util::{form_encode, unix_seconds},
};

const SID: &str = "76561190000000001";
const IMG_HOST: &str = "https://images.steamusercontent.com";

fn future() -> u64 {
    unix_seconds() + 1_000_000
}
fn refresh_token() -> String {
    fake_jwt(future(), SID, &["web", "derive"])
}
fn community_token(tag: &str) -> String {
    // The extra audience makes each fake token distinct and greppable.
    fake_jwt(future(), SID, &["web:community", tag])
}

struct Env {
    root: PathBuf,
}

impl Env {
    fn new(label: &str) -> Self {
        Self {
            root: temp_dir(label),
        }
    }
    fn store(&self) -> CredentialStore {
        CredentialStore::new(self.root.join("state"))
    }
    fn worker<'a>(&self, fake: &'a Fake) -> Worker<'a> {
        Worker {
            transport: fake,
            store: self.store(),
            cache: PreviewCache::new(self.root.join("cache")),
            retry_delay: Duration::ZERO,
        }
    }
    /// A signed-in store holding a usable community token.
    fn sign_in(&self, community: Option<&str>) {
        let mut credential = sample(&refresh_token());
        credential.steam_id = SID.into();
        if let Some(token) = community {
            credential.community_access_token = Secret::new(token);
        }
        self.store().save(&credential).unwrap();
    }
}

fn call(worker: &Worker, op: &str, params: Value) -> Value {
    let line = json!({"version":1,"op":op,"params":params}).to_string();
    serde_json::from_str(&worker.handle(line.as_bytes())).unwrap()
}

fn err_code(value: &Value) -> &str {
    assert_eq!(value["ok"], false, "{value}");
    value["error"].as_str().unwrap()
}

fn query_params() -> Value {
    json!({"search":"","tags":[],"sort":"trend_week","page":1})
}

fn finalize_ok() -> Result<crate::transport::Response, TransportError> {
    jresp(
        200,
        &json!({"steamID":SID,"transfer_info":[
            {"url":"https://steamcommunity.com/login/settoken","params":{"nonce":"n1","auth":"a1"}}]}),
    )
}

fn transfer_ok(token: &str) -> Result<crate::transport::Response, TransportError> {
    let cookie = format!(
        "steamLoginSecure={}; Path=/; Secure",
        form_encode(&format!("{SID}||{token}"))
    );
    raw(200, &[("set-cookie", &cookie)], br#"{"result":1}"#)
}

fn item(id: u64) -> Value {
    json!({"publishedfileid":id.to_string(),"title":"Fake wallpaper","short_description":"short",
        "preview_url":format!("{IMG_HOST}/fake{id}/"),
        "tags":[{"tag":"Scene"},{"tag":"1920 x 1080"}],"creator":"76561190000000009",
        "file_size":"12345","subscriptions":10,"favorited":3,"time_updated":1700000000,
        "vote_data":{"score":0.87}})
}

// ---- envelope ----

#[test]
fn envelope_validation() {
    let env = Env::new("env");
    let fake = Fake::default();
    let w = env.worker(&fake);
    let line = |s: &str| serde_json::from_str::<Value>(&w.handle(s.as_bytes())).unwrap();
    for bad in [
        r#"{"version":1,"op":"auth.status","params":{},"extra":1}"#,
        r#"{"version":2,"op":"auth.status","params":{}}"#,
        r#"{"version":1,"op":"auth.status","params":{"x":1}}"#,
        r#"{"version":1,"op":"nope","params":{}}"#,
        r#"not json"#,
        r#"{"version":1,"op":"details","params":{"id":"0"}}"#,
        r#"{"version":1,"op":"details","params":{"id":"12a"}}"#,
        r#"{"version":1,"op":"details","params":{"id":"123456789012345678901"}}"#,
        r#"{"version":1,"op":"subscribe","params":{"id":""}}"#,
        r#"{"version":1,"op":"query","params":{"search":"","tags":[],"sort":"bogus","page":1}}"#,
        r#"{"version":1,"op":"query","params":{"search":"","tags":[],"sort":"recent","page":0}}"#,
        r#"{"version":1,"op":"query","params":{"search":"","tags":[],"sort":"recent","page":1,"x":1}}"#,
    ] {
        assert_eq!(err_code(&line(bad)), "bad_request", "{bad}");
    }
    let huge = format!(
        r#"{{"version":1,"op":"auth.status","params":{{}},"pad":"{}"}}"#,
        "x".repeat(70 * 1024)
    );
    assert_eq!(err_code(&line(&huge)), "bad_request");
    assert!(
        fake.seen.borrow().is_empty(),
        "validation must not touch the network"
    );
    let ok = line(r#"{"version":1,"op":"auth.status","params":{}}"#);
    assert_eq!(ok["result"]["signed_in"], false);
    assert_eq!(ok["result"]["account_name"], "");
}

// ---- auth.begin / auth.poll ----

#[test]
fn begin_happy_path_and_request_shape() {
    let env = Env::new("begin");
    let fake = Fake::new(vec![jresp(
        200,
        &json!({"response":{"client_id":"1234567890","request_id":"cmVxdWVzdA==",
            "challenge_url":"https://s.example.test/q/1/fake","interval":2.0}}),
    )]);
    let out = call(&env.worker(&fake), "auth.begin", json!({}));
    let r = &out["result"];
    assert_eq!(r["client_id"], "1234567890");
    assert_eq!(r["request_id"], "cmVxdWVzdA==");
    assert_eq!(r["challenge_url"], "https://s.example.test/q/1/fake");
    assert_eq!(r["poll_interval_ms"], 2000);
    assert_eq!(r["expires_in_ms"], 180000);
    let seen = fake.seen.borrow();
    assert_eq!(seen[0].method, Method::Post);
    let Body::Form(form) = &seen[0].body else {
        panic!("form body")
    };
    assert_eq!(form[0].0, "input_json");
    let sent: Value = serde_json::from_str(&form[0].1).unwrap();
    assert_eq!(
        sent["device_details"]["device_friendly_name"],
        "KDE Wallpaper Engine"
    );
    assert_eq!(sent["device_details"]["platform_type"], 2);
}

#[test]
fn begin_incomplete_challenge_is_protocol_error() {
    let env = Env::new("begin-bad");
    let fake = Fake::new(vec![jresp(200, &json!({"response":{"client_id":"1"}}))]);
    assert_eq!(
        err_code(&call(&env.worker(&fake), "auth.begin", json!({}))),
        "protocol"
    );
}

fn poll_params() -> Value {
    json!({"client_id":"111","request_id":"cmVx","challenge_url":"https://s.example.test/q/1/old"})
}

#[test]
fn poll_states() {
    let env = Env::new("poll");
    let fake = Fake::new(vec![
        jresp(200, &json!({"response":{}})),
        jresp(200, &json!({"response":{"had_remote_interaction":true}})),
        jresp(
            200,
            &json!({"response":{"new_client_id":"222","new_challenge_url":"https://s.example.test/q/1/new"}}),
        ),
        jresp(500, &json!({})),
    ]);
    let w = env.worker(&fake);
    let a = call(&w, "auth.poll", poll_params());
    assert_eq!(a["result"]["state"], "awaiting_scan");
    assert_eq!(
        a["result"]["challenge_url"],
        "https://s.example.test/q/1/old"
    );
    assert_eq!(
        call(&w, "auth.poll", poll_params())["result"]["state"],
        "awaiting_confirmation"
    );
    let c = call(&w, "auth.poll", poll_params());
    assert_eq!(c["result"]["state"], "challenge_changed");
    assert_eq!(c["result"]["client_id"], "222");
    assert_eq!(
        c["result"]["challenge_url"],
        "https://s.example.test/q/1/new"
    );
    assert_eq!(
        err_code(&call(&w, "auth.poll", poll_params())),
        "steam_error"
    );
    let seen = fake.seen.borrow();
    let Body::Form(form) = &seen[0].body else {
        panic!()
    };
    assert_eq!(form[0], ("client_id".into(), "111".into()));
    assert_eq!(form[1], ("request_id".into(), "cmVx".into()));
    assert_eq!(form[2], ("format".into(), "json".into()));
    drop(seen);
    let mut bad = poll_params();
    bad["challenge_url"] = json!("http://insecure.example.test/");
    assert_eq!(err_code(&call(&w, "auth.poll", bad)), "bad_request");
}

#[test]
fn poll_expired_via_eresult_header() {
    let env = Env::new("poll-exp");
    let fake = Fake::new(vec![raw(200, &[("x-eresult", "27")], b"{}")]);
    let out = call(&env.worker(&fake), "auth.poll", poll_params());
    assert_eq!(out["result"]["state"], "expired");
}

#[test]
fn poll_success_stores_private_credential_and_leaks_nothing() {
    let env = Env::new("poll-ok");
    let refresh = refresh_token();
    let community = community_token("sentinel-community");
    let fake = Fake::new(vec![
        jresp(
            200,
            &json!({"response":{"refresh_token":refresh,"account_name":"fake_account","access_token":""}}),
        ),
        finalize_ok(),
        transfer_ok(&community),
    ]);
    let w = env.worker(&fake);
    let line = w.handle(
        json!({"version":1,"op":"auth.poll","params":poll_params()})
            .to_string()
            .as_bytes(),
    );
    let out: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(out["result"]["state"], "signed_in");
    assert_eq!(out["result"]["account_name"], "fake_account");
    for secret in [&refresh, &community] {
        assert!(
            !line.contains(secret.as_str()),
            "token leaked into the response"
        );
    }
    let path = env.store().path();
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    assert_eq!(
        fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
        0o700
    );
    let stored = env.store().load().unwrap();
    assert_eq!(stored.steam_id, SID);
    assert_eq!(stored.community_access_token.expose(), community);
    // status is local and token-free
    let status = w.handle(br#"{"version":1,"op":"auth.status","params":{}}"#);
    assert!(status.contains(r#""signed_in":true"#) && !status.contains(&refresh));
    assert_eq!(fake.seen.borrow().len(), 3);
}

#[test]
fn poll_success_with_a_bad_refresh_token_stores_nothing() {
    let env = Env::new("poll-bad");
    let fake = Fake::new(vec![jresp(
        200,
        &json!({"response":{"refresh_token":"not-a-jwt","account_name":"x"}}),
    )]);
    assert_eq!(
        err_code(&call(&env.worker(&fake), "auth.poll", poll_params())),
        "protocol"
    );
    assert!(!env.store().path().exists());
}

#[test]
fn sign_out_removes_the_file() {
    let env = Env::new("signout");
    env.sign_in(None);
    let fake = Fake::default();
    let out = call(&env.worker(&fake), "auth.sign_out", json!({}));
    assert_eq!(out["result"]["signed_in"], false);
    assert!(!env.store().path().exists());
    assert!(fake.seen.borrow().is_empty());
}

// ---- token lifecycle ----

#[test]
fn rederives_on_401_then_succeeds() {
    let env = Env::new("rederive");
    env.sign_in(None);
    let t1 = community_token("first");
    let t2 = community_token("second");
    let fake = Fake::new(vec![
        finalize_ok(),
        transfer_ok(&t1),
        jresp(401, &json!({})),
        finalize_ok(),
        transfer_ok(&t2),
        jresp(200, &json!({"response":{"total":0}})),
    ]);
    let out = call(&env.worker(&fake), "query", query_params());
    assert_eq!(out["ok"], true, "{out}");
    let seen = fake.seen.borrow();
    assert!(
        seen[2]
            .url
            .contains(&format!("access_token={}", form_encode(&t1)))
    );
    assert!(
        seen[5]
            .url
            .contains(&format!("access_token={}", form_encode(&t2)))
    );
    assert_eq!(
        env.store().load().unwrap().community_access_token.expose(),
        t2
    );
}

#[test]
fn two_rejections_sign_out_and_delete_the_file() {
    let env = Env::new("reject2");
    env.sign_in(None);
    let t = community_token("x");
    let fake = Fake::new(vec![
        finalize_ok(),
        transfer_ok(&t),
        jresp(403, &json!({})),
        finalize_ok(),
        transfer_ok(&t),
        jresp(401, &json!({})),
    ]);
    let out = call(&env.worker(&fake), "query", query_params());
    assert_eq!(err_code(&out), "signed_out");
    assert!(!env.store().path().exists());
}

#[test]
fn refused_refresh_token_signs_out() {
    let env = Env::new("refused");
    env.sign_in(None);
    let fake = Fake::new(vec![jresp(200, &json!({"error":"expired"}))]);
    assert_eq!(
        err_code(&call(&env.worker(&fake), "query", query_params())),
        "signed_out"
    );
    assert!(!env.store().path().exists());
}

#[test]
fn expired_refresh_token_signs_out_without_network() {
    let env = Env::new("expired");
    let mut c = sample(&fake_jwt(unix_seconds() - 10, SID, &["web", "derive"]));
    c.steam_id = SID.into();
    env.store().save(&c).unwrap();
    let fake = Fake::default();
    assert_eq!(
        err_code(&call(&env.worker(&fake), "query", query_params())),
        "signed_out"
    );
    assert!(fake.seen.borrow().is_empty());
    assert!(!env.store().path().exists());
}

#[test]
fn offline_during_derivation_keeps_the_credential() {
    let env = Env::new("offline");
    env.sign_in(None);
    let fake = Fake::new(vec![Err(TransportError::Offline)]);
    assert_eq!(
        err_code(&call(&env.worker(&fake), "query", query_params())),
        "offline"
    );
    assert!(env.store().path().exists());
}

#[test]
fn transfer_failure_is_retried_a_bounded_number_of_times() {
    let env = Env::new("transfer-retry");
    env.sign_in(None);
    let t = community_token("x");
    let fake = Fake::new(vec![
        finalize_ok(),
        raw(500, &[], b""),
        transfer_ok(&t),
        jresp(200, &json!({"response":{"total":0}})),
    ]);
    assert_eq!(
        call(&env.worker(&fake), "query", query_params())["ok"],
        true
    );
    // exhausted: two 500s then give up with steam_error
    let env = Env::new("transfer-giveup");
    env.sign_in(None);
    let fake = Fake::new(vec![finalize_ok(), raw(500, &[], b""), raw(500, &[], b"")]);
    assert_eq!(
        err_code(&call(&env.worker(&fake), "query", query_params())),
        "steam_error"
    );
    assert_eq!(fake.seen.borrow().len(), 3);
}

#[test]
fn only_the_community_transfer_is_posted() {
    let env = Env::new("transfer-community-only");
    env.sign_in(None);
    let t = community_token("x");
    let fake = Fake::new(vec![
        jresp(
            200,
            &json!({"steamID":SID,"transfer_info":[
                {"url":"https://store.steampowered.com/login/settoken","params":{"nonce":"n0","auth":"a0"}},
                {"url":"https://other.example.test/login/settoken","params":{"nonce":"n2","auth":"a2"}},
                {"url":"https://steamcommunity.com/login/settoken","params":{"nonce":"n1","auth":"a1"}}]}),
        ),
        transfer_ok(&t),
        jresp(200, &json!({"response":{"total":0}})),
    ]);
    assert_eq!(
        call(&env.worker(&fake), "query", query_params())["ok"],
        true
    );
    // finalize, one transfer, the query: the other sites were never contacted.
    assert_eq!(fake.seen.borrow().len(), 3);
}

#[test]
fn derivation_does_not_resurrect_a_credential_removed_meanwhile() {
    let env = Env::new("derive-after-signout");
    env.sign_in(None);
    let t = community_token("x");
    let store = env.store();
    let fake = Fake::new(vec![finalize_ok(), transfer_ok(&t)]);
    fake.before_send(1, Box::new(move || store.remove()));
    assert_eq!(
        err_code(&call(&env.worker(&fake), "query", query_params())),
        "signed_out"
    );
    assert!(!env.store().path().exists());
}

#[test]
fn transfer_to_a_foreign_host_is_refused() {
    let env = Env::new("foreign");
    env.sign_in(None);
    let fake = Fake::new(vec![jresp(
        200,
        &json!({"steamID":SID,"transfer_info":[{"url":"https://evil.example.test/x","params":{}}]}),
    )]);
    assert_eq!(
        err_code(&call(&env.worker(&fake), "query", query_params())),
        "protocol"
    );
    assert_eq!(fake.seen.borrow().len(), 1);
}

// ---- query ----

#[test]
fn query_normal_page_maps_items_and_builds_params() {
    let env = Env::new("q-ok");
    env.sign_in(Some(&community_token("c")));
    let fake = Fake::new(vec![jresp(
        200,
        &json!({"response":{"total":95,"publishedfiledetails":[item(1001), item(1002)]}}),
    )]);
    let out = call(
        &env.worker(&fake),
        "query",
        json!({"search":"fake search","tags":["Scene","Anime"],"sort":"trend_month","page":2}),
    );
    let r = &out["result"];
    assert_eq!(r["page"], 2);
    assert_eq!(r["total"], 95);
    assert_eq!(r["has_more"], true);
    let first = &r["items"][0];
    assert_eq!(first["id"], "1001");
    assert_eq!(first["kind"], "scene");
    assert_eq!(first["creator_id"], "76561190000000009");
    assert_eq!(first["file_size"], 12345);
    assert_eq!(first["subscriptions"], 10);
    assert_eq!(first["favorited"], 3);
    assert_eq!(first["preview_file"], "");
    assert_eq!(first["description"], "");
    assert_eq!(first["tags"], json!(["Scene", "1920 x 1080"]));
    let url = fake.seen.borrow()[0].url.clone();
    for needle in [
        "appid=431960",
        "query_type=3",
        "days=30",
        "page=2",
        "numperpage=30",
        "search_text=fake%20search",
        "requiredtags%5B0%5D=Anime",
        "requiredtags%5B1%5D=Everyone",
        "excludedtags%5B0%5D=Application",
        "return_tags=true",
        "return_previews=true",
        "return_vote_data=true",
        "return_short_description=true",
        "format=json",
        "access_token=",
    ] {
        assert!(url.contains(needle), "missing {needle} in {url}");
    }
    assert!(url.contains("excludedtags%5B") && url.contains("=Video") && url.contains("=Web"));
    assert!(!url.contains("=Mature") || url.contains("excludedtags"));
}

#[test]
fn query_empty_and_last_page() {
    let env = Env::new("q-empty");
    env.sign_in(Some(&community_token("c")));
    let fake = Fake::new(vec![
        jresp(200, &json!({"response":{"total":0}})),
        jresp(
            200,
            &json!({"response":{"total":31,"publishedfiledetails":[item(5)]}}),
        ),
    ]);
    let w = env.worker(&fake);
    let empty = call(&w, "query", query_params());
    assert_eq!(empty["result"]["items"], json!([]));
    assert_eq!(empty["result"]["has_more"], false);
    let mut p = query_params();
    p["page"] = json!(2);
    let last = call(&w, "query", p);
    assert_eq!(last["result"]["has_more"], false);
}

#[test]
fn query_failure_mapping() {
    let env = Env::new("q-fail");
    env.sign_in(Some(&community_token("c")));
    let fake = Fake::new(vec![
        raw(200, &[], b"{not json"),
        jresp(200, &json!({"nothing":1})),
        jresp(429, &json!({})),
        jresp(500, &json!({})),
        Err(TransportError::TooLarge),
        Err(TransportError::Timeout),
        Err(TransportError::Offline),
    ]);
    let w = env.worker(&fake);
    let codes: Vec<String> = (0..7)
        .map(|_| err_code(&call(&w, "query", query_params())).to_string())
        .collect();
    assert_eq!(
        codes,
        [
            "protocol",
            "protocol",
            "rate_limited",
            "steam_error",
            "protocol",
            "timeout",
            "offline"
        ]
    );
}

#[test]
fn query_caps_strings_and_skips_bad_items() {
    let env = Env::new("q-caps");
    env.sign_in(Some(&community_token("c")));
    let mut big = item(77);
    big["title"] = json!("t".repeat(1000));
    big["short_description"] = json!("s".repeat(5000));
    big["tags"] = json!(
        (0..100)
            .map(|i| json!({"tag": format!("{i}{}", "x".repeat(200))}))
            .collect::<Vec<_>>()
    );
    big["preview_url"] = json!("http://images.steamusercontent.com/insecure");
    let bad = json!({"publishedfileid":"0","title":"zero"});
    let fake = Fake::new(vec![jresp(
        200,
        &json!({"response":{"total":2,"publishedfiledetails":[big, bad]}}),
    )]);
    let out = call(&env.worker(&fake), "query", query_params());
    let items = out["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"].as_str().unwrap().len(), 256);
    assert_eq!(items[0]["short_description"].as_str().unwrap().len(), 1024);
    assert_eq!(items[0]["tags"].as_array().unwrap().len(), 32);
    assert!(items[0]["tags"][0].as_str().unwrap().chars().count() <= 64);
    assert_eq!(items[0]["preview_url"], "");
}

#[test]
fn query_fills_preview_file_for_cached_items_without_fetching() {
    let env = Env::new("q-cached");
    env.sign_in(Some(&community_token("c")));
    let fake = Fake::new(vec![
        raw(200, &[("content-type", "image/png")], b"\x89PNGfake"),
        jresp(
            200,
            &json!({"response":{"total":1,"publishedfiledetails":[item(42)]}}),
        ),
    ]);
    let w = env.worker(&fake);
    let url = format!("{IMG_HOST}/fake42/");
    let p = call(&w, "previews", json!({"items":[{"id":"42","url":url}]}));
    let path = p["result"]["files"]["42"].as_str().unwrap().to_string();
    let out = call(&w, "query", query_params());
    assert_eq!(out["result"]["items"][0]["preview_file"], path.as_str());
    assert_eq!(
        fake.seen.borrow().len(),
        2,
        "query made exactly its own call"
    );
}

#[test]
fn details_fills_description() {
    let env = Env::new("details");
    let mut it = item(9);
    it["description"] = json!("long fake description");
    it["result"] = json!(1);
    let fake = Fake::new(vec![
        jresp(
            200,
            &json!({"response":{"result":1,"publishedfiledetails":[it]}}),
        ),
        jresp(
            200,
            &json!({"response":{"result":1,"publishedfiledetails":[{"publishedfileid":"9","result":9}]}}),
        ),
    ]);
    let w = env.worker(&fake);
    let out = call(&w, "details", json!({"id":"9"}));
    assert_eq!(out["result"]["description"], "long fake description");
    let seen = fake.seen.borrow();
    let Body::Form(form) = &seen[0].body else {
        panic!()
    };
    assert_eq!(form[0], ("itemcount".into(), "1".into()));
    assert_eq!(form[1], ("publishedfileids[0]".into(), "9".into()));
    drop(seen);
    assert_eq!(
        err_code(&call(&w, "details", json!({"id":"9"}))),
        "protocol"
    );
}

// ---- subscribe ----

#[test]
fn subscribe_and_unsubscribe_send_the_upstream_form() {
    let env = Env::new("sub");
    let token = community_token("c");
    env.sign_in(Some(&token));
    let fake = Fake::new(vec![
        jresp(200, &json!({"response":{}})),
        jresp(200, &json!({"response":{}})),
    ]);
    let w = env.worker(&fake);
    assert_eq!(
        call(&w, "subscribe", json!({"id":"555"}))["result"]["accepted"],
        true
    );
    assert_eq!(
        call(&w, "unsubscribe", json!({"id":"555"}))["result"]["accepted"],
        true
    );
    let seen = fake.seen.borrow();
    assert!(
        seen[0]
            .url
            .ends_with("/IPublishedFileService/Subscribe/v1/")
    );
    assert!(
        seen[1]
            .url
            .ends_with("/IPublishedFileService/Unsubscribe/v1/")
    );
    for request in seen.iter() {
        assert_eq!(request.method, Method::Post);
        assert_eq!(
            request.body,
            Body::Form(vec![
                ("access_token".into(), token.clone()),
                ("publishedfileid".into(), "555".into()),
                ("list_type".into(), "1".into()),
                ("notify_client".into(), "1".into()),
            ])
        );
    }
}

#[test]
fn subscribe_without_credential_is_signed_out() {
    let env = Env::new("sub-out");
    let fake = Fake::default();
    assert_eq!(
        err_code(&call(&env.worker(&fake), "subscribe", json!({"id":"5"}))),
        "signed_out"
    );
}

// ---- previews ----

fn png_ok(body: &[u8]) -> Result<crate::transport::Response, TransportError> {
    raw(200, &[("content-type", "image/png")], body)
}

fn preview_call(w: &Worker, items: &[(&str, &str)]) -> Value {
    let list: Vec<Value> = items
        .iter()
        .map(|(id, url)| json!({"id":id,"url":url}))
        .collect();
    call(w, "previews", json!({"items": list}))
}

#[test]
fn previews_refuse_bad_urls_hosts_types_and_sizes() {
    let env = Env::new("pv-refuse");
    let fake = Fake::new(vec![
        raw(200, &[("content-type", "text/html")], b"<html>"),
        png_ok(&[0u8; 100]),
        Err(TransportError::TooLarge),
    ]);
    let mut w = env.worker(&fake);
    w.cache.file_cap = 50;
    let out = preview_call(
        &w,
        &[
            ("1", "http://images.steamusercontent.com/a"),
            ("2", "https://evil.example.test/a"),
            ("3", "https://steamcdn-a.akamaihd.net.evil.test/a"),
            ("4", "https://other.akamaihd.net/a"),
            ("5", "https://u@images.steamusercontent.com/a"),
            ("6", "https://images.steamusercontent.com/html"),
            ("7", "https://images.steamusercontent.com/big"),
            ("8", "https://images.steamusercontent.com/huge"),
        ],
    );
    assert_eq!(out["ok"], true);
    assert_eq!(out["result"]["files"], json!({}));
    assert_eq!(
        fake.seen.borrow().len(),
        3,
        "only the three allowed hosts were fetched"
    );
    let cache = env.root.join("cache");
    let leftovers = fs::read_dir(&cache).map(|d| d.count()).unwrap_or(0);
    assert_eq!(leftovers, 0, "no partial files");
}

#[test]
fn previews_store_under_derived_name_and_serve_from_cache() {
    let env = Env::new("pv-ok");
    let url = format!("{IMG_HOST}/fake/1");
    let fake = Fake::new(vec![png_ok(b"\x89PNGfake")]);
    let w = env.worker(&fake);
    let out = preview_call(&w, &[("123", &url)]);
    let path = PathBuf::from(out["result"]["files"]["123"].as_str().unwrap());
    assert!(path.is_absolute());
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    use sha2::{Digest, Sha256};
    let hash = hex::encode(Sha256::digest(url.as_bytes()));
    assert_eq!(name, format!("123-{}.png", &hash[..16]));
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);

    // Second call: served from cache, mtime refreshed, no request.
    let old = SystemTime::now() - Duration::from_secs(10_000);
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let again = preview_call(&w, &[("123", &url)]);
    assert_eq!(again["result"]["files"]["123"], path.to_str().unwrap());
    assert_eq!(fake.seen.borrow().len(), 1);
    let age = SystemTime::now()
        .duration_since(fs::metadata(&path).unwrap().modified().unwrap())
        .unwrap();
    assert!(age < Duration::from_secs(60), "mtime was not touched");
}

#[test]
fn previews_evict_least_recently_modified_down_to_the_cap() {
    let env = Env::new("pv-evict");
    let fake = Fake::default();
    let mut w = env.worker(&fake);
    w.cache.total_cap = 250;
    let dir = env.root.join("cache");
    fs::create_dir_all(&dir).unwrap();
    let now = SystemTime::now();
    for (i, name) in ["a.png", "b.png", "c.png", "d.png"].iter().enumerate() {
        let path = dir.join(name);
        fs::write(&path, vec![0u8; 100]).unwrap();
        let age = Duration::from_secs(1000 - (i as u64) * 100);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(now - age)
            .unwrap();
    }
    assert_eq!(w.cache.evict(), 2);
    let mut left: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["c.png", "d.png"], "oldest two (a, b) went first");
}

#[test]
fn previews_ask_for_a_thumbnail_and_accept_images_served_as_octet_stream() {
    let env = Env::new("pv-thumb");
    let url = format!("{IMG_HOST}/fake/2");
    let fake = Fake::new(vec![raw(
        200,
        &[("content-type", "application/octet-stream")],
        b"\xFF\xD8\xFFfake",
    )]);
    let out = preview_call(&env.worker(&fake), &[("7", &url)]);
    let path = out["result"]["files"]["7"].as_str().unwrap();
    assert!(path.ends_with(".jpg"));
    let seen = fake.seen.borrow();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].url.starts_with(&format!("{url}?imw=512&imh=512&")));
}

#[test]
fn previews_fall_back_to_the_original_when_the_host_will_not_resize() {
    let env = Env::new("pv-thumb-fallback");
    let url = format!("{IMG_HOST}/fake/3");
    let fake = Fake::new(vec![raw(404, &[], b""), png_ok(b"\x89PNGok")]);
    let out = preview_call(&env.worker(&fake), &[("8", &url)]);
    assert!(out["result"]["files"].get("8").is_some());
    let seen = fake.seen.borrow();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].url, url);
}

#[test]
fn previews_refuse_non_image_bytes_whatever_the_header_says() {
    let env = Env::new("pv-sniff");
    let url = format!("{IMG_HOST}/fake/4");
    let fake = Fake::new(vec![raw(200, &[("content-type", "image/png")], b"<html>")]);
    let out = preview_call(&env.worker(&fake), &[("9", &url)]);
    assert_eq!(out["result"]["files"], json!({}));
}

#[test]
fn previews_follow_redirects_only_to_allowed_hosts() {
    let env = Env::new("pv-redir");
    let fake = Fake::new(vec![
        raw(302, &[("location", "https://evil.example.test/x")], b""),
        raw(
            302,
            &[("location", "https://cdn.akamai.steamstatic.com/y.png")],
            b"",
        ),
        png_ok(b"\x89PNGok"),
    ]);
    let w = env.worker(&fake);
    let out = preview_call(
        &w,
        &[
            ("1", "https://images.steamusercontent.com/a"),
            ("2", "https://images.steamusercontent.com/b"),
        ],
    );
    assert!(out["result"]["files"].get("1").is_none());
    assert!(out["result"]["files"].get("2").is_some());
    assert_eq!(fake.seen.borrow().len(), 3);
}

#[test]
fn previews_reject_more_than_sixty_items() {
    let env = Env::new("pv-many");
    let fake = Fake::default();
    let items: Vec<Value> = (1..=61)
        .map(|i| json!({"id":i.to_string(),"url":"https://x"}))
        .collect();
    let out = call(&env.worker(&fake), "previews", json!({"items": items}));
    assert_eq!(err_code(&out), "bad_request");
}

// ---- secrets never reach responses ----

#[test]
fn no_response_contains_a_token() {
    let env = Env::new("leak");
    let refresh = refresh_token();
    let c = community_token("leakcheck");
    let mut cred = Credential { ..sample(&refresh) };
    cred.steam_id = SID.into();
    cred.community_access_token = Secret::new(c.as_str());
    env.store().save(&cred).unwrap();
    let fake = Fake::new(vec![
        jresp(
            200,
            &json!({"response":{"total":1,"publishedfiledetails":[item(3)]}}),
        ),
        jresp(401, &json!({})),
        finalize_ok(),
        jresp(401, &json!({})),
    ]);
    let w = env.worker(&fake);
    let mut lines = Vec::new();
    for (op, params) in [
        ("auth.status", json!({})),
        ("query", query_params()),
        ("subscribe", json!({"id":"3"})),
    ] {
        lines.push(
            w.handle(
                json!({"version":1,"op":op,"params":params})
                    .to_string()
                    .as_bytes(),
            ),
        );
    }
    for line in &lines {
        assert!(!line.contains(&refresh) && !line.contains(&c), "{line}");
        assert!(!line.contains("access_token"), "{line}");
    }
}
