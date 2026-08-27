<p align="center">
  <img src="./assets/readme/hero.svg" width="100%"
       alt="Jot for Windows — hold a key, speak, and the text lands at your cursor. Saying &quot;let's meet at 1pm — actually, no, make it 2pm&quot; types &quot;Let's meet at 2 PM.&quot;">
</p>

<p align="center">
  <img alt="Platform: Windows 10 and 11" src="https://img.shields.io/badge/Windows-10%20%7C%2011-0078D4?style=flat-square&labelColor=1E1F20">
  <img alt="Built with Rust and GPUI" src="https://img.shields.io/badge/Rust%20%2B%20GPUI-1.97-CE422B?style=flat-square&labelColor=1E1F20">
  <img alt="198 tests" src="https://img.shields.io/badge/tests-198-34A853?style=flat-square&labelColor=1E1F20">
  <img alt="Apache 2.0 licensed" src="https://img.shields.io/badge/license-Apache--2.0-9AA0A6?style=flat-square&labelColor=1E1F20">
</p>

> A Windows port of [Jot](https://github.com/google-gemini/jot-gemini-transcribe-macOS) by Ammaar Reshi,
> rewritten in Rust on [GPUI](https://www.gpui.rs). Not an officially supported Google product.

Hold the dictation key, say the thing, let go. A moment later your words are in the app you were
already using — punctuated, filler words removed, cleaned up. No window to switch to, no transcript
to copy, no account to make.

## Try it

```powershell
git clone https://github.com/ubranch/jot-gemini-transcribe-Windows
cd jot-gemini-transcribe-Windows
cargo run -p jot
```

Setup takes about two minutes: paste a [Gemini API key](https://aistudio.google.com/apikey), let it
check your microphone, then hold **Right Ctrl** anywhere you can type.

You pay Google for what you dictate at [Gemini API pricing](https://ai.google.dev/pricing) — a free
tier exists and a typical dictation is a few seconds of audio. Jot itself is free and has no account.

## Three gestures

| Gesture | What happens |
| --- | --- |
| **Hold the key** | Records while held. Release and the text lands at your cursor. |
| **Hold it, tap `Space`** | Hands-free. Press the key again to finish. |
| **`Esc`** | Cancels. Anything over 10 seconds is still kept in History. |

Windows has no `fn` key — it is handled in keyboard firmware and never reaches a low-level hook — so
the default is **Right Ctrl**. Right Alt, Right Shift, Right Win and Caps Lock are also available.
Caps Lock and Right Win are swallowed while bound, so neither toggles shift-lock nor opens the Start
menu mid-dictation.

## Why it is different

**It follows a change of mind.** That is the whole pitch, and setup makes you do it once so you
believe it.

**It never loses your words.** Audio goes to disk from the first millisecond and the WAV header is
rewritten every second, so a crash, a `taskkill`, or a flat battery costs you nothing — the recording
is recovered on next launch. Offline, dictations queue and land when you reconnect. Release the key
mid-word and it keeps listening until you actually stop.

**It is private by architecture.** Your voice goes from your PC straight to the Gemini API with
*your* key, which lives in Windows Credential Manager and never in a settings file. No middleman
server, no account, no analytics, no keystroke logging — one network host, and you can read every
line of the code that talks to it.

**Your jargon, spelled right.** Names and product terms go in the Dictionary and ride along with the
audio, so the model hears "Kubernetes" instead of guessing "cooper netties" — corrected at the
source, not patched afterwards.

<p align="center">
  <img src="./assets/readme/pipeline.svg" width="100%"
       alt="Key down writes audio.wav to disk immediately; key up sends it to gemini-3.5-transcribe; a validation gate catches a model that answered instead of transcribing; insertion tries typing into a confirmed text field, then a guarded paste, then leaves it on the clipboard. Every outcome lands in History.">
</p>

## Settings

<p align="center">
  <img src="./assets/readme/settings.png" width="660"
       alt="The Settings window: dictation key picker, microphone picker, and switches for the resting indicator, start with Windows, sounds and double-tap lock.">
</p>

Pick the key, pick the microphone — a device you choose is never silently swapped for another — and
decide whether Jot starts with Windows. Recordings are kept for 7 days by default, or never, or
forever; transcripts stay until you delete them.

## Ported, not translated

Each of these is a platform constraint, not a shortcut:

| macOS | Windows | Why |
| --- | --- | --- |
| `fn` key | Right Ctrl | `fn` never reaches a Windows keyboard hook. |
| Hover-to-dictate dot, in-pill Stop | Informational pill; stop from the key or the tray | The pill is click-through and never activates. The alternative eats clicks across the bottom of every screen, or steals focus from the app the text is about to land in. |
| Liquid Glass pill | Solid elevated surface | Mica is an app-window material; it would tint the whole transparent rect. Nothing here pretends to be glass. |
| Accessibility API insertion | Typing into a UIA-confirmed control | Windows has no non-destructive insert-at-selection call — `SetValue` replaces a control's entire contents. |
| CAF + FLAC upload | WAV upload | One fewer encode on the latency path. |
| Keychain | Credential Manager | Same role, same guarantee. |
| Secure Input | UIA `IsPassword` + the secure desktop | Same rule: never record over, never paste into, a password field. |

Known limits, stated rather than hidden: the text fields support IME composition but are not a full
editor (no undo, no click-to-position caret); clipboard images round-trip but GDI metafiles do not;
and there is no auto-update server, so new versions are a manual download.

## Development

Requires Windows 10 or 11 and the toolchain pinned in `rust-toolchain.toml`.

```powershell
cargo test                  # 198 tests, headless
cargo clippy --all-targets  # clean
./scripts/package.ps1       # release build, staged folder and zip
```

`package.ps1` runs the format, lint and test gates before it builds, and warns loudly when it
produces an unsigned binary — pass `-CertificateThumbprint` to sign. Jot is portable: no installer,
data in `%LOCALAPPDATA%\Jot`, and it adds itself to startup only when you turn that on.

```text
crates/jot-core/    the engine, headless and testable
  hotkey.rs           the low-level-hook key set and the pure hold/lock/cancel grammar
  audio.rs            crash-safe WASAPI capture, device changes, resampling
  gemini.rs           the API client: auth, deadlines, status mapping
  transcription.rs    the pipeline, retries, the vocabulary fail-open
  validation.rs       the "never insert garbage" gate
  insertion.rs        the type → paste → clipboard ladder
  history.rs          the SQLite index and retention
  recovery.rs         the offline queue and launch-time crash recovery
  coordinator.rs      the brain
  win32.rs            foreground app, focus kind, clipboard, synthetic input
crates/jot/         the GPUI app
  pill.rs             the HUD and its waveform
  text_field.rs       the single-line field, including IME composition
  window_shell.rs     overlay flags, DPI-correct placement, Windows 11 chrome
  autostart.rs        the per-user Run key
```

Logging is lifecycle only — transcript text never appears in it — and rolls daily into
`%LOCALAPPDATA%\Jot\logs`:

```powershell
$env:JOT_LOG = "jot=debug,jot_core=debug"; cargo run -p jot
```

## License

Apache 2.0 — see [LICENSE](LICENSE). Bundled fonts (Google Sans Flex, Google Sans Code) are
SIL OFL 1.1, and the earcons are original works under the same Apache 2.0 licence. Details in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
