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

//! The dictionary: your jargon, spelled right.
//!
//! Terms ride along with the audio so the model hears "Kubernetes" instead of
//! guessing "cooper netties" — corrected at the source. A term that also carries
//! the misspelling gets a second, deterministic guarantee applied after the
//! model, which is the only part that is not a suggestion.

use crate::services::Services;
use crate::text_field::{Submit, TextField};
use crate::theme::{self, Theme};
use crate::views::widgets::{self, ButtonKind, StatusTone};
use gpui::{
    Context, Entity, Role, SharedString, Window, div, prelude::*, px, relative, uniform_list,
};
use jot_core::dictionary::{DictionaryEntry, TERM_LENGTH};

const ROW_HEIGHT: f32 = 64.0;

pub struct DictionaryView {
    services: Services,
    term: Entity<TextField>,
    misspelling: Entity<TextField>,
    entries: Vec<DictionaryEntry>,
    status: Option<(SharedString, StatusTone)>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl DictionaryView {
    pub fn new(services: Services, cx: &mut Context<Self>) -> Self {
        // Placeholders describe the field rather than showing a specimen term.
        // "Kubernetes" and "cooper netties" read as entries already present,
        // so there was no way to tell an empty field from a filled one.
        let term = cx.new(|cx| TextField::new("dictionary-term", "Term, spelled correctly", cx));
        let misspelling = cx.new(|cx| {
            TextField::new(
                "dictionary-misspelling",
                "How it gets misheard (optional)",
                cx,
            )
        });
        // Enter in either field adds the entry, so the whole flow is keyboard.
        let subscriptions = vec![
            cx.subscribe(&term, |this: &mut Self, _, _: &Submit, cx| this.add(cx)),
            cx.subscribe(&misspelling, |this: &mut Self, _, _: &Submit, cx| {
                this.add(cx)
            }),
        ];

        let entries = services.dictionary.entries();
        Self {
            services,
            term,
            misspelling,
            entries,
            status: None,
            _subscriptions: subscriptions,
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.entries = self.services.dictionary.entries();
        cx.notify();
    }

    fn add(&mut self, cx: &mut Context<Self>) {
        let term = self.term.read(cx).content().trim().to_string();
        let misspelling = self.misspelling.read(cx).content().trim().to_string();

        if !TERM_LENGTH.contains(&term.chars().count()) {
            self.status = Some((
                format!(
                    "A term has to be {} to {} characters",
                    TERM_LENGTH.start(),
                    TERM_LENGTH.end()
                )
                .into(),
                StatusTone::Bad,
            ));
            cx.notify();
            return;
        }

        let added = self.services.dictionary.add(
            &term,
            (!misspelling.is_empty()).then_some(misspelling.as_str()),
        );
        self.status = Some(if added {
            self.term.update(cx, |field, _| {
                field.take_content();
            });
            self.misspelling.update(cx, |field, _| {
                field.take_content();
            });
            (format!("Added {term}").into(), StatusTone::Good)
        } else {
            (
                format!("{term} is already in your dictionary").into(),
                StatusTone::Neutral,
            )
        });
        self.reload(cx);
    }

    fn row(
        &self,
        entry: &DictionaryEntry,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = entry.id;
        let starred = entry.starred;
        let services_star = self.services.clone();
        let services_remove = self.services.clone();

        div()
            .id(SharedString::from(format!("dictionary-row-{}", id)))
            .role(Role::ListItem)
            .aria_label(SharedString::from(entry.term.clone()))
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap(theme::spacing::S)
            .h(px(ROW_HEIGHT))
            .px(theme::spacing::S)
            .border_b_1()
            .border_color(theme.outline_variant)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(theme::type_scale::BODY_LARGE)
                            .text_color(theme.on_surface)
                            .child(entry.term.clone()),
                    )
                    .child(
                        div()
                            .text_size(theme::type_scale::LABEL)
                            .text_color(theme.on_surface_variant)
                            .child(match entry.misspelling.as_deref() {
                                Some(wrong) => {
                                    format!("always corrects \"{wrong}\"")
                                }
                                None => "spelling hint only".to_string(),
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(theme::spacing::XS)
                    .child(widgets::button(
                        SharedString::from(format!("star-{}", id)),
                        if starred { "Starred" } else { "Star" },
                        if starred {
                            ButtonKind::Primary
                        } else {
                            ButtonKind::Secondary
                        },
                        true,
                        theme,
                        cx.listener(move |this, _, _, cx| {
                            services_star.dictionary.toggle_star(id);
                            this.reload(cx);
                        }),
                    ))
                    .child(widgets::button(
                        SharedString::from(format!("remove-{}", id)),
                        "Remove",
                        ButtonKind::Destructive,
                        true,
                        theme,
                        cx.listener(move |this, _, _, cx| {
                            services_remove.dictionary.remove(id);
                            this.reload(cx);
                        }),
                    )),
            )
    }
}

impl Render for DictionaryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(window.appearance(), crate::window_shell::high_contrast());
        let entries = self.entries.clone();
        let count = entries.len();

        let list = if count == 0 {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.on_surface_variant)
                .child("No terms yet. Add the names and product words Jot keeps getting wrong.")
                .into_any_element()
        } else {
            div()
                .id("dictionary-list")
                .role(Role::List)
                .aria_label("Dictionary terms")
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .rounded(theme::radius::MEDIUM)
                .border_1()
                .border_color(theme.outline_variant)
                .bg(theme.surface)
                .child(
                    uniform_list(
                        "dictionary-rows",
                        count,
                        cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                            let theme = Theme::current(
                                window.appearance(),
                                crate::window_shell::high_contrast(),
                            );
                            range
                                .filter_map(|index| {
                                    entries
                                        .get(index)
                                        .map(|entry| this.row(entry, theme, cx).into_any_element())
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
                    .child("Dictionary"),
            )
            .child(
                div()
                    .text_size(theme::type_scale::LABEL)
                    .text_color(theme.on_surface_variant)
                    .child(
                        "Terms are sent with your audio so the model hears them correctly. \
                         Starred terms are sent first. Adding the misspelling as well makes the \
                         correction a guarantee rather than a hint.",
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(theme::spacing::XS)
                    .child(div().flex_1().child(self.term.clone()))
                    .child(div().flex_1().child(self.misspelling.clone()))
                    .child(widgets::button(
                        "add-term",
                        "Add",
                        ButtonKind::Primary,
                        true,
                        theme,
                        cx.listener(|this, _, _, cx| this.add(cx)),
                    )),
            )
            .when_some(self.status.clone(), |column, (message, tone)| {
                column.child(widgets::status_line(
                    "dictionary-status",
                    message,
                    tone,
                    theme,
                ))
            })
            .child(list)
            .child(
                div()
                    .flex()
                    .gap(theme::spacing::XS)
                    .child(widgets::button(
                        "export-csv",
                        "Export CSV…",
                        ButtonKind::Secondary,
                        count > 0,
                        theme,
                        {
                            let services = self.services.clone();
                            move |_, _, _| {
                                let Some(path) = crate::file_dialog::save(
                                    "Export dictionary",
                                    crate::file_dialog::CSV,
                                    "jot-dictionary.csv",
                                ) else {
                                    return;
                                };
                                if let Err(error) =
                                    std::fs::write(&path, services.dictionary.export_csv())
                                {
                                    tracing::error!(%error, "exporting the dictionary failed");
                                }
                            }
                        },
                    ))
                    .child(widgets::button(
                        "import-csv",
                        "Import CSV…",
                        ButtonKind::Secondary,
                        true,
                        theme,
                        cx.listener(|this, _, _, cx| {
                            let Some(path) = crate::file_dialog::open(
                                "Import dictionary",
                                crate::file_dialog::CSV,
                            ) else {
                                return;
                            };
                            match std::fs::read_to_string(&path) {
                                Ok(csv) => {
                                    let imported = this.services.dictionary.import_csv(&csv);
                                    this.status = Some(if imported > 0 {
                                        (
                                            format!("Imported {imported} terms").into(),
                                            StatusTone::Good,
                                        )
                                    } else {
                                        ("Nothing new in that file".into(), StatusTone::Neutral)
                                    });
                                }
                                Err(error) => {
                                    this.status = Some((
                                        format!("Couldn't read that file: {error}").into(),
                                        StatusTone::Bad,
                                    ));
                                }
                            }
                            this.reload(cx);
                        }),
                    )),
            )
    }
}
