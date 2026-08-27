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

//! Crash-safe WASAPI capture.
//!
//! Audio goes to disk from the first millisecond, so a crash, a kill, or a flat
//! battery costs the user nothing. The WAV header is rewritten once a second,
//! which is what makes a half-written file recoverable rather than a
//! zero-length lie.

use crate::levels::curve;
use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

/// The wire format: 16 kHz mono 16-bit PCM. Speech models want nothing more,
/// and it keeps the upload — which sits on the latency path — three times
/// smaller than a typical 48 kHz device rate.
pub const SAMPLE_RATE: u32 = 16_000;

/// How long `stop` keeps draining after the key comes up. The device delivers
/// whole buffers, so tearing the stream down immediately throws away the tail
/// of the last word.
const DRAIN_AFTER_STOP: Duration = Duration::from_millis(80);
/// How often the WAV header is rewritten, and how often the default input
/// device is re-checked.
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(1);
/// Consecutive failed writes before the session is declared unwritable.
const WRITE_FAILURE_LIMIT: u32 = 5;
/// Chunks the engine thread may fall behind by before audio is dropped. At
/// ~10 ms per chunk this is two seconds of slack — far past any plausible disk
/// hiccup, and bounded so a stalled disk cannot exhaust memory.
const CHUNK_QUEUE_DEPTH: usize = 200;

/// Result of a completed (or stopped) capture.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioCaptureResult {
    pub frames_written: u64,
    pub duration_seconds: f64,
    /// Seconds-from-start positions where a device change may have left a seam.
    pub gap_markers: Vec<f64>,
    /// Peak metered level (same 0…1 scale as the level callback). Distinguishes
    /// "held the key in silence" from "spoke but transcription came back empty".
    pub peak_level: f32,
    /// The same peak, computed from the bytes actually written to disk rather
    /// than from the metering path.
    pub written_peak_level: f32,
    /// False when NOTHING measured this session's loudness. Callers must then
    /// assume speech and upload: a wasted round-trip costs a fraction of a cent,
    /// a discarded session costs the user's words.
    pub peak_is_trustworthy: bool,
}

impl Default for AudioCaptureResult {
    fn default() -> Self {
        Self {
            frames_written: 0,
            duration_seconds: 0.0,
            gap_markers: Vec::new(),
            peak_level: 1.0,
            written_peak_level: 1.0,
            peak_is_trustworthy: true,
        }
    }
}

impl AudioCaptureResult {
    pub fn empty() -> Self {
        Self {
            frames_written: 0,
            duration_seconds: 0.0,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// No input device exists at all. "Mic didn't start" is the wrong advice
    /// on a machine with no microphone attached.
    NoInputDevice,
    Config(String),
    Io(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::NoInputDevice => write!(f, "no input device"),
            CaptureError::Config(detail) => write!(f, "device config: {detail}"),
            CaptureError::Io(detail) => write!(f, "recording file: {detail}"),
        }
    }
}

impl std::error::Error for CaptureError {}

type LevelFn = Box<dyn Fn(f32) + Send + Sync>;
type MessageFn = Box<dyn Fn(String) + Send + Sync>;
type NotifyFn = Box<dyn Fn() + Send + Sync>;

/// Callbacks are swappable while a capture is running: the coordinator
/// reassigns the level sink at key-up so it can hear whether the user is still
/// talking without also driving the waveform.
#[derive(Default)]
pub struct CaptureCallbacks {
    /// ~30 Hz RMS level in 0…1.
    pub on_level: Option<LevelFn>,
    /// Fired when the input device changed mid-recording (informational).
    pub on_device_change: Option<MessageFn>,
    /// Fired once when disk writes fail persistently — captured audio up to that
    /// point is preserved; the coordinator should finalize early.
    pub on_write_failure: Option<NotifyFn>,
    /// Fired once when the engine dies mid-recording and cannot be revived.
    /// Captured audio up to the seam is preserved; the coordinator should
    /// finalize with what exists — a pill that keeps "listening" while nothing
    /// records is the worst kind of word loss.
    pub on_engine_died: Option<MessageFn>,
}

/// Seam for the coordinator so it can be tested headless with a fake recorder.
pub trait AudioCapturing: Send {
    fn callbacks(&self) -> Arc<Mutex<CaptureCallbacks>>;
    /// Starts the engine and begins writing WAV to `path` immediately.
    fn start(&mut self, path: &Path) -> Result<(), CaptureError>;
    /// Stops and finalizes the file, first draining the in-flight buffer so the
    /// tail of the last word is not discarded. Safe to call more than once.
    fn stop(&mut self) -> AudioCaptureResult;
}

// ---------------------------------------------------------------------------
// Resampling
// ---------------------------------------------------------------------------

/// Box-filter decimator from the device rate to `SAMPLE_RATE`.
///
/// Every output sample is the mean of the input samples that fall inside its
/// window, so the common 48 kHz → 16 kHz case is an exact 3-tap average rather
/// than a naive "keep every third sample", which aliases sibilance down into
/// the speech band. Fractional ratios (44.1 kHz) carry their remainder forward.
#[derive(Debug)]
pub struct Resampler {
    ratio: f64,
    accumulator: f64,
    sum: f64,
    count: u32,
}

impl Resampler {
    pub fn new(input_rate: u32) -> Self {
        Self {
            ratio: input_rate.max(1) as f64 / SAMPLE_RATE as f64,
            accumulator: 0.0,
            sum: 0.0,
            count: 0,
        }
    }

