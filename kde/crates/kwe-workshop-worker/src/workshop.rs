// SPDX-License-Identifier: GPL-3.0-or-later
// Borrowed-From: waywallen/open-wallpaper-engine@b8180ad:waywallen/plugins/org.waywallen.open-wallpaper-engine/wallpaper_engine/api.lua (GPL-2.0; protocol reference — endpoints, field names and state machine; independently implemented)
// Borrowed-From: waywallen/open-wallpaper-engine@b8180ad:waywallen/plugins/org.waywallen.open-wallpaper-engine/wallpaper_engine/map.lua (GPL-2.0; protocol reference — endpoints, field names and state machine; independently implemented)
// Borrowed-From: waywallen/open-wallpaper-engine@b8180ad:waywallen/plugins/org.waywallen.open-wallpaper-engine/wallpaper_engine/subscription.lua (GPL-2.0; protocol reference — endpoints, field names and state machine; independently implemented)
//! `query`, `details`, `subscribe` / `unsubscribe` and the item mapping.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    envelope::Fail,
    previews::PreviewCache,
    session::Session,
    transport::{Body, Request, Transport, check_status},
    util::truncate_chars,
};

const APPID: &str = "431960";
const QUERY_FILES: &str = "https://api.steampowered.com/IPublishedFileService/QueryFiles/v1/";
const FILE_DETAILS: &str =
    "https://api.steampowered.com/ISteamRemoteStorage/GetPublishedFileDetails/v1/";
const SUBSCRIBE: &str = "https://api.steampowered.com/IPublishedFileService/Subscribe/v1/";
const UNSUBSCRIBE: &str = "https://api.steampowered.com/IPublishedFileService/Unsubscribe/v1/";
pub const PER_PAGE: u64 = 30;
const MAX_PAGE: u64 = 100_000;
const MAX_SEARCH_CHARS: usize = 256;
const MAX_TAGS_IN: usize = 16;

const TITLE_MAX: usize = 256;
const SHORT_MAX: usize = 1024;
const DESCRIPTION_MAX: usize = 16384;
const TAG_MAX: usize = 64;
const TAGS_MAX: usize = 32;
const KINDS: [&str; 4] = ["scene", "video", "web", "application"];
const TYPE_TAGS: [&str; 3] = ["Scene", "Video", "Web"];
const RATING_TAGS: [&str; 3] = ["Everyone", "Questionable", "Mature"];

