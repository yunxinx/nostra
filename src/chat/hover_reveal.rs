//! Hover-revealed copy affordance shared by message rows and reasoning cards.
//!
//! A turn's copy action is the same composition everywhere: a
//! [`Clipboard`](gpui_component::clipboard::Clipboard) button wrapped in a
//! `flex_none` div that stays in the element tree while invisible, so revealing
//! it on hover spends no layout and the hidden button is dropped from hit
//! testing and keyboard focus. This is the swatch-copy idiom from the
//! gpui-component story
//! (`crates/story/src/stories/theme_story/color_theme_story.rs`).

use gpui::{
    App, ElementId, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    Styled as _, Window, div,
};
use gpui_component::clipboard::Clipboard;

/// A copy button hidden until the pointer enters `hover_group`.
///
/// `id` must be unique within the window: [`Clipboard`] keys its Copy→Check
/// feedback state by it, so a stable id keeps that state across reconciliation
/// and list reordering. `value_fn` runs at click time rather than capturing a
/// render snapshot, so the clipboard always reflects the message's latest
/// state.
pub(crate) fn hover_reveal_copy(
    id: impl Into<ElementId>,
    hover_group: SharedString,
    tooltip: impl Into<SharedString>,
    value_fn: impl Fn(&mut Window, &mut App) -> SharedString + 'static,
    debug_selector: impl FnOnce() -> String,
) -> impl IntoElement {
    div()
        .flex_none()
        .debug_selector(debug_selector)
        .invisible()
        .group_hover(hover_group, |this| this.visible())
        .child(Clipboard::new(id).value_fn(value_fn).tooltip(tooltip))
}
