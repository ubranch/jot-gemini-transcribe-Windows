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

//! Getting words back: the offline queue and launch-time crash recovery.
//!
//! Neither of these ever auto-inserts. By the time a queued dictation drains,
//! the focus context that produced it is long gone, so the text lands in
//! History and the user decides where it goes.

use crate::file_layout;
use crate::gemini::TranscriptionError;
use crate::history::{DictationRecord, HistoryStore};
use crate::meta::{SessionMeta, SessionStatus};
use crate::transcription::{DictationContext, TranscriptionServicing};
use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How often connectivity is re-checked while dictations are queued.
const CONNECTIVITY_POLL: Duration = Duration::from_secs(5);
/// Deadline used for a recovered session whose duration was never written.
const UNKNOWN_DURATION: Duration = Duration::from_secs(60);

type CountFn = Box<dyn Fn(usize) + Send + Sync>;
type ErrorFn = Box<dyn Fn(TranscriptionError) + Send + Sync>;

#[derive(Default)]
pub struct RetryCallbacks {
    pub on_drained: Option<CountFn>,
    /// Fired once per blocked drain: the queue hit an account-level wall
    /// (auth, daily quota). Rows KEEP their queued promise and retry on the next
    /// external signal (launch, network flap, key change).
    pub on_drain_blocked: Option<ErrorFn>,
}

/// User-facing outcome of a manual Retry — a silent no-op reads as broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    Recovered,
    StillOffline,
    Blocked,
    Failed,
    AlreadyDone,
    Busy,
}

#[derive(Debug, Clone, PartialEq)]
enum ProcessResult {
    Recovered,
    StillOffline,
    Blocked(TranscriptionError),
    Failed,
    Skipped,
}

/// The offline queue: drains on network-restored and on launch, one session at
/// a time. Deliberately simple — no exponential ladders; connectivity IS the
/// retry signal.
pub struct RetryQueue {
    store: Arc<HistoryStore>,
    transcription: Arc<dyn TranscriptionServicing>,
    draining: Arc<AtomicBool>,
    pub callbacks: Arc<Mutex<RetryCallbacks>>,
}

impl RetryQueue {
    pub fn new(store: Arc<HistoryStore>, transcription: Arc<dyn TranscriptionServicing>) -> Self {
        Self {
            store,
            transcription,
            draining: Arc::new(AtomicBool::new(false)),
            callbacks: Arc::new(Mutex::new(RetryCallbacks::default())),
        }
    }

