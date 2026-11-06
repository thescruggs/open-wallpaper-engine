// SPDX-License-Identifier: GPL-3.0-or-later
//! Unit tests against the in-process stub server. Ephemeral loopback ports
//! only; no real OpenRGB server is involved.

use std::net::{Ipv4Addr, TcpListener};
use std::time::{Duration, Instant};

use crate::model::{encode_controller, parse_controller};
use crate::stub::{Fault, StubConfig, StubServer, sample_controller, sample_matrix_controller};
use crate::wire::packet_id;
use crate::*;

fn config(port: u16) -> ClientConfig {
    ClientConfig {
        port,
        client_name: "kwe-openrgb test".to_string(),
        connect_timeout: Duration::from_millis(500),
        io_timeout: Duration::from_millis(500),
        enumerate_budget: Duration::from_secs(3),
    }
}

fn stub(cfg: StubConfig) -> StubServer {
    StubServer::start(cfg).expect("start stub")
}

fn connected(server: &StubServer) -> Client {
    let mut c = Client::connect(&config(server.port())).expect("connect");
    c.refresh().expect("refresh");
    c
}

fn le(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

// ---- negotiation -------------------------------------------------------

#[test]
fn negotiates_min_of_client_and_server() {
    for (server, expect) in [(5, 5), (3, 3), (1, 1), (99, PROTOCOL_MAX)] {
        let s = stub(StubConfig {
            protocol_version: server,
            ..StubConfig::default()
        });
        let mut c = Client::connect(&config(s.port())).expect("connect");
        assert_eq!(c.protocol_version(), expect, "server {server}");
        assert_eq!(c.server_protocol_version(), server);
        c.refresh().expect("refresh");
        assert_eq!(c.controllers().len(), 2);
        // Controller data is requested at the negotiated version.
        let req = s
            .received()
            .into_iter()
            .find(|p| p.id == packet_id::REQUEST_CONTROLLER_DATA)
            .expect("data request");
        assert_eq!(req.payload, le(expect));
    }
}

#[test]
fn handshake_sends_version_then_client_name() {
    let s = stub(StubConfig::default());
    let _c = Client::connect(&config(s.port())).expect("connect");
    assert!(s.wait_for_packets(2, Duration::from_secs(2)));
    let rx = s.received();
    assert_eq!(rx[0].id, packet_id::REQUEST_PROTOCOL_VERSION);
    assert_eq!(rx[0].payload, le(PROTOCOL_MAX));
    assert_eq!(rx[1].id, packet_id::SET_CLIENT_NAME);
    assert_eq!(rx[1].payload, b"kwe-openrgb test\0");
    assert_eq!(s.client_names(), vec!["kwe-openrgb test".to_string()]);
}

#[test]
fn refuses_protocol_zero_and_silent_server() {
    let s = stub(StubConfig {
        protocol_version: 0,
        ..StubConfig::default()
    });
    let err = Client::connect(&config(s.port())).unwrap_err();
    assert!(
        matches!(err, Error::UnsupportedVersion { server: 0, .. }),
        "{err:?}"
    );

    let s = stub(StubConfig {
        fault: Fault::NoVersionReply,
        ..StubConfig::default()
    });
    let mut cfg = config(s.port());
    cfg.io_timeout = Duration::from_millis(150);
    let start = Instant::now();
    let err = Client::connect(&cfg).unwrap_err();
    assert!(
        matches!(err, Error::UnsupportedVersion { server: 0, .. }),
        "{err:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn config_is_loopback_only_and_validated() {
    // There is no address field to set; port 0 and bad names are rejected
    // before any socket is opened.
    assert_eq!(
        Client::connect(&config(0)).unwrap_err(),
        Error::InvalidArgument("port must be non-zero")
    );
    let mut cfg = config(1);
    cfg.client_name = "a\0b".into();
    assert!(matches!(
        Client::connect(&cfg).unwrap_err(),
        Error::InvalidArgument(_)
    ));
    cfg.client_name = "x".repeat(65);
    assert!(matches!(
        Client::connect(&cfg).unwrap_err(),
        Error::InvalidArgument(_)
    ));
    let mut cfg = config(1);
    cfg.io_timeout = Duration::ZERO;
    assert!(matches!(
        Client::connect(&cfg).unwrap_err(),
        Error::InvalidArgument(_)
    ));
    assert_eq!(ClientConfig::default().port, DEFAULT_PORT);
}

#[test]
fn refused_connection_is_server_absent_and_cheap() {
    let port = {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        l.local_addr().unwrap().port()
    };
    let start = Instant::now();
    let err = Client::connect(&config(port)).unwrap_err();
    assert_eq!(err, Error::ServerAbsent { port });
    assert!(err.is_server_absent());
    assert!(!err.is_connection_lost());
    assert!(
        start.elapsed() < Duration::from_millis(400),
        "{:?}",
        start.elapsed()
    );
}

// ---- enumeration -------------------------------------------------------

#[test]
fn enumerates_controllers_zones_leds_modes() {
    let s = stub(StubConfig {
        controllers: vec![
            sample_controller("Strip", &[("A", 4), ("B", 3)]),
            sample_matrix_controller("Board"),
        ],
        ..StubConfig::default()
    });
    let c = connected(&s);
    let list = c.controllers();
    assert_eq!(list.len(), 2);
    let strip = &list[0];
    assert_eq!(strip.id, ControllerId::new(0));
    assert_eq!(strip.name, "Strip");
    assert_eq!(strip.vendor, "KWE Test");
    assert_eq!(strip.location, "stub:Strip");
    assert_eq!(strip.zones.len(), 2);
    assert_eq!(strip.zones[1].name, "B");
    assert_eq!(strip.zones[1].leds_count, 3);
    assert_eq!(strip.leds.len(), 7);
    assert_eq!(strip.colors.len(), 7);
    assert_eq!(strip.zone_led_offset(1), Some(4));
    let (idx, mode) = strip.active_mode().unwrap();
    assert_eq!((idx, mode.name.as_str()), (2, "Rainbow"));
    assert_eq!(strip.modes[1].colors, vec![Rgb::new(0, 0, 255)]);
    let board = &list[1];
    assert_eq!(board.zones[0].kind, zone_type::MATRIX);
    let m = board.zones[0].matrix.as_ref().unwrap();
    assert_eq!((m.height, m.width, m.cells.len()), (2, 3, 6));
    assert!(!c.needs_refresh());
}

#[test]
fn controller_codec_round_trips_every_supported_version() {
    let mut c = sample_matrix_controller("RT");
    c.zones[0].segments.push(Segment {
        name: "seg".into(),
        kind: 1,
        start: 0,
        leds_count: 3,
    });
    c.flags = 7;
    c.zones[0].flags = 3;
    for v in PROTOCOL_MIN..=PROTOCOL_MAX {
        let bytes = encode_controller(&c, v);
        let parsed = parse_controller(&bytes, v, ControllerId::new(0)).expect("parse");
        let mut want = c.clone();
        if v < 3 {
            for m in &mut want.modes {
                m.brightness_min = 0;
                m.brightness_max = 0;
                m.brightness = 0;
            }
        }
        if v < 4 {
            want.zones[0].segments.clear();
        }
        if v < 5 {
            want.flags = 0;
            want.zones[0].flags = 0;
        }
        assert_eq!(parsed, want, "version {v}");
    }
}

// ---- updates and modes -------------------------------------------------

#[test]
fn update_leds_sends_exact_payload_and_reaches_server() {
    let s = stub(StubConfig::default());
    let mut c = connected(&s);
    let id = ControllerId::new(0);
    let colors: Vec<Rgb> = (0..6).map(|i| Rgb::new(i, 2 * i, 255 - i)).collect();
    c.update_leds(id, &colors).unwrap();
    let before = s.received().len();
    c.update_leds(id, &colors).unwrap();
    assert!(s.wait_for_packets(before + 1, Duration::from_secs(2)));
    let p = s.received().pop().unwrap();
    assert_eq!((p.dev_id, p.id), (0, packet_id::UPDATE_LEDS));
    let mut want = le(4 + 2 + 6 * 4);
    want.extend_from_slice(&6u16.to_le_bytes());
    for c in &colors {
        want.extend_from_slice(&[c.r, c.g, c.b, 0]);
    }
    assert_eq!(p.payload, want);
    assert_eq!(s.controllers()[0].colors, colors);
}

#[test]
fn update_leds_rejects_wrong_count_without_sending() {
    let s = stub(StubConfig::default());
    let mut c = connected(&s);
    let before = s.received().len();
    let err = c
        .update_leds(ControllerId::new(0), &[Rgb::new(1, 2, 3)])
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)));
    let err = c.update_leds(ControllerId::new(9), &[]).unwrap_err();
    assert_eq!(err, Error::UnknownController(9));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(s.received().len(), before);
    // The connection is still usable.
    c.update_leds(ControllerId::new(0), &[Rgb::default(); 6])
        .unwrap();
}

