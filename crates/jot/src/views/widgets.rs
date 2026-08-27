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
    AnyElement, Bounds, ClickEvent, Div, ElementId, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Rgba, Role, ScrollHandle, SharedString, Stateful, Window, canvas, div, fill, point,
    prelude::*, px, relative, rgba, size,
};
use std::cell::Cell;
use std::rc::Rc;

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

/// A labelled control that is too wide to sit beside its label.
///
/// The label carries the same weight and the description the same colour as
/// [`toggle`], deliberately: the two are stacked in the same panes, and a
/// picker whose title looked like a caption made Settings read as two
/// different products bolted together.
pub fn field_row(
    label: impl Into<SharedString>,
    description: impl Into<SharedString>,
    theme: Theme,
    control: impl IntoElement,
) -> impl IntoElement {
    let description = description.into();
    div()
        .flex()
        .flex_col()
        .gap(theme::spacing::XS)
        .px(theme::spacing::XS)
        .py(theme::spacing::XS)
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
                        .child(label.into()),
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
        .child(control)
}

/// The shell every tool window shares: background, font, padding, scroll.
/// A page's scroll position, owned by the view that renders the page.
///
/// GPUI keeps the offset for you, but it will not tell you about it unless you
/// hand it a handle, and [`page`] needs the numbers to draw a scrollbar.
#[derive(Clone, Default)]
pub struct PageScroll {
    handle: ScrollHandle,
    /// Distance from the top of the thumb to the pointer when the drag began.
    /// Without it the thumb jumps so its top meets the cursor on first move.
    grab: Rc<Cell<Option<Pixels>>>,
}

impl PageScroll {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Widths chosen so the bar is visible against a dark surface without becoming
/// the loudest thing on a settings page.
mod scrollbar {
    use gpui::{Pixels, px};

