// SPDX-License-Identifier: GPL-3.0-or-later
//! Runs the recording stub SDK server for smoke tests:
//!
//!   cargo run -p kwe-openrgb --features stub --example stub_server -- [--port N]
//!
//! Prints `{"port":N}` once listening (port 0 = ephemeral), runs until stdin
//! reaches EOF, then prints one JSON line summarising what it received
//! (packet ids in order, client names, final active modes) and exits.

use std::io::Read;

use kwe_openrgb::stub::{StubConfig, StubServer};
use serde_json::json;

fn main() {
    let mut port = 0u16;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match (flag.as_str(), it.next()) {
            ("--port", Some(v)) => port = v.parse().expect("--port N"),
            _ => {
                eprintln!("usage: stub_server [--port N]");
                std::process::exit(2);
            }
        }
    }
    let server = StubServer::start_on(port, StubConfig::default()).expect("bind 127.0.0.1");
    println!("{}", json!({"port": server.port()}));
    let mut sink = Vec::new();
    let _ = std::io::stdin().take(1 << 16).read_to_end(&mut sink);
    let ids = server.received_ids();
    let shown: Vec<u32> = ids.iter().copied().take(1000).collect();
    let modes: Vec<i32> = server.controllers().iter().map(|c| c.active_mode).collect();
    println!(
        "{}",
        json!({"received": ids.len(), "ids": shown, "client_names": server.client_names(),
               "active_modes": modes})
    );
}
