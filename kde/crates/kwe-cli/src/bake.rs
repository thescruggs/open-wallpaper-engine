// SPDX-License-Identifier: GPL-3.0-or-later
//! `kwe bake-clip`: record a bounded loop of a scene or web wallpaper into
//! an H.264 file for the SDDM login pool (`sddm/README.md`).
//!
//! The greeter runs as the `sddm` user before any session exists, so it
//! cannot reach the daemon or run a renderer. Instead this command drives
//! the same out-of-process worker the daemon launches (same argv contract,
//! same frame protocol v1 file), snapshots its frames at a fixed cadence,
//! and pipes them to `ffmpeg`. Nothing here parses wallpaper content.
//!
//! Bounds: frame size is capped by the protocol (8192²), the clip by
//! `--seconds` (≤120), the first-frame wait by `--first-frame-timeout`
//! (≤300 s), and worker/encoder shutdown by a 5 s grace before SIGKILL.

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use kwe_core::{default_steam_roots, default_wallpaper_engine_assets_dir};
use kwe_frame_protocol::{ProducerState, SharedFrameReader};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BakeKind {
    Scene,
    Web,
}

impl BakeKind {
    fn renderer_name(self) -> &'static str {
        match self {
            Self::Scene => "kwe-scene-renderer",
            Self::Web => "kwe-web-renderer",
        }
    }
}

pub struct BakeRequest {
    pub kind: BakeKind,
    pub content: PathBuf,
    pub output: PathBuf,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub seconds: u32,
    pub warmup: u32,
    pub first_frame_timeout: u32,
    pub renderer: Option<PathBuf>,
    pub assets_dir: Option<PathBuf>,
    pub verbose: bool,
}

const POLL: Duration = Duration::from_millis(50);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub fn bake_clip(request: &BakeRequest) -> Result<()> {
    if !request.content.exists() {
        bail!("content {} does not exist", request.content.display());
    }
    let renderer = match &request.renderer {
        Some(path) => path.clone(),
        None => sibling_binary(request.kind.renderer_name())?,
    };
    let work_dir = private_work_dir()?;
    let result = run(request, &renderer, &work_dir);
    let _ = fs::remove_dir_all(&work_dir);
    result
}

