// SPDX-License-Identifier: GPL-3.0-or-later
//! `kwe-led-worker` binary. Contract: `docs/LED_PROTOCOL_V1.md`.
//!
//! Exit codes: 0 normal stop (SIGTERM/SIGINT, `led_stop`, stdin EOF,
//! `--duration-s`); 2 usage. `--probe`: 0 reachable, 3 server absent,
//! 1 other error, 2 usage.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clap::Parser;
use kwe_led_protocol::{Mode, is_device_id, validate_frame_path};
use kwe_led_worker::engine::{Engine, EngineConfig, Settings, device_info};
use kwe_led_worker::log::LogLimiter;
use kwe_led_worker::runtime::{Outbox, RunOptions, Shared, read_lines, run};
use kwe_openrgb::{Client, ClientConfig, Error, PROTOCOL_MAX, PROTOCOL_MIN};
use serde_json::{Value, json};

static TERMINATED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signal: libc::c_int) {
    TERMINATED.store(true, Ordering::Release);
}

const PROBE_MAX_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "OpenRGB LED sync worker for kwe-daemon (loopback SDK only)"
)]
struct Args {
    /// OpenRGB SDK port on 127.0.0.1 (the address is not configurable).
    #[arg(long, default_value_t = kwe_openrgb::DEFAULT_PORT, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,
    /// Print one JSON line (reachability, protocol, device inventory) and
    /// exit. Read-only.
    #[arg(long)]
    probe: bool,
    /// Initial mode: off | ambient | spectrum | ambient+pulse.
    #[arg(long, default_value = "ambient", value_parser = parse_mode)]
    mode: Mode,
    /// Initial global brightness cap, percent.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u8).range(0..=100))]
    brightness: u8,
    /// Initial LED update rate (hard maximum 20).
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u8).range(1..=20))]
    rate_hz: u8,
    /// Initial frame file (absolute path).
    #[arg(long, value_parser = parse_frame)]
    frame: Option<PathBuf>,
    /// Initially disabled device id (16 hex digits); repeatable.
    #[arg(long = "disable-device", value_parser = parse_device_id)]
    disable_device: Vec<String>,
    /// Development: stdin EOF does not stop the worker.
    #[arg(long)]
    standalone: bool,
    /// Development: generate audio bands internally (2 Hz bass pulse and a
    /// sweeping peak) instead of waiting for `audio_bands` lines.
    #[arg(long)]
    synthetic_audio: bool,
    /// Stop (and restore) after this many seconds.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=3600))]
    duration_s: Option<u64>,
    /// First re-probe delay after the server was absent (doubles per miss).
    #[arg(long, default_value_t = 1000, hide = true, value_parser = clap::value_parser!(u64).range(50..=60_000))]
    backoff_min_ms: u64,
    /// Re-probe delay cap.
    #[arg(long, default_value_t = 60_000, hide = true, value_parser = clap::value_parser!(u64).range(50..=60_000))]
    backoff_max_ms: u64,
    /// `--probe` I/O timeout.
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..=30_000))]
    timeout_ms: u64,
}

fn parse_mode(text: &str) -> Result<Mode, String> {
    Mode::parse(text).ok_or_else(|| "expected off | ambient | spectrum | ambient+pulse".into())
}

fn parse_frame(text: &str) -> Result<PathBuf, String> {
    validate_frame_path(text).map_err(|e| e.to_string())
}

fn parse_device_id(text: &str) -> Result<String, String> {
    if is_device_id(text) {
        Ok(text.to_string())
    } else {
        Err("device id must be 16 lowercase hex digits".into())
    }
}

fn probe(args: &Args) -> i32 {
    let config = ClientConfig {
        port: args.port,
        client_name: "KDE Wallpaper Engine LED probe".into(),
        io_timeout: Duration::from_millis(args.timeout_ms),
        ..ClientConfig::default()
    };
    let mut out = json!({
        "version": kwe_led_protocol::VERSION,
        "type": "led_probe",
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
            out["reason"] = json!(kwe_led_worker::engine::error_reason(&e));
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
                    out["reason"] = json!(kwe_led_worker::engine::error_reason(&e));
                    out["error"] = json!(e.to_string());
                    1
                }
                Ok(list) => {
                    out["status"] = json!("ok");
                    out["device_count"] = json!(list.len());
                    let devices: Vec<Value> = list
                        .iter()
                        .take(kwe_led_protocol::MAX_STATUS_DEVICES)
                        .filter_map(|c| serde_json::to_value(device_info(c, true, false)).ok())
                        .collect();
                    out["devices"] = Value::Array(devices);
                    0
                }
            }
        }
    };
    let mut line = out.to_string();
    if line.len() > PROBE_MAX_OUTPUT_BYTES {
        out["devices"] = json!([]);
        out["devices_truncated"] = json!(true);
        line = out.to_string();
    }
    println!("{line}");
    code
}

fn main() {
    let args = match Args::try_parse() {
        Ok(a) => a,
        Err(e) => {
            let _ = e.print();
            std::process::exit(if e.use_stderr() { 2 } else { 0 });
        }
    };
    if args.backoff_max_ms < args.backoff_min_ms {
        eprintln!("--backoff-max-ms must be >= --backoff-min-ms");
        std::process::exit(2);
    }
    if args.probe {
        std::process::exit(probe(&args));
    }

    // SAFETY: the handler only stores to a static atomic, which is
    // async-signal-safe. Installed before any other thread starts. This is
    // the crate's only unsafe code (same pattern as kwe-audio-worker);
    // std offers no signal API, and SIGTERM must trigger the device
    // restore.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }

    let now = Instant::now();
    let mut cfg = EngineConfig::with_port(args.port);
    cfg.backoff_min = Duration::from_millis(args.backoff_min_ms);
    cfg.backoff_max = Duration::from_millis(args.backoff_max_ms);
    let settings = Settings {
        mode: args.mode,
        brightness: args.brightness,
        rate_hz: args.rate_hz,
        disabled: args.disable_device.iter().cloned().collect(),
    };
    let engine = Engine::new(cfg, settings, args.frame.clone(), now, LogLimiter::new(now));

    let shared = Shared::new();
    let reader_shared = Arc::clone(&shared);
    if std::thread::Builder::new()
        .name("kwe-led-stdin".into())
        .spawn(move || read_lines(std::io::stdin().lock(), &reader_shared))
        .is_err()
    {
        eprintln!("event=led.fatal detail=stdin_thread");
        std::process::exit(1);
    }
    let outbox = Outbox::start(std::io::stdout());
    run(
        engine,
        shared,
        outbox,
        &TERMINATED,
        RunOptions {
            standalone: args.standalone,
            synthetic_audio: args.synthetic_audio,
            duration: args.duration_s.map(Duration::from_secs),
        },
    );
    std::process::exit(0);
}
