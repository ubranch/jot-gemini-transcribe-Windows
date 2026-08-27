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

//! The two things the HUD needs from Win32 that GPUI does not expose.
//!
//! `WindowKind::PopUp` already gives a borderless, topmost, tool window. What it
//! does not give is a window that refuses focus and passes clicks through, and
//! GPUI has no cross-platform way to move a window after it is open. Both are
//! load-bearing here: a HUD that takes focus breaks the very insertion it is
//! announcing, and one that eats clicks makes the bottom of every screen dead.

use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// Where the pill should sit, in screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
}

/// Bottom-centre of the work area, given the monitor rectangle and window size.
///
/// Pure so the arithmetic is testable: an off-by-one here parks the pill under
/// the taskbar or half off the screen.
pub fn bottom_center(
    work_left: i32,
    work_top: i32,
    work_right: i32,
    work_bottom: i32,
    width: i32,
    height: i32,
    bottom_margin: i32,
) -> Placement {
    let center = work_left + (work_right - work_left) / 2;
    Placement {
        x: center - width / 2,
        // Clamped so a monitor shorter than the HUD still shows its top edge.
        y: (work_bottom - height - bottom_margin).max(work_top),
    }
}

/// Is the Windows high-contrast setting on?
///
/// GPUI reports reduced motion but not this, and a theme that ignores it is
/// unreadable for the people who turned it on.
pub fn high_contrast() -> bool {
    platform::high_contrast()
}

