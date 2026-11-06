// SPDX-License-Identifier: GPL-3.0-or-later
//! Packet framing and bounded primitive codecs.
//!
//! Every packet is a 16-byte header — magic `ORGB`, device id, packet id,
//! payload size, each `u32` — followed by the payload. All integers are
//! little-endian (the byte order every OpenRGB server build uses in
//! practice; the published description does not state it explicitly).

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::{Error, Result, limits};

pub const MAGIC: [u8; 4] = *b"ORGB";
pub const HEADER_LEN: usize = 16;

/// Packet ids used by this client (subset of the published table).
pub mod packet_id {
    pub const REQUEST_CONTROLLER_COUNT: u32 = 0;
    pub const REQUEST_CONTROLLER_DATA: u32 = 1;
    pub const ACK: u32 = 10;
    pub const REQUEST_PROTOCOL_VERSION: u32 = 40;
    pub const SET_CLIENT_NAME: u32 = 50;
    pub const DEVICE_LIST_UPDATED: u32 = 100;
    pub const UPDATE_LEDS: u32 = 1050;
    pub const UPDATE_ZONE_LEDS: u32 = 1051;
    pub const SET_CUSTOM_MODE: u32 = 1100;
    pub const UPDATE_MODE: u32 = 1101;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub dev_id: u32,
    pub id: u32,
    pub payload: Vec<u8>,
}

/// Encodes one complete packet (header + payload).
pub fn encode_packet(dev_id: u32, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&dev_id.to_le_bytes());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Parsed header: (dev_id, id, size). Validates magic and the size cap.
pub fn decode_header(header: &[u8; HEADER_LEN]) -> Result<(u32, u32, u32)> {
    if header[0..4] != MAGIC {
        return Err(Error::BadMagic);
    }
    let word =
        |i: usize| u32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
    let size = word(12);
    if size > limits::MAX_PACKET_BYTES {
        return Err(Error::PacketTooLarge {
            size,
            max: limits::MAX_PACKET_BYTES,
        });
    }
    Ok((word(4), word(8), size))
}

/// Reads exactly `buf.len()` bytes before `deadline`. Each `read` call gets
/// only the remaining time, so a server that drips one byte at a time still
/// hits the deadline.
fn read_exact_by(stream: &mut TcpStream, buf: &mut [u8], deadline: Instant) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::Timeout { op: "read" });
        }
        stream
            .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
            .map_err(|e| Error::from_io("read", &e))?;
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Err(Error::Disconnected),
            Ok(n) => filled += n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(Error::from_io("read", &e)),
        }
    }
    Ok(())
}

/// Reads one packet. The payload buffer grows in 64 KiB steps only as bytes
/// arrive, so a lying size field cannot force a large allocation up front.
pub(crate) fn read_packet(stream: &mut TcpStream, deadline: Instant) -> Result<Packet> {
    const CHUNK: usize = 64 * 1024;
    let mut header = [0u8; HEADER_LEN];
    read_exact_by(stream, &mut header, deadline)?;
    let (dev_id, id, size) = decode_header(&header)?;
    let size = size as usize;
    let mut payload = Vec::with_capacity(size.min(CHUNK));
    while payload.len() < size {
        let start = payload.len();
        let step = (size - start).min(CHUNK);
        payload.resize(start + step, 0);
        read_exact_by(stream, &mut payload[start..], deadline)?;
    }
    Ok(Packet {
        dev_id,
        id,
        payload,
    })
}

pub(crate) fn write_packet(
    stream: &mut TcpStream,
    dev_id: u32,
    id: u32,
    payload: &[u8],
    timeout: Duration,
) -> Result<()> {
    if payload.len() > limits::MAX_PACKET_BYTES as usize {
        return Err(Error::InvalidArgument(
            "outgoing packet exceeds the size limit",
        ));
    }
    let bytes = encode_packet(dev_id, id, payload);
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| Error::from_io("write", &e))?;
    stream
        .write_all(&bytes)
        .map_err(|e| Error::from_io("write", &e))?;
    Ok(())
}

/// Bounds-checked little-endian reader over one payload.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(Error::Malformed { what });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub(crate) fn u16(&mut self, what: &'static str) -> Result<u16> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self, what: &'static str) -> Result<u32> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn i32(&mut self, what: &'static str) -> Result<i32> {
        Ok(self.u32(what)? as i32)
    }

    /// `u16` length (NUL included) + bytes. Text after the first NUL is
    /// dropped; invalid UTF-8 is replaced, never rejected.
    pub(crate) fn string(&mut self, what: &'static str) -> Result<String> {
        let len = self.u16(what)?;
        if len > limits::MAX_STRING_BYTES {
            return Err(Error::LimitExceeded {
                what: "string length",
                value: u64::from(len),
                max: u64::from(limits::MAX_STRING_BYTES),
            });
        }
        let bytes = self.take(usize::from(len), what)?;
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        Ok(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }

    /// Reads a `u16` element count, rejecting counts above `max` and counts
    /// that cannot fit in the rest of the payload at `min_elem_bytes` each.
    pub(crate) fn count(
        &mut self,
        what: &'static str,
        max: u16,
        min_elem_bytes: usize,
    ) -> Result<usize> {
        let n = self.u16(what)?;
        if n > max {
            return Err(Error::LimitExceeded {
                what,
                value: u64::from(n),
                max: u64::from(max),
            });
        }
        let n = usize::from(n);
        if n.saturating_mul(min_elem_bytes) > self.remaining() {
            return Err(Error::Malformed { what });
        }
        Ok(n)
    }
}

/// Little-endian payload builder.
#[derive(Default)]
pub(crate) struct Writer {
    pub(crate) buf: Vec<u8>,
}

impl Writer {
    pub(crate) fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// `u16` length including NUL, bytes, NUL. Callers keep strings short.
    pub(crate) fn string(&mut self, s: &str) {
        let bytes = s.as_bytes();
        let len = bytes.len().min(usize::from(u16::MAX) - 1);
        self.u16((len + 1) as u16);
        self.buf.extend_from_slice(&bytes[..len]);
        self.buf.push(0);
    }

    /// Prefixes the buffer with a `u32` total size that includes itself, as
    /// the protocol's `data_size` fields do.
    pub(crate) fn finish_sized(self) -> Vec<u8> {
        let total = (self.buf.len() + 4) as u32;
        let mut out = Vec::with_capacity(self.buf.len() + 4);
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(&self.buf);
        out
    }
}
