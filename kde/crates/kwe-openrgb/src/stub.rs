// SPDX-License-Identifier: GPL-3.0-or-later
//! In-process stub OpenRGB SDK server (feature `stub`; always on in this
//! crate's tests). Binds an ephemeral loopback port, answers the packet
//! subset the client uses, keeps mutable per-controller state (active mode,
//! colours), records every packet it receives, can push a
//! device-list-updated notification, and injects faults.
//!
//! Test helper only: bounded (recording capped, payloads capped) but not
//! hardened; never ship it in a binary.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::model::{Controller, ControllerId, Led, MatrixMap, Mode, Rgb, Zone, encode_controller};
use crate::wire::{HEADER_LEN, MAGIC, encode_packet, packet_id};

const MAX_RECORDED: usize = 10_000;
const MAX_STUB_PAYLOAD: u32 = 16 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(10);

/// Fault injected into the server's replies.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Fault {
    #[default]
    None,
    /// Never answer `REQUEST_PROTOCOL_VERSION` (a protocol-0 server).
    NoVersionReply,
    /// Controller-data reply header carries a wrong magic.
    BadMagic,
    /// Controller-data reply header claims a huge payload.
    OversizedHeader,
    /// Controller-data reply sends half its payload, then closes.
    TruncatedThenClose,
    /// Controller-data reply is framed correctly but its name length points
    /// far past the payload.
    InnerLengthOverflow,
    /// Controller-data reply payload is deterministic pseudo-random bytes.
    Garbage,
    /// Controller-count reply claims one million controllers.
    HugeControllerCount,
    /// Sleep this long before each controller-data reply.
    Stall(Duration),
    /// Send each controller-data reply one byte at a time, sleeping this
    /// long between bytes.
    DripFeed(Duration),
    /// Sleep this long before a controller-data reply that directly follows
    /// a `SetCustomMode` for the same device (the client's re-read after
    /// taking control). Enumeration stays fast: a slow-but-alive server.
    StallAfterCustomMode(Duration),
}

#[derive(Debug, Clone)]
pub struct StubConfig {
    /// Highest protocol version this stub reports.
    pub protocol_version: u32,
    pub controllers: Vec<Controller>,
    pub fault: Fault,
    /// Answer every LED/mode update with an ACK packet (newer-server noise
    /// the client must drain without failing).
    pub ack_updates: bool,
}

impl Default for StubConfig {
    fn default() -> Self {
        Self {
            protocol_version: 5,
            controllers: vec![
                sample_controller("Stub Keyboard", &[("Keys", 6)]),
                sample_controller("Stub Strip", &[("Strip A", 4), ("Strip B", 3)]),
            ],
            fault: Fault::None,
            ack_updates: false,
        }
    }
}

/// A packet the stub received from a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedPacket {
    pub dev_id: u32,
    pub id: u32,
    pub payload: Vec<u8>,
}

/// A synthetic controller: modes `Direct` (per-LED), `Static`, `Rainbow`
/// (active), one LED per zone slot, all LEDs black.
pub fn sample_controller(name: &str, zones: &[(&str, u32)]) -> Controller {
    let mode = |name: &str, value: i32, color_mode: u32, colors: Vec<Rgb>| Mode {
        name: name.to_string(),
        value,
        flags: 1 << 5,
        speed_min: 0,
        speed_max: if name == "Rainbow" { 10 } else { 0 },
        brightness_min: 0,
        brightness_max: 100,
        colors_min: colors.len() as u32,
        colors_max: colors.len() as u32,
        speed: 3,
        brightness: 100,
        direction: 0,
        color_mode,
        colors,
    };
    let total: u32 = zones.iter().map(|(_, n)| n).sum();
    Controller {
        id: ControllerId::new(0),
        kind: 4,
        name: name.to_string(),
        vendor: "KWE Test".to_string(),
        description: "synthetic stub controller".to_string(),
        version: "1".to_string(),
        serial: format!("SN-{name}"),
        location: format!("stub:{name}"),
        modes: vec![
            mode("Direct", 0x10, 1, Vec::new()),
            mode("Static", 0x11, 2, vec![Rgb::new(0, 0, 255)]),
            mode("Rainbow", 0x12, 0, Vec::new()),
        ],
        active_mode: 2,
        zones: zones
            .iter()
            .map(|(zname, n)| Zone {
                name: (*zname).to_string(),
                kind: crate::zone_type::LINEAR,
                leds_min: *n,
                leds_max: *n,
                leds_count: *n,
                matrix: None,
                segments: Vec::new(),
                flags: 0,
            })
            .collect(),
        leds: (0..total)
            .map(|i| Led {
                name: format!("LED {i}"),
                value: i,
            })
            .collect(),
        colors: vec![Rgb::default(); total as usize],
        flags: 0,
    }
}

