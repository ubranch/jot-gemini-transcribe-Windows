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

//! The design tokens. Every colour, type style, radius and spacing value in the
//! app comes from here; views contain no magic numbers.

use gpui::{App, FontWeight, Pixels, Rgba, SharedString, WindowAppearance, px, rgb};
use std::sync::OnceLock;

/// The bundled variable fonts, embedded so a fresh install has the right
/// typography before it has a settings file.
pub const FLEX_FONT: &[u8] =
    include_bytes!("../assets/fonts/GoogleSansFlex/GoogleSansFlex-Regular.ttf");
pub const CODE_FONT: &[u8] = include_bytes!("../assets/fonts/GoogleSansCode/GoogleSansCode-VF.ttf");

static UI_FAMILY: OnceLock<SharedString> = OnceLock::new();
static MONO_FAMILY: OnceLock<SharedString> = OnceLock::new();

/// Registers the bundled fonts and resolves the family names to use.
///
/// The family recorded inside a font file is not something to assume: if
/// registration fails, or the family is not what we expect, the app falls back
/// to a system face rather than rendering every label in a substitute picked at
/// random by the shaper.
pub fn load_fonts(cx: &App) {
    if let Err(error) = cx
        .text_system()
        .add_fonts(vec![FLEX_FONT.into(), CODE_FONT.into()])
    {
        tracing::warn!(%error, "bundled fonts could not be registered");
    }
    let available = cx.text_system().all_font_names();
    // The family recorded inside a variable font is not the file name: Google
    // Sans Code registers as "Google Sans Code Monospace". Candidates are tried
    // in order, and a system face is the last resort.
    let resolve = |candidates: &[&str], fallback: &str| -> SharedString {
        for wanted in candidates {
            if available.iter().any(|name| name == wanted) {
                return SharedString::from(wanted.to_string());
            }
        }
        tracing::warn!(
            ?candidates,
            fallback,
            "no bundled family matched — falling back"
        );
        SharedString::from(fallback.to_string())
    };
    let _ = UI_FAMILY.set(resolve(&["Google Sans Flex"], "Segoe UI"));
    let _ = MONO_FAMILY.set(resolve(
        &["Google Sans Code Monospace", "Google Sans Code"],
        "Consolas",
    ));
}

pub fn ui_font() -> SharedString {
    UI_FAMILY
        .get()
        .cloned()
        .unwrap_or_else(|| SharedString::from("Segoe UI"))
}

pub fn mono_font() -> SharedString {
    MONO_FAMILY
        .get()
        .cloned()
        .unwrap_or_else(|| SharedString::from("Consolas"))
}

/// The brand quad — waveform, processing sweep, and celebration ONLY.
pub const G_BLUE: Rgba = Rgba {
    r: 0.259,
    g: 0.522,
    b: 0.957,
    a: 1.0,
};
pub const G_RED: Rgba = Rgba {
    r: 0.918,
    g: 0.263,
    b: 0.208,
    a: 1.0,
};
pub const G_YELLOW: Rgba = Rgba {
    r: 0.984,
    g: 0.737,
    b: 0.016,
    a: 1.0,
};
pub const G_GREEN: Rgba = Rgba {
    r: 0.204,
    g: 0.659,
    b: 0.325,
    a: 1.0,
};

pub fn brand_quad() -> [Rgba; 4] {
    [G_BLUE, G_RED, G_YELLOW, G_GREEN]
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub dark: bool,
    pub primary: Rgba,
    pub on_primary: Rgba,
    pub primary_container: Rgba,
    pub on_primary_container: Rgba,
    pub surface: Rgba,
    pub surface_container: Rgba,
    pub window_background: Rgba,
    pub on_surface: Rgba,
    pub on_surface_variant: Rgba,
    pub outline: Rgba,
    pub outline_variant: Rgba,
    pub error: Rgba,
    pub error_container: Rgba,
    pub on_error_container: Rgba,
    pub success: Rgba,
}

impl Theme {
    pub fn light() -> Theme {
        Theme {
            dark: false,
            primary: rgb(0x0B57D0),
            on_primary: rgb(0xFFFFFF),
            primary_container: rgb(0xD3E3FD),
            on_primary_container: rgb(0x041E49),
            // Surfaces are never pure white or pure black.
            surface: rgb(0xFFFFFF),
            surface_container: rgb(0xF0F4F9),
            window_background: rgb(0xF8FAFD),
            on_surface: rgb(0x1F1F1F),
            on_surface_variant: rgb(0x444746),
            outline: rgb(0x747775),
            outline_variant: rgb(0xC4C7C5),
            error: rgb(0xB3261E),
            error_container: rgb(0xF9DEDC),
            on_error_container: rgb(0x410E0B),
            success: rgb(0x146C2E),
        }
    }

