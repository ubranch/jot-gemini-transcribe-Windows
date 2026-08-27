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

//! Jot for Windows: hold a key, speak, and the text lands where your cursor is.
//!
//! Three loops meet here, each owning a thread Windows insists on:
//! the keyboard hook, the notification-area icon, and GPUI's application loop.
//! None of them blocks another; they meet through channels.

// A dictation tool that flashed a console window on every launch would be
// unusable, so the release build is a GUI subsystem binary.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod file_dialog;
mod hotkey_hook;
mod pill;
mod services;
mod sound;
mod text_field;
mod theme;
mod tray;
mod views;
mod window_shell;

use anyhow::Result;
use gpui::{
    App, AppContext as _, Bounds, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind, WindowOptions, px, size,
};
use gpui_platform::application;
use jot_core::coordinator::{CoordinatorUpdate, DictationCoordinator};
use jot_core::hotkey::{HotkeyIntent, HotkeyProcessor};
use jot_core::settings::SettingsStore;
use jot_core::state_machine::{DictationOutcome, DictationState, SilenceReason};
use pill::{PillState, PillView};
use services::Services;
use sound::Earcon;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;
use tray::TrayCommand;

/// How often the pill re-reads its clocks while a session is live.
const CLOCK_TICK: Duration = Duration::from_millis(200);

fn main() -> Result<()> {
    // Held for the process lifetime: dropping it stops the log file flushing.
    let _log_guard = init_tracing();

    // One runtime for the whole engine, installed before anything can schedule
    // on it. Work reaches the coordinator from threads that know nothing about
    // tokio, so a bare `tokio::spawn` would panic in half the code paths.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("jot-worker")
        .build()?;
    jot_core::runtime::install(runtime.handle().clone());
    let _runtime_guard = runtime.enter();

    let services = Services::new()?;
    let settings = services.settings.get();

    // A stale Run entry points at a build that has moved; rewrite it so
    // "start with Windows" keeps meaning this executable.
    if autostart::is_stale() {
        tracing::info!("rewriting a stale start-with-Windows entry");
        autostart::set_enabled(true);
    }

    sound::start();
    let hotkey_events = hotkey_hook::install(settings.hotkey_key);
    let tray_commands = tray::install();

    runtime.spawn(run_hotkey_grammar(
        hotkey_events,
        services.coordinator.clone(),
        services.settings.clone(),
    ));
    runtime.spawn(follow_hotkey_setting(services.settings.clone()));
    services.start_background_work();

    let app_services = services.clone();
    application().run(move |cx: &mut App| {
        theme::load_fonts(cx);
        // Jot has no dock presence and no main window: closing the last window
        // must leave the tray icon and the dictation key working.
        cx.set_quit_mode(gpui::QuitMode::Explicit);

        let hud = match open_hud(cx, app_services.settings.get().show_idle_indicator) {
            Ok(hud) => hud,
            Err(error) => {
                tracing::error!(%error, "could not open the HUD — dictation would be invisible");
                cx.quit();
                return;
            }
        };

        drive_pill(hud, app_services.clone(), cx);
        run_clock(hud, cx);
        handle_tray(tray_commands, hud, app_services.clone(), cx);

        if !app_services.settings.get().has_completed_onboarding {
            views::open_onboarding(&app_services, cx);
        }
    });

    Ok(())
}

/// Starts logging. The returned guard flushes the file writer and must live for
/// the whole process.
///
/// Release builds are GUI-subsystem binaries with no stdout at all, so a
/// console-only subscriber means that when something goes wrong for a real user
/// there is nothing to look at. Lines go to a daily file under the app's data
/// directory, and to stderr as well while developing.
fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    // Transcript text is never logged; these lines are lifecycle only.
    let filter = EnvFilter::try_from_env("JOT_LOG")
        .unwrap_or_else(|_| EnvFilter::new("jot=info,jot_core=info"));

    let directory = jot_core::file_layout::app_support_root().join("logs");
    let (writer, guard) = match std::fs::create_dir_all(&directory) {
        Ok(()) => {
            let (writer, guard) = tracing_appender::non_blocking(tracing_appender::rolling::daily(
                &directory, "jot.log",
            ));
            (Some(writer), Some(guard))
        }
        Err(_) => (None, None),
    };

    let registry = tracing_subscriber::registry().with(filter);
    let console = fmt::layer()
        .with_target(true)
        .with_writer(std::io::stderr)
        .with_ansi(cfg!(debug_assertions));
    match writer {
        Some(writer) => {
            let file = fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(writer);
            let _ = registry.with(console).with(file).try_init();
        }
        None => {
            let _ = registry.with(console).try_init();
        }
    }
    guard
}

