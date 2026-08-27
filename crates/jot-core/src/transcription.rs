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

//! The transcription pipeline.
//!
//! ```text
//! WAV → interactions (mode: smart, custom_vocabulary)
//!     → [optional] flash-lite cleanup for per-app tone → validation gate
//!     → replacement engine → inserted text.
//! ```
//!
//! The model does filler removal, self-correction collapse and list formatting
//! itself, so the default path is ONE call. The cleanup pass survives as an
//! opt-in because it is the only thing that carries per-app tone.
//!
//! Rules: one silent retry on transient transcribe failures; cleanup has a hard
//! deadline and NEVER blocks a good transcript; every failure is a typed
//! `TranscriptionError` mapping to the failure matrix.

use crate::dictionary::DictionaryStore;
use crate::gemini::{GeminiClient, TranscriptionError};
use crate::prompt;
use crate::replacement;
use crate::settings::{FormattingPolicy, GeminiConfig, SettingsStore};
use crate::validation;
use crate::{timeout, validation::Verdict};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

/// Cleanup budget: probe median 0.3s; a hard cap so the raw fallback keeps the
/// pipeline fast.
pub const CLEANUP_DEADLINE: Duration = Duration::from_millis(1500);
/// Total bytes of dictionary vocabulary allowed on the wire.
const VOCABULARY_BUDGET: usize = 2_048;
/// The mime type of what Jot records. WAV rather than FLAC: it is what the
/// capture engine already writes, and one fewer encode on the latency path.
pub const AUDIO_MIME_TYPE: &str = "audio/wav";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptionResult {
    pub raw_transcript: String,
    pub cleaned_transcript: String,
    pub model_id: String,
}

/// Snapshot of where the user was dictating, captured at hotkey-down.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DictationContext {
    /// Process image name of the foreground window ("Code.exe").
    pub target_app_exe: Option<String>,
    pub target_app_name: Option<String>,
    /// Process id, so insertion can prove focus never moved.
    pub target_pid: Option<u32>,
}

/// Transcription seam; tests use fakes.
#[async_trait]
pub trait TranscriptionServicing: Send + Sync {
    async fn transcribe(
        &self,
        audio_path: &Path,
        duration: Duration,
        context: &DictationContext,
    ) -> Result<TranscriptionResult, TranscriptionError>;

    /// Opens the connection a transcript will travel over, before there is one.
    ///
    /// A no-op by default so tests and fakes need not care.
    async fn warm(&self) {}
}

/// Vocabulary is suppressed once it has PROVABLY broken a request.
///
/// Keyed on the vocabulary itself rather than a bare flag: editing the
/// Dictionary changes the key and we try again, so one bad entry cannot disable
/// the feature until relaunch with nothing telling the user why.
#[derive(Default)]
struct Suppression {
    blocked: Mutex<Option<u64>>,
}

impl Suppression {
    fn is_blocked(&self, vocabulary: &[String]) -> bool {
        *self.blocked.lock() == Some(hash_of(vocabulary))
    }

    fn block(&self, vocabulary: &[String]) {
        *self.blocked.lock() = Some(hash_of(vocabulary));
    }
}

fn hash_of(vocabulary: &[String]) -> u64 {
    let mut hasher = DefaultHasher::new();
    vocabulary.hash(&mut hasher);
    hasher.finish()
}

pub struct GeminiTranscriptionService {
    client: Arc<GeminiClient>,
    settings: Arc<SettingsStore>,
    dictionary: Arc<DictionaryStore>,
    suppression: Suppression,
}

impl GeminiTranscriptionService {
    pub fn new(
        client: Arc<GeminiClient>,
        settings: Arc<SettingsStore>,
        dictionary: Arc<DictionaryStore>,
    ) -> Self {
        Self {
            client,
            settings,
            dictionary,
            suppression: Suppression::default(),
        }
    }

    fn vocabulary_if_enabled(&self) -> Vec<String> {
        let vocabulary = self.dictionary.sanitized_vocabulary(VOCABULARY_BUDGET);
        if self.suppression.is_blocked(&vocabulary) {
            Vec::new()
        } else {
            vocabulary
        }
    }