    pub fn push(&mut self, sample: f32, out: &mut Vec<f32>) {
        self.sum += sample as f64;
        self.count += 1;
        self.accumulator += 1.0;
        while self.accumulator >= self.ratio {
            self.accumulator -= self.ratio;
            if self.count > 0 {
                out.push((self.sum / self.count as f64) as f32);
            }
            self.sum = 0.0;
            self.count = 0;
        }
    }
}

fn rms_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    curve::level_from_rms((sum / samples.len() as f64).sqrt() as f32)
}

fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

struct Chunk {
    samples: Vec<i16>,
    level: f32,
}

#[derive(Default)]
struct Tally {
    frames_written: u64,
    gap_markers: Vec<f64>,
    peak_level: f32,
    written_peak_level: f32,
    metered_anything: bool,
}

pub struct WasapiCapture {
    callbacks: Arc<Mutex<CaptureCallbacks>>,
    stop_flag: Arc<AtomicBool>,
    engine: Option<std::thread::JoinHandle<Tally>>,
    result: Option<AudioCaptureResult>,
    /// The microphone the user chose, by name. `None` follows the Windows
    /// default device, including when the user changes it mid-recording.
    preferred_device: Option<String>,
}

impl Default for WasapiCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl WasapiCapture {
    pub fn new() -> Self {
        Self::for_device(None)
    }

    pub fn for_device(preferred_device: Option<String>) -> Self {
        Self {
            callbacks: Arc::new(Mutex::new(CaptureCallbacks::default())),
            stop_flag: Arc::new(AtomicBool::new(false)),
            engine: None,
            result: None,
            preferred_device: preferred_device.filter(|name| !name.trim().is_empty()),
        }
    }
}

impl AudioCapturing for WasapiCapture {
    fn callbacks(&self) -> Arc<Mutex<CaptureCallbacks>> {
        self.callbacks.clone()
    }

    fn start(&mut self, path: &Path) -> Result<(), CaptureError> {
        if self.engine.is_some() {
            return Ok(());
        }
        // The device is probed on THIS thread so a missing microphone or an
        // unusable config is a synchronous error the coordinator can classify,
        // rather than an asynchronous death the user sees as a hung pill.
        let (device, config) = open_input(self.preferred_device.as_deref())?;
        let writer = open_writer(path)?;

        let (chunk_tx, chunk_rx) = sync_channel::<Chunk>(CHUNK_QUEUE_DEPTH);
        let callbacks = self.callbacks.clone();
        let stop_flag = self.stop_flag.clone();
        stop_flag.store(false, Ordering::SeqCst);
        let path = path.to_path_buf();
        let preferred = self.preferred_device.clone();

        let engine = std::thread::Builder::new()
            .name("jot-audio".into())
            .spawn(move || {
                run_engine(EngineArgs {
                    device,
                    config,
                    writer,
                    path,
                    chunk_tx,
                    chunk_rx,
                    callbacks,
                    stop_flag,
                    preferred,
                })
            })
            .map_err(|error| CaptureError::Io(error.to_string()))?;

        self.engine = Some(engine);
        Ok(())
    }

