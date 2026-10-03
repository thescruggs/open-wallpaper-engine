// SPDX-License-Identifier: GPL-3.0-or-later
//! `workshop.*` RPCs (docs/WORKSHOP_PROTOCOL_V1.md section 2): the daemon
//! side of Steam Workshop browsing. The daemon stays network-free; every
//! operation spawns `kwe-workshop-worker` once (hardened one-shot, modelled
//! on `inspect.rs`), writes one JSON request line to its stdin and reads one
//! JSON response line from its stdout.
//!
//! Also holds `CatalogFreshness`: the stat-only change detector that lets the
//! `catalog` RPC rescan on its own when a Workshop manifest changed.

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::{io::AsRawFd, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    persist::ensure_private_dir,
    supervisor::{
        RendererKind, cleanup_renderer_home, env_allowlist, set_nonblocking, signal_process_group,
    },
};

const WORKER_ENVELOPE_VERSION: u64 = 1;
/// Contract: one response line of at most 4 MiB. Read one byte past it.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Contract: one request line of at most 64 KiB.
const MAX_REQUEST_LINE_BYTES: usize = 64 * 1024;
const DEFAULT_WORKER_TIMEOUT: Duration = Duration::from_secs(40);
const STDERR_TAIL_BYTES: usize = 512;
const STDERR_DRAIN_CHUNKS_PER_TICK: usize = 16;
const MAX_DETAIL_BYTES: usize = 256;
const MAX_ERROR_CODE_BYTES: usize = 64;
/// Longest a pending sign-in is kept, whatever the worker claims.
const MAX_LOGIN_LIFETIME: Duration = Duration::from_secs(3600);

const MAX_SEARCH_BYTES: usize = 256;
const MAX_TAG_BYTES: usize = 64;
const MAX_TAGS: usize = 16;
const MAX_PAGE: u32 = 100_000;
const MAX_PREVIEW_ITEMS: usize = 60;
/// 60 items must still fit the worker's 64 KiB request line.
const MAX_PREVIEW_URL_BYTES: usize = 768;
const SORTS: [&str; 7] = [
    "trend_day",
    "trend_week",
    "trend_month",
    "trend_year",
    "recent",
    "most_subscribed",
    "top_rated",
];

// Worker resource ceilings. RLIMIT_AS leaves room for TLS, thread stacks and
// glibc arenas; RLIMIT_FSIZE leaves room for a 4 MiB preview plus its temp
// file; RLIMIT_NPROC is deliberately not set (it counts every process of the
// user, so any value is either useless or breaks the worker); core dumps off.
const WORKER_ADDRESS_SPACE_MIB: u64 = 4096;
const WORKER_FILE_SIZE_MIB: u64 = 16;
const WORKER_OPEN_FILES: u64 = 256;

static LAUNCH_SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub(crate) struct WorkshopConfig {
    pub worker_path: PathBuf,
    /// `<daemon state dir>/steam`, private (0700).
    pub state_dir: PathBuf,
    /// `<daemon state dir>/workshop-previews`, private (0700).
    pub cache_dir: PathBuf,
    /// Parent of the per-launch private HOME dirs.
    pub runtime_dir: PathBuf,
    pub worker_timeout: Duration,
}

