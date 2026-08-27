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

//! Low-level Gemini API client.
//!
//! Uses non-streaming `generateContent`: the transcribe model delivers its
//! entire result in one lump anyway, so streaming buys nothing but parsing
//! complexity today.

use crate::settings::TranscriptionMode;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptionError {
    Offline,
    Network(String),
    /// Permanent request failure (400) — retrying is pointless.
    BadRequest(String),
    /// 401 — the key itself was rejected.
    Auth,
    /// 403/404 — the key is fine but this model is gated, renamed, or unknown.
    /// Distinct from `Auth`: "fix your key" is the WRONG advice here.
    ModelUnavailable {
        model: String,
        detail: Option<String>,
    },
    /// 429 that is a real daily/hard quota.
    RateLimitedDaily,
    /// 429 per-minute throttle — clears on its own; retryable.
    RateLimitedTransient,
    Timeout,
    EmptyTranscript,
    SafetyBlocked,
}

impl std::fmt::Display for TranscriptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranscriptionError::Offline => write!(f, "offline"),
            TranscriptionError::Network(detail) => write!(f, "network: {detail}"),
            TranscriptionError::BadRequest(detail) => write!(f, "bad request: {detail}"),
            TranscriptionError::Auth => write!(f, "API key rejected"),
            TranscriptionError::ModelUnavailable { model, .. } => {
                write!(f, "model {model} not accessible")
            }
            TranscriptionError::RateLimitedDaily => write!(f, "daily quota exhausted"),
            TranscriptionError::RateLimitedTransient => write!(f, "rate limited"),
            TranscriptionError::Timeout => write!(f, "timed out"),
            TranscriptionError::EmptyTranscript => write!(f, "empty transcript"),
            TranscriptionError::SafetyBlocked => write!(f, "blocked by safety filters"),
        }
    }
}

impl std::error::Error for TranscriptionError {}

/// Why a key check failed, because "false" is not enough to act on.
///
/// Onboarding must let someone past a check it could not perform (a captive
/// portal, a VPN coming up) while HARD-BLOCKING a key the server actively
/// rejected. Collapsing both into `false` is what lets a bad key through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCheck {
    Valid,
    /// The server answered, and the answer was no. Never advance on this.
    Rejected(Option<String>),
    /// We never got an answer. Not the key's fault — let them continue.
    Unreachable,
}

type KeyProvider = Box<dyn Fn() -> Option<String> + Send + Sync>;

pub struct GeminiClient {
    http: reqwest::Client,
    api_key: KeyProvider,
}

impl GeminiClient {
    pub fn new(api_key: KeyProvider) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(crate::timeout::CONNECT)
            // Fail fast into the retry/queue path rather than parking a request
            // while the machine is offline.
            .tcp_keepalive(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self { http, api_key }
    }

    /// Audio-only request on the legacy `:generateContent` transport.
    ///
    /// The transcribe models ignore prompts, and `audioTranscriptionConfig.
    /// wordTimestamp` MUST be true or the transcript comes back empty. `mode` is
    /// NOT available here — it parses but returns an empty text part, and the
    /// two are mutually exclusive (400 together). `customVocabulary` does work,
    /// so the Dictionary keeps biasing the recogniser even on this transport.
    pub async fn transcribe(
        &self,
        audio: &[u8],
        mime_type: &str,
        model: &str,
        endpoint: &str,
        deadline: Duration,
        custom_vocabulary: &[String],
    ) -> Result<String, TranscriptionError> {
        let mut audio_config = json!({ "wordTimestamp": true, "diarization": false });
        if !custom_vocabulary.is_empty() {
            audio_config["customVocabulary"] = json!(custom_vocabulary);
        }
        let body = json!({
            "contents": [{
                "role": "user",
                "parts": [{
                    "inline_data": { "mime_type": mime_type, "data": BASE64.encode(audio) }
                }],
            }],
            "generationConfig": {
                "temperature": 0,
                "audioTranscriptionConfig": audio_config,
            },
        });
        let data = self
            .post(
                &format!("v1beta/models/{model}:generateContent"),
                body,
                endpoint,
                deadline,
                model,
                true,
                false,
            )
            .await?;
        extract_text(&data)
    }

