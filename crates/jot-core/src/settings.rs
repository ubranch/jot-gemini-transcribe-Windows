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

//! JSON-file-backed settings. Endpoint and model IDs are overridable because
//! preview models get renamed.

use crate::file_layout;
use crate::hotkey::HotkeyKey;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use time::OffsetDateTime;
use tokio::sync::broadcast;

/// Endpoint + model configuration, overridable in Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeminiConfig {
    pub endpoint: String,
    pub transcribe_model: String,
    pub cleanup_model: String,
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://generativelanguage.googleapis.com".into(),
            // PRODUCT DECISION, not a tunable default: Jot ships on
            // gemini-3.5-transcribe. Do not swap it, and do not add automatic
            // substitution — no other model is this product. A user can still
            // pin something else in Settings → Advanced.
            transcribe_model: "gemini-3.5-transcribe".into(),
            cleanup_model: "gemini-3.5-flash-lite".into(),
        }
    }
}

/// How a dictation gets formatted. Two independent flags rather than a
/// three-valued enum, because all four combinations are meaningful — in
/// particular (native_smart: false, cleanup_pass: true) is the exact pipeline
/// Jot shipped before native smart existed, and that is the configuration you
/// want reachable if smart mode ever regresses server-side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormattingPolicy {
    pub native_smart: bool,
    pub cleanup_pass: bool,
}

impl FormattingPolicy {
    pub fn mode(self) -> TranscriptionMode {
        if self.native_smart {
            TranscriptionMode::Smart
        } else {
            TranscriptionMode::Verbatim
        }
    }

    /// The gate only has a real reference to compare against when a second model
    /// actually rewrote the text.
    pub fn runs_validation_gate(self) -> bool {
        self.cleanup_pass
    }
}

/// How the transcription model formats its output.
///
/// `Verbatim` produces output byte-identical to sending no transcription config
/// at all, so it is the server default and the field is omitted entirely for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptionMode {
    /// Exactly what was said, punctuated.
    Verbatim,
    /// The model removes fillers, collapses self-corrections ("at 1pm —
    /// actually, no, 2pm"), formats spoken lists and adds paragraph breaks.
    Smart,
}

impl TranscriptionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TranscriptionMode::Verbatim => "verbatim",
            TranscriptionMode::Smart => "smart",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// True once the user finished onboarding — a deliberate "I'll add it later"
    /// must not re-trap them in the wizard every launch.
    pub has_completed_onboarding: bool,
    pub endpoint_override: Option<String>,
    pub transcribe_model_override: Option<String>,
    pub cleanup_model_override: Option<String>,
    /// Double-tap the dictation key to lock hands-free. OFF by default: firm
    /// taps routinely exceed the hold threshold, misreading tap-tap as
    /// hold→finalize. The timing-free gesture is Space-while-holding.
    pub double_tap_lock: bool,
    /// Show the resting dot at the bottom of the screen when idle. Off = the
    /// pill only appears while dictating.
    pub show_idle_indicator: bool,
    pub sounds_enabled: bool,
    pub hotkey_key: HotkeyKey,
    /// The microphone to record from, by name. `None` follows the Windows
    /// default, including when the user changes it mid-recording.
    pub preferred_input_device: Option<String>,
    /// Native `mode: "smart"` — the default transcription path.
    pub smart_transcription: bool,
    /// The opt-in second pass through the cleanup model — this is what carries
    /// per-app tone. Off by default: it costs a round trip and sends the
    /// transcript text a second time.
    pub smart_cleanup_pass: bool,
    /// Escape hatch back to the pre-native-smart transport. Smart formatting is
    /// unavailable there (`mode` returns an empty transcript), so this
    /// necessarily means verbatim plus the optional tone pass.
    pub legacy_transcribe_endpoint: bool,
    /// Experimental: judge speech RELATIVE to the room instead of against fixed
    /// thresholds that assume a quiet one. Off by default until dogfood data
    /// earns the flip. The measurements it would act on are recorded either way.
    pub experimental_noise_handling: bool,
    /// Days to keep audio files (transcripts are kept until deleted). 0 = forever.
    pub audio_retention_days: i64,
    /// Auto-degrade bookkeeping: three gate trips in 24h ⇒ tone pass off.
    #[serde(with = "gate_trip_stamps")]
    pub gate_trips: Vec<OffsetDateTime>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            has_completed_onboarding: false,
            endpoint_override: None,
            transcribe_model_override: None,
            cleanup_model_override: None,
            double_tap_lock: false,
            show_idle_indicator: true,
            sounds_enabled: true,
            hotkey_key: HotkeyKey::default(),
            preferred_input_device: None,
            smart_transcription: true,
            smart_cleanup_pass: false,
            legacy_transcribe_endpoint: false,
            experimental_noise_handling: false,
            audio_retention_days: 7,
            gate_trips: Vec::new(),
        }
    }
}