    /// Drains at launch, then whenever the machine comes back online.
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let queue = self.clone();
        crate::runtime::spawn(async move {
            queue.drain().await;
            let mut was_online = connectivity::is_online();
            loop {
                tokio::time::sleep(CONNECTIVITY_POLL).await;
                let online = connectivity::is_online();
                let came_online = online && !was_online;
                was_online = online;
                if came_online {
                    tracing::info!("network restored — draining the retry queue");
                    queue.drain().await;
                }
            }
        })
    }

    pub async fn drain(&self) {
        if self.draining.swap(true, Ordering::SeqCst) {
            return;
        }
        let _guard = DrainGuard(self.draining.clone());

        let retryable = self.store.retryable_records();
        if retryable.is_empty() {
            return;
        }
        let mut recovered = 0;

        for record in retryable {
            match self.process(&record).await {
                ProcessResult::Recovered => recovered += 1,
                ProcessResult::StillOffline => {
                    tracing::info!("still offline — pausing the drain");
                    self.report_drained(recovered);
                    return;
                }
                ProcessResult::Blocked(error) => {
                    // Auth and daily-quota walls apply to every remaining row:
                    // stop, keep their queued status, tell the user ONCE — never
                    // silently convert "will retry automatically" into permanent
                    // failures.
                    tracing::warn!(%error, "drain blocked — keeping the queue intact");
                    self.report_drained(recovered);
                    if let Some(on_blocked) = &self.callbacks.lock().on_drain_blocked {
                        on_blocked(error);
                    }
                    return;
                }
                ProcessResult::Failed | ProcessResult::Skipped => continue,
            }
        }
        self.report_drained(recovered);
    }

    fn report_drained(&self, recovered: usize) {
        if recovered == 0 {
            return;
        }
        if let Some(on_drained) = &self.callbacks.lock().on_drained {
            on_drained(recovered);
        }
    }

    /// Manual per-item retry (the History context menu). Shares the drain guard
    /// so a manual retry cannot double-process a record the drain is already
    /// sending.
    pub async fn retry_single(&self, record: &DictationRecord) -> RetryOutcome {
        if self.draining.swap(true, Ordering::SeqCst) {
            return RetryOutcome::Busy;
        }
        let _guard = DrainGuard(self.draining.clone());

        match self.process(record).await {
            ProcessResult::Recovered => {
                self.report_drained(1);
                RetryOutcome::Recovered
            }
            ProcessResult::StillOffline => RetryOutcome::StillOffline,
            ProcessResult::Blocked(error) => {
                if let Some(on_blocked) = &self.callbacks.lock().on_drain_blocked {
                    on_blocked(error);
                }
                RetryOutcome::Blocked
            }
            ProcessResult::Failed => RetryOutcome::Failed,
            ProcessResult::Skipped => RetryOutcome::AlreadyDone,
        }
    }

    async fn process(&self, record: &DictationRecord) -> ProcessResult {
        let folder = record.folder.clone();
        let Some(mut meta) = SessionMeta::read(&folder) else {
            return ProcessResult::Skipped;
        };
        // Re-read status from disk: a concurrent path may have finished it.
        if matches!(
            meta.status,
            SessionStatus::AwaitingChip | SessionStatus::Inserted | SessionStatus::Recovered
        ) {
            return ProcessResult::Skipped;
        }
        // A transcript already exists (a crash after transcription, audio since
        // purged): recover the WORDS instead of dead-ending on missing audio.
        if meta.raw_transcript.is_some() {
            meta.status = SessionStatus::Recovered;
            meta.error_code = None;
            self.commit(&meta, &folder);
            return ProcessResult::Recovered;
        }

        let audio = file_layout::audio_wav(&folder);
        if !audio.exists() {
            meta.status = SessionStatus::Failed;
            meta.error_code = Some("audio_purged".into());
            self.commit(&meta, &folder);
            return ProcessResult::Failed;
        }

        let duration = session_duration(&meta, &audio);
        let context = DictationContext {
            target_app_exe: meta.target_app_exe.clone(),
            target_app_name: meta.target_app_name.clone(),
            target_pid: None,
        };
        match self
            .transcription
            .transcribe(&audio, duration, &context)
            .await
        {
            Ok(result) => {
                meta.raw_transcript = Some(result.raw_transcript);
                meta.cleaned_transcript = Some(result.cleaned_transcript);
                meta.model_id = Some(result.model_id);
                // Recovered, NOT AwaitingChip: the text was never put on the
                // clipboard, so no chip may promise "Ready to paste".
                meta.status = SessionStatus::Recovered;
                self.commit(&meta, &folder);
                ProcessResult::Recovered
            }
            Err(
                error @ (TranscriptionError::Offline
                | TranscriptionError::Network(_)
                | TranscriptionError::Timeout
                | TranscriptionError::RateLimitedTransient),
            ) => {
                tracing::debug!(%error, "retry still transient");
                ProcessResult::StillOffline
            }
            // Account-level wall: NOT this row's fault. Keep its queued status
            // untouched so the promise survives to the next drain.
            Err(error @ (TranscriptionError::Auth | TranscriptionError::RateLimitedDaily)) => {
                ProcessResult::Blocked(error)
            }
            Err(TranscriptionError::ModelUnavailable { model, detail }) => {
                meta.status = SessionStatus::Failed;
                meta.error_code = Some("model".into());
                meta.error_message =
                    Some(detail.unwrap_or(format!("model {model} not accessible")));
                self.commit(&meta, &folder);
                ProcessResult::Failed
            }
            // Permanent: mark it failed so the queue never spins on it.
            Err(TranscriptionError::BadRequest(message)) => {
                meta.status = SessionStatus::Failed;
                meta.error_code = Some("bad_request".into());
                meta.error_message = Some(message);
                self.commit(&meta, &folder);
                ProcessResult::Failed
            }
            Err(error) => {
                meta.status = SessionStatus::Failed;
                meta.error_code = Some(format!("retry_{error}"));
                self.commit(&meta, &folder);
                ProcessResult::Failed
            }
        }
    }

    fn commit(&self, meta: &SessionMeta, folder: &Path) {
        meta.write(folder);
        self.store.upsert(meta, folder);
    }
}

struct DrainGuard(Arc<AtomicBool>);

impl Drop for DrainGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Crashed sessions never wrote a duration — estimate from the file so the
/// network deadline scales properly instead of using the base budget for a
/// ten-minute recording.
fn session_duration(meta: &SessionMeta, audio: &Path) -> Duration {
    meta.audio_duration_seconds
        .or_else(|| file_layout::estimated_duration_of_wav(audio))
        .map(Duration::from_secs_f64)
        .unwrap_or(UNKNOWN_DURATION)
}

