//! Install uses a separate pipe, lifetime and event grammar from capture.
use super::{POLL, Tree, VoiceAdapter, node};
use crate::LaunchSpec;
use anyhow::{Result, anyhow, ensure};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

const SCRIPT: &str = concat!(
    include_str!("bridge.mjs"),
    "\n",
    include_str!("setup.mjs"),
    "\nawait setupMain(...process.argv.slice(1));\n"
);
// The pin allows 20 minutes for runtime plus two hours for model acquisition.
const INSTALL: Duration = Duration::from_secs(150 * 60);
const CLEANUP: Duration = Duration::from_secs(30);
const MAX_RECORD: u64 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceSetupComponent {
    Runtime,
    Model,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceSetupPhase {
    Download,
    Extract,
    Publish,
}

#[derive(Debug, PartialEq, Eq)]
pub enum VoiceSetupEvent {
    Progress {
        component: VoiceSetupComponent,
        phase: VoiceSetupPhase,
        received: u64,
        total: u64,
    },
    Ready,
    /// Cancellation does not roll back a component that has been published.
    Cancelled,
    Error {
        code: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Record {
    Admit {
        component: VoiceSetupComponent,
    },
    Progress {
        component: VoiceSetupComponent,
        phase: VoiceSetupPhase,
        received: u64,
        total: u64,
    },
    Publish {
        workspace: String,
        component: VoiceSetupComponent,
        id: String,
    },
    Ready,
    Cancelled,
    Error {
        code: String,
    },
}

pub struct VoiceSetupSession(Arc<AtomicBool>);

impl VoiceSetupSession {
    /// Request cancellation without blocking. Keep receiving until terminal.
    pub fn cancel(&self) {
        self.0.store(true, Relaxed);
    }
}

impl Drop for VoiceSetupSession {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl VoiceAdapter {
    /// Whether this build and the paired installation support in-app setup.
    /// The helper still revalidates the filesystem before writing.
    pub fn setup_supported(&self) -> bool {
        cfg!(target_os = "linux") && self.mode == "bundled" && self.setup_anchor
    }

    /// Start only the fixed, no-capture setup for this paired backend.
    /// `origin` must be the launch that originally selected this adapter. A set
    /// shutdown signal immediately tears down the helper; it does not wait
    /// for staging cleanup. Keep the handle until a terminal event arrives.
    pub fn start_setup_until_shutdown(
        &self,
        origin: &LaunchSpec,
        shutdown: Arc<AtomicBool>,
    ) -> Result<(VoiceSetupSession, async_channel::Receiver<VoiceSetupEvent>)> {
        let cancel = Arc::new(AtomicBool::new(false));
        let session = VoiceSetupSession(cancel.clone());
        let (sender, events) = async_channel::bounded(2);
        let adapter = self.clone();
        let origin = origin.clone();
        std::thread::Builder::new()
            .name("pi-voice-setup".into())
            .spawn(move || {
                let result = adapter
                    .run_setup(&origin, &cancel, &shutdown, &sender)
                    .unwrap_or_else(|_| setup_error("bridge"));
                let result = if shutdown.load(Relaxed)
                    || (cancel.load(Relaxed) && !is_protocol_error(Some(&result)))
                {
                    VoiceSetupEvent::Cancelled
                } else {
                    result
                };
                // Progress uses at most one slot, including with a stalled UI.
                let _ = sender.try_send(result);
            })?;
        Ok((session, events))
    }

    fn run_setup(
        &self,
        origin: &LaunchSpec,
        cancel: &AtomicBool,
        shutdown: &AtomicBool,
        events: &async_channel::Sender<VoiceSetupEvent>,
    ) -> Result<VoiceSetupEvent> {
        if cancel.load(Relaxed) || shutdown.load(Relaxed) || events.is_closed() {
            return Ok(VoiceSetupEvent::Cancelled);
        }
        // No portable ACL or exclusive-directory publication proof on other
        // platforms. Do not fall back to the pin's destructive publish path.
        if !self.setup_supported() {
            return Ok(setup_error("unsupported"));
        }
        if !self.matches_launch(origin) {
            return Ok(setup_error("backend-changed"));
        }
        for path in [&self.root, &self.node, &self.package, &self.home] {
            ensure!(dunce::canonicalize(path)?.as_path() == path.as_path());
        }
        let Ok(_node_guard) = node::verify(&self.node, self.mode) else {
            return Ok(setup_error("unsupported"));
        };
        if cancel.load(Relaxed) || shutdown.load(Relaxed) || events.is_closed() {
            return Ok(VoiceSetupEvent::Cancelled);
        }
        let mut command = self.isolated_command(&self.node);
        command
            .args(["--no-warnings", "--input-type=module", "-e", SCRIPT, "--"])
            .arg(&self.package)
            .arg(self.mode)
            .arg(&self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        let mut tree = Tree::spawn(&mut command)?;
        supervise(
            &mut tree,
            &self.home.join("config/gentle-ai/voice"),
            cancel,
            shutdown,
            events,
            || self.matches_launch(origin),
        )
    }
}

fn setup_error(code: &str) -> VoiceSetupEvent {
    VoiceSetupEvent::Error { code: code.into() }
}

fn is_protocol_error(event: Option<&VoiceSetupEvent>) -> bool {
    matches!(event, Some(VoiceSetupEvent::Error { code }) if code == "protocol")
}

fn supervise(
    tree: &mut Tree,
    home: &Path,
    cancel: &AtomicBool,
    shutdown: &AtomicBool,
    events: &async_channel::Sender<VoiceSetupEvent>,
    still_origin: impl Fn() -> bool,
) -> Result<VoiceSetupEvent> {
    let mut input = tree
        .child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("No setup stdin"))?;
    let output = tree
        .child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("No setup stdout"))?;
    let (tx, rx) = mpsc::sync_channel(4);
    std::thread::Builder::new()
        .name("pi-voice-setup-reader".into())
        .spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let mut line = Vec::new();
                let value = match reader
                    .by_ref()
                    .take(MAX_RECORD)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) => {
                        let _ = tx.send(None);
                        return;
                    }
                    Ok(_) if line.last() == Some(&b'\n') => serde_json::from_slice::<Record>(&line),
                    _ => return,
                };
                let Ok(record) = value else { return };
                if tx.send(Some(record)).is_err() {
                    return;
                }
            }
        })?;
    if shutdown.load(Relaxed) || cancel.load(Relaxed) || events.is_closed() {
        return Ok(VoiceSetupEvent::Cancelled);
    }
    if !still_origin() {
        return Ok(setup_error("backend-changed"));
    }
    // An unsupported helper can exit before reading the first command.
    let _ = input.write_all(b"install\n");
    let mut input = Some(input);
    let deadline = Instant::now() + INSTALL;
    let mut abort = None;
    let mut terminal = None;
    let mut exit_deadline = None;
    let mut status = None;
    let mut eof = false;
    let mut admitted = None;
    let mut admission_denied = false;
    let mut published = false;
    loop {
        if shutdown.load(Relaxed) {
            return Ok(VoiceSetupEvent::Cancelled);
        }
        let now = Instant::now();
        if abort.is_none() && (cancel.load(Relaxed) || events.is_closed() || now >= deadline) {
            let event = if cancel.load(Relaxed) || events.is_closed() {
                VoiceSetupEvent::Cancelled
            } else {
                setup_error("timeout")
            };
            abort = Some((now + CLEANUP, event));
            if let Some(mut pipe) = input.take() {
                let _ = pipe.write_all(b"cancel\n");
            }
        }
        if abort.as_ref().is_some_and(|(limit, _)| now >= *limit)
            || exit_deadline.is_some_and(|limit| now >= limit)
        {
            return Ok(abort.map_or_else(|| setup_error("bridge"), |(_, event)| event));
        }
        if status.is_none() {
            status = tree.exited()?;
        }
        if let Some(status) = status.filter(|_| eof) {
            if status.code() == Some(1) && is_protocol_error(terminal.as_ref()) {
                return Ok(setup_error("protocol"));
            }
            if let Some((_, event)) = abort {
                return Ok(event);
            }
            return Ok(if status.success() {
                checked_terminal(
                    terminal.ok_or_else(|| anyhow!("Missing setup terminal record"))?,
                    &still_origin,
                )
            } else {
                setup_error("bridge")
            });
        }
        if eof {
            std::thread::sleep(POLL);
            continue;
        }
        let record = match rx.recv_timeout(POLL) {
            Ok(None) if terminal.is_some() => {
                eof = true;
                continue;
            }
            Ok(Some(record)) if terminal.is_none() => record,
            Err(RecvTimeoutError::Timeout) => continue,
            _ => return Ok(abort.map_or_else(|| setup_error("bridge"), |(_, event)| event)),
        };
        match record {
            Record::Admit { component } => {
                if let Some(pipe) = input.as_mut()
                    && !cancel.load(Relaxed)
                    && !shutdown.load(Relaxed)
                    && !events.is_closed()
                    && Instant::now() < deadline
                {
                    // Runtime can be skipped if already installed. Otherwise
                    // only the forward transition from runtime to model is valid.
                    ensure!(
                        !admission_denied
                            && matches!(
                                (admitted, component),
                                (None, _)
                                    | (
                                        Some(VoiceSetupComponent::Runtime),
                                        VoiceSetupComponent::Model
                                    )
                            )
                    );
                    let current = still_origin();
                    if cancel.load(Relaxed)
                        || shutdown.load(Relaxed)
                        || events.is_closed()
                        || Instant::now() >= deadline
                    {
                        continue;
                    }
                    let reply: &[u8] = if current {
                        admitted = Some(component);
                        published = false;
                        match component {
                            VoiceSetupComponent::Runtime => b"admitted-runtime\n",
                            VoiceSetupComponent::Model => b"admitted-model\n",
                        }
                    } else {
                        admission_denied = true;
                        b"admission-denied\n"
                    };
                    pipe.write_all(reply)?;
                }
            }
            Record::Progress {
                component,
                phase,
                received,
                total,
            } => {
                ensure!(received <= total && total <= 9_007_199_254_740_991);
                if abort.is_none() {
                    ensure!(!admission_denied && admitted == Some(component) && !published);
                }
                if abort.is_none() && events.is_empty() {
                    let _ = events.try_send(VoiceSetupEvent::Progress {
                        component,
                        phase,
                        received,
                        total,
                    });
                }
            }
            Record::Publish {
                workspace,
                component,
                id,
            } => {
                if let Some(pipe) = input.as_mut()
                    && !cancel.load(Relaxed)
                    && !shutdown.load(Relaxed)
                    && !events.is_closed()
                    && Instant::now() < deadline
                {
                    ensure!(!admission_denied && admitted == Some(component) && !published);
                    published = true;
                    let reply = publish(home, &workspace, component, &id, &still_origin)?;
                    pipe.write_all(reply)?;
                }
            }
            Record::Ready => {
                ensure!(!admission_denied);
                terminal = Some(VoiceSetupEvent::Ready);
            }
            Record::Cancelled => terminal = Some(VoiceSetupEvent::Cancelled),
            Record::Error { code } => {
                ensure!(matches!(
                    code.as_str(),
                    "unsupported"
                        | "protocol"
                        | "timeout"
                        | "http"
                        | "redirect"
                        | "size"
                        | "integrity"
                        | "archive"
                        | "io"
                        | "unsafe-path"
                        | "invalid-component"
                        | "not-ready"
                        | "backend-changed"
                ));
                terminal = Some(VoiceSetupEvent::Error { code });
            }
        }
        if terminal.is_some() {
            drop(input.take());
            exit_deadline = Some(Instant::now() + CLEANUP);
        }
    }
}

