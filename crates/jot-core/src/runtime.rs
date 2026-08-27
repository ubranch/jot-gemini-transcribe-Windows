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

//! The async runtime handle the engine schedules on.
//!
//! Work reaches the coordinator from threads that know nothing about tokio —
//! the audio engine's writer thread, the keyboard hook, the GPUI application
//! thread — and a bare `tokio::spawn` from any of them panics. Everything in
//! this crate goes through here instead, so there is exactly one place that
//! knows where background work runs.

use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

static HANDLE: OnceLock<Handle> = OnceLock::new();

/// Records the runtime the engine should schedule on. Call once at startup.
pub fn install(handle: Handle) {
    let _ = HANDLE.set(handle);
}

/// The installed runtime, falling back to the caller's own when the engine is
/// driven directly from an async context — which is what tests do.
pub fn handle() -> Handle {
    HANDLE
        .get()
        .cloned()
        .or_else(|| Handle::try_current().ok())
        .expect("no async runtime installed — call runtime::install at startup")
}

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    handle().spawn(future)
}

pub fn spawn_blocking<F, R>(job: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    handle().spawn_blocking(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn work_runs_on_the_ambient_runtime_when_none_is_installed() {
        assert_eq!(spawn(async { 7 }).await.unwrap(), 7);
        assert_eq!(spawn_blocking(|| 9).await.unwrap(), 9);
    }
}