/// The HUD host: a transparent, borderless, always-on-top window that spans the
/// pill's widest state. It is opened once and never closed — hiding it would
/// cost a window creation on the latency path of every key press.
fn open_hud(cx: &mut App, show_idle_indicator: bool) -> Result<WindowHandle<PillView>> {
    let (width, height) = pill::HUD_SIZE;
    // The real position is set from the work area immediately after creation;
    // this origin only has to be somewhere valid until then.
    let origin = cx
        .primary_display()
        .map(|display| display.bounds().origin)
        .unwrap_or_default();

    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin,
                size: size(px(width), px(height)),
            })),
            titlebar: None,
            focus: false,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            app_id: Some("com.ammaar.jot".into()),
            ..Default::default()
        },
        |window, cx| {
            // Refuses focus, passes clicks through, and sits in the work area
            // rather than under the taskbar — see `window_shell`.
            window_shell::make_overlay(window, pill::HUD_BOTTOM_MARGIN);
            cx.new(|_| {
                let mut pill = PillView::new();
                // Start at rest, not blank: the resting indicator is the only
                // thing telling the user Jot is running at all.
                pill.state = if show_idle_indicator {
                    PillState::IdleDot
                } else {
                    PillState::Hidden
                };
                pill
            })
        },
    )?;
    Ok(handle)
}

/// Bridges coordinator updates onto the pill, and plays the earcons.
fn drive_pill(hud: WindowHandle<PillView>, services: Services, cx: &mut App) {
    let mut updates = services.coordinator.subscribe();
    let mut settings_changes = services.settings.subscribe();
    cx.spawn(async move |cx| {
        let mut silence = SilenceReason::NoSpeech;
        let mut last_words = 0usize;
        loop {
            let update = tokio::select! {
                update = updates.recv() => match update {
                    Ok(update) => update,
                    Err(_) => continue,
                },
                // A toggled resting indicator must take effect immediately, not
                // after the next unrelated transition.
                change = settings_changes.recv() => {
                    if matches!(change, Ok("showIdleIndicator")) {
                        let show = services.settings.get().show_idle_indicator;
                        let state = services.coordinator.state();
                        let projected = PillView::project(state, silence, show);
                        let _ = hud.update(cx, |view, _, cx| {
                            view.state = projected;
                            cx.notify();
                        });
                    }
                    continue;
                }
            };

            let sounds = services.settings.get().sounds_enabled;
            match update {
                CoordinatorUpdate::MicLevel(level) => {
                    let _ = hud.update(cx, |view, _, cx| {
                        view.level = level;
                        cx.notify();
                    });
                }
                CoordinatorUpdate::SilenceReason(reason) => silence = reason,
                CoordinatorUpdate::Result(text) => {
                    last_words = text.split_whitespace().count();
                }
                CoordinatorUpdate::CoachingHint(hint) => {
                    let Some(hint) = hint else { continue };
                    let _ = hud.update(cx, |view, _, cx| {
                        // A hint never overwrites a live recording's waveform.
                        if !matches!(view.state, PillState::Listening { .. }) {
                            view.state = PillState::Notice(hint.into());
                            cx.notify();
                        }
                    });
                }
                CoordinatorUpdate::State(state) => {
                    play_transition(state, sounds);
                    hotkey_hook::set_session_active(
                        !state.is_terminal() && state != DictationState::Idle,
                    );
                    tray::set_dictating(state.is_recording());

                    let show_idle = services.settings.get().show_idle_indicator;
                    let mut projected = PillView::project(state, silence, show_idle);
                    if let PillState::Success { words } = &mut projected {
                        *words = last_words;
                    }
                    let terminal = state.is_terminal();
                    let _ = hud.update(cx, |view, window, cx| {
                        if state == DictationState::Warming {
                            // Follow the display the user is actually dictating
                            // on — it does not move again mid-session.
                            window_shell::place_on_active_display(window, pill::HUD_BOTTOM_MARGIN);
                            view.recording_since = Some(Instant::now());
                        }
                        if matches!(state, DictationState::Finalizing) {
                            view.processing_since = Some(Instant::now());
                        }
                        if terminal {
                            view.recording_since = None;
                            view.processing_since = None;
                            view.level = 0.0;
                            view.slow = false;
                        }
                        view.state = projected;
                        cx.notify();
                    });

                    if terminal {
                        // The outcome is shown, then the pill goes back to rest.
                        let hud = hud;
                        let settings = services.settings.clone();
                        let coordinator = services.coordinator.clone();
                        cx.spawn(async move |cx| {
                            cx.background_executor()
                                .timer(Duration::from_millis(1600))
                                .await;
                            // Only if nothing new started in the meantime.
                            if coordinator.state().is_terminal() {
                                let show = settings.get().show_idle_indicator;
                                let _ = hud.update(cx, |view, _, cx| {
                                    view.state = if show {
                                        PillState::IdleDot
                                    } else {
                                        PillState::Hidden
                                    };
                                    cx.notify();
                                });
                            }
                        })
                        .detach();
                    }
                }
            }
        }
    })
    .detach();
}