    fn stop(&mut self) -> AudioCaptureResult {
        if let Some(result) = &self.result {
            return result.clone();
        }
        let Some(engine) = self.engine.take() else {
            return AudioCaptureResult::empty();
        };
        self.stop_flag.store(true, Ordering::SeqCst);
        let tally = engine.join().unwrap_or_default();

        let result = AudioCaptureResult {
            frames_written: tally.frames_written,
            duration_seconds: tally.frames_written as f64 / SAMPLE_RATE as f64,
            gap_markers: tally.gap_markers,
            peak_level: tally.peak_level,
            written_peak_level: tally.written_peak_level,
            peak_is_trustworthy: tally.metered_anything || tally.frames_written > 0,
        };
        self.result = Some(result.clone());
        result
    }
}

impl Drop for WasapiCapture {
    fn drop(&mut self) {
        // A dropped capture must not leave the microphone open and the WAV
        // header unfinished.
        if self.engine.is_some() {
            let _ = self.stop();
        }
    }
}

type Writer = hound::WavWriter<std::io::BufWriter<std::fs::File>>;

struct EngineArgs {
    device: cpal::Device,
    config: cpal::SupportedStreamConfig,
    writer: Writer,
    path: PathBuf,
    chunk_tx: SyncSender<Chunk>,
    chunk_rx: Receiver<Chunk>,
    callbacks: Arc<Mutex<CaptureCallbacks>>,
    stop_flag: Arc<AtomicBool>,
    preferred: Option<String>,
}

