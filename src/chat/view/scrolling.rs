//! Smooth-scroll easing state, migrated from the former `chat/scrolling.rs`.
//!
//! Only line-based wheel deltas are eased; pixel-precise deltas stay on the
//! native path. Inactive windows cancel queued motion instead of scheduling
//! frames AppKit would throttle (see `quality-guidelines.md`).

use gpui::{App, Pixels, Window, px};

/// Fraction of the remaining wheel distance applied on each animation frame.
pub(crate) const SMOOTH_SCROLL_FRAME_FRACTION: f32 = 0.22;
pub(crate) const SMOOTH_SCROLL_FINISH_THRESHOLD: Pixels = px(0.75);

#[cfg(test)]
thread_local! {
    static REASONING_SMOOTH_INVALIDATIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
pub(crate) fn reset_reasoning_smooth_invalidations() {
    REASONING_SMOOTH_INVALIDATIONS.set(0);
}

#[cfg(test)]
pub(crate) fn reasoning_smooth_invalidations() -> usize {
    REASONING_SMOOTH_INVALIDATIONS.get()
}

#[cfg(test)]
pub(in crate::chat) fn record_reasoning_smooth_invalidation() {
    REASONING_SMOOTH_INVALIDATIONS.set(REASONING_SMOOTH_INVALIDATIONS.get().saturating_add(1));
}

#[derive(Default)]
pub(in crate::chat) struct SmoothScrollState {
    pub(in crate::chat) remaining: Pixels,
    pub(in crate::chat) frame_scheduled: bool,
}

impl SmoothScrollState {
    pub(in crate::chat) fn enqueue(&mut self, distance: Pixels) {
        self.remaining += distance;
    }

    pub(in crate::chat) fn next_step(&mut self) -> Option<Pixels> {
        if self.remaining >= -SMOOTH_SCROLL_FINISH_THRESHOLD
            && self.remaining <= SMOOTH_SCROLL_FINISH_THRESHOLD
        {
            let step = self.remaining;
            self.remaining = Pixels::ZERO;
            return (step != Pixels::ZERO).then_some(step);
        }

        let step = self.remaining * SMOOTH_SCROLL_FRAME_FRACTION;
        self.remaining -= step;
        Some(step)
    }

    pub(in crate::chat) fn cancel_motion(&mut self) {
        self.remaining = Pixels::ZERO;
    }
}

pub(in crate::chat) fn smooth_scroll_animation_enabled(window: &Window, enabled: bool) -> bool {
    window.is_window_active() && enabled
}

pub(crate) fn set_smooth_scrolling(
    enabled: bool,
    preference_handle: &crate::preferences::PreferenceHandle,
    cx: &mut App,
) {
    if preference_handle.snapshot().smooth_chat_scrolling == enabled {
        return;
    }
    crate::preferences::update_with(cx, preference_handle, |prefs| {
        prefs.smooth_chat_scrolling = enabled
    });
    cx.refresh_windows();
}