    /// The ONE place a transcription request is sent. Every caller — primary,
    /// silent retry, and the empty-transcript second chance — goes through here.
    async fn send_transcribe(
        &self,
        audio: &[u8],
        config: &GeminiConfig,
        policy: FormattingPolicy,
        legacy_endpoint: bool,
        vocabulary: &[String],
        deadline: Duration,
    ) -> Result<String, TranscriptionError> {
        match self
            .send_once(audio, config, policy, legacy_endpoint, vocabulary, deadline)
            .await
        {
            // Fail open. `BadRequest` is deliberately terminal everywhere else,
            // but one strange dictionary entry must never be able to break a
            // user's own dictation. Covers BOTH transports.
            Err(TranscriptionError::BadRequest(message)) if !vocabulary.is_empty() => {
                tracing::error!(
                    "transcribe rejected with vocabulary ({message}) — retrying without it"
                );
                let text = self
                    .send_once(audio, config, policy, legacy_endpoint, &[], deadline)
                    .await?;
                // Only latch once the vocabulary-free retry SUCCEEDS. If it also
                // fails, the vocabulary was innocent — and a bad API key returns
                // 400 here, not 401, so latching eagerly would disable the
                // Dictionary for the rest of the launch over an auth problem.
                self.suppression.block(vocabulary);
                Ok(text)
            }
            other => other,
        }
    }

    /// The transport decision lives in ONE place so the fail-open retry above
    /// cannot silently switch endpoints half way through a recovery.
    async fn send_once(
        &self,
        audio: &[u8],
        config: &GeminiConfig,
        policy: FormattingPolicy,
        legacy_endpoint: bool,
        vocabulary: &[String],
        deadline: Duration,
    ) -> Result<String, TranscriptionError> {
        if legacy_endpoint {
            // Verbatim only — `mode` returns an empty transcript on this
            // endpoint. The tone pass, if enabled, still runs on top.
            self.client
                .transcribe(
                    audio,
                    AUDIO_MIME_TYPE,
                    &config.transcribe_model,
                    &config.endpoint,
                    deadline,
                    vocabulary,
                )
                .await
        } else {
            self.client
                .transcribe_interaction(
                    audio,
                    AUDIO_MIME_TYPE,
                    &config.transcribe_model,
                    &config.endpoint,
                    policy.mode(),
                    vocabulary,
                    deadline,
                )
                .await
        }
    }

    async fn transcribe_with_retry(
        &self,
        audio: &[u8],
        config: &GeminiConfig,
        policy: FormattingPolicy,
        legacy_endpoint: bool,
        vocabulary: &[String],
        deadline: Duration,
    ) -> Result<String, TranscriptionError> {
        match self
            .send_transcribe(audio, config, policy, legacy_endpoint, vocabulary, deadline)
            .await
        {
            // One silent retry for transient classes (audio is safe on disk).
            Err(error @ (TranscriptionError::Network(_) | TranscriptionError::Timeout)) => {
                tracing::info!("transcribe retrying after {error}");
                tokio::time::sleep(Duration::from_millis(500)).await;
                self.send_transcribe(audio, config, policy, legacy_endpoint, vocabulary, deadline)
                    .await
            }
            other => other,
        }
    }

    async fn cleanup_or_fallback(
        &self,
        raw: &str,
        context: &DictationContext,
        config: &GeminiConfig,
    ) -> String {
        let tone = prompt::tone_category(context.target_app_exe.as_deref());
        let rules = self.dictionary.replacement_rules();
        let text = prompt::cleanup_prompt(
            raw,
            tone,
            &self.dictionary.sanitized_vocabulary(VOCABULARY_BUDGET),
            &self.dictionary.spellings(),
        );

        let response = self
            .client
            .cleanup(
                &text,
                &config.cleanup_model,
                &config.endpoint,
                CLEANUP_DEADLINE,
            )
            .await;

        match response {
            Ok(response) => {
                let cleaned = validation::strip_artifacts(&response);
                let verdict = validation::validate(raw, &cleaned);
                if verdict.accepted {
                    // The dictionary's hard guarantee: explicit wrong→right
                    // rules always win.
                    replacement::apply(&rules, &cleaned)
                } else {
                    self.record_gate_trip(&verdict);
                    replacement::apply(&rules, raw)
                }
            }
            // A deadline miss or network hiccup on cleanup never costs the
            // dictation — and the dictionary guarantee still holds.
            Err(error) => {
                tracing::info!("cleanup unavailable ({error}) — inserting raw");
                replacement::apply(&rules, raw)
            }
        }
    }

    fn record_gate_trip(&self, verdict: &Verdict) {
        let trips = self.settings.record_gate_trip(OffsetDateTime::now_utc());
        tracing::warn!(
            reason = verdict.reason.as_deref().unwrap_or("?"),
            trips,
            "cleanup gate REJECTED — inserting raw"
        );
        // Auto-degrade: three gate trips in 24h means cleanup can't be trusted
        // right now — switch to exact transcription until re-enabled. Native
        // smart transcription is unaffected; the user loses an extra, not words.
        if trips >= 3 && self.settings.get().smart_cleanup_pass {
            self.settings
                .update(Some("smartFormattingAutoDegraded"), |settings| {
                    settings.smart_cleanup_pass = false;
                });
            tracing::warn!("cleanup unreliable (3 gate trips in 24h) — tone pass auto-disabled");
        }
    }
}

