// SPDX-License-Identifier: GPL-3.0-or-later
//! Controller data model and its version-dependent codec (protocol 1..=5).

use std::fmt;

use crate::wire::{Reader, Writer};
use crate::{Error, Result, limits};

/// One LED colour. On the wire: R, G, B, then one padding byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parses `RRGGBB` or `#RRGGBB`.
    pub fn parse_hex(text: &str) -> Option<Self> {
        let hex = text.strip_prefix('#').unwrap_or(text);
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(hex, 16).ok()?;
        Some(Self::new((v >> 16) as u8, (v >> 8) as u8, v as u8))
    }

    fn read(r: &mut Reader<'_>) -> Result<Self> {
        let b = r.take(4, "colour")?;
        Ok(Self::new(b[0], b[1], b[2]))
    }

    pub(crate) fn write(self, w: &mut Writer) {
        w.buf.extend_from_slice(&[self.r, self.g, self.b, 0]);
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// Server-assigned controller handle. For protocol 1..=5 this is the index
/// in the server's list and is invalidated by a device-list update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ControllerId(u32);

impl ControllerId {
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    pub const fn index(self) -> u32 {
        self.0
    }
}

impl fmt::Display for ControllerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable identity of a physical device across re-enumerations (indexes
/// are not stable). Used to restore saved modes and for per-device settings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceKey {
    pub name: String,
    pub vendor: String,
    pub serial: String,
    pub location: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mode {
    pub name: String,
    /// Opaque server value; echoed back unchanged in `UpdateMode`.
    pub value: i32,
    pub flags: u32,
    pub speed_min: u32,
    pub speed_max: u32,
    /// Protocol 3+; zero below.
    pub brightness_min: u32,
    pub brightness_max: u32,
    pub colors_min: u32,
    pub colors_max: u32,
    pub speed: u32,
    pub brightness: u32,
    pub direction: u32,
    pub color_mode: u32,
    pub colors: Vec<Rgb>,
}

/// Lower bound of one encoded mode, for count-vs-payload checks.
const MODE_MIN_BYTES: usize = 2 + 9 * 4 + 2;

impl Mode {
    /// True for OpenRGB's per-LED `Direct` mode — the mode the server picks
    /// first for `SetCustomMode` (it selects by this name). It has no stored
    /// lighting of its own: what it shows is the colour array last sent, so
    /// remembering it as a "previous" mode also needs those colours.
    pub fn is_direct(&self) -> bool {
        self.name.eq_ignore_ascii_case("direct")
    }

    fn read(r: &mut Reader<'_>, version: u32) -> Result<Self> {
        let name = r.string("mode name")?;
        let value = r.i32("mode value")?;
        let flags = r.u32("mode flags")?;
        let speed_min = r.u32("mode speed")?;
        let speed_max = r.u32("mode speed")?;
        let (brightness_min, brightness_max) = if version >= 3 {
            (r.u32("mode brightness")?, r.u32("mode brightness")?)
        } else {
            (0, 0)
        };
        let colors_min = r.u32("mode colours")?;
        let colors_max = r.u32("mode colours")?;
        let speed = r.u32("mode speed")?;
        let brightness = if version >= 3 {
            r.u32("mode brightness")?
        } else {
            0
        };
        let direction = r.u32("mode direction")?;
        let color_mode = r.u32("mode colour mode")?;
        let n = r.count("mode colour count", limits::MAX_MODE_COLORS, 4)?;
        let mut colors = Vec::with_capacity(n);
        for _ in 0..n {
            colors.push(Rgb::read(r)?);
        }
        Ok(Self {
            name,
            value,
            flags,
            speed_min,
            speed_max,
            brightness_min,
            brightness_max,
            colors_min,
            colors_max,
            speed,
            brightness,
            direction,
            color_mode,
            colors,
        })
    }

    pub(crate) fn write(&self, w: &mut Writer, version: u32) {
        w.string(&self.name);
        w.i32(self.value);
        w.u32(self.flags);
        w.u32(self.speed_min);
        w.u32(self.speed_max);
        if version >= 3 {
            w.u32(self.brightness_min);
            w.u32(self.brightness_max);
        }
        w.u32(self.colors_min);
        w.u32(self.colors_max);
        w.u32(self.speed);
        if version >= 3 {
            w.u32(self.brightness);
        }
        w.u32(self.direction);
        w.u32(self.color_mode);
        let n = self.colors.len().min(usize::from(limits::MAX_MODE_COLORS));
        w.u16(n as u16);
        for c in &self.colors[..n] {
            c.write(w);
        }
    }
}

/// Zone type values as reported by the server.
pub mod zone_type {
    pub const SINGLE: i32 = 0;
    pub const LINEAR: i32 = 1;
    pub const MATRIX: i32 = 2;
}

/// Row-major map of LED indexes; `0xFFFF_FFFF` marks an empty cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixMap {
    pub height: u32,
    pub width: u32,
    pub cells: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub name: String,
    pub kind: i32,
    pub start: u32,
    pub leds_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    pub name: String,
    /// See [`zone_type`].
    pub kind: i32,
    pub leds_min: u32,
    pub leds_max: u32,
    pub leds_count: u32,
    pub matrix: Option<MatrixMap>,
    /// Protocol 4+.
    pub segments: Vec<Segment>,
    /// Protocol 5+.
    pub flags: u32,
}

const ZONE_MIN_BYTES: usize = 2 + 4 * 4 + 2;

impl Zone {
    fn read(r: &mut Reader<'_>, version: u32) -> Result<Self> {
        let name = r.string("zone name")?;
        let kind = r.i32("zone type")?;
        let leds_min = r.u32("zone leds")?;
        let leds_max = r.u32("zone leds")?;
        let leds_count = r.u32("zone leds")?;
        let matrix_len = usize::from(r.u16("zone matrix length")?);
        let matrix = if matrix_len == 0 {
            None
        } else {
            let body = r.take(matrix_len, "zone matrix")?;
            Some(parse_matrix(body)?)
        };
        let mut segments = Vec::new();
        if version >= 4 {
            let n = r.count("segment count", limits::MAX_SEGMENTS, 2 + 12)?;
            segments.reserve(n);
            for _ in 0..n {
                segments.push(Segment {
                    name: r.string("segment name")?,
                    kind: r.i32("segment type")?,
                    start: r.u32("segment start")?,
                    leds_count: r.u32("segment leds")?,
                });
            }
        }
        let flags = if version >= 5 {
            r.u32("zone flags")?
        } else {
            0
        };
        Ok(Self {
            name,
            kind,
            leds_min,
            leds_max,
            leds_count,
            matrix,
            segments,
            flags,
        })
    }

    #[cfg(any(test, feature = "stub"))]
    fn write(&self, w: &mut Writer, version: u32) {
        w.string(&self.name);
        w.i32(self.kind);
        w.u32(self.leds_min);
        w.u32(self.leds_max);
        w.u32(self.leds_count);
        match &self.matrix {
            None => w.u16(0),
            Some(m) => {
                w.u16((8 + 4 * m.cells.len()) as u16);
                w.u32(m.height);
                w.u32(m.width);
                for c in &m.cells {
                    w.u32(*c);
                }
            }
        }
        if version >= 4 {
            w.u16(self.segments.len() as u16);
            for s in &self.segments {
                w.string(&s.name);
                w.i32(s.kind);
                w.u32(s.start);
                w.u32(s.leds_count);
            }
        }
        if version >= 5 {
            w.u32(self.flags);
        }
    }
}

fn parse_matrix(body: &[u8]) -> Result<MatrixMap> {
    let mut r = Reader::new(body);
    let height = r.u32("zone matrix height")?;
    let width = r.u32("zone matrix width")?;
    let cells = height.checked_mul(width).ok_or(Error::Malformed {
        what: "zone matrix size",
    })?;
    if cells > limits::MAX_MATRIX_CELLS {
        return Err(Error::LimitExceeded {
            what: "zone matrix cells",
            value: u64::from(cells),
            max: u64::from(limits::MAX_MATRIX_CELLS),
        });
    }
    if r.remaining() != cells as usize * 4 {
        return Err(Error::Malformed {
            what: "zone matrix size",
        });
    }
    let mut out = Vec::with_capacity(cells as usize);
    for _ in 0..cells {
        out.push(r.u32("zone matrix cell")?);
    }
    Ok(MatrixMap {
        height,
        width,
        cells: out,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Led {
    pub name: String,
    pub value: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Controller {
    pub id: ControllerId,
    /// Device type as reported by the server.
    pub kind: i32,
    pub name: String,
    /// Protocol 1+.
    pub vendor: String,
    pub description: String,
    pub version: String,
    pub serial: String,
    pub location: String,
    pub modes: Vec<Mode>,
    /// Raw server value; may be out of range on a broken server — use
    /// [`Controller::active_mode`].
    pub active_mode: i32,
    pub zones: Vec<Zone>,
    pub leds: Vec<Led>,
    pub colors: Vec<Rgb>,
    /// Protocol 5+.
    pub flags: u32,
}

impl Controller {
    pub fn key(&self) -> DeviceKey {
        DeviceKey {
            name: self.name.clone(),
            vendor: self.vendor.clone(),
            serial: self.serial.clone(),
            location: self.location.clone(),
        }
    }

    /// The active mode, if the server's index is in range.
    pub fn active_mode(&self) -> Option<(usize, &Mode)> {
        let idx = usize::try_from(self.active_mode).ok()?;
        self.modes.get(idx).map(|m| (idx, m))
    }

    /// Estimated heap + inline size of this parsed controller, counted
    /// against [`crate::limits::MAX_ENUMERATION_BYTES`]. Deliberately an
    /// over-estimate of what is retained (strings at their length, every
    /// element at its in-memory size).
    pub fn approx_heap_bytes(&self) -> usize {
        use std::mem::size_of;
        let strings = [
            &self.name,
            &self.vendor,
            &self.description,
            &self.version,
            &self.serial,
            &self.location,
        ]
        .iter()
        .map(|s| s.len())
        .sum::<usize>();
        let modes: usize = self
            .modes
            .iter()
            .map(|m| size_of::<Mode>() + m.name.len() + m.colors.len() * size_of::<Rgb>())
            .sum();
        let zones: usize = self
            .zones
            .iter()
            .map(|z| {
                size_of::<Zone>()
                    + z.name.len()
                    + z.matrix.as_ref().map_or(0, |m| m.cells.len() * 4)
                    + z.segments
                        .iter()
                        .map(|s| size_of::<Segment>() + s.name.len())
                        .sum::<usize>()
            })
            .sum();
        let leds: usize = self
            .leds
            .iter()
            .map(|l| size_of::<Led>() + l.name.len())
            .sum();
        size_of::<Controller>()
            + strings
            + modes
            + zones
            + leds
            + self.colors.len() * size_of::<Rgb>()
    }

    /// Offset of `zone`'s first LED in the controller-wide LED array.
    pub fn zone_led_offset(&self, zone: usize) -> Option<usize> {
        if zone >= self.zones.len() {
            return None;
        }
        self.zones[..zone]
            .iter()
            .try_fold(0usize, |acc, z| acc.checked_add(z.leds_count as usize))
    }
}

/// Parses a `REQUEST_CONTROLLER_DATA` reply payload (starting at its
/// `data_size` field) encoded at `version`. Trailing bytes are ignored.
pub(crate) fn parse_controller(
    payload: &[u8],
    version: u32,
    id: ControllerId,
) -> Result<Controller> {
    let mut r = Reader::new(payload);
    let data_size = r.u32("controller data size")? as usize;
    if data_size > payload.len() {
        return Err(Error::Malformed {
            what: "controller data size",
        });
    }
    let kind = r.i32("controller type")?;
    let name = r.string("controller name")?;
    let vendor = if version >= 1 {
        r.string("controller vendor")?
    } else {
        String::new()
    };
    let description = r.string("controller description")?;
    let version_text = r.string("controller version")?;
    let serial = r.string("controller serial")?;
    let location = r.string("controller location")?;

    let n_modes = r.count("mode count", limits::MAX_MODES, MODE_MIN_BYTES)?;
    let active_mode = r.i32("active mode")?;
    let mut modes = Vec::with_capacity(n_modes);
    for _ in 0..n_modes {
        modes.push(Mode::read(&mut r, version)?);
    }

    let n_zones = r.count("zone count", limits::MAX_ZONES, ZONE_MIN_BYTES)?;
    let mut zones = Vec::with_capacity(n_zones);
    for _ in 0..n_zones {
        zones.push(Zone::read(&mut r, version)?);
    }

    let n_leds = r.count("LED count", limits::MAX_LEDS, 2 + 4)?;
    let mut leds = Vec::with_capacity(n_leds);
    for _ in 0..n_leds {
        leds.push(Led {
            name: r.string("LED name")?,
            value: r.u32("LED value")?,
        });
    }

    let n_colors = r.count("colour count", limits::MAX_LEDS, 4)?;
    let mut colors = Vec::with_capacity(n_colors);
    for _ in 0..n_colors {
        colors.push(Rgb::read(&mut r)?);
    }

    let mut flags = 0;
    if version >= 5 {
        let n_names = r.count("LED display name count", limits::MAX_LEDS, 2)?;
        for _ in 0..n_names {
            let len = r.u16("LED display name")?;
            if len > limits::MAX_STRING_BYTES {
                return Err(Error::LimitExceeded {
                    what: "string length",
                    value: u64::from(len),
                    max: u64::from(limits::MAX_STRING_BYTES),
                });
            }
            r.take(usize::from(len), "LED display name")?;
        }
        flags = r.u32("controller flags")?;
    }

    Ok(Controller {
        id,
        kind,
        name,
        vendor,
        description,
        version: version_text,
        serial,
        location,
        modes,
        active_mode,
        zones,
        leds,
        colors,
        flags,
    })
}

/// Server-side encoding of a controller (stub server and round-trip tests).
#[cfg(any(test, feature = "stub"))]
pub(crate) fn encode_controller(c: &Controller, version: u32) -> Vec<u8> {
    let mut w = Writer::default();
    w.i32(c.kind);
    w.string(&c.name);
    if version >= 1 {
        w.string(&c.vendor);
    }
    w.string(&c.description);
    w.string(&c.version);
    w.string(&c.serial);
    w.string(&c.location);
    w.u16(c.modes.len() as u16);
    w.i32(c.active_mode);
    for m in &c.modes {
        m.write(&mut w, version);
    }
    w.u16(c.zones.len() as u16);
    for z in &c.zones {
        z.write(&mut w, version);
    }
    w.u16(c.leds.len() as u16);
    for l in &c.leds {
        w.string(&l.name);
        w.u32(l.value);
    }
    w.u16(c.colors.len() as u16);
    for col in &c.colors {
        col.write(&mut w);
    }
    if version >= 5 {
        w.u16(0);
        w.u32(c.flags);
    }
    w.finish_sized()
}
