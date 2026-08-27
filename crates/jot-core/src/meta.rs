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

//! Per-dictation metadata, persisted as `meta.json` in the session folder.
//!
//! Written at every status transition so crash recovery can tell exactly how
//! far a session got. Terminal-status writes are the source of truth for "was
//! anything lost?"

use crate::file_layout;
use serde::{Deserialize, Serialize};
use std::path::Path;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionStatus {
    Recording,
    /// Audio finalized on disk; transcription not yet complete.
    Recorded,
    Transcribing,
    Inserted,
    CopiedToClipboard,
    AwaitingChip,
    HeldSecure,
    QueuedForRetry,
    /// Transcribed after a crash/offline drain — the text was NEVER put on the
    /// clipboard, so no UI may promise "Ready to paste".
    Recovered,
    Silent,
    Cancelled,
    Failed,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Recording => "recording",
            SessionStatus::Recorded => "recorded",
            SessionStatus::Transcribing => "transcribing",
            SessionStatus::Inserted => "inserted",
            SessionStatus::CopiedToClipboard => "copiedToClipboard",
            SessionStatus::AwaitingChip => "awaitingChip",
            SessionStatus::HeldSecure => "heldSecure",
            SessionStatus::QueuedForRetry => "queuedForRetry",
            SessionStatus::Recovered => "recovered",
            SessionStatus::Silent => "silent",
            SessionStatus::Cancelled => "cancelled",
            SessionStatus::Failed => "failed",
        }
    }

    /// The inverse of `as_str`. Not `FromStr`: an unknown status is a
    /// `None` to fall back from, not an error to propagate.
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "recording" => SessionStatus::Recording,
            "recorded" => SessionStatus::Recorded,
            "transcribing" => SessionStatus::Transcribing,
            "inserted" => SessionStatus::Inserted,
            "copiedToClipboard" => SessionStatus::CopiedToClipboard,
            "awaitingChip" => SessionStatus::AwaitingChip,
            "heldSecure" => SessionStatus::HeldSecure,
            "queuedForRetry" => SessionStatus::QueuedForRetry,
            "recovered" => SessionStatus::Recovered,
            "silent" => SessionStatus::Silent,
            "cancelled" => SessionStatus::Cancelled,
            "failed" => SessionStatus::Failed,
            _ => return None,
        })
    }

    /// In-flight statuses are never shown in History: the live session would
    /// surface in the attention shelf with Retry/Discard controls that
    /// double-upload or destroy it mid-flight. Crash recovery reads them
    /// separately and normalizes every one at launch.
    pub fn is_in_flight(self) -> bool {
        matches!(
            self,
            SessionStatus::Recording | SessionStatus::Recorded | SessionStatus::Transcribing
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_app_exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_app_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_duration_seconds: Option<f64>,
    /// Device-change gap markers: seconds-from-start where audio may have a seam.
    #[serde(default)]
    pub gap_markers: Vec<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_transcript: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleaned_transcript: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Human-relevant API error detail (e.g. the 404 body naming the model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Key-up → terminal-state latency, for the local stats overlay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline_seconds: Option<f64>,
    /// How loud the room was (10th-percentile level, dBFS) and how loud the
    /// speech got. Recorded on EVERY session regardless of settings: these are
    /// the numbers that let the noise thresholds be calibrated from real
    /// dictations instead of guessed, and they make a History row diagnostic
    /// ("it was −38 dB in there") rather than merely descriptive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_floor_dbfs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_peak_dbfs: Option<f64>,
}

impl SessionMeta {
    pub fn new(id: Uuid, started_at: OffsetDateTime, status: SessionStatus) -> Self {
        Self {
            id,
            started_at,
            status,
            target_app_exe: None,
            target_app_name: None,
            audio_duration_seconds: None,
            gap_markers: Vec::new(),
            raw_transcript: None,
            cleaned_transcript: None,
            error_code: None,
            error_message: None,
            model_id: None,
            pipeline_seconds: None,
            noise_floor_dbfs: None,
            speech_peak_dbfs: None,
        }
    }

    pub fn display_text(&self) -> &str {
        self.cleaned_transcript
            .as_deref()
            .or(self.raw_transcript.as_deref())
            .unwrap_or("")
    }

    /// Atomic write: a torn `meta.json` after a crash is indistinguishable from
    /// a lost session, so the file is only ever replaced whole.
    pub fn write(&self, folder: &Path) {
        let target = file_layout::meta_json(folder);
        let temp = folder.join("meta.json.tmp");
        let encoded = match serde_json::to_vec_pretty(self) {
            Ok(encoded) => encoded,
            Err(error) => {
                tracing::error!(id = %self.id, %error, "SessionMeta: encode failed");
                return;
            }
        };
        if let Err(error) = std::fs::write(&temp, &encoded) {
            tracing::error!(id = %self.id, %error, "SessionMeta: write failed");
            return;
        }
        if let Err(error) = std::fs::rename(&temp, &target) {
            tracing::error!(id = %self.id, %error, "SessionMeta: rename failed");
            let _ = std::fs::remove_file(&temp);
        }
    }

    pub fn read(folder: &Path) -> Option<Self> {
        let data = std::fs::read(file_layout::meta_json(folder)).ok()?;
        serde_json::from_slice(&data).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn sample() -> SessionMeta {
        SessionMeta::new(
            Uuid::parse_str("0123abcd-0000-4000-8000-000000000000").unwrap(),
            datetime!(2026-08-27 14:05:09 UTC),
            SessionStatus::Recording,
        )
    }

    #[test]
    fn round_trips_through_disk() {
        let temp = tempfile::tempdir().unwrap();
        let mut meta = sample();
        meta.raw_transcript = Some("hello there".into());
        meta.audio_duration_seconds = Some(1.25);
        meta.status = SessionStatus::Inserted;
        meta.write(temp.path());

        assert_eq!(SessionMeta::read(temp.path()).unwrap(), meta);
        // The temp file never survives a successful write.
        assert!(!temp.path().join("meta.json.tmp").exists());
    }

    #[test]
    fn missing_optional_fields_still_decode() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("meta.json"),
            r#"{"id":"0123abcd-0000-4000-8000-000000000000",
                "startedAt":"2026-08-27T14:05:09Z","status":"failed"}"#,
        )
        .unwrap();
        let meta = SessionMeta::read(temp.path()).unwrap();
        assert_eq!(meta.status, SessionStatus::Failed);
        assert!(meta.gap_markers.is_empty());
        assert_eq!(meta.noise_floor_dbfs, None);
    }

    #[test]
    fn status_strings_match_the_persisted_form() {
        for status in [
            SessionStatus::Recording,
            SessionStatus::CopiedToClipboard,
            SessionStatus::QueuedForRetry,
            SessionStatus::Failed,
        ] {
            assert_eq!(SessionStatus::parse(status.as_str()), Some(status));
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", status.as_str()));
        }
        assert_eq!(SessionStatus::parse("bogus"), None);
    }

    #[test]
    fn display_text_prefers_the_cleaned_transcript() {
        let mut meta = sample();
        assert_eq!(meta.display_text(), "");
        meta.raw_transcript = Some("raw".into());
        assert_eq!(meta.display_text(), "raw");
        meta.cleaned_transcript = Some("clean".into());
        assert_eq!(meta.display_text(), "clean");
    }
}
