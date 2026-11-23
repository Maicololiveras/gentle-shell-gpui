//! The composer: context line, attachments, text area and its bottom bar.

use std::time::{Duration, Instant};

/// The focus pulse's length.
const PULSE: Duration = Duration::from_millis(460);
/// A typed kiss's flight, the least time between two, and how many fly.
const KISS_LIFE: Duration = Duration::from_millis(900);
const KISS_GAP: Duration = Duration::from_millis(90);
const KISSES_MAX: usize = 10;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputToken, Textarea};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::activity::{ChipPopoverOptions, chip_popover};
use super::dialogs::checkbox_box;
use super::{NARROW, TINY, image_tile, number_badge};
use crate::features::session::SessionView;
use crate::features::session::attachments::PastedText;
use crate::features::session::voice::Status;
use crate::shared::kisses::{HEART, LIPS, noise, paint_sprite};
use crate::shared::ui;
use gentle_core::input_style::InputStyle;
use gentle_core::transcript::format_tokens;

impl SessionView {
    /// How full the context is (0..=100) and its tone, by band
    /// ([`ContextFill`]): faint while calm, amber while filling, red when
    /// full; `None` while calm.
    fn context_level(&self) -> Option<(f32, Option<Hsla>)> {
        use gentle_core::usage::context::ContextFill;
        let percent = self
            .usage
            .context_percent
            .map(|p| p.clamp(0.0, 100.0) as f32)?;
        let tone = match ContextFill::of(f64::from(percent)) {
            ContextFill::Calm => None,
            ContextFill::Filling => Some(ui::warning()),
            ContextFill::Full => Some(ui::danger()),
        };
        Some((percent, tone))
    }

    /// While the agent works a light runs round the composer's action
    /// button (stop, or send while you steer), just outside it, on the live
    /// overlay. Round buttons get a round lap (the orbit's corner radius
    /// clamps to a circle); Bar's framed Stop follows its corners; Prompt's
    /// words get none.
    fn action_ring(&self, style: InputStyle) -> Option<Div> {
        /// How far outside the button the light runs.
        const GAP: f32 = 3.;
        let radius = match style {
            InputStyle::Prompt => return None,
            InputStyle::Bar => rems((6. + GAP) / 16.),
            _ => rems(99.),
        };
        self.busy().then(|| {
            self.live_slots
                .slot("action-ring", move || {
                    ui::orbit(ui::petal(), radius).into_any_element()
                })
                .absolute()
                .top(px(-GAP))
                .left(px(-GAP))
                .right(px(-GAP))
                .bottom(px(-GAP))
        })
    }

    /// The context, after Tern's, drawn as part of the composer's own top
    /// border (one line, not two): from past the box's left corner the
    /// border fills as the conversation uses its window, in the band's
    /// color ([`Self::context_level`]), its percent riding the fill's end
    /// just above the box. The window's size closes the activity line
    /// ([`Self::render_context_caption`]). Work in progress is the light
    /// round the action button ([`Self::action_ring`]), not this line.
    /// Place it in the box (`relative`, 1 px border); `radius` is the box's
    /// corner radius.
    fn render_context_edge(&self, radius: Rems) -> Option<Div> {
        let (percent, alert) = self.context_level()?;
        let fill = alert.unwrap_or(ui::fg().opacity(0.45));
        let used = percent / 100.;
        let tip = self.context_tip(percent);
        Some(
            div()
                .absolute()
                // On the border's own line: `absolute` insets start inside it.
                .top(px(-1.))
                .left(radius)
                .right(radius)
                .h(px(1.))
                .child(div().h_full().w(relative(used)).bg(fill))
                .when(percent >= 1., |d| {
                    d.child(
                        div()
                            .id("context-percent")
                            .absolute()
                            .bottom(px(4.))
                            .left(relative(used))
                            .ml(rems(-16. / 16.))
                            .w(rems(32. / 16.))
                            .text_center()
                            .font_family(ui::mono())
                            .text_size(ui::code_rems(9.))
                            .line_height(rems(11. / 16.))
                            .text_color(alert.unwrap_or(ui::faint()))
                            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                            .child(format!("{percent:.0}%")),
                    )
                }),
        )
    }

