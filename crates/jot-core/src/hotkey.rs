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

//! The hotkey key set and the pure hold/lock/cancel grammar.
//!
//! Windows has no `fn` key: it is handled in keyboard firmware and never
//! reaches `WH_KEYBOARD_LL`, so the macOS default has no equivalent here. The
//! set below is instead the bare keys a low-level hook *can* see and swallow.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A bare-modifier dictation key, identified by Windows virtual-key code.
///
/// These are keys `RegisterHotKey` cannot bind on their own (it requires a
/// modifier + a normal key) — hence the low-level keyboard hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HotkeyKey {
    RightControl,
    RightAlt,
    RightShift,
    RightWin,
    /// Held like `fn` and in the same corner. Requires the hook to swallow the
    /// key so the shift-lock state never toggles while dictating.
    CapsLock,
}

impl Default for HotkeyKey {
    /// Right Ctrl: reachable with the right thumb/pinky, almost never used bare,
    /// and unlike Caps Lock it carries no toggle state to suppress.
    fn default() -> Self {
        HotkeyKey::RightControl
    }
}

impl HotkeyKey {
    pub const ALL: [HotkeyKey; 5] = [
        HotkeyKey::RightControl,
        HotkeyKey::RightAlt,
        HotkeyKey::RightShift,
        HotkeyKey::RightWin,
        HotkeyKey::CapsLock,
    ];

    /// Windows virtual-key code as reported by `KBDLLHOOKSTRUCT::vkCode`.
    pub fn vk_code(self) -> u32 {
        match self {
            HotkeyKey::RightControl => 0xA3, // VK_RCONTROL
            HotkeyKey::RightAlt => 0xA5,     // VK_RMENU
            HotkeyKey::RightShift => 0xA1,   // VK_RSHIFT
            HotkeyKey::RightWin => 0x5C,     // VK_RWIN
            HotkeyKey::CapsLock => 0x14,     // VK_CAPITAL
        }
    }

    /// True when this key must be swallowed by the hook rather than passed on.
    ///
    /// Caps Lock toggles a global state, and Right Win opens the Start menu on
    /// release — both would fire on every dictation. The plain modifiers are
    /// passed through: swallowing them would break chords the user still types.
    pub fn must_suppress(self) -> bool {
        matches!(self, HotkeyKey::CapsLock | HotkeyKey::RightWin)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            HotkeyKey::RightControl => "Right Ctrl",
            HotkeyKey::RightAlt => "Right Alt",
            HotkeyKey::RightShift => "Right Shift",
            HotkeyKey::RightWin => "Right Win",
            HotkeyKey::CapsLock => "Caps Lock",
        }
    }

    pub fn from_vk(vk: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.vk_code() == vk)
    }
}

/// What the hotkey layer asks the session coordinator to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyIntent {
    /// Key went down — start recording NOW (audio from t=0, before classification).
    Begin,
    /// Double-tap detected — lock into hands-free recording.
    LockIn,
    /// Hold released (or stop requested while locked) — finalize and transcribe.
    Finalize,
    /// Esc — cancel the session (audio still saved per retention policy).
    Cancel,
    /// Single short tap with no second tap — cancel quietly and show the
    /// "Hold to talk — tap Space to lock" coaching hint (never an error sound).
    ShortTapHint,
    /// Another key was typed within the interruption window — accidental chord,
    /// cancel silently.
    AbortAccidental,
}

/// Timing constants for the hotkey grammar.
pub mod tuning {
    use std::time::Duration;

