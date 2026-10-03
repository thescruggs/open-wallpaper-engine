// SPDX-License-Identifier: GPL-3.0-or-later
//! `kwe-workshop-worker --state-dir <dir> --cache-dir <dir>`
//!
//! One-shot: reads one JSON request line from stdin, writes one JSON
//! response line to stdout, exits 0 whenever a response was written. This is
//! the only process that talks to Steam or reads the Steam credential file.
//! Contract: `docs/WORKSHOP_PROTOCOL_V1.md` section 1.

mod app;
mod auth;
mod envelope;
mod jwt;
mod previews;
mod session;
mod store;
#[cfg(test)]
mod tests;
mod transport;
mod util;
mod workshop;

use std::{
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

fn parse_args() -> Option<(PathBuf, PathBuf)> {
    let mut args = std::env::args_os().skip(1);
    let (mut state, mut cache) = (None, None);
    while let Some(flag) = args.next() {
        let value = PathBuf::from(args.next()?);
        match flag.to_str()? {
            "--state-dir" if state.is_none() => state = Some(value),
            "--cache-dir" if cache.is_none() => cache = Some(value),
            _ => return None,
        }
    }
    Some((state?, cache?))
}

fn write_line(line: &str) -> ExitCode {
    let mut out = std::io::stdout().lock();
    match out
        .write_all(line.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(2),
    }
}

fn main() -> ExitCode {
    // A panic message could quote data; print a fixed record instead.
    std::panic::set_hook(Box::new(|_| util::log("worker.panic", "")));
    let Some((state_dir, cache_dir)) = parse_args() else {
        eprintln!("usage: kwe-workshop-worker --state-dir <dir> --cache-dir <dir>");
        return ExitCode::from(64);
    };
    let mut input = Vec::new();
    let read = std::io::stdin()
        .lock()
        .take(envelope::MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut input);
    if read.is_err() {
        return ExitCode::from(65);
    }
    let transport = transport::UreqTransport::new();
    let worker = app::Worker {
        transport: &transport,
        store: store::CredentialStore::new(state_dir),
        cache: previews::PreviewCache::new(cache_dir),
        retry_delay: Duration::from_millis(500),
    };
    write_line(&worker.handle(&input))
}
