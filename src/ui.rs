//! Shared interaction primitives for app-specific UI elements.

use gpui::{App, KeyDownEvent, Window};

/// Consume a keyboard activation for an element with button semantics.
///
/// Returns `true` exactly once for Enter or Space. Space's native scrolling
/// and further event propagation are suppressed so callers only perform their
/// view-specific activation.
pub(crate) fn consume_button_key(event: &KeyDownEvent, window: &mut Window, cx: &mut App) -> bool {
    if event.is_held || !matches!(event.keystroke.key.as_str(), "enter" | "space") {
        return false;
    }

    window.prevent_default();
    cx.stop_propagation();
    true
}