fn run_engine(args: EngineArgs) -> Tally {
    let EngineArgs {
        device,
        config,
        mut writer,
        path,
        chunk_tx,
        chunk_rx,
        callbacks,
        stop_flag,
        preferred,
    } = args;

    let mut tally = Tally::default();
    let started = Instant::now();
    let overflowed = Arc::new(AtomicBool::new(false));

    // Identity, not the friendly name: two docks can present microphones with
    // exactly the same name, and a rename is not a device change.
    let mut current_identity = device_identity(preferred.as_deref(), &device);
    let mut stream = match build_stream(&device, &config, chunk_tx.clone(), overflowed.clone()) {
        Ok(stream) => Some(stream),
        Err(error) => {
            notify_engine_died(&callbacks, &format!("Microphone stopped ({error})"));
            None
        }
    };

    let mut last_housekeeping = Instant::now();
    let mut write_failures = 0_u32;
    let mut write_failure_reported = false;
    let mut stopping_since: Option<Instant> = None;

    loop {
        match chunk_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                let mut wrote = true;
                for sample in &chunk.samples {
                    if writer.write_sample(*sample).is_err() {
                        wrote = false;
                        break;
                    }
                    let level = curve::level_from_rms((*sample as f32 / i16::MAX as f32).abs());
                    tally.written_peak_level = tally.written_peak_level.max(level);
                }
                if wrote {
                    write_failures = 0;
                    tally.frames_written += chunk.samples.len() as u64;
                } else {
                    write_failures += 1;
                    if write_failures >= WRITE_FAILURE_LIMIT && !write_failure_reported {
                        write_failure_reported = true;
                        tracing::error!(path = %path.display(), "sustained WAV write failures");
                        if let Some(on_write_failure) = &callbacks.lock().on_write_failure {
                            on_write_failure();
                        }
                    }
                }
                tally.metered_anything = true;
                tally.peak_level = tally.peak_level.max(chunk.level);
                if let Some(on_level) = &callbacks.lock().on_level {
                    on_level(chunk.level);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if overflowed.swap(false, Ordering::SeqCst) {
            tracing::warn!("audio chunk queue overflowed — disk cannot keep up");
        }

        if stop_flag.load(Ordering::SeqCst) {
            // Keep the stream alive briefly: the device hands over whole
            // buffers, so dropping it the instant the key comes up truncates
            // the last word.
            let since = *stopping_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= DRAIN_AFTER_STOP {
                break;
            }
            continue;
        }

        if last_housekeeping.elapsed() >= HOUSEKEEPING_INTERVAL {
            last_housekeeping = Instant::now();
            // Rewriting the header each second is what makes a killed process
            // leave a playable, transcribable file behind.
            if let Err(error) = writer.flush() {
                tracing::warn!(%error, "WAV flush failed");
            }
            if let Some((device, config, identity, name)) =
                changed_input(preferred.as_deref(), &current_identity)
            {
                tracing::info!(to = %name, "input device changed");
                current_identity = identity;
                tally.gap_markers.push(started.elapsed().as_secs_f64());
                drop(stream.take());
                match build_stream(&device, &config, chunk_tx.clone(), overflowed.clone()) {
                    Ok(rebuilt) => {
                        stream = Some(rebuilt);
                        if let Some(on_device_change) = &callbacks.lock().on_device_change {
                            on_device_change(format!("Switched to {name}"));
                        }
                    }
                    Err(error) => {
                        notify_engine_died(&callbacks, &format!("Microphone stopped ({error})"));
                        break;
                    }
                }
            }
        }
    }

    drop(stream);
    // Drain whatever the audio thread queued between the stop decision and the
    // stream actually going away.
    while let Ok(chunk) = chunk_rx.try_recv() {
        for sample in &chunk.samples {
            if writer.write_sample(*sample).is_err() {
                break;
            }
            tally.frames_written += 1;
        }
    }
    if let Err(error) = writer.finalize() {
        tracing::error!(%error, path = %path.display(), "finalizing the WAV failed");
    }
    tally
}

fn notify_engine_died(callbacks: &Arc<Mutex<CaptureCallbacks>>, message: &str) {
    tracing::error!(message, "audio engine died mid-recording");
    if let Some(on_engine_died) = &callbacks.lock().on_engine_died {
        on_engine_died(message.to_string());
    }
}

fn device_name(device: &cpal::Device) -> String {
    device.name().unwrap_or_else(|_| "input device".into())
}

/// Opens the chosen microphone, or the Windows default when none is chosen.
///
/// A chosen device that has been unplugged is NOT silently swapped for the
/// default: recording from a different microphone than the one the user picked,
/// without saying so, is worse than refusing.
fn open_input(
    preferred: Option<&str>,
) -> Result<(cpal::Device, cpal::SupportedStreamConfig), CaptureError> {
    let host = cpal::default_host();
    let device = match preferred {
        Some(wanted) => host
            .input_devices()
            .ok()
            .and_then(|mut devices| {
                devices.find(|device| device.name().is_ok_and(|name| name == wanted))
            })
            .ok_or(CaptureError::NoInputDevice)?,
        None => host
            .default_input_device()
            .ok_or(CaptureError::NoInputDevice)?,
    };
    let config = device
        .default_input_config()
        .map_err(|error| CaptureError::Config(error.to_string()))?;
    Ok((device, config))
}

/// The WASAPI endpoint id of the default capture device.
#[cfg(target_os = "windows")]
fn default_endpoint_id() -> Option<String> {
    use windows::Win32::Media::Audio::{
        IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eCommunications,
    };
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};

    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eCapture, eCommunications)
            .ok()?;
        let id = device.GetId().ok()?;
        let text = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const _));
        text
    }
}

#[cfg(not(target_os = "windows"))]
fn default_endpoint_id() -> Option<String> {
    None
}

/// The stable identity of the device currently in use.
///
/// A user-chosen microphone is identified by the name they picked, because that
/// is what has to keep matching. The system default is identified by its WASAPI
/// endpoint id, which survives a rename and tells apart two devices that share
/// a friendly name.
fn device_identity(preferred: Option<&str>, device: &cpal::Device) -> String {
    match preferred {
        Some(name) => format!("chosen:{name}"),
        None => match default_endpoint_id() {
            Some(id) => format!("endpoint:{id}"),
            // No endpoint id available: fall back to the name rather than
            // treating every poll as a change.
            None => format!("name:{}", device_name(device)),
        },
    }
}

