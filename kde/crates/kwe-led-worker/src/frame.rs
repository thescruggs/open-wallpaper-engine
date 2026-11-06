// SPDX-License-Identifier: GPL-3.0-or-later
//! Ambient source: samples a renderer's frame file (frame protocol v1)
//! read-only with the seqlock discipline, using positioned reads only.
//!
//! Open-per-tick, no mapping (see `docs/LED_PROTOCOL_V1.md` §Ambient):
//! * The file is opened, sampled, and closed inside one call, so the worker
//!   never holds a descriptor between ticks. When the daemon unlinks a
//!   replaced or quarantined renderer's file, the worker's next tick sees
//!   `Absent` (or the new path) and the inode is freed immediately; a held
//!   descriptor would pin up to 512 MiB of tmpfs and keep reading a dead
//!   frame.
//! * No `mmap`: a renderer truncating its file cannot deliver `SIGBUS` to
//!   this process (the same reason the Plasma consumer uses `pread`). A
//!   concurrent truncation shows up as a short read → `Invalid`.
//! * Cost per tick: one `open`, one `fstat`, a 64-byte header read, one row
//!   read per grid row (≤ 36 rows × width × 4 bytes), one 8-byte generation
//!   re-read, one `close`. When the generation has not moved since the last
//!   accepted snapshot, only the header is read.
//!
//! The generation is read with a positioned read rather than an atomic load
//! (exactly like the Plasma consumer); a torn generation read can only make
//! the two reads differ, which is a retry, never an accepted torn frame.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use kwe_frame_protocol::{
    FrameSpec, HEADER_BYTES, MAGIC, MAX_MAPPING_BYTES, PIXEL_FORMAT_BGRA8888_PREMULTIPLIED,
    SLOT_COUNT, VERSION,
};

use crate::color::Rgbf;
use crate::mapping::{Grid, grid_dims};

/// Seqlock attempts per tick before reporting `Busy`.
pub const MAX_ATTEMPTS: usize = 4;
const OFFSET_GENERATION: u64 = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sample {
    /// A new stable snapshot was sampled into the grid.
    Fresh,
    /// Same file, same generation as the last accepted snapshot.
    Unchanged,
    /// The path does not exist.
    Absent,
    /// The file is not a valid, complete frame file.
    Invalid(&'static str),
    /// The writer was mid-publish on every attempt, or has not published
    /// its first frame yet.
    Busy,
}

#[derive(Debug, Default)]
pub struct FrameSampler {
    identity: Option<(u64, u64)>,
    generation: Option<u64>,
    grid: Option<Grid>,
    size: Option<(u32, u32)>,
    row: Vec<u8>,
}

struct Header {
    spec: FrameSpec,
    generation: u64,
    slot: u32,
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

fn parse_header(b: &[u8; HEADER_BYTES], file_len: u64) -> Result<Header, &'static str> {
    if b[0..8] != MAGIC {
        return Err("magic");
    }
    if u32_at(b, 8) != VERSION {
        return Err("version");
    }
    if u32_at(b, 12) != HEADER_BYTES as u32 {
        return Err("header size");
    }
    if u32_at(b, 36) != PIXEL_FORMAT_BGRA8888_PREMULTIPLIED {
        return Err("pixel format");
    }
    if u32_at(b, 40) != SLOT_COUNT {
        return Err("slot count");
    }
    let spec = FrameSpec::new(u32_at(b, 24), u32_at(b, 28)).map_err(|_| "dimensions")?;
    if u32_at(b, 32) != spec.stride {
        return Err("stride");
    }
    if u64_at(b, 16) != spec.file_bytes || file_len != spec.file_bytes {
        return Err("file size");
    }
    let slot = u32_at(b, 56);
    if slot >= SLOT_COUNT {
        return Err("active slot");
    }
    Ok(Header {
        spec,
        generation: u64_at(b, 48),
        slot,
    })
}

fn read_at(file: &File, buf: &mut [u8], offset: u64) -> Result<(), Sample> {
    file.read_exact_at(buf, offset).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Sample::Invalid("truncated")
        } else {
            Sample::Invalid("read")
        }
    })
}

impl FrameSampler {
    /// The grid of the last accepted snapshot (kept across failures so the
    /// caller can hold it).
    pub fn grid(&self) -> Option<&Grid> {
        self.grid.as_ref()
    }

