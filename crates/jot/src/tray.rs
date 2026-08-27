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

//! The notification-area icon — Jot's counterpart to the macOS menu bar item.
//!
//! It owns a hidden window on its own thread with its own message loop, because
//! `Shell_NotifyIcon` delivers its callbacks as window messages. That keeps it
//! entirely independent of GPUI's loop: neither can stall the other, and the
//! tray keeps working while every Jot window is closed.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

pub const ICON: &[u8] = include_bytes!("../assets/jot.ico");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// Start or stop a hands-free dictation from the menu, for anyone who
    /// cannot hold the key.
    ToggleDictation,
    OpenHistory,
    OpenDictionary,
    OpenSettings,
    OpenOnboarding,
    About,
    Quit,
}

impl TrayCommand {
    fn from_id(id: u32) -> Option<Self> {
        Some(match id {
            1 => TrayCommand::ToggleDictation,
            2 => TrayCommand::OpenHistory,
            3 => TrayCommand::OpenDictionary,
            4 => TrayCommand::OpenSettings,
            5 => TrayCommand::OpenOnboarding,
            6 => TrayCommand::About,
            7 => TrayCommand::Quit,
            _ => return None,
        })
    }

    fn id(self) -> u32 {
        match self {
            TrayCommand::ToggleDictation => 1,
            TrayCommand::OpenHistory => 2,
            TrayCommand::OpenDictionary => 3,
            TrayCommand::OpenSettings => 4,
            TrayCommand::OpenOnboarding => 5,
            TrayCommand::About => 6,
            TrayCommand::Quit => 7,
        }
    }

    fn label(self, dictating: bool) -> &'static str {
        match self {
            TrayCommand::ToggleDictation => {
                if dictating {
                    "Stop dictation"
                } else {
                    "Start dictation"
                }
            }
            TrayCommand::OpenHistory => "History…",
            TrayCommand::OpenDictionary => "Dictionary…",
            TrayCommand::OpenSettings => "Settings…",
            TrayCommand::OpenOnboarding => "Setup guide…",
            TrayCommand::About => "About Jot",
            TrayCommand::Quit => "Quit Jot",
        }
    }
}

const MENU_ORDER: [TrayCommand; 7] = [
    TrayCommand::ToggleDictation,
    TrayCommand::OpenHistory,
    TrayCommand::OpenDictionary,
    TrayCommand::OpenSettings,
    TrayCommand::OpenOnboarding,
    TrayCommand::About,
    TrayCommand::Quit,
];

/// Separators go after the dictation control and before Quit.
fn separator_after(command: TrayCommand) -> bool {
    matches!(command, TrayCommand::ToggleDictation | TrayCommand::About)
}

static COMMANDS: OnceLock<UnboundedSender<TrayCommand>> = OnceLock::new();
/// Mirrors the session state so the menu can say "Stop" while a dictation runs.
/// Read inside the message loop, which cannot ask anyone else.
static DICTATING: AtomicU32 = AtomicU32::new(0);

pub fn set_dictating(dictating: bool) {
    DICTATING.store(dictating as u32, Ordering::SeqCst);
}

fn is_dictating() -> bool {
    DICTATING.load(Ordering::SeqCst) != 0
}

/// Installs the tray icon and returns the command stream.
pub fn install() -> UnboundedReceiver<TrayCommand> {
    let (tx, rx) = unbounded_channel();
    let _ = COMMANDS.set(tx);
    std::thread::Builder::new()
        .name("jot-tray".into())
        .spawn(platform::run_tray_thread)
        .expect("spawning the tray thread");
    rx
}

fn emit(command: TrayCommand) {
    if let Some(commands) = COMMANDS.get() {
        let _ = commands.send(command);
    }
}