#[test]
fn update_zone_leds_targets_one_zone() {
    let s = stub(StubConfig::default());
    let mut c = connected(&s);
    let id = ControllerId::new(1); // zones of 4 and 3
    let red = [Rgb::new(255, 0, 0); 3];
    c.update_zone_leds(id, 1, &red).unwrap();
    assert!(matches!(
        c.update_zone_leds(id, 1, &red[..2]).unwrap_err(),
        Error::InvalidArgument(_)
    ));
    assert!(matches!(
        c.update_zone_leds(id, 5, &red).unwrap_err(),
        Error::InvalidArgument(_)
    ));
    let n = s.received().len();
    c.update_leds(id, &[Rgb::default(); 7]).unwrap(); // marker to flush ordering
    assert!(s.wait_for_packets(n + 1, Duration::from_secs(2)));
    let zone_pkt = s
        .received()
        .into_iter()
        .find(|p| p.id == packet_id::UPDATE_ZONE_LEDS)
        .unwrap();
    assert_eq!(&zone_pkt.payload[4..8], &1u32.to_le_bytes());
}

#[test]
fn take_control_then_restore_previous_mode() {
    let s = stub(StubConfig::default());
    let mut c = connected(&s);
    let id = ControllerId::new(0);
    assert_eq!(s.controllers()[0].active_mode, 2);
    c.take_control(id).unwrap();
    assert_eq!(
        s.controllers()[0].active_mode,
        0,
        "server switched to Direct"
    );
    assert_eq!(
        c.controller(id).unwrap().active_mode,
        0,
        "client re-read it"
    );
    // A second take_control does not overwrite the remembered mode.
    c.take_control(id).unwrap();
    assert_eq!(c.saved_mode_count(), 1);
    assert!(c.restore(id).unwrap());
    assert!(!c.restore(id).unwrap(), "nothing left to restore");
    assert!(s.wait_for_packets(1, Duration::from_secs(1)));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(s.controllers()[0].active_mode, 2);
    let p = s
        .received()
        .into_iter()
        .rfind(|p| p.id == packet_id::UPDATE_MODE)
        .unwrap();
    assert_eq!(&p.payload[4..8], &2i32.to_le_bytes());
    // Mode block: name "Rainbow" + NUL, then the echoed opaque value.
    assert_eq!(&p.payload[8..10], &8u16.to_le_bytes());
    assert_eq!(&p.payload[10..18], b"Rainbow\0");
    assert_eq!(&p.payload[18..22], &0x12i32.to_le_bytes());
}

