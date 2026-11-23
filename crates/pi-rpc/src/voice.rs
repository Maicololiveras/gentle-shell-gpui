//! Bounded, single-use dictation. Owns no draft state and never uses Pi RPC.

use crate::LaunchSpec;
use anyhow::{Result, anyhow, ensure};
use serde::Deserialize;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

mod node;
mod setup;
pub use setup::{VoiceSetupComponent, VoiceSetupEvent, VoiceSetupPhase, VoiceSetupSession};
#[cfg(test)]
mod tests;
mod tree;
use tree::Tree;

const SCRIPT: &str = concat!(
    include_str!("voice/bridge.mjs"),
    "\nawait main(...process.argv.slice(1));\n"
);
const POLL: Duration = Duration::from_millis(25);
const GRACE: Duration = Duration::from_secs(4);
const SESSION: Duration = Duration::from_secs(380);
const PROBE: Duration = Duration::from_secs(5);
const MAX_LINE: u64 = 100 * 1024; // Maximum JSON escaping of 16K UTF-16 units.
const ENV: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "SystemRoot",
    "TEMP",
    "TMP",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "PULSE_SERVER",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
];

fn bundled_package_matches(root: &Path, selected_package: &Path, package: &Path) -> Option<()> {
    // Resolve `current` (or the release named by the Windows marker) separately
    // from the package link. The target cannot nominate a different release.
    let release = dunce::canonicalize(selected_package.parent()?.parent()?).ok()?;
    if release.parent()? != root.join("versions") {
        return None;
    }
    let modules = release.join("node_modules");
    if package == modules.join("gentle-pi") {
        return Some(());
    }
    if release.file_name()? != "5.0.0-1.6.0" {
        return None;
    }
    let store = modules.join(".pnpm");
    let relative = package.strip_prefix(store).ok()?;
    let slot = relative.iter().next()?;
    let peers = slot.to_str()?.strip_prefix("gentle-pi@5.0.0_")?;
    // pnpm's encoded peer suffix occupies one component, followed by the
    // exact package tail. A relocated store fails the canonical prefix check.
    (!peers.is_empty()
        && peers
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@.+_-".contains(&b))
        && relative == Path::new(slot).join("node_modules").join("gentle-pi"))
    .then_some(())
}

/// A vetted install paired to the selected backend, not to the focused folder
/// or to the globally preferred Gentle Shell installation.
#[derive(Debug, Clone)]
pub struct VoiceAdapter {
    root: PathBuf,
    node: PathBuf,
    package: PathBuf,
    home: PathBuf,
    mode: &'static str,
    setup_anchor: bool,
    env: Vec<(&'static str, OsString)>,
}

/// One non-RPC record. `Ready` is emitted only by a no-capture probe.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum VoiceEvent {
    Ready,
    Recording,
    Transcribing,
    Result { text: String, truncated: bool },
    Cancelled,
    Error { code: String },
}

