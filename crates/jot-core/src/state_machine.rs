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

//! The pure session-lifecycle transition function.

use serde::{Deserialize, Serialize};

/// Why a dictation session ended unsuccessfully. Copy for each case lives with
/// the failure matrix (docs/design/product-reliability.md, F1–F24).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DictationFailure {
    /// Audio engine failed to start or died.
    Audio,
    /// No input device exists at all.
    NoMicrophone,
    /// Zero buffers captured (engine race, F21) — never shown as an empty transcript.
    NoAudio,
    /// Transport-level failure after the silent retry (F1/F2/F6/F7/F8).
    Network,
    /// 401 — key invalid or revoked (F4/F19).
    Auth,
    /// 403/404 — key fine, model gated/renamed (points at Settings → Advanced).
    ModelAccess,
    /// 400 — permanent request failure.
    BadRequest,
    /// 429 per-minute throttle — clears on its own, retryable from History.
    RateLimited,
    /// 429 with a hard (daily) quota (F5).
    QuotaExhausted,
    /// Deadline exceeded per `TimeoutPolicy` (F7).
    Timeout,
    /// Validation gate failed even after the verbatim retry (F9a/F10).
    Validation,
    /// API refused via safety block after verbatim retry (F12).
    SafetyBlocked,
    /// Disk write failure (F22).
    Storage,
}

/// Terminal outcome of a successful-enough session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictationOutcome {
    /// Text landed at the cursor via the insertion ladder.
    Inserted,
    /// Focus changed mid-flight; user was offered a chip instead of a blind paste (F17).
    AwaitingChip,
    /// Ladder exhausted; text left on the clipboard with a visible hint (F16).
    CopiedToClipboard,
    /// A password field was focused at insert time — text lives in History only (F18).
    HeldForSecureField,
    /// Offline or transient failure; recording queued for auto-retry (F1).
    QueuedForRetry,
    /// Silence-only audio (F9b) — kept in History, no error theatrics.
    Silent,
}

/// A single dictation session's lifecycle. The coordinator may run several
/// sessions concurrently (one recording + N in flight); each session steps
/// through this machine independently, keyed by its UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DictationState {
    #[default]
    Idle,
    /// Hotkey went down: session folder created, audio engine starting.
    Warming,
    Recording {
        locked: bool,
    },
    /// Key released / stop pressed: engine stopping, WAV flushed.
    Finalizing,
    Transcribing,
    Inserting,
    Done(DictationOutcome),
    Cancelled,
    Failed(DictationFailure),
}