impl WorkshopConfig {
    pub fn new(worker_path: PathBuf, daemon_state_dir: &Path, runtime_dir: PathBuf) -> Self {
        Self {
            worker_path,
            state_dir: daemon_state_dir.join("steam"),
            cache_dir: daemon_state_dir.join("workshop-previews"),
            runtime_dir,
            worker_timeout: DEFAULT_WORKER_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkshopError {
    pub code: String,
    pub detail: String,
}

impl WorkshopError {
    fn new(code: &str, detail: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            detail: cap_text(&detail.into(), MAX_DETAIL_BYTES),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({"error": self.code, "detail": self.detail})
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Auth = 0,
    Query = 1,
    Mutation = 2,
    // Thumbnails and the detail sheet must not hold up paging or a new search.
    Details = 3,
    Previews = 4,
}

#[derive(Debug)]
struct PendingLogin {
    client_id: String,
    request_id: String,
    challenge_url: String,
    deadline: Instant,
}

struct Inner {
    config: WorkshopConfig,
    pending: Mutex<Option<PendingLogin>>,
    gates: [AtomicBool; 5],
    /// Shared with `CatalogFreshness`: set after a successful
    /// subscribe/unsubscribe.
    catalog_stale: Arc<AtomicBool>,
}

/// Clears its family gate on drop (panics included).
struct GateGuard {
    inner: Arc<Inner>,
    family: Family,
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.inner.gates[self.family as usize].store(false, Ordering::Release);
    }
}

#[derive(Clone)]
pub(crate) struct WorkshopService {
    inner: Arc<Inner>,
}

/// What the accept thread does with a validated request.
pub(crate) enum Dispatch {
    /// Answer right now on the accept thread (validation error, busy,
    /// `cancel`, expired login, ...).
    Immediate(Value),
    /// Run on a dedicated thread. At most one exists per family.
    Run(Job),
}

enum JobKind {
    AuthStatus,
    AuthBegin,
    AuthPoll(PendingSnapshot),
    AuthSignOut,
    Worker { op: &'static str, params: Value },
}

struct PendingSnapshot {
    client_id: String,
    request_id: String,
    challenge_url: String,
}

pub(crate) struct Job {
    service: WorkshopService,
    guard: GateGuard,
    kind: JobKind,
}

impl Job {
    /// Blocking: spawns the worker and waits (at most the worker timeout).
    /// The family gate is released before the value is returned, so the
    /// caller can never observe the answer while the gate is still held.
    pub fn run(self) -> Value {
        let Job {
            service,
            guard,
            kind,
        } = self;
        let value = service.run_kind(kind);
        drop(guard);
        value
    }
}

impl WorkshopService {
    pub fn new(config: WorkshopConfig, catalog_stale: Arc<AtomicBool>) -> Self {
        for dir in [&config.state_dir, &config.cache_dir] {
            if let Err(error) = ensure_private_dir(dir) {
                eprintln!(
                    "event=workshop.dir_error path={} detail={error}",
                    dir.display()
                );
            }
        }
        Self {
            inner: Arc::new(Inner {
                config,
                pending: Mutex::new(None),
                gates: std::array::from_fn(|_| AtomicBool::new(false)),
                catalog_stale,
            }),
        }
    }

    /// Test helper and simple callers: prepare and run inline.
    #[cfg(test)]
    pub fn call(&self, method: &str, params: &Value) -> Value {
        match self.prepare(method, params) {
            Dispatch::Immediate(value) => value,
            Dispatch::Run(job) => job.run(),
        }
    }

    fn acquire(&self, family: Family) -> Option<GateGuard> {
        if self.inner.gates[family as usize].swap(true, Ordering::AcqRel) {
            None
        } else {
            Some(GateGuard {
                inner: self.inner.clone(),
                family,
            })
        }
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, Option<PendingLogin>> {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Validate, take the family gate, and decide where the work runs.
    pub fn prepare(&self, method: &str, params: &Value) -> Dispatch {
        let busy = || {
            Dispatch::Immediate(json!({
                "error": "busy",
                "detail": "another Workshop operation of this kind is in flight",
            }))
        };
        let invalid = |detail: String| {
            Dispatch::Immediate(json!({"error": "invalid_params", "detail": detail}))
        };
        match method {
            "workshop.auth.status"
            | "workshop.auth.begin"
            | "workshop.auth.poll"
            | "workshop.auth.sign_out" => {
                if let Err(detail) = expect_empty(params) {
                    return invalid(detail);
                }
                let Some(guard) = self.acquire(Family::Auth) else {
                    return busy();
                };
                let kind = match method {
                    "workshop.auth.status" => JobKind::AuthStatus,
                    "workshop.auth.begin" => JobKind::AuthBegin,
                    "workshop.auth.sign_out" => JobKind::AuthSignOut,
                    _ => match self.poll_snapshot() {
                        PollPlan::NoPending => {
                            return Dispatch::Immediate(json!({
                                "error": "no_pending_login",
                                "detail": "no sign-in is in progress",
                            }));
                        }
                        PollPlan::Expired => {
                            return Dispatch::Immediate(json!({
                                "state": "expired",
                                "challenge_url": "",
                                "account_name": "",
                            }));
                        }
                        PollPlan::Poll(snapshot) => JobKind::AuthPoll(snapshot),
                    },
                };
                Dispatch::Run(Job {
                    service: self.clone(),
                    guard,
                    kind,
                })
            }
            "workshop.auth.cancel" => {
                if let Err(detail) = expect_empty(params) {
                    return invalid(detail);
                }
                *self.lock_pending() = None;
                Dispatch::Immediate(json!({}))
            }
            "workshop.query" | "workshop.details" | "workshop.previews" => {
                let validated = match method {
                    "workshop.query" => validate_query(params).map(|p| ("query", Family::Query, p)),
                    "workshop.details" => {
                        validate_id_params(params).map(|p| ("details", Family::Details, p))
                    }
                    _ => validate_previews(params).map(|p| ("previews", Family::Previews, p)),
                };
                match validated {
                    Err(detail) => invalid(detail),
                    Ok((op, family, params)) => match self.acquire(family) {
                        None => busy(),
                        Some(guard) => Dispatch::Run(Job {
                            service: self.clone(),
                            guard,
                            kind: JobKind::Worker { op, params },
                        }),
                    },
                }
            }
            "workshop.subscribe" | "workshop.unsubscribe" => match validate_id_params(params) {
                Err(detail) => invalid(detail),
                Ok(params) => match self.acquire(Family::Mutation) {
                    None => busy(),
                    Some(guard) => Dispatch::Run(Job {
                        service: self.clone(),
                        guard,
                        kind: JobKind::Worker {
                            op: if method == "workshop.subscribe" {
                                "subscribe"
                            } else {
                                "unsubscribe"
                            },
                            params,
                        },
                    }),
                },
            },
            _ => Dispatch::Immediate(json!({"error": "unknown_method", "method": method})),
        }
    }

    fn poll_snapshot(&self) -> PollPlan {
        let mut pending = self.lock_pending();
        match pending.as_ref() {
            None => PollPlan::NoPending,
            Some(login) if Instant::now() >= login.deadline => {
                *pending = None;
                PollPlan::Expired
            }
            Some(login) => PollPlan::Poll(PendingSnapshot {
                client_id: login.client_id.clone(),
                request_id: login.request_id.clone(),
                challenge_url: login.challenge_url.clone(),
            }),
        }
    }

    fn run_kind(&self, kind: JobKind) -> Value {
        match kind {
            JobKind::AuthStatus => self.auth_status(),
            JobKind::AuthBegin => self.auth_begin(),
            JobKind::AuthPoll(snapshot) => self.auth_poll(snapshot),
            JobKind::AuthSignOut => {
                *self.lock_pending() = None;
                let result = self.run_worker("auth.sign_out", json!({}));
                *self.lock_pending() = None;
                into_value(result.map(|_| json!({"signed_in": false})))
            }
            JobKind::Worker { op, params } => {
                let result = self.run_worker(op, params);
                if result.is_ok() && matches!(op, "subscribe" | "unsubscribe") {
                    self.inner.catalog_stale.store(true, Ordering::Release);
                }
                into_value(result)
            }
        }
    }

    fn auth_status(&self) -> Value {
        match self.run_worker("auth.status", json!({})) {
            Ok(result) => json!({
                "worker_available": true,
                "signed_in": result.get("signed_in").and_then(Value::as_bool).unwrap_or(false),
                "account_name": capped_str(&result, "account_name", 256),
            }),
            Err(error) if error.code == "worker_unavailable" => json!({
                "worker_available": false,
                "signed_in": false,
                "account_name": "",
            }),
            Err(error) => error.to_value(),
        }
    }

    fn auth_begin(&self) -> Value {
        // begin replaces any pending login, whatever the worker answers.
        *self.lock_pending() = None;
        let result = match self.run_worker("auth.begin", json!({})) {
            Ok(result) => result,
            Err(error) => return error.to_value(),
        };
        let field = |name: &str| result.get(name).and_then(Value::as_str).map(str::to_owned);
        let (Some(client_id), Some(request_id), Some(challenge_url)) = (
            field("client_id"),
            field("request_id"),
            field("challenge_url"),
        ) else {
            return WorkshopError::new("protocol", "auth.begin result is missing fields")
                .to_value();
        };
        let poll_interval_ms = result.get("poll_interval_ms").and_then(Value::as_u64);
        let expires_in_ms = result.get("expires_in_ms").and_then(Value::as_u64);
        let (Some(poll_interval_ms), Some(expires_in_ms)) = (poll_interval_ms, expires_in_ms)
        else {
            return WorkshopError::new("protocol", "auth.begin result is missing timings")
                .to_value();
        };
        let lifetime = Duration::from_millis(expires_in_ms).min(MAX_LOGIN_LIFETIME);
        *self.lock_pending() = Some(PendingLogin {
            client_id,
            request_id,
            challenge_url: challenge_url.clone(),
            deadline: Instant::now() + lifetime,
        });
        json!({
            "challenge_url": challenge_url,
            "poll_interval_ms": poll_interval_ms,
            "expires_in_ms": expires_in_ms,
        })
    }

    fn auth_poll(&self, snapshot: PendingSnapshot) -> Value {
        let params = json!({
            "client_id": snapshot.client_id,
            "request_id": snapshot.request_id,
            "challenge_url": snapshot.challenge_url,
        });
        let result = match self.run_worker("auth.poll", params) {
            Ok(result) => result,
            Err(error) => return error.to_value(),
        };
        let Some(state) = result.get("state").and_then(Value::as_str) else {
            return WorkshopError::new("protocol", "auth.poll result has no state").to_value();
        };
        let state = state.to_string();
        let account_name = capped_str(&result, "account_name", 256);
        let mut challenge_url = snapshot.challenge_url.clone();
        {
            // Only touch the record this poll was started from; a concurrent
            // begin/cancel/sign_out wins.
            let mut pending = self.lock_pending();
            let still_ours = pending
                .as_ref()
                .is_some_and(|login| login.request_id == snapshot.request_id);
            if still_ours {
                match state.as_str() {
                    "signed_in" | "expired" => *pending = None,
                    "challenge_changed" => {
                        if let Some(login) = pending.as_mut() {
                            if let Some(url) = result.get("challenge_url").and_then(Value::as_str) {
                                login.challenge_url = url.to_string();
                            }
                            if let Some(id) = result.get("client_id").and_then(Value::as_str) {
                                login.client_id = id.to_string();
                            }
                            challenge_url = login.challenge_url.clone();
                        }
                    }
                    _ => {}
                }
            }
        }
        if matches!(state.as_str(), "signed_in" | "expired") {
            challenge_url = String::new();
        }
        json!({
            "state": state,
            "challenge_url": challenge_url,
            "account_name": account_name,
        })
    }

    /// Spawn the worker for one operation and return its `result`.
    pub fn run_worker(&self, op: &str, params: Value) -> Result<Value, WorkshopError> {
        let config = &self.inner.config;
        let request = json!({"version": WORKER_ENVELOPE_VERSION, "op": op, "params": params});
        let mut line = serde_json::to_vec(&request)
            .map_err(|error| WorkshopError::new("worker_failed", error.to_string()))?;
        line.push(b'\n');
        if line.len() > MAX_REQUEST_LINE_BYTES {
            return Err(WorkshopError::new(
                "invalid_params",
                "request exceeds the worker's 64 KiB line",
            ));
        }
        // Never log bodies; auth ops additionally never log stderr.
        let is_auth = op.starts_with("auth.");
        let outcome = spawn_and_collect(config, &line);
        let collected = match outcome {
            Ok(collected) => collected,
            Err(error) => {
                eprintln!("event=workshop.worker_error op={op} code={}", error.code);
                return Err(error);
            }
        };
        let result = parse_envelope(&collected.stdout);
        if let Err(error) = &result {
            if is_auth || collected.stderr_tail.is_empty() {
                eprintln!("event=workshop.worker_error op={op} code={}", error.code);
            } else {
                eprintln!(
                    "event=workshop.worker_error op={op} code={} stderr={}",
                    error.code,
                    String::from_utf8_lossy(&collected.stderr_tail).replace('\n', " ")
                );
            }
        }
        result
    }
}

enum PollPlan {
    NoPending,
    Expired,
    Poll(PendingSnapshot),
}

fn into_value(result: Result<Value, WorkshopError>) -> Value {
    match result {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(other) => json!({"value": other}),
        Err(error) => error.to_value(),
    }
}

fn capped_str(value: &Value, key: &str, max: usize) -> String {
    cap_text(value.get(key).and_then(Value::as_str).unwrap_or(""), max)
}

fn cap_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

// ---------------------------------------------------------------- params

fn expect_empty(params: &Value) -> Result<(), String> {
    match params {
        Value::Null => Ok(()),
        Value::Object(map) if map.is_empty() => Ok(()),
        _ => Err("params must be empty".to_string()),
    }
}

fn check_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 20
        || !id.bytes().all(|byte| byte.is_ascii_digit())
        || id.bytes().all(|byte| byte == b'0')
        || id.starts_with('0')
    {
        return Err("id must be 1-20 ASCII digits and not 0".to_string());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdParams {
    id: String,
}

fn validate_id_params(params: &Value) -> Result<Value, String> {
    let parsed: IdParams = serde_json::from_value(params.clone()).map_err(|e| e.to_string())?;
    check_id(&parsed.id)?;
    Ok(json!({"id": parsed.id}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryParams {
    #[serde(default)]
    search: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default = "default_page")]
    page: u32,
}

fn default_sort() -> String {
    "trend_week".to_string()
}

fn default_page() -> u32 {
    1
}

fn validate_query(params: &Value) -> Result<Value, String> {
    let parsed: QueryParams = serde_json::from_value(params.clone()).map_err(|e| e.to_string())?;
    if parsed.search.len() > MAX_SEARCH_BYTES {
        return Err(format!("search exceeds {MAX_SEARCH_BYTES} bytes"));
    }
    if parsed.tags.len() > MAX_TAGS {
        return Err(format!("at most {MAX_TAGS} tags"));
    }
    if parsed
        .tags
        .iter()
        .any(|tag| tag.is_empty() || tag.len() > MAX_TAG_BYTES)
    {
        return Err(format!("tags must be 1-{MAX_TAG_BYTES} bytes"));
    }
    if !SORTS.contains(&parsed.sort.as_str()) {
        return Err("unknown sort".to_string());
    }
    if parsed.page < 1 || parsed.page > MAX_PAGE {
        return Err(format!("page must be 1-{MAX_PAGE}"));
    }
    Ok(json!({
        "search": parsed.search,
        "tags": parsed.tags,
        "sort": parsed.sort,
        "page": parsed.page,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewsParams {
    items: Vec<PreviewItem>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewItem {
    id: String,
    url: String,
}

fn validate_previews(params: &Value) -> Result<Value, String> {
    let parsed: PreviewsParams =
        serde_json::from_value(params.clone()).map_err(|e| e.to_string())?;
    if parsed.items.len() > MAX_PREVIEW_ITEMS {
        return Err(format!("at most {MAX_PREVIEW_ITEMS} items"));
    }
    let mut items = Vec::with_capacity(parsed.items.len());
    for item in parsed.items {
        check_id(&item.id)?;
        if item.url.is_empty() || item.url.len() > MAX_PREVIEW_URL_BYTES {
            return Err(format!("url must be 1-{MAX_PREVIEW_URL_BYTES} bytes"));
        }
        items.push(json!({"id": item.id, "url": item.url}));
    }
    Ok(json!({"items": items}))
}

// ---------------------------------------------------------------- worker

struct Collected {
    stdout: Vec<u8>,
    stderr_tail: Vec<u8>,
}

fn parse_envelope(stdout: &[u8]) -> Result<Value, WorkshopError> {
    let unparsable = || {
        WorkshopError::new(
            "worker_failed",
            "worker response was not one valid envelope",
        )
    };
    let text = std::str::from_utf8(stdout).map_err(|_| unparsable())?;
    let mut lines = text.split('\n').filter(|line| !line.trim().is_empty());
    let (Some(line), None) = (lines.next(), lines.next()) else {
        return Err(unparsable());
    };
    let envelope: Value = serde_json::from_str(line).map_err(|_| unparsable())?;
    let Value::Object(map) = envelope else {
        return Err(unparsable());
    };
    if map.get("version").and_then(Value::as_u64) != Some(WORKER_ENVELOPE_VERSION) {
        return Err(unparsable());
    }
    match map.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(map
            .get("result")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()))),
        Some(false) => {
            let code = map.get("error").and_then(Value::as_str).unwrap_or("");
            let well_formed = !code.is_empty()
                && code.len() <= MAX_ERROR_CODE_BYTES
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
            let detail = map.get("detail").and_then(Value::as_str).unwrap_or("");
            if well_formed {
                Err(WorkshopError::new(code, detail))
            } else {
                Err(WorkshopError::new(
                    "protocol",
                    "worker error code malformed",
                ))
            }
        }
        None => Err(unparsable()),
    }
}

fn spawn_and_collect(
    config: &WorkshopConfig,
    request_line: &[u8],
) -> Result<Collected, WorkshopError> {
    if !config.worker_path.is_file() {
        return Err(WorkshopError::new(
            "worker_unavailable",
            "kwe-workshop-worker was not found",
        ));
    }
    let serial = LAUNCH_SERIAL.fetch_add(1, Ordering::Relaxed);
    let home_dir = config
        .runtime_dir
        .join(format!("workshop-home-{}-{serial}", std::process::id()));
    ensure_private_dir(&home_dir)
        .map_err(|error| WorkshopError::new("worker_failed", format!("home dir: {error}")))?;
    let result = spawn_in_home(config, request_line, &home_dir);
    cleanup_renderer_home(&home_dir);
    result
}

fn spawn_in_home(
    config: &WorkshopConfig,
    request_line: &[u8],
    home_dir: &Path,
) -> Result<Collected, WorkshopError> {
    let expected_parent = i32::try_from(std::process::id())
        .map_err(|_| WorkshopError::new("worker_failed", "pid overflow"))?;
    let mut command = ProcessCommand::new(&config.worker_path);
    command
        .arg("--state-dir")
        .arg(&config.state_dir)
        .arg("--cache-dir")
        .arg(&config.cache_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        // HOME + PATH only: no session bus, display or runtime dir. DNS and
        // CA certificates come from the system paths, not the environment.
        .envs(env_allowlist(RendererKind::Test, home_dir, false));
    // SAFETY: runs in the child between fork and exec; only
    // async-signal-safe libc calls, no allocation.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != expected_parent {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "daemon exited before worker exec",
                ));
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            set_limit(libc::RLIMIT_AS, WORKER_ADDRESS_SPACE_MIB * 1024 * 1024)?;
            set_limit(libc::RLIMIT_FSIZE, WORKER_FILE_SIZE_MIB * 1024 * 1024)?;
            set_limit(libc::RLIMIT_NOFILE, WORKER_OPEN_FILES)?;
            set_limit(libc::RLIMIT_CORE, 0)?;
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(WorkshopError::new(
                "worker_unavailable",
                "kwe-workshop-worker could not be executed",
            ));
        }
        Err(error) => {
            return Err(WorkshopError::new(
                "worker_failed",
                format!("spawn failed: {error}"),
            ));
        }
    };
    let collected = supervise(&mut child, config.worker_timeout, request_line);
    // Sweep any straggler left in the worker's process group.
    signal_process_group(child.id(), libc::SIGKILL);
    collected
}

fn set_limit(resource: libc::__rlimit_resource_t, value: u64) -> std::io::Result<()> {
    let value = value as libc::rlim_t;
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: `limit` is a valid rlimit and `resource` a libc constant.
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn kill_and_reap(child: &mut Child) {
    signal_process_group(child.id(), libc::SIGKILL);
    let _ = child.wait();
}

fn supervise(
    child: &mut Child,
    wall_timeout: Duration,
    request_line: &[u8],
) -> Result<Collected, WorkshopError> {
    let failed = |child: &mut Child, detail: &str| {
        kill_and_reap(child);
        WorkshopError::new("worker_failed", detail)
    };
    let (Some(mut stdout), Some(mut stderr), Some(mut stdin)) =
        (child.stdout.take(), child.stderr.take(), child.stdin.take())
    else {
        return Err(failed(child, "worker pipes not captured"));
    };
    if set_nonblocking(stdout.as_raw_fd()).is_err() || set_nonblocking(stderr.as_raw_fd()).is_err()
    {
        return Err(failed(child, "failed to set pipes non-blocking"));
    }
    // The request is at most 64 KiB; a dedicated writer keeps a child that
    // never reads stdin from blocking this loop. It ends when the pipe closes
    // (the child exits or is killed).
    let line = request_line.to_vec();
    let writer = std::thread::Builder::new()
        .name("workshop-stdin".to_string())
        .spawn(move || {
            let _ = stdin.write_all(&line);
            // stdin drops here: EOF.
        })
        .map_err(|error| failed(child, &format!("writer thread: {error}")))?;

    let deadline = Instant::now() + wall_timeout;
    let mut out: Vec<u8> = Vec::new();
    let mut err_tail: Vec<u8> = Vec::new();
    let outcome = loop {
        let oversize = drain_stdout(&mut stdout, &mut out);
        drain_stderr_tail(&mut stderr, &mut err_tail);
        if oversize {
            break Err(failed(child, "worker response exceeded 4 MiB"));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let oversize = drain_stdout(&mut stdout, &mut out);
                drain_stderr_tail(&mut stderr, &mut err_tail);
                if oversize {
                    break Err(WorkshopError::new(
                        "worker_failed",
                        "worker response exceeded 4 MiB",
                    ));
                }
                if !status.success() {
                    break Err(WorkshopError::new(
                        "worker_failed",
                        format!("worker exited with {status}"),
                    ));
                }
                break Ok(Collected {
                    stdout: out,
                    stderr_tail: err_tail,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    break Err(failed(child, "worker timed out"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => break Err(failed(child, &format!("wait failed: {error}"))),
        }
    };
    let _ = writer.join();
    outcome
}

fn drain_stdout(pipe: &mut impl Read, buffer: &mut Vec<u8>) -> bool {
    if buffer.len() > MAX_RESPONSE_BYTES {
        return true;
    }
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > MAX_RESPONSE_BYTES {
                    return true;
                }
            }
            Err(_) => break,
        }
    }
    false
}

fn drain_stderr_tail(pipe: &mut impl Read, buffer: &mut Vec<u8>) {
    let mut chunk = [0_u8; 4096];
    for _ in 0..STDERR_DRAIN_CHUNKS_PER_TICK {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > STDERR_TAIL_BYTES {
                    let excess = buffer.len() - STDERR_TAIL_BYTES;
                    buffer.drain(..excess);
                }
            }
            Err(_) => break,
        }
    }
}

// ------------------------------------------------------ catalog freshness

type Stamp = Option<(SystemTime, u64)>;
/// Manifest path -> (mtime, size); absent file = `None`.
pub(crate) type ManifestSignature = BTreeMap<PathBuf, Stamp>;

/// Stat-only detector for Workshop manifest changes (contract section 2).
pub(crate) struct CatalogFreshness {
    window: Duration,
    stale: Arc<AtomicBool>,
    stat_passes: AtomicU64,
    state: Mutex<FreshState>,
}

struct FreshState {
    last_check: Option<Instant>,
    known: ManifestSignature,
}

impl CatalogFreshness {
    pub fn new(window: Duration, stale: Arc<AtomicBool>) -> Self {
        Self {
            window,
            stale,
            stat_passes: AtomicU64::new(0),
            state: Mutex::new(FreshState {
                last_check: None,
                known: BTreeMap::new(),
            }),
        }
    }

    pub fn default_window() -> Duration {
        Duration::from_secs(2)
    }

    #[cfg(test)]
    pub fn stat_passes(&self) -> u64 {
        self.stat_passes.load(Ordering::Relaxed)
    }

    fn signature(&self, libraries: &[PathBuf]) -> ManifestSignature {
        self.stat_passes.fetch_add(1, Ordering::Relaxed);
        let mut signature = BTreeMap::new();
        for library in libraries {
            let steamapps = library.join("steamapps");
            for path in [
                steamapps.join("workshop/appworkshop_431960.acf"),
                steamapps.join("appworkshop_431960.acf"),
            ] {
                let stamp = fs::metadata(&path)
                    .ok()
                    .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
                signature.insert(path, stamp);
            }
        }
        signature
    }

    /// `Some(signature)` when a rescan is due: the stale flag is set, or the
    /// rate window has passed and the manifests differ from the last scan.
    /// The signature was taken BEFORE the rescan; pass it to `commit`.
    pub fn poll(&self, libraries: &[PathBuf]) -> Option<ManifestSignature> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let stale = self.stale.swap(false, Ordering::AcqRel);
        let due = state
            .last_check
            .is_none_or(|last| last.elapsed() >= self.window);
        if !stale && !due {
            return None;
        }
        state.last_check = Some(Instant::now());
        let signature = self.signature(libraries);
        (stale || signature != state.known).then_some(signature)
    }

    pub fn commit(&self, signature: ManifestSignature) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.known = signature;
        state.last_check = Some(Instant::now());
    }

    /// Record the current manifests as the baseline (startup, explicit
    /// `rescan`).
    pub fn record(&self, libraries: &[PathBuf]) {
        let signature = self.signature(libraries);
        self.commit(signature);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kwe-workshop-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_script(root: &Path, name: &str, body: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn service(
        root: &Path,
        worker: PathBuf,
        timeout: Duration,
    ) -> (WorkshopService, Arc<AtomicBool>) {
        let stale = Arc::new(AtomicBool::new(false));
        let mut config = WorkshopConfig::new(worker, &root.join("state"), root.join("runtime"));
        config.worker_timeout = timeout;
        (WorkshopService::new(config, stale.clone()), stale)
    }

    const OK_ENVELOPE: &str = r#"{"version":1,"ok":true,"result":{"accepted":true}}"#;

    fn echo_script(root: &Path, line: &str) -> PathBuf {
        write_script(root, "worker.sh", &format!("cat >/dev/null\necho '{line}'"))
    }

    #[test]
    fn ok_envelope_passes_result_through_and_marks_catalog_stale() {
        let root = temp_dir("ok");
        let (svc, stale) = service(
            &root,
            echo_script(&root, OK_ENVELOPE),
            Duration::from_secs(5),
        );
        assert!(!stale.load(Ordering::Acquire));
        let result = svc.call("workshop.subscribe", &json!({"id": "100"}));
        assert_eq!(result, json!({"accepted": true}));
        assert!(stale.load(Ordering::Acquire));
        // dirs created private
        let mode = fs::metadata(root.join("state/steam"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(root.join("state/workshop-previews").is_dir());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn query_does_not_mark_stale_and_error_envelope_passes_through() {
        let root = temp_dir("err");
        let script = echo_script(
            &root,
            r#"{"version":1,"ok":false,"error":"signed_out","detail":"no credential"}"#,
        );
        let (svc, stale) = service(&root, script, Duration::from_secs(5));
        let result = svc.call("workshop.subscribe", &json!({"id": "100"}));
        assert_eq!(result["error"], "signed_out");
        assert_eq!(result["detail"], "no credential");
        assert!(
            !stale.load(Ordering::Acquire),
            "failed mutation stays clean"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn long_error_detail_is_capped() {
        let root = temp_dir("cap");
        let detail = "x".repeat(5000);
        let script = echo_script(
            &root,
            &format!(r#"{{"version":1,"ok":false,"error":"steam_error","detail":"{detail}"}}"#),
        );
        let (svc, _) = service(&root, script, Duration::from_secs(5));
        let result = svc.call("workshop.details", &json!({"id": "100"}));
        assert_eq!(result["error"], "steam_error");
        assert!(result["detail"].as_str().unwrap().len() <= MAX_DETAIL_BYTES);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn failure_modes_map_to_worker_errors() {
        let root = temp_dir("modes");
        let timeout = Duration::from_millis(400);
        let cases: Vec<(&str, PathBuf, &str)> = vec![
            (
                "sleep",
                write_script(&root, "sleep.sh", "sleep 30"),
                "worker_failed",
            ),
            (
                "oversize",
                write_script(
                    &root,
                    "big.sh",
                    "cat >/dev/null\nhead -c 5000000 /dev/zero | tr '\\0' 'a'",
                ),
                "worker_failed",
            ),
            (
                "exit1",
                write_script(
                    &root,
                    "exit1.sh",
                    &format!("cat >/dev/null\necho '{OK_ENVELOPE}'\nexit 3"),
                ),
                "worker_failed",
            ),
            (
                "garbage",
                echo_script(&root, "not json at all"),
                "worker_failed",
            ),
            (
                "version",
                echo_script(&root, r#"{"version":2,"ok":true,"result":{}}"#),
                "worker_failed",
            ),
            (
                "two lines",
                write_script(
                    &root,
                    "two.sh",
                    &format!("cat >/dev/null\necho '{OK_ENVELOPE}'\necho '{OK_ENVELOPE}'"),
                ),
                "worker_failed",
            ),
            ("missing", root.join("nope"), "worker_unavailable"),
        ];
        for (label, path, expected) in cases {
            let (svc, stale) = service(&root, path, timeout);
            let started = Instant::now();
            let result = svc.call("workshop.subscribe", &json!({"id": "100"}));
            assert_eq!(result["error"], expected, "{label}: {result}");
            assert!(started.elapsed() < Duration::from_secs(5), "{label}");
            assert!(!stale.load(Ordering::Acquire), "{label}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn timeout_kills_the_process_group() {
        let root = temp_dir("pgkill");
        let pidfile = root.join("pid");
        let script = write_script(
            &root,
            "w.sh",
            &format!("sleep 60 &\necho $! > {}\nwait", pidfile.display()),
        );
        let (svc, _) = service(&root, script, Duration::from_millis(500));
        let result = svc.call("workshop.details", &json!({"id": "100"}));
        assert_eq!(result["error"], "worker_failed");
        let pid: i32 = fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // SAFETY: signal 0 only probes existence.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        // A killed-but-unreaped child of init may linger briefly as a zombie;
        // accept either gone or zombie by checking /proc state.
        if alive {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            assert!(stat.contains(") Z"), "grandchild survived: {stat}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn busy_gate_is_per_family() {
        let root = temp_dir("busy");
        let script = write_script(
            &root,
            "w.sh",
            &format!("cat >/dev/null\nsleep 1\necho '{OK_ENVELOPE}'"),
        );
        let (svc, _) = service(&root, script, Duration::from_secs(10));
        let Dispatch::Run(job) = svc.prepare("workshop.details", &json!({"id": "100"})) else {
            panic!("first details call must run");
        };
        let handle = std::thread::spawn(move || job.run());
        let second = svc.call("workshop.details", &json!({"id": "200"}));
        assert_eq!(second["error"], "busy");
        // Paging and thumbnails are not held up by an open detail sheet.
        let query = svc.call("workshop.query", &json!({"search": "x"}));
        assert_ne!(query["error"], "busy");
        let other = svc.call("workshop.auth.status", &json!({}));
        assert_eq!(
            other["worker_available"], true,
            "auth family is independent: {other}"
        );
        let mutation = svc.call("workshop.subscribe", &json!({"id": "100"}));
        assert_eq!(mutation, json!({"accepted": true}));
        handle.join().unwrap();
        // gate released afterwards
        let again = svc.call("workshop.details", &json!({"id": "100"}));
        assert_ne!(again["error"], "busy");
        fs::remove_dir_all(&root).unwrap();
    }

    fn begin_envelope(client: &str, request: &str, expires_ms: u64) -> String {
        format!(
            r#"{{"version":1,"ok":true,"result":{{"client_id":"{client}","request_id":"{request}","challenge_url":"https://example.invalid/c/{client}","poll_interval_ms":2000,"expires_in_ms":{expires_ms}}}}}"#
        )
    }

    /// Fake worker that copies its stdin to `<root>/stdin-<op>` (op is read
    /// from the request line) and answers with the file `<root>/reply-<op>`.
    fn recording_worker(root: &Path) -> PathBuf {
        let script = format!(
            "cat > {r}/stdin.tmp\nop=$(sed -n 's/.*\"op\":\"\\([a-z._]*\\)\".*/\\1/p' {r}/stdin.tmp)\nmv {r}/stdin.tmp {r}/stdin-$op\ncat {r}/reply-$op",
            r = root.display()
        );
        write_script(root, "rec.sh", &script)
    }

    fn set_reply(root: &Path, op: &str, line: &str) {
        fs::write(root.join(format!("reply-{op}")), format!("{line}\n")).unwrap();
    }

    fn recorded(root: &Path, op: &str) -> Value {
        serde_json::from_str(&fs::read_to_string(root.join(format!("stdin-{op}"))).unwrap())
            .unwrap()
    }

    #[test]
    fn pending_login_lifecycle() {
        let root = temp_dir("login");
        let (svc, _) = service(&root, recording_worker(&root), Duration::from_secs(10));

        let none = svc.call("workshop.auth.poll", &json!({}));
        assert_eq!(none["error"], "no_pending_login");

        set_reply(&root, "auth.begin", &begin_envelope("c1", "r1", 600_000));
        let begin = svc.call("workshop.auth.begin", &json!({}));
        assert_eq!(begin["challenge_url"], "https://example.invalid/c/c1");
        assert_eq!(begin["poll_interval_ms"], 2000);
        assert_eq!(begin["expires_in_ms"], 600_000);
        assert!(begin.get("client_id").is_none() && begin.get("request_id").is_none());

        // poll passes the stored ids; rotation updates them
        set_reply(
            &root,
            "auth.poll",
            r#"{"version":1,"ok":true,"result":{"state":"challenge_changed","client_id":"c2","challenge_url":"https://example.invalid/c/c2","account_name":""}}"#,
        );
        let poll = svc.call("workshop.auth.poll", &json!({}));
        assert_eq!(poll["state"], "challenge_changed");
        assert_eq!(poll["challenge_url"], "https://example.invalid/c/c2");
        let sent = recorded(&root, "auth.poll");
        assert_eq!(sent["version"], 1);
        assert_eq!(sent["params"]["client_id"], "c1");
        assert_eq!(sent["params"]["request_id"], "r1");
        assert_eq!(
            sent["params"]["challenge_url"],
            "https://example.invalid/c/c1"
        );

        set_reply(
            &root,
            "auth.poll",
            r#"{"version":1,"ok":true,"result":{"state":"awaiting_scan","client_id":"c2","challenge_url":"https://example.invalid/c/c2","account_name":""}}"#,
        );
        let poll = svc.call("workshop.auth.poll", &json!({}));
        assert_eq!(poll["state"], "awaiting_scan");
        let sent = recorded(&root, "auth.poll");
        assert_eq!(sent["params"]["client_id"], "c2");
        assert_eq!(sent["params"]["request_id"], "r1");
        assert_eq!(
            sent["params"]["challenge_url"],
            "https://example.invalid/c/c2"
        );

        // signed_in clears
        set_reply(
            &root,
            "auth.poll",
            r#"{"version":1,"ok":true,"result":{"state":"signed_in","client_id":"c2","challenge_url":"","account_name":"fake_user"}}"#,
        );
        let poll = svc.call("workshop.auth.poll", &json!({}));
        assert_eq!(poll["state"], "signed_in");
        assert_eq!(poll["account_name"], "fake_user");
        assert_eq!(
            svc.call("workshop.auth.poll", &json!({}))["error"],
            "no_pending_login"
        );

        // cancel and sign_out drop a pending login
        svc.call("workshop.auth.begin", &json!({}));
        assert_eq!(svc.call("workshop.auth.cancel", &json!({})), json!({}));
        assert_eq!(
            svc.call("workshop.auth.poll", &json!({}))["error"],
            "no_pending_login"
        );
        svc.call("workshop.auth.begin", &json!({}));
        set_reply(
            &root,
            "auth.sign_out",
            r#"{"version":1,"ok":true,"result":{"signed_in":false}}"#,
        );
        assert_eq!(
            svc.call("workshop.auth.sign_out", &json!({})),
            json!({"signed_in": false})
        );
        assert_eq!(
            svc.call("workshop.auth.poll", &json!({}))["error"],
            "no_pending_login"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn expired_deadline_answers_expired_without_spawning() {
        let root = temp_dir("expiry");
        let (svc, _) = service(&root, recording_worker(&root), Duration::from_secs(10));
        set_reply(&root, "auth.begin", &begin_envelope("c1", "r1", 50));
        svc.call("workshop.auth.begin", &json!({}));
        std::thread::sleep(Duration::from_millis(120));
        let poll = svc.call("workshop.auth.poll", &json!({}));
        assert_eq!(poll["state"], "expired");
        assert!(!root.join("stdin-auth.poll").exists(), "no worker spawn");
        assert_eq!(
            svc.call("workshop.auth.poll", &json!({}))["error"],
            "no_pending_login"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn auth_status_reports_worker_availability() {
        let root = temp_dir("status");
        let (svc, _) = service(&root, root.join("missing"), Duration::from_secs(2));
        assert_eq!(
            svc.call("workshop.auth.status", &json!({})),
            json!({"worker_available": false, "signed_in": false, "account_name": ""})
        );
        let script = echo_script(
            &root,
            r#"{"version":1,"ok":true,"result":{"signed_in":true,"account_name":"fake_user","steam_id":"1"}}"#,
        );
        let (svc, _) = service(&root, script, Duration::from_secs(5));
        assert_eq!(
            svc.call("workshop.auth.status", &json!({})),
            json!({"worker_available": true, "signed_in": true, "account_name": "fake_user"})
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn invalid_params_are_rejected_without_spawning() {
        let root = temp_dir("validate");
        let marker = root.join("spawned");
        let script = write_script(
            &root,
            "w.sh",
            &format!(
                "touch {}\ncat >/dev/null\necho '{OK_ENVELOPE}'",
                marker.display()
            ),
        );
        let (svc, _) = service(&root, script, Duration::from_secs(5));
        let previews = |n: usize| json!({"items": (0..n).map(|i| json!({"id": format!("{}", i + 1), "url": "https://example.invalid/a.jpg"})).collect::<Vec<_>>()});
        let bad: Vec<(&str, Value)> = vec![
            ("workshop.details", json!({"id": "0"})),
            ("workshop.details", json!({"id": ""})),
            ("workshop.details", json!({"id": "012"})),
            ("workshop.details", json!({"id": "abc"})),
            ("workshop.details", json!({"id": "1".repeat(21)})),
            ("workshop.details", json!({"id": "1", "extra": 1})),
            ("workshop.subscribe", json!({"id": "-1"})),
            ("workshop.unsubscribe", json!({})),
            ("workshop.query", json!({"page": 0})),
            ("workshop.query", json!({"sort": "bogus"})),
            ("workshop.query", json!({"surprise": true})),
            ("workshop.query", json!({"search": "s".repeat(257)})),
            (
                "workshop.query",
                json!({"tags": (0..33).map(|i| format!("t{i}")).collect::<Vec<_>>()}),
            ),
            ("workshop.query", json!({"tags": ["t".repeat(65)]})),
            ("workshop.previews", previews(61)),
            (
                "workshop.previews",
                json!({"items": [{"id": "0", "url": "https://x"}]}),
            ),
            (
                "workshop.previews",
                json!({"items": [{"id": "1", "url": "https://x", "z": 1}]}),
            ),
            ("workshop.auth.begin", json!({"x": 1})),
        ];
        for (method, params) in bad {
            let result = svc.call(method, &params);
            assert_eq!(
                result["error"], "invalid_params",
                "{method} {params}: {result}"
            );
        }
        assert!(
            !marker.exists(),
            "no worker may be spawned for invalid params"
        );
        let good = svc.call("workshop.previews", &previews(60));
        assert!(good.get("error").is_none(), "{good}");
        assert!(marker.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn worker_runs_with_cleared_environment_and_private_home() {
        let root = temp_dir("env");
        let envfile = root.join("env.txt");
        let argsfile = root.join("args.txt");
        let script = write_script(
            &root,
            "w.sh",
            &format!(
                "cat >/dev/null\nenv > {}\necho \"$@\" > {}\necho '{OK_ENVELOPE}'",
                envfile.display(),
                argsfile.display()
            ),
        );
        // SAFETY: test-only marker variable, set before any read of it.
        unsafe { std::env::set_var("KWE_WORKSHOP_TEST_MARKER", "leak-me") };
        let (svc, _) = service(&root, script, Duration::from_secs(5));
        let result = svc.call("workshop.subscribe", &json!({"id": "100"}));
        assert_eq!(result, json!({"accepted": true}));
        let env = fs::read_to_string(&envfile).unwrap();
        assert!(!env.contains("KWE_WORKSHOP_TEST_MARKER"), "{env}");
        assert!(!env.contains("leak-me"), "{env}");
        let real_home = std::env::var("HOME").unwrap_or_default();
        let home_line = env.lines().find(|line| line.starts_with("HOME=")).unwrap();
        assert!(home_line.contains("workshop-home-"), "{home_line}");
        if !real_home.is_empty() {
            assert_ne!(home_line, format!("HOME={real_home}"));
        }
        let args = fs::read_to_string(&argsfile).unwrap();
        assert!(
            args.contains("--state-dir") && args.contains("/state/steam"),
            "{args}"
        );
        assert!(
            args.contains("--cache-dir") && args.contains("/state/workshop-previews"),
            "{args}"
        );
        // HOME dir removed afterwards
        let leftovers = fs::read_dir(root.join("runtime"))
            .unwrap()
            .filter_map(Result::ok)
            .count();
        assert_eq!(leftovers, 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn freshness_window_and_stale_flag() {
        let root = temp_dir("fresh");
        let library = root.join("lib");
        fs::create_dir_all(library.join("steamapps/workshop")).unwrap();
        let stale = Arc::new(AtomicBool::new(false));
        let fresh = CatalogFreshness::new(Duration::from_millis(150), stale.clone());
        let libs = vec![library.clone()];
        fresh.record(&libs);
        let base = fresh.stat_passes();
        // inside the window: no stat at all
        assert!(fresh.poll(&libs).is_none());
        assert_eq!(fresh.stat_passes(), base);
        std::thread::sleep(Duration::from_millis(200));
        // window passed, nothing changed
        assert!(fresh.poll(&libs).is_none());
        assert_eq!(fresh.stat_passes(), base + 1);
        // change appears after the next window
        fs::write(
            library.join("steamapps/workshop/appworkshop_431960.acf"),
            "x",
        )
        .unwrap();
        assert!(fresh.poll(&libs).is_none(), "still inside the window");
        std::thread::sleep(Duration::from_millis(200));
        let signature = fresh.poll(&libs).expect("manifest appeared");
        fresh.commit(signature);
        std::thread::sleep(Duration::from_millis(200));
        assert!(fresh.poll(&libs).is_none());
        // stale flag forces a rescan inside the window
        stale.store(true, Ordering::Release);
        assert!(fresh.poll(&libs).is_some());
        assert!(!stale.load(Ordering::Acquire));
        fs::remove_dir_all(&root).unwrap();
    }
}
