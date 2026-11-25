//! Local dictation belongs to the tab that started it, not the focused pane.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use gpui_kit::*;
use pi_rpc::LaunchSpec;
use pi_rpc::voice::{VoiceAdapter, VoiceEvent, VoiceSession};

use super::SessionView;

pub(super) mod setup;
#[cfg(test)]
mod tests;

static MIC_IN_USE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Default)]
struct Shutdown(Arc<AtomicBool>);

impl Global for Shutdown {}

impl Shutdown {
    fn begin(&self) {
        self.0.store(true, Relaxed);
    }

    fn quitting(&self) -> bool {
        self.0.load(Relaxed)
    }
}

// The detached event consumer owns this even after the originating tab closes.
struct CaptureLease<'a>(&'a AtomicBool);

impl<'a> CaptureLease<'a> {
    fn acquire(busy: &'a AtomicBool) -> Option<Self> {
        busy.compare_exchange(false, true, Relaxed, Relaxed).ok()?;
        Some(Self(busy))
    }
}

impl Drop for CaptureLease<'_> {
    fn drop(&mut self) {
        self.0.store(false, Relaxed);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Status {
    #[default]
    Idle,
    Starting,
    Recording,
    Transcribing,
    Cancelling,
    Busy,
    Unavailable,
    Missing,
    DeviceError,
    Failed,
}

impl Status {
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::Idle => "Start dictation",
            Self::Starting => "Starting dictation…",
            Self::Recording => "Recording · stop and transcribe",
            Self::Transcribing => "Transcribing dictation…",
            Self::Cancelling => "Cancelling dictation…",
            Self::Busy => "Dictation is already in use in another tab. Try again when it ends.",
            Self::Unavailable => "Dictation is unavailable for this Gentle Shell install.",
            Self::Missing => {
                "Voice model or runtime missing. Check /gentle:voice in Gentle Shell’s terminal."
            }
            Self::DeviceError => "Microphone unavailable. Check your input device and permissions.",
            Self::Failed => "Dictation failed. Try again.",
        }
    }

    pub(super) fn can_cancel(self) -> bool {
        matches!(self, Self::Starting | Self::Recording | Self::Transcribing)
    }

    pub(super) fn active(self) -> bool {
        self.can_cancel() || self == Self::Cancelling
    }
}

#[derive(Default)]
pub(super) struct Voice {
    pub(super) launch: Option<(LaunchSpec, VoiceAdapter)>,
    pub(super) status: Status,
    session: Option<VoiceSession>,
    state_epoch: u64,
    state_ready: bool,
    pub(super) setup: setup::Setup,
}

impl Voice {
    pub(super) fn state_epoch(&self) -> u64 {
        self.state_epoch
    }

    pub(super) fn observe_state(&mut self, epoch: u64, success: bool) {
        if epoch == self.state_epoch && success {
            self.state_ready = true;
        }
    }

    pub(super) fn reset_state(&mut self) {
        self.state_epoch = self.state_epoch.wrapping_add(1);
        self.state_ready = false;
        self.setup.reset();
        self.invalidate();
    }

    pub(super) fn reset_backend(&mut self) {
        self.launch = None;
        self.reset_state();
    }

    fn can_start(&self, shutdown: &Shutdown) -> bool {
        self.state_ready && self.status == Status::Starting && !shutdown.quitting()
    }

    fn finish_if_running(&mut self, event: VoiceEvent, shutdown: &Shutdown) -> Option<String> {
        if shutdown.quitting() {
            self.invalidate();
        }
        self.finish(event)
    }

    pub(super) fn invalidate(&mut self) {
        if self.status.active() {
            if let Some(session) = &self.session {
                session.cancel();
            }
            self.status = Status::Cancelling;
        } else {
            self.status = Status::Idle;
        }
    }

    fn progress(&mut self, event: VoiceEvent) {
        match event {
            VoiceEvent::Recording if self.status == Status::Starting => {
                self.status = Status::Recording;
            }
            VoiceEvent::Transcribing if self.status == Status::Recording => {
                self.status = Status::Transcribing;
            }
            _ => {}
        }
    }

    fn finish(&mut self, event: VoiceEvent) -> Option<String> {
        self.session = None;
        let previous = std::mem::take(&mut self.status);
        if !previous.can_cancel() {
            return None;
        }
        match event {
            VoiceEvent::Result { text, .. } if previous == Status::Transcribing => {
                return Some(text);
            }
            VoiceEvent::Cancelled => {}
            VoiceEvent::Error { code } => {
                self.status = match code.as_str() {
                    "unsupported" | "node-version" => Status::Unavailable,
                    "not-installed" | "runtime-invalid" | "model-invalid" => Status::Missing,
                    "no-input-device" | "unsupported-input" | "capture" => Status::DeviceError,
                    _ => Status::Failed,
                };
            }
            _ => self.status = Status::Failed,
        }
        None
    }
}

impl SessionView {
    /// Called from app quit, never on an ordinary window close. Every window
    /// shares the signal, including captures whose originating tab has closed.
    pub(crate) fn shutdown_dictation(cx: &mut App) -> bool {
        cx.default_global::<Shutdown>().begin();
        MIC_IN_USE.load(Relaxed) || setup::SETUP_IN_USE.load(Relaxed)
    }