impl DictationState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            DictationState::Done(_) | DictationState::Cancelled | DictationState::Failed(_)
        )
    }

    pub fn is_recording(self) -> bool {
        matches!(self, DictationState::Recording { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictationEvent {
    // Hotkey intents
    HotkeyBegin,
    LockIn,
    Finalize,
    Cancel,
    /// A non-modifier key was typed within the interruption window — accidental chord.
    AbortAccidental,

    // Audio
    EngineStarted,
    EngineFailed(DictationFailure),
    AudioFinalized,
    NoAudioCaptured,
    SilenceOnly,

    // Transcription
    TranscriptReady,
    TranscriptFailed(DictationFailure),
    QueuedForRetry,

    // Insertion
    Inserted,
    InsertionFellBackToClipboard,
    FrontmostChangedAwaitingChip,
    InsertionBlockedSecure,
}

/// Pure transition function — the only place session-lifecycle rules live.
/// Returns `None` for events that are invalid/ignored in the given state (stale
/// completions, double events); callers log-and-drop those.
pub fn transition(state: DictationState, event: DictationEvent) -> Option<DictationState> {
    use DictationEvent as E;
    use DictationState as S;

    match (state, event) {
        // idle → warming
        (S::Idle, E::HotkeyBegin) => Some(S::Warming),

        // warming
        (S::Warming, E::EngineStarted) => Some(S::Recording { locked: false }),
        (S::Warming, E::EngineFailed(failure)) => Some(S::Failed(failure)),
        (S::Warming, E::Cancel | E::AbortAccidental) => Some(S::Cancelled),
        // Key released before the engine even reported started: still a real
        // dictation — finalize with whatever was captured (prewarm means audio
        // runs from t=0).
        (S::Warming, E::Finalize) => Some(S::Finalizing),

        // recording
        (S::Recording { locked: false }, E::LockIn) => Some(S::Recording { locked: true }),
        (S::Recording { .. }, E::Finalize) => Some(S::Finalizing),
        (S::Recording { .. }, E::Cancel | E::AbortAccidental) => Some(S::Cancelled),
        // Mid-recording engine death: whatever hit the WAV is preserved; the
        // finalize path decides between transcribing the partial audio and
        // surfacing the error.
        (S::Recording { .. }, E::EngineFailed(_)) => Some(S::Finalizing),

        // finalizing
        (S::Finalizing, E::AudioFinalized) => Some(S::Transcribing),
        (S::Finalizing, E::NoAudioCaptured) => Some(S::Failed(DictationFailure::NoAudio)),
        (S::Finalizing, E::SilenceOnly) => Some(S::Done(DictationOutcome::Silent)),
        (S::Finalizing, E::Cancel) => Some(S::Cancelled),

        // transcribing
        (S::Transcribing, E::TranscriptReady) => Some(S::Inserting),
        // Empty transcript on a very short clip = silence, not an error (F9b).
        (S::Transcribing, E::SilenceOnly) => Some(S::Done(DictationOutcome::Silent)),
        (S::Transcribing, E::TranscriptFailed(failure)) => Some(S::Failed(failure)),
        (S::Transcribing, E::QueuedForRetry) => Some(S::Done(DictationOutcome::QueuedForRetry)),
        (S::Transcribing, E::Cancel) => Some(S::Cancelled),

        // inserting — no cancel here: the text exists, History has it regardless.
        (S::Inserting, E::Inserted) => Some(S::Done(DictationOutcome::Inserted)),
        (S::Inserting, E::InsertionFellBackToClipboard) => {
            Some(S::Done(DictationOutcome::CopiedToClipboard))
        }
        (S::Inserting, E::FrontmostChangedAwaitingChip) => {
            Some(S::Done(DictationOutcome::AwaitingChip))
        }
        (S::Inserting, E::InsertionBlockedSecure) => {
            Some(S::Done(DictationOutcome::HeldForSecureField))
        }

        _ => None,
    }
}

/// Why a session ended with no speech.
///
/// Deliberately NOT a payload on `DictationOutcome::Silent`: the only thing that
/// differs is the sentence shown in the pill, and widening the state machine —
/// and every exhaustive match over it — for a copy change would be the wrong trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SilenceReason {
    /// Genuinely quiet — a held key in a quiet room, or a muted mic.
    #[default]
    NoSpeech,
    /// A loud room with nothing rising above it. The recording is KEPT, because a
    /// high absolute peak means the judgement could be wrong.
    TooNoisy,
}

#[cfg(test)]
mod tests {
    use super::DictationEvent as E;
    use super::DictationState as S;
    use super::*;

    #[test]
    fn hold_release_runs_the_happy_path() {
        let s = transition(S::Idle, E::HotkeyBegin).unwrap();
        assert_eq!(s, S::Warming);
        let s = transition(s, E::EngineStarted).unwrap();
        assert_eq!(s, S::Recording { locked: false });
        let s = transition(s, E::Finalize).unwrap();
        assert_eq!(s, S::Finalizing);
        let s = transition(s, E::AudioFinalized).unwrap();
        assert_eq!(s, S::Transcribing);
        let s = transition(s, E::TranscriptReady).unwrap();
        assert_eq!(s, S::Inserting);
        let s = transition(s, E::Inserted).unwrap();
        assert_eq!(s, S::Done(DictationOutcome::Inserted));
        assert!(s.is_terminal());
    }

    #[test]
    fn release_during_warming_still_finalizes() {
        assert_eq!(transition(S::Warming, E::Finalize), Some(S::Finalizing));
    }

    #[test]
    fn engine_death_mid_recording_finalizes_rather_than_failing() {
        assert_eq!(
            transition(
                S::Recording { locked: true },
                E::EngineFailed(DictationFailure::Audio)
            ),
            Some(S::Finalizing)
        );
    }

    #[test]
    fn inserting_ignores_cancel() {
        assert_eq!(transition(S::Inserting, E::Cancel), None);
    }

    #[test]
    fn locked_recording_cannot_lock_again() {
        assert_eq!(transition(S::Recording { locked: true }, E::LockIn), None);
    }

    #[test]
    fn stale_completions_are_dropped() {
        assert_eq!(transition(S::Idle, E::TranscriptReady), None);
        assert_eq!(transition(S::Cancelled, E::Inserted), None);
    }
}
