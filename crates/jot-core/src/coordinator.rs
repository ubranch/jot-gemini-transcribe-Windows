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

//! The brain: translates hotkey intents into session lifecycle via the pure
//! state machine, drives audio → transcription → insertion, and owns the
//! per-session folder and `meta.json` writes.

use crate::audio::{AudioCaptureResult, AudioCapturing, CaptureError};
use crate::file_layout;
use crate::gemini::TranscriptionError;
use crate::hotkey::HotkeyIntent;
use crate::insertion::{InsertionOutcome, TextInserting};
use crate::levels::{NoiseFloorEstimator, curve};
use crate::meta::{SessionMeta, SessionStatus};
use crate::state_machine::{
    DictationEvent, DictationFailure, DictationState, SilenceReason, transition,
};
use crate::transcription::{DictationContext, TranscriptionServicing};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio::task::AbortHandle;
use uuid::Uuid;

/// Below this metered peak the user simply didn't speak. Whisper-quiet speech
/// peaks well above it.
pub const SILENCE_PEAK_THRESHOLD: f32 = 0.06;
/// Clips shorter than this can't contain a word — never sent to the API.
pub const MINIMUM_SENDABLE_DURATION: Duration = Duration::from_millis(400);
/// Zero frames on a hold shorter than this is an accidental blip, not an engine
/// failure — the first buffer simply hadn't arrived yet.
pub const BLIP_HOLD_THRESHOLD: Duration = Duration::from_millis(800);
/// Above this level at key-up, the user was still mid-word. Costs nothing in the
/// common case: already-quiet releases stop instantly.
pub const TRAILING_SPEECH_THRESHOLD: f32 = 0.08;
/// Quiet this long means they finished the word.
pub const TRAILING_QUIET_TO_STOP: Duration = Duration::from_millis(250);
/// Hard cap so a noisy room can never hold a session open.
pub const TRAILING_CAPTURE_CAP: Duration = Duration::from_millis(1500);
/// How far above the measured room a level must sit to still read as speech.
/// Only ever raises the bar from `TRAILING_SPEECH_THRESHOLD`, never lowers it.
pub const TRAILING_FLOOR_MARGIN_DB: f64 = 3.0;
/// The session must have shown at least this much separation between speech and
/// room before its energy readings are trusted enough to stop early. Below it,
/// the capture runs to the cap and never clips a word.
pub const TRAILING_TRUST_SNR: f64 = 12.0;
/// A mis-estimated floor must never make ordinary speech read as quiet.
pub const TRAILING_RELATIVE_CAP: f32 = 0.30;
/// Nothing rose this far above the room means nobody spoke, whatever the
/// absolute peak says. Used to classify an empty transcript honestly.
pub const EMPTY_TRANSCRIPT_SNR_THRESHOLD: f64 = 8.0;
/// A discard needs BOTH a quiet absolute peak and no separation from the room.
/// This clause can only ever prevent a discard, never cause one.
pub const DISCARD_SNR_THRESHOLD: f64 = 6.0;
/// Soft warning at 9:00, hard stop and transcribe at 10:00.
pub const RECORDING_WARN: Duration = Duration::from_secs(540);
pub const RECORDING_CAP: Duration = Duration::from_secs(600);
/// Cancelled recordings at least this long stay recoverable in History — an
/// accidental Esc after minutes of dictation must not destroy the words.
pub const CANCEL_KEEP_THRESHOLD: f64 = 10.0;
/// How often the trailing-capture loop re-reads the level.
const TRAILING_POLL: Duration = Duration::from_millis(60);

pub const COACH_TIP: &str = "Hold to talk · tap Space while holding for hands-free";

/// Everything a surface needs to render, pushed rather than polled.
#[derive(Debug, Clone, PartialEq)]
pub enum CoordinatorUpdate {
    State(DictationState),
    MicLevel(f32),
    /// A neutral, informational line: a coaching tip, a device change, a
    /// refusal. `None` clears it.
    CoachingHint(Option<String>),
    /// The text that just landed, for "paste last" and the success badge.
    Result(String),
    SilenceReason(SilenceReason),
}

pub struct Session {
    pub id: Uuid,
    pub folder: PathBuf,
    pub started_at: OffsetDateTime,
    pub context: DictationContext,
    pub meta: SessionMeta,
    /// Peak mic level from capture — silence vs dropped-transcript evidence.
    pub peak_level: f32,
}

type SessionUpdateFn = Box<dyn Fn(&SessionMeta, &Path) + Send + Sync>;
type SessionDiscardFn = Box<dyn Fn(Uuid) + Send + Sync>;

#[derive(Default)]
pub struct CoordinatorCallbacks {
    /// Fired after every `meta.json` write — the app mirrors sessions into the
    /// history index.
    pub on_session_update: Option<SessionUpdateFn>,
    /// Fired when a session's artifacts were discarded entirely (blips,
    /// no-speech, short cancels) — the app removes its History row. Disk mirrors
    /// the UI: what History doesn't show, we don't store.
    pub on_session_discard: Option<SessionDiscardFn>,
}

/// Injectable seams, so every failure path is reachable headlessly.
pub struct CoordinatorDeps {
    pub audio_factory: Box<dyn Fn() -> Box<dyn AudioCapturing> + Send + Sync>,
    pub transcription: Arc<dyn TranscriptionServicing>,
    pub insertion: Arc<dyn TextInserting>,
    pub context_provider: Box<dyn Fn() -> DictationContext + Send + Sync>,
    /// Injectable clock so hold-duration classification is testable.
    pub now: Box<dyn Fn() -> OffsetDateTime + Send + Sync>,
    pub noise_handling_enabled: Box<dyn Fn() -> bool + Send + Sync>,
    /// Injectable because the real check reads system-wide state. Left
    /// un-injected, every begin-a-session test would fail on a machine that
    /// happens to have a password box focused — a coin flip for a contributor,
    /// not a bug in their change.
    pub secure_field_focused: Box<dyn Fn() -> bool + Send + Sync>,
}

struct Inner {
    state: DictationState,
    session: Option<Session>,
    capture: Option<Box<dyn AudioCapturing>>,
    /// Most recent metered level — decides whether the user was mid-word when
    /// they released the key.
    latest_level: f32,
    mic_level: f32,
    /// How loud the room is. Always measured, never in charge: what it feeds is
    /// gated on `noise_handling_active`, what it records is not.
    noise_floor: NoiseFloorEstimator,
    /// The experiment's state, read ONCE at key-down. A toggle flipped while the
    /// pill is up must not change the rules the live recording is judged by.
    noise_handling_active: bool,
    /// A lock that arrived while the engine was still coming up — applied on
    /// `EngineStarted`, cleared when the session ends.
    pending_lock_in: bool,
    last_silence_reason: SilenceReason,
    last_result: Option<String>,
    cap_tasks: Vec<AbortHandle>,
    /// The in-flight transcription — aborted when the user cancels, so Esc does
    /// not leave the network work running.
    in_flight: Option<AbortHandle>,
}