    /// Audio transcription via `POST {endpoint}/v1beta/interactions`.
    ///
    /// A DIFFERENT surface from `:generateContent`, with a different response
    /// envelope (`steps/content/text`, not `candidates/content/parts`) and the
    /// model named in the BODY rather than the URL path. It is the only place
    /// `mode: "smart"` works.
    // Every argument here is a distinct axis of the request the endpoint takes;
    // bundling them into a struct would only move the same list one level away.
    #[allow(clippy::too_many_arguments)]
    pub async fn transcribe_interaction(
        &self,
        audio: &[u8],
        mime_type: &str,
        model: &str,
        endpoint: &str,
        mode: TranscriptionMode,
        custom_vocabulary: &[String],
        deadline: Duration,
    ) -> Result<String, TranscriptionError> {
        let mut body = json!({
            "model": model,
            "input": [{ "type": "audio", "mime_type": mime_type, "data": BASE64.encode(audio) }],
        });
        if let Some(config) = transcription_config(mode, custom_vocabulary) {
            body["generation_config"] = json!({ "transcription_config": config });
        }
        let data = self
            .post(
                "v1beta/interactions",
                body,
                endpoint,
                deadline,
                model,
                false,
                false,
            )
            .await?;
        extract_interaction_text(&data)
    }

