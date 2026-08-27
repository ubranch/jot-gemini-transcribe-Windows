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

//! A single-line text field with IME support.
//!
//! GPUI ships no text input at the pinned revision, and Jot needs four: an API
//! key, a dictionary term, its misspelling, and a History search.
//!
//! Text does NOT arrive through key events. `WM_CHAR` and `WM_IME_COMPOSITION`
//! are routed by the platform into the [`EntityInputHandler`] installed during
//! paint, which is what makes Chinese, Japanese, Korean and Vietnamese input
//! work — the composition is marked, shown underlined, and committed as one
//! edit. Key events are only used for the operations an IME does not own:
//! caret movement, deletion, select-all, copy and paste.
//!
//! What it still is not: a full editor. The IME candidate window is placed
//! against the field rather than the caret, and there is no word-wise motion or
//! double-click-to-select-word.

use crate::theme::{self, Theme};
use gpui::{
    Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    Focusable, Font, FontFeatures, FontStyle, FontWeight, KeyDownEvent, MouseDownEvent, Pixels,
    Point, Role, SharedString, TextRun, UTF16Selection, Window, canvas, div, prelude::*, px,
};
use std::ops::Range;

/// How many edits back a field remembers. A single-line field holds an API key
/// or a model name, not a document, so this is generous rather than tuned.
const UNDO_DEPTH: usize = 64;

/// A restorable moment: the text and where the caret was in it.
#[derive(Clone, PartialEq, Eq)]
struct Snapshot {
    content: String,
    selection: Range<usize>,
}

/// Emitted when the user presses Enter.
pub struct Submit;

pub struct TextField {
    /// Stable and unique within its window. Several fields share a pane, and a
    /// duplicate id collapses two of them into one accessibility node.
    id: SharedString,
    content: String,
    /// Byte range. `start == end` is a caret; both ends always sit on a
    /// character boundary.
    selection: Range<usize>,
    /// Byte range of the in-progress IME composition, if any.
    marked: Option<Range<usize>>,
    pub placeholder: SharedString,
    /// Renders as bullets and never reaches the clipboard through this field.
    pub masked: bool,
    pub focus_handle: FocusHandle,
    /// Captured at paint so the IME candidate window has somewhere to go, and
    /// so a click can be turned into a caret position.
    last_bounds: Option<Bounds<Pixels>>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// True while a run of single characters is being typed, so that a word
    /// undoes as a word instead of one letter per press.
    coalescing: bool,
}

impl gpui::EventEmitter<Submit> for TextField {}

impl Focusable for TextField {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TextField {
    pub fn new(
        id: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            id: id.into(),
            content: String::new(),
            selection: 0..0,
            marked: None,
            placeholder: placeholder.into(),
            masked: false,
            focus_handle: cx.focus_handle(),
            last_bounds: None,
            undo: Vec::new(),
            redo: Vec::new(),
            coalescing: false,
        }
    }