/// The rfc3339 helper `time` ships only covers a bare `Option`, so the vector
/// of gate-trip timestamps gets its own thin adapter.
mod gate_trip_stamps {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    pub fn serialize<S: Serializer>(
        value: &[OffsetDateTime],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let encoded: Vec<String> = value
            .iter()
            .filter_map(|stamp| stamp.format(&Rfc3339).ok())
            .collect();
        encoded.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<OffsetDateTime>, D::Error> {
        let raw = Vec::<String>::deserialize(deserializer)?;
        Ok(raw
            .iter()
            .filter_map(|text| OffsetDateTime::parse(text, &Rfc3339).ok())
            .collect())
    }
}

impl Settings {
    /// Single source of truth for endpoint-override validity — the Settings UI
    /// warning and the effective config MUST use the same predicate, or one of
    /// them lies about which endpoint is in use.
    pub fn usable_endpoint(raw: Option<&str>) -> Option<String> {
        let raw = raw?.trim();
        if raw.is_empty() {
            return None;
        }
        let lowered = raw.to_ascii_lowercase();
        if !lowered.starts_with("http://") && !lowered.starts_with("https://") {
            return None;
        }
        Some(raw.trim_end_matches('/').to_string())
    }

    pub fn gemini_config(&self) -> GeminiConfig {
        let mut config = GeminiConfig::default();
        if let Some(endpoint) = Self::usable_endpoint(self.endpoint_override.as_deref()) {
            config.endpoint = endpoint;
        }
        if let Some(model) = self
            .transcribe_model_override
            .as_deref()
            .filter(|m| !m.is_empty())
        {
            config.transcribe_model = model.to_string();
        }
        if let Some(model) = self
            .cleanup_model_override
            .as_deref()
            .filter(|m| !m.is_empty())
        {
            config.cleanup_model = model.to_string();
        }
        config
    }

    pub fn formatting_policy(&self) -> FormattingPolicy {
        FormattingPolicy {
            native_smart: self.smart_transcription,
            cleanup_pass: self.smart_cleanup_pass,
        }
    }
}

/// Process-wide settings, persisted as JSON and broadcasting every write.
///
/// Runtime surfaces that render a setting (pill, status line, hotkey engine)
/// subscribe so toggles take effect the moment they are flipped — never "on the
/// next unrelated transition". `gate_trips` bookkeeping is exempt from the
/// broadcast: nothing renders it.
pub struct SettingsStore {
    path: PathBuf,
    data: RwLock<Settings>,
    changes: broadcast::Sender<&'static str>,
}

static GLOBAL: LazyLock<Arc<SettingsStore>> =
    LazyLock::new(|| Arc::new(SettingsStore::open(file_layout::settings_json())));

impl SettingsStore {
    pub fn global() -> Arc<SettingsStore> {
        GLOBAL.clone()
    }

    pub fn open(path: PathBuf) -> Self {
        let data = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let (changes, _) = broadcast::channel(64);
        Self {
            path,
            data: RwLock::new(data),
            changes,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<&'static str> {
        self.changes.subscribe()
    }

    pub fn get(&self) -> Settings {
        self.data.read().clone()
    }

    /// Mutate, persist, and announce. `key` names the setting for subscribers;
    /// pass `None` for bookkeeping that no surface renders.
    pub fn update(&self, key: Option<&'static str>, mutate: impl FnOnce(&mut Settings)) {
        let snapshot = {
            let mut data = self.data.write();
            mutate(&mut data);
            data.clone()
        };
        self.persist(&snapshot);
        if let Some(key) = key {
            let _ = self.changes.send(key);
        }
    }

    fn persist(&self, settings: &Settings) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(encoded) = serde_json::to_vec_pretty(settings) else {
            tracing::error!("SettingsStore: encode failed");
            return;
        };
        let temp = self.path.with_extension("json.tmp");
        if std::fs::write(&temp, &encoded).is_ok()
            && let Err(error) = std::fs::rename(&temp, &self.path)
        {
            tracing::error!(%error, "SettingsStore: rename failed");
            let _ = std::fs::remove_file(&temp);
        }
    }

