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

//! A queryable index over the session folders.
//!
//! The folders remain the source of truth — one `meta.json` per dictation. The
//! database only makes History fast to search and stats cheap, so a corrupt
//! index is a rebuild rather than a data loss.

use crate::file_layout;
use crate::meta::{SessionMeta, SessionStatus};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, params};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use tokio::sync::broadcast;

/// History is a library of words plus things needing attention — never an event
/// log. Visible: anything with a transcript; retryable failures and
/// offline-queued items; long cancelled recordings (recoverable). Silent rows
/// and short cancels are discarded at the source and filtered here for legacy
/// data. In-flight statuses are NOT visible: the live session would surface in
/// the attention shelf with Retry/Discard controls that double-upload or
/// destroy it mid-flight. Crash recovery reads those separately.
const VISIBLE: &str = "(raw_transcript IS NOT NULL OR cleaned_transcript IS NOT NULL \
     OR status IN ('failed','queuedForRetry') \
     OR (status = 'cancelled' AND duration_seconds >= 10)) \
     AND status != 'silent'";

/// Error codes that a retry could plausibly clear.
const RETRYABLE_ERROR_CODES: [&str; 3] = ["network", "timeout", "rate_limit"];

#[derive(Debug, Clone, PartialEq)]
pub struct DictationRecord {
    pub id: String,
    pub folder: PathBuf,
    pub started_at: OffsetDateTime,
    pub status: String,
    pub target_app_name: Option<String>,
    pub target_app_exe: Option<String>,
    pub duration_seconds: Option<f64>,
    pub raw_transcript: Option<String>,
    pub cleaned_transcript: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub pipeline_seconds: Option<f64>,
}

impl DictationRecord {
    pub fn from_meta(meta: &SessionMeta, folder: &Path) -> Self {
        Self {
            id: meta.id.to_string(),
            folder: folder.to_path_buf(),
            started_at: meta.started_at,
            status: meta.status.as_str().to_string(),
            target_app_name: meta.target_app_name.clone(),
            target_app_exe: meta.target_app_exe.clone(),
            duration_seconds: meta.audio_duration_seconds,
            raw_transcript: meta.raw_transcript.clone(),
            cleaned_transcript: meta.cleaned_transcript.clone(),
            error_code: meta.error_code.clone(),
            error_message: meta.error_message.clone(),
            pipeline_seconds: meta.pipeline_seconds,
        }
    }

    pub fn display_text(&self) -> &str {
        self.cleaned_transcript
            .as_deref()
            .or(self.raw_transcript.as_deref())
            .unwrap_or("")
    }

    pub fn status(&self) -> Option<SessionStatus> {
        SessionStatus::parse(&self.status)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub total_words: usize,
    pub total_dictations: usize,
    pub average_wpm: u32,
}

pub struct HistoryStore {
    connection: Mutex<Connection>,
    changes: broadcast::Sender<()>,
}

impl HistoryStore {
    pub fn standard() -> Result<Self> {
        Self::open(&file_layout::history_sqlite())
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating the history directory")?;
        }
        let connection = Self::open_or_recreate(path)?;
        let (changes, _) = broadcast::channel(16);
        let store = Self {
            connection: Mutex::new(connection),
            changes,
        };
        store.migrate()?;
        Ok(store)
    }

    /// A corrupt index must not disable history and recovery forever — the
    /// folders are the source of truth and `reindex` rebuilds from them, so the
    /// broken file is quarantined and a fresh one takes its place.
    fn open_or_recreate(path: &Path) -> Result<Connection> {
        match Self::open_verified(path) {
            Ok(connection) => Ok(connection),
            Err(error) => {
                tracing::error!(%error, "history index unreadable — quarantining and recreating");
                let stamp = OffsetDateTime::now_utc().unix_timestamp();
                let quarantine = path.with_extension(format!("corrupt-{stamp}.sqlite"));
                let _ = std::fs::rename(path, &quarantine);
                Self::open_verified(path)
            }
        }
    }