    pub fn masked(mut self) -> Self {
        self.masked = true;
        self
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn set_content(&mut self, text: impl Into<String>) {
        self.content = text.into();
        self.selection = self.content.len()..self.content.len();
        self.marked = None;
        // Filling a field from settings is not an edit the user made, so there
        // is nothing for them to undo back past.
        self.undo.clear();
        self.redo.clear();
        self.coalescing = false;
    }

    pub fn take_content(&mut self) -> String {
        self.selection = 0..0;
        self.marked = None;
        std::mem::take(&mut self.content)
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    // ----- UTF-16 conversion
    //
    // The platform speaks UTF-16 offsets and the buffer is UTF-8, so every
    // handler method converts at its boundary. Getting this wrong corrupts
    // exactly the text that needed an IME in the first place.

    fn byte_to_utf16(&self, byte: usize) -> usize {
        self.content[..byte.min(self.content.len())]
            .chars()
            .map(char::len_utf16)
            .sum()
    }

    fn utf16_to_byte(&self, offset: usize) -> usize {
        let mut seen = 0;
        for (byte, character) in self.content.char_indices() {
            if seen >= offset {
                return byte;
            }
            seen += character.len_utf16();
        }
        self.content.len()
    }

    fn utf16_range(&self, range: Range<usize>) -> Range<usize> {
        self.byte_to_utf16(range.start)..self.byte_to_utf16(range.end)
    }

    fn byte_range(&self, range: Range<usize>) -> Range<usize> {
        let start = self.utf16_to_byte(range.start);
        let end = self.utf16_to_byte(range.end);
        start.min(end)..start.max(end)
    }

    // ----- Editing

    /// Records the current text so the next edit can be undone.
    ///
    /// `coalesce` groups a run of single typed characters into one undo step:
    /// undoing a word one letter at a time is technically correct and nobody
    /// wants it. Anything else — a paste, a deletion, a replaced selection —
    /// breaks the run and becomes its own step.
    fn checkpoint(&mut self, coalesce: bool) {
        if coalesce && self.coalescing {
            return;
        }
        let snapshot = Snapshot {
            content: self.content.clone(),
            selection: self.selection.clone(),
        };
        if self.undo.last() != Some(&snapshot) {
            self.undo.push(snapshot);
            if self.undo.len() > UNDO_DEPTH {
                self.undo.remove(0);
            }
        }
        // A fresh edit starts a new branch of history, so anything undone past
        // this point can no longer be redone.
        self.redo.clear();
        self.coalescing = coalesce;
    }

    fn undo(&mut self) {
        let Some(previous) = self.undo.pop() else {
            return;
        };
        self.redo.push(self.snapshot());
        self.restore(previous);
    }

    fn redo(&mut self) {
        let Some(next) = self.redo.pop() else {
            return;
        };
        self.undo.push(self.snapshot());
        self.restore(next);
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            content: self.content.clone(),
            selection: self.selection.clone(),
        }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.content = snapshot.content;
        let end = self.content.len();
        self.selection = snapshot.selection.start.min(end)..snapshot.selection.end.min(end);
        // An in-flight composition describes text that no longer exists.
        self.marked = None;
        self.coalescing = false;
    }

    /// Replaces `range` with `text` and leaves the caret after it.
    ///
    /// A single-line field: newlines become spaces rather than silently
    /// truncating whatever came after them, which is how a pasted API key with
    /// a trailing newline used to lose its second half.
    fn replace(&mut self, range: Range<usize>, text: &str) {
        let sanitized = text.replace(['\n', '\r'], " ");
        let range = range.start.min(self.content.len())..range.end.min(self.content.len());
        self.content.replace_range(range.clone(), &sanitized);
        let caret = range.start + sanitized.len();
        self.selection = caret..caret;
        self.marked = None;
    }

    fn delete_backwards(&mut self) {
        self.checkpoint(false);
        if self.selection.start != self.selection.end {
            let range = self.selection.clone();
            self.replace(range, "");
            return;
        }
        let Some(previous) = self.content[..self.selection.start].chars().next_back() else {
            return;
        };
        let range = (self.selection.start - previous.len_utf8())..self.selection.start;
        self.replace(range, "");
    }

    fn delete_forwards(&mut self) {
        self.checkpoint(false);
        if self.selection.start != self.selection.end {
            let range = self.selection.clone();
            self.replace(range, "");
            return;
        }
        let Some(next) = self.content[self.selection.end..].chars().next() else {
            return;
        };
        let range = self.selection.end..(self.selection.end + next.len_utf8());
        self.replace(range, "");
    }

    /// Moves the caret, or extends the selection when `extend` is set.
    fn move_caret(&mut self, to: usize, extend: bool) {
        if extend {
            self.selection = self.selection.start..to;
        } else {
            self.selection = to..to;
        }
    }

    fn previous_boundary(&self) -> usize {
        let from = self.selection.end;
        self.content[..from]
            .chars()
            .next_back()
            .map_or(from, |character| from - character.len_utf8())
    }

    fn next_boundary(&self) -> usize {
        let from = self.selection.end;
        self.content[from..]
            .chars()
            .next()
            .map_or(from, |character| from + character.len_utf8())
    }

    /// Turns a click into a caret position.
    ///
    /// The text is re-shaped with the same font and size it was painted with,
    /// so the boundary this lands on is the one under the pointer rather than a
    /// guess from an average character width. Masked fields shape their bullets
    /// instead, and the answer is mapped back through the character count —
    /// a bullet is three bytes and the key behind it usually is not.
    fn caret_from_click(&self, at: Point<Pixels>, window: &Window) -> Option<usize> {
        let bounds = self.last_bounds?;
        let visible = self.rendered(0..self.content.len());
        if visible.is_empty() {
            return Some(0);
        }
        let font = Font {
            family: if self.masked {
                theme::mono_font()
            } else {
                theme::ui_font()
            },
            features: FontFeatures::default(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };
        let size = theme::type_scale::BODY;
        let run = TextRun {
            len: visible.len(),
            font,
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window.text_system().shape_line(
            SharedString::from(visible.clone()),
            size,
            &[run],
            None,
        );

        // `last_bounds` is the field's box; the text starts one horizontal
        // padding in.
        let x = at.x - bounds.origin.x - theme::spacing::S;
        let index = line.closest_index_for_x(x.max(px(0.0)));
        let characters = visible[..index.min(visible.len())].chars().count();
        Some(
            self.content
                .char_indices()
                .nth(characters)
                .map_or(self.content.len(), |(offset, _)| offset),
        )
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;

        if modifiers.control || modifiers.platform {
            match keystroke.key.as_str() {
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        self.checkpoint(false);
                        let range = self.selection.clone();
                        self.replace(range, &text);
                    }
                }
                "a" => self.selection = 0..self.content.len(),
                "z" if modifiers.shift => self.redo(),
                "z" => self.undo(),
                // Windows' other redo binding. Supporting one and not the other
                // reads as a missing feature to whoever learned the other one.
                "y" => self.redo(),
                // Never from a masked field: the API key does not leave here.
                "c" if self.selection.start != self.selection.end && !self.masked => {
                    let selected = self.content[self.selection.clone()].to_string();
                    cx.write_to_clipboard(ClipboardItem::new_string(selected));
                }
                _ => return,
            }
            cx.notify();
            return;
        }

        match keystroke.key.as_str() {
            "backspace" => self.delete_backwards(),
            "delete" => self.delete_forwards(),
            "left" => {
                let to = self.previous_boundary();
                self.move_caret(to, modifiers.shift);
            }
            "right" => {
                let to = self.next_boundary();
                self.move_caret(to, modifiers.shift);
            }
            "home" => self.move_caret(0, modifiers.shift),
            "end" => self.move_caret(self.content.len(), modifiers.shift),
            "enter" => {
                cx.emit(Submit);
                return;
            }
            // Anything else that produces text arrives through the input
            // handler, not here. Inserting `key_char` as well would type every
            // character twice and break composition outright.
            _ => return,
        }
        cx.notify();
    }

    fn rendered(&self, range: Range<usize>) -> String {
        if self.masked {
            "•".repeat(self.content[range].chars().count())
        } else {
            self.content[range].to_string()
        }
    }
}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.byte_range(range_utf16);
        *adjusted_range = Some(self.utf16_range(range.clone()));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.utf16_range(self.selection.clone()),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let marked = self.marked.clone()?;
        Some(self.byte_to_utf16(marked.start)..self.byte_to_utf16(marked.end))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // No range means "whatever is marked, else the selection" — that is how
        // a committed composition replaces the text it was composing.
        let range = match range_utf16 {
            Some(range) => self.byte_range(range),
            None => self.marked.clone().unwrap_or(self.selection.clone()),
        };
        // One character landing on a caret is typing and coalesces; a longer
        // string is a committed composition or a platform paste, and is not.
        let typing = text.chars().count() == 1 && range.start == range.end;
        self.checkpoint(typing);
        self.replace(range, text);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = match range_utf16 {
            Some(range) => self.byte_range(range),
            None => self.marked.clone().unwrap_or(self.selection.clone()),
        };
        // Only the start of a composition is worth remembering: every keystroke
        // inside one rewrites the same marked run, and each would otherwise
        // become its own undo step.
        if self.marked.is_none() {
            self.checkpoint(false);
        }
        let sanitized = new_text.replace(['\n', '\r'], " ");
        let range = range.start.min(self.content.len())..range.end.min(self.content.len());
        self.content.replace_range(range.clone(), &sanitized);

