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

//! The insertion ladder, with the two guards that fix the most-reported bugs in
//! this category of app:
//!
//! ```text
//!   guard 1: foreground app changed since dictation started -> NEVER paste blind
//!            (text goes to the clipboard; the HUD offers it)
//!   guard 2: a password field has focus -> refuse entirely, no clipboard leak
//!
//!   tier 1: synthetic Unicode typing into a proven text control (no clipboard)
//!   tier 2: clipboard + synthesized Ctrl+V with guarded restore
//!   tier 3: text left on the clipboard, visible hint
//! ```
//!
//! Tier 1 is the Windows counterpart of the macOS Accessibility write: it is the
//! only path that never touches the clipboard. Windows has no non-destructive
//! "insert at selection" automation call — `IUIAutomationValuePattern::SetValue`
//! replaces the entire control's contents — so typing is the honest equivalent,
//! gated on UI Automation confirming a text control actually has focus.

use crate::transcription::DictationContext;
use crate::win32::{self, FocusKind};
use async_trait::async_trait;
use std::time::Duration;

/// How long the clipboard keeps the transcript before the user's own contents
/// go back. Slow applications read the clipboard late; restoring too early
/// pastes the user's OLD clipboard.
pub const RESTORE_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertionOutcome {
    Inserted,
    FrontmostChanged,
    FellBackToClipboard,
    /// A password field had focus — the text stays in History, never on the
    /// clipboard.
    BlockedSecureField,
}

/// Insertion seam; tests use fakes.
#[async_trait]
pub trait TextInserting: Send + Sync {
    async fn insert(&self, text: &str, context: &DictationContext) -> InsertionOutcome;
}

#[derive(Default)]
pub struct InsertionCoordinator;

impl InsertionCoordinator {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TextInserting for InsertionCoordinator {
    async fn insert(&self, text: &str, context: &DictationContext) -> InsertionOutcome {
        // Guard 2: a password field, or the secure desktop. The transcript stays
        // in History only — putting it on the clipboard would leak it to every
        // clipboard reader on the machine.
        if win32::focus_kind() == FocusKind::Password {
            tracing::warn!("password field focused at insert time — refusing (History only)");
            return InsertionOutcome::BlockedSecureField;
        }

        // Guard 1: is the user still where they started dictating?
        if moved_away(context) {
            tracing::info!("foreground changed since dictation started — no blind paste");
            copy_only(text);
            return InsertionOutcome::FrontmostChanged;
        }

        // Tier 1: type it. No clipboard involved, so nothing to restore and
        // nothing for a clipboard manager to archive.
        if text.chars().count() <= win32::MAX_TYPED_CHARS
            && win32::focus_kind() == FocusKind::EditableText
            && win32::type_text(text)
        {
            tracing::info!("inserted by typing");
            return InsertionOutcome::Inserted;
        }

        // Guard 1 re-check: the focus queries above take a UI Automation
        // round-trip, and an Alt+Tab in that window would land the Ctrl+V in the
        // wrong app.
        if moved_away(context) {
            tracing::info!("foreground changed during the typing attempt — no blind paste");
            copy_only(text);
            return InsertionOutcome::FrontmostChanged;
        }

        // Tier 2: guarded paste. "True" means the Ctrl+V was POSTED — there is
        // no OS-level delivery receipt for a synthetic paste. That is the
        // industry floor; the text also stays recoverable in History.
        if paste(text).await {
            tracing::info!("Ctrl+V posted (delivery is best-effort)");
            return InsertionOutcome::Inserted;
        }

        // Tier 3: clipboard floor.
        copy_only(text);
        InsertionOutcome::FellBackToClipboard
    }
}

/// True when the foreground process is provably not the one the user was
/// dictating into. An unknown pid on either side is NOT treated as a move: the
/// cost of a false positive is a chip the user has to click, every time.
fn moved_away(context: &DictationContext) -> bool {
    let Some(expected) = context.target_pid else {
        return false;
    };
    match win32::foreground_app().pid {
        Some(current) => current != expected,
        None => false,
    }
}

/// Puts text on the clipboard WITHOUT restore — the tier-3 floor and the
/// focus-changed path ("Copied — press Ctrl+V").
pub fn copy_only(text: &str) {
    win32::write_clipboard(text);
}

/// Writes the transcript, posts Ctrl+V, and puts the user's clipboard back a
/// second later — but only if nothing else has touched it in the meantime.
///
/// Returns as soon as Ctrl+V is posted. The restore runs detached so the session
/// is not pinned in `Inserting` for a second after the text visibly landed:
/// that window used to silently reject the next dictation's `begin`.
async fn paste(text: &str) -> bool {
    let snapshot = win32::snapshot_clipboard();
    let Some(sequence) = win32::write_clipboard(text) else {
        return false;
    };
    if !win32::post_paste() {
        // Leave the text on the clipboard — it is the fallback content.
        return false;
    }
    crate::runtime::spawn(async move {
        tokio::time::sleep(RESTORE_DELAY).await;
        // Restore only if nothing else touched the clipboard since our write: a
        // user copy mid-flight wins.
        if win32::clipboard_sequence_number() == sequence && !snapshot.is_empty() {
            win32::restore_clipboard(snapshot);
        }
    });
    true
}

// ---------------------------------------------------------------------------
// Test double
// ---------------------------------------------------------------------------

/// Records what it was asked to insert and returns a scripted outcome.
pub struct FakeInserter {
    pub outcome: InsertionOutcome,
    pub inserted: parking_lot::Mutex<Vec<String>>,
}

impl Default for FakeInserter {
    fn default() -> Self {
        Self {
            outcome: InsertionOutcome::Inserted,
            inserted: parking_lot::Mutex::new(Vec::new()),
        }
    }
}

impl FakeInserter {
    pub fn with_outcome(outcome: InsertionOutcome) -> Self {
        Self {
            outcome,
            ..Default::default()
        }
    }
}

#[async_trait]
impl TextInserting for FakeInserter {
    async fn insert(&self, text: &str, _context: &DictationContext) -> InsertionOutcome {
        self.inserted.lock().push(text.to_string());
        self.outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_target_pid_never_reads_as_a_move() {
        // Refusing to paste whenever we cannot prove where we are would put a
        // chip in front of the user on every single dictation.
        assert!(!moved_away(&DictationContext::default()));
    }

    #[test]
    fn a_known_target_pid_is_compared_against_the_foreground() {
        let context = DictationContext {
            target_pid: Some(u32::MAX), // no real process can own this
            ..Default::default()
        };
        // Either the foreground pid is unknown (no move) or it differs (move).
        // Both are correct here; what must never happen is a panic or a claim
        // that a nonexistent process is still in front.
        let foreground = win32::foreground_app().pid;
        assert_eq!(moved_away(&context), foreground.is_some());
    }

    #[tokio::test]
    async fn the_fake_records_what_it_was_handed() {
        let inserter = FakeInserter::with_outcome(InsertionOutcome::FellBackToClipboard);
        let outcome = inserter
            .insert("hello there", &DictationContext::default())
            .await;
        assert_eq!(outcome, InsertionOutcome::FellBackToClipboard);
        assert_eq!(inserter.inserted.lock().as_slice(), ["hello there"]);
    }
}
