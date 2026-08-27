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

//! Setup: about two minutes, and it makes you try the thing once so you believe it.
//!
//! The key check is deliberately three-valued. A key the server actively
//! rejected is a hard block; a check we could not perform — a captive portal, a
//! VPN coming up — must let the user continue, or setup strands people whose
//! key is fine.

use crate::services::Services;
use crate::text_field::{Submit, TextField};
use crate::theme::{self, Theme};
use crate::views::widgets::{self, ButtonKind, StatusTone};
use gpui::{Context, Entity, Role, SharedString, Window, div, prelude::*, relative};
use jot_core::audio;
use jot_core::credentials;
use jot_core::gemini::KeyCheck;
use jot_core::state_machine::{DictationOutcome, DictationState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Key,
    Microphone,
    TryIt,
    Done,
}

impl Step {
    fn index(self) -> usize {
        match self {
            Step::Key => 1,
            Step::Microphone => 2,
            Step::TryIt => 3,
            Step::Done => 4,
        }
    }
}

const TOTAL_STEPS: usize = 4;

pub struct OnboardingView {
    services: Services,
    step: Step,
    api_key: Entity<TextField>,
    status: Option<(SharedString, StatusTone)>,
    checking: bool,
    /// Set once a dictation actually completes while this window is open.
    tried_it: bool,
    _subscriptions: Vec<gpui::Subscription>,
}