impl VoiceAdapter {
    /// Unknown wrappers and ambiguous install paths fail closed. The helper
    /// checks the exact package code and selected Node's platform at runtime.
    pub fn from_launch(spec: &LaunchSpec) -> Option<Self> {
        let bundled = spec.bundled.as_ref()?;
        if !spec.launcher.is_absolute() || !bundled.node.is_absolute() {
            return None;
        }
        let (mode, install, home) = if bundled.managed {
            let managed = crate::launch::managed_install(&spec.launcher)?;
            let homes = managed.homes();
            let install = homes.config.parent()?.to_owned();
            if spec.program != bundled.node
                || spec.args.first().map(Path::new)
                    != Some(
                        bundled
                            .package
                            .join("bin")
                            .join("gentle-shell.mjs")
                            .as_path(),
                    )
                || std::env::var_os("GENTLE_SHELL_PREFIX").is_some_and(|s| !s.is_empty())
            {
                return None;
            }
            let config = spec
                .env
                .iter()
                .find(|(key, _)| key == "GENTLE_PI_CONFIG_HOME")?;
            if Path::new(&config.1) != homes.config.as_path()
                || bundled.node != install.join("runtime").join("node").join("node.exe")
                || bundled.package
                    != install
                        .join("prefix")
                        .join("node_modules")
                        .join("gentle-pi")
            {
                return None;
            }
            ("managed", install, homes.config)
        } else {
            if spec.program != spec.launcher {
                return None;
            }
            let selected = crate::launch::bundled_install(&spec.launcher)?;
            if selected.node != bundled.node || selected.package != bundled.package {
                return None;
            }
            let install = spec.launcher.parent()?.parent()?.to_owned();
            if dunce::canonicalize(&spec.launcher).ok()?
                != dunce::canonicalize(&install)
                    .ok()?
                    .join(spec.launcher.strip_prefix(&install).ok()?)
            {
                return None;
            }
            ("bundled", install.clone(), install)
        };
        if !install.is_absolute() {
            return None;
        }
        let root = dunce::canonicalize(&install).ok()?;
        let node = dunce::canonicalize(&bundled.node).ok()?;
        let package = dunce::canonicalize(crate::gentle_pi_root(spec)?).ok()?;
        let expected_node = root.join(bundled.node.strip_prefix(&install).ok()?);
        let runtime = root.join("runtime");
        if node != expected_node
            || !node.starts_with(&runtime)
            || !node.is_file()
            || !package.is_dir()
            || !package.starts_with(&root)
        {
            return None;
        }
        let home = if mode == "managed" {
            if package != root.join("prefix").join("node_modules").join("gentle-pi") {
                return None;
            }
            let config = root.join(home.strip_prefix(&install).ok()?);
            if dunce::canonicalize(&config).ok()? != config {
                return None;
            }
            config
        } else {
            bundled_package_matches(&root, &bundled.package, &package)?;
            root.clone()
        };
        if home.to_str().is_none() || package.to_str().is_none() {
            return None;
        }
        let env = ENV
            .iter()
            .filter_map(|&key| {
                spec.env
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.clone())
                    .or_else(|| std::env::var_os(key))
                    .map(|value| (key, value))
            })
            .collect();
        Some(Self {
            setup_anchor: install == root,
            root,
            node,
            package,
            home,
            mode,
            env,
        })
    }

    /// Check that the original RPC launch still resolves to this pinned pair.
    /// This compares paths, not file identity; do not use the fresh resolution
    /// to run dictation or as a substitute for the runtime trust checks.
    pub fn matches_launch(&self, spec: &LaunchSpec) -> bool {
        Self::from_launch(spec).is_some_and(|fresh| {
            fresh.root == self.root
                && fresh.node == self.node
                && fresh.package == self.package
                && fresh.home == self.home
                && fresh.mode == self.mode
        })
    }

    /// Check platform and installed voice components without opening a device.
    /// Run on a background executor; returns `Ready` or an `Error` record.
    pub fn probe(&self) -> Result<VoiceEvent> {
        self.probe_inner(None)
    }

    /// Probe with the same app-quit signal used for recording.
    pub fn probe_until_shutdown(&self, shutdown: Arc<AtomicBool>) -> Result<VoiceEvent> {
        self.probe_inner(Some(shutdown))
    }

    fn probe_inner(&self, shutdown: Option<Arc<AtomicBool>>) -> Result<VoiceEvent> {
        let (session, events) = self.spawn(true, shutdown)?;
        let result = events
            .recv_blocking()
            .map_err(|_| anyhow!("Voice helper stopped."));
        drop(session);
        result
    }

    /// Start recording once. Keep the handle for as long as events are relevant.
    pub fn start(&self) -> Result<(VoiceSession, async_channel::Receiver<VoiceEvent>)> {
        self.spawn(false, None)
    }

    /// Start with a one-way app-quit signal. Once set, bypass the normal
    /// cancellation grace period and tear down the process tree.
    pub fn start_until_shutdown(
        &self,
        shutdown: Arc<AtomicBool>,
    ) -> Result<(VoiceSession, async_channel::Receiver<VoiceEvent>)> {
        self.spawn(false, Some(shutdown))
    }

    fn isolated_command(&self, path: &Path) -> Command {
        let mut command = crate::command(path);
        command
            .current_dir(&self.package)
            .env_clear()
            .envs(self.env.iter().cloned())
            .stderr(Stdio::null());
        command
    }

    fn spawn(
        &self,
        probe: bool,
        shutdown: Option<Arc<AtomicBool>>,
    ) -> Result<(VoiceSession, async_channel::Receiver<VoiceEvent>)> {
        let controls = Arc::new(Controls {
            shutdown,
            ..Controls::default()
        });
        let session = VoiceSession(controls.clone());
        let (events, receiver) = async_channel::bounded(4);
        let adapter = self.clone();
        std::thread::Builder::new()
            .name("pi-voice".into())
            .spawn(move || {
                let result =
                    adapter
                        .run(probe, &controls, &events)
                        .unwrap_or_else(|_| VoiceEvent::Error {
                            code: "bridge".into(),
                        });
                let result = if controls.cancel.load(Relaxed) || controls.quitting() {
                    VoiceEvent::Cancelled
                } else {
                    result
                };
                let _ = events.try_send(result);
            })?;
        Ok((session, receiver))
    }

    fn run(
        &self,
        probe: bool,
        controls: &Controls,
        events: &async_channel::Sender<VoiceEvent>,
    ) -> Result<VoiceEvent> {
        if controls.cancel.load(Relaxed) || controls.quitting() {
            return Ok(VoiceEvent::Cancelled);
        }
        for path in [&self.root, &self.node, &self.package, &self.home] {
            ensure!(
                dunce::canonicalize(path)?.as_path() == path.as_path(),
                "Voice install path changed"
            );
        }
        let mut command = self.isolated_command(&self.node);
        command
            .args(["--no-warnings", "--input-type=module", "-e", SCRIPT, "--"])
            .arg(&self.package)
            .arg(self.mode)
            .arg(&self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        // Hold the verified file until Tree has cleaned up the helper/worker.
        let Ok(_node_guard) = node::verify(&self.node, self.mode) else {
            return Ok(VoiceEvent::Error {
                code: "unsupported".into(),
            });
        };
        if controls.cancel.load(Relaxed) || controls.quitting() {
            return Ok(VoiceEvent::Cancelled);
        }
        let mut tree = Tree::spawn(&mut command)?;
        supervise(&mut tree, probe, controls, events)
    }
}

