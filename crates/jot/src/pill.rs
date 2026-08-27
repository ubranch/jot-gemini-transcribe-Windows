// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The pill: the only thing Jot draws while you dictate.
//!
//! On Windows the pill is deliberately **informational only** — it is a
//! click-through, never-activating overlay. macOS gets a hover-to-dictate idle
//! dot and an in-pill Stop button; here those would mean an always-on-top window
//! that swallows clicks across the bottom of every screen, or one that steals
//! focus from the app the text is about to land in. Stopping a hands-free
//! session is the dictation key, or "Stop dictation" in the notification area.

use crate::theme::{self, Theme};
use gpui::{Context, Rgba, SharedString, Window, div, prelude::*, px, rgba};
use jot_core::state_machine::{DictationOutcome, DictationState, SilenceReason};
use std::time::{Duration, Instant};

/// The pill's semantic state — a pure projection of coordinator state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PillState {
    #[default]
    Hidden,
    IdleDot,
    Listening {
        locked: bool,
    },
    Processing,
    Success {
        words: usize,
    },
    /// Neutral informational chip (coaching hint, copied-to-clipboard, offline).
    Notice(SharedString),
    /// Error styling: an error-container surface and "saved to History" framing.
    Error(SharedString),
}

const PILL_HEIGHT: f32 = 48.0;
const LISTENING_WIDTH: f32 = 200.0;
const LOCKED_WIDTH: f32 = 268.0;
const PROCESSING_WIDTH: f32 = 132.0;
const PROCESSING_SLOW_WIDTH: f32 = 220.0;
const MAX_CHIP_WIDTH: f32 = 560.0;

// Waveform geometry.
const BAR_COUNT: usize = 5;
const BAR_WIDTH: f32 = 6.0;
const BAR_GAP: f32 = 4.0;
const BAR_MIN_HEIGHT: f32 = 6.0;
const BAR_MAX_HEIGHT: f32 = 38.0;
/// Per-bar personality: the centre bar leads, its neighbours follow.
const BAR_WEIGHTS: [f32; BAR_COUNT] = [0.6, 0.88, 1.0, 0.8, 0.55];
const BAR_PHASES: [f32; BAR_COUNT] = [0.0, 0.9, 1.7, 2.6, 3.4];
/// Redraw budget. The fastest visual term is a 2.4–4.6 Hz per-bar shimmer while
/// listening and a ~0.7 Hz chase while processing, so full display rate buys
/// nothing but frames.
const LISTENING_FRAME: Duration = Duration::from_millis(1000 / 24);
const PROCESSING_FRAME: Duration = Duration::from_millis(1000 / 12);

/// Fast-attack, slow-release smoothing: bars leap with your voice and fall like
/// a VU meter rather than a strobe.
const ATTACK: f32 = 0.38;
const RELEASE: f32 = 0.09;

pub struct PillView {
    pub state: PillState,
    pub level: f32,
    pub elapsed: Duration,
    /// The "Still working…" state, entered past `timeout::SLOW_STATE_UI`.
    pub slow: bool,
    bars: [f32; BAR_COUNT],
    started: Instant,
    last_frame: Option<Instant>,
    /// When the current recording began, for the hands-free timer.
    pub recording_since: Option<Instant>,
    /// When the current transcription began, for the "Still working…" state.
    pub processing_since: Option<Instant>,
}

impl Default for PillView {
    fn default() -> Self {
        Self::new()
    }
}

impl PillView {
    pub fn new() -> Self {
        Self {
            state: PillState::Hidden,
            level: 0.0,
            elapsed: Duration::ZERO,
            slow: false,
            bars: [0.0; BAR_COUNT],
            started: Instant::now(),
            last_frame: None,
            recording_since: None,
            processing_since: None,
        }
    }

    /// Recomputes the timer and the slow state from the session clocks.
    /// Returns true when something visible changed, so the caller only
    /// repaints on a real difference.
    pub fn refresh_clock(&mut self) -> bool {
        let elapsed = self
            .recording_since
            .map(|since| Duration::from_secs(since.elapsed().as_secs()))
            .unwrap_or_default();
        let slow = self
            .processing_since
            .is_some_and(|since| since.elapsed() >= jot_core::timeout::SLOW_STATE_UI);
        let changed = elapsed != self.elapsed || slow != self.slow;
        self.elapsed = elapsed;
        self.slow = slow;
        changed
    }

