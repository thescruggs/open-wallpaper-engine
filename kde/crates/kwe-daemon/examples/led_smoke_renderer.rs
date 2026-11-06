// SPDX-License-Identifier: GPL-3.0-or-later
//! `scripts/smoke-led.sh` helper (L5): a stand-in `--renderer` that
//! publishes one SOLID colour (RGB 0,160,255) through the frame protocol, so
//! the smoke can assert the colour the LED worker drives. `kwe-test-renderer`
//! only draws its animated test pattern. An ODD `--fps` selects a fast
//! colour cycle instead (the LED worker skips unchanged colours, so the
//! smoke's rate-bound case needs a changing picture). Accepts the supervisor's renderer
//! argv (`--output/--width/--height/--fps`, everything else ignored); stdin
//! is never read (the supervisor's writes are nonblocking); SIGTERM's
//! default action ends it. Never shipped; test builds only.

use std::{path::PathBuf, thread, time::Duration};

use kwe_frame_protocol::{FrameSpec, SharedFrameWriter};

/// BGRA of RGB(0, 160, 255).
const PIXEL: [u8; 4] = [255, 160, 0, 255];

fn main() {
    let mut output: Option<PathBuf> = None;
    let (mut width, mut height, mut fps) = (160_u32, 90_u32, 30_u32);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--output" => output = args.next().map(PathBuf::from),
            "--width" => width = args.next().and_then(|v| v.parse().ok()).unwrap_or(width),
            "--height" => height = args.next().and_then(|v| v.parse().ok()).unwrap_or(height),
            "--fps" => fps = args.next().and_then(|v| v.parse().ok()).unwrap_or(fps),
            _ => {}
        }
    }
    let output = output.expect("--output");
    let (width, height) = (width.clamp(1, 1920), height.clamp(1, 1080));
    let spec = FrameSpec::new(width, height).expect("frame spec");
    let mut writer = SharedFrameWriter::create(&output, spec).expect("frame mapping");
    let stride = spec.stride as usize;
    let mut pixels = vec![0_u8; stride * height as usize];
    for row in pixels.chunks_mut(stride) {
        for px in row.chunks_mut(4).take(width as usize) {
            px.copy_from_slice(&PIXEL);
        }
    }
    let animated = fps % 2 == 1;
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps.clamp(1, 60)));
    let mut frame = 0_u32;
    loop {
        if animated {
            // Red/green ramp that never repeats two frames in a row.
            let level = ((frame * 37) % 256) as u8;
            let pixel = [0, level, 255 - level, 255];
            for row in pixels.chunks_mut(stride) {
                for px in row.chunks_mut(4).take(width as usize) {
                    px.copy_from_slice(&pixel);
                }
            }
            frame = frame.wrapping_add(1);
        }
        writer.publish(&pixels).expect("publish");
        thread::sleep(interval);
    }
}
