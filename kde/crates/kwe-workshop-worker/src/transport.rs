// SPDX-License-Identifier: GPL-3.0-or-later
//! The only door to the network. Every Steam call is a `Request` handed to a
//! `Transport`; tests script the answers, `main` wires the ureq one.

use std::{io::Read, time::Duration};

use crate::{envelope::Fail, util};

pub const JSON_BODY_CAP: usize = 2 * 1024 * 1024;
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    None,
    Form(Vec<(String, String)>),
    Multipart(Vec<(String, String)>),
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Body,
    pub max_body: usize,
    pub timeout: Duration,
}

impl Request {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            url: url.into(),
            headers: Vec::new(),
            body: Body::None,
            max_body: JSON_BODY_CAP,
            timeout: CALL_TIMEOUT,
        }
    }

    pub fn get_query(base: &str, query: &[(String, String)]) -> Self {
        Self::get(format!("{base}?{}", util::encode_pairs(query)))
    }

    pub fn post(url: impl Into<String>, body: Body) -> Self {
        Self {
            method: Method::Post,
            body,
            ..Self::get(url)
        }
    }

    pub fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_string()));
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct Response {
    pub status: u16,
    /// Lower-cased names; repeated headers appear repeatedly.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn headers_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.headers
            .iter()
            .filter(move |(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self) -> Result<serde_json::Value, Fail> {
        serde_json::from_slice(&self.body).map_err(|_| Fail::Protocol("response is not JSON"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    Offline,
    Timeout,
    TooLarge,
    Other,
}

impl From<TransportError> for Fail {
    fn from(error: TransportError) -> Fail {
        match error {
            TransportError::Offline => Fail::Offline,
            TransportError::Timeout => Fail::Timeout,
            TransportError::TooLarge => Fail::Protocol("response too large"),
            TransportError::Other => Fail::Protocol("transport error"),
        }
    }
}

pub trait Transport {
    fn send(&self, request: &Request) -> Result<Response, TransportError>;
}

/// Maps a non-success status for calls where 401/403 are not special.
pub fn check_status(response: &Response) -> Result<(), Fail> {
    match response.status {
        200..=299 => Ok(()),
        429 => Err(Fail::RateLimited),
        status => Err(Fail::Steam(status)),
    }
}

/// multipart/form-data body with a fresh boundary.
pub fn encode_multipart(fields: &[(String, String)]) -> (String, Vec<u8>) {
    let boundary = format!("----kwe{}", util::random_hex12());
    let mut body = Vec::new();
    for (name, value) in fields {
        let name: String = name
            .chars()
            .filter(|c| !matches!(c, '"' | '\r' | '\n'))
            .collect();
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .https_only(true)
            .user_agent("KDE-Wallpaper-Engine-workshop-worker")
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

fn classify(error: &ureq::Error) -> TransportError {
    match error {
        ureq::Error::Timeout(_) => TransportError::Timeout,
        ureq::Error::HostNotFound
        | ureq::Error::ConnectionFailed
        | ureq::Error::Io(_)
        | ureq::Error::Tls(_)
        | ureq::Error::Rustls(_) => TransportError::Offline,
        _ => TransportError::Other,
    }
}

impl Transport for UreqTransport {
    fn send(&self, request: &Request) -> Result<Response, TransportError> {
        let result = match request.method {
            Method::Get => {
                let mut builder = self.agent.get(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(*name, value);
                }
                builder
                    .config()
                    .timeout_global(Some(request.timeout))
                    .build()
                    .call()
            }
            Method::Post => {
                let mut builder = self.agent.post(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(*name, value);
                }
                let builder = builder
                    .config()
                    .timeout_global(Some(request.timeout))
                    .build();
                match &request.body {
                    Body::None => builder.send_empty(),
                    Body::Form(pairs) => builder
                        .header("Content-Type", "application/x-www-form-urlencoded")
                        .send(util::encode_pairs(pairs).as_bytes()),
                    Body::Multipart(fields) => {
                        let (content_type, bytes) = encode_multipart(fields);
                        builder
                            .header("Content-Type", &content_type)
                            .send(&bytes[..])
                    }
                }
            }
        };
        let mut response = result.map_err(|e| classify(&e))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                Some((
                    name.as_str().to_ascii_lowercase(),
                    value.to_str().ok()?.to_string(),
                ))
            })
            .collect();
        let mut body = Vec::new();
        let limit = request.max_body as u64 + 1;
        response
            .body_mut()
            .as_reader()
            .take(limit)
            .read_to_end(&mut body)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::TimedOut {
                    TransportError::Timeout
                } else {
                    TransportError::Offline
                }
            })?;
        if body.len() > request.max_body {
            return Err(TransportError::TooLarge);
        }
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque};

    type Hook = (usize, Box<dyn FnOnce()>);

    /// Scripted transport: answers are popped in order; every request is
    /// recorded. An empty script answers `Offline`.
    #[derive(Default)]
    pub struct Fake {
        pub script: RefCell<VecDeque<Result<Response, TransportError>>>,
        pub seen: RefCell<Vec<Request>>,
        /// Runs once, just before the request with this index is answered.
        pub hook: RefCell<Option<Hook>>,
    }

    impl Fake {
        pub fn new(script: Vec<Result<Response, TransportError>>) -> Self {
            Self {
                script: RefCell::new(script.into()),
                seen: RefCell::default(),
                hook: RefCell::default(),
            }
        }

        pub fn before_send(&self, index: usize, action: Box<dyn FnOnce()>) {
            *self.hook.borrow_mut() = Some((index, action));
        }
    }

    impl Transport for Fake {
        fn send(&self, request: &Request) -> Result<Response, TransportError> {
            let index = self.seen.borrow().len();
            self.seen.borrow_mut().push(request.clone());
            let due = self
                .hook
                .borrow()
                .as_ref()
                .is_some_and(|(at, _)| *at == index);
            if due && let Some((_, action)) = self.hook.borrow_mut().take() {
                action();
            }
            self.script
                .borrow_mut()
                .pop_front()
                .unwrap_or(Err(TransportError::Offline))
        }
    }

    pub fn json(status: u16, body: &serde_json::Value) -> Result<Response, TransportError> {
        Ok(Response {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string().into_bytes(),
        })
    }

    pub fn raw(
        status: u16,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Response, TransportError> {
        Ok(Response {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_vec(),
        })
    }
}
