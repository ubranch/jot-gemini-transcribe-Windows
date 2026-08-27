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

//! The Win32 bridge: foreground app identity, focused-element inspection,
//! clipboard round-tripping, and synthetic keystrokes.
//!
//! Every entry point here runs on one dedicated thread. Clipboard ownership and
//! UI Automation both care which thread they were called from, and COM only has
//! to be initialised once if there is only one caller.

use crate::transcription::DictationContext;
use parking_lot::Mutex;
use std::sync::LazyLock;
use std::sync::mpsc::{Sender, channel};

/// Stamped into every synthetic event Jot posts, so the low-level keyboard hook
/// can tell its own paste keystrokes from the user's typing. Without it the
/// hook reads our Ctrl+V as an accidental chord and cancels the next dictation.
pub const SYNTHETIC_TAG: usize = 0x4A_4F_54_00; // "JOT\0"

/// Text longer than this never goes through synthetic Unicode typing: it is two
/// input events per character, and applications that coalesce or drop fast
/// synthetic input turn a long transcript into a half-typed one.
pub const MAX_TYPED_CHARS: usize = 240;

/// What the focused UI element is, as far as UI Automation can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusKind {
    /// A real text control that will accept typed characters.
    EditableText,
    /// A password box, or the secure desktop. Never type or paste here.
    Password,
    /// Focus is somewhere that will not take text, or UI Automation could not
    /// answer at all.
    Unknown,
}

/// Who owns the foreground window right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForegroundApp {
    pub pid: Option<u32>,
    /// Process image name, e.g. `Code.exe`.
    pub exe: Option<String>,
    /// Window title, which is the closest thing Windows has to an app name a
    /// user recognises in a History row.
    pub name: Option<String>,
}

impl ForegroundApp {
    pub fn as_context(&self) -> DictationContext {
        DictationContext {
            target_app_exe: self.exe.clone(),
            target_app_name: self.name.clone().or_else(|| self.exe.clone()),
            target_pid: self.pid,
        }
    }
}

// ---------------------------------------------------------------------------
// The dedicated input thread
// ---------------------------------------------------------------------------

type Job = Box<dyn FnOnce() + Send>;

static INPUT_THREAD: LazyLock<Mutex<Sender<Job>>> = LazyLock::new(|| {
    let (tx, rx) = channel::<Job>();
    std::thread::Builder::new()
        .name("jot-input".into())
        .spawn(move || {
            platform::init_com();
            while let Ok(job) = rx.recv() {
                job();
            }
        })
        .expect("spawning the input thread");
    Mutex::new(tx)
});

/// Runs `job` on the input thread and waits for its result.
pub fn on_input_thread<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = channel();
    INPUT_THREAD
        .lock()
        .send(Box::new(move || {
            let _ = tx.send(job());
        }))
        .expect("the input thread outlives the process");
    rx.recv().expect("the input thread never drops a job")
}

// ---------------------------------------------------------------------------
// Public API — thin wrappers that hop onto the input thread
// ---------------------------------------------------------------------------

pub fn foreground_app() -> ForegroundApp {
    on_input_thread(platform::foreground_app)
}

pub fn focus_kind() -> FocusKind {
    on_input_thread(platform::focus_kind)
}

/// The process id owning the focused element, when UI Automation can say.
pub fn focused_process_id() -> Option<u32> {
    on_input_thread(platform::focused_process_id)
}

/// Types `text` as Unicode key events. Returns false when the events could not
/// be posted at all — never a promise that the target app accepted them.
pub fn type_text(text: &str) -> bool {
    let text = text.to_string();
    on_input_thread(move || platform::type_text(&text))
}

/// Posts Ctrl+V. Returns false when the events could not be posted.
pub fn post_paste() -> bool {
    on_input_thread(platform::post_paste)
}

/// Every clipboard format Jot could put back, captured before it overwrites the
/// clipboard with a transcript.
#[derive(Debug, Default)]
pub struct ClipboardSnapshot {
    entries: Vec<(u32, Vec<u8>)>,
}