    /// Text-only cleanup call (flash-lite class, thinking minimized).
    ///
    /// The thinking knob differs by model generation:
    ///  - `gemini-2.x`: `thinkingConfig.thinkingBudget: 0`
    ///  - `gemini-3.x+`: `thinkingConfig.thinkingLevel: "low"`
    pub async fn cleanup(
        &self,
        prompt: &str,
        model: &str,
        endpoint: &str,
        deadline: Duration,
    ) -> Result<String, TranscriptionError> {
        let thinking = if model.starts_with("gemini-2") {
            json!({ "thinkingBudget": 0 })
        } else {
            json!({ "thinkingLevel": "low" })
        };
        let body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": prompt }] }],
            "generationConfig": { "temperature": 0, "thinkingConfig": thinking },
        });
        let data = self
            .post(
                &format!("v1beta/models/{model}:generateContent"),
                body,
                endpoint,
                deadline,
                model,
                true,
                false,
            )
            .await?;
        extract_text(&data)
    }

    /// Cheap key validation for onboarding/Settings.
    pub async fn validate_key(&self, endpoint: &str) -> KeyCheck {
        let url = format!("{endpoint}/v1beta/models?pageSize=1");
        let Ok(response) = self
            .authorized(self.http.get(&url))
            .timeout(Duration::from_secs(10))
            .send()
            .await
        else {
            return KeyCheck::Unreachable;
        };
        let status = response.status().as_u16();
        let body = response.bytes().await.unwrap_or_default();
        match status {
            200 => KeyCheck::Valid,
            // A bad key returns 400 with API_KEY_INVALID on this API, not 401.
            // Treating only 401 as rejection would call it unreachable.
            400 | 401 | 403 => KeyCheck::Rejected(error_message(&body)),
            500..=599 => KeyCheck::Unreachable,
            _ => KeyCheck::Rejected(error_message(&body)),
        }
    }

    /// First model in `candidates` this key can actually reach, or `None`.
    /// Onboarding runs this so "your key works" means the whole pipeline works,
    /// not just that the key authenticates.
    pub async fn resolve_available_model(
        &self,
        candidates: &[String],
        endpoint: &str,
    ) -> Option<String> {
        for model in candidates {
            let url = format!("{endpoint}/v1beta/models/{model}");
            let Ok(response) = self
                .authorized(self.http.get(&url))
                .timeout(Duration::from_secs(8))
                .send()
                .await
            else {
                continue;
            };
            if response.status().is_success() {
                return Some(model.clone());
            }
        }
        None
    }

    /// Whether a key is stored at all.
    ///
    /// Without this the first dictation of a fresh install spends two round
    /// trips to be told 403 by an endpoint that names neither the key nor the
    /// model, and the user is shown "couldn't reach Gemini" for a problem that
    /// is entirely local.
    pub fn has_api_key(&self) -> bool {
        (self.api_key)().is_some_and(|key| !key.trim().is_empty())
    }

    /// Header, never `?key=` — query strings leak into logs and proxies.
    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match (self.api_key)() {
            Some(key) => request.header("x-goog-api-key", key),
            None => request,
        }
    }

    /// Shared transport for EVERY Gemini call: auth, the true wall-clock
    /// deadline, and the HTTP status → `TranscriptionError` mapping.
    ///
    /// That mapping must never fork per-endpoint — the retry queue branches on
    /// `RateLimitedDaily` vs `RateLimitedTransient` to decide between "keep this
    /// row queued forever" and "mark it failed", so two copies would drift into
    /// a data-loss bug.
    #[allow(clippy::too_many_arguments)]
    async fn post(
        &self,
        path: &str,
        body: Value,
        endpoint: &str,
        deadline: Duration,
        model_label: &str,
        // False when the model is named in the BODY rather than the URL path
        // (interactions). A 403/404 there is about the ENDPOINT, not the model,
        // so reporting "your key can't use this model" would send the user to
        // the wrong fix.
        model_is_in_path: bool,
        is_retry_after_429: bool,
    ) -> Result<Vec<u8>, TranscriptionError> {
        let url = format!("{endpoint}/{path}");
        let request = self
            .authorized(self.http.post(&url))
            .header("Content-Type", "application/json")
            .json(&body);

        // reqwest's own timeout is generous about idle sockets; the true overall
        // deadline is enforced here so a stalled response can never outlive it.
        let response = match tokio::time::timeout(deadline, request.send()).await {
            Err(_) => return Err(TranscriptionError::Timeout),
            Ok(Err(error)) if error.is_timeout() => return Err(TranscriptionError::Timeout),
            Ok(Err(error)) if error.is_connect() => return Err(TranscriptionError::Offline),
            Ok(Err(error)) => return Err(TranscriptionError::Network(error.to_string())),
            Ok(Ok(response)) => response,
        };

        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("Retry-After")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<f64>().ok());
        let data = match response.bytes().await {
            Ok(data) => data.to_vec(),
            Err(error) => return Err(TranscriptionError::Network(error.to_string())),
        };

        match status {
            200 => Ok(data),
            401 => Err(TranscriptionError::Auth),
            403 | 404 => {
                // Key authenticated but this model is not available to it —
                // gated, renamed, or unknown. "Fix your key" would misdirect;
                // name the model instead.
                let detail = error_message(&data);
                tracing::error!(status, path, model_label, "Gemini rejected the request");
                if model_is_in_path {
                    Err(TranscriptionError::ModelUnavailable {
                        model: model_label.to_string(),
                        detail,
                    })
                } else {
                    // The transcription endpoint itself is unreachable for this
                    // key. Retryable, and the copy points at the Advanced escape
                    // hatch rather than at a model the key demonstrably reaches.
                    Err(TranscriptionError::Network(format!(
                        "interactions_unavailable_{status}"
                    )))
                }
            }
            429 => {
                // Per-minute throttles carry a short retryDelay — honor it once.
                if !is_retry_after_429
                    && let Some(delay) =
                        retry_delay_seconds(&data, retry_after).filter(|d| *d <= 8.0)
                {
                    tracing::info!(delay, "429 with retryDelay — waiting once");
                    tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                    return Box::pin(self.post(
                        path,
                        body,
                        endpoint,
                        deadline,
                        model_label,
                        model_is_in_path,
                        true,
                    ))
                    .await;
                }
                // Only a real daily/hard quota is terminal; a per-minute throttle
                // (or an unparseable body) clears on its own and stays retryable.
                if is_daily_quota(&data) {
                    Err(TranscriptionError::RateLimitedDaily)
                } else {
                    Err(TranscriptionError::RateLimitedTransient)
                }
            }
            // Permanent: malformed request — retrying is pointless.
            400 => Err(TranscriptionError::BadRequest(
                error_message(&data).unwrap_or_else(|| "http_400".into()),
            )),
            500..=599 => Err(TranscriptionError::Network(format!("http_{status}"))),
            _ => Err(TranscriptionError::Network(
                error_message(&data).unwrap_or_else(|| format!("http_{status}")),
            )),
        }
    }
}

/// The ONLY place a transcription config is constructed.
///
/// NEVER add `language_codes` here: sending
/// `{"mode":"smart","language_codes":["en-US"]}` returns VERBATIM output with
/// HTTP 200 and no error — smart mode is silently disabled and there is no
/// runtime signal whatsoever. `custom_vocabulary` is safe to combine.
pub fn transcription_config(
    mode: TranscriptionMode,
    custom_vocabulary: &[String],
) -> Option<Value> {
    match mode {
        // Server default. Omitting the field is byte-identical to sending it.
        TranscriptionMode::Verbatim => (!custom_vocabulary.is_empty())
            .then(|| json!({ "custom_vocabulary": custom_vocabulary })),
        TranscriptionMode::Smart => {
            let mut config = json!({ "mode": "smart" });
            if !custom_vocabulary.is_empty() {
                config["custom_vocabulary"] = json!(custom_vocabulary);
            }
            Some(config)
        }
    }
}

