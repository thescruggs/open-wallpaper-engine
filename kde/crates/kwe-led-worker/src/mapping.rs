// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure mapping from sources (a downsampled frame grid, audio bands) to the
//! LEDs of one zone, and assembly of a whole controller's colour array.

use kwe_openrgb::{Controller, Zone, zone_type};

use crate::color::{BLACK, Rgbf, hsv, mean, vivid_average};

/// Ambient sampling grid cap: at most 64 × 36 = 2304 samples per tick.
pub const GRID_MAX_W: usize = 64;
pub const GRID_MAX_H: usize = 36;

/// Row-major downsampled frame, `width * height == cells.len()`, both ≥ 1.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    width: usize,
    height: usize,
    cells: Vec<Rgbf>,
}

impl Grid {
    pub fn new(width: usize, height: usize, cells: Vec<Rgbf>) -> Option<Self> {
        (width > 0 && height > 0 && width.checked_mul(height) == Some(cells.len())).then_some(
            Self {
                width,
                height,
                cells,
            },
        )
    }

    pub fn solid(width: usize, height: usize, c: Rgbf) -> Option<Self> {
        Self::new(width, height, vec![c; width.checked_mul(height)?])
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn cells(&self) -> &[Rgbf] {
        &self.cells
    }

    fn region(&self, (x0, x1): (usize, usize), (y0, y1): (usize, usize)) -> Vec<Rgbf> {
        let mut out = Vec::with_capacity((x1 - x0) * (y1 - y0));
        for y in y0..y1 {
            out.extend_from_slice(&self.cells[y * self.width + x0..y * self.width + x1]);
        }
        out
    }
}

/// Grid dimensions for a `width × height` frame: never more cells than the
/// frame has pixels and never more than the cap.
pub fn grid_dims(width: u32, height: u32) -> (usize, usize) {
    (
        (width as usize).clamp(1, GRID_MAX_W),
        (height as usize).clamp(1, GRID_MAX_H),
    )
}

/// Half-open source span `[start, end)` covered by output `i` of `n` when
/// `n` outputs are spread over `len` inputs. Never empty for `len ≥ 1`;
/// outputs share an input when `n > len`.
pub fn span(i: usize, n: usize, len: usize) -> (usize, usize) {
    if len == 0 || n == 0 {
        return (0, 0);
    }
    let i = i.min(n - 1);
    let start = (i * len / n).min(len - 1);
    let end = ((i + 1) * len / n).clamp(start + 1, len);
    (start, end)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZoneShape {
    Single,
    Linear,
    /// Row-major zone-local LED indexes; `u32::MAX` (or any index ≥ the
    /// zone's LED count) marks an empty cell.
    Matrix {
        height: usize,
        width: usize,
        cells: Vec<u32>,
    },
}

/// Classifies a zone. A matrix zone without a usable map falls back to
/// linear; unknown zone types are treated as linear.
pub fn zone_shape(zone: &Zone) -> ZoneShape {
    match (zone.kind, &zone.matrix) {
        (zone_type::SINGLE, _) => ZoneShape::Single,
        (zone_type::MATRIX, Some(m))
            if m.height > 0
                && m.width > 0
                && (m.height as usize).checked_mul(m.width as usize) == Some(m.cells.len()) =>
        {
            ZoneShape::Matrix {
                height: m.height as usize,
                width: m.width as usize,
                cells: m.cells.clone(),
            }
        }
        _ => ZoneShape::Linear,
    }
}

fn place_matrix(
    height: usize,
    width: usize,
    cells: &[u32],
    leds: usize,
    fill: Rgbf,
    mut cell_color: impl FnMut(usize, usize) -> Rgbf,
) -> Vec<Rgbf> {
    let mut out = vec![fill; leds];
    for r in 0..height {
        for c in 0..width {
            let idx = cells[r * width + c] as usize;
            if idx < leds {
                out[idx] = cell_color(r, c);
            }
        }
    }
    out
}

/// Ambient colours for one zone of `leds` LEDs:
/// * single → chroma-weighted average of the whole frame;
/// * linear → LED `i` takes the chroma-weighted average of its share of
///   the frame's columns (left → right), full height;
/// * matrix → each cell takes the mean of its box of the grid; LEDs absent
///   from the map take the whole-frame colour.
pub fn ambient_zone(grid: &Grid, shape: &ZoneShape, leds: usize) -> Vec<Rgbf> {
    if leds == 0 {
        return Vec::new();
    }
    match shape {
        ZoneShape::Single => vec![vivid_average(grid.cells.iter()); leds],
        ZoneShape::Linear => (0..leds)
            .map(|i| {
                let cols = span(i, leds, grid.width);
                vivid_average(grid.region(cols, (0, grid.height)).iter())
            })
            .collect(),
        ZoneShape::Matrix {
            height,
            width,
            cells,
        } => {
            let fill = vivid_average(grid.cells.iter());
            place_matrix(*height, *width, cells, leds, fill, |r, c| {
                let cols = span(c, *width, grid.width);
                let rows = span(r, *height, grid.height);
                mean(grid.region(cols, rows).iter())
            })
        }
    }
}

/// Mono raw bands: `mean(left, right)`, clamped to `0..=1`, at the
/// pipeline's own (small) scale — see [`window_level`]. Non-finite values
/// count as 0. Mismatched lengths use the shorter.
pub fn mono_raw(left: &[f32], right: &[f32]) -> Vec<f32> {
    left.iter()
        .zip(right)
        .map(|(l, r)| {
            let v = (l + r) * 0.5;
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            }
        })
        .collect()
}

/// Loudness of one analysis window: the sum of its mono raw bands.
///
/// `kwe_core::audio::analyze_stereo` divides each band's windowed |sample|
/// sum by the WHOLE window length, so the bands of one window add up to
/// the window's mean Hann-weighted |x| ≈ 0.43 × its RMS for noise-like
/// music — independent of the band count (16/32/64) and window length.
/// A single band is that divided by the band count: about 100× below the
/// `0..1` range the old worker assumed (review fixes D, M1).
pub fn window_level(raw: &[f32]) -> f32 {
    raw.iter().sum()
}

/// Automatic gain for display (review fixes D, M1). Tracks a running peak
/// of the loudest band, expressed per band count (`max(raw) × bands`, so
/// it is on the [`window_level`] scale): instant attack, exponential decay
/// of [`AutoGain::DECAY_DB_PER_S`], never below [`AutoGain::FLOOR`].
/// Bands are shown as `sqrt(raw × bands / peak)` (the perceptual lift the
/// worker always used), so the loudest recent band is full scale whatever
/// the playback volume.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoGain {
    peak: f32,
}