    /// Press shorter than this is a "tap"; at/above is a hold (push-to-talk).
    pub const HOLD_THRESHOLD: Duration = Duration::from_millis(300);
    /// Max gap between the first tap's key-up and the second tap's key-down to
    /// count as a double-tap.
    pub const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(500);
    /// A non-hotkey keystroke within this window of session start aborts as accidental.
    pub const INTERRUPTION_WINDOW: Duration = Duration::from_millis(1000);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    HotkeyDown,
    HotkeyUp,
    EscDown,
    OtherKeyDown,
    /// Space pressed while the hotkey is physically held — the timing-free
    /// hands-free gesture ("hold, tap Space, let go").
    SpaceLock,
    /// The double-tap window expired (fed back by the timer the caller armed).
    DoubleTapTimeout,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Effects {
    pub intents: Vec<HotkeyIntent>,
    /// Arm the double-tap timer to fire after this duration (`None` = leave as-is).
    pub arm_timer: Option<Duration>,
    pub disarm_timer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Idle,
    /// Key physically down, classification pending (hold vs tap).
    Pressed {
        down_at: Duration,
        session_start_at: Duration,
    },
    /// First short tap released; waiting for a possible second tap. Still recording.
    PendingSecondTap { session_start_at: Duration },
    /// Hands-free.
    Locked,
}

/// The pure hotkey grammar:
///
/// ```text
///   hold >= 0.3s           -> push-to-talk: key-up finalizes
///   tap, tap (<= 0.5s gap) -> hands-free lock: press again finalizes
///   single tap             -> coaching hint, session quietly cancelled
///   Esc                    -> cancel
///   other key < 1s in      -> accidental chord, silent abort
/// ```
///
/// Recording ALWAYS starts on the first key-down (`Begin`) so no audio is ever
/// lost while the grammar disambiguates. Between the taps of a double-tap the
/// session keeps recording.
///
/// Pure and clock-free: callers pass monotonic timestamps; timers are returned
/// as effects and fed back in as `DoubleTapTimeout`.
#[derive(Debug, Default)]
pub struct HotkeyProcessor {
    phase: Phase,
    /// When set, the next key-up of the hotkey belongs to an already-classified
    /// press (lock stop, cancel, abort) and must be swallowed without effects.
    swallow_next_up: bool,
    /// When off, a short tap hints immediately and never arms the double-tap
    /// window. Default OFF: firm taps routinely exceed the hold threshold,
    /// misreading tap-tap as hold→finalize. Space-while-holding replaced it.
    pub double_tap_lock_enabled: bool,
}

impl HotkeyProcessor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snap back to idle after the coordinator REFUSES a begin (password field,
    /// busy) — otherwise a Space-lock on the phantom session strands the grammar
    /// in `Locked` and silently eats the next dictation attempt.
    pub fn reset(&mut self) {
        self.phase = Phase::Idle;
        self.swallow_next_up = false;
    }

    /// True while the hotkey is physically down (the Space-lock gesture window).
    pub fn is_key_held(&self) -> bool {
        matches!(self.phase, Phase::Pressed { .. })
    }

    /// True whenever a dictation session is in flight from the hotkey's
    /// perspective — the hook uses this to decide whether to intercept Esc.
    pub fn is_session_active(&self) -> bool {
        self.phase != Phase::Idle
    }

    pub fn handle(&mut self, event: HotkeyEvent, now: Duration) -> Effects {
        use HotkeyEvent as E;
        let mut fx = Effects::default();

        match (self.phase, event) {
            // ----- idle
            (Phase::Idle, E::HotkeyDown) => {
                self.phase = Phase::Pressed {
                    down_at: now,
                    session_start_at: now,
                };
                self.swallow_next_up = false;
                fx.intents.push(HotkeyIntent::Begin);
            }
            // Residual up from a press we already classified (lock stop, cancel…).
            (Phase::Idle, E::HotkeyUp) => self.swallow_next_up = false,
            (Phase::Idle, _) => {}

            // ----- pressed (key down, disambiguating)
            (
                Phase::Pressed {
                    down_at,
                    session_start_at,
                },
                E::HotkeyUp,
            ) => {
                if self.swallow_next_up {
                    self.swallow_next_up = false;
                } else if now.saturating_sub(down_at) >= tuning::HOLD_THRESHOLD {
                    self.phase = Phase::Idle;
                    fx.intents.push(HotkeyIntent::Finalize);
                } else if self.double_tap_lock_enabled {
                    self.phase = Phase::PendingSecondTap { session_start_at };
                    fx.arm_timer = Some(tuning::DOUBLE_TAP_WINDOW);
                } else {
                    self.phase = Phase::Idle;
                    fx.intents.push(HotkeyIntent::ShortTapHint);
                }
            }
            (Phase::Pressed { .. }, E::EscDown) => {
                self.phase = Phase::Idle;
                self.swallow_next_up = true;
                fx.intents.push(HotkeyIntent::Cancel);
            }
            (
                Phase::Pressed {
                    session_start_at, ..
                },
                E::OtherKeyDown,
            ) => {
                if now.saturating_sub(session_start_at) < tuning::INTERRUPTION_WINDOW {
                    self.phase = Phase::Idle;
                    self.swallow_next_up = true;
                    fx.intents.push(HotkeyIntent::AbortAccidental);
                }
                // After the window: the user is deliberately chording/typing
                // mid-hold — keep going.
            }
            // Hold + tap Space = hands-free, no timing window. The hotkey release
            // that follows belongs to this gesture and must not finalize.
            (Phase::Pressed { .. }, E::SpaceLock) => {
                self.phase = Phase::Locked;
                self.swallow_next_up = true;
                fx.intents.push(HotkeyIntent::LockIn);
            }
            (Phase::Pressed { .. }, E::HotkeyDown | E::DoubleTapTimeout) => {}

            // ----- pendingSecondTap (short tap released, window open, still recording)
            (Phase::PendingSecondTap { .. }, E::HotkeyDown) => {
                self.phase = Phase::Locked;
                self.swallow_next_up = true;
                fx.intents.push(HotkeyIntent::LockIn);
                fx.disarm_timer = true;
            }
            (Phase::PendingSecondTap { .. }, E::DoubleTapTimeout) => {
                self.phase = Phase::Idle;
                fx.intents.push(HotkeyIntent::ShortTapHint);
            }
            (Phase::PendingSecondTap { .. }, E::EscDown) => {
                self.phase = Phase::Idle;
                fx.intents.push(HotkeyIntent::Cancel);
                fx.disarm_timer = true;
            }
            (Phase::PendingSecondTap { session_start_at }, E::OtherKeyDown) => {
                self.phase = Phase::Idle;
                fx.disarm_timer = true;
                fx.intents.push(
                    if now.saturating_sub(session_start_at) < tuning::INTERRUPTION_WINDOW {
                        HotkeyIntent::AbortAccidental
                    } else {
                        HotkeyIntent::Cancel
                    },
                );
            }
            (Phase::PendingSecondTap { .. }, E::HotkeyUp) => self.swallow_next_up = false,
            // Key not held — Space types normally.
            (Phase::PendingSecondTap { .. }, E::SpaceLock) => {}

            // ----- locked (hands-free)
            (Phase::Locked, E::HotkeyDown) => {
                self.phase = Phase::Idle;
                self.swallow_next_up = true;
                fx.intents.push(HotkeyIntent::Finalize);
            }
            (Phase::Locked, E::EscDown) => {
                self.phase = Phase::Idle;
                fx.intents.push(HotkeyIntent::Cancel);
            }
            (Phase::Locked, E::HotkeyUp) => self.swallow_next_up = false,
            (Phase::Locked, E::OtherKeyDown | E::DoubleTapTimeout | E::SpaceLock) => {}
        }

        fx
    }
}

#[cfg(test)]
mod tests {
    use super::HotkeyEvent as E;
    use super::HotkeyIntent as I;
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn hold_then_release_is_push_to_talk() {
        let mut p = HotkeyProcessor::new();
        assert_eq!(p.handle(E::HotkeyDown, ms(0)).intents, vec![I::Begin]);
        assert_eq!(p.handle(E::HotkeyUp, ms(900)).intents, vec![I::Finalize]);
        assert!(!p.is_session_active());
    }