/// Repaints the hands-free timer and the "Still working…" state.
fn run_clock(hud: WindowHandle<PillView>, cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(CLOCK_TICK).await;
            let alive = hud.update(cx, |view, _, cx| {
                // Only repaints on a real difference: a per-second timer must
                // not become a 5 Hz redraw of a static pill.
                if view.refresh_clock() {
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();
}

fn play_transition(state: DictationState, sounds: bool) {
    let earcon = match state {
        DictationState::Warming => Some(Earcon::Start),
        DictationState::Recording { locked: true } => Some(Earcon::Lock),
        DictationState::Finalizing => Some(Earcon::Stop),
        DictationState::Cancelled => Some(Earcon::Cancel),
        DictationState::Failed(_) => Some(Earcon::Error),
        DictationState::Done(DictationOutcome::Inserted) => Some(Earcon::Success),
        // A silent or queued dictation is not a failure and gets no alarm.
        _ => None,
    };
    if let Some(earcon) = earcon {
        sound::play(earcon, sounds);
    }
}

fn handle_tray(
    mut commands: UnboundedReceiver<TrayCommand>,
    hud: WindowHandle<PillView>,
    services: Services,
    cx: &mut App,
) {
    let _ = hud;
    cx.spawn(async move |cx| {
        while let Some(command) = commands.recv().await {
            let services = services.clone();
            cx.update(|cx| match command {
                TrayCommand::ToggleDictation => {
                    // The menu path for anyone who cannot hold a key: it starts
                    // hands-free directly rather than pretending to be a hold.
                    if services.coordinator.state().is_recording() {
                        services.coordinator.handle(HotkeyIntent::Finalize);
                    } else if services.coordinator.handle(HotkeyIntent::Begin) {
                        services.coordinator.handle(HotkeyIntent::LockIn);
                    }
                }
                TrayCommand::OpenHistory => views::open_history(&services, cx),
                TrayCommand::OpenDictionary => views::open_dictionary(&services, cx),
                TrayCommand::OpenSettings => views::open_settings(&services, cx),
                TrayCommand::OpenOnboarding => views::open_onboarding(&services, cx),
                TrayCommand::About => views::open_about(cx),
                TrayCommand::Quit => cx.quit(),
            });
        }
    })
    .detach();
}

/// Runs the pure hotkey grammar over the hook's event stream.
///
/// The grammar is clock-free and returns timers as effects, so the double-tap
/// window lives here as a deadline on the next receive rather than as a task
/// that could outlive the gesture it belongs to.
async fn run_hotkey_grammar(
    mut events: UnboundedReceiver<hotkey_hook::TimedEvent>,
    coordinator: Arc<DictationCoordinator>,
    settings: Arc<SettingsStore>,
) {
    let mut processor = HotkeyProcessor::new();
    let mut deadline: Option<Duration> = None;

    loop {
        let (event, at) = match deadline {
            Some(deadline_at) => {
                let wait = deadline_at.saturating_sub(hotkey_hook::now());
                match tokio::time::timeout(wait, events.recv()).await {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(_) => {
                        deadline = None;
                        (
                            jot_core::hotkey::HotkeyEvent::DoubleTapTimeout,
                            hotkey_hook::now(),
                        )
                    }
                }
            }
            None => match events.recv().await {
                Some(event) => event,
                None => break,
            },
        };

        // Read per-event: a toggle flipped in Settings applies to the next
        // gesture, never mid-gesture.
        processor.double_tap_lock_enabled = settings.get().double_tap_lock;
        let effects = processor.handle(event, at);

        if let Some(window) = effects.arm_timer {
            deadline = Some(at + window);
        }
        if effects.disarm_timer {
            deadline = None;
        }
        for intent in effects.intents {
            // A refused begin must reach the grammar, or a Space-lock on the
            // phantom session strands it and eats the next dictation.
            if !coordinator.handle(intent) {
                processor.reset();
            }
        }
        hotkey_hook::set_session_active(processor.is_session_active());
        hotkey_hook::set_key_held(processor.is_key_held());
    }
}

/// Rebinds the hook when the dictation key changes in Settings.
async fn follow_hotkey_setting(settings: Arc<SettingsStore>) {
    let mut changes = settings.subscribe();
    while let Ok(key) = changes.recv().await {
        if key == "hotkeyKey" {
            let hotkey = settings.get().hotkey_key;
            tracing::info!(key = hotkey.display_name(), "dictation key rebound");
            hotkey_hook::set_hotkey(hotkey);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_states_worth_hearing_make_a_sound() {
        // A silent dictation and a queued one are not failures; an alarm for
        // either trains the user to distrust the error earcon.
        for state in [
            DictationState::Done(DictationOutcome::Silent),
            DictationState::Done(DictationOutcome::QueuedForRetry),
            DictationState::Idle,
            DictationState::Recording { locked: false },
        ] {
            play_transition(state, false);
        }
    }

    #[test]
    fn the_clock_tick_is_fast_enough_for_a_one_second_timer() {
        assert!(CLOCK_TICK <= Duration::from_millis(500));
    }
}