    /// Records a validation-gate trip and returns how many happened in the last
    /// 24 hours. Three means cleanup can't be trusted right now.
    pub fn record_gate_trip(&self, now: OffsetDateTime) -> usize {
        let mut count = 0;
        self.update(None, |settings| {
            settings
                .gate_trips
                .retain(|stamp| (now - *stamp).whole_seconds() < 86_400);
            settings.gate_trips.push(now);
            count = settings.gate_trips.len();
        });
        count
    }

    /// A deliberate re-enable of the tone pass is a clean slate: without this,
    /// one stale trip inside the old 24h window instantly re-degrades it.
    pub fn set_smart_cleanup_pass(&self, enabled: bool) {
        self.update(Some("smartCleanupPass"), |settings| {
            if enabled {
                settings.gate_trips.clear();
            }
            settings.smart_cleanup_pass = enabled;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SettingsStore) {
        let temp = tempfile::tempdir().unwrap();
        let store = SettingsStore::open(temp.path().join("settings.json"));
        (temp, store)
    }

    #[test]
    fn defaults_match_the_shipped_product() {
        let settings = Settings::default();
        assert!(settings.smart_transcription);
        assert!(!settings.smart_cleanup_pass);
        assert!(!settings.double_tap_lock);
        assert!(settings.show_idle_indicator);
        assert_eq!(settings.audio_retention_days, 7);
        assert_eq!(settings.hotkey_key, HotkeyKey::RightControl);
    }

    #[test]
    fn the_microphone_defaults_to_whatever_windows_is_using() {
        assert_eq!(Settings::default().preferred_input_device, None);
    }

    #[test]
    fn writes_persist_and_reload() {
        let (temp, store) = store();
        store.update(Some("soundsEnabled"), |s| s.sounds_enabled = false);
        let reopened = SettingsStore::open(temp.path().join("settings.json"));
        assert!(!reopened.get().sounds_enabled);
    }

    #[test]
    fn subscribers_hear_the_changed_key() {
        let (_temp, store) = store();
        let mut rx = store.subscribe();
        store.update(Some("showIdleIndicator"), |s| s.show_idle_indicator = false);
        assert_eq!(rx.try_recv().unwrap(), "showIdleIndicator");
    }

    #[test]
    fn bookkeeping_writes_do_not_notify() {
        let (_temp, store) = store();
        let mut rx = store.subscribe();
        store.record_gate_trip(OffsetDateTime::now_utc());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn only_http_endpoints_override_the_default() {
        assert_eq!(Settings::usable_endpoint(None), None);
        assert_eq!(Settings::usable_endpoint(Some("  ")), None);
        assert_eq!(Settings::usable_endpoint(Some("ftp://x")), None);
        assert_eq!(
            Settings::usable_endpoint(Some("https://example.com/")).as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn gate_trips_expire_after_a_day() {
        let (_temp, store) = store();
        let now = OffsetDateTime::now_utc();
        store.update(None, |s| {
            s.gate_trips = vec![now - time::Duration::hours(25)];
        });
        assert_eq!(store.record_gate_trip(now), 1);
        assert_eq!(store.record_gate_trip(now), 2);
        assert_eq!(store.record_gate_trip(now), 3);
    }

    #[test]
    fn re_enabling_the_tone_pass_clears_stale_trips() {
        let (_temp, store) = store();
        let now = OffsetDateTime::now_utc();
        store.record_gate_trip(now);
        store.record_gate_trip(now);
        store.set_smart_cleanup_pass(true);
        assert!(store.get().gate_trips.is_empty());
        assert_eq!(store.record_gate_trip(now), 1);
    }

    #[test]
    fn model_overrides_apply_only_when_non_empty() {
        let mut settings = Settings::default();
        assert_eq!(
            settings.gemini_config().transcribe_model,
            "gemini-3.5-transcribe"
        );
        settings.transcribe_model_override = Some(String::new());
        assert_eq!(
            settings.gemini_config().transcribe_model,
            "gemini-3.5-transcribe"
        );
        settings.transcribe_model_override = Some("custom-model".into());
        assert_eq!(settings.gemini_config().transcribe_model, "custom-model");
    }
}