/// Launch-time crash recovery: the MOST RECENT interrupted session is
/// auto-transcribed (that is the one the user actually lost mid-flow); older
/// interrupted folders become manual "Recovered — Retry" rows.
type RecoveredFn = Box<dyn Fn(String) + Send + Sync>;

pub struct RecoveryScanner {
    store: Arc<HistoryStore>,
    transcription: Arc<dyn TranscriptionServicing>,
    pub on_recovered: Mutex<Option<RecoveredFn>>,
}

impl RecoveryScanner {
    pub fn new(store: Arc<HistoryStore>, transcription: Arc<dyn TranscriptionServicing>) -> Self {
        Self {
            store,
            transcription,
            on_recovered: Mutex::new(None),
        }
    }

    pub async fn scan_and_recover(&self) {
        // The reindex walks every recording folder and decodes every meta.json —
        // unbounded as history grows, and the hotkey is already armed by now, so
        // it must not run where a key press would queue behind it.
        let store = self.store.clone();
        let _ = crate::runtime::spawn_blocking(move || store.reindex()).await;

        let interrupted = self.store.interrupted_records();
        if interrupted.is_empty() {
            return;
        }
        tracing::info!(count = interrupted.len(), "interrupted sessions found");

        for (index, record) in interrupted.iter().enumerate() {
            let folder = record.folder.clone();
            let Some(mut meta) = SessionMeta::read(&folder) else {
                continue;
            };

            // Crashed AFTER the transcript was stored (mid-insertion): the text
            // exists — surface it without re-uploading anything. This MUST come
            // before the audio check: under "never keep audio" the recording is
            // already purged, and the other order buries the recovered WORDS as
            // a dead-end "no audio" failure.
            if meta.raw_transcript.is_some() {
                meta.status = SessionStatus::Recovered;
                self.commit(&meta, &folder);
                if index == 0 {
                    self.announce();
                }
                continue;
            }

            let audio = file_layout::audio_wav(&folder);
            if !audio.exists() {
                meta.status = SessionStatus::Failed;
                meta.error_code = Some("no_audio_file".into());
                self.commit(&meta, &folder);
                continue;
            }

            if index > 0 {
                // Older interruptions: keep the audio, mark for manual retry.
                meta.status = SessionStatus::QueuedForRetry;
                meta.error_code.get_or_insert_with(|| "recovered".into());
                self.commit(&meta, &folder);
                continue;
            }

            // Auto-transcribe only the most recent, to stay quota-respectful.
            let context = DictationContext {
                target_app_exe: meta.target_app_exe.clone(),
                target_app_name: meta.target_app_name.clone(),
                target_pid: None,
            };
            match self
                .transcription
                .transcribe(&audio, session_duration(&meta, &audio), &context)
                .await
            {
                Ok(result) => {
                    meta.raw_transcript = Some(result.raw_transcript);
                    meta.cleaned_transcript = Some(result.cleaned_transcript);
                    meta.model_id = Some(result.model_id);
                    // Text ready, the user decides in History — never on the
                    // clipboard, because nothing put it there.
                    meta.status = SessionStatus::Recovered;
                    self.commit(&meta, &folder);
                    self.announce();
                    tracing::info!(id = %record.id, "recovered an interrupted dictation");
                }
                Err(error) => {
                    meta.status = SessionStatus::QueuedForRetry;
                    self.commit(&meta, &folder);
                    tracing::warn!(%error, "recovery transcription failed — queued");
                }
            }
        }
    }

    fn announce(&self) {
        if let Some(on_recovered) = self.on_recovered.lock().as_ref() {
            on_recovered("Recovered your last dictation — it's in History".into());
        }
    }

    fn commit(&self, meta: &SessionMeta, folder: &Path) {
        meta.write(folder);
        self.store.upsert(meta, folder);
    }
}

/// Is there a route to the internet right now?
mod connectivity {
    #[cfg(target_os = "windows")]
    pub fn is_online() -> bool {
        use windows::Win32::Networking::WinInet::{INTERNET_CONNECTION, InternetGetConnectedState};
        let mut flags = INTERNET_CONNECTION::default();
        // A "yes" here is a route, not a reachable Gemini endpoint — captive
        // portals still answer yes. That is fine: the drain treats a transport
        // failure as "still offline" and simply waits for the next edge.
        unsafe { InternetGetConnectedState(&mut flags, 0).is_ok() }
    }

    #[cfg(not(target_os = "windows"))]
    pub fn is_online() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::TranscriptionError;
    use crate::transcription::TranscriptionResult;
    use async_trait::async_trait;
    use time::OffsetDateTime;
    use uuid::Uuid;

