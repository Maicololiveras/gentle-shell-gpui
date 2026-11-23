use super::{CaptureLease, Shutdown, Status, Voice};
use pi_rpc::voice::VoiceEvent;
use std::sync::atomic::AtomicBool;

#[test]
fn cancellation_discards_late_results_and_keeps_the_app_lease_until_cleanup() {
    let busy = AtomicBool::new(false);
    let lease = CaptureLease::acquire(&busy).unwrap();
    let mut voice = Voice {
        status: Status::Starting,
        ..Voice::default()
    };
    voice.progress(VoiceEvent::Recording);
    assert_eq!(voice.status, Status::Recording);
    voice.progress(VoiceEvent::Transcribing);
    assert_eq!(voice.status, Status::Transcribing);
    voice.invalidate();
    assert_eq!(voice.status, Status::Cancelling);
    voice.progress(VoiceEvent::Transcribing);
    assert_eq!(voice.status, Status::Cancelling);
    assert!(CaptureLease::acquire(&busy).is_none());
    assert!(voice.finish(result("late")).is_none());
    assert_eq!(voice.status, Status::Idle);
    drop(lease);
    assert!(CaptureLease::acquire(&busy).is_some());
}

#[test]
fn terminal_events_offer_only_current_text_or_fixed_status() {
    let mut voice = Voice::default();
    for (code, status) in [
        ("not-installed", Status::Missing),
        ("no-input-device", Status::DeviceError),
        ("unsupported", Status::Unavailable),
        ("unexpected private detail", Status::Failed),
    ] {
        voice.status = Status::Starting;
        assert!(
            voice
                .finish(VoiceEvent::Error { code: code.into() })
                .is_none()
        );
        assert_eq!(voice.status, status);
    }
    voice.status = Status::Transcribing;
    assert_eq!(
        voice.finish(result(" spoken text ")).as_deref(),
        Some(" spoken text ")
    );
    assert_eq!(voice.status, Status::Idle);
}

#[test]
fn quit_closes_the_probe_gate_and_discards_an_active_result_across_tabs() {
    let shutdown = Shutdown::default();
    let mut pending = Voice {
        status: Status::Starting,
        ..Voice::default()
    };
    pending.observe_state(pending.state_epoch(), true);
    let mut active = Voice {
        status: Status::Transcribing,
        ..Voice::default()
    };
    assert!(pending.can_start(&shutdown));
    shutdown.begin();
    assert!(!pending.can_start(&shutdown));
    assert!(
        active
            .finish_if_running(result("late"), &shutdown)
            .is_none()
    );
    assert_eq!(active.status, Status::Idle);
}

#[test]
fn a_pending_capture_cannot_start_before_the_backend_answers_get_state() {
    let shutdown = Shutdown::default();
    let mut pending = Voice {
        status: Status::Starting,
        ..Voice::default()
    };
    let previous = pending.state_epoch();
    assert!(!pending.can_start(&shutdown));
    pending.observe_state(previous, false);
    assert!(!pending.can_start(&shutdown));
    pending.observe_state(previous, true);
    assert!(pending.can_start(&shutdown));
    pending.observe_state(previous, false);
    assert!(pending.can_start(&shutdown));

    pending.reset_state();
    pending.finish(VoiceEvent::Cancelled);
    pending.status = Status::Starting;
    pending.observe_state(previous, true);
    assert!(!pending.can_start(&shutdown));
    pending.observe_state(pending.state_epoch(), true);
    assert!(pending.can_start(&shutdown));
}

#[test]
fn setup_requires_missing_components_support_and_explicit_live_consent() {
    use super::setup::Stage;
    let mut voice = Voice::default();
    assert!(!voice.offer_setup(true));
    voice.status = Status::Missing;
    assert!(!voice.offer_setup(false));
    assert!(voice.offer_setup(true));
    assert_eq!(voice.setup.stage, Stage::Offer);
    assert_eq!(voice.begin_setup(true), None);
    voice.observe_state(voice.state_epoch(), true);
    assert_eq!(voice.begin_setup(false), None);
    let attempt = voice.begin_setup(true).unwrap();
    assert!(voice.setup.can_launch(attempt));
    assert_eq!(voice.begin_setup(true), None);
    assert_eq!(voice.status, Status::Missing);
}