pub struct DictationCoordinator {
    inner: Mutex<Inner>,
    deps: CoordinatorDeps,
    updates: broadcast::Sender<CoordinatorUpdate>,
    pub callbacks: Mutex<CoordinatorCallbacks>,
}

impl DictationCoordinator {
    pub fn new(deps: CoordinatorDeps) -> Arc<Self> {
        let (updates, _) = broadcast::channel(256);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                state: DictationState::Idle,
                session: None,
                capture: None,
                latest_level: 0.0,
                mic_level: 0.0,
                noise_floor: NoiseFloorEstimator::default(),
                noise_handling_active: false,
                pending_lock_in: false,
                last_silence_reason: SilenceReason::NoSpeech,
                last_result: None,
                cap_tasks: Vec::new(),
                in_flight: None,
            }),
            deps,
            updates,
            callbacks: Mutex::new(CoordinatorCallbacks::default()),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CoordinatorUpdate> {
        self.updates.subscribe()
    }

    pub fn state(&self) -> DictationState {
        self.inner.lock().state
    }

    pub fn last_silence_reason(&self) -> SilenceReason {
        self.inner.lock().last_silence_reason
    }

    pub fn last_result(&self) -> Option<String> {
        self.inner.lock().last_result.clone()
    }

    /// "Delete All History" should also forget the paste-last buffer — a user
    /// wiping their words expects them gone from everywhere we hold them.
    pub fn clear_last_result(&self) {
        self.inner.lock().last_result = None;
    }

    /// Folder of the live session, if any — "Delete All" must not sweep it.
    pub fn active_session_folder(&self) -> Option<PathBuf> {
        self.inner
            .lock()
            .session
            .as_ref()
            .map(|session| session.folder.clone())
    }

    // ----- Hotkey entry point

    /// Returns whether the intent was ACCEPTED. A refused `Begin` (password
    /// field, session already active) must reach the hotkey grammar, or a
    /// Space-lock on the refused session strands it locked and eats the next
    /// dictation.
    pub fn handle(self: &Arc<Self>, intent: HotkeyIntent) -> bool {
        match intent {
            HotkeyIntent::Begin => self.begin_session(),
            HotkeyIntent::LockIn => {
                let mut inner = self.inner.lock();
                // Engine start is deferred a tick (and Bluetooth mics take
                // longer): a lock arriving during warming must not be dropped —
                // latch it and apply the moment the engine reports started.
                if inner.state == DictationState::Warming {
                    inner.pending_lock_in = true;
                    return true;
                }
                self.apply(&mut inner, DictationEvent::LockIn)
            }
            HotkeyIntent::Finalize => {
                self.finalize_session();
                true
            }
            HotkeyIntent::Cancel => {
                self.cancel_session(None);
                true
            }
            HotkeyIntent::ShortTapHint => {
                self.handle_short_tap();
                true
            }
            HotkeyIntent::AbortAccidental => {
                self.handle_accidental_chord();
                true
            }
        }
    }

    /// A quick tap is context-sensitive — a tap must NEVER destroy someone
    /// else's session:
    ///  - hands-free recording → the tap STOPS it (finalize)
    ///  - the tap's own young session (warming / unlocked) → cancel and coach
    ///  - a session in flight → ignored; the transcript is sacred
    ///  - idle or terminal → just the coaching hint
    fn handle_short_tap(self: &Arc<Self>) {
        // Read the state out before matching: a lock guard held in the scrutinee
        // lives for the whole match, and every arm below re-enters the lock.
        let state = self.inner.lock().state;
        match state {
            DictationState::Recording { locked: true } => self.finalize_session(),
            DictationState::Warming | DictationState::Recording { .. } => {
                self.cancel_session(Some(COACH_TIP.into()));
            }
            DictationState::Finalizing
            | DictationState::Transcribing
            | DictationState::Inserting => {
                tracing::info!("short tap ignored — session in flight");
            }
            _ => self.emit_hint(Some(COACH_TIP.into())),
        }
    }

    /// An accidental chord is context-sensitive for the same reason as a short
    /// tap — a chord must NEVER destroy someone else's session. Grammar-locked
    /// sessions finalize on key-down and never reach here, so a locked session
    /// seen here was started from the UI: finalize it rather than destroy words.
    fn handle_accidental_chord(self: &Arc<Self>) {
        let state = self.inner.lock().state;
        match state {
            DictationState::Recording { locked: true } => self.finalize_session(),
            DictationState::Warming | DictationState::Recording { .. } => self.cancel_session(None),
            DictationState::Finalizing
            | DictationState::Transcribing
            | DictationState::Inserting => {
                tracing::info!("accidental chord ignored — session in flight");
            }
            _ => {}
        }
    }

    // ----- Session lifecycle

    fn begin_session(self: &Arc<Self>) -> bool {
        {
            let inner = self.inner.lock();
            if !(inner.state == DictationState::Idle || inner.state.is_terminal()) {
                tracing::info!(state = ?inner.state, "begin ignored: session already active");
                return false;
            }
        }
        // Never record over a password field.
        if (self.deps.secure_field_focused)() {
            self.emit_hint(Some(
                "Can't dictate here — a password field has focus".into(),
            ));
            tracing::info!("begin refused: password field focused");
            return false;
        }

        let id = Uuid::new_v4();
        let started_at = (self.deps.now)();
        let folder = match file_layout::make_session_folder(id, started_at) {
            Ok(folder) => folder,
            Err(error) => {
                tracing::error!(%error, "session setup failed");
                let mut inner = self.inner.lock();
                self.apply(
                    &mut inner,
                    DictationEvent::EngineFailed(DictationFailure::Storage),
                );
                return false;
            }
        };

        let context = (self.deps.context_provider)();
        let mut meta = SessionMeta::new(id, started_at, SessionStatus::Recording);
        meta.target_app_exe = context.target_app_exe.clone();
        meta.target_app_name = context.target_app_name.clone();
        meta.write(&folder);

        // Open the connection now, while the user is still drawing breath. It
        // is the one part of the upload that can be paid for early.
        let transcription = self.deps.transcription.clone();
        crate::runtime::spawn(async move { transcription.warm().await });

        let mut capture = (self.deps.audio_factory)();
        self.install_capture_callbacks(capture.as_mut());

        {
            let mut inner = self.inner.lock();
            inner.state = DictationState::Idle;
            inner.pending_lock_in = false; // never inherit a stale latch
            self.apply(&mut inner, DictationEvent::HotkeyBegin);
            inner.noise_floor = NoiseFloorEstimator::default();
            inner.noise_handling_active = (self.deps.noise_handling_enabled)();
            inner.session = Some(Session {
                id,
                folder: folder.clone(),
                started_at,
                context,
                meta: meta.clone(),
                peak_level: 1.0,
            });
        }
        self.emit_hint(None);
        self.notify_session_update(&meta, &folder);

        // Prewarm on key-down: the engine starts before grammar classification
        // so audio from t=0 is never lost. Deferred off this thread because
        // opening the device blocks — hundreds of milliseconds on a Bluetooth
        // headset renegotiating — and the key press must be acknowledged
        // instantly.
        let coordinator = self.clone();
        crate::runtime::spawn_blocking(move || {
            coordinator.start_capture_if_still_warming(capture, id, &folder)
        });
        true
    }

    fn install_capture_callbacks(self: &Arc<Self>, capture: &mut dyn AudioCapturing) {
        let callbacks = capture.callbacks();
        let mut callbacks = callbacks.lock();

        let coordinator = self.clone();
        callbacks.on_level = Some(Box::new(move |level| {
            coordinator.ingest_level(level, true);
        }));

        let coordinator = self.clone();
        callbacks.on_device_change = Some(Box::new(move |message| {
            tracing::info!(message, "device change surfaced");
            // A mid-recording mic switch must be VISIBLE: an auto-connecting
            // headset changes what is being recorded.
            if coordinator.inner.lock().state.is_recording() {
                coordinator.emit_hint(Some(message));
            }
        }));

        let coordinator = self.clone();
        callbacks.on_write_failure = Some(Box::new(move || {
            coordinator.handle_write_failure();
        }));

        let coordinator = self.clone();
        callbacks.on_engine_died = Some(Box::new(move |message| {
            coordinator.handle_engine_death(message);
        }));
    }

    /// The deferred half of `begin_session`. The session may already be gone by
    /// the time this runs (Esc during warming, a blip release) — starting the
    /// mic for a dead session would record with no session to own the audio.
    fn start_capture_if_still_warming(
        self: &Arc<Self>,
        mut capture: Box<dyn AudioCapturing>,
        session_id: Uuid,
        folder: &Path,
    ) {
        {
            let inner = self.inner.lock();
            let still_ours = inner.session.as_ref().is_some_and(|s| s.id == session_id);
            if !still_ours || inner.state != DictationState::Warming {
                tracing::info!("engine start skipped — session moved on before the mic came up");
                return;
            }
        }

        match capture.start(&file_layout::audio_wav(folder)) {
            Ok(()) => {
                let mut inner = self.inner.lock();
                // Re-check: the session can end while the device was opening.
                if inner.session.as_ref().is_none_or(|s| s.id != session_id) {
                    drop(inner);
                    let _ = capture.stop();
                    return;
                }
                inner.capture = Some(capture);
                self.apply(&mut inner, DictationEvent::EngineStarted);
                if inner.pending_lock_in {
                    inner.pending_lock_in = false;
                    self.apply(&mut inner, DictationEvent::LockIn);
                }
                drop(inner);
                self.start_cap_timers();
            }
            Err(error) => {
                tracing::error!(%error, "audio engine failed to start");
                // Honest failure taxonomy: "Mic didn't start" is wrong advice on
                // a machine with no input device at all. And zero frames were
                // captured, so there is NOTHING to store — a "Failed" History row
                // with a dead-end Retry would be a lie.
                let failure = if error == CaptureError::NoInputDevice {
                    DictationFailure::NoMicrophone
                } else {
                    DictationFailure::Audio
                };
                let mut inner = self.inner.lock();
                self.apply(&mut inner, DictationEvent::EngineFailed(failure));
                self.discard_session_artifacts(&mut inner);
            }
        }
    }

    // ----- Recording cap and disk failure

    fn start_cap_timers(self: &Arc<Self>) {
        let warn = {
            let coordinator = self.clone();
            crate::runtime::spawn(async move {
                tokio::time::sleep(RECORDING_WARN).await;
                if coordinator.inner.lock().state.is_recording() {
                    coordinator.emit_hint(Some("One minute left — 10-minute limit".into()));
                }
            })
            .abort_handle()
        };
        let stop = {
            let coordinator = self.clone();
            crate::runtime::spawn(async move {
                tokio::time::sleep(RECORDING_CAP).await;
                if coordinator.inner.lock().state.is_recording() {
                    tracing::info!("recording cap reached — finalizing");
                    coordinator.finalize_session();
                }
            })
            .abort_handle()
        };
        self.inner.lock().cap_tasks = vec![warn, stop];
    }

    fn handle_write_failure(self: &Arc<Self>) {
        let recording = self.inner.lock().state.is_recording();
        if !recording {
            return;
        }
        tracing::error!("sustained WAV write failures — finalizing with what we have");
        self.update_meta(|meta| meta.error_code = Some("disk_write".into()));
        self.finalize_session();
    }

    /// The engine died mid-recording and could not be revived: a pill that keeps
    /// "listening" while nothing records loses every word after the seam.
    /// Finalize with the partial audio.
    fn handle_engine_death(self: &Arc<Self>, message: String) {
        let recording = self.inner.lock().state.is_recording();
        if !recording {
            return;
        }
        tracing::error!(message, "audio engine died mid-recording");
        self.update_meta(|meta| meta.error_code = Some("engine_died".into()));
        self.emit_hint(Some(format!("{message} — dictating what was captured")));
        self.finalize_session();
    }

    /// The ONE place levels enter the coordinator.
    ///
    /// The trailing-speech loop swaps the level callback, replacing the closure
    /// installed at session start. Both paths must feed the estimator or it
    /// starves in exactly the window that needs it most — the moment after
    /// key-up when we are deciding whether the user is still talking.
    fn ingest_level(&self, level: f32, update_meter: bool) {
        let mut inner = self.inner.lock();
        if update_meter {
            inner.mic_level = level;
            let _ = self.updates.send(CoordinatorUpdate::MicLevel(level));
        }
        inner.latest_level = level;
        inner.noise_floor.ingest(level);
    }

    /// The level below which the user has stopped talking.
    ///
    /// Absolute by default. With the experiment on it rises to sit just above a
    /// loud room — but only UPWARD from the absolute threshold, and only when
    /// the session has proved it can tell speech from the room at all. Without
    /// that separation the energy signal is not trustworthy, so today's
    /// behaviour stands and we pay the full cap rather than risk clipping a word.
    fn trailing_threshold(inner: &Inner) -> f32 {
        if !inner.noise_handling_active {
            return TRAILING_SPEECH_THRESHOLD;
        }
        let (Some(floor_db), Some(snr)) = (
            inner.noise_floor.floor_db(),
            inner.noise_floor.measured_snr(),
        ) else {
            return TRAILING_SPEECH_THRESHOLD;
        };
        if snr < TRAILING_TRUST_SNR {
            return TRAILING_SPEECH_THRESHOLD;
        }
        let target_db = floor_db + TRAILING_FLOOR_MARGIN_DB;
        let relative = curve::level_from_rms(10_f64.powf(target_db / 20.0) as f32);
        // Both bounds are compile-time constants with SPEECH < CAP, so this
        // cannot hit `clamp`'s panic.
        relative.clamp(TRAILING_SPEECH_THRESHOLD, TRAILING_RELATIVE_CAP)
    }

    fn finalize_session(self: &Arc<Self>) {
        let (engine, was_speaking) = {
            let mut inner = self.inner.lock();
            if inner.session.is_none() {
                return;
            }
            // The machine decides first; side effects only on an ACCEPTED
            // finalize. A second stop while a session is in flight must not stop
            // capture or clobber meta.
            if !self.apply(&mut inner, DictationEvent::Finalize) {
                return;
            }
            self.stop_cap_timers(&mut inner);
            let was_speaking = inner.latest_level >= Self::trailing_threshold(&inner);
            inner.mic_level = 0.0;
            let _ = self.updates.send(CoordinatorUpdate::MicLevel(0.0));
            // Hand the engine off and release it immediately: stopping drains
            // the in-flight buffer and tears the graph down — tens to hundreds
            // of milliseconds that must not freeze the caller. The pill already
            // shows Finalizing, so the wait is visually covered.
            (inner.capture.take(), was_speaking)
        };

        let coordinator = self.clone();
        crate::runtime::spawn(async move {
            let result = match engine {
                Some(mut engine) => {
                    if was_speaking {
                        coordinator.capture_trailing_speech(engine.as_mut()).await;
                    }
                    crate::runtime::spawn_blocking(move || engine.stop())
                        .await
                        .unwrap_or_else(|_| AudioCaptureResult::empty())
                }
                None => AudioCaptureResult::empty(),
            };
            coordinator.complete_finalize(result).await;
        });
    }

    /// Keeps the mic open past key-up until the user actually stops talking.
    /// Returns as soon as they are quiet — capped so it can never hang.
    async fn capture_trailing_speech(self: &Arc<Self>, engine: &mut dyn AudioCapturing) {
        let start = Instant::now();
        // The engine keeps reporting levels after key-up; watch them directly.
        {
            let coordinator = self.clone();
            engine.callbacks().lock().on_level = Some(Box::new(move |level| {
                // update_meter: false — the pill already shows Finalizing; this
                // is about hearing whether they are still talking, not drawing
                // bars.
                coordinator.ingest_level(level, false);
            }));
        }
        let mut quiet_since: Option<Instant> = None;
        while start.elapsed() < TRAILING_CAPTURE_CAP {
            tokio::time::sleep(TRAILING_POLL).await;
            let inner = self.inner.lock();
            let quiet = inner.latest_level < Self::trailing_threshold(&inner);
            drop(inner);
            if quiet {
                let since = *quiet_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= TRAILING_QUIET_TO_STOP {
                    break;
                }
            } else {
                quiet_since = None;
            }
        }
        tracing::info!(
            ms = start.elapsed().as_millis() as u64,
            "carried capture past key-up — you were still talking"
        );
    }

    async fn complete_finalize(self: &Arc<Self>, result: AudioCaptureResult) {
        let (session_id, folder, context, duration) = {
            let mut inner = self.inner.lock();
            let Some(session) = inner.session.as_ref() else {
                return;
            };
            let held_for = ((self.deps.now)() - session.started_at)
                .try_into()
                .unwrap_or(Duration::ZERO);

            if result.frames_written == 0 {
                if held_for < BLIP_HOLD_THRESHOLD {
                    // Released before the first buffer landed. Not an error —
                    // and not worth storing (pill feedback only).
                    inner.last_silence_reason = SilenceReason::NoSpeech;
                    let _ = self
                        .updates
                        .send(CoordinatorUpdate::SilenceReason(SilenceReason::NoSpeech));
                    self.apply(&mut inner, DictationEvent::SilenceOnly);
                } else {
                    // Zero frames means nothing a Retry could ever transcribe.
                    // Show the error in the pill, store no dead-end row.
                    self.apply(&mut inner, DictationEvent::NoAudioCaptured);
                }
                self.discard_session_artifacts(&mut inner);
                return;
            }

            let separation = inner.noise_floor.measured_snr();
            // Only meaningful together: a peak with no floor to compare against
            // says nothing about the room, and would read as a measurement.
            let room_floor_db = inner.noise_floor.floor_db();
            let speech_peak_db = room_floor_db.map(|_| inner.noise_floor.peak_db());
            if let Some(session) = inner.session.as_mut() {
                session.peak_level = result.peak_level;
                session.meta.status = SessionStatus::Recorded;
                session.meta.audio_duration_seconds = Some(result.duration_seconds);
                session.meta.gap_markers = result.gap_markers.clone();
                // Recorded unconditionally — this is the data that calibrates
                // the thresholds, and it has to exist before the behaviour that
                // uses it.
                session.meta.noise_floor_dbfs = room_floor_db;
                session.meta.speech_peak_dbfs = speech_peak_db;
            }
            let (meta, folder) = {
                let session = inner.session.as_ref().expect("checked above");
                (session.meta.clone(), session.folder.clone())
            };
            self.notify_session_update(&meta, &folder);

            let duration = Duration::from_secs_f64(result.duration_seconds.max(0.0));
            // Micro-clips can't contain a word — classify locally, never upload.
            let too_short = duration < MINIMUM_SENDABLE_DURATION;
            // Digital silence (a muted mic, zero input volume) can't transcribe
            // either. Two further clauses, both of which can only ever PREVENT a
            // discard: unmeasured loudness means upload rather than guess, and a
            // quiet absolute peak that still rose clearly above the room is
            // someone speaking softly in a quiet place, not a dead mic.
            let rose_above_room = separation.unwrap_or(f64::INFINITY) >= DISCARD_SNR_THRESHOLD;
            let digital_silence = result.peak_is_trustworthy
                && result.peak_level < SILENCE_PEAK_THRESHOLD
                && !rose_above_room;
            if !result.peak_is_trustworthy {
                tracing::info!("peak unmeasured — uploading rather than guessing silence");
            }
            if too_short || digital_silence {
                inner.last_silence_reason = SilenceReason::NoSpeech;
                let _ = self
                    .updates
                    .send(CoordinatorUpdate::SilenceReason(SilenceReason::NoSpeech));
                self.apply(&mut inner, DictationEvent::SilenceOnly);
                self.discard_session_artifacts(&mut inner);
                return;
            }
            self.apply(&mut inner, DictationEvent::AudioFinalized);

            let session = inner.session.as_ref().expect("checked above");
            (
                session.id,
                session.folder.clone(),
                session.context.clone(),
                duration,
            )
        };

        let coordinator = self.clone();
        let started = Instant::now();
        let handle = crate::runtime::spawn(async move {
            let audio = file_layout::audio_wav(&folder);
            match coordinator
                .deps
                .transcription
                .transcribe(&audio, duration, &context)
                .await
            {
                Ok(outcome) => {
                    coordinator
                        .complete_transcription(session_id, outcome, started)
                        .await
                }
                Err(error) => coordinator.fail_transcription(session_id, error),
            }
        });
        self.inner.lock().in_flight = Some(handle.abort_handle());
    }

    async fn complete_transcription(
        self: &Arc<Self>,
        session_id: Uuid,
        outcome: crate::transcription::TranscriptionResult,
        started: Instant,
    ) {
        let context = {
            let mut inner = self.inner.lock();
            // Stale completion.
            if inner.session.as_ref().is_none_or(|s| s.id != session_id) {
                return;
            }
            if let Some(session) = inner.session.as_mut() {
                session.meta.raw_transcript = Some(outcome.raw_transcript.clone());
                session.meta.cleaned_transcript = Some(outcome.cleaned_transcript.clone());
                session.meta.model_id = Some(outcome.model_id.clone());
                session.meta.status = SessionStatus::Transcribing;
            }
            let (meta, folder, context) = {
                let session = inner.session.as_ref().expect("checked above");
                (
                    session.meta.clone(),
                    session.folder.clone(),
                    session.context.clone(),
                )
            };
            self.notify_session_update(&meta, &folder);
            self.apply(&mut inner, DictationEvent::TranscriptReady);
            context
        };

        let insertion = self
            .deps
            .insertion
            .insert(&outcome.cleaned_transcript, &context)
            .await;
        let pipeline_seconds = started.elapsed().as_secs_f64();

        let (status, event) = match insertion {
            InsertionOutcome::Inserted => (SessionStatus::Inserted, DictationEvent::Inserted),
            InsertionOutcome::FrontmostChanged => (
                SessionStatus::AwaitingChip,
                DictationEvent::FrontmostChangedAwaitingChip,
            ),
            InsertionOutcome::FellBackToClipboard => (
                SessionStatus::CopiedToClipboard,
                DictationEvent::InsertionFellBackToClipboard,
            ),
            InsertionOutcome::BlockedSecureField => (
                SessionStatus::HeldSecure,
                DictationEvent::InsertionBlockedSecure,
            ),
        };

        let mut inner = self.inner.lock();
        if let Some(session) = inner.session.as_mut() {
            session.meta.status = status;
            session.meta.pipeline_seconds = Some(pipeline_seconds);
        }
        if let Some(session) = inner.session.as_ref() {
            let (meta, folder) = (session.meta.clone(), session.folder.clone());
            self.notify_session_update(&meta, &folder);
        }
        self.apply(&mut inner, event);
        inner.last_result = Some(outcome.cleaned_transcript.clone());
        let _ = self
            .updates
            .send(CoordinatorUpdate::Result(outcome.cleaned_transcript));
        inner.session = None;
    }

    fn fail_transcription(self: &Arc<Self>, session_id: Uuid, error: TranscriptionError) {
        let mut inner = self.inner.lock();
        if inner.session.as_ref().is_none_or(|s| s.id != session_id) {
            return;
        }

        // Empty transcript: silence is judged by AUDIO ENERGY, not duration — a
        // long quiet hold is "no speech", never "Failed". Speech energy present
        // but no transcript is a real failure, and retryable.
        if error == TranscriptionError::EmptyTranscript {
            let peak = inner.session.as_ref().map_or(1.0, |s| s.peak_level);
            let duration = inner
                .session
                .as_ref()
                .and_then(|s| s.meta.audio_duration_seconds)
                .unwrap_or(0.0);
            // Energy decides; the duration escape hatch only covers true blips —
            // a LOUD one-second "Hi!" with a dropped transcript is a real failure.
            if peak < SILENCE_PEAK_THRESHOLD || duration < 0.6 {
                inner.last_silence_reason = SilenceReason::NoSpeech;
                let _ = self
                    .updates
                    .send(CoordinatorUpdate::SilenceReason(SilenceReason::NoSpeech));
                self.apply(&mut inner, DictationEvent::SilenceOnly);
                self.discard_session_artifacts(&mut inner);
                return;
            }
            // A loud room with nothing rising above it. The model is right that
            // there is no speech here, and calling it a failure hands the user an
            // error earcon, a red pill and a Retry that can never succeed — for
            // the same gesture that reads as a soft "didn't catch that" in a
            // quiet room. Classify honestly, but KEEP the recording: a high
            // absolute peak means we might be wrong, and Retry has to still
            // exist when we are.
            let snr = inner.noise_floor.measured_snr();
            if inner.noise_handling_active
                && snr.is_some_and(|s| s < EMPTY_TRANSCRIPT_SNR_THRESHOLD)
            {
                tracing::info!(
                    snr = snr.unwrap_or_default(),
                    "empty transcript barely above the room — no speech, not a failure"
                );
                // NOT Silent: the visible filter ends with `status != 'silent'`
                // and retention treats Silent as purge-eligible — so a silent row
                // would be invisible AND its audio deleted, while the pill says
                // "saved to History". Failed is in the visible set and gets a
                // Retry button, and "tooNoisy" is not in the auto-drain list, so
                // nothing re-uploads on its own.
                self.mutate_meta(&mut inner, |meta| {
                    meta.status = SessionStatus::Failed;
                    meta.error_code = Some("tooNoisy".into());
                });
                inner.last_silence_reason = SilenceReason::TooNoisy;
                let _ = self
                    .updates
                    .send(CoordinatorUpdate::SilenceReason(SilenceReason::TooNoisy));
                self.apply(&mut inner, DictationEvent::SilenceOnly);
                inner.session = None;
                return;
            }
        }

        // Offline is not a failure — the audio queues and drains on reconnect.
        if error == TranscriptionError::Offline {
            self.mutate_meta(&mut inner, |meta| {
                meta.status = SessionStatus::QueuedForRetry;
                meta.error_code = Some("offline".into());
            });
            self.apply(&mut inner, DictationEvent::QueuedForRetry);
            inner.session = None;
            return;
        }

        let (failure, code, detail) = classify(&error);
        self.mutate_meta(&mut inner, |meta| {
            meta.status = SessionStatus::Failed;
            meta.error_code = Some(code.to_string());
            meta.error_message = detail.clone();
        });
        self.apply(&mut inner, DictationEvent::TranscriptFailed(failure));
        inner.session = None;
    }

    fn cancel_session(self: &Arc<Self>, hint: Option<String>) {
        let engine = {
            let mut inner = self.inner.lock();
            // The machine decides first; side effects only on an ACCEPTED
            // cancel — a rejected cancel must not corrupt meta or the session.
            if !self.apply(&mut inner, DictationEvent::Cancel) {
                drop(inner);
                self.emit_hint(hint);
                return;
            }
            if let Some(in_flight) = inner.in_flight.take() {
                in_flight.abort(); // stop the network work too
            }
            inner.mic_level = 0.0;
            let _ = self.updates.send(CoordinatorUpdate::MicLevel(0.0));
            self.stop_cap_timers(&mut inner);
            inner.capture.take()
        };
        // Feedback is immediate; the bookkeeping can wait.
        self.emit_hint(hint);

        // Teardown drains the tail, so the keep-or-discard decision — which
        // needs the real recorded duration — completes after it. The pill is
        // already showing cancelled, so nothing visible waits.
        let coordinator = self.clone();
        crate::runtime::spawn(async move {
            let result = match engine {
                Some(mut engine) => crate::runtime::spawn_blocking(move || engine.stop())
                    .await
                    .ok(),
                None => None,
            };
            coordinator.complete_cancel(result);
        });
    }

    fn complete_cancel(self: &Arc<Self>, result: Option<AudioCaptureResult>) {
        let mut inner = self.inner.lock();
        // Post-finalize cancels have no live capture — fall back to the duration
        // finalize already persisted. Reading 0 here would destroy a recording
        // of ANY length when Esc lands during transcription.
        let duration = result
            .map(|result| result.duration_seconds)
            .or_else(|| {
                inner
                    .session
                    .as_ref()
                    .and_then(|s| s.meta.audio_duration_seconds)
            })
            .unwrap_or(0.0);
        let has_transcript = inner
            .session
            .as_ref()
            .is_some_and(|s| s.meta.raw_transcript.is_some());

        if has_transcript || duration >= CANCEL_KEEP_THRESHOLD {
            // Long cancels stay recoverable — History shows them with Retry.
            self.mutate_meta(&mut inner, |meta| {
                meta.status = SessionStatus::Cancelled;
                if meta.audio_duration_seconds.is_none() {
                    meta.audio_duration_seconds = Some(duration);
                }
            });
            inner.session = None;
        } else {
            // Blips and short deliberate cancels leave no trace: the pill
            // already gave feedback in the moment; hidden audio is pure
            // liability.
            self.discard_session_artifacts(&mut inner);
        }
    }

    /// Removes the session folder and asks the app to drop its History row.
    fn discard_session_artifacts(&self, inner: &mut Inner) {
        let Some(session) = inner.session.take() else {
            return;
        };
        let _ = std::fs::remove_dir_all(&session.folder);
        if let Some(on_discard) = &self.callbacks.lock().on_session_discard {
            on_discard(session.id);
        }
    }

    // ----- Machine plumbing

    fn apply(&self, inner: &mut Inner, event: DictationEvent) -> bool {
        let Some(next) = transition(inner.state, event) else {
            tracing::debug!(?event, state = ?inner.state, "ignored event");
            return false;
        };
        inner.state = next;
        tracing::info!(state = ?next, "state changed");
        let _ = self.updates.send(CoordinatorUpdate::State(next));
        true
    }

    fn stop_cap_timers(&self, inner: &mut Inner) {
        for task in inner.cap_tasks.drain(..) {
            task.abort();
        }
    }

    fn emit_hint(&self, hint: Option<String>) {
        let _ = self.updates.send(CoordinatorUpdate::CoachingHint(hint));
    }

    fn update_meta(&self, mutate: impl FnOnce(&mut SessionMeta)) {
        let mut inner = self.inner.lock();
        self.mutate_meta(&mut inner, mutate);
    }

    fn mutate_meta(&self, inner: &mut Inner, mutate: impl FnOnce(&mut SessionMeta)) {
        let Some(session) = inner.session.as_mut() else {
            return;
        };
        mutate(&mut session.meta);
        let (meta, folder) = (session.meta.clone(), session.folder.clone());
        self.notify_session_update(&meta, &folder);
    }

    fn notify_session_update(&self, meta: &SessionMeta, folder: &Path) {
        meta.write(folder);
        if let Some(on_update) = &self.callbacks.lock().on_session_update {
            on_update(meta, folder);
        }
    }
}