/// sort key -> (EPublishedFileQueryType, trend window in days).
fn sort_params(sort: &str) -> Option<(u32, Option<u32>)> {
    Some(match sort {
        "trend_day" => (3, Some(1)),
        "trend_week" => (3, Some(7)),
        "trend_month" => (3, Some(30)),
        "trend_year" => (3, Some(365)),
        "recent" => (1, None),
        "most_subscribed" => (9, None),
        "top_rated" => (0, None),
        _ => return None,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryParams {
    search: String,
    tags: Vec<String>,
    sort: String,
    page: u64,
}

impl QueryParams {
    fn check(&self) -> Result<(u32, Option<u32>), Fail> {
        let bad = |d: &str| Err(Fail::BadRequest(d.into()));
        if self.page == 0 || self.page > MAX_PAGE {
            return bad("page must be 1..=100000");
        }
        if self.search.chars().count() > MAX_SEARCH_CHARS {
            return bad("search too long");
        }
        if self.tags.len() > MAX_TAGS_IN
            || self
                .tags
                .iter()
                .any(|t| t.is_empty() || t.chars().count() > TAG_MAX)
        {
            return bad("invalid tags");
        }
        sort_params(&self.sort).ok_or_else(|| Fail::BadRequest("unknown sort".into()))
    }
}

/// Builds the QueryFiles parameters (without the access token). Only
/// wallpapers are requested; the content rating defaults to Everyone.
fn query_pairs(params: &QueryParams, query_type: u32, days: Option<u32>) -> Vec<(String, String)> {
    let mut excluded: BTreeSet<&str> = ["Application", "Preset", "Asset", "Asset Pack"].into();
    let mut required: Vec<&str> = Vec::new();
    let mut types = BTreeSet::new();
    let mut ratings = BTreeSet::new();
    for tag in &params.tags {
        if let Some(t) = TYPE_TAGS.iter().find(|t| **t == tag) {
            types.insert(*t);
        } else if let Some(r) = RATING_TAGS.iter().find(|r| **r == tag) {
            ratings.insert(*r);
        } else if !required.contains(&tag.as_str()) {
            required.push(tag);
        }
    }
    if !types.is_empty() {
        excluded.extend(TYPE_TAGS.iter().filter(|t| !types.contains(*t)));
    }
    if ratings.is_empty() {
        ratings.insert("Everyone");
    }
    excluded.extend(RATING_TAGS.iter().filter(|r| !ratings.contains(*r)));
    if ratings.len() == 1 {
        required.extend(ratings.iter());
    }
    let mut pairs: Vec<(String, String)> = vec![
        ("appid".into(), APPID.into()),
        ("query_type".into(), query_type.to_string()),
        ("page".into(), params.page.to_string()),
        ("numperpage".into(), PER_PAGE.to_string()),
        ("search_text".into(), params.search.clone()),
        ("return_tags".into(), "true".into()),
        ("return_previews".into(), "true".into()),
        ("return_vote_data".into(), "true".into()),
        ("return_short_description".into(), "true".into()),
        ("format".into(), "json".into()),
    ];
    if let Some(days) = days {
        pairs.push(("days".into(), days.to_string()));
    }
    for (i, tag) in required.iter().enumerate() {
        pairs.push((format!("requiredtags[{i}]"), (*tag).into()));
    }
    for (i, tag) in excluded.iter().enumerate() {
        pairs.push((format!("excludedtags[{i}]"), (*tag).into()));
    }
    pairs
}

fn as_u64(value: Option<&Value>) -> u64 {
    match value {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn text(item: &Value, key: &str, max: usize) -> String {
    truncate_chars(
        item.get(key).and_then(Value::as_str).unwrap_or_default(),
        max,
    )
}

/// Maps one Steam item. Returns None when it has no usable id.
pub fn map_item(item: &Value, cache: &PreviewCache, with_description: bool) -> Option<Value> {
    let id = match item.get("publishedfileid")? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if !crate::envelope::valid_id(&id) {
        return None;
    }
    let tags: Vec<String> = item
        .get("tags")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|t| match t {
                    Value::Object(o) => o.get("tag").and_then(Value::as_str),
                    Value::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .filter(|t| !t.is_empty())
                .take(TAGS_MAX)
                .map(|t| truncate_chars(t, TAG_MAX))
                .collect()
        })
        .unwrap_or_default();
    let kind = tags
        .iter()
        .map(|t| t.to_ascii_lowercase())
        .find(|t| KINDS.contains(&t.as_str()))
        .unwrap_or_default();
    let preview_url = [
        item.get("preview_url").and_then(Value::as_str),
        item.get("previews")
            .and_then(Value::as_array)
            .and_then(|p| p.first())
            .and_then(|p| p.get("url"))
            .and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .find(|u| !u.is_empty())
    .filter(|u| crate::util::https_host(u).is_some())
    .unwrap_or_default()
    .to_string();
    let preview_file = cache
        .lookup(&id, &preview_url)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let creator = match item.get("creator") {
        Some(Value::String(s)) => truncate_chars(s, 20),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let subscriptions = ["subscriptions", "lifetime_subscriptions"]
        .iter()
        .map(|k| as_u64(item.get(*k)))
        .max()
        .unwrap_or(0);
    let favorited = ["favorited", "lifetime_favorited"]
        .iter()
        .map(|k| as_u64(item.get(*k)))
        .max()
        .unwrap_or(0);
    let score = item
        .get("vote_data")
        .and_then(|v| v.get("score"))
        .and_then(Value::as_f64)
        .filter(|s| s.is_finite())
        .unwrap_or(0.0);
    Some(json!({
        "id": id,
        "title": text(item, "title", TITLE_MAX),
        "short_description": text(item, "short_description", SHORT_MAX),
        "description": if with_description { text(item, "description", DESCRIPTION_MAX) } else { String::new() },
        "preview_url": preview_url,
        "preview_file": preview_file,
        "tags": tags,
        "kind": kind,
        "creator_id": creator,
        "file_size": as_u64(item.get("file_size")),
        "subscriptions": subscriptions,
        "favorited": favorited,
        "score": score,
        "time_updated": as_u64(item.get("time_updated")),
    }))
}

pub fn query(session: &Session, cache: &PreviewCache, params: QueryParams) -> Result<Value, Fail> {
    let (query_type, days) = params.check()?;
    let pairs = query_pairs(&params, query_type, days);
    let response = session.authorized(|token| {
        let mut with_token = pairs.clone();
        with_token.push(("access_token".into(), token.to_string()));
        Request::get_query(QUERY_FILES, &with_token)
    })?;
    check_status(&response)?;
    let body = response.json()?;
    let inner = body
        .get("response")
        .filter(|r| r.is_object())
        .ok_or(Fail::Protocol("missing response object"))?;
    let list = match inner.get("publishedfiledetails") {
        None => &[][..],
        Some(Value::Array(list)) => list.as_slice(),
        Some(_) => return Err(Fail::Protocol("publishedfiledetails is not a list")),
    };
    let items: Vec<Value> = list
        .iter()
        .take(PER_PAGE as usize * 2)
        .filter_map(|item| map_item(item, cache, false))
        .collect();
    let total = as_u64(inner.get("total"));
    let has_more = params.page.saturating_mul(PER_PAGE) < total;
    Ok(json!({"items":items,"page":params.page,"total":total,"has_more":has_more}))
}

pub fn details(transport: &dyn Transport, cache: &PreviewCache, id: &str) -> Result<Value, Fail> {
    let response = transport.send(&Request::post(
        FILE_DETAILS,
        Body::Form(vec![
            ("itemcount".into(), "1".into()),
            ("publishedfileids[0]".into(), id.into()),
        ]),
    ))?;
    check_status(&response)?;
    let body = response.json()?;
    let item = body
        .get("response")
        .and_then(|r| r.get("publishedfiledetails"))
        .and_then(Value::as_array)
        .and_then(|l| l.first())
        .ok_or(Fail::Protocol("missing item details"))?;
    if item.get("result").is_some_and(|r| r.as_i64() != Some(1)) {
        return Err(Fail::Protocol("item not available"));
    }
    let mapped = map_item(item, cache, true).ok_or(Fail::Protocol("unusable item"))?;
    if mapped["id"] != id {
        return Err(Fail::Protocol("details answered for another item"));
    }
    Ok(mapped)
}

pub fn subscribe(session: &Session, id: &str, subscribed: bool) -> Result<Value, Fail> {
    let url = if subscribed { SUBSCRIBE } else { UNSUBSCRIBE };
    let response = session.authorized(|token| {
        Request::post(
            url,
            Body::Form(vec![
                ("access_token".into(), token.to_string()),
                ("publishedfileid".into(), id.to_string()),
                ("list_type".into(), "1".into()),
                ("notify_client".into(), "1".into()),
            ]),
        )
    })?;
    check_status(&response)?;
    Ok(json!({"accepted":true}))
}