/// Picks the image inside an `.ico` closest to `wanted` pixels wide.
///
/// A width byte of 0 means 256, which is why the largest entry in a modern icon
/// file reads as the smallest if you take the byte at face value.
pub fn icon_entry(ico: &[u8], wanted: u32) -> Option<&[u8]> {
    const HEADER: usize = 6;
    const ENTRY: usize = 16;
    if ico.len() < HEADER || ico[0..4] != [0, 0, 1, 0] {
        return None;
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let mut best: Option<(u32, usize, usize)> = None;
    for index in 0..count {
        let at = HEADER + index * ENTRY;
        if at + ENTRY > ico.len() {
            break;
        }
        let width = match ico[at] {
            0 => 256,
            width => width as u32,
        };
        let size = u32::from_le_bytes(ico[at + 8..at + 12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(ico[at + 12..at + 16].try_into().ok()?) as usize;
        if offset + size > ico.len() || size == 0 {
            continue;
        }
        let distance = width.abs_diff(wanted);
        if best.is_none_or(|(best_distance, _, _)| distance < best_distance) {
            best = Some((distance, offset, size));
        }
    }
    best.map(|(_, offset, size)| &ico[offset..offset + size])
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, TRUE, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, HICON,
        LR_DEFAULTCOLOR, MF_SEPARATOR, MF_STRING, MSG, PostQuitMessage, RegisterClassW,
        SM_CXSMICON, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RIGHTALIGN, TPM_RIGHTBUTTON,
        TrackPopupMenu, WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_DESTROY, WM_LBUTTONUP,
        WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
    };
    use windows::core::{PCWSTR, w};

    /// Our private message id for the notification-area callback.
    const WM_TRAY: u32 = WM_APP + 1;
    const ICON_ID: u32 = 1;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn run_tray_thread() {
        let Ok(instance) = (unsafe { GetModuleHandleW(None) }) else {
            tracing::error!("no module handle — the tray icon is unavailable");
            return;
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: w!("JotTrayHost"),
            ..Default::default()
        };
        unsafe { RegisterClassW(&class) };

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("JotTrayHost"),
                w!("Jot"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                HINSTANCE(instance.0),
                None,
            )
        };
        let Ok(hwnd) = hwnd else {
            tracing::error!("could not create the tray host window");
            return;
        };

        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: ICON_ID,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: load_icon(),
            ..Default::default()
        };
        let tip = wide("Jot — hold the dictation key and speak");
        data.szTip[..tip.len()].copy_from_slice(&tip);
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            tracing::error!("the notification area rejected the icon");
        }

        let mut message = MSG::default();
        unsafe {
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                DispatchMessageW(&message);
            }
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }

    /// The `.ico` is embedded, so the icon exists before the app has a
    /// resource file, an installer, or a settings directory.
    ///
    /// `CreateIconFromResourceEx` wants ONE image, not a whole icon file, so
    /// the directory is walked here and the entry closest to the notification
    /// area's icon size is handed over. Picking the 256-pixel entry and letting
    /// Windows downscale it is what makes a tray icon look muddy.
    fn load_icon() -> HICON {
        let wanted = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
        let Some(image) = super::icon_entry(ICON, wanted) else {
            tracing::error!("the embedded icon has no usable image");
            return HICON::default();
        };
        unsafe {
            CreateIconFromResourceEx(
                image,
                TRUE,
                // The version every icon file since Windows 3.0 declares.
                0x0003_0000,
                wanted as i32,
                wanted as i32,
                LR_DEFAULTCOLOR,
            )
        }
        .unwrap_or_default()
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_TRAY => {
                let event = lparam.0 as u32;
                match event {
                    // A left click is the fast path to the thing people open
                    // most; the full menu is on the right button.
                    WM_LBUTTONUP => emit(TrayCommand::OpenHistory),
                    WM_RBUTTONUP => unsafe { show_menu(hwnd) },
                    _ => {}
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                if let Some(command) = TrayCommand::from_id((wparam.0 & 0xFFFF) as u32) {
                    emit(command);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    unsafe fn show_menu(hwnd: HWND) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let dictating = is_dictating();
            for command in MENU_ORDER {
                let label = wide(command.label(dictating));
                let _ = AppendMenuW(
                    menu,
                    MF_STRING,
                    command.id() as usize,
                    PCWSTR(label.as_ptr()),
                );
                if separator_after(command) {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                }
            }
            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            // Documented dance: without foreground ownership the menu does not
            // dismiss when the user clicks elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let _ = TrackPopupMenu(
                menu,
                TPM_RIGHTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    pub fn run_tray_thread() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_ids_round_trip_and_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for command in MENU_ORDER {
            assert!(seen.insert(command.id()), "duplicate id for {command:?}");
            assert_eq!(TrayCommand::from_id(command.id()), Some(command));
        }
        // Id 0 is what `TrackPopupMenu` reports when nothing was chosen.
        assert_eq!(TrayCommand::from_id(0), None);
    }

    #[test]
    fn the_dictation_item_names_what_it_will_do() {
        assert_eq!(TrayCommand::ToggleDictation.label(false), "Start dictation");
        assert_eq!(TrayCommand::ToggleDictation.label(true), "Stop dictation");
    }

    #[test]
    fn quit_is_separated_from_the_rest_of_the_menu() {
        let last = MENU_ORDER[MENU_ORDER.len() - 1];
        assert_eq!(last, TrayCommand::Quit);
        assert!(separator_after(MENU_ORDER[MENU_ORDER.len() - 2]));
    }

    #[test]
    fn the_embedded_icon_is_a_real_ico_with_several_sizes() {
        assert_eq!(&ICON[..4], &[0, 0, 1, 0], "not an ICO header");
        let entries = u16::from_le_bytes([ICON[4], ICON[5]]);
        assert!(entries > 1, "tray icons need small sizes, got {entries}");
    }
}