fn run(request: &BakeRequest, renderer: &Path, work_dir: &Path) -> Result<()> {
    let frame_path = work_dir.join("frame.bin");
    let home = work_dir.join("home");
    fs::create_dir(&home).with_context(|| format!("create {}", home.display()))?;
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;

    let mut command = Command::new(renderer);
    command
        .arg("--output")
        .arg(&frame_path)
        .arg("--width")
        .arg(request.width.to_string())
        .arg("--height")
        .arg(request.height.to_string())
        .arg("--fps")
        .arg(request.fps.to_string())
        .arg("--scaling")
        .arg("fill")
        .arg("--content")
        .arg(&request.content)
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", bake_cache_home())
        // Kept open for the run: the worker's stdin is its NDJSON input
        // channel and EOF there is not a stop request we want to rely on.
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(if request.verbose {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
    match request.kind {
        BakeKind::Scene => {
            command.arg("--mute");
            let assets = request
                .assets_dir
                .clone()
                .or_else(|| default_wallpaper_engine_assets_dir(&default_steam_roots()));
            if let Some(assets) = assets {
                command.arg("--assets-dir").arg(assets);
            }
        }
        // Web workers stay network-isolated: a login-screen clip never
        // needs the per-wallpaper network grant.
        BakeKind::Web => {}
    }
    let worker = command
        .spawn()
        .with_context(|| format!("spawn {}", renderer.display()))?;
    let mut worker = KillOnDrop(Some(worker));

    let (reader, first_pixels) = wait_for_first_frame(request, &frame_path, worker.child())?;
    let spec = reader.spec();
    if spec.width != request.width || spec.height != request.height {
        bail!(
            "renderer published {}x{}, expected {}x{}",
            spec.width,
            spec.height,
            request.width,
            request.height
        );
    }

    // Warm-up: scenes often fade in or settle particle systems; discard it.
    let warmup_end = Instant::now() + Duration::from_secs(u64::from(request.warmup));
    while Instant::now() < warmup_end {
        check_worker(worker.child())?;
        thread::sleep(POLL);
    }

    let partial = request.output.with_extension("partial.mp4");
    let mut encoder = KillOnDrop(Some(spawn_encoder(request, &partial)?));
    let frames = u64::from(request.seconds) * u64::from(request.fps);
    let interval = Duration::from_secs_f64(1.0 / f64::from(request.fps));
    // Seeded with the first frame: at large sizes a snapshot can lose all
    // its seqlock retries to the producer, and the clip must still start.
    let mut last = first_pixels;
    let start = Instant::now();
    for index in 0..frames {
        let due = start + interval.mul_f64(index as f64);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
        check_worker(worker.child())?;
        // A seqlock miss (Busy) re-emits the previous frame so the clip
        // keeps its cadence instead of stalling.
        if let Ok(snapshot) = reader.snapshot() {
            if snapshot.producer_state == ProducerState::Failed {
                bail!("renderer reported failure while recording");
            }
            last = snapshot.pixels;
        }
        let stdin = encoder.child().stdin.as_mut().context("encoder stdin")?;
        stdin
            .write_all(&last)
            .context("ffmpeg stopped accepting frames")?;
    }

    worker.terminate();
    let mut encoder_child = encoder.0.take().context("encoder already reaped")?;
    drop(encoder_child.stdin.take());
    let status = wait_bounded(&mut encoder_child, Duration::from_secs(120))?;
    if !status.success() {
        let _ = fs::remove_file(&partial);
        bail!("ffmpeg failed ({status})");
    }
    fs::rename(&partial, &request.output)
        .with_context(|| format!("replace {}", request.output.display()))?;
    Ok(())
}

fn wait_for_first_frame(
    request: &BakeRequest,
    frame_path: &Path,
    worker: &mut Child,
) -> Result<(SharedFrameReader, Vec<u8>)> {
    let deadline = Instant::now() + Duration::from_secs(u64::from(request.first_frame_timeout));
    let mut reader = None;
    let pixels = loop {
        check_worker(worker)?;
        if reader.is_none() && frame_path.exists() {
            reader = SharedFrameReader::open(frame_path).ok();
        }
        if let Some(open) = &reader
            && let Ok(snapshot) = open.snapshot()
        {
            match snapshot.producer_state {
                ProducerState::Failed => bail!("renderer reported failure before first frame"),
                ProducerState::Running if snapshot.sequence > 0 => break snapshot.pixels,
                _ => {}
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "no frame within {} s (raise --first-frame-timeout for cold shader caches)",
                request.first_frame_timeout
            );
        }
        thread::sleep(POLL);
    };
    Ok((reader.context("frame file vanished")?, pixels))
}

fn check_worker(worker: &mut Child) -> Result<()> {
    if let Some(status) = worker.try_wait()? {
        // 73 backend_reject / 74 no_drawable_content: the worker refused
        // this content; the caller should skip it, not retry.
        bail!("renderer exited early ({status})");
    }
    Ok(())
}

fn spawn_encoder(request: &BakeRequest, partial: &Path) -> Result<Child> {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "rawvideo", "-pix_fmt", "bgra"])
        .arg("-s")
        .arg(format!("{}x{}", request.width, request.height))
        .arg("-framerate")
        .arg(request.fps.to_string())
        .args(["-i", "-", "-an"])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"])
        .args(["-pix_fmt", "yuv420p", "-movflags", "+faststart"])
        .arg(partial)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn ffmpeg (is it installed?)")
}

/// Kills (SIGTERM, then SIGKILL after a grace period) and reaps a child on
/// every exit path, so an error mid-recording never leaks a GPU renderer.
struct KillOnDrop(Option<Child>);

impl KillOnDrop {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("child present until terminate")
    }

    fn terminate(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: plain kill(2) on our own unreaped child's pid.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        if wait_bounded(&mut child, SHUTDOWN_GRACE).is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn wait_bounded(child: &mut Child, limit: Duration) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            bail!("process {} did not exit within {:?}", child.id(), limit);
        }
        thread::sleep(POLL);
    }
}

/// The renderer beside this `kwe` (packaged layout), else the first one on
/// `$PATH` (a development `kwe` run from `target/`).
fn sibling_binary(name: &str) -> Result<PathBuf> {
    let executable = std::env::current_exe().context("locate kwe")?;
    let sibling = executable.with_file_name(name);
    if sibling.is_file() {
        return Ok(sibling);
    }
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .with_context(|| format!("{name} not found beside kwe or on PATH; pass --renderer"))
}

fn private_work_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let dir = base.join(format!("kwe-bake-{}-{nonce}", std::process::id()));
    fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Persistent shader cache so re-baking a scene skips the cold compile.
fn bake_cache_home() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("kwe/bake")
}
