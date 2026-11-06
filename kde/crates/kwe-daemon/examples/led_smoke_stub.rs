// SPDX-License-Identifier: GPL-3.0-or-later
//! `scripts/smoke-led.sh` helper (L5): the recording stub OpenRGB SDK
//! server (`kwe_openrgb::stub`, dev-only) driven line by line over stdin, so
//! the smoke can start it late (the "server appears" case) and inspect what
//! it received while the daemon runs. Never shipped; test builds only.
//!
//! Commands (one per line), each answered with one JSON line on stdout:
//!
//! - `listen <port>` — bind 127.0.0.1:<port> (refuses 6742, the real SDK
//!   port) → `{"listening": port}` / `{"error": "..."}`
//! - `snapshot` → `{"listening", "connections", "client_names", "total",
//!   "counts": {"<packet id>": n}, "update_leds_by_device": [n, ...],
//!   "controllers": [{"name", "active_mode", "colors": [[r, g, b], ...]}]}`
//! - `quit` (or stdin EOF) → `{"bye": true}`, exit 0.

use std::io::{BufRead, Write};

use kwe_openrgb::stub::{StubConfig, StubServer};
use kwe_openrgb::wire::packet_id;
use serde_json::{Value, json};

const REAL_SDK_PORT: u16 = 6742;

fn snapshot(server: Option<&StubServer>) -> Value {
    let Some(server) = server else {
        return json!({"listening": false, "connections": 0, "total": 0});
    };
    let received = server.received();
    let mut counts = serde_json::Map::new();
    let controllers = server.controllers();
    let mut by_device = vec![0_u64; controllers.len()];
    for packet in &received {
        let entry = counts.entry(packet.id.to_string()).or_insert(json!(0));
        *entry = json!(entry.as_u64().unwrap_or(0) + 1);
        if packet.id == packet_id::UPDATE_LEDS
            && let Some(slot) = by_device.get_mut(packet.dev_id as usize)
        {
            *slot += 1;
        }
    }
    json!({
        "listening": true,
        "port": server.port(),
        "connections": server.connections(),
        "client_names": server.client_names(),
        "total": received.len(),
        "counts": counts,
        "update_leds_by_device": by_device,
        "controllers": controllers.iter().map(|c| json!({
            "name": c.name,
            "active_mode": c.active_mode,
            "colors": c.colors.iter().map(|rgb| json!([rgb.r, rgb.g, rgb.b])).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn main() {
    let mut server: Option<StubServer> = None;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let mut words = line.split_whitespace();
        let reply = match (words.next(), words.next()) {
            (Some("listen"), Some(port)) => match port.parse::<u16>() {
                Ok(REAL_SDK_PORT) | Ok(0) => json!({"error": "refusing port 0/6742"}),
                Ok(_) if server.is_some() => json!({"error": "already listening"}),
                Ok(port) => match StubServer::start_on(port, StubConfig::default()) {
                    Ok(started) => {
                        server = Some(started);
                        json!({"listening": port})
                    }
                    Err(error) => json!({"error": error.to_string()}),
                },
                Err(error) => json!({"error": error.to_string()}),
            },
            (Some("snapshot"), None) => snapshot(server.as_ref()),
            (Some("quit"), None) => break,
            _ => json!({"error": format!("unknown command {line:?}")}),
        };
        if writeln!(stdout, "{reply}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            break;
        }
    }
    let _ = writeln!(stdout, "{}", json!({"bye": true}));
}