#[test]
fn restore_all_matches_devices_by_key_after_reorder() {
    // Distinct original modes: a stale index would restore the wrong one.
    let mut strip = sample_controller("Stub Strip", &[("A", 4), ("B", 3)]);
    strip.active_mode = 1;
    let s = stub(StubConfig {
        controllers: vec![sample_controller("Stub Keyboard", &[("Keys", 6)]), strip],
        ..StubConfig::default()
    });
    let mut c = connected(&s);
    c.take_control(ControllerId::new(0)).unwrap();
    c.take_control(ControllerId::new(1)).unwrap();
    // Server reorders its list and announces it.
    let mut list = s.controllers();
    list.reverse();
    s.set_controllers(list);
    s.notify_device_list_updated();
    std::thread::sleep(Duration::from_millis(100));
    let reports = c.restore_all();
    assert_eq!(reports.len(), 2);
    assert!(reports.iter().all(|r| r.result.is_ok()), "{reports:?}");
    std::thread::sleep(Duration::from_millis(100));
    let after: Vec<(String, i32)> = s
        .controllers()
        .into_iter()
        .map(|c| (c.name, c.active_mode))
        .collect();
    assert_eq!(
        after,
        vec![
            ("Stub Strip".to_string(), 1),
            ("Stub Keyboard".to_string(), 2)
        ]
    );
    assert_eq!(c.saved_mode_count(), 0);
}

#[test]
fn restore_all_reports_vanished_device() {
    let s = stub(StubConfig::default());
    let mut c = connected(&s);
    c.take_control(ControllerId::new(1)).unwrap();
    s.set_controllers(vec![s.controllers()[0].clone()]);
    s.notify_device_list_updated();
    std::thread::sleep(Duration::from_millis(100));
    let reports = c.restore_all();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].result, Err(Error::DeviceGone));
    assert_eq!(reports[0].device.name, "Stub Strip");
}

// ---- notifications -----------------------------------------------------