/// Like [`sample_controller`] with a single 2x3 matrix zone of 6 LEDs.
pub fn sample_matrix_controller(name: &str) -> Controller {
    let mut c = sample_controller(name, &[("Matrix", 6)]);
    c.zones[0].kind = crate::zone_type::MATRIX;
    c.zones[0].matrix = Some(MatrixMap {
        height: 2,
        width: 3,
        cells: vec![0, 1, 2, 3, 4, 5],
    });
    c
}

struct Shared {
    protocol_version: u32,
    controllers: Vec<Controller>,
    fault: Fault,
    ack_updates: bool,
    received: Vec<RecordedPacket>,
    client_names: Vec<String>,
    writers: Vec<Arc<Mutex<TcpStream>>>,
    connections: usize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct StubServer {
    port: u16,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    handlers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl StubServer {
    /// Starts on an ephemeral `127.0.0.1` port.
    pub fn start(config: StubConfig) -> io::Result<Self> {
        Self::start_on(0, config)
    }

    /// Starts on a chosen loopback port (0 = ephemeral).
    pub fn start_on(port: u16, config: StubConfig) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Mutex::new(Shared {
            protocol_version: config.protocol_version,
            controllers: config.controllers,
            fault: config.fault,
            ack_updates: config.ack_updates,
            received: Vec::new(),
            client_names: Vec::new(),
            writers: Vec::new(),
            connections: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let handlers = Arc::new(Mutex::new(Vec::new()));
        let accept = {
            let (shared, stop, handlers) = (shared.clone(), stop.clone(), handlers.clone());
            thread::spawn(move || accept_loop(&listener, &shared, &stop, &handlers))
        };
        Ok(Self {
            port,
            shared,
            stop,
            accept: Some(accept),
            handlers,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn received(&self) -> Vec<RecordedPacket> {
        lock(&self.shared).received.clone()
    }

    pub fn received_ids(&self) -> Vec<u32> {
        lock(&self.shared).received.iter().map(|p| p.id).collect()
    }

    pub fn client_names(&self) -> Vec<String> {
        lock(&self.shared).client_names.clone()
    }

    pub fn connections(&self) -> usize {
        lock(&self.shared).connections
    }

    /// Current server-side controller state (active modes, colours).
    pub fn controllers(&self) -> Vec<Controller> {
        lock(&self.shared).controllers.clone()
    }

    pub fn set_controllers(&self, controllers: Vec<Controller>) {
        lock(&self.shared).controllers = controllers;
    }

    pub fn set_fault(&self, fault: Fault) {
        lock(&self.shared).fault = fault;
    }

    /// Pushes `DEVICE_LIST_UPDATED` to every connected client.
    pub fn notify_device_list_updated(&self) {
        let writers = lock(&self.shared).writers.clone();
        let packet = encode_packet(0, packet_id::DEVICE_LIST_UPDATED, &[]);
        for w in writers {
            let _ = lock(&w).write_all(&packet);
        }
    }

    /// Blocks until at least `n` packets were recorded or `timeout` passes.
    pub fn wait_for_packets(&self, n: usize, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if lock(&self.shared).received.len() >= n {
                return true;
            }
            thread::sleep(POLL);
        }
        lock(&self.shared).received.len() >= n
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for w in lock(&self.shared).writers.drain(..) {
            let _ = lock(&w).shutdown(std::net::Shutdown::Both);
        }
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
        for t in lock(&self.handlers).drain(..) {
            let _ = t.join();
        }
    }
}

fn accept_loop(
    listener: &TcpListener,
    shared: &Arc<Mutex<Shared>>,
    stop: &Arc<AtomicBool>,
    handlers: &Arc<Mutex<Vec<JoinHandle<()>>>>,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(POLL * 5));
                let Ok(writer) = stream.try_clone() else {
                    continue;
                };
                let writer = Arc::new(Mutex::new(writer));
                {
                    let mut s = lock(shared);
                    s.writers.push(writer.clone());
                    s.connections += 1;
                }
                let (shared, stop) = (shared.clone(), stop.clone());
                let handle = thread::spawn(move || {
                    let _ = handle_connection(stream, &writer, &shared, &stop);
                    let _ = lock(&writer).shutdown(std::net::Shutdown::Both);
                });
                lock(handlers).push(handle);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(_) => thread::sleep(POLL),
        }
    }
}

/// Reads exactly `buf.len()` bytes; `Ok(false)` on EOF or stop.
fn read_full(stream: &mut TcpStream, buf: &mut [u8], stop: &AtomicBool) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        if stop.load(Ordering::SeqCst) {
            return Ok(false);
        }
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn sleep_unless_stopped(d: Duration, stop: &AtomicBool) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        thread::sleep(POLL.min(end - Instant::now()));
    }
    true
}