fn checked_terminal(
    event: VoiceSetupEvent,
    still_origin: impl FnOnce() -> bool,
) -> VoiceSetupEvent {
    if event == VoiceSetupEvent::Ready && !still_origin() {
        setup_error("backend-changed")
    } else {
        event
    }
}

pub(super) fn publish(
    home: &Path,
    workspace: &str,
    component: VoiceSetupComponent,
    id: &str,
    still_origin: impl FnOnce() -> bool,
) -> Result<&'static [u8]> {
    let valid_workspace = workspace
        .strip_prefix(".desktop-setup-")
        .is_some_and(|s| s.len() == 6 && s.bytes().all(|b| b.is_ascii_alphanumeric()));
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let valid_id = id.len() <= 128
        && id.bytes().next().is_some_and(alnum)
        && id.bytes().all(|b| alnum(b) || b"._-".contains(&b));
    ensure!(valid_workspace && valid_id);
    let parent = match component {
        VoiceSetupComponent::Runtime => "runtime",
        VoiceSetupComponent::Model => "models",
    };
    let source = home.join(workspace).join(parent).join(id);
    let destination_parent = home.join(parent);
    if !trusted_chain(&source) || !trusted_chain(&destination_parent) {
        return Ok(b"unsafe-path\n");
    }
    if !still_origin() {
        return Ok(b"backend-changed\n");
    }
    match rename_exclusive(&source, &destination_parent.join(id)) {
        Ok(()) => Ok(b"published\n"),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(b"publish-conflict\n")
        }
        Err(_) => Ok(b"publish-error\n"),
    }
}