impl Default for AutoGain {
    fn default() -> Self {
        Self { peak: Self::FLOOR }
    }
}

impl AutoGain {
    /// Running-peak decay: 10 dB per second. A quiet passage 20 dB below a
    /// loud one is back at full scale within 2 s, 30 dB below within 3 s;
    /// between beats (≤ 0.5 s) the reference falls ≤ 5 dB, so beats still
    /// read as beats.
    pub const DECAY_DB_PER_S: f32 = 10.0;
    /// Reference floor, `max(raw) × bands` units: 6.4e-3 is the loudest
    /// band of −37 dBFS noise. Quieter material is shown below full scale
    /// instead of being amplified (a frame just above the silence floor
    /// peaks at ≈ 37 %, digital silence is black); −40 dBFS music still
    /// reaches ≈ 90 % on its loudest band.
    pub const FLOOR: f32 = 6.4e-3;

    /// The current reference (tests, diagnostics).
    pub fn peak(&self) -> f32 {
        self.peak
    }

    /// Advances the reference by `dt` seconds and returns the display
    /// bands for `raw`. `sound == false` (a frame under the silence floor)
    /// lets the reference decay and shows black.
    pub fn step(&mut self, dt: f32, raw: &[f32], sound: bool) -> Vec<f32> {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        self.peak *= 10f32.powf(-Self::DECAY_DB_PER_S * dt / 20.0);
        if !sound {
            self.peak = self.peak.max(Self::FLOOR);
            return vec![0.0; raw.len()];
        }
        let n = raw.len() as f32;
        let loudest = raw.iter().copied().fold(0.0f32, f32::max) * n;
        self.peak = self.peak.max(loudest).max(Self::FLOOR);
        raw.iter()
            .map(|v| (v * n / self.peak).clamp(0.0, 1.0).sqrt())
            .collect()
    }
}

/// Bass energy: mean of the lowest eighth of the bands (at least 2).
pub fn bass_energy(bands: &[f32]) -> f32 {
    if bands.is_empty() {
        return 0.0;
    }
    let n = (bands.len() / 8).max(2).min(bands.len());
    (bands[..n].iter().sum::<f32>() / n as f32).clamp(0.0, 1.0)
}

fn band_level(bands: &[f32], i: usize, n: usize) -> f32 {
    let (a, b) = span(i, n, bands.len());
    if a >= b {
        return 0.0;
    }
    bands[a..b].iter().sum::<f32>() / (b - a) as f32
}