#[derive(Default)]
struct Controls {
    stop: AtomicBool,
    cancel: AtomicBool,
    shutdown: Option<Arc<AtomicBool>>,
}

impl Controls {
    fn quitting(&self) -> bool {
        self.shutdown
            .as_ref()
            .is_some_and(|signal| signal.load(Relaxed))
    }
}

pub struct VoiceSession(Arc<Controls>);

impl VoiceSession {
    /// Request transcription when recording, idempotent. No blocking IO.
    pub fn stop(&self) {
        self.0.stop.store(true, Relaxed);
    }

    /// Discard this session. Ignore all later events except cancellation.
    /// This does not block for the helper to exit.
    pub fn cancel(&self) {
        self.0.cancel.store(true, Relaxed);
    }
}

impl Drop for VoiceSession {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn supervise(
    tree: &mut Tree,
    probe: bool,
    controls: &Controls,
    events: &async_channel::Sender<VoiceEvent>,
) -> Result<VoiceEvent> {
    let mut input = tree
        .child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("No voice stdin"))?;
    let output = tree
        .child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("No voice stdout"))?;
    let (tx, rx) = std::sync::mpsc::sync_channel(4);
    std::thread::Builder::new()
        .name("pi-voice-reader".into())
        .spawn(move || {
            let mut reader = BufReader::new(output);
            for count in 0..=3 {
                let mut line = Vec::new();
                match reader.by_ref().take(MAX_LINE).read_until(b'\n', &mut line) {
                    Ok(0) => {
                        let _ = tx.send(None);
                        break;
                    }
                    Ok(_) if count < 3 && line.last() == Some(&b'\n') => {}
                    _ => break,
                }
                let Ok(event) = serde_json::from_slice::<VoiceEvent>(&line) else {
                    break;
                };
                if tx.send(Some(event)).is_err() {
                    break;
                }
            }
        })?;
    if controls.cancel.load(Relaxed) || controls.quitting() {
        return Ok(VoiceEvent::Cancelled);
    }
    // An unsupported package can answer and exit before the first write.
    let _ = input.write_all(if probe { b"probe\n" } else { b"start\n" });
    let mut deadline = Instant::now() + if probe { PROBE } else { SESSION };
    let (mut cancelled, mut recording, mut transcribing, mut stopped) =
        (false, false, false, false);
    let mut status = None;
    let terminal = loop {
        if controls.quitting() {
            return Ok(VoiceEvent::Cancelled);
        }
        ensure!(Instant::now() < deadline, "Voice helper timed out");
        if status.is_none() {
            status = tree.exited()?;
        }
        if !cancelled && controls.cancel.load(Relaxed) {
            let _ = input.write_all(b"cancel\n");
            cancelled = true;
            deadline = deadline.min(Instant::now() + GRACE);
        } else if !cancelled
            && recording
            && !transcribing
            && !stopped
            && controls.stop.load(Relaxed)
        {
            // The recording limit may have already finished the helper.
            let _ = input.write_all(b"stop\n");
            stopped = true;
        }
        match rx.recv_timeout(POLL) {
            Ok(Some(VoiceEvent::Recording)) if !probe && !recording => {
                recording = true;
                let _ = events.try_send(VoiceEvent::Recording);
            }
            Ok(Some(VoiceEvent::Transcribing)) if recording && !transcribing => {
                transcribing = true;
                let _ = events.try_send(VoiceEvent::Transcribing);
            }
            Ok(Some(event @ VoiceEvent::Result { .. })) if transcribing => break event,
            Ok(Some(event @ VoiceEvent::Ready)) if probe => break event,
            Ok(Some(event @ (VoiceEvent::Cancelled | VoiceEvent::Error { .. }))) => break event,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            _ => return Err(anyhow!("Invalid voice helper response")),
        }
    };
    drop(input);
    let deadline = deadline.min(Instant::now() + GRACE);
    let mut eof = false;
    while Instant::now() < deadline {
        if controls.quitting() {
            return Ok(VoiceEvent::Cancelled);
        }
        if status.is_none() {
            status = tree.exited()?;
        }
        if !eof {
            match rx.recv_timeout(POLL) {
                Ok(None) => eof = true,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                _ => return Err(anyhow!("Trailing voice helper output")),
            }
        }
        if let Some(status) = status {
            return Ok(
                if status.success()
                    || matches!(&terminal, VoiceEvent::Error { code } if code == "protocol")
                {
                    terminal
                } else {
                    VoiceEvent::Error {
                        code: "bridge".into(),
                    }
                },
            );
        }
        std::thread::sleep(POLL);
    }
    Err(anyhow!("Voice helper did not exit"))
}
