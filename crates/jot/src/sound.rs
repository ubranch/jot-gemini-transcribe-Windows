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

//! Earcons.
//!
//! The audio device lives on a thread of its own: opening it can block, the
//! handle is not `Send`, and nothing about a UI sound should be able to stall a
//! key press. Failing to play one is never an error worth surfacing — a
//! dictation that works silently is still a dictation that works.

use std::io::Cursor;
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Earcon {
    Start,
    Stop,
    Success,
    Error,
    Cancel,
    Lock,
    Celebration,
}

impl Earcon {
    fn bytes(self) -> &'static [u8] {
        match self {
            Earcon::Start => include_bytes!("../assets/sounds/start.wav"),
            Earcon::Stop => include_bytes!("../assets/sounds/stop.wav"),
            Earcon::Success => include_bytes!("../assets/sounds/success.wav"),
            Earcon::Error => include_bytes!("../assets/sounds/error.wav"),
            Earcon::Cancel => include_bytes!("../assets/sounds/cancel.wav"),
            Earcon::Lock => include_bytes!("../assets/sounds/lock.wav"),
            Earcon::Celebration => include_bytes!("../assets/sounds/celebration.wav"),
        }
    }
}

static PLAYER: OnceLock<Sender<Earcon>> = OnceLock::new();

/// Starts the audio thread. Safe to call more than once.
pub fn start() {
    PLAYER.get_or_init(|| {
        let (tx, rx) = channel::<Earcon>();
        std::thread::Builder::new()
            .name("jot-earcons".into())
            .spawn(move || {
                let Ok((_stream, handle)) = rodio::OutputStream::try_default() else {
                    tracing::info!("no output device — earcons are off");
                    // Keep draining so senders never block on a full channel.
                    while rx.recv().is_ok() {}
                    return;
                };
                while let Ok(earcon) = rx.recv() {
                    let Ok(sink) = rodio::Sink::try_new(&handle) else {
                        continue;
                    };
                    match rodio::Decoder::new(Cursor::new(earcon.bytes())) {
                        Ok(source) => {
                            sink.append(source);
                            // Detach so a long earcon cannot delay the next one.
                            sink.detach();
                        }
                        Err(error) => tracing::warn!(?earcon, %error, "earcon failed to decode"),
                    }
                }
            })
            .expect("spawning the earcon thread");
        tx
    });
}

/// Plays an earcon if sounds are enabled. Never blocks the caller.
pub fn play(earcon: Earcon, enabled: bool) {
    if !enabled {
        return;
    }
    if let Some(player) = PLAYER.get() {
        let _ = player.send(earcon);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_earcon_carries_real_audio() {
        for earcon in [
            Earcon::Start,
            Earcon::Stop,
            Earcon::Success,
            Earcon::Error,
            Earcon::Cancel,
            Earcon::Lock,
            Earcon::Celebration,
        ] {
            let bytes = earcon.bytes();
            assert!(bytes.len() > 44, "{earcon:?} is header-only");
            assert_eq!(&bytes[..4], b"RIFF", "{earcon:?} is not a WAV");
        }
    }

    #[test]
    fn playing_with_sounds_off_does_nothing_even_before_start() {
        // No thread running and no panic: a muted user must never hit a code
        // path the enabled one does not.
        play(Earcon::Start, false);
    }
}