/// Returns the new input device when the one in use is no longer the right one.
///
/// WASAPI can report this through `IMMNotificationClient`, but that requires a
/// COM apartment on this thread for the lifetime of the recording; a
/// once-a-second comparison costs nothing on the recording path and cannot
/// leave a dangling callback behind.
fn changed_input(
    preferred: Option<&str>,
    current: &str,
) -> Option<(cpal::Device, cpal::SupportedStreamConfig, String, String)> {
    let (device, config) = open_input(preferred).ok()?;
    let identity = device_identity(preferred, &device);
    (identity != current).then(|| {
        let name = device_name(&device);
        (device, config, identity, name)
    })
}

fn open_writer(path: &Path) -> Result<Writer, CaptureError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| CaptureError::Io(error.to_string()))?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    hound::WavWriter::create(path, spec).map_err(|error| CaptureError::Io(error.to_string()))
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    chunk_tx: SyncSender<Chunk>,
    overflowed: Arc<AtomicBool>,
) -> Result<cpal::Stream, CaptureError> {
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build_typed::<f32>(device, config, chunk_tx, overflowed),
        cpal::SampleFormat::I16 => build_typed::<i16>(device, config, chunk_tx, overflowed),
        cpal::SampleFormat::U16 => build_typed::<u16>(device, config, chunk_tx, overflowed),
        cpal::SampleFormat::I32 => build_typed::<i32>(device, config, chunk_tx, overflowed),
        cpal::SampleFormat::I8 => build_typed::<i8>(device, config, chunk_tx, overflowed),
        cpal::SampleFormat::U8 => build_typed::<u8>(device, config, chunk_tx, overflowed),
        other => Err(CaptureError::Config(format!(
            "unsupported sample format {other:?}"
        ))),
    }?;
    stream
        .play()
        .map_err(|error| CaptureError::Config(error.to_string()))?;
    Ok(stream)
}

fn build_typed<T>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    chunk_tx: SyncSender<Chunk>,
    overflowed: Arc<AtomicBool>,
) -> Result<cpal::Stream, CaptureError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels().max(1) as usize;
    let mut resampler = Resampler::new(config.sample_rate().0);
    let mut mono = Vec::<f32>::with_capacity(1024);

    let stream = device
        .build_input_stream(
            &config.config(),
            move |input: &[T], _| {
                mono.clear();
                for frame in input.chunks(channels) {
                    // Downmix: a headset's second channel is the same voice, and
                    // averaging is quieter about a dead channel than picking one.
                    let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
                    resampler.push(sum / frame.len() as f32, &mut mono);
                }
                if mono.is_empty() {
                    return;
                }
                let chunk = Chunk {
                    samples: mono.iter().copied().map(to_i16).collect(),
                    level: rms_level(&mono),
                };
                // Never block the audio thread: a stalled disk must glitch the
                // recording, not the whole system's audio graph.
                if let Err(TrySendError::Full(_)) = chunk_tx.try_send(chunk) {
                    overflowed.store(true, Ordering::SeqCst);
                }
            },
            |error| tracing::error!(%error, "input stream error"),
            None,
        )
        .map_err(|error| CaptureError::Config(error.to_string()))?;
    Ok(stream)
}

/// Input device names for the Settings pane.
pub fn input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|devices| devices.filter_map(|device| device.name().ok()).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Test double
// ---------------------------------------------------------------------------

/// A recorder that writes nothing and reports exactly what a test tells it to.
/// The coordinator's failure paths are all reachable through this.
pub struct FakeCapture {
    pub callbacks: Arc<Mutex<CaptureCallbacks>>,
    pub result: AudioCaptureResult,
    pub start_error: Option<CaptureError>,
    /// Levels replayed to the level callback the moment capture starts, so
    /// tests can exercise the room-noise branches the real engine feeds.
    pub levels: Vec<f32>,
    pub started: bool,
    pub stopped: bool,
}