pub fn extract_text(data: &[u8]) -> Result<String, TranscriptionError> {
    let Ok(json) = serde_json::from_slice::<Value>(data) else {
        return Err(TranscriptionError::Network("unparseable_response".into()));
    };
    if json
        .get("promptFeedback")
        .and_then(|feedback| feedback.get("blockReason"))
        .is_some()
    {
        return Err(TranscriptionError::SafetyBlocked);
    }
    let Some(first) = json
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
    else {
        return Err(TranscriptionError::Network("no_candidates".into()));
    };
    if first.get("finishReason").and_then(Value::as_str) == Some("SAFETY") {
        return Err(TranscriptionError::SafetyBlocked);
    }
    Ok(first
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default())
}

/// interactions envelope:
/// `{"id","status","steps":[{"type","content":[{"type","text"}]}],"usage"}`
///
/// Empty text is returned as `""` rather than an error, exactly as
/// `extract_text` does: the service owns the empty-transcript retry and the
/// coordinator classifies silence by audio energy. Failing here would route a
/// quiet dictation into the failure path instead.
pub fn extract_interaction_text(data: &[u8]) -> Result<String, TranscriptionError> {
    let Ok(json) = serde_json::from_slice::<Value>(data) else {
        return Err(TranscriptionError::Network("unparseable_response".into()));
    };
    let status = json
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("missing");
    if status != "completed" {
        return Err(map_interaction_status(status, &json));
    }
    let Some(steps) = json.get("steps").and_then(Value::as_array) else {
        return Err(TranscriptionError::Network("no_steps".into()));
    };
    Ok(steps
        .iter()
        .filter(|step| step.get("type").and_then(Value::as_str) == Some("model_output"))
        .filter_map(|step| step.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|content| {
            (content.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| content.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect())
}

/// HTTP 200 with a non-"completed" status. Unknown statuses map to `Network`
/// (RETRYABLE) rather than `BadRequest` (terminal) on purpose: under
/// never-lose-words the safe direction on an unrecognised string is keeping the
/// row queued and recoverable, not marking it permanently failed.
pub fn map_interaction_status(status: &str, json: &Value) -> TranscriptionError {
    let detail = json
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if status == "failed" || status == "error" {
        let lowered = detail.to_lowercase();
        if lowered.contains("safety") || lowered.contains("blocked") {
            return TranscriptionError::SafetyBlocked;
        }
        tracing::error!("interactions failed");
        return TranscriptionError::Network("interaction_failed".into());
    }
    tracing::error!(status, "interactions returned an unexpected status");
    TranscriptionError::Network(format!("interaction_status_{status}"))
}

static RETRY_DELAY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""retryDelay"\s*:\s*"([0-9.]+)s""#).expect("retry delay is a valid regex")
});
static PER_DAY_QUOTA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""quotaId"\s*:\s*"[^"]*PerDay[^"]*""#).expect("quota id is a valid regex")
});

/// Extracts a short retry hint from a 429: the `Retry-After` header or the
/// `google.rpc.RetryInfo` `"retryDelay": "2s"` detail in the error body.
pub fn retry_delay_seconds(data: &[u8], retry_after_header: Option<f64>) -> Option<f64> {
    if let Some(seconds) = retry_after_header {
        return Some(seconds);
    }
    let body = std::str::from_utf8(data).ok()?;
    RETRY_DELAY
        .captures(body)
        .and_then(|captures| captures.get(1))
        .and_then(|group| group.as_str().parse().ok())
}

fn is_daily_quota(data: &[u8]) -> bool {
    std::str::from_utf8(data).is_ok_and(|body| PER_DAY_QUOTA.is_match(body))
}

