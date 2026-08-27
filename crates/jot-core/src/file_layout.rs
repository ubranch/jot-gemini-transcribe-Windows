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

//! Where everything lives on disk: one folder per dictation holding
//! `audio.wav` (the crash-safe master), and `meta.json`.

use anyhow::{Context, Result};
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use time::OffsetDateTime;
use time::macros::format_description;
use uuid::Uuid;

/// Test hook: unit tests MUST sandbox here — the suite once wrote failed-session
/// folders straight into the user's real History.
static OVERRIDE_ROOT: LazyLock<RwLock<Option<PathBuf>>> = LazyLock::new(|| RwLock::new(None));

pub fn set_override_root(root: Option<PathBuf>) {
    *OVERRIDE_ROOT.write() = root;
}

/// Serialises the tests that swap the override root. The root is process-global
/// state, so two tests sandboxing at once would read each other's directories.
#[doc(hidden)]
pub static TEST_ROOT_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// `%LOCALAPPDATA%\Jot` — per-machine, never roamed. Recordings can reach
/// hundreds of megabytes and roaming profiles would sync every one of them.
pub fn app_support_root() -> PathBuf {
    if let Some(root) = OVERRIDE_ROOT.read().clone() {
        return root;
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Jot")
}

pub fn recordings_root() -> PathBuf {
    app_support_root().join("recordings")
}

pub fn settings_json() -> PathBuf {
    app_support_root().join("settings.json")
}

pub fn dictionary_json() -> PathBuf {
    app_support_root().join("dictionary.json")
}

pub fn history_sqlite() -> PathBuf {
    app_support_root().join("history.sqlite")
}

/// Creates (if needed) and returns a fresh session folder. The name is
/// timestamp-prefixed for human sortability in Explorer.
pub fn make_session_folder(id: Uuid, now: OffsetDateTime) -> Result<PathBuf> {
    let stamp = now
        .format(format_description!(
            "[year][month][day]-[hour][minute][second]"
        ))
        .context("formatting the session timestamp")?;
    let short = &id.simple().to_string()[..8];
    let path = recordings_root().join(format!("{stamp}-{short}"));
    std::fs::create_dir_all(&path)
        .with_context(|| format!("creating session folder {}", path.display()))?;
    Ok(path)
}

pub fn audio_wav(folder: &Path) -> PathBuf {
    folder.join("audio.wav")
}

pub fn meta_json(folder: &Path) -> PathBuf {
    folder.join("meta.json")
}

/// Bytes per second of the capture format: 16 kHz mono 16-bit PCM.
pub const WAV_BYTES_PER_SECOND: u64 = 32_000;
/// Canonical 44-byte RIFF/PCM header written by `hound`.
pub const WAV_HEADER_BYTES: u64 = 44;

/// Duration estimate from WAV byte size — for crash-recovered sessions whose
/// meta never got a duration.
pub fn estimated_duration_of_wav(path: &Path) -> Option<f64> {
    let bytes = std::fs::metadata(path).ok()?.len();
    if bytes <= WAV_HEADER_BYTES + 4096 {
        return None;
    }
    Some((bytes - WAV_HEADER_BYTES) as f64 / WAV_BYTES_PER_SECOND as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn session_folder_is_timestamp_prefixed_and_unique() {
        let _guard = TEST_ROOT_LOCK.lock();
        let temp = tempfile::tempdir().unwrap();
        set_override_root(Some(temp.path().to_path_buf()));

        let id = Uuid::parse_str("0123abcd-0000-4000-8000-000000000000").unwrap();
        let folder = make_session_folder(id, datetime!(2026-08-27 14:05:09 UTC)).unwrap();
        assert!(folder.is_dir());
        assert_eq!(
            folder.file_name().unwrap().to_str().unwrap(),
            "20260827-140509-0123abcd"
        );
        assert_eq!(folder.parent().unwrap(), recordings_root());

        set_override_root(None);
    }

    #[test]
    fn duration_estimate_ignores_header_only_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("audio.wav");

        std::fs::write(&path, vec![0u8; 44]).unwrap();
        assert_eq!(estimated_duration_of_wav(&path), None);

        // 2 seconds of 16 kHz mono 16-bit audio plus the header.
        std::fs::write(&path, vec![0u8; 44 + 64_000]).unwrap();
        assert_eq!(estimated_duration_of_wav(&path), Some(2.0));
    }
}