// Check from the filesystem root downward: once a trusted child of a
// root-owned sticky directory exists, other UIDs cannot replace it. These
// checks and the pathname-based rename do not defend against root or another
// process with this same UID mutating the tree mid-operation. The kernel
// no-replace flag still prevents replacing any entry at the resolved target.
#[cfg(target_os = "linux")]
fn trusted_chain(path: &Path) -> bool {
    use std::{fs, os::unix::fs::MetadataExt};
    // SAFETY: getuid has no arguments and does not access Rust memory.
    let uid = unsafe { libc::getuid() };
    let ancestors: Vec<_> = path.ancestors().collect();
    path.is_absolute()
        && ancestors.into_iter().rev().all(|ancestor| {
            fs::symlink_metadata(ancestor).is_ok_and(|info| {
                info.is_dir()
                    && (info.uid() == 0 || info.uid() == uid)
                    && (info.mode() & 0o022 == 0
                        || (ancestor != path && info.uid() == 0 && info.mode() & 0o1000 != 0))
            })
        })
        && fs::canonicalize(path).is_ok_and(|canonical| canonical == path)
}

#[cfg(not(target_os = "linux"))]
fn trusted_chain(_path: &Path) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn rename_exclusive(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // SAFETY: both arguments are live NUL-terminated path strings. The
    // kernel's no-replace operation fails even for an empty destination.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_exclusive(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_requires_the_origin_to_still_match() {
        assert_eq!(
            checked_terminal(VoiceSetupEvent::Ready, || false),
            setup_error("backend-changed")
        );
        assert_eq!(
            checked_terminal(VoiceSetupEvent::Ready, || true),
            VoiceSetupEvent::Ready
        );
        assert_eq!(
            checked_terminal(VoiceSetupEvent::Cancelled, || panic!("not ready")),
            VoiceSetupEvent::Cancelled
        );
    }
}
