// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure colour math: float colours, HSV, vivid averaging, temporal
//! smoothing, and the brightness cap.

use kwe_openrgb::Rgb;

/// Colour with channels in `0.0..=1.0` (sRGB-encoded values, not linear
/// light: the LED controllers take sRGB bytes and the math only blends).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rgbf {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

pub const BLACK: Rgbf = Rgbf::new(0.0, 0.0, 0.0);

impl Rgbf {
    pub const fn new(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b }
    }

    pub fn from_u8(r: u8, g: u8, b: u8) -> Self {
        Self::new(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
        )
    }

    pub fn scale(self, k: f32) -> Self {
        Self::new(self.r * k, self.g * k, self.b * k)
    }

    pub fn max_channel(self) -> f32 {
        self.r.max(self.g).max(self.b)
    }

    pub fn min_channel(self) -> f32 {
        self.r.min(self.g).min(self.b)
    }

    /// Clamps to `0..=1` (NaN → 0) and rounds to bytes.
    pub fn to_rgb(self) -> Rgb {
        let q = |v: f32| {
            if v.is_nan() {
                0
            } else {
                (v.clamp(0.0, 1.0) * 255.0).round() as u8
            }
        };
        Rgb::new(q(self.r), q(self.g), q(self.b))
    }
}

/// HSV → RGB; `h` in degrees (wrapped), `s`/`v` clamped to `0..=1`.
pub fn hsv(h: f32, s: f32, v: f32) -> Rgbf {
    let h = if h.is_finite() {
        h.rem_euclid(360.0)
    } else {
        0.0
    };
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    Rgbf::new(r + m, g + m, b + m)
}

/// "Dominant-ish" average for single-colour zones: a plain mean of a
/// wallpaper is usually a muddy grey, so each sample is weighted by
/// `1 + 4 * saturation * value` (chroma-weighted mean). Empty → black.
pub fn vivid_average<'a>(samples: impl IntoIterator<Item = &'a Rgbf>) -> Rgbf {
    let (mut r, mut g, mut b, mut w) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for c in samples {
        let chroma = c.max_channel() - c.min_channel();
        let weight = 1.0 + 4.0 * chroma.max(0.0);
        r += c.r * weight;
        g += c.g * weight;
        b += c.b * weight;
        w += weight;
    }
    if w <= 0.0 {
        BLACK
    } else {
        Rgbf::new(r / w, g / w, b / w)
    }
}

/// Plain mean. Empty → black.
pub fn mean<'a>(samples: impl IntoIterator<Item = &'a Rgbf>) -> Rgbf {
    let (mut r, mut g, mut b, mut n) = (0.0f32, 0.0f32, 0.0f32, 0u32);
    for c in samples {
        r += c.r;
        g += c.g;
        b += c.b;
        n += 1;
    }
    if n == 0 {
        BLACK
    } else {
        let n = n as f32;
        Rgbf::new(r / n, g / n, b / n)
    }
}

/// Exponential smoothing factor for a step of `dt` seconds with time
/// constant `tau` seconds. `tau <= 0` → 1 (no smoothing).
pub fn alpha(dt: f32, tau: f32) -> f32 {
    if tau <= 0.0 || !tau.is_finite() {
        return 1.0;
    }
    let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
    1.0 - (-dt / tau).exp()
}

/// Time constants (seconds) for one smoothing step. `attack` applies when a
/// channel rises, `release` when it falls.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Taus {
    pub attack: f32,
    pub release: f32,
}

impl Taus {
    pub const fn symmetric(tau: f32) -> Self {
        Self {
            attack: tau,
            release: tau,
        }
    }
}

/// Smooths one scalar toward `target`.
pub fn smooth_scalar(current: f32, target: f32, dt: f32, taus: Taus) -> f32 {
    let tau = if target > current {
        taus.attack
    } else {
        taus.release
    };
    current + (target - current) * alpha(dt, tau)
}

/// Per-LED temporal smoother. The first step after a (re)size jumps
/// straight to the target.
#[derive(Debug, Clone, Default)]
pub struct Smoother {
    state: Vec<Rgbf>,
}

impl Smoother {
    pub fn step(&mut self, target: &[Rgbf], dt: f32, taus: Taus) -> &[Rgbf] {
        if self.state.len() != target.len() {
            self.state = target.to_vec();
            return &self.state;
        }
        for (cur, tgt) in self.state.iter_mut().zip(target) {
            cur.r = smooth_scalar(cur.r, tgt.r, dt, taus);
            cur.g = smooth_scalar(cur.g, tgt.g, dt, taus);
            cur.b = smooth_scalar(cur.b, tgt.b, dt, taus);
        }
        &self.state
    }