    #[test]
    fn short_tap_coaches_when_double_tap_lock_is_off() {
        let mut p = HotkeyProcessor::new();
        p.handle(E::HotkeyDown, ms(0));
        let fx = p.handle(E::HotkeyUp, ms(100));
        assert_eq!(fx.intents, vec![I::ShortTapHint]);
        assert!(fx.arm_timer.is_none());
    }

    #[test]
    fn double_tap_locks_when_enabled() {
        let mut p = HotkeyProcessor::new();
        p.double_tap_lock_enabled = true;
        p.handle(E::HotkeyDown, ms(0));
        let fx = p.handle(E::HotkeyUp, ms(100));
        assert_eq!(fx.arm_timer, Some(tuning::DOUBLE_TAP_WINDOW));
        assert!(fx.intents.is_empty());
        let fx = p.handle(E::HotkeyDown, ms(300));
        assert_eq!(fx.intents, vec![I::LockIn]);
        assert!(fx.disarm_timer);
        // The release belonging to the lock gesture is swallowed.
        assert!(p.handle(E::HotkeyUp, ms(360)).intents.is_empty());
        // Pressing again finalizes.
        assert_eq!(p.handle(E::HotkeyDown, ms(5000)).intents, vec![I::Finalize]);
    }

    #[test]
    fn expired_double_tap_window_coaches() {
        let mut p = HotkeyProcessor::new();
        p.double_tap_lock_enabled = true;
        p.handle(E::HotkeyDown, ms(0));
        p.handle(E::HotkeyUp, ms(100));
        assert_eq!(
            p.handle(E::DoubleTapTimeout, ms(600)).intents,
            vec![I::ShortTapHint]
        );
    }

