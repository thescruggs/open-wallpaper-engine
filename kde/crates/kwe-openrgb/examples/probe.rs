// SPDX-License-Identifier: GPL-3.0-or-later
//! OpenRGB SDK diagnostic probe. Prints exactly one bounded JSON line.
//!
//!   cargo run -p kwe-openrgb --example probe -- [--port N] [--timeout-ms N]
//!       [--set-color RRGGBB --hold-ms N]
//!
//! Default is read-only (connect, negotiate, enumerate). `--set-color`
//! switches every controller to its per-LED mode, paints it, holds for
//! `--hold-ms` (≤10000, default 1000), then restores each device's previous
//! mode. Exit: 0 reachable, 3 server absent, 1 other error, 2 usage.

use std::time::Duration;

use kwe_openrgb::{Client, ClientConfig, Error, PROTOCOL_MAX, PROTOCOL_MIN, Rgb};
use serde_json::{Value, json};

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_TEXT: usize = 80;
const MAX_LISTED: usize = 32;

struct Args {
    port: u16,
    timeout_ms: u64,
    color: Option<Rgb>,
    hold_ms: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        port: kwe_openrgb::DEFAULT_PORT,
        timeout_ms: 1000,
        color: None,
        hold_ms: 1000,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--port" => args.port = value()?.parse().map_err(|_| "bad --port")?,
            "--timeout-ms" => args.timeout_ms = value()?.parse().map_err(|_| "bad --timeout-ms")?,
            "--hold-ms" => args.hold_ms = value()?.parse().map_err(|_| "bad --hold-ms")?,
            "--set-color" => {
                args.color = Some(Rgb::parse_hex(&value()?).ok_or("bad --set-color (RRGGBB)")?)
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if args.port == 0 || !(1..=30_000).contains(&args.timeout_ms) || args.hold_ms > 10_000 {
        return Err("out-of-range argument".into());
    }
    Ok(args)
}

fn text(s: &str) -> String {
    s.chars().take(MAX_TEXT).collect()
}

fn controllers_json(client: &Client) -> Value {
    Value::Array(
        client
            .controllers()
            .iter()
            .take(MAX_LISTED)
            .map(|c| {
                json!({
                    "index": c.id.index(),
                    "name": text(&c.name),
                    "vendor": text(&c.vendor),
                    "type": c.kind,
                    "location": text(&c.location),
                    "leds": c.leds.len(),
                    "active_mode": c.active_mode().map(|(i, m)| json!({"index": i, "name": text(&m.name)})),
                    "modes": c.modes.iter().take(MAX_LISTED).map(|m| text(&m.name)).collect::<Vec<_>>(),
                    "zones": c.zones.iter().take(MAX_LISTED).map(|z| json!({
                        "name": text(&z.name), "type": z.kind, "leds": z.leds_count,
                        "matrix": z.matrix.as_ref().map(|m| [m.height, m.width]),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

fn color_test(client: &mut Client, color: Rgb, hold_ms: u64) -> Value {
    let ids: Vec<_> = client
        .controllers()
        .iter()
        .map(|c| (c.id, c.leds.len()))
        .collect();
    let mut painted = Vec::new();
    for (id, leds) in ids {
        let r = client
            .take_control(id)
            .and_then(|()| client.update_leds(id, &vec![color; leds]));
        painted.push(
            json!({"index": id.index(), "ok": r.is_ok(), "error": r.err().map(|e| e.to_string())}),
        );
    }
    std::thread::sleep(Duration::from_millis(hold_ms));
    let restored: Vec<Value> = client
        .restore_all()
        .into_iter()
        .map(|r| {
            json!({"device": text(&r.device.name), "ok": r.result.is_ok(),
                         "error": r.result.err().map(|e| e.to_string())})
        })
        .collect();
    json!({"color": color.to_string(), "held_ms": hold_ms, "painted": painted, "restored": restored})
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            println!("{}", json!({"status": "usage_error", "error": e}));
            std::process::exit(2);
        }
    };
    let config = ClientConfig {
        port: args.port,
        client_name: "KWE probe".into(),
        io_timeout: Duration::from_millis(args.timeout_ms),
        ..ClientConfig::default()
    };
    let mut out = json!({
        "port": args.port,
        "client_protocol": {"min": PROTOCOL_MIN, "max": PROTOCOL_MAX},
    });
    let code = match Client::connect(&config) {
        Err(e @ Error::ServerAbsent { .. }) => {
            out["status"] = json!("server_absent");
            out["reachable"] = json!(false);
            out["error"] = json!(e.to_string());
            3
        }
        Err(e) => {
            out["status"] = json!("error");
            out["reachable"] = json!(true);
            out["error"] = json!(e.to_string());
            1
        }
        Ok(mut client) => {
            out["reachable"] = json!(true);
            out["server_protocol"] = json!(client.server_protocol_version());
            out["negotiated_protocol"] = json!(client.protocol_version());
            match client.refresh() {
                Err(e) => {
                    out["status"] = json!("error");
                    out["error"] = json!(e.to_string());
                    1
                }
                Ok(list) => {
                    out["status"] = json!("ok");
                    out["controller_count"] = json!(list.len());
                    out["controllers"] = controllers_json(&client);
                    if let Some(color) = args.color {
                        out["color_test"] = color_test(&mut client, color, args.hold_ms);
                    }
                    0
                }
            }
        }
    };
    let mut line = out.to_string();
    if line.len() > MAX_OUTPUT_BYTES {
        out["controllers"] = json!([]);
        out["truncated"] = json!(true);
        line = out.to_string();
    }
    println!("{line}");
    std::process::exit(code);
}