    /// Projects coordinator state onto the pill, given the resting-dot setting.
    pub fn project(
        state: DictationState,
        silence: SilenceReason,
        show_idle_indicator: bool,
    ) -> PillState {
        let resting = if show_idle_indicator {
            PillState::IdleDot
        } else {
            PillState::Hidden
        };
        match state {
            DictationState::Idle => resting,
            // Warming already shows as listening: the key press must be
            // acknowledged before the microphone is open, or a fast talker gets
            // no feedback for the first hundred milliseconds.
            DictationState::Warming => PillState::Listening { locked: false },
            DictationState::Recording { locked } => PillState::Listening { locked },
            DictationState::Finalizing
            | DictationState::Transcribing
            | DictationState::Inserting => PillState::Processing,
            DictationState::Cancelled => resting,
            DictationState::Done(outcome) => match outcome {
                DictationOutcome::Inserted => PillState::Success { words: 0 },
                DictationOutcome::AwaitingChip => {
                    PillState::Notice("Copied — press Ctrl+V to paste".into())
                }
                DictationOutcome::CopiedToClipboard => {
                    PillState::Notice("Copied — press Ctrl+V to paste".into())
                }
                DictationOutcome::HeldForSecureField => {
                    PillState::Notice("Password field — saved to History only".into())
                }
                DictationOutcome::QueuedForRetry => {
                    PillState::Notice("Offline — saved, will send when you reconnect".into())
                }
                DictationOutcome::Silent => match silence {
                    SilenceReason::NoSpeech => PillState::Notice("Didn't catch that".into()),
                    SilenceReason::TooNoisy => {
                        PillState::Notice("Too noisy to hear you — saved to History".into())
                    }
                },
            },
            DictationState::Failed(failure) => PillState::Error(failure_copy(failure)),
        }
    }

    fn frame_interval(&self) -> Option<Duration> {
        match self.state {
            PillState::Listening { .. } => Some(LISTENING_FRAME),
            PillState::Processing => Some(PROCESSING_FRAME),
            _ => None,
        }
    }

    /// Advances the bar smoother and asks for another frame only while
    /// something is actually moving.
    fn tick(&mut self, window: &Window, reduce_motion: bool) {
        let Some(interval) = self.frame_interval() else {
            self.last_frame = None;
            return;
        };
        if reduce_motion {
            // Static level meter: opacity carries the response instead.
            self.last_frame = None;
            return;
        }
        let now = Instant::now();
        let due = self
            .last_frame
            .is_none_or(|last| now.duration_since(last) >= interval);
        if due {
            self.last_frame = Some(now);
            let time = self.started.elapsed().as_secs_f32();
            for index in 0..BAR_COUNT {
                let target = self.bar_target(index, time);
                let coefficient = if target > self.bars[index] {
                    ATTACK
                } else {
                    RELEASE
                };
                self.bars[index] += (target - self.bars[index]) * coefficient;
            }
        }
        window.request_animation_frame();
    }

    fn bar_target(&self, index: usize, time: f32) -> f32 {
        let shimmer = (time * std::f32::consts::TAU * (2.4 + index as f32 * 0.55)
            + BAR_PHASES[index] * 2.0)
            .sin();
        (self.level * BAR_WEIGHTS[index] * (1.0 + shimmer * 0.35)).clamp(0.0, 1.0)
    }

    fn bar_height(&self, index: usize, reduce_motion: bool) -> f32 {
        if reduce_motion {
            return BAR_MIN_HEIGHT + BAR_WEIGHTS[index] * 14.0;
        }
        let time = self.started.elapsed().as_secs_f32();
        if self.state == PillState::Processing {
            // A gentle staggered chase — "thinking".
            let phase = (time * std::f32::consts::TAU / 1.4 + BAR_PHASES[index]).sin();
            return 16.0 + phase * 6.0;
        }
        // Idle breathing keeps the pill alive between phrases.
        let idle = (time * std::f32::consts::TAU * 0.8 + BAR_PHASES[index]).sin() * 2.0;
        (BAR_MIN_HEIGHT + idle + self.bars[index] * (BAR_MAX_HEIGHT - BAR_MIN_HEIGHT))
            .clamp(BAR_MIN_HEIGHT, BAR_MAX_HEIGHT)
    }