/// Hue across a spectrum: bass blue (240°) → treble red (0°).
fn spectrum_hue(i: usize, n: usize) -> f32 {
    if n <= 1 {
        return 240.0;
    }
    240.0 * (1.0 - i as f32 / (n - 1) as f32)
}

/// Spectrum colours for one zone:
/// * single → bass pulse: hue purple → red as energy rises, value = energy;
/// * linear → LEDs spread across the bands, hue by position, value = level;
/// * matrix → one bar per column (bands spread over columns), lit from the
///   bottom row up, the top cell partially.
pub fn spectrum_zone(bands: &[f32], shape: &ZoneShape, leds: usize) -> Vec<Rgbf> {
    if leds == 0 {
        return Vec::new();
    }
    if bands.is_empty() {
        return vec![BLACK; leds];
    }
    match shape {
        ZoneShape::Single => {
            let e = bass_energy(bands);
            vec![hsv(270.0 * (1.0 - e), 1.0, e); leds]
        }
        ZoneShape::Linear => (0..leds)
            .map(|i| hsv(spectrum_hue(i, leds), 1.0, band_level(bands, i, leds)))
            .collect(),
        ZoneShape::Matrix {
            height,
            width,
            cells,
        } => {
            let levels: Vec<f32> = (0..*width).map(|c| band_level(bands, c, *width)).collect();
            place_matrix(*height, *width, cells, leds, BLACK, |r, c| {
                let from_bottom = (*height - 1 - r) as f32;
                let fill = (levels[c] * *height as f32 - from_bottom).clamp(0.0, 1.0);
                hsv(spectrum_hue(c, *width), 1.0, fill)
            })
        }
    }
}