    pub(super) fn toggle_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shutdown = cx.default_global::<Shutdown>().clone();
        if shutdown.quitting()
            || !self.voice.state_ready
            || !self.backend.is_ready()
            || self.voice.setup.open()
        {
            return;
        }
        if self.voice.status == Status::Recording {
            if let Some(session) = &self.voice.session {
                session.stop();
                self.voice.status = Status::Transcribing;
                cx.notify();
            }
            return;
        }
        if self.voice.status.active() {
            return;
        }
        let Some((spec, pinned)) = self.voice.launch.clone() else {
            self.voice.status = Status::Unavailable;
            cx.notify();
            return;
        };
        let Some(lease) = CaptureLease::acquire(&MIC_IN_USE) else {
            self.voice.status = Status::Busy;
            cx.notify();
            return;
        };
        self.voice.status = Status::Starting;
        let origin_focus = window.focused(cx);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let signal = shutdown.0.clone();
            let adapter = cx
                .background_executor()
                .spawn(async move {
                    if signal.load(Relaxed) {
                        return Err(VoiceEvent::Cancelled);
                    }
                    if !pinned.matches_launch(&spec) {
                        return Err(VoiceEvent::Error {
                            code: "unsupported".into(),
                        });
                    }
                    match pinned
                        .probe_until_shutdown(signal)
                        .unwrap_or_else(|_| bridge_error())
                    {
                        VoiceEvent::Ready => Ok(pinned),
                        error => Err(error),
                    }
                })
                .await;
            let terminal = match adapter {
                Ok(adapter) => {
                    // Gate the start on the same entity that owns the consent and
                    // cancel handle. A close or reset during probe cannot start it.
                    let events = cx.update(|_, cx| {
                        this.update(cx, |this, _| {
                            if !this.voice.can_start(&shutdown) || !this.backend.is_ready() {
                                return Err(VoiceEvent::Cancelled);
                            }
                            let (session, events) = adapter
                                .start_until_shutdown(shutdown.0.clone())
                                .map_err(|_| bridge_error())?;
                            this.voice.session = Some(session);
                            Ok(events)
                        })
                        .unwrap_or(Err(VoiceEvent::Cancelled))
                    });
                    match events {
                        Ok(Ok(events)) => loop {
                            match events.recv().await {
                                Ok(event @ (VoiceEvent::Recording | VoiceEvent::Transcribing)) => {
                                    let _ = this.update(cx, |this, cx| {
                                        this.voice.progress(event);
                                        cx.notify();
                                    });
                                }
                                Ok(terminal) => break terminal,
                                Err(_) => break bridge_error(),
                            }
                        },
                        Ok(Err(error)) => error,
                        Err(_) => VoiceEvent::Cancelled,
                    }
                }
                Err(error) => error,
            };
            // The probe or capture supervisor has already cleaned up when it
            // emits its terminal record. Never free the lease on cancel alone.
            drop(lease);
            // An inactive tab may not be in the current render tree. Use
            // the captured window and weak entity, not the visible tab.
            let _ = cx.update(|window, cx| {
                let _ = this.update(cx, |this, cx| {
                    let missing = this.voice.status == Status::Starting
                        && matches!(&terminal, VoiceEvent::Error { code } if code == "not-installed");
                    this.finish_dictation(terminal, window, cx);
                    let supported = this.backend.is_ready()
                        && this.voice.state_ready
                        && this
                            .voice
                            .launch
                            .as_ref()
                            .is_some_and(|(_, adapter)| adapter.setup_supported());
                    if missing && this.voice.offer_setup(supported) {
                        let focus_now = this.owns_voice_setup_window_focus(window, cx)
                            || (origin_focus.is_none() && window.focused(cx).is_none());
                        this.focus_voice_setup(focus_now, window, cx);
                    }
                });
            });
        })
        .detach();
    }

    pub(super) fn cancel_dictation(&mut self, cx: &mut Context<Self>) {
        self.voice.invalidate();
        cx.notify();
    }

    pub(super) fn finish_dictation(
        &mut self,
        event: VoiceEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shutdown = cx.default_global::<Shutdown>();
        if let Some(text) = self.voice.finish_if_running(event, shutdown) {
            self.insert_dictation(&text, window, cx);
        }
        cx.notify();
    }

    fn insert_dictation(&self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.composer.update(cx, |state, cx| {
            let value = state.value();
            let end = value.len();
            let prefix = if value
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace())
            {
                " "
            } else {
                ""
            };
            let selection = state.selected_range();
            let reversed = state.is_selection_reversed();
            state.set_selected_range(end..end, cx);
            state.insert(format!("{prefix}{text}"), window, cx);
            if selection != (end..end) {
                state.set_selected_range_with_direction(selection, reversed, cx);
            }
        });
    }
}

fn bridge_error() -> VoiceEvent {
    VoiceEvent::Error {
        code: "bridge".into(),
    }
}
