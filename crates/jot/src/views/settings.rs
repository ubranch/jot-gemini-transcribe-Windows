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

//! Settings: the dictation key, what the pill shows, how a transcript is
//! formatted, how long audio is kept, and the escape hatches.

use crate::hotkey_hook;
use crate::services::Services;
use crate::text_field::TextField;
use crate::theme::{self, Theme};
use crate::views::widgets::{self, ButtonKind, StatusTone};
use gpui::{Context, Entity, Role, SharedString, Window, div, prelude::*, relative};
use jot_core::audio;
use jot_core::credentials;
use jot_core::gemini::KeyCheck;
use jot_core::hotkey::HotkeyKey;

/// Audio retention options, in the order they read as a sentence.
const RETENTION_CHOICES: [(i64, &str); 4] = [
    (-1, "Never keep"),
    (7, "7 days"),
    (30, "30 days"),
    (0, "Forever"),
];

pub struct SettingsView {
    services: Services,
    api_key: Entity<TextField>,
    endpoint: Entity<TextField>,
    transcribe_model: Entity<TextField>,
    cleanup_model: Entity<TextField>,
    key_status: Option<(SharedString, StatusTone)>,
    advanced_status: Option<SharedString>,
    /// True once the user has asked to wipe everything and been shown what
    /// that means.
    confirming_delete_all: bool,
    /// Mirrors the registry so the switch shows what the machine actually does,
    /// not what we last asked for.
    autostart_enabled: bool,
    scroll: widgets::PageScroll,
}

impl SettingsView {
    pub fn new(services: Services, cx: &mut Context<Self>) -> Self {
        let settings = services.settings.get();
        let endpoint = cx.new(|cx| {
            TextField::new(
                "settings-endpoint",
                "https://generativelanguage.googleapis.com",
                cx,
            )
        });
        let transcribe_model =
            cx.new(|cx| TextField::new("settings-transcribe-model", "gemini-3.5-transcribe", cx));
        let cleanup_model =
            cx.new(|cx| TextField::new("settings-cleanup-model", "gemini-3.5-flash-lite", cx));

        endpoint.update(cx, |field, _| {
            field.set_content(settings.endpoint_override.clone().unwrap_or_default())
        });
        transcribe_model.update(cx, |field, _| {
            field.set_content(
                settings
                    .transcribe_model_override
                    .clone()
                    .unwrap_or_default(),
            )
        });
        cleanup_model.update(cx, |field, _| {
            field.set_content(settings.cleanup_model_override.clone().unwrap_or_default())
        });

        Self {
            services,
            // The stored key is never read back into the UI: it lives in the
            // credential manager and this field only ever writes a new one.
            api_key: cx.new(|cx| {
                TextField::new("settings-api-key", "Paste a new Gemini API key", cx).masked()
            }),
            endpoint,
            transcribe_model,
            cleanup_model,
            key_status: None,
            advanced_status: None,
            confirming_delete_all: false,
            autostart_enabled: crate::autostart::is_enabled(),
            scroll: widgets::PageScroll::new(),
        }
    }

    fn delete_all(&mut self, cx: &mut Context<Self>) {
        // The live session's folder is spared: deleting everything must not
        // destroy the dictation in flight.
        let active = self.services.coordinator.active_session_folder();
        self.services.history.delete_all(true, active.as_deref());
        // A user wiping their words expects them gone from the paste-last
        // buffer too.
        self.services.coordinator.clear_last_result();
        self.confirming_delete_all = false;
        cx.notify();
    }

    fn save_api_key(&mut self, cx: &mut Context<Self>) {
        let key = self.api_key.update(cx, |field, cx| {
            cx.notify();
            field.take_content()
        });
        let key = key.trim().to_string();
        if key.is_empty() {
            self.key_status = Some((
                "Nothing to save — paste a key first".into(),
                StatusTone::Bad,
            ));
            cx.notify();
            return;
        }
        if let Err(error) = credentials::set_api_key(&key) {
            // The message never contains the key itself.
            tracing::error!(%error, "storing the API key failed");
            self.key_status = Some((
                "Windows Credential Manager refused the key".into(),
                StatusTone::Bad,
            ));
            cx.notify();
            return;
        }
        self.key_status = Some(("Saved — checking it works…".into(), StatusTone::Neutral));
        cx.notify();

        let client = self.services.client.clone();
        let endpoint = self.services.settings.get().gemini_config().endpoint;
        cx.spawn(async move |this, cx| {
            let check = client.validate_key(&endpoint).await;
            let status = match check {
                KeyCheck::Valid => ("Key works".into(), StatusTone::Good),
                // The server answered and the answer was no.
                KeyCheck::Rejected(detail) => (
                    SharedString::from(detail.unwrap_or_else(|| "Google rejected this key".into())),
                    StatusTone::Bad,
                ),
                // Never blame the key for a network we could not reach.
                KeyCheck::Unreachable => (
                    "Saved, but Google was unreachable — it will be used anyway".into(),
                    StatusTone::Neutral,
                ),
            };
            let _ = this.update(cx, |this, cx| {
                this.key_status = Some(status);
                cx.notify();
            });
        })
        .detach();
    }