pub fn hwnd_of(window: &Window) -> Option<isize> {
    // Fully qualified: GPUI's own `Window::window_handle` returns its
    // `AnyWindowHandle`, which is a different thing entirely.
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

/// Marks a window as a passive overlay — never focused, never clicked — and
/// places it inside the work area of the active display.
///
/// Placement is part of this rather than a later step because the taskbar is
/// topmost as well: a pill positioned against the screen edge is simply drawn
/// behind it, which looks exactly like a pill that never appeared.
pub fn make_overlay(window: &Window, bottom_margin: f32) {
    if let Some(hwnd) = hwnd_of(window) {
        platform::make_overlay(hwnd);
        platform::place_on_active_display(hwnd, bottom_margin);
    }
}

/// Moves the HUD to the bottom centre of the display holding the foreground
/// window — where the text is about to land.
pub fn place_on_active_display(window: &Window, bottom_margin: f32) {
    if let Some(hwnd) = hwnd_of(window) {
        platform::place_on_active_display(hwnd, bottom_margin);
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::bottom_center;
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Dwm::{
        DWMNCRP_DISABLED, DWMWA_BORDER_COLOR, DWMWA_NCRENDERING_POLICY,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
    };
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    };
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, HWND_TOPMOST,
        SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOSIZE, SetWindowLongPtrW, SetWindowPos, ShowWindow,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_EX_WINDOWEDGE,
    };

    /// `DWMWA_COLOR_NONE` — documented, but not exported by the `windows` crate.
    const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;

    pub fn high_contrast() -> bool {
        use windows::Win32::UI::Accessibility::HIGHCONTRASTW;
        use windows::Win32::UI::WindowsAndMessaging::{
            SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
        };
        let mut info = HIGHCONTRASTW {
            cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
            ..Default::default()
        };
        let queried = unsafe {
            SystemParametersInfoW(
                SPI_GETHIGHCONTRAST,
                info.cbSize,
                Some(&mut info as *mut _ as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        // HCF_HIGHCONTRASTON
        queried.is_ok() && info.dwFlags.0 & 0x0000_0001 != 0
    }

    pub fn make_overlay(hwnd: isize) {
        let hwnd = HWND(hwnd as *mut _);
        unsafe {
            let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            let wanted = (current
                // A raised window edge on a transparent overlay is a grey
                // rectangle floating over the desktop.
                & !WS_EX_WINDOWEDGE.0)
                // Never becomes the active window: the app the text is going
                // into must keep focus through the whole dictation.
                | WS_EX_NOACTIVATE.0
                // Clicks pass straight through. The pill is informational on
                // Windows precisely because the alternative is a 600-pixel dead
                // strip across the bottom of every screen.
                | WS_EX_TRANSPARENT.0
                // Keeps it out of Alt+Tab and the taskbar.
                | WS_EX_TOOLWINDOW.0;
            if wanted != current {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, wanted as isize);
            }

            // Windows 11 draws a rounded 1px border around EVERY top-level
            // window, including a borderless transparent one. On the HUD that
            // reads as a grey box around the pill.
            //
            // Three attributes are needed, not one: the corner preference stops
            // the rounding, the border colour removes the left, right and bottom
            // edges, and only disabling non-client rendering removes the top
            // one — DWM draws that edge as caption chrome, which the border
            // colour does not cover.
            let policy = DWMNCRP_DISABLED;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_NCRENDERING_POLICY,
                &policy as *const _ as *const _,
                std::mem::size_of_val(&policy) as u32,
            );
            let corner = DWMWCP_DONOTROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const _ as *const _,
                std::mem::size_of_val(&corner) as u32,
            );
            let none = DWMWA_COLOR_NONE;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR,
                &none as *const _ as *const _,
                std::mem::size_of_val(&none) as u32,
            );
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE_NOSIZE_NOACTIVATE);
            // Explicitly, and without activating. A `WS_EX_NOACTIVATE` popup is
            // left hidden by the normal show path, which makes the pill exist
            // and never appear — the worst possible failure for a HUD.
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }

    const SWP_NOMOVE_NOSIZE_NOACTIVATE:
        windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS =
        windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS(
            0x0002 /* SWP_NOMOVE */ | SWP_NOSIZE.0 | SWP_NOACTIVATE.0,
        );

    pub fn place_on_active_display(hwnd: isize, bottom_margin: f32) {
        let hwnd = HWND(hwnd as *mut _);
        unsafe {
            // The window's OWN rect, in physical pixels. Centring from the
            // logical size instead is wrong by the scale factor: on a 150%
            // display a 600pt pill is 900px wide and lands 150px off centre.
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_err() {
                return;
            }
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            // The margin is a design value in points, so it scales too.
            let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
            let margin = (bottom_margin * scale) as i32;

            // The display hosting the app being dictated into, not the one Jot
            // happened to launch on and not wherever the pointer is parked:
            // keyboard-first users routinely dictate on one screen with the
            // mouse on another.
            let target = GetForegroundWindow();
            let monitor = MonitorFromWindow(
                if target.0.is_null() { hwnd } else { target },
                MONITOR_DEFAULTTONEAREST,
            );
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return;
            }
            let RECT {
                left,
                top,
                right,
                bottom,
            } = info.rcWork;
            let placement = bottom_center(left, top, right, bottom, width, height, margin);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                placement.x,
                placement.y,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    pub fn high_contrast() -> bool {
        false
    }
    pub fn make_overlay(_hwnd: isize) {}
    pub fn place_on_active_display(_hwnd: isize, _margin: f32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pill_centres_horizontally_and_sits_above_the_taskbar() {
        // A 1920x1080 display with a 40px taskbar.
        let placement = bottom_center(0, 0, 1920, 1040, 600, 96, 16);
        assert_eq!(placement.x, 660);
        assert_eq!(placement.y, 1040 - 96 - 16);
    }

    #[test]
    fn a_secondary_display_to_the_left_gets_negative_coordinates() {
        let placement = bottom_center(-1920, 0, 0, 1080, 600, 96, 16);
        assert_eq!(placement.x, -1260);
    }

    #[test]
    fn a_display_shorter_than_the_hud_still_shows_its_top_edge() {
        let placement = bottom_center(0, 0, 800, 60, 600, 96, 16);
        assert_eq!(placement.y, 0);
    }
}
