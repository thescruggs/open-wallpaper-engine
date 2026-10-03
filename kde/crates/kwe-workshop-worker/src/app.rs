// SPDX-License-Identifier: GPL-3.0-or-later
//! Op dispatch. `Worker::handle` turns one request line into one response
//! line and never panics across the boundary.

use std::{panic::AssertUnwindSafe, time::Duration};

use serde::Deserialize;
use serde_json::Value;

use crate::{
    auth,
    envelope::{self, Fail, IdParams},
    previews::PreviewCache,
    session::Session,
    store::CredentialStore,
    transport::Transport,
    util::log,
    workshop,
};

pub struct Worker<'a> {
    pub transport: &'a dyn Transport,
    pub store: CredentialStore,
    pub cache: PreviewCache,
    pub retry_delay: Duration,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewItem {
    id: String,
    url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewParams {
    items: Vec<PreviewItem>,
}

impl Worker<'_> {
    pub fn handle(&self, input: &[u8]) -> String {
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| self.dispatch(input)))
            .unwrap_or_else(|_| {
                log("worker.panic", "");
                Err(Fail::Internal("internal error"))
            });
        envelope::render(result)
    }

    fn dispatch(&self, input: &[u8]) -> Result<Value, Fail> {
        let request = envelope::parse_envelope(input)?;
        let params = &request.params;
        let session = Session {
            transport: self.transport,
            store: &self.store,
            retry_delay: self.retry_delay,
        };
        match request.op.as_str() {
            "auth.status" => {
                envelope::parse_params::<Empty>(params)?;
                Ok(auth::status(&self.store))
            }
            "auth.begin" => {
                envelope::parse_params::<Empty>(params)?;
                auth::begin(self.transport)
            }
            "auth.poll" => auth::poll(&session, envelope::parse_params(params)?),
            "auth.sign_out" => {
                envelope::parse_params::<Empty>(params)?;
                Ok(auth::sign_out(&self.store))
            }
            "query" => workshop::query(&session, &self.cache, envelope::parse_params(params)?),
            "details" => {
                let id = envelope::parse_params::<IdParams>(params)?.checked()?;
                workshop::details(self.transport, &self.cache, &id)
            }
            "previews" => {
                let parsed: PreviewParams = envelope::parse_params(params)?;
                if parsed.items.len() > crate::previews::MAX_ITEMS {
                    return Err(Fail::BadRequest("at most 60 items".into()));
                }
                let mut items = Vec::new();
                for item in parsed.items {
                    if !envelope::valid_id(&item.id) || item.url.len() > 2048 {
                        return Err(Fail::BadRequest("invalid preview item".into()));
                    }
                    items.push((item.id, item.url));
                }
                self.cache.run(self.transport, &items)
            }
            "subscribe" | "unsubscribe" => {
                let id = envelope::parse_params::<IdParams>(params)?.checked()?;
                workshop::subscribe(&session, &id, request.op == "subscribe")
            }
            _ => Err(Fail::BadRequest("unknown op".into())),
        }
    }
}
