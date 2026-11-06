// SPDX-License-Identifier: GPL-3.0-or-later
//! Error taxonomy (documented in `docs/OPENRGB_CLIENT.md`).

use std::io;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum Error {
    /// Nothing listens on the loopback port (connection refused). The normal
    /// state on most machines: report quietly, re-probe slowly.
    #[error("OpenRGB SDK server is not running on 127.0.0.1:{port}")]
    ServerAbsent { port: u16 },
    /// A deadline expired. The connection is unusable afterwards.
    #[error("timed out during {op}")]
    Timeout { op: &'static str },
    /// The server closed or reset the connection, or an earlier transport
    /// error made the connection unusable.
    #[error("connection to the OpenRGB SDK server was lost")]
    Disconnected,
    #[error("i/o error during {op}: {kind}")]
    Io {
        op: &'static str,
        kind: io::ErrorKind,
    },
    #[error("packet does not start with the ORGB magic")]
    BadMagic,
    #[error("packet of {size} bytes exceeds the {max}-byte limit")]
    PacketTooLarge { size: u32, max: u32 },
    /// A packet was framed correctly but its contents do not parse
    /// (truncated field, count larger than the payload, bad matrix size).
    #[error("malformed {what}")]
    Malformed { what: &'static str },
    #[error("{what} {value} exceeds the limit of {max}")]
    LimitExceeded {
        what: &'static str,
        value: u64,
        max: u64,
    },
    #[error("server protocol version {server} is outside the supported range {min}..={max}")]
    UnsupportedVersion { server: u32, min: u32, max: u32 },
    #[error("server sent more than {max} unrelated packets while a reply was expected")]
    TooManyUnsolicited { max: usize },
    /// The server announced a device-list change; controller ids are stale
    /// until [`crate::Client::refresh`] succeeds.
    #[error("OpenRGB device list changed; re-enumerate before using controller ids")]
    DeviceListChanged,
    #[error("controller {0} is not in the current device list")]
    UnknownController(u32),
    /// A device whose mode was saved is no longer present (restore only).
    #[error("device is no longer present")]
    DeviceGone,
    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),
}

impl Error {
    /// The quiet, expected "no SDK server" outcome.
    pub fn is_server_absent(&self) -> bool {
        matches!(self, Error::ServerAbsent { .. })
    }

    /// True when the connection must be dropped and re-established.
    pub fn is_connection_lost(&self) -> bool {
        matches!(
            self,
            Error::Timeout { .. }
                | Error::Disconnected
                | Error::Io { .. }
                | Error::BadMagic
                | Error::PacketTooLarge { .. }
                | Error::TooManyUnsolicited { .. }
        )
    }

    pub(crate) fn from_io(op: &'static str, err: &io::Error) -> Self {
        match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Error::Timeout { op },
            io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected => Error::Disconnected,
            kind => Error::Io { op, kind },
        }
    }
}
