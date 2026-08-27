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

//! The handful of controls the settings, history and dictionary panes share.
//!
//! Every one of them is keyboard-reachable, carries a role and a label, and
//! shows focus. That is the whole reason they live here instead of being
//! open-coded per pane: an accessible control copied five times is an
//! inaccessible control four times.

use crate::theme::{self, Theme};
use gpui::{
    AnyElement, ClickEvent, Div, ElementId, Rgba, Role, SharedString, Stateful, Window, div,
    prelude::*, px, relative, rgba,
};

pub fn with_alpha(color: Rgba, alpha: f32) -> Rgba {
    rgba(
        ((color.r * 255.0) as u32) << 24
            | ((color.g * 255.0) as u32) << 16
            | ((color.b * 255.0) as u32) << 8
            | (alpha.clamp(0.0, 1.0) * 255.0) as u32,
    )
}

/// Composites a state layer ON TOP of a base colour and returns the result,
/// still opaque.
///
/// A hover style REPLACES the background rather than layering over it, so
/// setting it to a translucent tint makes the control's fill vanish under the
/// pointer instead of brightening.
pub fn state_layer(base: Rgba, layer: Rgba, amount: f32) -> Rgba {
    let t = amount.clamp(0.0, 1.0);
    Rgba {
        r: base.r + (layer.r - base.r) * t,
        g: base.g + (layer.g - base.g) * t,
        b: base.b + (layer.b - base.b) * t,
        a: base.a,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// The one action a pane is about.
    Primary,
    /// Everything else.
    Secondary,
    /// Deletes something the user cannot get back.
    Destructive,
}

/// Wraps a control so it hugs its content inside a stretching column.
///
/// Panes are flex columns, and a bare button in one stretches edge to edge,
/// which reads as a banner rather than something to press.
pub fn hug(control: impl IntoElement) -> Div {
    div().flex().child(control)
}

/// A button. `on_click` is a plain closure so callers can hand it
/// `cx.listener(...)` without this module knowing their view type.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    enabled: bool,
    theme: Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    let label = label.into();
    let (background, foreground, border) = match kind {
        ButtonKind::Primary => (theme.primary, theme.on_primary, theme.primary),
        ButtonKind::Secondary => (theme.surface, theme.on_surface, theme.outline_variant),
        ButtonKind::Destructive => (theme.surface, theme.error, theme.error),
    };
    let (background, foreground, border) = if enabled {
        (background, foreground, border)
    } else {
        // Disabled is stated in colour AND in the accessibility tree — colour
        // alone is not a state for anyone who cannot see it.
        (
            with_alpha(background, 0.4),
            with_alpha(foreground, 0.4),
            with_alpha(border, 0.4),
        )
    };

    div()
        .id(id)
        .role(Role::Button)
        .aria_label(if enabled {
            label.clone()
        } else {
            format!("{label} (unavailable)").into()
        })
        .tab_stop(enabled)
        .px(theme::spacing::S)
        .py(px(7.0))
        .rounded(theme::radius::SMALL)
        .border_1()
        .border_color(border)
        .bg(background)
        .text_size(theme::type_scale::BODY)
        .font_weight(theme::weight::MEDIUM)
        .line_height(relative(theme::line_height::TIGHT))
        .text_color(foreground)
        .when(enabled, |button| {
            let hovered = state_layer(background, foreground, theme::state_layer::HOVER);
            let pressed = state_layer(background, foreground, theme::state_layer::PRESSED);
            button
                .cursor_pointer()
                .hover(move |style| style.bg(hovered))
                // Press feedback lands on mouse-down, so the button acknowledges
                // the click before the work behind it starts.
                .active(move |style| style.bg(pressed))
                .focus_visible(move |style| style.border_color(foreground))
                .on_click(move |event, window, cx| on_click(event, window, cx))
        })
        .child(label)
}