impl ClipboardSnapshot {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub fn snapshot_clipboard() -> ClipboardSnapshot {
    on_input_thread(platform::snapshot_clipboard)
}

pub fn restore_clipboard(snapshot: ClipboardSnapshot) {
    on_input_thread(move || platform::restore_clipboard(&snapshot));
}

/// Writes `text`, tagged so Windows clipboard history and cloud sync skip it,
/// and returns the clipboard sequence number our write produced.
pub fn write_clipboard(text: &str) -> Option<u32> {
    let text = text.to_string();
    on_input_thread(move || platform::write_clipboard(&text))
}

pub fn clipboard_sequence_number() -> u32 {
    on_input_thread(platform::clipboard_sequence_number)
}

// ---------------------------------------------------------------------------
// Platform implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod platform {
    use super::{ClipboardSnapshot, FocusKind, ForegroundApp, MAX_TYPED_CHARS, SYNTHETIC_TAG};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
        GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, UIA_DocumentControlTypeId, UIA_EditControlTypeId,
        UIA_TextPatternId,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyboardLayout, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        SendInput, VIRTUAL_KEY, VK_CONTROL, VkKeyScanExW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    };
    use windows::core::{Interface, PCWSTR, PWSTR};

    /// Clipboard formats that ask Windows to keep a payload out of clipboard
    /// history (Win+V) and out of the cloud clipboard. These are the direct
    /// equivalent of marking a pasteboard item transient: a dictated transcript
    /// must not silently accumulate in a system-wide history.
    const HISTORY_OPT_OUT: [&str; 2] =
        ["CanIncludeInClipboardHistory", "CanUploadToCloudClipboard"];

    /// Clipboard formats that are backed by a GDI handle rather than an
    /// HGLOBAL, so their bytes cannot be copied and restored directly.
    ///
    /// Images are NOT in this list: `CF_DIB` and `CF_DIBV5` are HGLOBAL, and
    /// every application that puts a picture on the clipboard offers at least
    /// one of them alongside `CF_BITMAP`. Excluding them is what made a copied
    /// screenshot disappear after a dictation.
    const HANDLE_BACKED_FORMATS: [u32; 4] = [
        2,  // CF_BITMAP    — an HBITMAP
        3,  // CF_METAFILEPICT
        9,  // CF_PALETTE   — an HPALETTE
        14, // CF_ENHMETAFILE
    ];

    pub fn init_com() {
        // UI Automation is an apartment-threaded COM server. Failure here is not
        // fatal: the focus checks degrade to Unknown and the ladder still works.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
    }

    fn automation() -> Option<IUIAutomation> {
        thread_local! {
            static AUTOMATION: Option<IUIAutomation> =
                unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok() };
        }
        AUTOMATION.with(|automation| automation.clone())
    }

    pub fn foreground_app() -> ForegroundApp {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                // No foreground window at all: the secure desktop is up (UAC),
                // or the shell is between windows.
                return ForegroundApp::default();
            }
            let mut pid = 0_u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            ForegroundApp {
                pid: (pid != 0).then_some(pid),
                exe: process_image_name(pid),
                name: window_title(hwnd),
            }
        }
    }

    unsafe fn window_title(hwnd: HWND) -> Option<String> {
        let mut buffer = [0_u16; 256];
        let length = unsafe { GetWindowTextW(hwnd, &mut buffer) };
        (length > 0).then(|| String::from_utf16_lossy(&buffer[..length as usize]))
    }

    fn process_image_name(pid: u32) -> Option<String> {
        if pid == 0 {
            return None;
        }
        unsafe {
            let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buffer = [0_u16; 512];
            let mut length = buffer.len() as u32;
            let queried = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            );
            let _ = CloseHandle(handle);
            queried.ok()?;
            let full = String::from_utf16_lossy(&buffer[..length as usize]);
            full.rsplit('\\').next().map(str::to_string)
        }
    }

    pub fn focus_kind() -> FocusKind {
        let Some(automation) = automation() else {
            return FocusKind::Unknown;
        };
        unsafe {
            // With the secure desktop up there is no focused element to fetch,
            // which is exactly the case that must never be typed into.
            let Ok(element) = automation.GetFocusedElement() else {
                return if GetForegroundWindow().0.is_null() {
                    FocusKind::Password
                } else {
                    FocusKind::Unknown
                };
            };
            if element.CurrentIsPassword().is_ok_and(|is| is.as_bool()) {
                return FocusKind::Password;
            }
            let control_type = element.CurrentControlType().unwrap_or_default();
            if control_type == UIA_EditControlTypeId || control_type == UIA_DocumentControlTypeId {
                return FocusKind::EditableText;
            }
            // A control that exposes TextPattern takes typed characters even
            // when its control type is something bespoke (terminals, editors).
            if element.GetCurrentPattern(UIA_TextPatternId).is_ok() {
                return FocusKind::EditableText;
            }
            FocusKind::Unknown
        }
    }

    pub fn focused_process_id() -> Option<u32> {
        let automation = automation()?;
        unsafe {
            let element = automation.GetFocusedElement().ok()?;
            element.CurrentProcessId().ok().map(|pid| pid as u32)
        }
    }

    fn key_event(vk: VIRTUAL_KEY, scan: u16, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(flags),
                    time: 0,
                    dwExtraInfo: SYNTHETIC_TAG,
                },
            },
        }
    }

    fn send(inputs: &[INPUT]) -> bool {
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        sent as usize == inputs.len()
    }

    pub fn type_text(text: &str) -> bool {
        if text.chars().count() > MAX_TYPED_CHARS {
            return false;
        }
        let mut inputs = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            inputs.push(key_event(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE.0));
            inputs.push(key_event(
                VIRTUAL_KEY(0),
                unit,
                KEYEVENTF_UNICODE.0 | KEYEVENTF_KEYUP.0,
            ));
        }
        if inputs.is_empty() {
            return true;
        }
        send(&inputs)
    }

    pub fn post_paste() -> bool {
        // Layout-correct 'V': on a Dvorak or AZERTY layout the physical key that
        // produces 'v' is not the one VK_V names.
        let layout = unsafe { GetKeyboardLayout(0) };
        let scan = unsafe { VkKeyScanExW('v' as u16, layout) };
        if scan == -1 {
            return false;
        }
        let vk = VIRTUAL_KEY((scan as u16) & 0x00FF);
        let inputs = [
            key_event(VK_CONTROL, 0, 0),
            key_event(vk, 0, 0),
            key_event(vk, 0, KEYEVENTF_KEYUP.0),
            key_event(VK_CONTROL, 0, KEYEVENTF_KEYUP.0),
        ];
        send(&inputs)
    }

    struct ClipboardGuard;

    impl ClipboardGuard {
        /// The clipboard is a global lock other processes are also fighting for;
        /// one retry pass covers the usual "a clipboard manager got there first".
        fn open() -> Option<Self> {
            for _ in 0..5 {
                if unsafe { OpenClipboard(None) }.is_ok() {
                    return Some(Self);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            None
        }
    }

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    pub fn clipboard_sequence_number() -> u32 {
        unsafe { GetClipboardSequenceNumber() }
    }

    pub fn snapshot_clipboard() -> ClipboardSnapshot {
        let mut snapshot = ClipboardSnapshot::default();
        let Some(_guard) = ClipboardGuard::open() else {
            return snapshot;
        };
        unsafe {
            let mut format = EnumClipboardFormats(0);
            while format != 0 {
                if !HANDLE_BACKED_FORMATS.contains(&format)
                    && let Some(bytes) = read_format(format)
                {
                    snapshot.entries.push((format, bytes));
                }
                format = EnumClipboardFormats(format);
            }
        }
        snapshot
    }

    unsafe fn read_format(format: u32) -> Option<Vec<u8>> {
        unsafe {
            let handle = GetClipboardData(format).ok()?;
            let global = HGLOBAL(handle.0);
            let size = GlobalSize(global);
            if size == 0 {
                return None;
            }
            let pointer = GlobalLock(global);
            if pointer.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(pointer as *const u8, size).to_vec();
            let _ = GlobalUnlock(global);
            Some(bytes)
        }
    }

    unsafe fn write_format(format: u32, bytes: &[u8]) -> bool {
        unsafe {
            let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else {
                return false;
            };
            let pointer = GlobalLock(global);
            if pointer.is_null() {
                return false;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
            let _ = GlobalUnlock(global);
            // On success the clipboard owns the memory; on failure we still do.
            SetClipboardData(format, HANDLE(global.0)).is_ok()
        }
    }

    pub fn restore_clipboard(snapshot: &ClipboardSnapshot) {
        let Some(_guard) = ClipboardGuard::open() else {
            return;
        };
        unsafe {
            let _ = EmptyClipboard();
            for (format, bytes) in &snapshot.entries {
                write_format(*format, bytes);
            }
        }
    }

    pub fn write_clipboard(text: &str) -> Option<u32> {
        let _guard = ClipboardGuard::open()?;
        let mut utf16: Vec<u16> = text.encode_utf16().collect();
        utf16.push(0);
        let bytes = unsafe {
            std::slice::from_raw_parts(utf16.as_ptr() as *const u8, utf16.len() * 2).to_vec()
        };
        unsafe {
            let _ = EmptyClipboard();
            if !write_format(13 /* CF_UNICODETEXT */, &bytes) {
                return None;
            }
            for name in HISTORY_OPT_OUT {
                let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                let format = RegisterClipboardFormatW(PCWSTR(wide.as_ptr()));
                if format != 0 {
                    write_format(format, &0_u32.to_le_bytes());
                }
            }
            Some(GetClipboardSequenceNumber())
        }
    }

    // Keeps the `Interface` import honest across `windows` versions where
    // `GetCurrentPattern` is only reachable through the trait.
    #[allow(dead_code)]
    fn _assert_interface_in_scope(automation: &IUIAutomation) -> *mut std::ffi::c_void {
        automation.as_raw()
    }
}

/// Non-Windows builds exist only so `cargo check` on a contributor's Mac or
/// Linux box reaches the pure logic. Nothing here does anything.
#[cfg(not(target_os = "windows"))]
mod platform {
    use super::{ClipboardSnapshot, FocusKind, ForegroundApp};

    pub fn init_com() {}
    pub fn foreground_app() -> ForegroundApp {
        ForegroundApp::default()
    }
    pub fn focus_kind() -> FocusKind {
        FocusKind::Unknown
    }
    pub fn focused_process_id() -> Option<u32> {
        None
    }
    pub fn type_text(_text: &str) -> bool {
        false
    }
    pub fn post_paste() -> bool {
        false
    }
    pub fn snapshot_clipboard() -> ClipboardSnapshot {
        ClipboardSnapshot::default()
    }
    pub fn restore_clipboard(_snapshot: &ClipboardSnapshot) {}
    pub fn write_clipboard(_text: &str) -> Option<u32> {
        None
    }
    pub fn clipboard_sequence_number() -> u32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_input_thread_runs_work_and_returns_it() {
        assert_eq!(on_input_thread(|| 2 + 2), 4);
        // The same thread is reused, so a second call must not deadlock.
        assert_eq!(on_input_thread(|| "again"), "again");
    }

    #[test]
    fn foreground_context_prefers_a_window_title_but_falls_back_to_the_exe() {
        let app = ForegroundApp {
            pid: Some(42),
            exe: Some("Code.exe".into()),
            name: None,
        };
        assert_eq!(
            app.as_context().target_app_name.as_deref(),
            Some("Code.exe")
        );

        let named = ForegroundApp {
            name: Some("main.rs — jot".into()),
            ..app
        };
        assert_eq!(
            named.as_context().target_app_name.as_deref(),
            Some("main.rs — jot")
        );
    }

    // Real hardware events arrive with dwExtraInfo of 0; drivers that set it use
    // small values. A four-byte marker keeps our paste distinguishable, and
    // checking it at compile time means it can never silently regress.
    const _TAG_IS_DISTINCTIVE: () = assert!(SYNTHETIC_TAG > 0xFFFF);
}