fn word(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn handle_connection(
    mut stream: TcpStream,
    writer: &Mutex<TcpStream>,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
) -> io::Result<()> {
    let send = |dev: u32, id: u32, payload: &[u8]| -> io::Result<()> {
        lock(writer).write_all(&encode_packet(dev, id, payload))
    };
    let mut negotiated = 0u32;
    let mut after_custom_mode: Option<u32> = None;
    loop {
        let mut header = [0u8; HEADER_LEN];
        if !read_full(&mut stream, &mut header, stop)? {
            return Ok(());
        }
        if header[0..4] != MAGIC {
            return Ok(());
        }
        let dev_id = word(&header, 4).unwrap_or(0);
        let id = word(&header, 8).unwrap_or(0);
        let size = word(&header, 12).unwrap_or(0);
        if size > MAX_STUB_PAYLOAD {
            return Ok(());
        }
        let mut payload = vec![0u8; size as usize];
        if !read_full(&mut stream, &mut payload, stop)? {
            return Ok(());
        }
        let (fault, server_version, ack_updates) = {
            let mut s = lock(shared);
            if s.received.len() < MAX_RECORDED {
                s.received.push(RecordedPacket {
                    dev_id,
                    id,
                    payload: payload.clone(),
                });
            }
            (s.fault.clone(), s.protocol_version, s.ack_updates)
        };
        let follows_custom_mode = after_custom_mode.take() == Some(dev_id);
        match id {
            packet_id::REQUEST_PROTOCOL_VERSION => {
                let client = word(&payload, 0).unwrap_or(0);
                negotiated = client.min(server_version);
                if fault != Fault::NoVersionReply {
                    send(0, id, &server_version.to_le_bytes())?;
                }
            }
            packet_id::SET_CLIENT_NAME => {
                let end = payload
                    .iter()
                    .position(|b| *b == 0)
                    .unwrap_or(payload.len());
                let name = String::from_utf8_lossy(&payload[..end]).into_owned();
                lock(shared).client_names.push(name);
            }
            packet_id::REQUEST_CONTROLLER_COUNT => {
                let count = if fault == Fault::HugeControllerCount {
                    1_000_000
                } else {
                    lock(shared).controllers.len() as u32
                };
                send(0, id, &count.to_le_bytes())?;
            }
            packet_id::REQUEST_CONTROLLER_DATA => {
                let version = word(&payload, 0).unwrap_or(negotiated);
                let body = lock(shared)
                    .controllers
                    .get(dev_id as usize)
                    .map(|c| encode_controller(c, version));
                let Some(body) = body else {
                    continue;
                };
                if let Fault::StallAfterCustomMode(d) = fault
                    && follows_custom_mode
                    && !sleep_unless_stopped(d, stop)
                {
                    return Ok(());
                }
                if !send_controller_reply(&mut stream, writer, dev_id, body, &fault, stop)? {
                    return Ok(());
                }
            }
            packet_id::UPDATE_LEDS => {
                if let Some(colors) = parse_colors(&payload, 4) {
                    let mut s = lock(shared);
                    if let Some(c) = s.controllers.get_mut(dev_id as usize)
                        && colors.len() == c.colors.len()
                    {
                        c.colors = colors;
                    }
                }
                if ack_updates {
                    send(dev_id, packet_id::ACK, &ack(id))?;
                }
            }
            packet_id::UPDATE_ZONE_LEDS => {
                let zone = word(&payload, 4).unwrap_or(u32::MAX) as usize;
                if let Some(colors) = parse_colors(&payload, 8) {
                    let mut s = lock(shared);
                    if let Some(c) = s.controllers.get_mut(dev_id as usize)
                        && let Some(offset) = c.zone_led_offset(zone)
                        && offset + colors.len() <= c.colors.len()
                    {
                        c.colors[offset..offset + colors.len()].copy_from_slice(&colors);
                    }
                }
                if ack_updates {
                    send(dev_id, packet_id::ACK, &ack(id))?;
                }
            }
            packet_id::SET_CUSTOM_MODE => {
                after_custom_mode = Some(dev_id);
                let mut s = lock(shared);
                if let Some(c) = s.controllers.get_mut(dev_id as usize) {
                    let pick = ["direct", "custom", "static"].iter().find_map(|want| {
                        c.modes
                            .iter()
                            .position(|m| m.name.eq_ignore_ascii_case(want))
                    });
                    if let Some(idx) = pick {
                        c.active_mode = idx as i32;
                    }
                }
            }
            packet_id::UPDATE_MODE => {
                if let Some(idx) = word(&payload, 4) {
                    let mut s = lock(shared);
                    if let Some(c) = s.controllers.get_mut(dev_id as usize)
                        && (idx as usize) < c.modes.len()
                    {
                        c.active_mode = idx as i32;
                    }
                }
                if ack_updates {
                    send(dev_id, packet_id::ACK, &ack(id))?;
                }
            }
            _ => {}
        }
    }
}

fn ack(id: u32) -> Vec<u8> {
    let mut v = id.to_le_bytes().to_vec();
    v.extend_from_slice(&0u32.to_le_bytes());
    v
}

/// Colours of an UpdateLEDs (`offset` 4) or UpdateZoneLEDs (`offset` 8)
/// payload.
fn parse_colors(payload: &[u8], offset: usize) -> Option<Vec<Rgb>> {
    let n = payload.get(offset..offset + 2)?;
    let n = usize::from(u16::from_le_bytes([n[0], n[1]]));
    let body = payload.get(offset + 2..offset + 2 + n * 4)?;
    Some(
        body.chunks_exact(4)
            .map(|c| Rgb::new(c[0], c[1], c[2]))
            .collect(),
    )
}

/// Sends one controller-data reply with the configured fault. `Ok(false)`
/// means the connection must close.
fn send_controller_reply(
    stream: &mut TcpStream,
    writer: &Mutex<TcpStream>,
    dev_id: u32,
    body: Vec<u8>,
    fault: &Fault,
    stop: &AtomicBool,
) -> io::Result<bool> {
    let id = packet_id::REQUEST_CONTROLLER_DATA;
    match fault {
        Fault::BadMagic => {
            let mut p = encode_packet(dev_id, id, &body);
            p[0..4].copy_from_slice(b"OGRB");
            lock(writer).write_all(&p)?;
        }
        Fault::OversizedHeader => {
            let mut p = encode_packet(dev_id, id, &[]);
            p[12..16].copy_from_slice(&0x7fff_fff0u32.to_le_bytes());
            lock(writer).write_all(&p)?;
        }
        Fault::TruncatedThenClose => {
            let p = encode_packet(dev_id, id, &body);
            let cut = HEADER_LEN + body.len() / 2;
            lock(writer).write_all(&p[..cut])?;
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return Ok(false);
        }
        Fault::InnerLengthOverflow => {
            let mut b = Vec::new();
            b.extend_from_slice(&64u32.to_le_bytes());
            b.extend_from_slice(&4i32.to_le_bytes());
            b.extend_from_slice(&60_000u16.to_le_bytes());
            b.extend_from_slice(b"abc\0");
            lock(writer).write_all(&encode_packet(dev_id, id, &b))?;
        }
        Fault::Garbage => {
            let mut x: u32 = 0x1234_5678 ^ dev_id;
            let junk: Vec<u8> = (0..300)
                .map(|_| {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (x >> 24) as u8
                })
                .collect();
            lock(writer).write_all(&encode_packet(dev_id, id, &junk))?;
        }
        Fault::Stall(d) => {
            if !sleep_unless_stopped(*d, stop) {
                return Ok(false);
            }
            lock(writer).write_all(&encode_packet(dev_id, id, &body))?;
        }
        Fault::DripFeed(d) => {
            for byte in encode_packet(dev_id, id, &body) {
                if !sleep_unless_stopped(*d, stop) {
                    return Ok(false);
                }
                lock(writer).write_all(&[byte])?;
            }
        }
        Fault::None
        | Fault::NoVersionReply
        | Fault::HugeControllerCount
        | Fault::StallAfterCustomMode(_) => {
            lock(writer).write_all(&encode_packet(dev_id, id, &body))?;
        }
    }
    Ok(true)
}