    /// Frame dimensions of the last accepted snapshot.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.size
    }

    /// Producer sequence (generation / 2) of the last accepted snapshot.
    pub fn sequence(&self) -> Option<u64> {
        self.generation.map(|g| g / 2)
    }

    /// Forgets the source identity (frame path changed or cleared) but
    /// keeps the last grid, so the caller's hold policy can bridge a
    /// wallpaper switch without blinking dark.
    pub fn forget_source(&mut self) {
        self.identity = None;
        self.generation = None;
    }

    /// Opens `path`, takes at most one stable sample, closes it.
    pub fn read(&mut self, path: &Path) -> Sample {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Sample::Absent,
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                return Sample::Invalid("symlink");
            }
            Err(_) => return Sample::Invalid("open"),
        };
        let meta = match file.metadata() {
            Ok(m) => m,
            Err(_) => return Sample::Invalid("stat"),
        };
        if !meta.is_file() {
            return Sample::Invalid("not a regular file");
        }
        if meta.len() < HEADER_BYTES as u64 {
            return Sample::Invalid("truncated");
        }
        if meta.len() > MAX_MAPPING_BYTES {
            return Sample::Invalid("file size");
        }
        let identity = (meta.dev(), meta.ino());
        if self.identity != Some(identity) {
            // Replaced file (new renderer): never compare generations
            // across inodes.
            self.identity = Some(identity);
            self.generation = None;
        }
        match self.sample(&file, meta.len()) {
            Ok(s) | Err(s) => s,
        }
    }

    fn sample(&mut self, file: &File, file_len: u64) -> Result<Sample, Sample> {
        let mut header = [0u8; HEADER_BYTES];
        for _ in 0..MAX_ATTEMPTS {
            read_at(file, &mut header, 0)?;
            let h = parse_header(&header, file_len).map_err(Sample::Invalid)?;
            if h.generation == 0 {
                return Ok(Sample::Busy);
            }
            if !h.generation.is_multiple_of(2) {
                std::thread::yield_now();
                continue;
            }
            if self.generation == Some(h.generation) && self.grid.is_some() {
                return Ok(Sample::Unchanged);
            }
            let (w, hgt) = (h.spec.width as usize, h.spec.height as usize);
            let (gw, gh) = grid_dims(h.spec.width, h.spec.height);
            let stride = h.spec.stride as usize;
            let slot_base = HEADER_BYTES as u64 + h.spec.slot_bytes * u64::from(h.slot);
            self.row.resize(stride, 0);
            let mut cells = Vec::with_capacity(gw * gh);
            for gy in 0..gh {
                let y = (2 * gy + 1) * hgt / (2 * gh);
                read_at(file, &mut self.row, slot_base + (y * stride) as u64)?;
                for gx in 0..gw {
                    let x = (2 * gx + 1) * w / (2 * gw);
                    let px = &self.row[x * 4..x * 4 + 4];
                    // BGRA, premultiplied: the premultiplied RGB is the
                    // colour composited over black, which is what a lit
                    // LED should show.
                    cells.push(Rgbf::from_u8(px[2], px[1], px[0]));
                }
            }
            let mut after = [0u8; 8];
            read_at(file, &mut after, OFFSET_GENERATION)?;
            if u64::from_le_bytes(after) != h.generation {
                std::thread::yield_now();
                continue;
            }
            let Some(grid) = Grid::new(gw, gh, cells) else {
                return Err(Sample::Invalid("grid"));
            };
            self.grid = Some(grid);
            self.generation = Some(h.generation);
            self.size = Some((h.spec.width, h.spec.height));
            return Ok(Sample::Fresh);
        }
        Ok(Sample::Busy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwe_frame_protocol::{SharedFrameWriter, VERSION};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kwe-led-frame-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bgra(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = f(x, y);
                v.extend_from_slice(&[b, g, r, 255]);
            }
        }
        v
    }

    #[test]
    fn samples_stable_frames_and_detects_unchanged() {
        let dir = temp_dir("stable");
        let path = dir.join("f.bin");
        let spec = FrameSpec::new(100, 50).unwrap();
        let mut w = SharedFrameWriter::create(&path, spec).unwrap();
        let mut s = FrameSampler::default();
        assert_eq!(s.read(&path), Sample::Busy, "nothing published yet");
        w.publish(&bgra(100, 50, |x, _| {
            if x < 50 { [255, 0, 0] } else { [0, 0, 255] }
        }))
        .unwrap();
        assert_eq!(s.read(&path), Sample::Fresh);
        let g = s.grid().unwrap();
        assert_eq!((g.width(), g.height()), (64, 36));
        assert_eq!(g.cells()[0], Rgbf::new(1.0, 0.0, 0.0));
        assert_eq!(g.cells()[63], Rgbf::new(0.0, 0.0, 1.0));
        assert_eq!(s.read(&path), Sample::Unchanged);
        assert_eq!(s.sequence(), Some(1));
        assert_eq!(s.size(), Some((100, 50)));
        w.publish(&bgra(100, 50, |_, _| [0, 255, 0])).unwrap();
        assert_eq!(s.read(&path), Sample::Fresh);
        assert_eq!(s.grid().unwrap().cells()[0], Rgbf::new(0.0, 1.0, 0.0));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn absent_replaced_truncated_and_hostile_files() {
        let dir = temp_dir("hostile");
        let path = dir.join("f.bin");
        let mut s = FrameSampler::default();
        assert_eq!(s.read(&path), Sample::Absent);

        let spec = FrameSpec::new(4, 4).unwrap();
        let mut w = SharedFrameWriter::create(&path, spec).unwrap();
        w.publish(&bgra(4, 4, |_, _| [9, 9, 9])).unwrap();
        assert_eq!(s.read(&path), Sample::Fresh);
        // Replaced by a new file with a lower generation: still Fresh.
        drop(w);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(s.read(&path), Sample::Absent);
        assert!(s.grid().is_some(), "last grid kept for the hold policy");
        let mut w = SharedFrameWriter::create(&path, spec).unwrap();
        w.publish(&bgra(4, 4, |_, _| [200, 0, 0])).unwrap();
        assert_eq!(s.read(&path), Sample::Fresh);
        assert_eq!(s.grid().unwrap().cells()[0].r, 200.0 / 255.0);

        // Truncated mid-file.
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(64 + 10).unwrap();
        assert_eq!(s.read(&path), Sample::Invalid("file size"));
        f.set_len(10).unwrap();
        assert_eq!(s.read(&path), Sample::Invalid("truncated"));
        drop(w);

        // Garbage of a plausible size.
        std::fs::write(&path, vec![0xAB; 200]).unwrap();
        assert_eq!(s.read(&path), Sample::Invalid("magic"));
        // Header that lies about dimensions.
        let mut hdr = vec![0u8; 64];
        hdr[0..8].copy_from_slice(&MAGIC);
        hdr[8..12].copy_from_slice(&VERSION.to_le_bytes());
        hdr[12..16].copy_from_slice(&64u32.to_le_bytes());
        hdr[24..28].copy_from_slice(&100_000u32.to_le_bytes());
        hdr[28..32].copy_from_slice(&1u32.to_le_bytes());
        hdr[36..40].copy_from_slice(&1u32.to_le_bytes());
        hdr[40..44].copy_from_slice(&2u32.to_le_bytes());
        std::fs::write(&path, &hdr).unwrap();
        assert_eq!(s.read(&path), Sample::Invalid("dimensions"));

        // Symlink and directory.
        let link = dir.join("link.bin");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(s.read(&link), Sample::Invalid("symlink"));
        assert_eq!(s.read(&dir), Sample::Invalid("not a regular file"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn odd_generation_is_busy_never_torn() {
        let dir = temp_dir("odd");
        let path = dir.join("f.bin");
        let spec = FrameSpec::new(4, 4).unwrap();
        let mut w = SharedFrameWriter::create(&path, spec).unwrap();
        w.publish(&bgra(4, 4, |_, _| [1, 2, 3])).unwrap();
        drop(w);
        // Simulate a writer stuck mid-publish: odd generation.
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all_at(&3u64.to_le_bytes(), OFFSET_GENERATION)
            .unwrap();
        let mut s = FrameSampler::default();
        assert_eq!(s.read(&path), Sample::Busy);
        assert!(s.grid().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