    struct ScriptedService {
        responses: Mutex<Vec<Result<TranscriptionResult, TranscriptionError>>>,
        calls: Arc<AtomicBool>,
    }

    impl ScriptedService {
        fn new(responses: Vec<Result<TranscriptionResult, TranscriptionError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    #[async_trait]
    impl TranscriptionServicing for ScriptedService {
        async fn transcribe(
            &self,
            _audio: &Path,
            _duration: Duration,
            _context: &DictationContext,
        ) -> Result<TranscriptionResult, TranscriptionError> {
            self.calls.store(true, Ordering::SeqCst);
            let mut responses = self.responses.lock();
            if responses.is_empty() {
                return Err(TranscriptionError::Offline);
            }
            responses.remove(0)
        }
    }

    fn transcript(text: &str) -> TranscriptionResult {
        TranscriptionResult {
            raw_transcript: text.into(),
            cleaned_transcript: text.into(),
            model_id: "test".into(),
        }
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        store: Arc<HistoryStore>,
        folder: std::path::PathBuf,
    }

    fn fixture(status: SessionStatus, with_audio: bool) -> (Fixture, SessionMeta) {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(HistoryStore::open(&temp.path().join("history.sqlite")).unwrap());
        let folder = temp.path().join("session");
        std::fs::create_dir_all(&folder).unwrap();
        let meta = SessionMeta::new(Uuid::new_v4(), OffsetDateTime::now_utc(), status);
        meta.write(&folder);
        store.upsert(&meta, &folder);
        if with_audio {
            std::fs::write(file_layout::audio_wav(&folder), vec![0u8; 44 + 32_000]).unwrap();
        }
        (
            Fixture {
                _temp: temp,
                store,
                folder,
            },
            meta,
        )
    }

    #[tokio::test]
    async fn a_queued_row_with_audio_is_recovered_never_inserted() {
        let (fixture, _) = fixture(SessionStatus::QueuedForRetry, true);
        let service = Arc::new(ScriptedService::new(vec![Ok(transcript("hello there"))]));
        let queue = RetryQueue::new(fixture.store.clone(), service);

        queue.drain().await;

        let meta = SessionMeta::read(&fixture.folder).unwrap();
        assert_eq!(meta.status, SessionStatus::Recovered);
        assert_eq!(meta.cleaned_transcript.as_deref(), Some("hello there"));
    }

    #[tokio::test]
    async fn an_existing_transcript_is_recovered_without_re_uploading() {
        let (fixture, mut meta) = fixture(SessionStatus::QueuedForRetry, false);
        meta.raw_transcript = Some("already transcribed".into());
        meta.write(&fixture.folder);
        fixture.store.upsert(&meta, &fixture.folder);

        let service = Arc::new(ScriptedService::new(vec![]));
        let called = service.calls.clone();
        let queue = RetryQueue::new(fixture.store.clone(), service);
        queue.drain().await;

        assert!(!called.load(Ordering::SeqCst), "must not re-upload");
        assert_eq!(
            SessionMeta::read(&fixture.folder).unwrap().status,
            SessionStatus::Recovered
        );
    }

    #[tokio::test]
    async fn purged_audio_with_no_transcript_fails_rather_than_spinning() {
        let (fixture, _) = fixture(SessionStatus::QueuedForRetry, false);
        let service = Arc::new(ScriptedService::new(vec![]));
        let queue = RetryQueue::new(fixture.store.clone(), service);
        queue.drain().await;

        let meta = SessionMeta::read(&fixture.folder).unwrap();
        assert_eq!(meta.status, SessionStatus::Failed);
        assert_eq!(meta.error_code.as_deref(), Some("audio_purged"));
    }

    #[tokio::test]
    async fn an_auth_wall_keeps_the_queued_promise_intact() {
        let (fixture, _) = fixture(SessionStatus::QueuedForRetry, true);
        let service = Arc::new(ScriptedService::new(vec![Err(TranscriptionError::Auth)]));
        let queue = RetryQueue::new(fixture.store.clone(), service);

        let blocked = Arc::new(AtomicBool::new(false));
        {
            let blocked = blocked.clone();
            queue.callbacks.lock().on_drain_blocked =
                Some(Box::new(move |_| blocked.store(true, Ordering::SeqCst)));
        }
        queue.drain().await;

        assert!(blocked.load(Ordering::SeqCst), "the user is told once");
        assert_eq!(
            SessionMeta::read(&fixture.folder).unwrap().status,
            SessionStatus::QueuedForRetry
        );
    }

    #[tokio::test]
    async fn a_permanent_failure_is_marked_so_the_queue_stops_retrying_it() {
        let (fixture, _) = fixture(SessionStatus::QueuedForRetry, true);
        let service = Arc::new(ScriptedService::new(vec![Err(
            TranscriptionError::BadRequest("malformed".into()),
        )]));
        let queue = RetryQueue::new(fixture.store.clone(), service);
        queue.drain().await;

        let meta = SessionMeta::read(&fixture.folder).unwrap();
        assert_eq!(meta.status, SessionStatus::Failed);
        assert_eq!(meta.error_code.as_deref(), Some("bad_request"));
        assert!(fixture.store.retryable_records().is_empty());
    }

    #[tokio::test]
    async fn offline_leaves_the_row_queued_for_the_next_edge() {
        let (fixture, _) = fixture(SessionStatus::QueuedForRetry, true);
        let service = Arc::new(ScriptedService::new(vec![Err(TranscriptionError::Offline)]));
        let queue = RetryQueue::new(fixture.store.clone(), service);
        queue.drain().await;

        assert_eq!(
            SessionMeta::read(&fixture.folder).unwrap().status,
            SessionStatus::QueuedForRetry
        );
    }

    #[tokio::test]
    async fn a_finished_row_is_skipped_by_a_manual_retry() {
        let (fixture, mut meta) = fixture(SessionStatus::QueuedForRetry, true);
        meta.status = SessionStatus::Inserted;
        meta.write(&fixture.folder);

        let service = Arc::new(ScriptedService::new(vec![]));
        let queue = RetryQueue::new(fixture.store.clone(), service);
        let record = DictationRecord::from_meta(&meta, &fixture.folder);
        assert_eq!(queue.retry_single(&record).await, RetryOutcome::AlreadyDone);
    }

    #[tokio::test]
    // The lock serialises tests that swap the process-global recordings root.
    // Each `#[tokio::test]` gets its own single-threaded runtime, so holding it
    // across an await cannot starve another test's executor.
    #[allow(clippy::await_holding_lock)]
    async fn only_the_newest_interruption_is_auto_transcribed() {
        let _guard = file_layout::TEST_ROOT_LOCK.lock();
        let temp = tempfile::tempdir().unwrap();
        file_layout::set_override_root(Some(temp.path().to_path_buf()));
        let store = Arc::new(HistoryStore::open(&temp.path().join("history.sqlite")).unwrap());

        let mut folders = Vec::new();
        for (index, offset) in [0_i64, 3600].into_iter().enumerate() {
            let folder = file_layout::recordings_root().join(format!("session-{index}"));
            std::fs::create_dir_all(&folder).unwrap();
            let mut meta = SessionMeta::new(
                Uuid::new_v4(),
                OffsetDateTime::now_utc() - time::Duration::seconds(offset),
                SessionStatus::Recorded,
            );
            meta.audio_duration_seconds = Some(1.0);
            meta.write(&folder);
            store.upsert(&meta, &folder);
            std::fs::write(file_layout::audio_wav(&folder), vec![0u8; 44 + 32_000]).unwrap();
            folders.push(folder);
        }

        let service = Arc::new(ScriptedService::new(vec![Ok(transcript("newest"))]));
        let scanner = RecoveryScanner::new(store.clone(), service);
        scanner.scan_and_recover().await;

        // folders[0] is the newest (offset 0).
        assert_eq!(
            SessionMeta::read(&folders[0]).unwrap().status,
            SessionStatus::Recovered
        );
        assert_eq!(
            SessionMeta::read(&folders[1]).unwrap().status,
            SessionStatus::QueuedForRetry
        );

        file_layout::set_override_root(None);
    }

    #[test]
    fn a_missing_duration_is_estimated_from_the_recording() {
        let temp = tempfile::tempdir().unwrap();
        let audio = temp.path().join("audio.wav");
        std::fs::write(&audio, vec![0u8; 44 + 64_000]).unwrap();
        let meta = SessionMeta::new(
            Uuid::new_v4(),
            OffsetDateTime::now_utc(),
            SessionStatus::Recorded,
        );
        assert_eq!(session_duration(&meta, &audio), Duration::from_secs(2));
    }

    #[test]
    fn an_unmeasurable_recording_falls_back_to_a_bounded_guess() {
        let temp = tempfile::tempdir().unwrap();
        let audio = temp.path().join("missing.wav");
        let meta = SessionMeta::new(
            Uuid::new_v4(),
            OffsetDateTime::now_utc(),
            SessionStatus::Recorded,
        );
        assert_eq!(session_duration(&meta, &audio), UNKNOWN_DURATION);
    }
}
