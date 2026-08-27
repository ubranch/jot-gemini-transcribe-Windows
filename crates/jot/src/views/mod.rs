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

//! Jot's windows. There is no main window: each of these is a tool the tray
//! opens, and closing the last one leaves the dictation key working.

pub mod dictionary;
pub mod history;
pub mod onboarding;
pub mod settings;
pub mod widgets;

use crate::services::Services;
use crate::theme::{self, Theme};
use crate::views::widgets::StatusTone;
use gpui::{
    AnyWindowHandle, App, Bounds, Render, Role, TitlebarOptions, WindowBounds, WindowOptions, div,
    prelude::*, px, relative, size,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Open tool windows, so a second request focuses the existing one instead of
/// stacking duplicates on top of each other.
static OPEN_WINDOWS: LazyLock<Mutex<HashMap<&'static str, AnyWindowHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Opens a tool window, or brings the existing one forward.
pub fn open_tool_window<V: 'static + Render>(
    key: &'static str,
    title: &'static str,
    width: f32,
    height: f32,
    cx: &mut App,
    build: impl FnOnce(&mut gpui::Window, &mut App) -> gpui::Entity<V>,
) {
    if let Some(existing) = OPEN_WINDOWS.lock().get(key).copied()
        && cx.windows().contains(&existing)
    {
        let _ = existing.update(cx, |_, window, _| window.activate_window());
        return;
    }

    let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            focus: true,
            show: true,
            app_id: Some("com.ammaar.jot".into()),
            // Fixed size: every pane already scrolls, and a resizable one just
            // invites a layout nobody designed.
            is_resizable: false,
            is_minimizable: true,
            ..Default::default()
        },
        |window, cx| build(window, cx),
    );

    match opened {
        Ok(handle) => {
            OPEN_WINDOWS.lock().insert(key, handle.into());
            cx.activate(true);
        }
        Err(error) => tracing::error!(%error, window = key, "could not open window"),
    }
}

pub fn open_settings(services: &Services, cx: &mut App) {
    let services = services.clone();
    open_tool_window(
        "settings",
        "Jot Settings",
        620.0,
        720.0,
        cx,
        move |_, cx| cx.new(|cx| settings::SettingsView::new(services, cx)),
    );
}

pub fn open_history(services: &Services, cx: &mut App) {
    let services = services.clone();
    open_tool_window("history", "Jot History", 760.0, 620.0, cx, move |_, cx| {
        cx.new(|cx| history::HistoryView::new(services, cx))
    });
}

pub fn open_dictionary(services: &Services, cx: &mut App) {
    let services = services.clone();
    open_tool_window(
        "dictionary",
        "Jot Dictionary",
        640.0,
        540.0,
        cx,
        move |_, cx| cx.new(|cx| dictionary::DictionaryView::new(services, cx)),
    );
}

pub fn open_onboarding(services: &Services, cx: &mut App) {
    let services = services.clone();
    open_tool_window(
        "onboarding",
        "Set up Jot",
        620.0,
        440.0,
        cx,
        move |_, cx| cx.new(|cx| onboarding::OnboardingView::new(services, cx)),
    );
}

pub fn open_about(cx: &mut App) {
    open_tool_window("about", "About Jot", 440.0, 340.0, cx, |_, cx| {
        cx.new(AboutView::new)
    });
}

/// About, which is also where Jot asks whether it is out of date.
///
/// The check runs here rather than at startup on purpose: opening this window
/// is a deliberate act, so the one request Jot makes to anything other than the
/// Gemini API stays something the user asked for.
pub struct AboutView {
    update: UpdateState,
    scroll: widgets::PageScroll,
}

enum UpdateState {
    Checking,
    UpToDate,
    Available(jot_core::update::Update),
    /// GitHub was unreachable or unhappy. Worth saying, not worth retrying:
    /// this window is not the reason anyone opened Jot.
    Unknown,
}

impl AboutView {
    fn new(cx: &mut gpui::Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            let outcome =
                jot_core::runtime::spawn(jot_core::update::check(env!("CARGO_PKG_VERSION"))).await;
            let state = match outcome {
                Ok(Ok(Some(update))) => UpdateState::Available(update),
                Ok(Ok(None)) => UpdateState::UpToDate,
                Ok(Err(error)) => {
                    tracing::debug!(?error, "update check failed");
                    UpdateState::Unknown
                }
                Err(error) => {
                    tracing::debug!(?error, "update check did not run");
                    UpdateState::Unknown
                }
            };
            this.update(cx, |this, cx| {
                this.update = state;
                cx.notify();
            })
            .ok();
        })
        .detach();

        Self {
            update: UpdateState::Checking,
            scroll: widgets::PageScroll::new(),
        }
    }

    fn update_element(&self, theme: Theme) -> gpui::AnyElement {
        match &self.update {
            UpdateState::Checking => widgets::status_line(
                "update",
                "Checking for updates…",
                StatusTone::Neutral,
                theme,
            )
            .into_any_element(),
            UpdateState::UpToDate => widgets::status_line(
                "update",
                "You are on the latest version.",
                StatusTone::Good,
                theme,
            )
            .into_any_element(),
            UpdateState::Unknown => widgets::status_line(
                "update",
                "Could not reach GitHub to check for updates.",
                StatusTone::Neutral,
                theme,
            )
            .into_any_element(),
            UpdateState::Available(update) => {
                let page = update.page.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(widgets::status_line(
                        "update",
                        format!("Jot {} is available.", update.version),
                        StatusTone::Good,
                        theme,
                    ))
                    .child(widgets::hug(widgets::button(
                        "open-release",
                        "Open the download page",
                        widgets::ButtonKind::Primary,
                        true,
                        theme,
                        move |_, _, cx| cx.open_url(&page),
                    )))
                    .into_any_element()
            }
        }
    }
}

impl Render for AboutView {
    fn render(
        &mut self,
        window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        let theme = Theme::current(window.appearance(), crate::window_shell::high_contrast());
        let update = self.update_element(theme);
        widgets::page(
            "about",
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
                    .child("Jot")
                    .into_any_element(),
                div()
                    .text_size(theme::type_scale::BODY)
                    .text_color(theme.on_surface_variant)
                    .child(format!("Version {}", env!("CARGO_PKG_VERSION")))
                    .into_any_element(),
                update,
                div()
                    .text_size(theme::type_scale::BODY)
                    .child(
                        "Hold the dictation key, say the thing, let go. \
                         Your voice goes from this PC straight to the Gemini API \
                         with your key — no middleman server, no account, no analytics.",
                    )
                    .into_any_element(),
                div()
                    .text_size(theme::type_scale::LABEL)
                    .text_color(theme.on_surface_variant)
                    .child(
                        "Apache 2.0 licensed. Bundled fonts are SIL OFL 1.1. \
                         This is not an officially supported Google product.",
                    )
                    .into_any_element(),
            ],
        )
    }
}
