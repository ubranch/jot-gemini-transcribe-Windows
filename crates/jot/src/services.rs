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

//! The object graph, assembled once and shared by every surface.

use anyhow::Result;
use jot_core::audio::{AudioCapturing, WasapiCapture};
use jot_core::coordinator::{CoordinatorDeps, DictationCoordinator};
use jot_core::dictionary::DictionaryStore;
use jot_core::gemini::GeminiClient;
use jot_core::history::{HistoryStore, RetentionPolicy};
use jot_core::insertion::InsertionCoordinator;
use jot_core::recovery::{RecoveryScanner, RetryQueue};
use jot_core::settings::SettingsStore;
use jot_core::transcription::{GeminiTranscriptionService, TranscriptionServicing};
use jot_core::{credentials, file_layout, win32};
use std::sync::Arc;
use time::OffsetDateTime;

#[derive(Clone)]
pub struct Services {
    pub settings: Arc<SettingsStore>,
    pub dictionary: Arc<DictionaryStore>,
    pub history: Arc<HistoryStore>,
    pub client: Arc<GeminiClient>,
    pub transcription: Arc<dyn TranscriptionServicing>,
    pub coordinator: Arc<DictationCoordinator>,
    pub retry_queue: Arc<RetryQueue>,
}

impl Services {
    pub fn new() -> Result<Self> {
        let settings = SettingsStore::global();
        let dictionary = DictionaryStore::global();
        let history = Arc::new(HistoryStore::standard()?);

        // The key is read fresh on every request: changing it in Settings must
        // take effect on the next dictation, not the next launch.
        let client = Arc::new(GeminiClient::new(Box::new(credentials::api_key)));
        let transcription: Arc<dyn TranscriptionServicing> = Arc::new(
            GeminiTranscriptionService::new(client.clone(), settings.clone(), dictionary.clone()),
        );

        let coordinator = DictationCoordinator::new(CoordinatorDeps {
            audio_factory: {
                let settings = settings.clone();
                // Read per session: switching microphone in Settings applies to
                // the next dictation, never to one already recording.
                Box::new(move || {
                    Box::new(WasapiCapture::for_device(
                        settings.get().preferred_input_device,
                    )) as Box<dyn AudioCapturing>
                })
            },
            transcription: transcription.clone(),
            insertion: Arc::new(InsertionCoordinator::new()),
            // Captured at key-down, so insertion can prove focus never moved and
            // the cleanup prompt knows which tone the target app wants.
            context_provider: Box::new(|| win32::foreground_app().as_context()),
            now: Box::new(OffsetDateTime::now_utc),
            noise_handling_enabled: {
                let settings = settings.clone();
                Box::new(move || settings.get().experimental_noise_handling)
            },
            secure_field_focused: Box::new(|| win32::focus_kind() == win32::FocusKind::Password),
        });

        // The coordinator owns the folders; History mirrors them.
        {
            let history_for_update = history.clone();
            let history_for_discard = history.clone();
            let mut callbacks = coordinator.callbacks.lock();
            callbacks.on_session_update = Some(Box::new(move |meta, folder| {
                history_for_update.upsert(meta, folder);
            }));
            callbacks.on_session_discard = Some(Box::new(move |id| {
                // Disk mirrors the UI: what History doesn't show, we don't store.
                history_for_discard.delete(&id.to_string(), false);
            }));
        }

        let retry_queue = Arc::new(RetryQueue::new(history.clone(), transcription.clone()));

        Ok(Self {
            settings,
            dictionary,
            history,
            client,
            transcription,
            coordinator,
            retry_queue,
        })
    }

    /// Launch work that must not block the first key press: crash recovery, the
    /// offline drain, and audio retention.
    pub fn start_background_work(&self) {
        let scanner = Arc::new(RecoveryScanner::new(
            self.history.clone(),
            self.transcription.clone(),
        ));
        jot_core::runtime::spawn(async move {
            scanner.scan_and_recover().await;
        });
        self.retry_queue.start();

        let settings = self.settings.clone();
        jot_core::runtime::spawn_blocking(move || {
            RetentionPolicy {
                audio_retention_days: settings.get().audio_retention_days,
            }
            .purge_expired_audio(&file_layout::recordings_root(), OffsetDateTime::now_utc());
        });
    }
}