    fn bar_color(&self, index: usize, reduce_motion: bool) -> Rgba {
        if self.state != PillState::Processing {
            return theme::G_BLUE;
        }
        let quad = theme::brand_quad();
        if reduce_motion {
            return quad[index % quad.len()];
        }
        // The four-colour sweep travels across the frozen silhouette. This is
        // the only place the brand quad animates.
        let time = self.started.elapsed().as_secs_f32();
        let sweep = (time % 1.2) / 1.2;
        let position = (index as f32 / BAR_COUNT as f32 + sweep).fract();
        quad[(position * quad.len() as f32) as usize % quad.len()]
    }

    fn timer_text(&self) -> SharedString {
        let seconds = self.elapsed.as_secs();
        format!("{}:{:02}", seconds / 60, seconds % 60).into()
    }

    fn waveform(&self, reduce_motion: bool) -> impl IntoElement {
        let mut row = div()
            .flex()
            .items_center()
            .gap(px(BAR_GAP))
            .h(px(BAR_MAX_HEIGHT));
        for index in 0..BAR_COUNT {
            let height = self.bar_height(index, reduce_motion);
            let color = self.bar_color(index, reduce_motion);
            let opacity = if reduce_motion {
                if self.state == PillState::Processing {
                    0.8
                } else {
                    0.4 + self.level * 0.6
                }
            } else {
                1.0
            };
            row = row.child(
                div()
                    .w(px(BAR_WIDTH))
                    .h(px(height))
                    .rounded(px(BAR_WIDTH / 2.0))
                    .bg(with_alpha(color, opacity)),
            );
        }
        row
    }

    /// The pill surface.
    ///
    /// Windows offers no backdrop material a small transient popup can honestly
    /// use — Mica is an app-window material and would tint this window's whole
    /// transparent rect — so this is a solid elevated surface with a hairline
    /// outline, not anything pretending to be glass.
    fn surface(
        &self,
        theme: &Theme,
        width: Option<f32>,
        background: Rgba,
        content: impl IntoElement,
    ) -> impl IntoElement {
        let mut pill = div()
            .flex()
            .items_center()
            .justify_center()
            .gap(theme::spacing::S)
            .px(theme::spacing::M)
            .h(px(PILL_HEIGHT))
            .rounded(theme::radius::full(px(PILL_HEIGHT)))
            .border_1()
            .border_color(with_alpha(theme.outline_variant, 0.6))
            .bg(background)
            .shadow_lg();
        pill = match width {
            Some(width) => pill.w(px(width)),
            None => pill.max_w(px(MAX_CHIP_WIDTH)),
        };
        pill.child(content)
    }
}