    pub fn current(&self) -> &[Rgbf] {
        &self.state
    }
}

/// Applies the global brightness cap (`0..=100` %; larger values clamp).
pub fn apply_brightness(c: Rgbf, percent: u8) -> Rgbf {
    c.scale(f32::from(percent.min(100)) / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn hsv_primaries_and_bounds() {
        assert_eq!(hsv(0.0, 1.0, 1.0).to_rgb(), Rgb::new(255, 0, 0));
        assert_eq!(hsv(120.0, 1.0, 1.0).to_rgb(), Rgb::new(0, 255, 0));
        assert_eq!(hsv(240.0, 1.0, 1.0).to_rgb(), Rgb::new(0, 0, 255));
        assert_eq!(hsv(360.0, 1.0, 1.0).to_rgb(), Rgb::new(255, 0, 0));
        assert_eq!(hsv(-120.0, 1.0, 0.5).to_rgb(), Rgb::new(0, 0, 128));
        assert_eq!(hsv(f32::NAN, 2.0, 7.0).to_rgb(), Rgb::new(255, 0, 0));
        assert_eq!(hsv(10.0, 0.0, 0.0).to_rgb(), Rgb::new(0, 0, 0));
    }

    #[test]
    fn to_rgb_clamps_and_handles_nan() {
        assert_eq!(Rgbf::new(-1.0, 2.0, f32::NAN).to_rgb(), Rgb::new(0, 255, 0));
        assert_eq!(Rgbf::from_u8(1, 128, 255).to_rgb(), Rgb::new(1, 128, 255));
    }

    #[test]
    fn vivid_average_favours_saturated_samples() {
        let red = Rgbf::new(1.0, 0.0, 0.0);
        let grey = Rgbf::new(0.5, 0.5, 0.5);
        let plain = mean([red, grey].iter());
        let vivid = vivid_average([red, grey].iter());
        assert!(close(plain.r, 0.75) && close(plain.g, 0.25));
        // weights 5 and 1 → r = (5 + 0.5) / 6, g = 0.5 / 6
        assert!(close(vivid.r, 5.5 / 6.0), "{vivid:?}");
        assert!(close(vivid.g, 0.5 / 6.0), "{vivid:?}");
        assert_eq!(vivid_average([].iter()), BLACK);
        assert_eq!(mean([].iter()), BLACK);
    }

    #[test]
    fn alpha_is_bounded_and_monotonic() {
        assert_eq!(alpha(0.1, 0.0), 1.0);
        assert_eq!(alpha(0.1, f32::NAN), 1.0);
        assert_eq!(alpha(0.0, 0.3), 0.0);
        assert_eq!(alpha(-1.0, 0.3), 0.0);
        let a = alpha(0.1, 0.3);
        assert!(a > 0.0 && a < 1.0);
        assert!(alpha(0.2, 0.3) > a);
        assert!(close(alpha(0.3, 0.3), 1.0 - (-1.0f32).exp()));
    }

    #[test]
    fn smoother_converges_without_overshoot_and_uses_attack_release() {
        let mut s = Smoother::default();
        let white = [Rgbf::new(1.0, 1.0, 1.0)];
        let black = [BLACK];
        assert_eq!(s.step(&black, 0.1, Taus::symmetric(0.3)), &black);
        let mut last = 0.0;
        for _ in 0..50 {
            let v = s.step(&white, 0.1, Taus::symmetric(0.3))[0].r;
            assert!(v >= last && v <= 1.0, "monotone, no overshoot");
            last = v;
        }
        assert!(last > 0.99);
        // Fast attack, slow release.
        let taus = Taus {
            attack: 0.01,
            release: 10.0,
        };
        let mut s = Smoother::default();
        s.step(&black, 0.1, taus);
        assert!(s.step(&white, 0.1, taus)[0].r > 0.99);
        assert!(s.step(&black, 0.1, taus)[0].r > 0.98);
        // Resize resets.
        assert_eq!(s.step(&[BLACK, BLACK], 0.1, taus).len(), 2);
        assert_eq!(s.current()[0], BLACK);
    }

    #[test]
    fn brightness_cap_scales_and_clamps() {
        let c = Rgbf::new(1.0, 0.5, 0.0);
        assert_eq!(apply_brightness(c, 100), c);
        assert_eq!(apply_brightness(c, 0).to_rgb(), Rgb::new(0, 0, 0));
        assert_eq!(apply_brightness(c, 50).to_rgb(), Rgb::new(128, 64, 0));
        assert_eq!(apply_brightness(c, 250), c);
    }
}