impl Default for FakeCapture {
    fn default() -> Self {
        Self {
            callbacks: Arc::new(Mutex::new(CaptureCallbacks::default())),
            result: AudioCaptureResult {
                frames_written: 16_000,
                duration_seconds: 1.0,
                gap_markers: Vec::new(),
                peak_level: 0.5,
                written_peak_level: 0.5,
                peak_is_trustworthy: true,
            },
            start_error: None,
            levels: Vec::new(),
            started: false,
            stopped: false,
        }
    }
}

impl AudioCapturing for FakeCapture {
    fn callbacks(&self) -> Arc<Mutex<CaptureCallbacks>> {
        self.callbacks.clone()
    }

    fn start(&mut self, path: &Path) -> Result<(), CaptureError> {
        if let Some(error) = self.start_error.clone() {
            return Err(error);
        }
        // Still create the file: callers reasonably assume a started capture has
        // something on disk to hand to the encoder.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, []);
        for level in &self.levels {
            if let Some(on_level) = &self.callbacks.lock().on_level {
                on_level(*level);
            }
        }
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) -> AudioCaptureResult {
        self.stopped = true;
        self.result.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resample(input_rate: u32, samples: &[f32]) -> Vec<f32> {
        let mut resampler = Resampler::new(input_rate);
        let mut out = Vec::new();
        for sample in samples {
            resampler.push(*sample, &mut out);
        }
        out
    }

    #[test]
    fn integer_ratio_decimation_averages_rather_than_dropping() {
        // 48 kHz → 16 kHz is exactly 3:1.
        let out = resample(48_000, &[0.0, 0.3, 0.6, 1.0, 1.0, 1.0]);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.3).abs() < 1e-6);
        assert!((out[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn fractional_ratios_keep_the_output_rate_honest() {
        let input: Vec<f32> = (0..44_100).map(|_| 0.5).collect();
        let out = resample(44_100, &input);
        // One second in, one second out — within a sample of the target rate.
        assert!(
            out.len().abs_diff(SAMPLE_RATE as usize) <= 1,
            "got {} samples",
            out.len()
        );
    }

    #[test]
    fn passthrough_rate_is_a_no_op() {
        let out = resample(SAMPLE_RATE, &[0.1, 0.2, 0.3]);
        assert_eq!(out.len(), 3);
        assert!((out[2] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn silence_meters_as_zero_and_full_scale_saturates() {
        assert_eq!(rms_level(&[0.0; 128]), 0.0);
        assert_eq!(rms_level(&[]), 0.0);
        assert_eq!(rms_level(&[1.0; 128]), 1.0);
    }

    #[test]
    fn quiet_speech_lands_mid_scale_rather_than_hugging_the_floor() {
        // −34 dBFS is an ordinary quiet-room speaking level.
        let level = rms_level(&[0.02; 256]);
        assert!(level > 0.1 && level < 0.5, "level was {level}");
    }

    #[test]
    fn sample_conversion_clamps_instead_of_wrapping() {
        assert_eq!(to_i16(2.0), i16::MAX);
        assert_eq!(to_i16(-2.0), -i16::MAX);
        assert_eq!(to_i16(0.0), 0);
    }

    #[test]
    fn an_unmetered_capture_is_marked_untrustworthy() {
        let tally = Tally::default();
        let result = AudioCaptureResult {
            frames_written: tally.frames_written,
            duration_seconds: 0.0,
            gap_markers: tally.gap_markers,
            peak_level: tally.peak_level,
            written_peak_level: tally.written_peak_level,
            peak_is_trustworthy: tally.metered_anything || tally.frames_written > 0,
        };
        assert!(!result.peak_is_trustworthy);
    }

    #[test]
    fn the_fake_reports_a_start_failure_without_touching_disk() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("audio.wav");
        let mut capture = FakeCapture {
            start_error: Some(CaptureError::NoInputDevice),
            ..Default::default()
        };
        assert_eq!(capture.start(&path), Err(CaptureError::NoInputDevice));
        assert!(!path.exists());
        assert!(!capture.started);
    }
}