    /// The current model's context window, in tokens.
    fn context_window(&self) -> Option<u64> {
        self.models
            .current
            .as_ref()
            .map(|m| m.context_window)
            .filter(|w| *w > 0)
    }

    /// The hover on the context's figures: "Context used: 60K of 1M
    /// tokens (6%)". The tokens are the percent of the window: the backend
    /// reports only the percent.
    fn context_tip(&self, percent: f32) -> SharedString {
        match self.context_window() {
            Some(window) => {
                let used = (window as f64 * f64::from(percent) / 100.).round() as u64;
                format!(
                    "Context used: {} of {} tokens ({percent:.0}%)",
                    format_tokens(used),
                    format_tokens(window)
                )
            }
            None => format!("Context used: {percent:.0}%"),
        }
        .into()
    }

    /// The window's size at the activity line's end, over the context
    /// line's end: "1M", faint until the band's tone takes it; its hover
    /// says how much is used.
    pub(super) fn render_context_caption(&self) -> Option<Stateful<Div>> {
        let (percent, alert) = self.context_level()?;
        let window = format_tokens(self.context_window()?);
        let tip = self.context_tip(percent);
        Some(
            div()
                .id("context-window")
                .flex_none()
                .px_1()
                .font_family(ui::mono())
                .text_size(ui::code_rems(10.))
                .text_color(alert.unwrap_or(ui::faint()))
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                .child(window),
        )
    }

    /// The context percentage as a chip: compact now, toggle auto-compaction.
    pub(super) fn render_context_chip(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let high = self
            .usage
            .context_percent
            .is_some_and(|p| p >= crate::features::alerts::context_threshold(cx));
        let compacting = self.usage.compacting;
        let auto = self.usage.auto_compaction;
        let ready = self.backend.is_ready();
        let used = match self.usage.context_percent {
            Some(p) => format!("{p:.0}%"),
            None => "–".into(),
        };
        let window = self
            .models
            .current
            .as_ref()
            .map(|m| m.context_window)
            .filter(|w| *w > 0)
            .map(format_tokens);
        let label = if compacting {
            "compacting…".to_string()
        } else {
            used.clone()
        };
        let count = match &window {
            Some(window) => format!("{used} of {window}"),
            None => used,
        };
        let color = if high { ui::warning() } else { ui::faint() };
        let weak = cx.entity().downgrade();
        chip_popover(
            ChipPopoverOptions::new("context", IconName::Gauge, color, label)
                .title("Context")
                .count(count),
            move || {
                let compact = weak.clone();
                let toggle = weak.clone();
                vec![
                    div()
                        .text_size(rems(12. / 16.))
                        .text_color(ui::muted())
                        .whitespace_normal()
                        .child(if high {
                            "Context is nearly full. Compacting summarizes the conversation so far to free room."
                        } else {
                            "Compacting summarizes the conversation so far to free room. Add what to keep with /compact <instructions>."
                        })
                        .into_any_element(),
                    h_flex()
                        .pt_1()
                        .child(
                            Button::new("context-compact")
                                .small()
                                .primary()
                                .icon(Icon::new(IconName::ChevronsDownUp))
                                .label(if compacting { "Compacting…" } else { "Compact now" })
                                .loading(compacting)
                                .disabled(compacting || !ready)
                                .on_click(move |_, _, cx| {
                                    let _ = compact.update(cx, |this, cx| this.compact(None, cx));
                                }),
                        )
                        .into_any_element(),
                    h_flex()
                        .id("context-auto-compaction")
                        .gap_2()
                        .pt_1()
                        .cursor_pointer()
                        .text_size(rems(12. / 16.))
                        .text_color(ui::fg())
                        .when(auto.is_none() || !ready, |d| d.opacity(0.5))
                        .child(checkbox_box(auto.unwrap_or(false)))
                        .child("Compact automatically when nearly full")
                        .when_some(auto.filter(|_| ready), |d, enabled| {
                            d.on_click(move |_, _, cx| {
                                let _ = toggle.update(cx, |this, cx| {
                                    this.set_auto_compaction(!enabled, cx)
                                });
                            })
                        })
                        .into_any_element(),
                ]
            },
        )
    }

