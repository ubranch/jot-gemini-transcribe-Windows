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

//! History: the proof that nothing was ever lost.
//!
//! It is a library of words plus the things needing attention — never an event
//! log. Anything with a transcript shows; so do retryable failures, queued
//! dictations, and long cancelled recordings. Everything else was discarded at
//! the source.

use crate::services::Services;
use crate::text_field::{self, Submit, TextField};
use crate::theme::{self, Theme};
use crate::views::widgets::{self, ButtonKind, StatusTone};
use gpui::{
    Context, Entity, Role, SharedString, Window, div, prelude::*, px, relative, uniform_list,
};
use jot_core::history::DictationRecord;
use jot_core::meta::SessionStatus;
use jot_core::recovery::RetryOutcome;
use time::OffsetDateTime;
use time::macros::format_description;

const PAGE_SIZE: usize = 500;
const ROW_HEIGHT: f32 = 116.0;
/// The transcript line gets a fixed box; `flex_1` let the actions squeeze it
/// to zero and a failure row then explained nothing.
const TEXT_HEIGHT: f32 = 44.0;

pub struct HistoryView {
    services: Services,
    search: Entity<TextField>,
    records: Vec<DictationRecord>,
    status: Option<(SharedString, StatusTone)>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl HistoryView {
    pub fn new(services: Services, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextField::new("history-search", "Search your dictations", cx));
        let subscription = cx.subscribe(&search, |this: &mut Self, _, _: &Submit, cx| {
            this.reload(cx);
        });

        // History refreshes as dictations land, retries drain, and rows are
        // deleted — no polling.
        let mut changes = services.history.subscribe();
        cx.spawn(async move |this, cx| {
            while changes.recv().await.is_ok() {
                if this.update(cx, |this, cx| this.reload(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let mut view = Self {
            services,
            search,
            records: Vec::new(),
            status: None,
            _subscriptions: vec![subscription],
        };
        view.reload(cx);
        view
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).content().to_string();
        let query = (!query.trim().is_empty()).then_some(query);
        self.records = self.services.history.records(query.as_deref(), PAGE_SIZE);
        cx.notify();
    }

    fn retry(&mut self, record: DictationRecord, cx: &mut Context<Self>) {
        self.status = Some(("Retrying…".into(), StatusTone::Neutral));
        cx.notify();
        let queue = self.services.retry_queue.clone();
        cx.spawn(async move |this, cx| {
            let outcome = queue.retry_single(&record).await;
            // A silent no-op reads as broken, so every outcome says something.
            let status = match outcome {
                RetryOutcome::Recovered => {
                    ("Recovered — the text is in this row now", StatusTone::Good)
                }
                RetryOutcome::StillOffline => {
                    ("Still offline — it stays queued", StatusTone::Neutral)
                }
                RetryOutcome::Blocked => (
                    "Your key or quota is blocking this — it stays queued",
                    StatusTone::Bad,
                ),
                RetryOutcome::Failed => ("That one can't be recovered", StatusTone::Bad),
                RetryOutcome::AlreadyDone => ("Already done", StatusTone::Neutral),
                RetryOutcome::Busy => ("Another retry is running", StatusTone::Neutral),
            };
            let _ = this.update(cx, |this, cx| {
                this.status = Some((status.0.into(), status.1));
                this.reload(cx);
            });
        })
        .detach();
    }

    fn row(
        &self,
        record: &DictationRecord,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status = record.status().unwrap_or(SessionStatus::Failed);
        let text = record.display_text().to_string();
        let has_text = !text.is_empty();
        let retryable = matches!(
            status,
            SessionStatus::Failed | SessionStatus::QueuedForRetry | SessionStatus::Cancelled
        ) && !has_text;

        let subtitle = format!(
            "{} · {}{}",
            format_time(record.started_at),
            record.target_app_name.as_deref().unwrap_or("Unknown app"),
            record
                .duration_seconds
                .map(|seconds| format!(" · {seconds:.1}s"))
                .unwrap_or_default()
        );

        let id = record.id.clone();
        let services = self.services.clone();
        let record_for_retry = record.clone();

        div()
            .id(SharedString::from(format!(
                "history-row-{}",
                record.id.clone()
            )))
            .role(Role::ListItem)
            .aria_label(if has_text {
                SharedString::from(text.clone())
            } else {
                SharedString::from(status_label(status).to_string())
            })
            // Without this the row shrinks to its content and the separator
            // beneath it stops half way across the list.
            .w_full()
            .flex()
            .flex_col()
            .gap(theme::spacing::XXS)
            .h(px(ROW_HEIGHT))
            .p(theme::spacing::S)
            .border_b_1()
            .border_color(theme.outline_variant)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme::spacing::XS)
                    .child(status_chip(status, theme))
                    .child(
                        div()
                            .text_size(theme::type_scale::LABEL)
                            .text_color(theme.on_surface_variant)
                            .child(subtitle),
                    ),
            )
            .child(
                div()
                    .h(px(TEXT_HEIGHT))
                    .overflow_hidden()
                    .text_size(theme::type_scale::BODY_LARGE)
                    .text_color(if has_text {
                        theme.on_surface
                    } else {
                        theme.on_surface_variant
                    })
                    .child(if has_text {
                        text.clone()
                    } else {
                        // A row that just repeats its own chip tells the user
                        // nothing they can act on.
                        record
                            .error_message
                            .clone()
                            .unwrap_or_else(|| error_summary(record.error_code.as_deref(), status))
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap(theme::spacing::XS)
                    .when(has_text, |row| {
                        let text = text.clone();
                        row.child(widgets::button(
                            SharedString::from(format!("copy-{}", id.clone())),
                            "Copy",
                            ButtonKind::Secondary,
                            true,
                            theme,
                            move |_, _, cx| text_field::set_clipboard_text(text.clone(), cx),
                        ))
                    })
                    .when(retryable, |row| {
                        row.child(widgets::button(
                            SharedString::from(format!("retry-{}", id.clone())),
                            "Retry",
                            ButtonKind::Primary,
                            true,
                            theme,
                            cx.listener({
                                let record = record_for_retry.clone();
                                move |this, _, _, cx| this.retry(record.clone(), cx)
                            }),
                        ))
                    })
                    .child(widgets::button(
                        SharedString::from(format!("delete-{}", id.clone())),
                        "Delete",
                        ButtonKind::Destructive,
                        true,
                        theme,
                        move |_, _, _| services.history.delete(&id, true),
                    )),
            )
    }
}

impl Render for HistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(window.appearance(), crate::window_shell::high_contrast());
        let stats = self.services.history.stats();
        let count = self.records.len();
        let records = self.records.clone();

        let summary = if count == 0 && stats.total_dictations == 0 {
            "Nothing yet — hold the dictation key and say something".to_string()
        } else if stats.total_dictations == 0 {
            // Saved, but nothing has come back from the model yet: saying
            // "nothing yet" above a visible row would be a plain lie.
            format!("{count} saved · nothing transcribed yet")
        } else if stats.average_wpm > 0 {
            format!(
                "{} dictations · {} words · about {} words per minute",
                stats.total_dictations, stats.total_words, stats.average_wpm
            )
        } else {
            format!(
                "{} dictations · {} words",
                stats.total_dictations, stats.total_words
            )
        };

        let list = if count == 0 {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.on_surface_variant)
                .child(if self.search.read(cx).is_empty() {
                    "No dictations yet."
                } else {
                    "Nothing matches that search."
                })
                .into_any_element()
        } else {
            div()
                .id("history-list")
                .role(Role::List)
                .aria_label("Your dictations")
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .rounded(theme::radius::MEDIUM)
                .border_1()
                .border_color(theme.outline_variant)
                .bg(theme.surface)
                .child(
                    // Virtualized: a heavy user's History is thousands of rows,
                    // and rendering all of them would stall the window.
                    uniform_list(
                        "history-rows",
                        count,
                        cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                            let theme = Theme::current(
                                window.appearance(),
                                crate::window_shell::high_contrast(),
                            );
                            range
                                .filter_map(|index| {
                                    records.get(index).map(|record| {
                                        this.row(record, theme, cx).into_any_element()
                                    })
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .size_full(),
                )
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(theme::spacing::S)
            .p(theme::spacing::L)
            .font_family(theme::ui_font())
            .bg(theme.window_background)
            .text_color(theme.on_surface)
            .child(
                div()
                    .id("heading")
                    .role(Role::Heading)
                    .aria_level(1)
                    .text_size(theme::type_scale::HEADLINE)
                    .font_weight(theme::weight::MEDIUM)
                    .line_height(relative(theme::line_height::TIGHT))
                    .child("History"),
            )
            .child(
                div()
                    .text_size(theme::type_scale::LABEL)
                    .text_color(theme.on_surface_variant)
                    .child(summary),
            )
            .child(self.search.clone())
            .when_some(self.status.clone(), |column, (message, tone)| {
                column.child(widgets::status_line("history-status", message, tone, theme))
            })
            .child(list)
    }
}

fn format_time(at: OffsetDateTime) -> String {
    let local = time::UtcOffset::current_local_offset()
        .map(|offset| at.to_offset(offset))
        .unwrap_or(at);
    local
        .format(format_description!(
            "[day] [month repr:short] [hour]:[minute]"
        ))
        .unwrap_or_else(|_| "unknown time".into())
}

/// The short word that goes in the chip. The row's body line carries the
/// explanation, so a chip repeating a whole sentence just crowds the row.
fn status_chip_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Inserted => "Inserted",
        SessionStatus::CopiedToClipboard | SessionStatus::AwaitingChip => "Copied",
        SessionStatus::HeldSecure => "Held back",
        SessionStatus::QueuedForRetry => "Queued",
        SessionStatus::Recovered => "Recovered",
        SessionStatus::Cancelled => "Cancelled",
        SessionStatus::Failed => "Failed",
        SessionStatus::Silent => "No speech",
        SessionStatus::Recording | SessionStatus::Recorded | SessionStatus::Transcribing => {
            "In flight"
        }
    }
}

fn status_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Inserted => "Inserted",
        SessionStatus::CopiedToClipboard => "Copied to clipboard",
        SessionStatus::AwaitingChip => "Copied — was ready to paste",
        SessionStatus::HeldSecure => "Held back from a password field",
        SessionStatus::QueuedForRetry => "Queued — will send when you reconnect",
        SessionStatus::Recovered => "Recovered — it was never put on the clipboard",
        SessionStatus::Cancelled => "Cancelled — the recording is kept",
        SessionStatus::Failed => "Failed",
        SessionStatus::Silent => "No speech",
        SessionStatus::Recording | SessionStatus::Recorded | SessionStatus::Transcribing => {
            "In flight"
        }
    }
}

/// Human copy for a stored error code, so a row explains itself even when the
/// API gave no message.
fn error_summary(code: Option<&str>, status: SessionStatus) -> String {
    match code {
        Some("offline") => "You were offline — it will send when you reconnect",
        Some("network") => "Couldn't reach Gemini",
        Some("timeout") => "Gemini took too long to answer",
        Some("auth") => "No usable API key — add one in Settings",
        Some("model") => "Your key can't use this model — see Settings → Advanced",
        Some("quota") => "Daily quota reached",
        Some("rate_limit") => "Rate limited — try again in a moment",
        Some("bad_request") => "Gemini refused the request",
        Some("safety") => "Gemini declined to transcribe this",
        Some("empty") => "Nothing came back from the model",
        Some("tooNoisy") => "Too noisy to make out any speech",
        Some("disk_write") => "Writing the recording failed — check free space",
        Some("engine_died") => "The microphone stopped mid-recording",
        Some("audio_purged") | Some("no_audio_file") => {
            "The recording is gone, so this can't be transcribed"
        }
        _ => status_label(status),
    }
    .to_string()
}

fn status_chip(status: SessionStatus, theme: Theme) -> impl IntoElement {
    let (background, foreground) = match status {
        SessionStatus::Inserted | SessionStatus::Recovered => {
            (widgets::with_alpha(theme.success, 0.16), theme.success)
        }
        SessionStatus::Failed => (theme.error_container, theme.on_error_container),
        SessionStatus::QueuedForRetry | SessionStatus::Cancelled => (
            widgets::with_alpha(theme.on_surface_variant, 0.14),
            theme.on_surface_variant,
        ),
        _ => (theme.primary_container, theme.on_primary_container),
    };
    div()
        .px(theme::spacing::XS)
        .py(px(2.0))
        .rounded(theme::radius::XS)
        .bg(background)
        .text_size(theme::type_scale::LABEL_SMALL)
        .font_weight(theme::weight::MEDIUM)
        .text_color(foreground)
        .child(status_chip_label(status))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_has_copy_and_none_of_it_lies_about_the_clipboard() {
        for status in [
            SessionStatus::Inserted,
            SessionStatus::CopiedToClipboard,
            SessionStatus::AwaitingChip,
            SessionStatus::HeldSecure,
            SessionStatus::QueuedForRetry,
            SessionStatus::Recovered,
            SessionStatus::Cancelled,
            SessionStatus::Failed,
            SessionStatus::Silent,
        ] {
            let label = status_label(status);
            assert!(!label.is_empty());
        }
        // A recovered dictation was never put on the clipboard, so its row must
        // not imply it is ready to paste.
        assert!(!status_label(SessionStatus::Recovered).contains("paste"));
        assert!(!status_label(SessionStatus::HeldSecure).contains("Copied"));
    }

    #[test]
    fn an_error_row_explains_itself_without_an_api_message() {
        // The chip already says "Failed"; the line under it has to add
        // something the user can act on.
        for code in [
            "offline",
            "network",
            "timeout",
            "auth",
            "model",
            "quota",
            "rate_limit",
            "bad_request",
            "safety",
            "empty",
            "tooNoisy",
            "disk_write",
            "engine_died",
            "audio_purged",
        ] {
            let summary = error_summary(Some(code), SessionStatus::Failed);
            assert_ne!(summary, "Failed", "{code} adds nothing");
            assert!(!summary.is_empty());
        }
        // An unrecognised code still says something rather than nothing.
        assert_eq!(
            error_summary(Some("who-knows"), SessionStatus::Failed),
            "Failed"
        );
        assert_eq!(error_summary(None, SessionStatus::Failed), "Failed");
    }

    #[test]
    fn chip_labels_stay_short_while_the_row_body_explains() {
        for status in [
            SessionStatus::Inserted,
            SessionStatus::CopiedToClipboard,
            SessionStatus::AwaitingChip,
            SessionStatus::HeldSecure,
            SessionStatus::QueuedForRetry,
            SessionStatus::Recovered,
            SessionStatus::Cancelled,
            SessionStatus::Failed,
            SessionStatus::Silent,
        ] {
            let chip = status_chip_label(status);
            assert!(!chip.is_empty());
            // A chip wide enough to hold a sentence pushes the timestamp and
            // the app name off the row.
            assert!(chip.len() <= 12, "{status:?} chip too long: {chip}");
        }
    }

    #[test]
    fn a_cancelled_row_says_the_audio_survived() {
        assert!(status_label(SessionStatus::Cancelled).contains("kept"));
    }

    #[test]
    fn timestamps_render_rather_than_panicking_on_a_missing_local_offset() {
        let formatted = format_time(OffsetDateTime::UNIX_EPOCH);
        assert!(formatted.contains("Jan"), "{formatted}");
    }
}
