//! Small shared helper for programmatically dismissing a gpui-component
//! [`Popover`](gpui_component::popover::Popover) from outside its content
//! closure.

use std::{cell::RefCell, rc::Rc};

use gpui::{App, WeakEntity, Window};
use gpui_component::popover::PopoverState;

/// Holds the weak popover state bound while the popover content renders, so a
/// owning view can dismiss the popover after the content closure has returned.
///
/// The popover state lives in window-keyed storage and only exists while the
/// popover has rendered at least once, so dismissal is best-effort: [`Self::dismiss`]
/// returns `false` when no popover is currently bound.
#[derive(Clone, Default)]
pub(crate) struct PopoverDismissHandle(Rc<RefCell<Option<WeakEntity<PopoverState>>>>);

impl PopoverDismissHandle {
    /// Capture the popover state for later dismissal. Called from inside the
    /// popover content closure.
    pub(crate) fn bind(&self, state: WeakEntity<PopoverState>) {
        *self.0.borrow_mut() = Some(state);
    }

    /// Dismiss the bound popover, if any. Returns whether a popover was bound.
    pub(crate) fn dismiss(&self, window: &mut Window, cx: &mut App) -> bool {
        let Some(state) = self.0.borrow().as_ref().and_then(WeakEntity::upgrade) else {
            return false;
        };
        window.defer(cx, move |window, cx| {
            state.update(cx, |state, cx| state.dismiss(window, cx));
        });
        true
    }

    /// Dismiss the bound popover, then run `after` on the next deferred turn.
    /// Used when an action inside the popover should first close it.
    pub(crate) fn dismiss_then(
        &self,
        window: &mut Window,
        cx: &mut App,
        after: impl FnOnce(&mut Window, &mut App) + 'static,
    ) {
        let state = self.0.borrow().as_ref().and_then(WeakEntity::upgrade);
        window.defer(cx, move |window, cx| {
            if let Some(state) = state {
                state.update(cx, |state, cx| state.dismiss(window, cx));
            }
            after(window, cx);
        });
    }
}
