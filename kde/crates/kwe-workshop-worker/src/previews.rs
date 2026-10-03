// SPDX-License-Identifier: GPL-3.0-or-later
//! Preview image cache: https-only, Steam content hosts only, `image/*`
//! only, size-capped while streaming, LRU-evicted by modification time.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{
    envelope::{Fail, valid_id},
    store::ensure_private_dir,
    transport::{Request, Transport},
    util::{https_host, log, unix_nanos},
};

pub const FILE_CAP: usize = 4 * 1024 * 1024;
pub const TOTAL_CAP: u64 = 256 * 1024 * 1024;
pub const OP_BUDGET: Duration = Duration::from_secs(30);
pub const MAX_ITEMS: usize = 60;
const MAX_REDIRECTS: usize = 3;
const EXTENSIONS: [&str; 5] = ["jpg", "png", "gif", "webp", "bmp"];

pub struct PreviewCache {
    pub dir: PathBuf,
    pub total_cap: u64,
    pub file_cap: usize,
    pub budget: Duration,
}

/// Steam content hosts. `akamaihd.net` is shared by many tenants, so only
/// labels starting with `steam` are accepted there.
pub fn host_allowed(host: &str) -> bool {
    let under = |root: &str| host == root || host.ends_with(&format!(".{root}"));
    under("steamusercontent.com")
        || under("steamstatic.com")
        || (host.ends_with(".akamaihd.net") && host.starts_with("steam"))
}

fn extension_for(content_type: &str) -> Option<&'static str> {
    let mime = content_type.split(';').next()?.trim().to_ascii_lowercase();
    match mime.as_str() {
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/bmp" => Some("bmp"),
        _ => None,
    }
}

fn stem(id: &str, url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    format!("{id}-{}", &hex::encode(digest)[..16])
}

impl PreviewCache {
    pub fn new(dir: PathBuf) -> Self {
        let dir = std::path::absolute(&dir).unwrap_or(dir);
        Self {
            dir,
            total_cap: TOTAL_CAP,
            file_cap: FILE_CAP,
            budget: OP_BUDGET,
        }
    }

    /// The cached file for this preview, touching its mtime (LRU).
    pub fn lookup(&self, id: &str, url: &str) -> Option<PathBuf> {
        if !valid_id(id) || url.is_empty() {
            return None;
        }
        let stem = stem(id, url);
        for ext in EXTENSIONS {
            let path = self.dir.join(format!("{stem}.{ext}"));
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            touch(&path);
            return Some(path);
        }
        None
    }

    fn validate(url: &str) -> Result<(), &'static str> {
        let host = https_host(url).ok_or("not_https")?;
        host_allowed(&host).then_some(()).ok_or("host_not_allowed")
    }

    /// Fetches one preview; Err carries a short reason word for the log.
    fn fetch(
        &self,
        transport: &dyn Transport,
        id: &str,
        url: &str,
        remaining: Duration,
    ) -> Result<PathBuf, &'static str> {
        let deadline = Instant::now() + remaining;
        let mut current = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            Self::validate(&current)?;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("budget");
            }
            let mut request = Request::get(current.clone());
            request.max_body = self.file_cap;
            request.timeout = left.min(crate::transport::CALL_TIMEOUT);
            let response = transport.send(&request).map_err(|_| "transport")?;
            if (300..400).contains(&response.status) {
                let location = response.header("location").ok_or("redirect")?;
                current = location.to_string();
                continue;
            }
            if !response.ok() {
                return Err("status");
            }
            let ext = extension_for(response.header("content-type").unwrap_or_default())
                .ok_or("not_image")?;
            if response.body.is_empty() || response.body.len() > self.file_cap {
                return Err("size");
            }
            return self
                .write(&stem(id, url), ext, &response.body)
                .map_err(|_| "write");
        }
        Err("redirects")
    }

    fn write(&self, stem: &str, ext: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
        ensure_private_dir(&self.dir)?;
        let target = self.dir.join(format!("{stem}.{ext}"));
        let temporary = self.dir.join(format!(
            ".{stem}.tmp-{}-{}",
            std::process::id(),
            unix_nanos()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temporary)?;
        let done = file.write_all(bytes).and_then(|()| file.sync_all());
        if let Err(error) = done.and_then(|()| fs::rename(&temporary, &target)) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(target)
    }

    /// Deletes least-recently-modified regular files until the directory
    /// total is within the cap. Returns how many files were removed.
    pub fn evict(&self) -> usize {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let meta = fs::symlink_metadata(entry.path()).ok()?;
                meta.file_type().is_file().then(|| {
                    (
                        meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                        meta.len(),
                        entry.path(),
                    )
                })
            })
            .take(50_000)
            .collect();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));
        let mut removed = 0;
        for (_, size, path) in files {
            if total <= self.total_cap {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
                removed += 1;
            }
        }
        removed
    }

    /// The `previews` op. Failures of single images are skipped.
    pub fn run(
        &self,
        transport: &dyn Transport,
        items: &[(String, String)],
    ) -> Result<Value, Fail> {
        if items.len() > MAX_ITEMS {
            return Err(Fail::BadRequest("at most 60 items".into()));
        }
        let start = Instant::now();
        let mut files = Map::new();
        let mut fetched = false;
        for (id, url) in items {
            if !valid_id(id) {
                return Err(Fail::BadRequest("invalid id".into()));
            }
            if let Some(path) = self.lookup(id, url) {
                files.insert(
                    id.clone(),
                    Value::String(path.to_string_lossy().into_owned()),
                );
                continue;
            }
            let remaining = self.budget.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                log("previews.budget", "stopping early");
                break;
            }
            match self.fetch(transport, id, url, remaining) {
                Ok(path) => {
                    fetched = true;
                    files.insert(
                        id.clone(),
                        Value::String(path.to_string_lossy().into_owned()),
                    );
                }
                Err(reason) => log("previews.skipped", &format!("reason={reason}")),
            }
        }
        if fetched {
            self.evict();
        }
        Ok(serde_json::json!({ "files": files }))
    }
}

fn touch(path: &Path) {
    if let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        let _ = file.set_modified(SystemTime::now());
    }
}
