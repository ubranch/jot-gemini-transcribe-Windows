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

//! The common file dialogs, for importing and exporting the Dictionary.
//!
//! These block the calling thread while the user decides, so they run on the
//! shared input thread rather than the one painting the UI.

use std::path::PathBuf;

/// A file type offered in the dialog: what the user sees, and the pattern.
pub struct Filter {
    pub label: &'static str,
    pub pattern: &'static str,
}

pub const CSV: Filter = Filter {
    label: "Comma-separated values (*.csv)",
    pattern: "*.csv",
};

/// Asks for a file to read. `None` means the user cancelled.
pub fn open(title: &str, filter: Filter) -> Option<PathBuf> {
    let title = title.to_string();
    jot_core::win32::on_input_thread(move || platform::pick(&title, filter, false, ""))
}

/// Asks where to write. `None` means the user cancelled.
pub fn save(title: &str, filter: Filter, suggested_name: &str) -> Option<PathBuf> {
    let title = title.to_string();
    let suggested = suggested_name.to_string();
    jot_core::win32::on_input_thread(move || platform::pick(&title, filter, true, &suggested))
}

#[cfg(target_os = "windows")]
mod platform {
    use super::Filter;
    use std::path::PathBuf;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{
        FILEOPENDIALOGOPTIONS, FileOpenDialog, FileSaveDialog, IFileDialog, SIGDN_FILESYSPATH,
    };
    use windows::core::PCWSTR;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn pick(title: &str, filter: Filter, saving: bool, suggested: &str) -> Option<PathBuf> {
        unsafe {
            let dialog: IFileDialog = if saving {
                CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?
            } else {
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?
            };

            let title = wide(title);
            let _ = dialog.SetTitle(PCWSTR(title.as_ptr()));

            let label = wide(filter.label);
            let pattern = wide(filter.pattern);
            let specs = [COMDLG_FILTERSPEC {
                pszName: PCWSTR(label.as_ptr()),
                pszSpec: PCWSTR(pattern.as_ptr()),
            }];
            let _ = dialog.SetFileTypes(&specs);
            // So a typed name without an extension still lands as .csv.
            let extension = wide(filter.pattern.trim_start_matches("*."));
            let _ = dialog.SetDefaultExtension(PCWSTR(extension.as_ptr()));

            if !suggested.is_empty() {
                let suggested = wide(suggested);
                let _ = dialog.SetFileName(PCWSTR(suggested.as_ptr()));
            }
            if let Ok(options) = dialog.GetOptions() {
                // FOS_FORCEFILESYSTEM: never hand back a shell item that has no
                // path on disk, because the caller can only read files.
                let _ = dialog.SetOptions(options | FILEOPENDIALOGOPTIONS(0x40));
            }

            // A cancelled dialog returns an error; that is not a failure worth
            // reporting to the user.
            dialog.Show(None).ok()?;
            let item = dialog.GetResult().ok()?;
            let path = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let text = path.to_string().ok();
            windows::Win32::System::Com::CoTaskMemFree(Some(path.0 as *const _));
            text.map(PathBuf::from)
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use super::Filter;
    use std::path::PathBuf;

    pub fn pick(_title: &str, _filter: Filter, _saving: bool, _suggested: &str) -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_extension_falls_out_of_the_pattern() {
        assert_eq!(CSV.pattern.trim_start_matches("*."), "csv");
    }
}