impl OnboardingView {
    pub fn new(services: Services, cx: &mut Context<Self>) -> Self {
        let api_key = cx.new(|cx| TextField::new("onboarding-api-key", "AIza…", cx).masked());
        let subscription = cx.subscribe(&api_key, |this: &mut Self, _, _: &Submit, cx| {
            this.save_key(cx)
        });

        // Step 3 advances by itself the moment a dictation lands, so the user
        // is told they succeeded by the thing succeeding.
        let mut updates = services.coordinator.subscribe();
        cx.spawn(async move |this, cx| {
            while let Ok(update) = updates.recv().await {
                let landed = matches!(
                    update,
                    jot_core::coordinator::CoordinatorUpdate::State(DictationState::Done(
                        DictationOutcome::Inserted
                            | DictationOutcome::CopiedToClipboard
                            | DictationOutcome::AwaitingChip
                    ))
                );
                if !landed {
                    continue;
                }
                let updated = this.update(cx, |this, cx| {
                    this.tried_it = true;
                    if this.step == Step::TryIt {
                        this.step = Step::Done;
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();

        // Someone reopening setup with a key already stored starts past step 1.
        let step = if credentials::api_key().is_some() {
            Step::Microphone
        } else {
            Step::Key
        };

        Self {
            services,
            step,
            api_key,
            status: None,
            checking: false,
            tried_it: false,
            _subscriptions: vec![subscription],
        }
    }

    fn save_key(&mut self, cx: &mut Context<Self>) {
        let key = self
            .api_key
            .update(cx, |field, _| field.content().trim().to_string());
        if key.is_empty() {
            self.status = Some(("Paste your key first".into(), StatusTone::Bad));
            cx.notify();
            return;
        }
        if let Err(error) = credentials::set_api_key(&key) {
            tracing::error!(%error, "storing the API key failed");
            self.status = Some((
                "Windows Credential Manager refused the key".into(),
                StatusTone::Bad,
            ));
            cx.notify();
            return;
        }
        self.api_key.update(cx, |field, _| {
            field.take_content();
        });

        self.checking = true;
        self.status = Some(("Checking your key…".into(), StatusTone::Neutral));
        cx.notify();

        let client = self.services.client.clone();
        let config = self.services.settings.get().gemini_config();
        cx.spawn(async move |this, cx| {
            let check = client.validate_key(&config.endpoint).await;
            // "Your key works" should mean the whole pipeline works, not just
            // that the key authenticates — so the model is checked too.
            let model = if check == KeyCheck::Valid {
                client
                    .resolve_available_model(std::slice::from_ref(&config.transcribe_model), &config.endpoint)
                    .await
            } else {
                None
            };

            let _ = this.update(cx, |this, cx| {
                this.checking = false;
                match check {
                    KeyCheck::Valid if model.is_some() => {
                        this.status = Some(("Key works".into(), StatusTone::Good));
                        this.step = Step::Microphone;
                    }
                    KeyCheck::Valid => {
                        // The key is fine; this model is not available to it.
                        // Naming the model sends them to the right fix.
                        this.status = Some((
                            format!(
                                "Your key works, but it can't reach {}. Pick another in Settings → Advanced.",
                                config.transcribe_model
                            )
                            .into(),
                            StatusTone::Bad,
                        ));
                    }
                    KeyCheck::Rejected(detail) => {
                        this.status = Some((
                            SharedString::from(
                                detail.unwrap_or_else(|| "Google rejected this key".into()),
                            ),
                            StatusTone::Bad,
                        ));
                    }
                    KeyCheck::Unreachable => {
                        // Never strand someone behind a captive portal.
                        this.status = Some((
                            "Couldn't reach Google to check it. Saved anyway — carry on.".into(),
                            StatusTone::Neutral,
                        ));
                        this.step = Step::Microphone;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn check_microphone(&mut self, cx: &mut Context<Self>) {
        let devices = audio::input_device_names();
        if devices.is_empty() {
            self.status = Some((
                "Windows reports no microphone. Plug one in and try again.".into(),
                StatusTone::Bad,
            ));
        } else {
            self.status = Some((
                format!("Found {}", devices.join(", ")).into(),
                StatusTone::Good,
            ));
            self.step = Step::TryIt;
        }
        cx.notify();
    }

    fn finish(&mut self, cx: &mut Context<Self>) {
        // The one celebratory sound in the app, for the one moment that earns
        // it — and only if they actually got a dictation through.
        if self.tried_it {
            crate::sound::play(
                crate::sound::Earcon::Celebration,
                self.services.settings.get().sounds_enabled,
            );
        }
        self.services
            .settings
            .update(Some("hasCompletedOnboarding"), |settings| {
                // A deliberate "I'll do it later" must not re-trap them in the
                // wizard on every launch.
                settings.has_completed_onboarding = true;
            });
        cx.notify();
    }

    fn body(&self, theme: Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let hotkey = self.services.settings.get().hotkey_key;
        match self.step {
            Step::Key => div()
                .flex()
                .flex_col()
                .gap(theme::spacing::S)
                .child(paragraph(
                    "Jot uses your own Gemini API key. Get one from Google AI Studio, \
                     then paste it here. It is stored in Windows Credential Manager and \
                     only ever sent to Google.",
                    theme,
                ))
                .child(paragraph("aistudio.google.com/apikey", theme))
                .child(self.api_key.clone())
                .child(widgets::hug(widgets::button(
                    "save-key",
                    if self.checking {
                        "Checking…"
                    } else {
                        "Save and check"
                    },
                    ButtonKind::Primary,
                    !self.checking,
                    theme,
                    cx.listener(|this, _, _, cx| this.save_key(cx)),
                )))
                .into_any_element(),

            Step::Microphone => div()
                .flex()
                .flex_col()
                .gap(theme::spacing::S)
                .child(paragraph(
                    "Jot records from your default Windows input device. \
                     Windows will ask for microphone permission the first time you dictate.",
                    theme,
                ))
                .child(widgets::hug(widgets::button(
                    "check-mic",
                    "Check my microphone",
                    ButtonKind::Primary,
                    true,
                    theme,
                    cx.listener(|this, _, _, cx| this.check_microphone(cx)),
                )))
                .into_any_element(),

            Step::TryIt => div()
                .flex()
                .flex_col()
                .gap(theme::spacing::S)
                .child(paragraph(
                    &format!(
                        "Click into any text box, hold {}, and say:\n\
                         \"let's meet at 1pm — actually, no, make it 2pm\"",
                        hotkey.display_name()
                    ),
                    theme,
                ))
                .child(paragraph(
                    "Jot should type \"Let's meet at 2 PM.\" — it follows a change of mind. \
                     This step finishes by itself as soon as a dictation lands.",
                    theme,
                ))
                .child(widgets::hug(widgets::button(
                    "skip-try",
                    "Skip this",
                    ButtonKind::Secondary,
                    true,
                    theme,
                    cx.listener(|this, _, _, cx| {
                        this.step = Step::Done;
                        cx.notify();
                    }),
                )))
                .into_any_element(),

            Step::Done => div()
                .flex()
                .flex_col()
                .gap(theme::spacing::S)
                .child(paragraph(
                    if self.tried_it {
                        "That's it. Hold the key anywhere you can type."
                    } else {
                        "That's it. Hold the key anywhere you can type — come back here from the \
                         notification area if you want to walk through it again."
                    },
                    theme,
                ))
                .child(paragraph(
                    &format!(
                        "Hold {} to talk · tap Space while holding for hands-free · Esc cancels",
                        hotkey.display_name()
                    ),
                    theme,
                ))
                .child(widgets::hug(widgets::button(
                    "finish",
                    "Done",
                    ButtonKind::Primary,
                    true,
                    theme,
                    cx.listener(|this, _, window, cx| {
                        this.finish(cx);
                        window.remove_window();
                    }),
                )))
                .into_any_element(),
        }
    }
}

impl Render for OnboardingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(window.appearance(), crate::window_shell::high_contrast());
        let step = self.step;
        let body = self.body(theme, cx);

        widgets::page(
            "onboarding",
            theme,
            vec![
                div()
                    .id("heading")
                    .role(Role::Heading)
                    .aria_level(1)
                    .text_size(theme::type_scale::HEADLINE)
                    .font_weight(theme::weight::MEDIUM)
                    .line_height(relative(theme::line_height::TIGHT))
                    .child("Set up Jot")
                    .into_any_element(),
                div()
                    .id("step-status")
                    .role(Role::Status)
                    .text_size(theme::type_scale::LABEL)
                    .text_color(theme.on_surface_variant)
                    .child(format!("Step {} of {TOTAL_STEPS}", step.index()))
                    .into_any_element(),
                body,
                div()
                    .when_some(self.status.clone(), |row, (message, tone)| {
                        row.child(widgets::status_line(
                            "onboarding-status",
                            message,
                            tone,
                            theme,
                        ))
                    })
                    .into_any_element(),
                widgets::hug(widgets::button(
                    "later",
                    "I'll finish this later",
                    ButtonKind::Secondary,
                    true,
                    theme,
                    cx.listener(|this, _, window, cx| {
                        this.finish(cx);
                        window.remove_window();
                    }),
                ))
                .into_any_element(),
            ],
        )
    }
}

fn paragraph(text: &str, theme: Theme) -> impl IntoElement {
    div()
        .text_size(theme::type_scale::BODY)
        .text_color(theme.on_surface)
        .child(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_is_within_the_advertised_count() {
        for step in [Step::Key, Step::Microphone, Step::TryIt, Step::Done] {
            assert!(step.index() >= 1 && step.index() <= TOTAL_STEPS);
        }
        assert_eq!(Step::Done.index(), TOTAL_STEPS);
    }
}