    fn open_verified(path: &Path) -> Result<Connection> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        // WAL plus NORMAL: the folders are the source of truth and `reindex`
        // rebuilds the index from them, so a rollback journal's per-write fsync
        // buys nothing but launch stalls.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        // Probe readability so corruption surfaces here, not at the first query.
        connection.pragma_query(None, "schema_version", |_| Ok(()))?;
        Ok(connection)
    }

    fn migrate(&self) -> Result<()> {
        self.connection.lock().execute_batch(
            "CREATE TABLE IF NOT EXISTS dictation (
                 id TEXT PRIMARY KEY,
                 folder TEXT NOT NULL,
                 started_at INTEGER NOT NULL,
                 status TEXT NOT NULL,
                 target_app_name TEXT,
                 target_app_exe TEXT,
                 duration_seconds REAL,
                 raw_transcript TEXT,
                 cleaned_transcript TEXT,
                 error_code TEXT,
                 error_message TEXT,
                 pipeline_seconds REAL
             );
             CREATE INDEX IF NOT EXISTS dictation_started_at ON dictation (started_at);
             CREATE INDEX IF NOT EXISTS dictation_status ON dictation (status);",
        )?;
        Ok(())
    }

    /// Fires after any write so an open History pane refreshes as dictations
    /// land, retries drain, or rows are deleted — no polling.
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changes.subscribe()
    }

    fn notify(&self) {
        let _ = self.changes.send(());
    }

    // ----- Writes

    pub fn upsert(&self, meta: &SessionMeta, folder: &Path) {
        let record = DictationRecord::from_meta(meta, folder);
        let result = self.connection.lock().execute(
            "INSERT INTO dictation (id, folder, started_at, status, target_app_name,
                 target_app_exe, duration_seconds, raw_transcript, cleaned_transcript,
                 error_code, error_message, pipeline_seconds)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET
                 folder = excluded.folder, started_at = excluded.started_at,
                 status = excluded.status, target_app_name = excluded.target_app_name,
                 target_app_exe = excluded.target_app_exe,
                 duration_seconds = excluded.duration_seconds,
                 raw_transcript = excluded.raw_transcript,
                 cleaned_transcript = excluded.cleaned_transcript,
                 error_code = excluded.error_code, error_message = excluded.error_message,
                 pipeline_seconds = excluded.pipeline_seconds",
            params![
                record.id,
                record.folder.to_string_lossy(),
                record.started_at.unix_timestamp(),
                record.status,
                record.target_app_name,
                record.target_app_exe,
                record.duration_seconds,
                record.raw_transcript,
                record.cleaned_transcript,
                record.error_code,
                record.error_message,
                record.pipeline_seconds,
            ],
        );
        match result {
            Ok(_) => self.notify(),
            Err(error) => tracing::error!(%error, "history upsert failed"),
        }
    }

    pub fn delete(&self, id: &str, remove_folder: bool) {
        let folder = self.record(id).map(|record| record.folder);
        let deleted = self
            .connection
            .lock()
            .execute("DELETE FROM dictation WHERE id = ?1", params![id]);
        match deleted {
            Ok(_) => {
                if remove_folder && let Some(folder) = folder {
                    let _ = std::fs::remove_dir_all(folder);
                }
                self.notify();
            }
            Err(error) => tracing::error!(%error, "history delete failed"),
        }
    }

    /// Wipes the index and, optionally, the folders — sweeping the DIRECTORY
    /// rather than the query, because the visible-records filter hides cancelled
    /// sessions whose audio would otherwise survive a "delete everything".
    /// The live session's folder is spared.
    pub fn delete_all(&self, remove_folders: bool, sparing: Option<&Path>) {
        if let Err(error) = self.connection.lock().execute("DELETE FROM dictation", []) {
            tracing::error!(%error, "history delete-all failed");
        }
        self.notify();
        if !remove_folders {
            return;
        }
        let Ok(entries) = std::fs::read_dir(file_layout::recordings_root()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if sparing.is_some_and(|spared| same_path(spared, &path)) {
                continue;
            }
            let _ = std::fs::remove_dir_all(path);
        }
    }

    /// Rebuilds the index from the folders on disk (launch reconciliation).
    ///
    /// Folders are the source of truth: rows whose folders vanished (external
    /// cleanup, a manual delete in Explorer) are pruned rather than shown as
    /// ghosts. The prune walks the UNFILTERED table — `records` hides silent and
    /// short-cancelled rows, and exactly those would otherwise linger forever as
    /// invisible ghosts.
    pub fn reindex(&self) {
        let Ok(entries) = std::fs::read_dir(file_layout::recordings_root()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && let Some(meta) = SessionMeta::read(&path)
            {
                self.upsert(&meta, &path);
            }
        }
        let orphans: Vec<String> = self
            .query("SELECT id, folder FROM dictation", &[], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .into_iter()
            .filter(|(_, folder)| !Path::new(folder).exists())
            .map(|(id, _)| id)
            .collect();
        for id in &orphans {
            self.delete(id, false);
        }
        if !orphans.is_empty() {
            tracing::info!(count = orphans.len(), "reindex pruned orphaned rows");
        }
    }

    // ----- Reads

    fn query<T>(
        &self,
        sql: &str,
        args: &[&dyn rusqlite::ToSql],
        map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Vec<T> {
        let connection = self.connection.lock();
        let Ok(mut statement) = connection.prepare(sql) else {
            return Vec::new();
        };
        match statement.query_map(args, map) {
            Ok(rows) => rows.flatten().collect(),
            Err(error) => {
                tracing::error!(%error, "history query failed");
                Vec::new()
            }
        }
    }

    fn record(&self, id: &str) -> Option<DictationRecord> {
        self.query(
            "SELECT * FROM dictation WHERE id = ?1",
            &[&id],
            row_to_record,
        )
        .pop()
    }

    pub fn records(&self, search: Option<&str>, limit: usize) -> Vec<DictationRecord> {
        match search.map(str::trim).filter(|query| !query.is_empty()) {
            Some(query) => {
                // Escape LIKE wildcards so a search for "100%" finds "100%".
                let pattern = format!(
                    "%{}%",
                    query
                        .replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                );
                self.query(
                    &format!(
                        "SELECT * FROM dictation WHERE (raw_transcript LIKE ?1 ESCAPE '\\'
                             OR cleaned_transcript LIKE ?1 ESCAPE '\\'
                             OR target_app_name LIKE ?1 ESCAPE '\\') AND {VISIBLE}
                         ORDER BY started_at DESC LIMIT ?2"
                    ),
                    &[&pattern, &(limit as i64)],
                    row_to_record,
                )
            }
            None => self.query(
                &format!(
                    "SELECT * FROM dictation WHERE {VISIBLE} ORDER BY started_at DESC LIMIT ?1"
                ),
                &[&(limit as i64)],
                row_to_record,
            ),
        }
    }

    /// Sessions the retry queue should drain: offline-queued plus transient
    /// failures. A daily quota or an auth wall is deliberately absent — those
    /// are account-level and must not be re-sent on a loop.
    pub fn retryable_records(&self) -> Vec<DictationRecord> {
        self.query(
            "SELECT * FROM dictation
             WHERE status = 'queuedForRetry'
                OR (status = 'failed' AND error_code IN (?1, ?2, ?3))
             ORDER BY started_at ASC",
            &[
                &RETRYABLE_ERROR_CODES[0],
                &RETRYABLE_ERROR_CODES[1],
                &RETRYABLE_ERROR_CODES[2],
            ],
            row_to_record,
        )
    }

    /// Non-terminal sessions found at launch: the app died mid-flight.
    pub fn interrupted_records(&self) -> Vec<DictationRecord> {
        self.query(
            "SELECT * FROM dictation
             WHERE status IN ('recording', 'recorded', 'transcribing')
             ORDER BY started_at DESC",
            &[],
            row_to_record,
        )
    }

    /// Every row id, visibility filter bypassed — observability for tests and
    /// the diagnostics pane.
    pub fn all_ids(&self) -> Vec<String> {
        self.query("SELECT id FROM dictation ORDER BY started_at", &[], |row| {
            row.get(0)
        })
    }

    pub fn stats(&self) -> Stats {
        let rows: Vec<(String, Option<f64>)> = self.query(
            "SELECT cleaned_transcript, duration_seconds FROM dictation
             WHERE cleaned_transcript IS NOT NULL",
            &[],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
        let mut total_words = 0;
        let mut timed_words = 0;
        let mut speech = 0.0;
        for (text, duration) in &rows {
            // Whitespace-aware split, so newlines count as separators too.
            let count = text.split_whitespace().count();
            total_words += count;
            if let Some(duration) = duration.filter(|d| *d > 0.0) {
                timed_words += count;
                speech += duration;
            }
        }
        Stats {
            total_words,
            total_dictations: rows.len(),
            // Words per minute only over rows that actually have a duration.
            average_wpm: if speech > 10.0 {
                (timed_words as f64 / (speech / 60.0)) as u32
            } else {
                0
            },
        }
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<DictationRecord> {
    Ok(DictationRecord {
        id: row.get("id")?,
        folder: PathBuf::from(row.get::<_, String>("folder")?),
        started_at: OffsetDateTime::from_unix_timestamp(row.get("started_at")?)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH),
        status: row.get("status")?,
        target_app_name: row.get("target_app_name")?,
        target_app_exe: row.get("target_app_exe")?,
        duration_seconds: row.get("duration_seconds")?,
        raw_transcript: row.get("raw_transcript")?,
        cleaned_transcript: row.get("cleaned_transcript")?,
        error_code: row.get("error_code")?,
        error_message: row.get("error_message")?,
        pipeline_seconds: row.get("pipeline_seconds")?,
    })
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/// Audio retention: purge audio files after N days while KEEPING transcripts
/// and meta forever (until the user deletes them). Audio is never deleted
/// before a transcript exists — that would break Retry.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    /// `0` keeps audio forever; a negative value means "never keep audio" and
    /// purges as soon as a transcript exists.
    pub audio_retention_days: i64,
}

impl RetentionPolicy {
    pub fn purge_expired_audio(&self, recordings_root: &Path, now: OffsetDateTime) -> usize {
        if self.audio_retention_days == 0 {
            return 0;
        }
        let cutoff = if self.audio_retention_days < 0 {
            // Everything eligible, including a dictation that just finished.
            now + time::Duration::seconds(60)
        } else {
            now - time::Duration::days(self.audio_retention_days)
        };

        let Ok(entries) = std::fs::read_dir(recordings_root) else {
            return 0;
        };
        let mut purged = 0;
        for entry in entries.flatten() {
            let folder = entry.path();
            if !folder.is_dir() {
                continue;
            }
            let Some(meta) = SessionMeta::read(&folder) else {
                continue;
            };
            // Transcript-less cancelled rows are retention-IMMUNE: their audio is
            // the only artifact — the "keep long cancels" rule preserved it
            // precisely so Retry works, and History advertises that the audio is
            // kept. Purging it makes that row a lie with a dead-end Retry.
            let has_words = meta.raw_transcript.is_some() || meta.status == SessionStatus::Silent;
            if meta.started_at >= cutoff || !has_words {
                continue;
            }
            let audio = file_layout::audio_wav(&folder);
            if audio.exists() && std::fs::remove_file(&audio).is_ok() {
                purged += 1;
            }
        }
        if purged > 0 {
            tracing::info!(
                purged,
                days = self.audio_retention_days,
                "purged expired audio"
            );
        }
        purged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::SessionMeta;
    use uuid::Uuid;

    fn store() -> (tempfile::TempDir, HistoryStore) {
        let temp = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(&temp.path().join("history.sqlite")).unwrap();
        (temp, store)
    }

    fn meta(status: SessionStatus) -> SessionMeta {
        SessionMeta::new(Uuid::new_v4(), OffsetDateTime::now_utc(), status)
    }

    #[test]
    fn upsert_updates_rather_than_duplicating() {
        let (temp, store) = store();
        let mut record = meta(SessionStatus::Recorded);
        store.upsert(&record, temp.path());
        record.status = SessionStatus::Inserted;
        record.cleaned_transcript = Some("hello there".into());
        store.upsert(&record, temp.path());

        assert_eq!(store.all_ids().len(), 1);
        let rows = store.records(None, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].display_text(), "hello there");
    }

    #[test]
    fn in_flight_and_silent_rows_stay_out_of_history() {
        let (temp, store) = store();
        for status in [
            SessionStatus::Recording,
            SessionStatus::Recorded,
            SessionStatus::Transcribing,
            SessionStatus::Silent,
        ] {
            store.upsert(&meta(status), temp.path());
        }
        assert!(store.records(None, 10).is_empty());
        assert_eq!(store.all_ids().len(), 4);
    }

    #[test]
    fn a_short_cancel_is_hidden_but_a_long_one_is_recoverable() {
        let (temp, store) = store();
        let mut short = meta(SessionStatus::Cancelled);
        short.audio_duration_seconds = Some(3.0);
        store.upsert(&short, temp.path());
        assert!(store.records(None, 10).is_empty());

        let mut long = meta(SessionStatus::Cancelled);
        long.audio_duration_seconds = Some(42.0);
        store.upsert(&long, temp.path());
        assert_eq!(store.records(None, 10).len(), 1);
    }

    #[test]
    fn search_escapes_like_wildcards() {
        let (temp, store) = store();
        let mut hit = meta(SessionStatus::Inserted);
        hit.cleaned_transcript = Some("we hit 100% of target".into());
        store.upsert(&hit, temp.path());
        let mut miss = meta(SessionStatus::Inserted);
        miss.cleaned_transcript = Some("nothing relevant".into());
        store.upsert(&miss, temp.path());

        assert_eq!(store.records(Some("100%"), 10).len(), 1);
        // A bare "%" searches for a literal percent sign. Unescaped it would be
        // a wildcard and match every row, including "nothing relevant".
        assert_eq!(store.records(Some("%"), 10).len(), 1);
    }

    #[test]
    fn only_transient_failures_are_offered_to_the_retry_queue() {
        let (temp, store) = store();
        for code in ["network", "timeout", "rate_limit", "auth", "quota"] {
            let mut record = meta(SessionStatus::Failed);
            record.error_code = Some(code.into());
            store.upsert(&record, temp.path());
        }
        store.upsert(&meta(SessionStatus::QueuedForRetry), temp.path());
        assert_eq!(store.retryable_records().len(), 4);
    }

    #[test]
    fn interrupted_records_are_the_crash_recovery_set() {
        let (temp, store) = store();
        store.upsert(&meta(SessionStatus::Recording), temp.path());
        store.upsert(&meta(SessionStatus::Transcribing), temp.path());
        store.upsert(&meta(SessionStatus::Inserted), temp.path());
        assert_eq!(store.interrupted_records().len(), 2);
    }

    #[test]
    fn stats_count_words_across_whitespace_and_ignore_untimed_rows_for_wpm() {
        let (temp, store) = store();
        let mut timed = meta(SessionStatus::Inserted);
        timed.cleaned_transcript = Some("one two\nthree four".into());
        timed.audio_duration_seconds = Some(60.0);
        store.upsert(&timed, temp.path());
        let mut untimed = meta(SessionStatus::Inserted);
        untimed.cleaned_transcript = Some("five six".into());
        store.upsert(&untimed, temp.path());

        let stats = store.stats();
        assert_eq!(stats.total_words, 6);
        assert_eq!(stats.total_dictations, 2);
        assert_eq!(stats.average_wpm, 4);
    }

    #[test]
    fn reindex_prunes_rows_whose_folders_are_gone() {
        let _guard = file_layout::TEST_ROOT_LOCK.lock();
        let temp = tempfile::tempdir().unwrap();
        file_layout::set_override_root(Some(temp.path().to_path_buf()));
        std::fs::create_dir_all(file_layout::recordings_root()).unwrap();
        let store = HistoryStore::open(&temp.path().join("history.sqlite")).unwrap();

        store.upsert(&meta(SessionStatus::Inserted), &temp.path().join("gone"));
        assert_eq!(store.all_ids().len(), 1);
        store.reindex();
        assert!(store.all_ids().is_empty());

        file_layout::set_override_root(None);
    }

    #[test]
    fn a_corrupt_index_is_quarantined_and_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.sqlite");
        std::fs::write(&path, b"this is definitely not a database").unwrap();

        let store = HistoryStore::open(&path).unwrap();
        store.upsert(&meta(SessionStatus::Inserted), temp.path());
        assert_eq!(store.all_ids().len(), 1);
        assert!(
            std::fs::read_dir(temp.path())
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains("corrupt-"))
        );
    }

    #[test]
    fn retention_keeps_audio_for_a_cancel_that_has_no_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("session");
        std::fs::create_dir_all(&folder).unwrap();
        let mut record = meta(SessionStatus::Cancelled);
        record.started_at = OffsetDateTime::now_utc() - time::Duration::days(30);
        record.write(&folder);
        std::fs::write(file_layout::audio_wav(&folder), b"audio").unwrap();

        let policy = RetentionPolicy {
            audio_retention_days: 7,
        };
        assert_eq!(
            policy.purge_expired_audio(temp.path(), OffsetDateTime::now_utc()),
            0
        );
        assert!(file_layout::audio_wav(&folder).exists());
    }

    #[test]
    fn retention_purges_transcribed_audio_past_the_cutoff() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("session");
        std::fs::create_dir_all(&folder).unwrap();
        let mut record = meta(SessionStatus::Inserted);
        record.started_at = OffsetDateTime::now_utc() - time::Duration::days(30);
        record.raw_transcript = Some("hello".into());
        record.write(&folder);
        std::fs::write(file_layout::audio_wav(&folder), b"audio").unwrap();

        let policy = RetentionPolicy {
            audio_retention_days: 7,
        };
        assert_eq!(
            policy.purge_expired_audio(temp.path(), OffsetDateTime::now_utc()),
            1
        );
        assert!(!file_layout::audio_wav(&folder).exists());
    }

    #[test]
    fn never_keep_audio_purges_a_dictation_that_just_finished() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("session");
        std::fs::create_dir_all(&folder).unwrap();
        let mut record = meta(SessionStatus::Inserted);
        record.raw_transcript = Some("hello".into());
        record.write(&folder);
        std::fs::write(file_layout::audio_wav(&folder), b"audio").unwrap();

        let policy = RetentionPolicy {
            audio_retention_days: -1,
        };
        assert_eq!(
            policy.purge_expired_audio(temp.path(), OffsetDateTime::now_utc()),
            1
        );
    }

    #[test]
    fn keeping_audio_forever_purges_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let policy = RetentionPolicy {
            audio_retention_days: 0,
        };
        assert_eq!(
            policy.purge_expired_audio(temp.path(), OffsetDateTime::now_utc()),
            0
        );
    }
}