    #[test]
    fn space_while_holding_locks_and_swallows_the_release() {
        let mut p = HotkeyProcessor::new();
        p.handle(E::HotkeyDown, ms(0));
        assert_eq!(p.handle(E::SpaceLock, ms(120)).intents, vec![I::LockIn]);
        assert!(p.handle(E::HotkeyUp, ms(200)).intents.is_empty());
        assert!(p.is_session_active());
        assert_eq!(p.handle(E::HotkeyDown, ms(4000)).intents, vec![I::Finalize]);
    }

    #[test]
    fn other_key_inside_the_window_aborts_but_outside_it_does_not() {
        let mut p = HotkeyProcessor::new();
        p.handle(E::HotkeyDown, ms(0));
        assert_eq!(
            p.handle(E::OtherKeyDown, ms(500)).intents,
            vec![I::AbortAccidental]
        );

        let mut p = HotkeyProcessor::new();
        p.handle(E::HotkeyDown, ms(0));
        assert!(p.handle(E::OtherKeyDown, ms(1500)).intents.is_empty());
        assert!(p.is_session_active());
    }

    #[test]
    fn esc_cancels_and_swallows_the_trailing_release() {
        let mut p = HotkeyProcessor::new();
        p.handle(E::HotkeyDown, ms(0));
        assert_eq!(p.handle(E::EscDown, ms(200)).intents, vec![I::Cancel]);
        assert!(p.handle(E::HotkeyUp, ms(250)).intents.is_empty());
    }

    #[test]
    fn reset_unsticks_a_locked_phantom_session() {
        let mut p = HotkeyProcessor::new();
        p.handle(E::HotkeyDown, ms(0));
        p.handle(E::SpaceLock, ms(50));
        p.reset();
        assert!(!p.is_session_active());
        assert_eq!(p.handle(E::HotkeyDown, ms(100)).intents, vec![I::Begin]);
    }

    #[test]
    fn key_codes_round_trip() {
        for key in HotkeyKey::ALL {
            assert_eq!(HotkeyKey::from_vk(key.vk_code()), Some(key));
        }
        assert_eq!(HotkeyKey::from_vk(0x41), None);
    }
}