/// Maps a transport error onto the failure matrix and the code stored in meta.
fn classify(error: &TranscriptionError) -> (DictationFailure, &'static str, Option<String>) {
    match error {
        // Handled before this point, but the mapping stays exhaustive.
        TranscriptionError::Offline => (DictationFailure::Network, "offline", None),
        TranscriptionError::BadRequest(message) => (
            DictationFailure::BadRequest,
            "bad_request",
            Some(message.clone()),
        ),
        TranscriptionError::ModelUnavailable { model, detail } => (
            DictationFailure::ModelAccess,
            "model",
            Some(
                detail
                    .clone()
                    .unwrap_or_else(|| format!("model {model} not accessible")),
            ),
        ),
        TranscriptionError::Network(_) => (DictationFailure::Network, "network", None),
        TranscriptionError::Auth => (DictationFailure::Auth, "auth", None),
        TranscriptionError::RateLimitedDaily => (DictationFailure::QuotaExhausted, "quota", None),
        TranscriptionError::RateLimitedTransient => {
            (DictationFailure::RateLimited, "rate_limit", None)
        }
        TranscriptionError::Timeout => (DictationFailure::Timeout, "timeout", None),
        TranscriptionError::EmptyTranscript => (DictationFailure::Validation, "empty", None),
        TranscriptionError::SafetyBlocked => (DictationFailure::SafetyBlocked, "safety", None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{CaptureCallbacks, FakeCapture};
    use crate::insertion::FakeInserter;
    use crate::state_machine::DictationOutcome;
    use crate::transcription::TranscriptionResult;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ScriptedService {
        response: Mutex<Option<Result<TranscriptionResult, TranscriptionError>>>,
        calls: Arc<AtomicUsize>,
        warmups: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TranscriptionServicing for ScriptedService {
        async fn warm(&self) {
            self.warmups.fetch_add(1, Ordering::SeqCst);
        }

        async fn transcribe(
            &self,
            _audio: &Path,
            _duration: Duration,
            _context: &DictationContext,
        ) -> Result<TranscriptionResult, TranscriptionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.response
                .lock()
                .take()
                .unwrap_or(Ok(TranscriptionResult {
                    raw_transcript: "hello there".into(),
                    cleaned_transcript: "Hello there.".into(),
                    model_id: "test".into(),
                }))
        }
    }

    struct Harness {
        _temp: tempfile::TempDir,
        _guard: parking_lot::MutexGuard<'static, ()>,
        coordinator: Arc<DictationCoordinator>,
        calls: Arc<AtomicUsize>,
        discarded: Arc<AtomicUsize>,
        warmups: Arc<AtomicUsize>,
    }

    fn harness(
        capture: FakeCapture,
        response: Option<Result<TranscriptionResult, TranscriptionError>>,
        insertion: InsertionOutcome,
        secure: bool,
    ) -> Harness {
        let guard = file_layout::TEST_ROOT_LOCK.lock();
        let temp = tempfile::tempdir().unwrap();
        file_layout::set_override_root(Some(temp.path().to_path_buf()));

        let calls = Arc::new(AtomicUsize::new(0));
        let discarded = Arc::new(AtomicUsize::new(0));
        let warmups = Arc::new(AtomicUsize::new(0));
        let result = capture.result.clone();
        let start_error = capture.start_error.clone();
        let levels = capture.levels.clone();

        let coordinator = DictationCoordinator::new(CoordinatorDeps {
            audio_factory: Box::new(move || {
                Box::new(FakeCapture {
                    callbacks: Arc::new(Mutex::new(CaptureCallbacks::default())),
                    result: result.clone(),
                    start_error: start_error.clone(),
                    levels: levels.clone(),
                    started: false,
                    stopped: false,
                })
            }),
            transcription: Arc::new(ScriptedService {
                response: Mutex::new(response),
                calls: calls.clone(),
                warmups: warmups.clone(),
            }),
            insertion: Arc::new(FakeInserter::with_outcome(insertion)),
            context_provider: Box::new(DictationContext::default),
            now: Box::new(OffsetDateTime::now_utc),
            noise_handling_enabled: Box::new(|| false),
            secure_field_focused: Box::new(move || secure),
        });
        {
            let discarded = discarded.clone();
            coordinator.callbacks.lock().on_session_discard = Some(Box::new(move |_| {
                discarded.fetch_add(1, Ordering::SeqCst);
            }));
        }
        Harness {
            _temp: temp,
            _guard: guard,
            coordinator,
            calls,
            discarded,
            warmups,
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            file_layout::set_override_root(None);
        }
    }

    /// The coordinator hands work to background tasks; give them a turn.
    async fn settle() {
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_held_key_runs_the_whole_pipeline() {
        let harness = harness(
            FakeCapture::default(),
            None,
            InsertionOutcome::Inserted,
            false,
        );
        assert!(harness.coordinator.handle(HotkeyIntent::Begin));
        settle().await;
        assert!(harness.coordinator.state().is_recording());

        harness.coordinator.handle(HotkeyIntent::Finalize);
        settle().await;

        assert_eq!(
            harness.coordinator.state(),
            DictationState::Done(DictationOutcome::Inserted)
        );
        assert_eq!(
            harness.coordinator.last_result().as_deref(),
            Some("Hello there.")
        );
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_password_field_refuses_the_session_so_the_grammar_can_reset() {
        let harness = harness(
            FakeCapture::default(),
            None,
            InsertionOutcome::Inserted,
            true,
        );
        assert!(!harness.coordinator.handle(HotkeyIntent::Begin));
        assert_eq!(harness.coordinator.state(), DictationState::Idle);
    }

    #[tokio::test]
    async fn a_second_begin_is_refused_while_a_session_is_live() {
        let harness = harness(
            FakeCapture::default(),
            None,
            InsertionOutcome::Inserted,
            false,
        );
        assert!(harness.coordinator.handle(HotkeyIntent::Begin));
        settle().await;
        assert!(!harness.coordinator.handle(HotkeyIntent::Begin));
    }

    #[tokio::test]
    async fn a_missing_microphone_is_named_rather_than_blamed_on_the_engine() {
        let harness = harness(
            FakeCapture {
                start_error: Some(CaptureError::NoInputDevice),
                ..Default::default()
            },
            None,
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        assert_eq!(
            harness.coordinator.state(),
            DictationState::Failed(DictationFailure::NoMicrophone)
        );
        // Zero frames captured means no dead-end History row.
        assert_eq!(harness.discarded.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_blip_with_no_frames_is_silence_not_an_error() {
        let harness = harness(
            FakeCapture {
                result: AudioCaptureResult::empty(),
                ..Default::default()
            },
            None,
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        harness.coordinator.handle(HotkeyIntent::Finalize);
        settle().await;

        assert_eq!(
            harness.coordinator.state(),
            DictationState::Done(DictationOutcome::Silent)
        );
        assert_eq!(harness.calls.load(Ordering::SeqCst), 0, "never uploaded");
        assert_eq!(harness.discarded.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_micro_clip_is_classified_locally_and_never_uploaded() {
        let harness = harness(
            FakeCapture {
                result: AudioCaptureResult {
                    frames_written: 3_200,
                    duration_seconds: 0.2,
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        harness.coordinator.handle(HotkeyIntent::Finalize);
        settle().await;

        assert_eq!(
            harness.coordinator.state(),
            DictationState::Done(DictationOutcome::Silent)
        );
        assert_eq!(harness.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_muted_mic_is_discarded_before_two_pointless_round_trips() {
        let harness = harness(
            FakeCapture {
                result: AudioCaptureResult {
                    frames_written: 32_000,
                    duration_seconds: 2.0,
                    peak_level: 0.001,
                    written_peak_level: 0.001,
                    peak_is_trustworthy: true,
                    gap_markers: Vec::new(),
                },
                // A measured, uniformly dead room: no separation between the
                // loudest sample and the floor.
                levels: vec![0.001; 32],
                ..Default::default()
            },
            None,
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        harness.coordinator.handle(HotkeyIntent::Finalize);
        settle().await;
        assert_eq!(harness.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_quiet_voice_in_a_quiet_room_is_uploaded_not_discarded() {
        // The absolute peak is below the silence threshold, but it rose clearly
        // above the room — someone speaking softly, not a dead microphone.
        let harness = harness(
            FakeCapture {
                result: AudioCaptureResult {
                    frames_written: 32_000,
                    duration_seconds: 2.0,
                    peak_level: 0.05,
                    written_peak_level: 0.05,
                    peak_is_trustworthy: true,
                    gap_markers: Vec::new(),
                },
                levels: {
                    let mut levels = vec![0.0005; 30];
                    levels.extend([0.05; 6]);
                    levels
                },
                ..Default::default()
            },
            None,
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        harness.coordinator.handle(HotkeyIntent::Finalize);
        settle().await;
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unmeasured_peak_uploads_rather_than_guessing_silence() {
        let harness = harness(
            FakeCapture {
                result: AudioCaptureResult {
                    frames_written: 32_000,
                    duration_seconds: 2.0,
                    peak_level: 0.0,
                    written_peak_level: 0.0,
                    peak_is_trustworthy: false,
                    gap_markers: Vec::new(),
                },
                ..Default::default()
            },
            None,
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        harness.coordinator.handle(HotkeyIntent::Finalize);
        settle().await;
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn offline_queues_the_recording_instead_of_failing_it() {
        let harness = harness(
            FakeCapture::default(),
            Some(Err(TranscriptionError::Offline)),
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        harness.coordinator.handle(HotkeyIntent::Finalize);
        settle().await;

        assert_eq!(
            harness.coordinator.state(),
            DictationState::Done(DictationOutcome::QueuedForRetry)
        );
        assert_eq!(harness.discarded.load(Ordering::SeqCst), 0, "audio kept");
    }

    #[tokio::test]
    async fn a_loud_empty_transcript_is_a_real_failure() {
        let harness = harness(
            FakeCapture::default(),
            Some(Err(TranscriptionError::EmptyTranscript)),
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        harness.coordinator.handle(HotkeyIntent::Finalize);
        settle().await;

        assert_eq!(
            harness.coordinator.state(),
            DictationState::Failed(DictationFailure::Validation)
        );
    }

    #[tokio::test]
    async fn a_quiet_empty_transcript_reads_as_no_speech() {
        let harness = harness(
            FakeCapture {
                result: AudioCaptureResult {
                    frames_written: 32_000,
                    duration_seconds: 2.0,
                    // Loud enough to upload, quiet enough to be no-speech when
                    // the transcript comes back empty.
                    peak_level: 0.05,
                    written_peak_level: 0.05,
                    peak_is_trustworthy: false,
                    gap_markers: Vec::new(),
                },
                ..Default::default()
            },
            Some(Err(TranscriptionError::EmptyTranscript)),
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        harness.coordinator.handle(HotkeyIntent::Finalize);
        settle().await;

        assert_eq!(
            harness.coordinator.state(),
            DictationState::Done(DictationOutcome::Silent)
        );
        assert_eq!(
            harness.coordinator.last_silence_reason(),
            SilenceReason::NoSpeech
        );
    }

    #[tokio::test]
    async fn recording_opens_the_connection_before_there_is_anything_to_send() {
        let harness = harness(
            FakeCapture::default(),
            None,
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        assert_eq!(
            harness.warmups.load(Ordering::SeqCst),
            1,
            "the TLS handshake belongs in the time the user spends speaking"
        );
    }

    #[tokio::test]
    async fn a_refused_session_opens_no_connection() {
        // A password field has focus, so there will be no upload to warm for.
        let harness = harness(
            FakeCapture::default(),
            None,
            InsertionOutcome::Inserted,
            true,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        assert_eq!(harness.warmups.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_short_cancel_leaves_no_trace() {
        let harness = harness(
            FakeCapture {
                result: AudioCaptureResult {
                    frames_written: 32_000,
                    duration_seconds: 2.0,
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        harness.coordinator.handle(HotkeyIntent::Cancel);
        settle().await;

        assert_eq!(harness.coordinator.state(), DictationState::Cancelled);
        assert_eq!(harness.discarded.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_long_cancel_stays_recoverable() {
        let harness = harness(
            FakeCapture {
                result: AudioCaptureResult {
                    frames_written: 16_000 * 45,
                    duration_seconds: 45.0,
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        let folder = harness.coordinator.active_session_folder().unwrap();
        harness.coordinator.handle(HotkeyIntent::Cancel);
        settle().await;

        assert_eq!(harness.discarded.load(Ordering::SeqCst), 0);
        assert_eq!(
            SessionMeta::read(&folder).unwrap().status,
            SessionStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn a_short_tap_stops_a_hands_free_session_rather_than_destroying_it() {
        let harness = harness(
            FakeCapture::default(),
            None,
            InsertionOutcome::Inserted,
            false,
        );
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;
        harness.coordinator.handle(HotkeyIntent::LockIn);
        assert_eq!(
            harness.coordinator.state(),
            DictationState::Recording { locked: true }
        );

        harness.coordinator.handle(HotkeyIntent::ShortTapHint);
        settle().await;
        assert_eq!(
            harness.coordinator.state(),
            DictationState::Done(DictationOutcome::Inserted)
        );
    }

    #[tokio::test]
    async fn a_lock_arriving_during_warming_is_latched_not_dropped() {
        let harness = harness(
            FakeCapture::default(),
            None,
            InsertionOutcome::Inserted,
            false,
        );
        // Begin returns before the engine has started, so this lands in Warming.
        harness.coordinator.handle(HotkeyIntent::Begin);
        harness.coordinator.handle(HotkeyIntent::LockIn);
        settle().await;
        assert_eq!(
            harness.coordinator.state(),
            DictationState::Recording { locked: true }
        );
    }

    #[tokio::test]
    async fn insertion_outcomes_are_recorded_faithfully() {
        for (outcome, expected, status) in [
            (
                InsertionOutcome::FrontmostChanged,
                DictationOutcome::AwaitingChip,
                SessionStatus::AwaitingChip,
            ),
            (
                InsertionOutcome::FellBackToClipboard,
                DictationOutcome::CopiedToClipboard,
                SessionStatus::CopiedToClipboard,
            ),
            (
                InsertionOutcome::BlockedSecureField,
                DictationOutcome::HeldForSecureField,
                SessionStatus::HeldSecure,
            ),
        ] {
            let harness = harness(FakeCapture::default(), None, outcome, false);
            harness.coordinator.handle(HotkeyIntent::Begin);
            settle().await;
            let folder = harness.coordinator.active_session_folder().unwrap();
            harness.coordinator.handle(HotkeyIntent::Finalize);
            settle().await;

            assert_eq!(harness.coordinator.state(), DictationState::Done(expected));
            assert_eq!(SessionMeta::read(&folder).unwrap().status, status);
        }
    }

    #[tokio::test]
    async fn subscribers_see_every_state_change() {
        let harness = harness(
            FakeCapture::default(),
            None,
            InsertionOutcome::Inserted,
            false,
        );
        let mut updates = harness.coordinator.subscribe();
        harness.coordinator.handle(HotkeyIntent::Begin);
        settle().await;

        let mut states = Vec::new();
        while let Ok(update) = updates.try_recv() {
            if let CoordinatorUpdate::State(state) = update {
                states.push(state);
            }
        }
        assert_eq!(states[0], DictationState::Warming);
        assert_eq!(states[1], DictationState::Recording { locked: false });
    }

    #[test]
    fn every_transport_error_maps_onto_the_failure_matrix() {
        let cases = [
            (TranscriptionError::Auth, DictationFailure::Auth, "auth"),
            (
                TranscriptionError::RateLimitedDaily,
                DictationFailure::QuotaExhausted,
                "quota",
            ),
            (
                TranscriptionError::RateLimitedTransient,
                DictationFailure::RateLimited,
                "rate_limit",
            ),
            (
                TranscriptionError::Timeout,
                DictationFailure::Timeout,
                "timeout",
            ),
            (
                TranscriptionError::SafetyBlocked,
                DictationFailure::SafetyBlocked,
                "safety",
            ),
        ];
        for (error, failure, code) in cases {
            let (mapped, mapped_code, _) = classify(&error);
            assert_eq!(mapped, failure);
            assert_eq!(mapped_code, code);
        }
    }

    #[test]
    fn a_gated_model_names_the_model_rather_than_blaming_the_key() {
        let (failure, code, detail) = classify(&TranscriptionError::ModelUnavailable {
            model: "gemini-3.5-transcribe".into(),
            detail: None,
        });
        assert_eq!(failure, DictationFailure::ModelAccess);
        assert_eq!(code, "model");
        assert!(detail.unwrap().contains("gemini-3.5-transcribe"));
    }

    #[test]
    fn the_trailing_threshold_only_ever_rises_above_the_absolute_floor() {
        let mut inner = Inner {
            state: DictationState::Idle,
            session: None,
            capture: None,
            latest_level: 0.0,
            mic_level: 0.0,
            noise_floor: NoiseFloorEstimator::default(),
            noise_handling_active: false,
            pending_lock_in: false,
            last_silence_reason: SilenceReason::NoSpeech,
            last_result: None,
            cap_tasks: Vec::new(),
            in_flight: None,
        };
        // Experiment off: always the absolute constant.
        assert_eq!(
            DictationCoordinator::trailing_threshold(&inner),
            TRAILING_SPEECH_THRESHOLD
        );

        // Experiment on but no measurable separation: unchanged.
        inner.noise_handling_active = true;
        for _ in 0..40 {
            inner.noise_floor.ingest(0.2);
        }
        assert_eq!(
            DictationCoordinator::trailing_threshold(&inner),
            TRAILING_SPEECH_THRESHOLD
        );

        // A loud room with clear speech separation raises the bar, but never
        // past the relative cap.
        let mut noisy = NoiseFloorEstimator::default();
        for _ in 0..90 {
            noisy.ingest(0.2);
        }
        for _ in 0..10 {
            noisy.ingest(1.0);
        }
        inner.noise_floor = noisy;
        let threshold = DictationCoordinator::trailing_threshold(&inner);
        assert!(threshold >= TRAILING_SPEECH_THRESHOLD);
        assert!(threshold <= TRAILING_RELATIVE_CAP);
    }
}
