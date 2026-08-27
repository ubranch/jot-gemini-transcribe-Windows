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

//! The one definition of Jot's 0…1 mic level, its inverse, and the room-noise
//! estimator built on top of it.

/// The level curve.
///
/// `level = min(1, (min(rms * 11, 1)) ^ 0.65)` — a compressive curve so quiet
/// speech lands mid-range instead of hugging the floor and loud speech
/// saturates gracefully. It lives here, alone and tested, because every "did
/// they speak?" decision in the pipeline is arithmetic on its output:
/// `SILENCE_PEAK_THRESHOLD` discards whole recordings,
/// `TRAILING_SPEECH_THRESHOLD` decides whether the last word is captured. A
/// wrong constant here loses words silently.
///
/// The curve SATURATES at `rms = 1/gain` (≈ −20.8 dBFS): every level of 1.0
/// maps back to that same RMS. Any SNR computed from these numbers is therefore
/// a **lower bound** — which is the safe direction, because it biases us toward
/// "this room is noisy", which biases us toward keeping audio.
pub mod curve {
    pub const GAIN: f32 = 11.0;
    pub const EXPONENT: f32 = 0.65;

    /// The bottom of the scale — quieter than any real microphone, used so a
    /// digital-silence buffer has a finite dB value instead of −∞.
    pub const FLOOR_DBFS: f64 = -120.0;

    /// RMS (0…1 linear) → Jot level (0…1).
    pub fn level_from_rms(rms: f32) -> f32 {
        if rms <= 0.0 {
            return 0.0;
        }
        (rms * GAIN).min(1.0).powf(EXPONENT).min(1.0)
    }

    /// Jot level → RMS. Exact inverse below saturation; at level 1.0 it returns
    /// the saturation RMS, which is a floor on the true value, not the value.
    pub fn rms_from_level(level: f32) -> f32 {
        if level <= 0.0 {
            return 0.0;
        }
        level.min(1.0).powf(1.0 / EXPONENT) / GAIN
    }

    /// Jot level → dBFS. This is the space the noise-floor estimator works in:
    /// dB is where "6 dB above the room" is a meaningful sentence and
    /// "0.02 above the room" is not.
    pub fn dbfs_from_level(level: f32) -> f64 {
        let linear = rms_from_level(level);
        if linear <= 0.0 {
            return FLOOR_DBFS;
        }
        (20.0 * (linear as f64).log10()).max(FLOOR_DBFS)
    }

    /// Where the curve stops distinguishing louder from loudest (≈ −20.8 dBFS).
    pub fn saturation_dbfs() -> f64 {
        20.0 * (1.0 / GAIN as f64).log10()
    }
}

/// Measures how loud the room is, so "did they speak?" can be asked relative to
/// the room instead of against a constant that assumes a quiet one.
///
/// Jot's absolute thresholds are `0.06` ≈ −58 dBFS and `0.08` ≈ −55 dBFS. Any
/// occupied room clears both, which is why noise doesn't merely degrade
/// accuracy — it makes the trailing-capture loop run to its full cap on every
/// dictation and turns "nobody spoke" into a hard failure.
///
/// **This type always runs, and it decides nothing.** It observes the level
/// stream and records numbers into `SessionMeta`; whether anything acts on them
/// is the caller's business, gated behind `experimental_noise_handling`.
#[derive(Debug, Clone)]
pub struct NoiseFloorEstimator {
    capacity: usize,
    samples: Vec<f64>,
    peak_db: f64,
    sample_count: usize,
}

impl Default for NoiseFloorEstimator {
    fn default() -> Self {
        // ~60s at the real ~10 Hz tap rate. The recording cap is 10 minutes, and
        // a floor from nine minutes ago is not this room any more.
        Self::with_capacity(600)
    }
}

