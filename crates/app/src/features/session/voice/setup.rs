//! Consent, progress and cancellation belong to the originating tab.

use super::{CaptureLease, Shutdown, Status, Voice};
use crate::features::session::SessionView;
use gpui_kit::*;
use pi_rpc::voice::{VoiceSetupComponent, VoiceSetupEvent, VoiceSetupPhase, VoiceSetupSession};
use std::sync::atomic::AtomicBool;

pub(super) static SETUP_IN_USE: AtomicBool = AtomicBool::new(false);

#[derive(Default, PartialEq, Eq, Debug)]
pub(in crate::features::session) enum Stage {
    #[default]
    Closed,
    Offer,
    Installing,
    Progress(VoiceSetupComponent, VoiceSetupPhase, u64, u64),
    Cancelling,
    Cancelled,
    Ready,
    Failed(&'static str),
}

impl Stage {
    fn installing(&self) -> bool {
        matches!(self, Self::Installing | Self::Progress(..))
    }
}

#[derive(Default)]
pub(in crate::features::session) struct Setup {
    pub(in crate::features::session) stage: Stage,
    attempt: u64,
    session: Option<VoiceSetupSession>,
    pub(in crate::features::session) focus: Option<SetupFocus>,
}

pub(in crate::features::session) struct SetupFocus {
    pub dismiss: FocusHandle,
    pub install: FocusHandle,
    previous: Option<FocusHandle>,
    deferred_prompt: Option<FocusHandle>,
}

impl SetupFocus {
    fn owns_focus(&self, window: &Window) -> bool {
        self.dismiss.is_focused(window) || self.install.is_focused(window)
    }
}

impl Setup {
    pub(in crate::features::session) fn open(&self) -> bool {
        self.stage != Stage::Closed
    }

    pub(super) fn reset(&mut self) {
        // Dropping the handle requests cancellation; later events cannot
        // update this closed dialog or a subsequently confirmed attempt.
        self.session = None;
        self.stage = Stage::Closed;
    }

    pub(super) fn cancel(&mut self) {
        if self.stage.installing() {
            if let Some(session) = &self.session {
                session.cancel();
            }
            self.stage = Stage::Cancelling;
        }
    }

    pub(super) fn can_launch(&self, attempt: u64) -> bool {
        self.attempt == attempt && self.stage == Stage::Installing
    }

    pub(super) fn apply(&mut self, attempt: u64, event: VoiceSetupEvent) -> bool {
        if self.attempt != attempt || !(self.stage.installing() || self.stage == Stage::Cancelling)
        {
            return false;
        }
        if let VoiceSetupEvent::Progress {
            component,
            phase,
            received,
            total,
        } = event
        {
            if self.stage == Stage::Cancelling {
                return false;
            }
            self.stage = Stage::Progress(component, phase, received, total);
            return true;
        }
        self.session = None;
        self.stage = if self.stage == Stage::Cancelling {
            Stage::Cancelled
        } else {
            match event {
                VoiceSetupEvent::Ready => Stage::Ready,
                VoiceSetupEvent::Cancelled => Stage::Cancelled,
                VoiceSetupEvent::Error { code } => Stage::Failed(match code.as_str() {
                    "backend-changed" => {
                        "The Gentle Shell installation changed. Restart this tab and try again."
                    }
                    "busy" => {
                        "Voice setup is already running in another tab. Try again when it ends."
                    }
                    "unsupported" => {
                        "This Gentle Shell installation does not support setup from Desktop."
                    }
                    "invalid-component" | "unsafe-path" => {
                        "Desktop could not safely install the voice components. Check /gentle:voice in Gentle Shell’s terminal."
                    }
                    "timeout" => "Voice setup timed out. Close this window and try again.",
                    _ => {
                        "Desktop could not finish installing or verifying the voice components. Close this window and try again."
                    }
                }),
                VoiceSetupEvent::Progress { .. } => unreachable!(),
            }
        };
        true
    }
}

impl Voice {
    pub(super) fn offer_setup(&mut self, supported: bool) -> bool {
        if self.status != Status::Missing || !supported || self.setup.open() {
            return false;
        }
        self.setup.stage = Stage::Offer;
        true
    }

    pub(super) fn begin_setup(&mut self, ready: bool) -> Option<u64> {
        if !ready || !self.state_ready || self.setup.stage != Stage::Offer {
            return None;
        }
        self.setup.attempt = self.setup.attempt.wrapping_add(1);
        self.setup.stage = Stage::Installing;
        Some(self.setup.attempt)
    }

