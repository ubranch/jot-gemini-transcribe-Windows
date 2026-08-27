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

//! Jot's headless engine: state machine, hotkey grammar, audio, transcription,
//! formatting, insertion and history. No UI framework leaks in here, so every
//! failure mode is exercisable without launching the app.

pub mod audio;
pub mod coordinator;
pub mod credentials;
pub mod dictionary;
pub mod file_layout;
pub mod gemini;
pub mod history;
pub mod hotkey;
pub mod insertion;
pub mod levels;
pub mod meta;
pub mod prompt;
pub mod recovery;
pub mod replacement;
pub mod runtime;
pub mod settings;
pub mod state_machine;
pub mod timeout;
pub mod transcription;
pub mod update;
pub mod validation;
pub mod win32;
