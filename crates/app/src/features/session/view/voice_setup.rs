//! The native setup screen follows the session and uses the live app theme.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pi_rpc::voice::{VoiceSetupComponent, VoiceSetupPhase};

use crate::features::session::{SessionView, voice::setup::Stage};
use crate::shared::ui;

impl SessionView {
    pub(super) fn on_voice_setup_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(focus) = self.voice.setup.focus.as_ref() else {
            return;
        };
        cx.stop_propagation();
        let modifiers = event.keystroke.modifiers;
        if modifiers.platform || modifiers.control || modifiers.alt {
            return;
        }
        let offer = self.voice.setup.stage == Stage::Offer;
        match event.keystroke.key.as_str() {
            "escape" => self.dismiss_voice_setup(window, cx),
            "tab" => {
                let next = if offer && focus.dismiss.is_focused(window) {
                    &focus.install
                } else {
                    &focus.dismiss
                };
                window.focus(next, cx);
                cx.notify();
            }
            "enter" | "space" if offer && focus.install.is_focused(window) => {
                self.confirm_voice_setup(window, cx);
            }
            "enter" | "space" if focus.dismiss.is_focused(window) => {
                self.dismiss_voice_setup(window, cx);
            }
            _ => {}
        }
    }

    pub(super) fn render_voice_setup(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let stage = &self.voice.setup.stage;
        let (title, message, dismiss) = match stage {
            Stage::Closed => return None,
            Stage::Offer => (
                "Set up dictation",
                "Gentle Shell needs a speech model and runtime to transcribe locally. Desktop will download only the missing components for the Gentle Shell installation used by this tab.",
                "Not now",
            ),
            Stage::Installing | Stage::Progress(..) => (
                "Installing voice components…",
                "Downloading and verifying the components for this Gentle Shell installation.",
                "Cancel",
            ),
            Stage::Cancelling => (
                "Cancelling voice setup…",
                "Waiting for the installer to stop.",
                "Cancel",
            ),
            Stage::Cancelled => (
                "Voice setup cancelled",
                "Any components already installed were kept. Click the microphone to check what is still needed.",
                "Done",
            ),
            Stage::Ready => (
                "Dictation is ready",
                "Close this window and click the microphone when you want to record.",
                "Done",
            ),
            Stage::Failed(message) => ("Voice setup didn’t finish", *message, "Done"),
        };
        let focus = self.voice.setup.focus.as_ref()?;
        let focus_frame = |handle: &FocusHandle| {
            div()
                .track_focus(handle)
                .rounded_lg()
                .border_1()
                .border_color(if handle.is_focused(window) {
                    ui::petal()
                } else {
                    ui::petal().opacity(0.)
                })
        };
        let footer = h_flex()
            .justify_end()
            .flex_wrap()
            .gap_2()
            .px_5()
            .py_3()
            .border_t_1()
            .border_color(ui::border())
            .child(
                focus_frame(&focus.dismiss).child(
                    Button::new("voice-setup-dismiss")
                        .label(dismiss)
                        .disabled(*stage == Stage::Cancelling)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dismiss_voice_setup(window, cx);
                        })),
                ),
            )
            .when(*stage == Stage::Offer, |footer| {
                footer.child(
                    focus_frame(&focus.install).child(
                        Button::new("voice-setup-install")
                            .label("Install")
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_voice_setup(window, cx);
                            })),
                    ),
                )
            });
        let (backdrop, panel) = ui::screen(Some(440.));
        Some(
            backdrop.occlude().child(
                panel
                    .max_w_full()
                    .font_family(cx.theme().font_family.clone())
                    .text_color(ui::fg())
                    .child(
                        v_flex()
                            .id("voice-setup-body")
                            .overflow_y_scroll()
                            .p_5()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(rems(42. / 16.))
                                    .flex_shrink_0()
                                    .rounded_lg()
                                    .bg(ui::well())
                                    .text_color(ui::petal())
                                    .child(Icon::new(IconName::Mic)),
                            )
                            .child(
                                div()
                                    .text_size(rems(22. / 16.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .child(div().text_sm().text_color(ui::muted()).child(message))
                            .children(progress(stage)),
                    )
                    .child(footer),
            ),
        )
    }
}

fn progress(stage: &Stage) -> Option<Div> {
    let (label, fraction) = match *stage {
        Stage::Installing => ("Checking this installation…".to_owned(), None),
        Stage::Progress(component, phase, received, total) => {
            let component = match component {
                VoiceSetupComponent::Runtime => "runtime",
                VoiceSetupComponent::Model => "speech model",
            };
            let phase = match phase {
                VoiceSetupPhase::Download => "Downloading",
                VoiceSetupPhase::Extract => "Extracting",
                VoiceSetupPhase::Publish => "Installing",
            };
            let fraction = (total > 0).then(|| (received as f64 / total as f64).clamp(0., 1.));
            let label = match fraction {
                Some(fraction) => format!("{phase} {component} · {:.0}%", fraction * 100.),
                None => format!("{phase} {component}…"),
            };
            (label, fraction)
        }
        _ => return None,
    };
    Some(
        v_flex()
            .gap_2()
            .text_sm()
            .text_color(ui::muted())
            .child(
                h_flex()
                    .gap_2()
                    .when(fraction.is_none(), |row| {
                        row.child(Spinner::new().xsmall().color(ui::petal()))
                    })
                    .child(label),
            )
            .when_some(fraction, |status, fraction| {
                status.child(
                    div()
                        .w_full()
                        .h_1()
                        .rounded_full()
                        .overflow_hidden()
                        .bg(ui::well())
                        .child(div().h_full().w(relative(fraction as f32)).bg(ui::petal())),
                )
            }),
    )
}
