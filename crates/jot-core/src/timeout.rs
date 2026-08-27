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

//! The single source of truth for every network deadline in the app.
//! No other file may define timeout constants.

use std::time::Duration;

/// TCP+TLS connect budget before the attempt is abandoned.
pub const CONNECT: Duration = Duration::from_secs(5);
/// Time to the first response byte after the request body is sent.
pub const TIME_TO_FIRST_BYTE: Duration = Duration::from_secs(10);
/// When the HUD flips to the "Still working…" slow state.
pub const SLOW_STATE_UI: Duration = Duration::from_secs(3);

/// Overall per-request deadline. Scales gently with audio length:
/// 5s clip → 31s; 10min clip → 2.5min. Never the unbounded 2×duration formula.
pub fn overall_deadline(audio_duration: Duration) -> Duration {
    Duration::from_secs_f64(30.0 + audio_duration.as_secs_f64() / 4.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_scales_gently_with_audio_length() {
        assert_eq!(
            overall_deadline(Duration::from_secs(5)).as_secs_f64(),
            31.25
        );
        assert_eq!(
            overall_deadline(Duration::from_secs(600)).as_secs_f64(),
            180.0
        );
    }

    #[test]
    fn a_zero_length_clip_still_gets_the_base_budget() {
        assert_eq!(overall_deadline(Duration::ZERO).as_secs(), 30);
    }
}