impl NoiseFloorEstimator {
    /// Below this many samples the percentile is meaningless — with tap buffers
    /// arriving at ~10 Hz this is ~0.8s of audio.
    pub const MINIMUM_SAMPLES: usize = 8;
    /// The floor is the quietest tenth of the session, not the minimum: a single
    /// anomalous buffer shouldn't define the room.
    pub const PERCENTILE: f64 = 0.10;

    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(Self::MINIMUM_SAMPLES);
        Self {
            capacity,
            samples: Vec::with_capacity(capacity),
            peak_db: curve::FLOOR_DBFS,
            sample_count: 0,
        }
    }

    pub fn ingest(&mut self, level: f32) {
        let db = curve::dbfs_from_level(level);
        self.peak_db = self.peak_db.max(db);
        self.sample_count += 1;
        if self.samples.len() == self.capacity {
            self.samples.remove(0);
        }
        self.samples.push(db);
    }

    pub fn peak_db(&self) -> f64 {
        self.peak_db
    }

    pub fn sample_count(&self) -> usize {
        self.sample_count
    }

    /// The room, in dBFS — the 10th percentile of everything heard so far.
    ///
    /// A rolling low percentile rather than "the first N milliseconds": prewarm
    /// means audio starts the instant the key goes down, which is exactly when a
    /// fast user is *already speaking*, so treating the head as noise would
    /// classify speech as the floor and make every downstream decision worse.
    /// Speech is intermittent — there are gaps between words and phrases — so
    /// the percentile converges on the floor within a second or two even when
    /// speech starts at sample 0, and it tracks a floor that rises mid-session.
    pub fn floor_db(&self) -> Option<f64> {
        if self.samples.len() < Self::MINIMUM_SAMPLES {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("levels are never NaN"));
        let index = (((sorted.len() - 1) as f64) * Self::PERCENTILE).round() as usize;
        Some(sorted[index])
    }

    /// Peak minus floor. A **lower bound** on the true SNR: the level curve
    /// saturates at −20.8 dBFS, so loud speech understates its own peak. Biasing
    /// low means we conclude "noisy" more readily than "clean", and every
    /// consequence of "noisy" in this codebase keeps audio rather than dropping it.
    pub fn measured_snr(&self) -> Option<f64> {
        self.floor_db().map(|floor| self.peak_db - floor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_round_trips_below_saturation() {
        for rms in [0.001_f32, 0.01, 0.03, 0.05, 0.08] {
            let level = curve::level_from_rms(rms);
            assert!(
                (curve::rms_from_level(level) - rms).abs() < 1e-5,
                "rms {rms}"
            );
        }
    }

    #[test]
    fn curve_saturates_at_gain_reciprocal() {
        assert_eq!(curve::level_from_rms(1.0 / curve::GAIN), 1.0);
        assert_eq!(curve::level_from_rms(0.9), 1.0);
        assert_eq!(curve::level_from_rms(0.0), 0.0);
    }

    #[test]
    fn silence_thresholds_land_where_the_docs_claim() {
        // 0.06 ≈ −58 dBFS, 0.08 ≈ −55 dBFS: the comments in the coordinator are
        // arithmetic on this curve, so pin them.
        assert!((curve::dbfs_from_level(0.06) - -58.0).abs() < 1.0);
        assert!((curve::dbfs_from_level(0.08) - -55.0).abs() < 1.0);
    }

    #[test]
    fn floor_needs_a_minimum_sample_count() {
        let mut estimator = NoiseFloorEstimator::default();
        for _ in 0..(NoiseFloorEstimator::MINIMUM_SAMPLES - 1) {
            estimator.ingest(0.02);
        }
        assert!(estimator.floor_db().is_none());
        estimator.ingest(0.02);
        assert!(estimator.floor_db().is_some());
    }

    #[test]
    fn floor_tracks_the_quiet_tenth_not_the_peak() {
        let mut estimator = NoiseFloorEstimator::default();
        for _ in 0..90 {
            estimator.ingest(0.02); // room
        }
        for _ in 0..10 {
            estimator.ingest(0.5); // speech
        }
        let floor = estimator.floor_db().unwrap();
        let peak = estimator.peak_db();
        assert!((floor - curve::dbfs_from_level(0.02)).abs() < 0.001);
        assert!(peak > floor);
        assert!(estimator.measured_snr().unwrap() > 12.0);
    }

    #[test]
    fn capacity_is_a_rolling_window() {
        let mut estimator = NoiseFloorEstimator::with_capacity(16);
        for _ in 0..16 {
            estimator.ingest(0.6);
        }
        for _ in 0..16 {
            estimator.ingest(0.01);
        }
        // The loud half has rolled out of the window entirely.
        assert!((estimator.floor_db().unwrap() - curve::dbfs_from_level(0.01)).abs() < 0.001);
        // …but the peak is a session-lifetime high-water mark, not windowed.
        assert!(estimator.peak_db() > curve::dbfs_from_level(0.5));
    }
}