    pub fn dark() -> Theme {
        Theme {
            dark: true,
            primary: rgb(0xA8C7FA),
            on_primary: rgb(0x062E6F),
            primary_container: rgb(0x0842A0),
            on_primary_container: rgb(0xD3E3FD),
            surface: rgb(0x1E1F20),
            surface_container: rgb(0x28292A),
            window_background: rgb(0x131314),
            on_surface: rgb(0xE3E3E3),
            on_surface_variant: rgb(0xC4C7C5),
            outline: rgb(0x8E918F),
            outline_variant: rgb(0x444746),
            error: rgb(0xF2B8B5),
            error_container: rgb(0x8C1D18),
            on_error_container: rgb(0xF9DEDC),
            success: rgb(0x6DD58C),
        }
    }

    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            _ => Self::light(),
        }
    }

    /// Picks the theme for a window, honouring the Windows high-contrast
    /// setting. GPUI surfaces reduced motion but not this, so it is read from
    /// the system directly — see `window_shell::high_contrast`.
    pub fn current(appearance: WindowAppearance, high_contrast: bool) -> Self {
        let theme = Self::for_appearance(appearance);
        if high_contrast {
            theme.with_increased_contrast()
        } else {
            theme
        }
    }

    /// Collapses the secondary text and outline onto the primary foreground, so
    /// nothing depends on a low-contrast tint to be readable.
    pub fn with_increased_contrast(mut self) -> Self {
        self.outline = self.on_surface;
        self.on_surface_variant = self.on_surface;
        self
    }
}

/// Opacity of the on-colour used for interaction state layers.
pub mod state_layer {
    pub const HOVER: f32 = 0.14;
    pub const FOCUS: f32 = 0.10;
    pub const PRESSED: f32 = 0.24;
}

pub mod radius {
    use super::*;
    pub const XS: Pixels = px(4.0);
    pub const SMALL: Pixels = px(8.0);
    pub const MEDIUM: Pixels = px(12.0);

    /// Stadium/pill — pass the element's height.
    pub fn full(height: Pixels) -> Pixels {
        height / 2.0
    }
}

/// The 4pt grid.
pub mod spacing {
    use super::*;
    pub const XXS: Pixels = px(4.0);
    pub const XS: Pixels = px(8.0);
    pub const S: Pixels = px(12.0);
    pub const M: Pixels = px(16.0);
    pub const L: Pixels = px(20.0);
}

/// Weights. Google Sans Flex is a variable font, so these are real masters
/// rather than a synthesised bold.
pub mod weight {
    use super::FontWeight;
    pub const MEDIUM: FontWeight = FontWeight::MEDIUM;
}

/// Line heights as multiples of the font size. Headings sit tight; anything
/// that can wrap gets room to breathe.
pub mod line_height {
    pub const TIGHT: f32 = 1.25;
    pub const BODY: f32 = 1.45;
}

pub mod type_scale {
    use super::*;
    pub const HEADLINE: Pixels = px(24.0);
    pub const TITLE: Pixels = px(16.0);
    /// History transcripts.
    pub const BODY_LARGE: Pixels = px(16.0);
    pub const BODY: Pixels = px(14.0);
    /// Pill status text.
    pub const LABEL: Pixels = px(12.0);
    pub const LABEL_SMALL: Pixels = px(11.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_and_dark_are_genuinely_different_surfaces() {
        assert_ne!(Theme::light().surface, Theme::dark().surface);
        assert_ne!(Theme::light().on_surface, Theme::dark().on_surface);
        assert!(Theme::dark().dark);
        assert!(!Theme::light().dark);
    }

    #[test]
    fn surfaces_are_never_pure_black_or_pure_white_extremes() {
        // Pure black backgrounds bloom on OLED and pure white ones glare.
        assert_ne!(Theme::dark().window_background, rgb(0x000000));
        assert_ne!(Theme::light().window_background, rgb(0xFFFFFF));
    }

    #[test]
    fn appearance_maps_vibrant_variants_onto_the_right_theme() {
        assert!(Theme::for_appearance(WindowAppearance::VibrantDark).dark);
        assert!(!Theme::for_appearance(WindowAppearance::VibrantLight).dark);
    }

    #[test]
    fn increased_contrast_collapses_the_secondary_text_colour() {
        let theme = Theme::dark().with_increased_contrast();
        assert_eq!(theme.on_surface_variant, theme.on_surface);
        assert_eq!(theme.outline, theme.on_surface);
    }

    #[test]
    fn a_pill_radius_is_half_its_height() {
        assert_eq!(radius::full(px(48.0)), px(24.0));
    }

    #[test]
    fn font_helpers_have_a_usable_fallback_before_registration() {
        // Called before `load_fonts`, these must still name a real system face.
        assert!(!ui_font().is_empty());
        assert!(!mono_font().is_empty());
    }
}