    pub(super) fn apply_setup(&mut self, attempt: u64, event: VoiceSetupEvent) -> bool {
        if !self.setup.apply(attempt, event) {
            return false;
        }
        if self.setup.stage == Stage::Ready && self.status == Status::Missing {
            self.status = Status::Idle;
        }
        true
    }
}

impl SessionView {
    pub(super) fn focus_voice_setup(
        &mut self,
        take_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = SetupFocus {
            dismiss: cx.focus_handle(),
            install: cx.focus_handle(),
            previous: take_focus.then(|| window.focused(cx)).flatten(),
            deferred_prompt: None,
        };
        if take_focus {
            window.focus(&focus.dismiss, cx);
        }
        self.voice.setup.focus = Some(focus);
    }

    pub(in crate::features::session) fn sync_voice_setup_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.voice.setup.open() {
            if self.owns_voice_setup_window_focus(window, cx)
                && let Some(focus) = &mut self.voice.setup.focus
                && !focus.owns_focus(window)
            {
                focus.previous = window.focused(cx);
                window.focus(&focus.dismiss, cx);
            }
        } else if let Some(focus) = self.voice.setup.focus.take()
            && focus.owns_focus(window)
        {
            let prompt = if self.extension.dialogs.is_empty() {
                None
            } else {
                focus.deferred_prompt
            };
            if let Some(previous) = prompt.or(focus.previous) {
                window.focus(&previous, cx);
            } else {
                self.focus_composer(window, cx);
            }
        }
    }

    pub(in crate::features::session) fn owns_voice_setup_window_focus(
        &self,
        window: &Window,
        cx: &App,
    ) -> bool {
        self.focus_handle.contains_focused(window, cx)
            || self.focus_handle.is_focused(window)
            || self.composer.focus_handle(cx).is_focused(window)
            || self.dialog_input.focus_handle(cx).is_focused(window)
            || self
                .voice
                .setup
                .focus
                .as_ref()
                .is_some_and(|focus| focus.owns_focus(window))
    }

    pub(in crate::features::session) fn focus_extension_prompt(
        &mut self,
        target: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.voice.setup.open() {
            window.focus(&target, cx);
            return;
        }
        let owns_focus = self.owns_voice_setup_window_focus(window, cx);
        if let Some(focus) = &mut self.voice.setup.focus {
            focus.deferred_prompt = Some(target);
            if owns_focus && !focus.owns_focus(window) {
                window.focus(&focus.dismiss, cx);
            }
        }
    }

    pub(in crate::features::session) fn dismiss_voice_setup(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.voice.setup.stage.installing() {
            self.voice.setup.cancel();
        } else if self.voice.setup.stage != Stage::Cancelling {
            self.voice.setup.reset();
            self.sync_voice_setup_focus(window, cx);
        }
        cx.notify();
    }

    pub(in crate::features::session) fn confirm_voice_setup(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shutdown = cx.default_global::<Shutdown>().clone();
        let Some((spec, pinned)) = self.voice.launch.clone() else {
            return;
        };
        if !pinned.setup_supported() {
            return;
        }
        let Some(attempt) = self
            .voice
            .begin_setup(!shutdown.quitting() && self.backend.is_ready())
        else {
            return;
        };
        if let Some(focus) = &self.voice.setup.focus {
            window.focus(&focus.dismiss, cx);
        }
        cx.notify();
        let Some(lease) = CaptureLease::acquire(&SETUP_IN_USE) else {
            self.voice.apply_setup(attempt, error("busy"));
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let resolved = cx
                .background_executor()
                .spawn(async move { pinned.matches_launch(&spec).then_some((spec, pinned)) })
                .await;
            let events = match resolved {
                Some((spec, pinned)) => cx
                    .update(|_, cx| {
                        this.update(cx, |this, _| {
                            if shutdown.quitting()
                                || !this.backend.is_ready()
                                || !this.voice.state_ready
                                || !this.voice.setup.can_launch(attempt)
                            {
                                return Err(VoiceSetupEvent::Cancelled);
                            }
                            let (session, events) = pinned
                                .start_setup_until_shutdown(&spec, shutdown.0.clone())
                                .map_err(|_| error("bridge"))?;
                            this.voice.setup.session = Some(session);
                            Ok(events)
                        })
                        .unwrap_or(Err(VoiceSetupEvent::Cancelled))
                    })
                    .unwrap_or(Err(VoiceSetupEvent::Cancelled)),
                None => Err(error("backend-changed")),
            };
            let terminal = match events {
                Ok(events) => loop {
                    match events.recv().await {
                        Ok(event @ VoiceSetupEvent::Progress { .. }) => {
                            let _ = this.update(cx, |this, cx| {
                                if !shutdown.quitting() && this.voice.apply_setup(attempt, event) {
                                    cx.notify();
                                }
                            });
                        }
                        Ok(event) => break event,
                        Err(_) => break error("bridge"),
                    }
                },
                Err(event) => event,
            };
            drop(lease);
            let _ = this.update(cx, |this, cx| {
                if !shutdown.quitting() && this.voice.apply_setup(attempt, terminal) {
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

fn error(code: &str) -> VoiceSetupEvent {
    VoiceSetupEvent::Error { code: code.into() }
}