/// Assembles a controller's full LED array (`controller.leds.len()`
/// entries) zone by zone in zone order, asking `zone_colors(shape, n)` for
/// each zone's `n` LEDs. LEDs not covered by any zone (inconsistent server
/// data) stay black; a controller without zones is one linear zone.
pub fn controller_colors(
    controller: &Controller,
    mut zone_colors: impl FnMut(&ZoneShape, usize) -> Vec<Rgbf>,
) -> Vec<Rgbf> {
    let total = controller.leds.len();
    let mut out = vec![BLACK; total];
    if controller.zones.is_empty() {
        let v = zone_colors(&ZoneShape::Linear, total);
        for (o, c) in out.iter_mut().zip(v) {
            *o = c;
        }
        return out;
    }
    let mut offset = 0usize;
    for zone in &controller.zones {
        let n = zone.leds_count as usize;
        let Some(end) = offset.checked_add(n).filter(|e| *e <= total) else {
            break;
        };
        let colors = zone_colors(&zone_shape(zone), n);
        for (o, c) in out[offset..end].iter_mut().zip(colors) {
            *o = c;
        }
        offset = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kwe_openrgb::{ControllerId, Led, MatrixMap};

    const RED: Rgbf = Rgbf::new(1.0, 0.0, 0.0);
    const BLUE: Rgbf = Rgbf::new(0.0, 0.0, 1.0);

    fn close(a: Rgbf, b: Rgbf) -> bool {
        (a.r - b.r).abs() < 1e-3 && (a.g - b.g).abs() < 1e-3 && (a.b - b.b).abs() < 1e-3
    }

    /// Left half red, right half blue.
    fn split_grid(w: usize, h: usize) -> Grid {
        let cells = (0..w * h)
            .map(|i| if i % w < w / 2 { RED } else { BLUE })
            .collect();
        Grid::new(w, h, cells).unwrap()
    }

    #[test]
    fn grid_validates_and_dims_are_capped() {
        assert!(Grid::new(0, 1, vec![]).is_none());
        assert!(Grid::new(2, 2, vec![BLACK; 3]).is_none());
        assert_eq!(grid_dims(1920, 1080), (64, 36));
        assert_eq!(grid_dims(8, 4), (8, 4));
        assert_eq!(grid_dims(0, 0), (1, 1));
    }

    #[test]
    fn spans_cover_everything_and_are_never_empty() {
        for len in 1..20 {
            for n in 1..40 {
                let mut covered = vec![false; len];
                for i in 0..n {
                    let (a, b) = span(i, n, len);
                    assert!(a < b && b <= len, "len {len} n {n} i {i}");
                    covered[a..b].iter_mut().for_each(|c| *c = true);
                }
                assert!(covered.iter().all(|c| *c), "len {len} n {n}");
            }
        }
        assert_eq!(span(0, 0, 5), (0, 0));
        assert_eq!(span(99, 4, 8), (6, 8), "index clamps");
    }

    #[test]
    fn ambient_linear_follows_horizontal_axis() {
        let g = split_grid(8, 2);
        let out = ambient_zone(&g, &ZoneShape::Linear, 4);
        assert!(close(out[0], RED) && close(out[1], RED));
        assert!(close(out[2], BLUE) && close(out[3], BLUE));
        // More LEDs than columns still maps monotonically.
        let out = ambient_zone(&g, &ZoneShape::Linear, 20);
        assert!(close(out[0], RED) && close(out[19], BLUE));
        assert!(ambient_zone(&g, &ZoneShape::Linear, 0).is_empty());
    }

    #[test]
    fn ambient_single_is_one_colour() {
        let g = Grid::solid(4, 4, RED).unwrap();
        assert_eq!(ambient_zone(&g, &ZoneShape::Single, 3), vec![RED; 3]);
        let g = split_grid(4, 4);
        let c = ambient_zone(&g, &ZoneShape::Single, 1)[0];
        assert!(close(c, Rgbf::new(0.5, 0.0, 0.5)), "{c:?}");
    }

    #[test]
    fn ambient_matrix_maps_cells_and_skips_holes() {
        let g = split_grid(8, 4);
        let shape = ZoneShape::Matrix {
            height: 1,
            width: 3,
            // LED 2 at the left, LED 0 at the right, a hole in the middle.
            cells: vec![2, u32::MAX, 0],
        };
        let out = ambient_zone(&g, &shape, 4);
        assert!(close(out[2], RED));
        assert!(close(out[0], BLUE));
        // LEDs 1 and 3 are not in the map: whole-frame colour.
        assert!(close(out[1], Rgbf::new(0.5, 0.0, 0.5)));
        assert!(close(out[3], out[1]));
        // Out-of-range indexes are ignored, never panic.
        let shape = ZoneShape::Matrix {
            height: 1,
            width: 1,
            cells: vec![7],
        };
        assert_eq!(ambient_zone(&g, &shape, 2).len(), 2);
    }

    #[test]
    fn audio_helpers() {
        let raw = mono_raw(&[1.0, 0.0, f32::NAN, 4.0], &[0.0, 0.0, 0.0, 4.0]);
        assert_eq!(raw, vec![0.5, 0.0, 0.0, 1.0]);
        assert_eq!(window_level(&raw), 1.5);
        assert_eq!(bass_energy(&[]), 0.0);
        let mut b = vec![0.0; 16];
        b[0] = 1.0;
        b[1] = 1.0;
        assert_eq!(bass_energy(&b), 1.0);
        b[0] = 0.0;
        assert_eq!(bass_energy(&b), 0.5);
        assert_eq!(bass_energy(&[0.4]), 0.4);
    }

    /// M1: instant attack, 10 dB/s decay, floor, silence shows black.
    #[test]
    fn auto_gain_attack_decay_floor() {
        let mut g = AutoGain::default();
        assert_eq!(g.peak(), AutoGain::FLOOR);
        // A loud frame (−10 dBFS scale) is full scale at once.
        let loud = vec![0.003f32; 64];
        let out = g.step(0.05, &loud, true);
        assert!(out.iter().all(|v| (*v - 1.0).abs() < 1e-6), "{out:?}");
        assert!((g.peak() - 0.003 * 64.0).abs() < 1e-6);
        // 20 dB quieter: dim at first, full scale again after 2 s.
        let quiet = vec![0.0003f32; 64];
        let first = g.step(0.05, &quiet, true)[0];
        assert!(first < 0.4, "{first}");
        for _ in 0..40 {
            g.step(0.05, &quiet, true);
        }
        assert!(g.step(0.05, &quiet, true)[0] > 0.99);
        // Silence: black, and the reference decays to the floor, no lower.
        assert!(g.step(10.0, &quiet, false).iter().all(|v| *v == 0.0));
        assert_eq!(g.peak(), AutoGain::FLOOR);
        // Near-silence is not amplified to full scale.
        let faint = vec![1.5e-5f32; 64];
        let v = g.step(0.05, &faint, true)[0];
        assert!(v < 0.4, "{v}");
        // Non-finite dt never poisons the reference.
        g.step(f32::NAN, &faint, true);
        assert!(g.peak().is_finite());
    }

    #[test]
    fn spectrum_zones_react_to_bands() {
        let silent = vec![0.0; 16];
        let mut loud_bass = vec![0.0; 16];
        loud_bass[..2].fill(1.0);
        // Single: bass pulse.
        assert_eq!(spectrum_zone(&silent, &ZoneShape::Single, 1)[0], BLACK);
        let c = spectrum_zone(&loud_bass, &ZoneShape::Single, 1)[0];
        assert!(c.max_channel() > 0.99 && c.r > 0.99, "{c:?}");
        // Linear: only the bass end lights.
        let out = spectrum_zone(&loud_bass, &ZoneShape::Linear, 8);
        assert!(out[0].max_channel() > 0.99);
        assert!(out[7].max_channel() < 1e-6);
        // Empty bands → black.
        assert_eq!(spectrum_zone(&[], &ZoneShape::Linear, 3), vec![BLACK; 3]);
    }

    #[test]
    fn spectrum_matrix_draws_bars_from_the_bottom() {
        // 2 columns, 4 rows; column 0 at level 0.5, column 1 silent.
        let shape = ZoneShape::Matrix {
            height: 4,
            width: 2,
            cells: (0..8).collect(),
        };
        let out = spectrum_zone(&[0.5, 0.0], &shape, 8);
        let lit = |r: usize, c: usize| out[r * 2 + c].max_channel();
        assert!(lit(3, 0) > 0.99 && lit(2, 0) > 0.99, "bottom two lit");
        assert!(lit(1, 0) < 1e-6 && lit(0, 0) < 1e-6, "top two dark");
        assert!((0..4).all(|r| lit(r, 1) < 1e-6));
    }

    fn zone(kind: i32, leds: u32, matrix: Option<MatrixMap>) -> Zone {
        Zone {
            name: "z".into(),
            kind,
            leds_min: leds,
            leds_max: leds,
            leds_count: leds,
            matrix,
            segments: Vec::new(),
            flags: 0,
        }
    }

    #[test]
    fn zone_shapes_classify_and_fall_back() {
        assert_eq!(
            zone_shape(&zone(zone_type::SINGLE, 1, None)),
            ZoneShape::Single
        );
        assert_eq!(
            zone_shape(&zone(zone_type::LINEAR, 4, None)),
            ZoneShape::Linear
        );
        assert_eq!(zone_shape(&zone(99, 4, None)), ZoneShape::Linear);
        assert_eq!(
            zone_shape(&zone(zone_type::MATRIX, 4, None)),
            ZoneShape::Linear
        );
        let bad = MatrixMap {
            height: 2,
            width: 2,
            cells: vec![0, 1, 2],
        };
        assert_eq!(
            zone_shape(&zone(zone_type::MATRIX, 4, Some(bad))),
            ZoneShape::Linear
        );
        let good = MatrixMap {
            height: 1,
            width: 2,
            cells: vec![1, 0],
        };
        assert!(matches!(
            zone_shape(&zone(zone_type::MATRIX, 2, Some(good))),
            ZoneShape::Matrix { .. }
        ));
    }

    fn controller(zones: Vec<Zone>, leds: usize) -> Controller {
        Controller {
            id: ControllerId::new(0),
            kind: 0,
            name: "c".into(),
            vendor: String::new(),
            description: String::new(),
            version: String::new(),
            serial: String::new(),
            location: String::new(),
            modes: Vec::new(),
            active_mode: 0,
            zones,
            leds: (0..leds)
                .map(|i| Led {
                    name: format!("{i}"),
                    value: i as u32,
                })
                .collect(),
            colors: Vec::new(),
            flags: 0,
        }
    }

    #[test]
    fn controller_assembly_respects_zone_offsets_and_bad_counts() {
        let c = controller(
            vec![
                zone(zone_type::SINGLE, 1, None),
                zone(zone_type::LINEAR, 3, None),
            ],
            4,
        );
        let out = controller_colors(&c, |shape, n| match shape {
            ZoneShape::Single => vec![RED; n],
            _ => vec![BLUE; n],
        });
        assert_eq!(out, vec![RED, BLUE, BLUE, BLUE]);
        // Zones claiming more LEDs than exist: the overflowing zone is
        // skipped and the array length is exactly the LED count.
        let c = controller(
            vec![
                zone(zone_type::LINEAR, 2, None),
                zone(zone_type::LINEAR, 9, None),
            ],
            3,
        );
        assert_eq!(
            controller_colors(&c, |_, n| vec![RED; n]),
            vec![RED, RED, BLACK]
        );
        // No zones: one linear zone over all LEDs.
        let c = controller(Vec::new(), 2);
        assert_eq!(controller_colors(&c, |_, n| vec![BLUE; n]), vec![BLUE; 2]);
    }
}