/// A labelled on/off switch.
pub fn toggle(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    on: bool,
    theme: Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> Stateful<Div> {
    let label = label.into();
    let description = description.into();
    let track = if on {
        theme.primary
    } else {
        theme.outline_variant
    };
    let knob = if on { theme.on_primary } else { theme.surface };

    div()
        .id(id)
        .role(Role::Switch)
        .aria_label(label.clone())
        // The state travels in the accessibility tree, not only in the pixels.
        .aria_toggled(if on {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .tab_stop(true)
        .flex()
        .items_center()
        .justify_between()
        .gap(theme::spacing::M)
        .px(theme::spacing::XS)
        .py(theme::spacing::XS)
        .rounded(theme::radius::SMALL)
        .cursor_pointer()
        .hover(move |style| {
            style.bg(state_layer(
                theme.surface,
                theme.on_surface,
                theme::state_layer::HOVER,
            ))
        })
        .active(move |style| {
            style.bg(state_layer(
                theme.surface,
                theme.on_surface,
                theme::state_layer::PRESSED,
            ))
        })
        .focus_visible(|style| style.bg(with_alpha(theme.on_surface, theme::state_layer::FOCUS)))
        .on_click(move |event, window, cx| on_click(event, window, cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(theme::type_scale::BODY)
                        .font_weight(theme::weight::MEDIUM)
                        .line_height(relative(theme::line_height::TIGHT))
                        .text_color(theme.on_surface)
                        .child(label),
                )
                .when(!description.is_empty(), |column| {
                    column.child(
                        div()
                            .text_size(theme::type_scale::LABEL)
                            .line_height(relative(theme::line_height::BODY))
                            .text_color(theme.on_surface_variant)
                            .child(description),
                    )
                }),
        )
        .child(
            div()
                // Fixed: a flex child shrinks to fit by default, and the row
                // with the longest description was squeezing the track down to
                // its knob.
                .flex_none()
                .w(px(40.0))
                .h(px(22.0))
                .rounded(px(11.0))
                .bg(track)
                .flex()
                .items_center()
                .when(on, |switch| switch.justify_end())
                .px(px(3.0))
                .child(
                    div()
                        .flex_none()
                        .w(px(16.0))
                        .h(px(16.0))
                        .rounded(px(8.0))
                        .bg(knob),
                ),
        )
}

/// A titled group of rows.
pub fn section(
    title: impl Into<SharedString>,
    theme: Theme,
    children: Vec<AnyElement>,
) -> impl IntoElement {
    let title = title.into();
    let mut column = div()
        .flex()
        .flex_col()
        .gap(theme::spacing::XS)
        .p(theme::spacing::M)
        .rounded(theme::radius::MEDIUM)
        .border_1()
        .border_color(theme.outline_variant)
        .bg(theme.surface)
        .child(
            div()
                // Derived from the title so several sections in one pane never
                // collide in the accessibility tree.
                .id(SharedString::from(format!("section-{title}")))
                .role(Role::Heading)
                .aria_level(2)
                .text_size(theme::type_scale::TITLE)
                .font_weight(theme::weight::MEDIUM)
                .line_height(relative(theme::line_height::TIGHT))
                .text_color(theme.on_surface)
                .pb(theme::spacing::XXS)
                .child(title.clone()),
        );
    for child in children {
        column = column.child(child);
    }
    column
}

/// A row of a label and whatever control belongs beside it.
pub fn field_row(
    label: impl Into<SharedString>,
    theme: Theme,
    control: impl IntoElement,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(theme::spacing::XXS)
        .py(theme::spacing::XS)
        .child(
            div()
                .text_size(theme::type_scale::LABEL)
                .line_height(relative(theme::line_height::BODY))
                .text_color(theme.on_surface_variant)
                .child(label.into()),
        )
        .child(control)
}

/// The shell every tool window shares: background, font, padding, scroll.
pub fn page(id: impl Into<ElementId>, theme: Theme, children: Vec<AnyElement>) -> impl IntoElement {
    let mut column = div()
        .id(id)
        .role(Role::ScrollView)
        .size_full()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(theme::spacing::M)
        .p(theme::spacing::L)
        .font_family(theme::ui_font())
        .line_height(relative(theme::line_height::BODY))
        .bg(theme.window_background)
        .text_color(theme.on_surface);
    for child in children {
        column = column.child(child);
    }
    column
}

/// A one-line status message. `tone` decides whether it reads as neutral,
/// good, or a problem — and it always says what to do next.
pub fn status_line(
    id: impl Into<ElementId>,
    message: impl Into<SharedString>,
    tone: StatusTone,
    theme: Theme,
) -> impl IntoElement {
    let color = match tone {
        StatusTone::Neutral => theme.on_surface_variant,
        StatusTone::Good => theme.success,
        StatusTone::Bad => theme.error,
    };
    div()
        .id(id)
        .role(Role::Status)
        .text_size(theme::type_scale::LABEL)
        .line_height(relative(theme::line_height::BODY))
        .text_color(color)
        .child(message.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    Neutral,
    Good,
    Bad,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_buttons_are_dimmed_rather_than_recoloured() {
        // Keeping the hue and dropping the alpha means a disabled destructive
        // button still reads as destructive.
        let faded = with_alpha(Theme::light().error, 0.4);
        assert!((faded.r - Theme::light().error.r).abs() < 0.01);
        assert!((faded.a - 0.4).abs() < 0.01);
    }

    #[test]
    fn alpha_is_clamped_to_the_valid_range() {
        assert_eq!(with_alpha(Theme::light().primary, 5.0).a, 1.0);
        assert_eq!(with_alpha(Theme::light().primary, -1.0).a, 0.0);
    }
}
