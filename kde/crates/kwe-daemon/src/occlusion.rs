// SPDX-License-Identifier: GPL-3.0-or-later
//! F3 occlusion detector lifecycle: owns the `kwe-occlusion-worker` child
//! (a small Qt/D-Bus helper that loads the packaged KWin script and relays
//! per-output "covered by a maximized/fullscreen window" transitions back
//! over the daemon socket as `occlusion.report`). Driven synchronously from
//! the supervisor tick — no thread of its own — because the only consumer
//! of its state is the supervisor's render-pause policy.
//!
//! Bounded like the audio worker: at most `MAX_RESTARTS` restarts within
//! `RESTART_WINDOW`, then disabled until the policy is toggled again; SIGTERM
//! with a grace period, then SIGKILL of the child's process group on stop.
//! While the detector is not running the desktop is treated as uncovered —
//! the failure mode is "wallpaper keeps rendering", never "wallpaper stays
//! frozen".

use std::{
    collections::VecDeque,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde::Serialize;

const MAX_RESTARTS: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(600);
const RESTART_DELAY: Duration = Duration::from_millis(500);
const STOP_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OcclusionDetectorStatus {
    /// The policy switch (`pause_when_covered`), not the child's liveness.
    pub enabled: bool,
    pub pid: Option<u32>,
    pub restarts: u64,
    /// Present once the restart budget is exhausted (or no worker binary
    /// was configured); cleared when the policy is toggled off and on.
    pub disabled_reason: Option<String>,
}

pub struct OcclusionDetector {
    worker_path: Option<PathBuf>,
    socket: PathBuf,
    enabled: bool,
    child: Option<Child>,
    restart_history: VecDeque<Instant>,
    restarts: u64,
    next_spawn_at: Option<Instant>,
    disabled_reason: Option<String>,
}

impl OcclusionDetector {
    /// `worker_path == None` means no detector binary is available (tests,
    /// or a daemon that cannot resolve its own executable): the policy can
    /// still be toggled and `occlusion.report` still accepted from any local
    /// caller, only the automatic KWin-backed source is missing.
    pub fn new(worker_path: Option<PathBuf>, socket: PathBuf) -> Self {
        Self {
            worker_path,
            socket,
            enabled: false,
            child: None,
            restart_history: VecDeque::new(),
            restarts: 0,
            next_spawn_at: None,
            disabled_reason: None,
        }
    }

    pub fn status(&self) -> OcclusionDetectorStatus {
        OcclusionDetectorStatus {
            enabled: self.enabled,
            pid: self.child.as_ref().map(Child::id),
            restarts: self.restarts,
            disabled_reason: self.disabled_reason.clone(),
        }
    }

    #[cfg(test)]
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Turns the detector on or off. Enabling resets the restart budget so
    /// a user re-enabling the policy after a bad spell gets a fresh start.
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            self.restart_history.clear();
            self.disabled_reason = None;
            self.next_spawn_at = Some(Instant::now());
        } else {
            self.next_spawn_at = None;
            self.stop_child();
        }
    }

    /// One supervisor tick: reap an exited child (scheduling a bounded
    /// restart) and spawn when due. Returns `true` when the child went away
    /// this tick, so the caller can drop any covered state it reported.
    pub fn tick(&mut self) -> bool {
        let mut lost = false;
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    eprintln!("event=occlusion.worker.exited status={status}");
                    self.child = None;
                    lost = true;
                    self.schedule_restart();
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("event=occlusion.worker.wait_error detail={error}");
                    self.child = None;
                    lost = true;
                    self.schedule_restart();
                }
            }
        }
        if self.enabled
            && self.child.is_none()
            && self.disabled_reason.is_none()
            && self.next_spawn_at.is_some_and(|at| Instant::now() >= at)
        {
            self.next_spawn_at = None;
            if let Err(error) = self.spawn() {
                eprintln!("event=occlusion.worker.spawn_failed detail={error}");
                self.schedule_restart();
            }
        }
        lost
    }

    pub fn shutdown(&mut self) {
        self.stop_child();
    }

    fn schedule_restart(&mut self) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let cutoff = now.checked_sub(RESTART_WINDOW).unwrap_or(now);
        while self
            .restart_history
            .front()
            .is_some_and(|front| *front < cutoff)
        {
            self.restart_history.pop_front();
        }
        if self.restart_history.len() >= MAX_RESTARTS {
            let reason = format!(
                "restart budget exhausted ({MAX_RESTARTS} within {}s)",
                RESTART_WINDOW.as_secs()
            );
            eprintln!("event=occlusion.worker.disabled detail={reason}");
            self.disabled_reason = Some(reason);
            self.next_spawn_at = None;
            return;
        }
        self.restart_history.push_back(now);
        self.restarts = self.restarts.saturating_add(1);
        self.next_spawn_at = Some(now + RESTART_DELAY);
    }

    fn spawn(&mut self) -> Result<()> {
        let Some(worker_path) = self.worker_path.clone() else {
            self.disabled_reason = Some("no occlusion worker binary configured".into());
            eprintln!("event=occlusion.worker.disabled detail=no worker binary configured");
            return Ok(());
        };
        let mut command = Command::new(&worker_path);
        command
            .arg("--socket")
            .arg(&self.socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        let expected_parent = i32::try_from(std::process::id()).context("daemon pid overflow")?;
        // SAFETY: runs in the child between fork and exec; only
        // async-signal-safe libc calls, no allocation (same posture as the
        // audio worker). Own process group + parent-death SIGTERM so a
        // crashed daemon never leaves a KWin script loaded behind it.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != expected_parent {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "daemon exited before occlusion worker exec",
                    ));
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .with_context(|| format!("launch {}", worker_path.display()))?;
        eprintln!("event=occlusion.worker.spawned pid={}", child.id());
        self.child = Some(child);
        Ok(())
    }

    fn stop_child(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = child.id();
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        signal_process_group(pid, libc::SIGTERM);
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        signal_process_group(pid, libc::SIGKILL);
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("event=occlusion.worker.forced_kill pid={pid}");
    }
}