/// The two endpoints do NOT share an error envelope:
/// ```text
///   :generateContent  bad key   -> {"error":{...}}
///   /v1beta/interactions        -> [{"error":{...}}]   (array-wrapped)
///   :generateContent  bad model -> {"code":404,"status":"NOT_FOUND"}
///   /v1beta/interactions        -> {"code":"not_found"} (string code, no status)
/// ```
/// Parsing only the object form loses the message on the array form, and the
/// user gets a bare "http_400" instead of "API key not valid".
pub fn error_message(data: &[u8]) -> Option<String> {
    let root: Value = serde_json::from_slice(data).ok()?;
    let object = match &root {
        Value::Object(_) => Some(&root),
        Value::Array(items) => items.first(),
        _ => None,
    }?;
    object
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_content_text_is_joined_across_parts() {
        let body =
            br#"{"candidates":[{"content":{"parts":[{"text":"hello "},{"text":"there"}]}}]}"#;
        assert_eq!(extract_text(body).unwrap(), "hello there");
    }

    #[test]
    fn an_empty_transcript_is_text_not_an_error() {
        // The coordinator classifies silence by audio energy; failing here would
        // route a quiet dictation into the failure path.
        let body = br#"{"candidates":[{"content":{"parts":[]}}]}"#;
        assert_eq!(extract_text(body).unwrap(), "");
    }

    #[test]
    fn safety_blocks_are_recognised_on_both_shapes() {
        let feedback = br#"{"promptFeedback":{"blockReason":"SAFETY"}}"#;
        assert_eq!(
            extract_text(feedback),
            Err(TranscriptionError::SafetyBlocked)
        );
        let finish = br#"{"candidates":[{"finishReason":"SAFETY"}]}"#;
        assert_eq!(extract_text(finish), Err(TranscriptionError::SafetyBlocked));
    }

    #[test]
    fn unparseable_responses_stay_retryable() {
        assert_eq!(
            extract_text(b"<html>502</html>"),
            Err(TranscriptionError::Network("unparseable_response".into()))
        );
    }

    #[test]
    fn interaction_text_reads_only_model_output_steps() {
        let body = br#"{"status":"completed","steps":[
            {"type":"thought","content":[{"type":"text","text":"ignored"}]},
            {"type":"model_output","content":[{"type":"text","text":"kept"}]}]}"#;
        assert_eq!(extract_interaction_text(body).unwrap(), "kept");
    }

    #[test]
    fn unknown_interaction_statuses_stay_retryable() {
        let body = br#"{"status":"queued"}"#;
        assert_eq!(
            extract_interaction_text(body),
            Err(TranscriptionError::Network(
                "interaction_status_queued".into()
            ))
        );
    }

    #[test]
    fn a_safety_worded_interaction_failure_maps_to_safety_blocked() {
        let json: Value =
            serde_json::from_str(r#"{"error":{"message":"Blocked by safety filters"}}"#).unwrap();
        assert_eq!(
            map_interaction_status("failed", &json),
            TranscriptionError::SafetyBlocked
        );
    }

    #[test]
    fn verbatim_omits_the_config_entirely_unless_vocabulary_is_present() {
        assert_eq!(transcription_config(TranscriptionMode::Verbatim, &[]), None);
        let with_terms =
            transcription_config(TranscriptionMode::Verbatim, &["gRPC".into()]).unwrap();
        assert_eq!(with_terms, json!({ "custom_vocabulary": ["gRPC"] }));
        assert!(with_terms.get("mode").is_none());
    }

    #[test]
    fn smart_mode_never_carries_language_codes() {
        // Pinning this: `language_codes` silently disables smart mode with a 200.
        let config = transcription_config(TranscriptionMode::Smart, &["gRPC".into()]).unwrap();
        assert_eq!(config["mode"], "smart");
        assert!(config.get("language_codes").is_none());
    }

    #[test]
    fn retry_delay_prefers_the_header_then_falls_back_to_the_body() {
        assert_eq!(retry_delay_seconds(b"{}", Some(2.0)), Some(2.0));
        assert_eq!(
            retry_delay_seconds(br#"{"error":{"details":[{"retryDelay":"3.5s"}]}}"#, None),
            Some(3.5)
        );
        assert_eq!(retry_delay_seconds(b"{}", None), None);
    }

    #[test]
    fn only_a_per_day_quota_id_reads_as_terminal() {
        assert!(is_daily_quota(
            br#"{"error":{"details":[{"quotaId":"GenerateRequestsPerDay"}]}}"#
        ));
        assert!(!is_daily_quota(
            br#"{"error":{"details":[{"quotaId":"GenerateRequestsPerMinute"}]}}"#
        ));
        assert!(!is_daily_quota(b"not json at all"));
    }

    #[test]
    fn error_messages_survive_the_array_wrapped_envelope() {
        assert_eq!(
            error_message(br#"{"error":{"message":"API key not valid"}}"#).as_deref(),
            Some("API key not valid")
        );
        assert_eq!(
            error_message(br#"[{"error":{"message":"API key not valid"}}]"#).as_deref(),
            Some("API key not valid")
        );
        assert_eq!(error_message(br#"{"code":"not_found"}"#), None);
    }
}
