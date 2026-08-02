//! View primitives shared across features.
//!
//! [`markdown`] renders assistant prose and fenced code blocks; it is used both
//! by the transcript and by any card that shows model output. [`model_select`]
//! is the title-bar model picker. Interaction helpers small enough not to need a
//! module of their own live directly here.

pub(crate) mod markdown;
pub(crate) mod math;
pub(crate) mod model_select;

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