#[test]
fn setup_cancellation_and_reset_reject_late_success_without_starting_capture() {
    use super::setup::Stage;
    use pi_rpc::voice::{VoiceSetupComponent, VoiceSetupEvent, VoiceSetupPhase};
    let mut voice = Voice::default();
    voice.observe_state(voice.state_epoch(), true);
    voice.status = Status::Missing;
    assert!(voice.offer_setup(true));
    let old = voice.begin_setup(true).unwrap();
    let progress = || VoiceSetupEvent::Progress {
        component: VoiceSetupComponent::Model,
        phase: VoiceSetupPhase::Download,
        received: 42,
        total: 100,
    };
    assert!(voice.setup.apply(old, progress()));
    assert_eq!(
        voice.setup.stage,
        Stage::Progress(
            VoiceSetupComponent::Model,
            VoiceSetupPhase::Download,
            42,
            100
        ),
    );
    voice.setup.cancel();
    assert!(!voice.setup.can_launch(old));
    assert!(!voice.setup.apply(old, progress()));
    assert!(voice.apply_setup(old, VoiceSetupEvent::Ready));
    assert_eq!(voice.setup.stage, Stage::Cancelled);
    assert_eq!(voice.status, Status::Missing);
    voice.reset_state();
    assert!(!voice.setup.apply(old, VoiceSetupEvent::Ready));
    voice.observe_state(voice.state_epoch(), true);
    voice.status = Status::Missing;
    assert!(voice.offer_setup(true));
    let current = voice.begin_setup(true).unwrap();
    assert_ne!(old, current);
    assert!(!voice.apply_setup(old, VoiceSetupEvent::Ready));
    assert_eq!(voice.status, Status::Missing);
    assert!(voice.apply_setup(current, VoiceSetupEvent::Ready));
    assert_eq!(voice.setup.stage, Stage::Ready);
    assert_eq!(voice.status, Status::Idle);
    voice.setup.reset();
    assert_eq!(voice.status.message(), "Start dictation");
}

#[test]
fn setup_reports_backend_drift_as_a_restart_without_clearing_missing_status() {
    use super::setup::Stage;
    use pi_rpc::voice::VoiceSetupEvent;
    let mut voice = Voice::default();
    voice.observe_state(voice.state_epoch(), true);
    voice.status = Status::Missing;
    assert!(voice.offer_setup(true));
    let attempt = voice.begin_setup(true).unwrap();
    assert!(voice.apply_setup(
        attempt,
        VoiceSetupEvent::Error {
            code: "backend-changed".into()
        },
    ));
    assert_eq!(
        voice.setup.stage,
        Stage::Failed("The Gentle Shell installation changed. Restart this tab and try again."),
    );
    assert_eq!(voice.status, Status::Missing);
}

#[gpui::test]
fn setup_keeps_dialog_focus_in_its_tab_and_restores_the_pending_prompt(
    cx: &mut gpui_kit::TestAppContext,
) {
    use crate::features::session::{SessionView, view_tests::tab};
    use gpui_kit::AppContext as _;
    use pi_rpc::{Incoming, UiMethod, UiRequest};

    let (origin, cx) = tab(cx);
    let prompt = || {
        Incoming::Ui(UiRequest {
            id: "confirm-setup-focus".into(),
            timeout: None,
            method: UiMethod::Confirm {
                title: "Confirm".into(),
                message: "Continue?".into(),
            },
        })
    };
    cx.update(|window, cx| {
        origin.update(cx, |view, cx| {
            view.focus_composer(window, cx);
            view.voice.status = Status::Missing;
            assert!(view.voice.offer_setup(true));
            view.focus_voice_setup(true, window, cx);
            let dismiss = view.voice.setup.focus.as_ref().unwrap().dismiss.clone();
            view.handle_batch(vec![prompt()], window, cx);
            assert!(dismiss.is_focused(window));
            view.dismiss_voice_setup(window, cx);
            assert!(view.focus_handle.is_focused(window));
            view.answer_dialog(pi_rpc::UiAnswer::Cancelled, window, cx);
        });
        let other = cx.new(|cx| SessionView::new(std::env::temp_dir(), None, window, cx, false));
        other
            .read(cx)
            .composer()
            .update(cx, |state, cx| state.focus(window, cx));
        let other_focus = window.focused(cx);
        origin.update(cx, |view, cx| {
            assert!(view.voice.offer_setup(true));
            view.focus_voice_setup(false, window, cx);
            view.handle_batch(vec![prompt()], window, cx);
            view.sync_voice_setup_focus(window, cx);
            assert_eq!(window.focused(cx), other_focus);
            view.dismiss_voice_setup(window, cx);
            assert_eq!(window.focused(cx), other_focus);
        });
    });
}

fn result(text: &str) -> VoiceEvent {
    VoiceEvent::Result {
        text: text.into(),
        truncated: false,
    }
}