    fn clear_api_key(&mut self, cx: &mut Context<Self>) {
        match credentials::clear_api_key() {
            Ok(()) => {
                self.key_status = Some(("Key removed from this PC".into(), StatusTone::Neutral))
            }
            Err(error) => {
                tracing::error!(%error, "clearing the API key failed");
                self.key_status = Some(("Could not remove the key".into(), StatusTone::Bad));
            }
        }
        cx.notify();
    }

    fn save_advanced(&mut self, cx: &mut Context<Self>) {
        let endpoint = self.endpoint.read(cx).content().trim().to_string();
        let transcribe = self.transcribe_model.read(cx).content().trim().to_string();
        let cleanup = self.cleanup_model.read(cx).content().trim().to_string();

        // The same predicate the effective config uses, so this message can
        // never disagree with which endpoint is actually in play.
        let endpoint_usable = jot_core::settings::Settings::usable_endpoint(Some(&endpoint));
        if !endpoint.is_empty() && endpoint_usable.is_none() {
            self.advanced_status =
                Some("That endpoint is not an http(s) URL — nothing was saved".into());
            cx.notify();
            return;
        }

        self.services.settings.update(Some("advanced"), |settings| {
            settings.endpoint_override = (!endpoint.is_empty()).then_some(endpoint);
            settings.transcribe_model_override = (!transcribe.is_empty()).then_some(transcribe);
            settings.cleanup_model_override = (!cleanup.is_empty()).then_some(cleanup);
        });
        self.advanced_status = Some("Saved — applies to your next dictation".into());
        cx.notify();
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(window.appearance(), crate::window_shell::high_contrast());
        let settings = self.services.settings.get();

        // ----- General
        let mut hotkey_row = div().flex().flex_wrap().gap(theme::spacing::XS);
        for key in HotkeyKey::ALL {
            let selected = settings.hotkey_key == key;
            let store = self.services.settings.clone();
            hotkey_row = hotkey_row.child(widgets::button(
                ("hotkey", key.vk_code()),
                key.display_name(),
                if selected {
                    ButtonKind::Primary
                } else {
                    ButtonKind::Secondary
                },
                true,
                theme,
                move |_, _, _| {
                    store.update(Some("hotkeyKey"), |settings| settings.hotkey_key = key);
                    // The hook rebinds from the settings subscription too; doing
                    // it here as well makes the click feel immediate even if the
                    // subscriber is busy.
                    hotkey_hook::set_hotkey(key);
                },
            ));
        }

        let mut microphone_row = div().flex().flex_wrap().gap(theme::spacing::XS);
        let devices = audio::input_device_names();
        let chosen = settings.preferred_input_device.clone();
        for (index, choice) in std::iter::once(None)
            .chain(devices.into_iter().map(Some))
            .enumerate()
        {
            let selected = chosen == choice;
            let store = self.services.settings.clone();
            let label = choice
                .clone()
                .unwrap_or_else(|| "System default".to_string());
            let value = choice.clone();
            microphone_row = microphone_row.child(widgets::button(
                SharedString::from(format!("microphone-{index}")),
                SharedString::from(label),
                if selected {
                    ButtonKind::Primary
                } else {
                    ButtonKind::Secondary
                },
                true,
                theme,
                move |_, _, _| {
                    let value = value.clone();
                    store.update(Some("preferredInputDevice"), |settings| {
                        settings.preferred_input_device = value;
                    });
                },
            ));
        }

        let general = widgets::section(
            "General",
            theme,
            vec![
                widgets::field_row(
                    "Dictation key",
                    "Windows has no fn key, so pick a bare key you never press alone",
                    theme,
                    hotkey_row,
                )
                .into_any_element(),
                widgets::field_row(
                    "Microphone",
                    "A device you pick is never silently swapped for another",
                    theme,
                    microphone_row,
                )
                .into_any_element(),
                widgets::toggle(
                    "idle-indicator",
                    "Show the resting indicator",
                    "A thin bar at the bottom of the screen when Jot is idle",
                    settings.show_idle_indicator,
                    theme,
                    {
                        let store = self.services.settings.clone();
                        move |_, _, _| {
                            let next = !store.get().show_idle_indicator;
                            store.update(Some("showIdleIndicator"), |s| {
                                s.show_idle_indicator = next
                            });
                        }
                    },
                )
                .into_any_element(),
                widgets::toggle(
                    "autostart",
                    "Start with Windows",
                    "A dictation key you have to launch first gets used once",
                    self.autostart_enabled,
                    theme,
                    cx.listener(|this, _, _, cx| {
                        let wanted = !this.autostart_enabled;
                        if crate::autostart::set_enabled(wanted) {
                            this.autostart_enabled = wanted;
                        } else {
                            // The registry refused; do not show a toggle that
                            // claims a state the machine does not have.
                            this.autostart_enabled = crate::autostart::is_enabled();
                        }
                        cx.notify();
                    }),
                )
                .into_any_element(),
                widgets::toggle(
                    "sounds",
                    "Sounds",
                    "Short earcons when a dictation starts, lands, or fails",
                    settings.sounds_enabled,
                    theme,
                    {
                        let store = self.services.settings.clone();
                        move |_, _, _| {
                            let next = !store.get().sounds_enabled;
                            store.update(Some("soundsEnabled"), |s| s.sounds_enabled = next);
                        }
                    },
                )
                .into_any_element(),
                widgets::toggle(
                    "double-tap",
                    "Double-tap to lock hands-free",
                    "Off by default: firm taps often read as a hold. Holding and tapping Space always works",
                    settings.double_tap_lock,
                    theme,
                    {
                        let store = self.services.settings.clone();
                        move |_, _, _| {
                            let next = !store.get().double_tap_lock;
                            store.update(Some("doubleTapLock"), |s| s.double_tap_lock = next);
                        }
                    },
                )
                .into_any_element(),
            ],
        );

        // ----- Dictation
        let dictation = widgets::section(
            "Dictation",
            theme,
            vec![
                widgets::toggle(
                    "smart-transcription",
                    "Smart transcription",
                    "The model removes fillers and follows a change of mind. Off gives you exactly what you said",
                    settings.smart_transcription,
                    theme,
                    {
                        let store = self.services.settings.clone();
                        move |_, _, _| {
                            let next = !store.get().smart_transcription;
                            store.update(Some("smartTranscription"), |s| {
                                s.smart_transcription = next
                            });
                        }
                    },
                )
                .into_any_element(),
                widgets::toggle(
                    "tone-pass",
                    "Match the tone of the app you're in",
                    "A second, optional model pass. Costs a round trip and sends the transcript text again",
                    settings.smart_cleanup_pass,
                    theme,
                    {
                        let store = self.services.settings.clone();
                        move |_, _, _| {
                            let next = !store.get().smart_cleanup_pass;
                            store.set_smart_cleanup_pass(next);
                        }
                    },
                )
                .into_any_element(),
                widgets::toggle(
                    "noise-handling",
                    "Judge speech against the room (experimental)",
                    "Helps in a loud room. Jot measures the room either way; this decides whether it acts on it",
                    settings.experimental_noise_handling,
                    theme,
                    {
                        let store = self.services.settings.clone();
                        move |_, _, _| {
                            let next = !store.get().experimental_noise_handling;
                            store.update(Some("experimentalNoiseHandling"), |s| {
                                s.experimental_noise_handling = next
                            });
                        }
                    },
                )
                .into_any_element(),
            ],
        );

        // ----- Privacy
        let mut retention_row = div().flex().flex_wrap().gap(theme::spacing::XS);
        for (days, label) in RETENTION_CHOICES {
            let selected = settings.audio_retention_days == days;
            let store = self.services.settings.clone();
            retention_row = retention_row.child(widgets::button(
                ("retention", days as u64),
                label,
                if selected {
                    ButtonKind::Primary
                } else {
                    ButtonKind::Secondary
                },
                true,
                theme,
                move |_, _, _| {
                    store.update(Some("audioRetentionDays"), |settings| {
                        settings.audio_retention_days = days
                    });
                },
            ));
        }

        let privacy = widgets::section(
            "Privacy",
            theme,
            vec![
                div()
                    .text_size(theme::type_scale::BODY)
                    .text_color(theme.on_surface_variant)
                    .child(
                        "Your voice goes from this PC straight to the Gemini API with your key. \
                         No middleman server, no account, no analytics.",
                    )
                    .into_any_element(),
                widgets::field_row(
                    "Keep recordings for",
                    "Transcripts are kept until you delete them",
                    theme,
                    retention_row,
                )
                .into_any_element(),
                widgets::field_row(
                    "Gemini API key",
                    "Stored in Windows Credential Manager, never in a settings file",
                    theme,
                    div()
                        .flex()
                        .flex_col()
                        .gap(theme::spacing::XS)
                        .child(self.api_key.clone())
                        .child(
                            div()
                                .flex()
                                .gap(theme::spacing::XS)
                                .child(widgets::button(
                                    "save-key",
                                    "Save key",
                                    ButtonKind::Primary,
                                    true,
                                    theme,
                                    cx.listener(|this, _, _, cx| this.save_api_key(cx)),
                                ))
                                .child(widgets::button(
                                    "clear-key",
                                    "Remove key",
                                    ButtonKind::Destructive,
                                    true,
                                    theme,
                                    cx.listener(|this, _, _, cx| this.clear_api_key(cx)),
                                )),
                        )
                        .when_some(self.key_status.clone(), |column, (message, tone)| {
                            column.child(widgets::status_line(
                                "settings-key-status",
                                message,
                                tone,
                                theme,
                            ))
                        }),
                )
                .into_any_element(),
                // Two steps, because this is the one control in the app that
                // destroys data with no way back.
                if self.confirming_delete_all {
                    div()
                        .flex()
                        .flex_col()
                        .gap(theme::spacing::XS)
                        .child(widgets::status_line(
                            "delete-all-warning",
                            format!(
                                "Delete {} saved dictations and every recording? This cannot be undone.",
                                self.services.history.stats().total_dictations
                            ),
                            StatusTone::Bad,
                            theme,
                        ))
                        .child(
                            div()
                                .flex()
                                .gap(theme::spacing::XS)
                                .child(widgets::button(
                                    "delete-all-confirm",
                                    "Yes, delete everything",
                                    ButtonKind::Destructive,
                                    true,
                                    theme,
                                    cx.listener(|this, _, _, cx| this.delete_all(cx)),
                                ))
                                .child(widgets::button(
                                    "delete-all-cancel",
                                    "Cancel",
                                    ButtonKind::Secondary,
                                    true,
                                    theme,
                                    cx.listener(|this, _, _, cx| {
                                        this.confirming_delete_all = false;
                                        cx.notify();
                                    }),
                                )),
                        )
                        .into_any_element()
                } else {
                    widgets::hug(widgets::button(
                        "delete-all",
                        "Delete all history and recordings",
                        ButtonKind::Destructive,
                        true,
                        theme,
                        cx.listener(|this, _, _, cx| {
                            this.confirming_delete_all = true;
                            cx.notify();
                        }),
                    ))
                    .into_any_element()
                },
            ],
        );

        // ----- Advanced
        let advanced = widgets::section(
            "Advanced",
            theme,
            vec![
                div()
                    .text_size(theme::type_scale::LABEL)
                    .text_color(theme.on_surface_variant)
                    .child("Leave these empty unless a model was renamed or you are pointing Jot at a proxy.")
                    .into_any_element(),
                widgets::field_row("API endpoint", "", theme, self.endpoint.clone()).into_any_element(),
                widgets::field_row("Transcription model", "", theme, self.transcribe_model.clone())
                    .into_any_element(),
                widgets::field_row("Tone-pass model", "", theme, self.cleanup_model.clone())
                    .into_any_element(),
                widgets::toggle(
                    "legacy-endpoint",
                    "Use the legacy transcription endpoint",
                    "An escape hatch if smart transcription regresses. Smart formatting is unavailable there",
                    settings.legacy_transcribe_endpoint,
                    theme,
                    {
                        let store = self.services.settings.clone();
                        move |_, _, _| {
                            let next = !store.get().legacy_transcribe_endpoint;
                            store.update(Some("legacyTranscribeEndpoint"), |s| {
                                s.legacy_transcribe_endpoint = next
                            });
                        }
                    },
                )
                .into_any_element(),
                div()
                    .flex()
                    .gap(theme::spacing::XS)
                    .child(widgets::button(
                        "save-advanced",
                        "Save",
                        ButtonKind::Primary,
                        true,
                        theme,
                        cx.listener(|this, _, _, cx| this.save_advanced(cx)),
                    ))
                    .into_any_element(),
                div()
                    .when_some(self.advanced_status.clone(), |row, message| {
                        row.child(widgets::status_line("settings-advanced-status", message, StatusTone::Neutral, theme))
                    })
                    .into_any_element(),
            ],
        );

        widgets::page(
            "settings",
            theme,
            &self.scroll,
            vec![
                div()
                    .id("heading")
                    .role(Role::Heading)
                    .aria_level(1)
                    .text_size(theme::type_scale::HEADLINE)
                    .font_weight(theme::weight::MEDIUM)
                    .line_height(relative(theme::line_height::TIGHT))
                    .child("Settings")
                    .into_any_element(),
                general.into_any_element(),
                dictation.into_any_element(),
                privacy.into_any_element(),
                advanced.into_any_element(),
            ],
        )
    }
}