impl Render for PillView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let reduce_motion = cx.reduce_motion();
        self.tick(window, reduce_motion);

        let theme = Theme::current(window.appearance(), crate::window_shell::high_contrast());

        let body: gpui::AnyElement = match self.state.clone() {
            PillState::Hidden => div().into_any_element(),

            PillState::IdleDot => div()
                .w(px(56.0))
                .h(px(5.0))
                .rounded(px(2.5))
                .bg(with_alpha(theme.on_surface, 0.28))
                .into_any_element(),

            PillState::Listening { locked } => {
                let width = if locked {
                    LOCKED_WIDTH
                } else {
                    LISTENING_WIDTH
                };
                let show_timer = locked || self.elapsed.as_secs() >= 10;
                self.surface(
                    &theme,
                    Some(width),
                    theme.surface,
                    div()
                        .flex()
                        .items_center()
                        .gap(theme::spacing::S)
                        .when(locked, |row| {
                            row.child(
                                div()
                                    .text_size(theme::type_scale::LABEL)
                                    .text_color(theme.on_surface_variant)
                                    .child("Hands-free"),
                            )
                        })
                        .when(show_timer, |row| {
                            row.child(
                                div()
                                    .font_family(theme::mono_font())
                                    .text_size(theme::type_scale::LABEL)
                                    .text_color(theme.on_surface_variant)
                                    .child(self.timer_text()),
                            )
                        })
                        .child(self.waveform(reduce_motion)),
                )
                .into_any_element()
            }

            PillState::Processing => self
                .surface(
                    &theme,
                    Some(if self.slow {
                        PROCESSING_SLOW_WIDTH
                    } else {
                        PROCESSING_WIDTH
                    }),
                    theme.surface,
                    div()
                        .flex()
                        .items_center()
                        .gap(theme::spacing::S)
                        .child(self.waveform(reduce_motion))
                        .when(self.slow, |row| {
                            row.child(
                                div()
                                    .text_size(theme::type_scale::LABEL)
                                    .text_color(theme.on_surface_variant)
                                    .child("Still working…"),
                            )
                        }),
                )
                .into_any_element(),

            PillState::Success { words } => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(theme::spacing::XXS)
                .child(
                    div()
                        .w(px(PILL_HEIGHT))
                        .h(px(PILL_HEIGHT))
                        .rounded(px(PILL_HEIGHT / 2.0))
                        .border_1()
                        .border_color(with_alpha(theme.outline_variant, 0.6))
                        .bg(theme.surface)
                        .shadow_lg()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_size(px(20.0))
                                .text_color(theme.success)
                                .child("✓"),
                        ),
                )
                // Word counts are noise on a short dictation and reassurance on
                // a long one.
                .when(words > 20, |column| {
                    column.child(
                        div()
                            .text_size(theme::type_scale::LABEL_SMALL)
                            .text_color(theme.on_surface_variant)
                            .child(SharedString::from(format!("{words} words"))),
                    )
                })
                .into_any_element(),

            PillState::Notice(message) => self
                .surface(
                    &theme,
                    None,
                    theme.surface,
                    div()
                        .text_size(theme::type_scale::LABEL)
                        .text_color(theme.on_surface)
                        .child(message),
                )
                .into_any_element(),

            PillState::Error(message) => self
                .surface(
                    &theme,
                    None,
                    theme.error_container,
                    div()
                        .flex()
                        .items_center()
                        .gap(theme::spacing::XS)
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(theme.on_error_container)
                                .child("!"),
                        )
                        .child(
                            div()
                                .text_size(theme::type_scale::LABEL)
                                .text_color(theme.on_error_container)
                                .child(message),
                        ),
                )
                .into_any_element(),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .justify_end()
            .items_center()
            .pb(theme::spacing::XS)
            .font_family(theme::ui_font())
            .child(body)
    }
}

/// Copy for each failure, from the failure matrix. Every one of these names
/// what happened and what the user can do, and none of them loses the words.
pub fn failure_copy(failure: jot_core::state_machine::DictationFailure) -> SharedString {
    use jot_core::state_machine::DictationFailure as F;
    match failure {
        F::Audio => "Microphone didn't start — check Windows sound settings",
        F::NoMicrophone => "No microphone found",
        F::NoAudio => "Nothing was recorded",
        F::Network => "Couldn't reach Gemini — saved to History, retry there",
        F::Auth => "No usable API key — add one in Settings",
        F::ModelAccess => "Your key can't use this model — see Settings → Advanced",
        F::BadRequest => "Gemini refused the request — saved to History",
        F::RateLimited => "Rate limited — saved to History, retry in a moment",
        F::QuotaExhausted => "Daily quota reached — saved to History",
        F::Timeout => "Timed out — saved to History, retry there",
        F::Validation => "Couldn't make out the words — saved to History",
        F::SafetyBlocked => "Gemini declined to transcribe this — saved to History",
        F::Storage => "Couldn't write to disk — check free space",
    }
    .into()
}

fn with_alpha(color: Rgba, alpha: f32) -> Rgba {
    rgba(
        ((color.r * 255.0) as u32) << 24
            | ((color.g * 255.0) as u32) << 16
            | ((color.b * 255.0) as u32) << 8
            | (alpha.clamp(0.0, 1.0) * 255.0) as u32,
    )
}

/// Window size for the HUD host. Wide enough for the longest chip, tall enough
/// for the success badge plus its caption.
pub const HUD_SIZE: (f32, f32) = (MAX_CHIP_WIDTH + 40.0, 96.0);
/// Gap between the pill and the bottom of the work area.
pub const HUD_BOTTOM_MARGIN: f32 = 16.0;