#[async_trait]
impl TranscriptionServicing for GeminiTranscriptionService {
    async fn warm(&self) {
        if !self.client.has_api_key() {
            return;
        }
        self.client
            .warm(&self.settings.get().gemini_config().endpoint)
            .await;
    }

    async fn transcribe(
        &self,
        audio_path: &Path,
        duration: Duration,
        context: &DictationContext,
    ) -> Result<TranscriptionResult, TranscriptionError> {
        // Fail locally rather than asking Google what it thinks of no key.
        if !self.client.has_api_key() {
            return Err(TranscriptionError::Auth);
        }

        let settings = self.settings.get();
        let config = settings.gemini_config();
        let policy = settings.formatting_policy();
        let legacy_endpoint = settings.legacy_transcribe_endpoint;

        let audio = tokio::fs::read(audio_path).await.map_err(|error| {
            // A missing or unreadable recording is not a transport problem, but
            // it is permanent for this row: the queue must not spin on it.
            TranscriptionError::BadRequest(format!("unreadable recording: {error}"))
        })?;

        // Read once per dictation: a toggle flipped mid-flight must not change
        // the rules this transcript is being produced under.
        let vocabulary = self.vocabulary_if_enabled();
        let deadline = timeout::overall_deadline(duration);

        let raw = self
            .transcribe_with_retry(
                &audio,
                &config,
                policy,
                legacy_endpoint,
                &vocabulary,
                deadline,
            )
            .await?;
        let mut trimmed = raw.trim().to_string();

        if trimmed.is_empty() && duration.as_secs_f64() >= 0.6 {
            // Second chance: an empty result on real audio is sometimes model
            // nondeterminism — one re-send before surfacing anything. It goes
            // through the same policy-aware call as the primary path, so this
            // rare branch cannot end up on a different pipeline.
            tracing::info!(
                seconds = duration.as_secs_f64(),
                "empty transcript on real audio — one re-send"
            );
            trimmed = self
                .send_transcribe(
                    &audio,
                    &config,
                    policy,
                    legacy_endpoint,
                    &vocabulary,
                    deadline,
                )
                .await
                .unwrap_or_default()
                .trim()
                .to_string();
        }
        if trimmed.is_empty() {
            // The coordinator classifies silence vs dropped-transcript by energy.
            return Err(TranscriptionError::EmptyTranscript);
        }

        if !policy.cleanup_pass {
            // Dictionary rules are a HARD guarantee — they apply on every path.
            // The gate is deliberately NOT run here: with no second model there
            // is no independent reference, and validate(raw: X, cleaned: X)
            // passes trivially, so running it would be theatre rather than safety.
            let rules = self.dictionary.replacement_rules();
            let text = replacement::apply(&rules, &trimmed);
            return Ok(TranscriptionResult {
                raw_transcript: trimmed,
                cleaned_transcript: text,
                model_id: format!("{}/{}", config.transcribe_model, policy.mode().as_str()),
            });
        }

        let cleaned = self.cleanup_or_fallback(&trimmed, context, &config).await;
        Ok(TranscriptionResult {
            raw_transcript: trimmed,
            cleaned_transcript: cleaned,
            model_id: format!(
                "{}/{}+{}",
                config.transcribe_model,
                policy.mode().as_str(),
                config.cleanup_model
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppression_is_keyed_on_the_vocabulary_not_a_bare_flag() {
        let suppression = Suppression::default();
        let bad = vec!["exploding".to_string()];
        assert!(!suppression.is_blocked(&bad));

        suppression.block(&bad);
        assert!(suppression.is_blocked(&bad));
        // Editing the Dictionary changes the key, so the feature comes back.
        assert!(!suppression.is_blocked(&["exploding".to_string(), "new".to_string()]));
    }

    #[test]
    fn an_empty_vocabulary_is_never_treated_as_suppressed_by_default() {
        let suppression = Suppression::default();
        assert!(!suppression.is_blocked(&[]));
    }

    #[test]
    fn model_id_records_the_pipeline_that_produced_the_text() {
        let config = GeminiConfig::default();
        let policy = FormattingPolicy {
            native_smart: true,
            cleanup_pass: false,
        };
        assert_eq!(
            format!("{}/{}", config.transcribe_model, policy.mode().as_str()),
            "gemini-3.5-transcribe/smart"
        );
    }
}
