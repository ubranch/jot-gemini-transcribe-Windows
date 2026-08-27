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

//! Start with Windows.
//!
//! A dictation key that only works after you remember to launch the app gets
//! used once. This writes the per-user `Run` key — no elevation, no scheduled
//! task, and the user can see and remove it in Task Manager → Startup.

/// The value name under `Run`. Also what Task Manager shows the user.
pub const ENTRY_NAME: &str = "Jot";

/// The command line stored in the registry: the current executable, quoted so a
/// path with spaces (`C:\Program Files\...`) is not split into arguments.
pub fn launch_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\"", exe.display()))
}

pub fn is_enabled() -> bool {
    platform::read().is_some()
}

/// True when the stored command points at a different build than the one
/// running — a leftover from a previous install location.
pub fn is_stale() -> bool {
    match (platform::read(), launch_command()) {
        (Some(stored), Some(current)) => !stored.eq_ignore_ascii_case(&current),
        _ => false,
    }
}

pub fn set_enabled(enabled: bool) -> bool {
    match (enabled, launch_command()) {
        (true, Some(command)) => platform::write(&command),
        (true, None) => false,
        (false, _) => platform::remove(),
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::ENTRY_NAME;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey,
        RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
    };
    use windows::core::{PCWSTR, w};

    const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn read() -> Option<String> {
        let name = wide(ENTRY_NAME);
        let mut size = 0_u32;
        unsafe {
            // Ask for the size first: the stored path length is not something
            // to guess at with a fixed buffer.
            RegGetValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut size),
            )
            .ok()
            .ok()?;
            let mut buffer = vec![0_u16; (size as usize / 2) + 1];
            RegGetValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr() as *mut _),
                Some(&mut size),
            )
            .ok()
            .ok()?;
            let text: String = String::from_utf16_lossy(&buffer);
            Some(text.trim_end_matches('\0').to_string())
        }
    }

    fn open(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Option<HKEY> {
        let mut key = HKEY::default();
        unsafe {
            RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, access, &mut key)
                .ok()
                .ok()?;
        }
        Some(key)
    }

    pub fn write(command: &str) -> bool {
        let Some(key) = open(KEY_WRITE | KEY_READ) else {
            return false;
        };
        let name = wide(ENTRY_NAME);
        let value = wide(command);
        let bytes =
            unsafe { std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2) };
        let result = unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(bytes)) };
        unsafe {
            let _ = RegCloseKey(key);
        }
        result.is_ok()
    }

    pub fn remove() -> bool {
        let Some(key) = open(KEY_WRITE | KEY_READ) else {
            return false;
        };
        let name = wide(ENTRY_NAME);
        let result = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
        unsafe {
            let _ = RegCloseKey(key);
        }
        // Already absent is success: the caller asked for "off", and it is off.
        result.is_ok() || result == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    pub fn read() -> Option<String> {
        None
    }
    pub fn write(_command: &str) -> bool {
        false
    }
    pub fn remove() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_quoted_so_program_files_still_works() {
        let command = launch_command().expect("the running exe has a path");
        assert!(
            command.starts_with('"') && command.ends_with('"'),
            "{command}"
        );
        assert!(command.to_lowercase().contains("jot"), "{command}");
    }

    #[test]
    fn a_missing_entry_is_neither_enabled_nor_stale() {
        // Whatever this machine's registry says, these two must agree: nothing
        // stored can never be "stale".
        if !is_enabled() {
            assert!(!is_stale());
        }
    }
}