#[cfg(test)]
mod tests {
    use super::*;
    use jot_core::state_machine::DictationFailure;

    #[test]
    fn warming_already_reads_as_listening() {
        // The key press must be acknowledged before the microphone is open.
        assert_eq!(
            PillView::project(DictationState::Warming, SilenceReason::NoSpeech, true),
            PillState::Listening { locked: false }
        );
    }

    #[test]
    fn the_resting_state_follows_the_idle_indicator_setting() {
        assert_eq!(
            PillView::project(DictationState::Idle, SilenceReason::NoSpeech, true),
            PillState::IdleDot
        );
        assert_eq!(
            PillView::project(DictationState::Idle, SilenceReason::NoSpeech, false),
            PillState::Hidden
        );
        // A cancel returns to rest rather than shouting about it.
        assert_eq!(
            PillView::project(DictationState::Cancelled, SilenceReason::NoSpeech, false),
            PillState::Hidden
        );
    }

    #[test]
    fn silence_copy_distinguishes_a_quiet_room_from_a_loud_one() {
        let quiet = PillView::project(
            DictationState::Done(DictationOutcome::Silent),
            SilenceReason::NoSpeech,
            true,
        );
        let noisy = PillView::project(
            DictationState::Done(DictationOutcome::Silent),
            SilenceReason::TooNoisy,
            true,
        );
        assert_ne!(quiet, noisy);
        assert!(matches!(quiet, PillState::Notice(_)));
    }

    #[test]
    fn a_held_transcript_never_claims_to_be_on_the_clipboard() {
        let held = PillView::project(
            DictationState::Done(DictationOutcome::HeldForSecureField),
            SilenceReason::NoSpeech,
            true,
        );
        let PillState::Notice(message) = held else {
            panic!("expected a notice");
        };
        assert!(!message.to_lowercase().contains("copied"), "{message}");
    }

    #[test]
    fn every_failure_has_copy_that_says_where_the_words_went() {
        for failure in [
            DictationFailure::Network,
            DictationFailure::Auth,
            DictationFailure::ModelAccess,
            DictationFailure::RateLimited,
            DictationFailure::QuotaExhausted,
            DictationFailure::Timeout,
            DictationFailure::Validation,
            DictationFailure::SafetyBlocked,
        ] {
            let copy = failure_copy(failure);
            assert!(!copy.is_empty());
            // Anything recoverable must say so; an error with no next step is
            // just a scolding.
            if !matches!(
                failure,
                DictationFailure::Auth | DictationFailure::ModelAccess
            ) {
                assert!(
                    copy.contains("History"),
                    "{failure:?} does not say where the words are: {copy}"
                );
            }
        }
    }

    #[test]
    fn animation_frames_are_only_requested_while_something_moves() {
        let mut pill = PillView::new();
        assert_eq!(pill.frame_interval(), None);
        pill.state = PillState::Listening { locked: false };
        assert_eq!(pill.frame_interval(), Some(LISTENING_FRAME));
        pill.state = PillState::Processing;
        assert_eq!(pill.frame_interval(), Some(PROCESSING_FRAME));
        pill.state = PillState::Success { words: 3 };
        assert_eq!(pill.frame_interval(), None);
    }

    // Bars must leap with the voice and fall like a VU meter, never a strobe.
    const _ATTACK_BEATS_RELEASE: () = assert!(ATTACK > RELEASE * 3.0);

    #[test]
    fn reduced_motion_bars_are_stable_across_time() {
        let pill = PillView::new();
        let first = pill.bar_height(2, true);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(pill.bar_height(2, true), first);
    }

    #[test]
    fn the_timer_reads_as_minutes_and_seconds() {
        let mut pill = PillView::new();
        pill.elapsed = Duration::from_secs(75);
        assert_eq!(pill.timer_text(), SharedString::from("1:15"));
    }

    #[test]
    fn alpha_blending_preserves_the_source_colour() {
        let faded = with_alpha(theme::G_BLUE, 0.5);
        assert!((faded.r - theme::G_BLUE.r).abs() < 0.01);
        assert!((faded.a - 0.5).abs() < 0.01);
    }
}