    pub const LANE: Pixels = px(12.0);
    pub const THUMB: Pixels = px(6.0);
    /// A thumb shorter than this is hard to see and impossible to grab, however
    /// long the content is.
    pub const MIN_THUMB: Pixels = px(32.0);
}

pub fn page(
    id: impl Into<ElementId>,
    theme: Theme,
    scroll: &PageScroll,
    children: Vec<AnyElement>,
) -> impl IntoElement {
    let mut column = div()
        .id(id)
        .role(Role::ScrollView)
        .size_full()
        .overflow_y_scroll()
        .track_scroll(&scroll.handle)
        .flex()
        .flex_col()
        .gap(theme::spacing::M)
        .p(theme::spacing::L)
        .font_family(theme::ui_font())
        .line_height(relative(theme::line_height::BODY))
        .text_color(theme.on_surface);
    for child in children {
        column = column.child(child);
    }

    // The scrolling column and the bar are siblings inside a positioned parent:
    // an absolutely positioned child *inside* the scroll container would scroll
    // away with the content it is supposed to describe.
    div()
        .relative()
        .size_full()
        .bg(theme.window_background)
        .child(column)
        .child(scrollbar(scroll, theme))
}

/// The scroll position indicator.
///
/// GPUI ships no scrollbar element, so this one is painted by hand. It reads
/// the handle during paint rather than during render, which matters: the
/// scrolling sibling has already been laid out by then, so the bar is correct
/// on the very first frame instead of appearing one frame late.
///
/// It draws nothing at all when the content fits. A page that cannot scroll
/// should not carry furniture that says it can.
fn scrollbar(scroll: &PageScroll, theme: Theme) -> impl IntoElement {
    let handle = scroll.handle.clone();
    let grab = scroll.grab.clone();

    let painter = {
        let handle = handle.clone();
        canvas(
            move |bounds, _, _| bounds,
            move |_, bounds: Bounds<Pixels>, window, _| {
                let Some(thumb) = thumb_bounds(&handle, bounds) else {
                    return;
                };
                window.paint_quad(
                    fill(thumb, with_alpha(theme.on_surface_variant, 0.55))
                        .corner_radii(scrollbar::THUMB / 2.0),
                );
            },
        )
        .size_full()
    };

    let drag_handle = handle.clone();
    let drag_grab = grab.clone();
    let up_grab = grab.clone();
    let down_handle = handle.clone();

    div()
        .id("page-scrollbar")
        .absolute()
        .top_0()
        .right_0()
        .h_full()
        .w(scrollbar::LANE)
        .child(painter)
        .on_mouse_down(
            gpui::MouseButton::Left,
            move |event: &MouseDownEvent, window, _| {
                let bounds = lane_bounds(&down_handle);
                if let Some(thumb) = thumb_bounds(&down_handle, bounds) {
                    let offset = event.position.y - thumb.origin.y;
                    // Pressing the track outside the thumb centres the thumb on the
                    // pointer, which is what every desktop scrollbar does.
                    grab.set(Some(if (px(0.0)..thumb.size.height).contains(&offset) {
                        offset
                    } else {
                        thumb.size.height / 2.0
                    }));
                    scroll_to_pointer(&down_handle, bounds, event.position.y, grab.get());
                    // Nothing in the view's state changed, so nothing would ask for
                    // a new frame and the content would sit still under a thumb
                    // that had already moved.
                    window.refresh();
                }
            },
        )
        .on_mouse_move(move |event: &MouseMoveEvent, window, _| {
            if drag_grab.get().is_some() {
                let bounds = lane_bounds(&drag_handle);
                scroll_to_pointer(&drag_handle, bounds, event.position.y, drag_grab.get());
                window.refresh();
            }
        })
        .on_mouse_up(gpui::MouseButton::Left, move |_: &MouseUpEvent, _, _| {
            up_grab.set(None);
        })
}

/// Where the bar lives, derived from the area the content scrolls inside.
fn lane_bounds(handle: &ScrollHandle) -> Bounds<Pixels> {
    let viewport = handle.bounds();
    Bounds {
        origin: point(
            viewport.origin.x + viewport.size.width - scrollbar::LANE,
            viewport.origin.y,
        ),
        size: size(scrollbar::LANE, viewport.size.height),
    }
}

/// `None` when the content fits, which is also the signal to draw nothing.
fn thumb_bounds(handle: &ScrollHandle, lane: Bounds<Pixels>) -> Option<Bounds<Pixels>> {
    // The offset runs negative as the content moves up under the viewport.
    let (top, height) =
        thumb_geometry(lane.size.height, handle.max_offset().y, -handle.offset().y)?;
    Some(Bounds {
        origin: point(
            lane.origin.x + (scrollbar::LANE - scrollbar::THUMB) / 2.0,
            lane.origin.y + top,
        ),
        size: size(scrollbar::THUMB, height),
    })
}

/// The thumb's top and height within a lane, or `None` when the content fits.
///
/// Split out from [`thumb_bounds`] because a [`ScrollHandle`]'s extent is only
/// filled in by layout, so the arithmetic is untestable while it is entangled
/// with one.
fn thumb_geometry(
    viewport: Pixels,
    overflow: Pixels,
    scrolled: Pixels,
) -> Option<(Pixels, Pixels)> {
    if overflow <= px(0.0) || viewport <= px(0.0) {
        return None;
    }
    let height = (viewport * (viewport / (viewport + overflow))).max(scrollbar::MIN_THUMB);
    // A thumb clamped up to the minimum would otherwise be able to travel past
    // the bottom of its own lane.
    let travel = (viewport - height).max(px(0.0));
    let progress = (scrolled / overflow).clamp(0.0, 1.0);
    Some((travel * progress, height))
}

fn scroll_to_pointer(
    handle: &ScrollHandle,
    lane: Bounds<Pixels>,
    pointer: Pixels,
    grab: Option<Pixels>,
) {
    let Some(thumb) = thumb_bounds(handle, lane) else {
        return;
    };
    let travel = lane.size.height - thumb.size.height;
    if travel <= px(0.0) {
        return;
    }
    let grab = grab.unwrap_or(thumb.size.height / 2.0);
    let top = (pointer - lane.origin.y - grab).clamp(px(0.0), travel);
    let overflow = handle.max_offset().y;
    handle.set_offset(point(handle.offset().x, -(top / travel) * overflow));
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
    fn content_that_fits_gets_no_thumb() {
        assert_eq!(thumb_geometry(px(720.0), px(0.0), px(0.0)), None);
        assert_eq!(thumb_geometry(px(0.0), px(880.0), px(0.0)), None);
    }

    #[test]
    fn the_thumb_is_as_tall_a_share_of_the_lane_as_the_viewport_is_of_the_content() {
        // 720 of 1600 total is 45%, and 45% of a 720px lane is 324px.
        let (top, height) = thumb_geometry(px(720.0), px(880.0), px(0.0)).unwrap();
        assert_eq!(top, px(0.0), "unscrolled content starts at the top");
        assert!((height - px(324.0)).abs() < px(0.5), "got {height:?}");
    }

    #[test]
    fn a_fully_scrolled_page_puts_the_thumb_against_the_bottom() {
        let lane = px(720.0);
        let (top, height) = thumb_geometry(lane, px(880.0), px(880.0)).unwrap();
        assert!(
            (top + height - lane).abs() < px(0.5),
            "got {top:?} + {height:?}"
        );
    }

    #[test]
    fn a_very_long_page_still_gets_a_grabbable_thumb() {
        // Proportionally this thumb would be under 4px tall.
        let (top, height) = thumb_geometry(px(720.0), px(200_000.0), px(200_000.0)).unwrap();
        assert_eq!(height, scrollbar::MIN_THUMB);
        // And the floor must not let it slide out of the lane.
        assert!(top + height <= px(720.0), "got {top:?} + {height:?}");
    }

    #[test]
    fn an_overscrolled_offset_does_not_push_the_thumb_out_of_the_lane() {
        let (top, height) = thumb_geometry(px(720.0), px(880.0), px(5_000.0)).unwrap();
        assert!(top + height <= px(720.0), "got {top:?} + {height:?}");
    }

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