#[test]
fn device_list_updated_forces_re_enumeration() {
    let s = stub(StubConfig::default());
    let mut c = connected(&s);
    assert_eq!(c.poll_events().unwrap(), PollOutcome::default());
    let mut list = s.controllers();
    list.push(sample_controller("Hotplugged", &[("Z", 2)]));
    s.set_controllers(list);
    s.notify_device_list_updated();
    std::thread::sleep(Duration::from_millis(100));
    let err = c
        .update_leds(ControllerId::new(0), &[Rgb::default(); 6])
        .unwrap_err();
    assert_eq!(err, Error::DeviceListChanged);
    assert!(c.needs_refresh());
    assert_eq!(c.refresh().unwrap().len(), 3);
    assert!(!c.needs_refresh());
    c.update_leds(ControllerId::new(2), &[Rgb::new(1, 1, 1); 2])
        .unwrap();
}

#[test]
fn poll_reports_device_list_update() {
    let s = stub(StubConfig::default());
    let mut c = connected(&s);
    s.notify_device_list_updated();
    let end = Instant::now() + Duration::from_secs(2);
    let mut seen = PollOutcome::default();
    while Instant::now() < end && !seen.device_list_updated {
        seen = c.poll_events().unwrap();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(seen.device_list_updated);
    assert!(seen.packets >= 1 || c.needs_refresh());
}

#[test]
fn acks_and_notifications_are_drained_not_fatal() {
    let s = stub(StubConfig {
        ack_updates: true,
        ..StubConfig::default()
    });
    let mut c = connected(&s);
    let id = ControllerId::new(0);
    for _ in 0..500 {
        c.update_leds(id, &[Rgb::new(9, 9, 9); 6]).unwrap();
    }
    // A request/reply exchange still works after ACK noise.
    c.take_control(id).unwrap();
    assert!(c.restore(id).unwrap());
    assert!(c.refresh().is_ok());
}

// ---- hostile / broken servers -----------------------------------------

fn refresh_error(fault: Fault) -> Error {
    let s = stub(StubConfig {
        fault,
        ..StubConfig::default()
    });
    let mut cfg = config(s.port());
    cfg.io_timeout = Duration::from_millis(300);
    let mut c = Client::connect(&cfg).expect("connect");
    let err = c.refresh().unwrap_err();
    assert!(c.needs_refresh());
    assert!(c.controllers().is_empty());
    err
}

#[test]
fn bad_magic_is_typed() {
    let err = refresh_error(Fault::BadMagic);
    assert_eq!(err, Error::BadMagic);
    assert!(err.is_connection_lost());
}

#[test]
fn oversized_packet_is_rejected_before_allocation() {
    let err = refresh_error(Fault::OversizedHeader);
    assert!(
        matches!(
            err,
            Error::PacketTooLarge {
                size: 0x7fff_fff0,
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn truncated_packet_then_close_is_disconnected() {
    assert_eq!(
        refresh_error(Fault::TruncatedThenClose),
        Error::Disconnected
    );
}

#[test]
fn inner_length_overflow_is_malformed_and_connection_survives() {
    let s = stub(StubConfig {
        fault: Fault::InnerLengthOverflow,
        ..StubConfig::default()
    });
    let mut c = Client::connect(&config(s.port())).unwrap();
    let err = c.refresh().unwrap_err();
    assert!(matches!(err, Error::Malformed { .. }), "{err:?}");
    assert!(!err.is_connection_lost());
    s.set_fault(Fault::None);
    assert_eq!(c.refresh().unwrap().len(), 2);
}

#[test]
fn garbage_payload_is_a_typed_error() {
    let err = refresh_error(Fault::Garbage);
    assert!(
        matches!(err, Error::Malformed { .. } | Error::LimitExceeded { .. }),
        "{err:?}"
    );
}

#[test]
fn huge_controller_count_is_limited() {
    let err = refresh_error(Fault::HugeControllerCount);
    assert!(
        matches!(
            err,
            Error::LimitExceeded {
                what: "controller count",
                value: 1_000_000,
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn stalled_server_hits_timeout_and_poisons_connection() {
    let start = Instant::now();
    let s = stub(StubConfig {
        fault: Fault::Stall(Duration::from_secs(10)),
        ..StubConfig::default()
    });
    let mut cfg = config(s.port());
    cfg.io_timeout = Duration::from_millis(200);
    let mut c = Client::connect(&cfg).unwrap();
    let err = c.refresh().unwrap_err();
    assert!(matches!(err, Error::Timeout { .. }), "{err:?}");
    assert!(c.is_connection_lost());
    assert_eq!(c.refresh().unwrap_err(), Error::Disconnected);
    assert_eq!(c.poll_events().unwrap_err(), Error::Disconnected);
    drop(s);
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn drip_fed_reply_hits_per_packet_deadline() {
    let start = Instant::now();
    let err = refresh_error(Fault::DripFeed(Duration::from_millis(20)));
    assert!(matches!(err, Error::Timeout { .. }), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn enumerate_budget_bounds_whole_refresh() {
    // Each reply arrives within io_timeout, but the total exceeds the budget.
    let s = stub(StubConfig {
        fault: Fault::Stall(Duration::from_millis(150)),
        controllers: (0..20)
            .map(|i| sample_controller(&format!("C{i}"), &[("Z", 1)]))
            .collect(),
        ..StubConfig::default()
    });
    let mut cfg = config(s.port());
    cfg.io_timeout = Duration::from_millis(400);
    cfg.enumerate_budget = Duration::from_millis(500);
    let mut c = Client::connect(&cfg).unwrap();
    let start = Instant::now();
    let err = c.refresh().unwrap_err();
    assert!(matches!(err, Error::Timeout { .. }), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn server_closing_mid_session_is_disconnected() {
    let s = stub(StubConfig::default());
    let mut c = connected(&s);
    drop(s);
    std::thread::sleep(Duration::from_millis(100));
    let mut last = Ok(());
    for _ in 0..20 {
        last = c.update_leds(ControllerId::new(0), &[Rgb::default(); 6]);
        if last.is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let err = last.unwrap_err();
    assert!(err.is_connection_lost(), "{err:?}");
    // restore_all never panics or blocks on a dead connection.
    assert!(c.restore_all().is_empty());
}

// ---- parser robustness -------------------------------------------------

#[test]
fn every_truncation_of_a_valid_controller_is_an_error() {
    let bytes = encode_controller(&sample_matrix_controller("T"), PROTOCOL_MAX);
    for len in 0..bytes.len() {
        assert!(
            parse_controller(&bytes[..len], PROTOCOL_MAX, ControllerId::new(0)).is_err(),
            "prefix {len} parsed"
        );
    }
}

#[test]
fn random_mutations_never_panic() {
    let base = encode_controller(&sample_matrix_controller("M"), PROTOCOL_MAX);
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..5000 {
        let mut b = base.clone();
        for _ in 0..(next() % 8 + 1) {
            let i = (next() as usize) % b.len();
            b[i] = next() as u8;
        }
        for v in PROTOCOL_MIN..=PROTOCOL_MAX {
            let _ = parse_controller(&b, v, ControllerId::new(0));
        }
    }
}

#[test]
fn per_controller_limits_are_enforced() {
    let mut c = sample_controller("Z", &[]);
    c.zones = (0..=limits::MAX_ZONES)
        .map(|i| Zone {
            name: format!("z{i}"),
            kind: 0,
            leds_min: 0,
            leds_max: 0,
            leds_count: 0,
            matrix: None,
            segments: Vec::new(),
            flags: 0,
        })
        .collect();
    let err = parse_controller(&encode_controller(&c, 5), 5, ControllerId::new(0)).unwrap_err();
    assert!(
        matches!(
            err,
            Error::LimitExceeded {
                what: "zone count",
                ..
            }
        ),
        "{err:?}"
    );

    let mut c = sample_controller("S", &[("A", 1)]);
    c.name = "n".repeat(usize::from(limits::MAX_STRING_BYTES));
    let err = parse_controller(&encode_controller(&c, 5), 5, ControllerId::new(0)).unwrap_err();
    assert!(
        matches!(
            err,
            Error::LimitExceeded {
                what: "string length",
                ..
            }
        ),
        "{err:?}"
    );

    let mut c = sample_matrix_controller("M");
    c.zones[0].matrix.as_mut().unwrap().width = 4; // 2x4 != 6 cells
    let err = parse_controller(&encode_controller(&c, 5), 5, ControllerId::new(0)).unwrap_err();
    assert_eq!(
        err,
        Error::Malformed {
            what: "zone matrix size"
        }
    );
}

#[test]
fn rgb_hex_parsing() {
    assert_eq!(Rgb::parse_hex("#FF8000"), Some(Rgb::new(255, 128, 0)));
    assert_eq!(Rgb::parse_hex("00ff00"), Some(Rgb::new(0, 255, 0)));
    assert_eq!(Rgb::parse_hex("fff"), None);
    assert_eq!(Rgb::parse_hex("zzzzzz"), None);
    assert_eq!(Rgb::new(1, 2, 3).to_string(), "#010203");
}