        let marked = range.start..(range.start + sanitized.len());
        // The caret sits where the IME says it is inside the composition, or at
        // its end when it does not say.
        self.selection = match new_selected_range {
            Some(selected) => {
                let within = self.content[marked.clone()]
                    .char_indices()
                    .map(|(byte, _)| byte)
                    .nth(selected.start)
                    .unwrap_or(sanitized.len());
                (marked.start + within)..(marked.start + within)
            }
            None => marked.end..marked.end,
        };
        self.marked = (!sanitized.is_empty()).then_some(marked);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // The whole field, not the caret: nothing here measures glyph
        // positions, so the candidate window is placed against the control
        // rather than pretending to know where the caret is.
        Some(self.last_bounds.unwrap_or(element_bounds))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.byte_to_utf16(self.content.len()))
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::current(window.appearance(), crate::window_shell::high_contrast());
        let focused = self.focus_handle.is_focused(window);
        let empty = self.content.is_empty();

        // Split the text at every boundary that changes how it is drawn, so the
        // caret, the selection and the IME composition can all be shown at once.
        let mut boundaries = vec![
            0,
            self.content.len(),
            self.selection.start,
            self.selection.end,
        ];
        if let Some(marked) = &self.marked {
            boundaries.push(marked.start);
            boundaries.push(marked.end);
        }
        boundaries.retain(|at| *at <= self.content.len() && self.content.is_char_boundary(*at));
        boundaries.sort_unstable();
        boundaries.dedup();

        let caret_at = self.selection.end;
        let has_selection = self.selection.start != self.selection.end;
        let mut runs = div().flex().items_center();
        for pair in boundaries.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            if focused && !has_selection && start == caret_at {
                runs = runs.child(caret(theme));
            }
            let selected =
                has_selection && start >= self.selection.start && end <= self.selection.end;
            let composing = self
                .marked
                .as_ref()
                .is_some_and(|marked| start >= marked.start && end <= marked.end);
            let mut run = div().child(self.rendered(start..end));
            if selected {
                run = run.bg(crate::views::widgets::with_alpha(theme.primary, 0.35));
            }
            if composing {
                // Underlined, because an IME composition is provisional text
                // and must not look like something already committed.
                run = run.text_decoration_1().text_decoration_solid();
            }
            runs = runs.child(run);
        }
        if focused && !has_selection && caret_at == self.content.len() {
            runs = runs.child(caret(theme));
        }

        let entity = cx.entity();
        let focus_handle = self.focus_handle.clone();

        div()
            .id(self.id.clone())
            .role(Role::TextInput)
            .aria_label(self.placeholder.clone())
            .track_focus(&self.focus_handle)
            .tab_stop(true)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window, cx);
                    if let Some(caret) = this.caret_from_click(event.position, window) {
                        this.selection = caret..caret;
                        this.coalescing = false;
                    }
                    cx.notify();
                }),
            )
            .flex()
            .items_center()
            .w_full()
            .h(px(34.0))
            .px(theme::spacing::S)
            .rounded(theme::radius::SMALL)
            .border_1()
            .border_color(if focused {
                theme.primary
            } else {
                theme.outline_variant
            })
            .bg(theme.surface)
            .cursor_text()
            .font_family(if self.masked {
                theme::mono_font()
            } else {
                theme::ui_font()
            })
            .text_size(theme::type_scale::BODY)
            // Installs the platform input handler for the next frame. This is
            // the only place text can arrive from, so it has to happen on every
            // paint while focused.
            .relative()
            .child(
                canvas(
                    move |bounds, _, _| bounds,
                    move |bounds, _, window, cx| {
                        entity.update(cx, |field, _| field.last_bounds = Some(bounds));
                        window.handle_input(
                            &focus_handle,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                    },
                )
                // Absolute so measuring the field costs it no layout space: an
                // in-flow canvas would take width from the text beside it.
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(if empty {
                div()
                    .flex()
                    .items_center()
                    .flex_1()
                    .overflow_hidden()
                    // A focused empty field drew no caret at all, because the
                    // placeholder branch never reached the run loop. Clicking a
                    // field and seeing nothing change reads as a dead control.
                    .when(focused, |row| row.child(caret(theme)))
                    .child(
                        div()
                            .text_color(theme.on_surface_variant)
                            .child(self.placeholder.clone()),
                    )
                    .into_any_element()
            } else {
                runs.flex_1()
                    .overflow_hidden()
                    .text_color(theme.on_surface)
                    .into_any_element()
            })
    }
}