    /// 64px tiles of what waits to be sent: numbered image thumbnails, then
    /// pasted-text cards; each with ×.
    fn render_attachments(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.attachments.is_empty() && self.pasted.is_empty() {
            return None;
        }
        let images: Vec<_> = self
            .attachments
            .iter()
            .enumerate()
            .map(|(ix, attachment)| {
                image_tile(
                    Some(attachment.image.clone()),
                    rems(64. / 16.),
                    rems(64. / 16.),
                )
                .child(number_badge(ix + 1))
                .child(remove_button(
                    ("attachment-remove", ix),
                    cx.listener(move |this, _, window, cx| this.remove_attachment(ix, window, cx)),
                ))
            })
            .collect();
        let pastes = self.pasted.iter().enumerate().map(|(ix, paste)| {
            pasted_card(ix + 1, paste).child(remove_button(
                ("pasted-remove", ix),
                cx.listener(move |this, _, window, cx| this.remove_pasted(ix, window, cx)),
            ))
        });
        Some(
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_2()
                .children(images)
                .children(pastes),
        )
    }

    /// The text field; a click anywhere in it focuses the text area.
    fn render_text_field(&self, ready: bool, cx: &mut Context<Self>) -> Div {
        let weak = cx.entity().downgrade();
        div()
            .min_w_0()
            .text_size(rems(14. / 16.))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.composer
                        .update(cx, |state, cx| state.focus(window, cx));
                }),
            )
            .child(
                Textarea::new(&self.composer)
                    .appearance(false)
                    .disabled(!ready)
                    // Pasted texts' tokens, as file-icon `#N` chips.
                    .token(|token, _, _| {
                        let chip = InputToken::new(token)
                            .icon(IconName::FileText)
                            .text_color(ui::info());
                        if token.is_selected() {
                            chip
                        } else {
                            chip.bg(ui::info().opacity(0.12))
                                .border_color(ui::info().opacity(0.22))
                        }
                    })
                    .on_paste(move |item, window, cx| {
                        weak.update(cx, |this, cx| this.paste_clipboard(item, window, cx))
                            .unwrap_or(false)
                    }),
            )
    }

    /// The text grew (you typed): blow a small kiss from the caret. At most
    /// one every `KISS_GAP` and `KISSES_MAX` alive, so it stays a sprinkle.
    /// None when Configuration turned them off, or with reduced motion.
    pub(in crate::features::session) fn blow_kiss(&mut self, cx: &mut Context<Self>) {
        let len = self.composer.read(cx).value().len();
        let typed = len > self.composer_len;
        self.composer_len = len;
        if !typed || !crate::shared::kisses::typing(cx).plays(cx.reduce_motion()) {
            return;
        }
        self.kisses.retain(|(at, _, _)| at.elapsed() < KISS_LIFE);
        let recent = self
            .kisses
            .last()
            .is_some_and(|(at, _, _)| at.elapsed() < KISS_GAP);
        if recent || self.kisses.len() >= KISSES_MAX {
            return;
        }
        let Some(caret) = self.composer.read(cx).prepaint_caret_geometry() else {
            return;
        };
        let bounds = caret.cursor_bounds();
        let at = point(bounds.origin.x, bounds.origin.y);
        self.kisses.push((Instant::now(), at, self.kisses_blown));
        self.kisses_blown += 1;
    }

    /// Drop the kisses that landed and end a finished focus pulse; whether
    /// either still plays (the view then asks for the next frame).
    pub(super) fn composer_animating(&mut self) -> bool {
        self.kisses.retain(|(at, _, _)| at.elapsed() < KISS_LIFE);
        if self.composer_pulse.is_some_and(|at| at.elapsed() >= PULSE) {
            self.composer_pulse = None;
        }
        !self.kisses.is_empty() || self.composer_pulse.is_some()
    }

    /// The kisses in the air: each rises from where it was blown, swaying,
    /// pops in small, and fades. Every fourth one is a heart. Painted in
    /// window coordinates, over the composer.
    fn render_kisses(&self) -> Option<impl IntoElement> {
        if self.kisses.is_empty() {
            return None;
        }
        let kisses = self.kisses.clone();
        Some(
            canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    for (born, from, i) in &kisses {
                        let age = born.elapsed().as_secs_f32() / KISS_LIFE.as_secs_f32();
                        if age >= 1. {
                            continue;
                        }
                        let i = *i;
                        let travel = 1. - (1. - age).powi(3);
                        let rise = 26. + 18. * noise(i, 2);
                        let drift = (noise(i, 1) * 2. - 1.) * 12.;
                        let sway = (age * 9. + i as f32).sin() * 4.;
                        let at = point(
                            from.x + px(drift * travel + sway),
                            from.y - px(rise * travel),
                        );
                        let grow = ui::spring((age * 3.).min(1.));
                        let dot = (1.1 + 0.4 * noise(i, 3)) * (0.5 + 0.5 * grow);
                        let alpha = if age > 0.55 { (1. - age) / 0.45 } else { 1. };
                        let heart = i % 4 == 3;
                        let (body, line) = if heart {
                            (ui::petal(), ui::petal())
                        } else {
                            (
                                hsla(340. / 360., 0.85, 0.62, 1.),
                                hsla(340. / 360., 0.70, 0.30, 1.),
                            )
                        };
                        paint_sprite(
                            window,
                            if heart { &HEART } else { &LIPS },
                            at,
                            dot,
                            body.opacity(alpha),
                            line.opacity(alpha),
                        );
                    }
                },
            )
            .absolute()
            .size_0(),
        )
    }

    /// Play the focus pulse (none with reduced motion): frames are asked
    /// for until it's over ([`Self::composer_animating`]). Timed from
    /// `composer_pulse` rather than `with_animation`, whose progress is
    /// lost when the session view is reused from cache, which would replay
    /// the pulse on every later redraw.
    pub(in crate::features::session) fn start_composer_pulse(&mut self, cx: &mut Context<Self>) {
        if !cx.reduce_motion() {
            self.composer_pulse = Some(Instant::now());
        }
    }

    /// The composer's box (background, border, shadow), drawn as a layer
    /// behind its content. During the focus pulse it swells outward on
    /// every side and settles back along one smooth arc. Only this layer
    /// moves (it's absolute), so the text, the buttons and everything
    /// around stay put, and nothing stays bigger.
    fn composer_shell(&self) -> Div {
        /// How far the pulse reaches past the box, on every side.
        const REACH: f32 = 6.;
        let t = self
            .composer_pulse
            .map(|at| (at.elapsed().as_secs_f32() / PULSE.as_secs_f32()).min(1.))
            .unwrap_or(1.);
        let out = px(-REACH * (std::f32::consts::PI * ease_in_out(t)).sin());
        div()
            .absolute()
            .top(out)
            .bottom(out)
            .left(out)
            .right(out)
            .rounded_2xl()
            .border_1()
            .border_color(if self.composer_focused {
                ui::petal().opacity(0.45)
            } else {
                ui::border()
            })
            .bg(ui::surface())
            .shadow_lg()
            .children(self.render_context_edge(rems(1.)))
    }

    /// The composer in the chosen input style, and for Prompt the status
    /// strip the pane draws along its bottom edge, edge to edge.
    pub(super) fn render_composer(
        &self,
        rem_size: Pixels,
        cx: &mut Context<Self>,
    ) -> (AnyElement, Option<AnyElement>) {
        let empty = self.composer.read(cx).value().trim().is_empty() && self.attachments.is_empty();
        let busy = self.busy();
        let ready = self.backend.is_ready();
        let style = crate::features::session::input_style::style(cx);
        let stop_now = self.stoppable() && empty;
        let send_tip = if busy {
            "Steer the running agent"
        } else {
            "Send"
        };

        let action_button = match style {
            InputStyle::Bar => self.bar_action(stop_now, empty || !ready, send_tip, cx),
            InputStyle::Prompt => self.console_action(stop_now, empty || !ready, send_tip, cx),
            InputStyle::Soft => self.soft_action(stop_now, empty || !ready, send_tip, cx),
            _ if stop_now => Button::new("stop")
                .small()
                .danger()
                .rounded(px(999.))
                .child(div().size(rems(9. / 16.)).rounded_sm().bg(ui::well()))
                .tooltip(format!("Stop ({})", ui::shortcut(".")))
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                .into_any_element(),
            _ => Button::new("send")
                .small()
                .primary()
                .rounded(px(999.))
                .icon(Icon::new(IconName::ArrowUp))
                .tooltip(send_tip)
                .disabled(empty || !ready)
                .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                .into_any_element(),
        };

        let width = self.pane_width.get();
        let narrow = width < f32::from(rems(NARROW / 16.).to_pixels(rem_size));
        // The activity line folds every extension status (and the legacy
        // limit chips) into one chip below `TINY`.
        let collapse = width < f32::from(rems(TINY / 16.).to_pixels(rem_size));
        // When the legacy limit chips are on the activity line, the plan
        // chip has nothing to add: one number, never the same percent twice.
        let legacy_chips_shown = !collapse && !self.extension.rate_limits.is_empty();
        let attach = Button::new("attach")
            .ghost()
            .xsmall()
            .text_color(ui::muted())
            // Soft opens with a +, as its round controls do.
            .icon(Icon::new(if style == InputStyle::Soft {
                IconName::Plus
            } else {
                IconName::Paperclip
            }))
            .tooltip("Attach images (or paste / drop them)")
            .disabled(!ready)
            .on_click(cx.listener(|this, _, window, cx| this.pick_images(window, cx)));
        // The model, effort and plan chips every style keeps together.
        let controls = h_flex()
            .min_w_0()
            .flex_shrink(1.)
            .gap_1p5()
            .items_center()
            .child(
                div()
                    .min_w_0()
                    .flex_shrink(1.)
                    .overflow_hidden()
                    .child(self.render_model_picker(narrow, cx)),
            )
            .when(
                self.models.current.as_ref().is_some_and(|m| m.reasoning),
                |this| this.child(self.render_effort_picker(narrow, cx)),
            )
            // The plan-limits chip keeps its width; the model picker before
            // it is the one that shrinks in a narrow pane.
            .when_some(
                self.render_plan_limits_chip(legacy_chips_shown, cx),
                |this, chip| this.child(div().flex_shrink_0().child(chip)),
            );

        // What the session spent, quiet beside the send; a narrow pane
        // leaves it out.
        let usage = (!narrow)
            .then(|| self.render_usage_chip())
            .flatten()
            .map(|chip| div().flex_shrink_0().child(chip));
        let voice = self.voice.status;
        let mic_label = if voice == Status::Idle && !ready {
            Status::Unavailable.message()
        } else {
            voice.message()
        };
        let mic = Button::new("dictation-mic")
            .ghost()
            .xsmall()
            .icon(Icon::new(IconName::Mic))
            .text_color(if voice == Status::Recording {
                ui::danger()
            } else {
                ui::muted()
            })
            .tooltip(mic_label)
            .accessibility_label(mic_label)
            .disabled(
                self.voice.setup.open()
                    || ((!ready || voice.active()) && voice != Status::Recording),
            )
            .on_click(cx.listener(|this, _, window, cx| this.toggle_dictation(window, cx)));
        // Keep dictation beside the existing send/stop action in every style.
        // The pulse still measures and surrounds only the send/stop action.
        let action = h_flex()
            .flex_shrink_0()
            .gap_1()
            .child(mic)
            .when(voice.can_cancel(), |this| {
                this.child(
                    Button::new("dictation-cancel")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::X))
                        .tooltip("Cancel dictation")
                        .accessibility_label("Cancel dictation")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_dictation(cx))),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_shrink_0()
                    .child(action_button)
                    .children(self.action_ring(style)),
            );
        let spacer = || div().flex_1().min_w_0();
        let row = || h_flex().w_full().min_w_0().gap_1p5().items_center();
        // The line or frame a focused composer lights up.
        let edge = |rest: Hsla| {
            if self.composer_focused {
                ui::petal().opacity(0.45)
            } else {
                rest
            }
        };
        let attachments = self.render_attachments(cx);
        let text = self.render_text_field(ready, cx);
        let pad = |d: Div| if narrow { d.px_3() } else { d.px_4() };

        let mut strip = None;
        let composer = match style {
            // A floating card, its controls inside.
            InputStyle::Card => v_flex().w_full().min_w_0().gap_1p5().child(
                div()
                    .relative()
                    .w_full()
                    .min_w_0()
                    .child(self.composer_shell())
                    .child(
                        pad(v_flex())
                            .relative()
                            .w_full()
                            .min_w_0()
                            .overflow_hidden()
                            .gap_2()
                            .pt_4()
                            .pb_2p5()
                            .children(attachments)
                            .child(text)
                            .child(
                                row()
                                    .child(attach)
                                    .child(controls)
                                    .child(spacer())
                                    .children(usage)
                                    .child(action),
                            ),
                    ),
            ),
            // Spine: the pane's dock draws the strip and its hairline (see
            // `Render for SessionView`); here the plain text, then one row:
            // flat model and effort chips, a divider, attach, and a
            // keycap send or a Stop · esc button at the end.
            InputStyle::Bar => v_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .children(attachments)
                .child(div().px_1().min_h(rems(28. / 16.)).child(text))
                .child(
                    row()
                        .gap_1()
                        .child(controls)
                        .child(
                            div()
                                .flex_none()
                                .w(px(1.))
                                .h(rems(14. / 16.))
                                .mx_1()
                                .bg(ui::border()),
                        )
                        .child(attach)
                        .child(spacer())
                        .children(usage)
                        .child(action),
                ),
            // Console: a framed `❯` line in the code font; the model,
            // spend and keys move to a status strip along the pane's
            // bottom edge, as a terminal's status line.
            InputStyle::Prompt => {
                let segment = |d: Div| {
                    d.h_full()
                        .flex()
                        .items_center()
                        .flex_none()
                        .px_2p5()
                        .border_r_1()
                        .border_color(ui::border())
                };
                strip = Some(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .h(rems(26. / 16.))
                        .flex_none()
                        .border_t_1()
                        .border_color(ui::border())
                        .bg(ui::well())
                        // Its bottom corners follow the pane's.
                        .rounded_b_xl()
                        .font_family(ui::mono())
                        .text_size(ui::code_rems(12.))
                        .text_color(ui::faint())
                        .overflow_hidden()
                        .child(
                            segment(div())
                                .rounded_bl_xl()
                                .bg(ui::accent().opacity(0.16))
                                .text_color(ui::accent())
                                .child("❀"),
                        )
                        .child(segment(div()).min_w_0().flex_shrink(1.).child(controls))
                        .child(segment(div()).px_1().child(attach))
                        .child(spacer())
                        .children(usage.map(|usage| {
                            segment(div()).px_1().border_r_0().border_l_1().child(usage)
                        }))
                        .child(segment(div()).border_r_0().border_l_1().child(action))
                        .into_any_element(),
                );
                h_flex()
                    .w_full()
                    .min_w_0()
                    .items_start()
                    .pl_3()
                    .pr_1()
                    .py(rems(2. / 16.))
                    .relative()
                    .rounded_md()
                    .border_1()
                    .border_color(edge(ui::border_strong()))
                    .bg(ui::well())
                    .children(self.render_context_edge(rems(6. / 16.)))
                    .font_family(ui::mono())
                    // On the text's first line: the text area's own top
                    // padding, then a line as tall as its lines.
                    .child(
                        div()
                            .flex_none()
                            .pt(TEXT_PAD)
                            .text_size(rems(14. / 16.))
                            .line_height(relative(TEXT_LINE))
                            .text_color(ui::accent())
                            .child("❯"),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .children(attachments)
                            .child(text),
                    )
            }
            // One rounded pill: attach, text and send on a line; the
            // model and spend in a quiet row below.
            InputStyle::Pill => v_flex()
                .w_full()
                .min_w_0()
                .gap_1p5()
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .items_end()
                        .gap_1()
                        .px_1p5()
                        .relative()
                        .rounded(rems(22. / 16.))
                        .border_1()
                        .border_color(edge(ui::border()))
                        .bg(ui::surface())
                        .children(self.render_context_edge(rems(22. / 16.)))
                        // Attach and send sit centered on the last line of
                        // text: each in a box as tall as one line with the
                        // text area's padding.
                        .child(on_text_line(attach))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_2()
                                .children(attachments.map(|a| div().pt_2().child(a)))
                                .child(text),
                        )
                        .child(on_text_line(action)),
                )
                .child(row().px_2().child(controls).child(spacer()).children(usage)),
            // Soft (after Devin's): a rounded box, the text on top and a
            // row of quiet round controls under it, the spend, the model
            // and a filled send at its end.
            InputStyle::Soft => v_flex().w_full().min_w_0().gap_1p5().child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .px_2()
                    .pt_1p5()
                    .pb_2()
                    .gap_1()
                    .relative()
                    .rounded(rems(20. / 16.))
                    .border_1()
                    .border_color(edge(ui::border()))
                    .bg(ui::surface())
                    .children(self.render_context_edge(rems(20. / 16.)))
                    .children(attachments.map(|a| div().px_2p5().pt_2().child(a)))
                    .child(div().min_h(rems(56. / 16.)).child(text))
                    .child(
                        row()
                            .px_0p5()
                            .child(attach)
                            .child(spacer())
                            .children(usage)
                            .child(controls)
                            .child(action),
                    ),
            ),
        };

        let main = v_flex()
            .w_full()
            .min_w_0()
            .max_w(rems(720. / 16.))
            .gap_1p5()
            .children(self.render_slash_menu(cx))
            .children(self.render_dialog(cx))
            .children(self.render_review_confirm(cx))
            .child(self.render_activity(rem_size, cx))
            .when(voice != Status::Idle, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(ui::muted())
                        .child(voice.message()),
                )
            })
            .child(composer)
            .children(self.render_kisses());
        (main.into_any_element(), strip)
    }

    /// Spine's keys: Stop as a framed button that names its key, send as a
    /// small ↵ keycap.
    fn bar_action(
        &self,
        stop: bool,
        blocked: bool,
        send_tip: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if stop {
            return h_flex()
                .id("stop")
                .h(rems(28. / 16.))
                .px_2p5()
                .gap_1p5()
                .rounded_md()
                .bg(ui::surface())
                .border_1()
                .border_color(ui::border_strong())
                .cursor_pointer()
                .hover(|s| s.bg(ui::surface_hover()))
                .tooltip(move |window, cx| {
                    Tooltip::new(format!("Stop (esc · {})", ui::shortcut("."))).build(window, cx)
                })
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                .child(div().size(rems(8. / 16.)).rounded_sm().bg(ui::fg()))
                .child(
                    div()
                        .text_size(rems(12. / 16.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(ui::fg())
                        .child("Stop"),
                )
                .child(
                    div()
                        .font_family(ui::mono())
                        .text_size(ui::code_rems(11.))
                        .text_color(ui::faint())
                        .child("esc"),
                )
                .into_any_element();
        }
        h_flex()
            .id("send")
            .h(rems(26. / 16.))
            .min_w(rems(30. / 16.))
            .px_2()
            .justify_center()
            .rounded_md()
            .bg(ui::surface())
            .border_1()
            .border_color(ui::border_strong())
            .text_size(rems(12. / 16.))
            .tooltip(move |window, cx| Tooltip::new(send_tip).build(window, cx))
            .map(|d| {
                if blocked {
                    d.text_color(ui::faint())
                } else {
                    d.text_color(ui::muted())
                        .cursor_pointer()
                        .hover(|s| s.bg(ui::surface_hover()).text_color(ui::fg()))
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                }
            })
            .child("↵")
            .into_any_element()
    }

    /// Console's keys, as a terminal's status line names them: `esc stop`
    /// while the agent runs, else `↵ send`.
    fn console_action(
        &self,
        stop: bool,
        blocked: bool,
        send_tip: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (key, word) = if stop {
            ("esc", "stop")
        } else {
            ("↵", "send")
        };
        h_flex()
            .id(if stop { "stop" } else { "send" })
            .gap_1()
            .child(div().text_color(ui::muted()).child(key))
            .child(word)
            .map(|d| {
                if stop {
                    d.cursor_pointer()
                        .hover(|s| s.text_color(ui::danger()))
                        .tooltip(|window, cx| Tooltip::new("Stop the agent").build(window, cx))
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                } else if blocked {
                    d.opacity(0.6)
                } else {
                    d.cursor_pointer()
                        .hover(|s| s.text_color(ui::fg()))
                        .tooltip(move |window, cx| Tooltip::new(send_tip).build(window, cx))
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                }
            })
            .into_any_element()
    }

    /// Soft's send: a filled round button in the text color (the page
    /// color for its arrow), dimmed until there's something to send; a
    /// square while the agent runs stops it.
    fn soft_action(
        &self,
        stop: bool,
        blocked: bool,
        send_tip: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let button = h_flex()
            .id(if stop { "stop" } else { "send" })
            .size(rems(28. / 16.))
            .flex_none()
            .justify_center()
            .rounded_full()
            .bg(ui::fg());
        if stop {
            return button
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .tooltip(move |window, cx| {
                    Tooltip::new(format!("Stop ({})", ui::shortcut("."))).build(window, cx)
                })
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                .child(div().size(rems(9. / 16.)).rounded_sm().bg(ui::screen_bg()))
                .into_any_element();
        }
        button
            .tooltip(move |window, cx| Tooltip::new(send_tip).build(window, cx))
            .map(|d| {
                if blocked {
                    d.opacity(0.3)
                } else {
                    d.cursor_pointer()
                        .hover(|s| s.opacity(0.85))
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
                }
            })
            .child(
                Icon::new(IconName::ArrowUp)
                    .small()
                    .text_color(ui::screen_bg()),
            )
            .into_any_element()
    }
}

/// The composer text area's own padding above and below its text
/// (gpui-component's multi-line padding at the default size).
const TEXT_PAD: Pixels = px(8.);
/// The text area's line height, relative to its 14 px text.
const TEXT_LINE: f32 = 1.5;

/// `element` centered on one line of the composer's text, padding
/// included: beside a text area it lines up with its line.
fn on_text_line(element: impl IntoElement) -> Div {
    div().flex_none().py(TEXT_PAD).child(
        div()
            .h(rems(14. * TEXT_LINE / 16.))
            .flex()
            .items_center()
            .child(element),
    )
}

/// The × in an attachment's top-right corner.
fn remove_button(
    id: impl Into<ElementId>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    h_flex()
        .id(id)
        .absolute()
        .top(rems(3. / 16.))
        .right(rems(3. / 16.))
        .size(rems(18. / 16.))
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(ui::well())
        .cursor_pointer()
        .hover(|s| s.bg(ui::danger()))
        .tooltip(|window, cx| Tooltip::new("Remove").build(window, cx))
        .on_click(on_click)
        .child(Icon::new(IconName::X).xsmall().text_color(ui::fg()))
}

/// Pasted text `n` waiting to be sent: a page with its first lines in tiny
/// type, then `#N` and how many lines it has.
fn pasted_card(n: usize, paste: &PastedText) -> Div {
    let lines = match paste.lines {
        1 => "+1 line".to_string(),
        lines => format!("+{lines} lines"),
    };
    h_flex()
        .relative()
        .flex_shrink_0()
        .h(rems(64. / 16.))
        .pl(rems(7. / 16.))
        .pr(rems(30. / 16.))
        .gap_2p5()
        .rounded_lg()
        .border_1()
        .border_color(ui::border_strong())
        .bg(ui::surface())
        .child(
            div()
                .flex_shrink_0()
                .w(rems(38. / 16.))
                .h(rems(48. / 16.))
                .p(rems(4. / 16.))
                .rounded_md()
                .overflow_hidden()
                .border_1()
                .border_color(ui::border())
                .bg(ui::well())
                .font_family(ui::mono())
                .text_size(ui::code_rems(5.))
                .line_height(ui::code_rems(6.))
                .text_color(ui::faint())
                .whitespace_nowrap()
                .child(paste.preview.clone()),
        )
        .child(
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::FileText)
                                .xsmall()
                                .text_color(ui::muted()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(ui::fg())
                                .child(format!("#{n}")),
                        ),
                )
                .child(
                    div()
                        .text_size(rems(11. / 16.))
                        .text_color(ui::muted())
                        .child(lines),
                ),
        )
}