impl Drop for OcclusionDetector {
    fn drop(&mut self) {
        self.stop_child();
    }
}

fn signal_process_group(pid: u32, signal: libc::c_int) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: the child was placed in a process group whose id equals
        // its pid before exec; a negative pid addresses that group.
        unsafe {
            libc::kill(-pid, signal);
        }
    }
}

/// Pure policy: the desktop counts as covered only when every reported
/// output is covered and at least one output was reported. An empty output
/// list (detector starting up, KWin gone) never pauses.
pub fn all_outputs_covered(outputs: &[String], covered: &[String]) -> bool {
    !outputs.is_empty() && outputs.iter().all(|output| covered.contains(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    fn temporary_directory(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "kwe-occlusion-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn fake_worker(root: &Path, body: &str) -> PathBuf {
        let script = root.join("occlusion-worker");
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn settle(detector: &mut OcclusionDetector, ticks: usize) -> bool {
        let mut lost = false;
        for _ in 0..ticks {
            lost |= detector.tick();
            thread::sleep(Duration::from_millis(20));
        }
        lost
    }

    #[test]
    fn covered_policy_requires_every_reported_output() {
        let outputs = vec!["DP-1".to_string(), "HDMI-A-1".to_string()];
        assert!(!all_outputs_covered(&[], &[]));
        assert!(!all_outputs_covered(&outputs, &["DP-1".to_string()]));
        assert!(all_outputs_covered(&outputs, &outputs));
        assert!(all_outputs_covered(
            &outputs[..1],
            &["HDMI-A-1".to_string(), "DP-1".to_string()]
        ));
    }

    #[test]
    fn disabled_detector_never_spawns_and_enabling_launches_with_the_socket() {
        let root = temporary_directory("spawn");
        let worker = fake_worker(&root, "printf '%s\\n' \"$@\" > \"$0.argv\"; sleep 30");
        let mut detector = OcclusionDetector::new(Some(worker.clone()), root.join("d.sock"));
        settle(&mut detector, 3);
        assert_eq!(detector.pid(), None);

        detector.set_enabled(true);
        settle(&mut detector, 3);
        let pid = detector.pid().expect("enabled detector must spawn");
        let argv = fs::read_to_string(format!("{}.argv", worker.display())).unwrap();
        assert!(argv.contains("--socket"), "{argv}");
        assert!(argv.contains("d.sock"), "{argv}");

        detector.set_enabled(false);
        assert_eq!(detector.pid(), None, "disabling stops the child");
        // The old pid is gone (reaped), no zombie remains.
        // SAFETY: kill with signal 0 only probes existence.
        let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        assert!(!alive, "child {pid} must be gone after disable");
    }

    #[test]
    fn crash_loop_exhausts_the_budget_and_reports_loss_each_time() {
        let root = temporary_directory("budget");
        let worker = fake_worker(&root, "exit 1");
        let mut detector = OcclusionDetector::new(Some(worker), root.join("d.sock"));
        detector.set_enabled(true);
        let mut losses = 0;
        for _ in 0..200 {
            if detector.tick() {
                losses += 1;
            }
            if detector.status().disabled_reason.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let status = detector.status();
        assert!(status.disabled_reason.is_some(), "{status:?}");
        assert_eq!(status.restarts, MAX_RESTARTS as u64);
        assert!(losses >= 1, "every exit must be reported as a loss");
        // Re-enabling clears the budget and tries again.
        detector.set_enabled(false);
        detector.set_enabled(true);
        assert!(detector.status().disabled_reason.is_none());
    }

    #[test]
    fn missing_binary_disables_without_crashing() {
        let root = temporary_directory("nobinary");
        let mut detector = OcclusionDetector::new(None, root.join("d.sock"));
        detector.set_enabled(true);
        settle(&mut detector, 2);
        assert!(detector.status().disabled_reason.is_some());
        assert_eq!(detector.pid(), None);
    }
}