/// A caret that does not blink: it still says "typing lands here", and a blink
/// is an animation nobody asked for.
fn caret(theme: Theme) -> impl IntoElement {
    div().w(px(1.5)).h(px(18.0)).bg(theme.primary)
}

pub fn set_clipboard_text(text: impl Into<String>, cx: &gpui::App) {
    cx.write_to_clipboard(ClipboardItem::new_string(text.into()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn field(cx: &mut TestAppContext) -> gpui::Entity<TextField> {
        cx.new(|cx| TextField::new("test-field", "placeholder", cx))
    }

    /// What typing a word actually looks like: one character at a time through
    /// the platform input handler.
    fn type_text(
        field: &mut TextField,
        text: &str,
        window: &mut Window,
        cx: &mut Context<TextField>,
    ) {
        for character in text.chars() {
            field.replace_text_in_range(None, &character.to_string(), window, cx);
        }
    }

    #[gpui::test]
    fn a_typed_word_undoes_as_a_word(cx: &mut TestAppContext) {
        let field = field(cx);
        let window = cx.add_window(|_, _| gpui::Empty);
        cx.update_window(window.into(), |_, window, cx| {
            field.update(cx, |field, cx| {
                type_text(field, "hello", window, cx);
                assert_eq!(field.content(), "hello");
                field.undo();
                assert_eq!(field.content(), "", "five keystrokes are one edit");
                field.redo();
                assert_eq!(field.content(), "hello");
            });
        })
        .unwrap();
    }

    #[gpui::test]
    fn a_deletion_is_its_own_step(cx: &mut TestAppContext) {
        let field = field(cx);
        let window = cx.add_window(|_, _| gpui::Empty);
        cx.update_window(window.into(), |_, window, cx| {
            field.update(cx, |field, cx| {
                type_text(field, "abc", window, cx);
                field.delete_backwards();
                assert_eq!(field.content(), "ab");
                field.undo();
                assert_eq!(field.content(), "abc", "the delete undoes on its own");
                field.undo();
                assert_eq!(field.content(), "", "and the typing behind it");
            });
        })
        .unwrap();
    }

    #[gpui::test]
    fn editing_after_an_undo_drops_the_redo(cx: &mut TestAppContext) {
        let field = field(cx);
        let window = cx.add_window(|_, _| gpui::Empty);
        cx.update_window(window.into(), |_, window, cx| {
            field.update(cx, |field, cx| {
                type_text(field, "one", window, cx);
                field.undo();
                type_text(field, "two", window, cx);
                field.redo();
                assert_eq!(field.content(), "two", "the abandoned branch is gone");
            });
        })
        .unwrap();
    }

    #[gpui::test]
    fn undo_on_an_untouched_field_does_nothing(cx: &mut TestAppContext) {
        let field = field(cx);
        field.update(cx, |field, _| {
            field.undo();
            field.redo();
            assert_eq!(field.content(), "");
        });
    }

    #[gpui::test]
    fn loading_a_value_leaves_nothing_to_undo_back_past(cx: &mut TestAppContext) {
        let field = field(cx);
        let window = cx.add_window(|_, _| gpui::Empty);
        cx.update_window(window.into(), |_, window, cx| {
            field.update(cx, |field, cx| {
                // Settings fills the field, then the user edits it.
                field.set_content("gemini-3.5-flash-lite");
                type_text(field, "X", window, cx);
                field.undo();
                assert_eq!(field.content(), "gemini-3.5-flash-lite");
                field.undo();
                assert_eq!(
                    field.content(),
                    "gemini-3.5-flash-lite",
                    "undo must not empty a field the user never emptied"
                );
            });
        })
        .unwrap();
    }

    #[gpui::test]
    fn history_is_bounded(cx: &mut TestAppContext) {
        let field = field(cx);
        field.update(cx, |field, _| {
            for index in 0..(UNDO_DEPTH * 2) {
                field.checkpoint(false);
                field.content.push_str(&index.to_string());
            }
            assert_eq!(field.undo.len(), UNDO_DEPTH);
        });
    }

    #[gpui::test]
    fn editing_keeps_the_caret_on_character_boundaries(cx: &mut TestAppContext) {
        let field = field(cx);
        field.update(cx, |field, _| {
            field.replace(0..0, "naïve café");
            assert_eq!(field.content(), "naïve café");

            field.delete_backwards();
            assert_eq!(field.content(), "naïve caf");

            let to = field.previous_boundary();
            field.move_caret(to, false);
            let to = field.previous_boundary();
            field.move_caret(to, false);
            field.replace(field.selection.clone(), "X");
            assert_eq!(field.content(), "naïve cXaf");
        });
    }

    #[gpui::test]
    fn utf16_offsets_round_trip_through_astral_characters(cx: &mut TestAppContext) {
        let field = field(cx);
        field.update(cx, |field, _| {
            // An emoji is two UTF-16 units and four UTF-8 bytes; the platform
            // counts the former and the buffer stores the latter.
            field.set_content("a😀b");
            assert_eq!(field.byte_to_utf16(field.content.len()), 4);
            assert_eq!(field.utf16_to_byte(0), 0);
            assert_eq!(field.utf16_to_byte(1), 1);
            assert_eq!(field.utf16_to_byte(3), 5);
            assert_eq!(field.utf16_to_byte(4), 6);
            // Past the end clamps rather than panicking.
            assert_eq!(field.utf16_to_byte(99), field.content.len());
        });
    }

    #[gpui::test]
    fn a_composition_is_marked_then_committed_as_one_edit(cx: &mut TestAppContext) {
        let field = field(cx);
        let window = cx.add_window(|_, _| gpui::Empty);
        cx.update_window(window.into(), |_, window, cx| {
            field.update(cx, |field, cx| {
                // Typing "ni" in a Chinese IME: provisional, and marked.
                field.replace_and_mark_text_in_range(None, "ni", None, window, cx);
                assert_eq!(field.content(), "ni");
                assert_eq!(field.marked, Some(0..2));

                // Choosing the candidate replaces the whole composition rather
                // than appending to it.
                field.replace_text_in_range(None, "\u{4f60}", window, cx);
                assert_eq!(field.content(), "\u{4f60}");
                assert!(field.marked.is_none(), "committed text is not provisional");
                assert_eq!(field.selection, 3..3, "caret follows the commit");
            });
        })
        .unwrap();
    }

    #[gpui::test]
    fn an_abandoned_composition_leaves_no_marked_text(cx: &mut TestAppContext) {
        let field = field(cx);
        let window = cx.add_window(|_, _| gpui::Empty);
        cx.update_window(window.into(), |_, window, cx| {
            field.update(cx, |field, cx| {
                field.replace_and_mark_text_in_range(None, "kan", None, window, cx);
                assert!(field.marked.is_some());
                field.unmark_text(window, cx);
                assert!(field.marked.is_none());
            });
        })
        .unwrap();
    }

    #[gpui::test]
    fn a_pasted_newline_never_truncates_the_value(cx: &mut TestAppContext) {
        let field = field(cx);
        field.update(cx, |field, _| {
            // An API key copied out of a web page often carries a trailing
            // newline; losing everything after it would be silent data loss.
            field.replace(0..0, "AIza-part-one\nAIza-part-two");
            assert_eq!(field.content(), "AIza-part-one AIza-part-two");
        });
    }

    #[gpui::test]
    fn a_masked_field_never_renders_its_value(cx: &mut TestAppContext) {
        let field = field(cx);
        field.update(cx, |field, _| {
            field.masked = true;
            field.set_content("AIzaSyExample");
            let shown = field.rendered(0..field.content.len());
            assert_eq!(shown, "•".repeat(13));
            assert!(!shown.contains("AIza"));
        });
    }

    #[gpui::test]
    fn selecting_then_typing_replaces_the_selection(cx: &mut TestAppContext) {
        let field = field(cx);
        field.update(cx, |field, _| {
            field.set_content("hello world");
            field.selection = 0..5;
            field.replace(field.selection.clone(), "goodbye");
            assert_eq!(field.content(), "goodbye world");
            assert_eq!(field.selection, 7..7);
        });
    }

    #[gpui::test]
    fn deleting_past_either_end_is_a_no_op(cx: &mut TestAppContext) {
        let field = field(cx);
        field.update(cx, |field, _| {
            field.delete_backwards();
            field.delete_forwards();
            assert_eq!(field.content(), "");

            field.replace(0..0, "ab");
            field.delete_forwards();
            assert_eq!(field.content(), "ab", "caret is already at the end");
            field.selection = 0..0;
            field.delete_backwards();
            assert_eq!(field.content(), "ab");
        });
    }

    #[gpui::test]
    fn taking_the_content_empties_the_field(cx: &mut TestAppContext) {
        let field = field(cx);
        field.update(cx, |field, _| {
            field.set_content("Kubernetes");
            assert_eq!(field.take_content(), "Kubernetes");
            assert!(field.is_empty());
            assert_eq!(field.selection, 0..0);
        });
    }
}
