use gpui::{App, Pixels, Window, px};

/// Fraction of the remaining wheel distance applied on each animation frame.
pub(super) const SMOOTH_SCROLL_FRAME_FRACTION: f32 = 0.22;
pub(super) const SMOOTH_SCROLL_FINISH_THRESHOLD: Pixels = px(0.75);

#[cfg(test)]
thread_local! {
    static REASONING_SMOOTH_INVALIDATIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
pub(super) fn reset_reasoning_smooth_invalidations() {
    REASONING_SMOOTH_INVALIDATIONS.set(0);
}

#[cfg(test)]
pub(super) fn reasoning_smooth_invalidations() -> usize {
    REASONING_SMOOTH_INVALIDATIONS.get()
}

#[cfg(test)]
pub(super) fn record_reasoning_smooth_invalidation() {
    REASONING_SMOOTH_INVALIDATIONS.set(REASONING_SMOOTH_INVALIDATIONS.get().saturating_add(1));
}

#[derive(Default)]
pub(super) struct SmoothScrollState {
    pub(super) remaining: Pixels,
    pub(super) frame_scheduled: bool,
}

impl SmoothScrollState {
    pub(super) fn enqueue(&mut self, distance: Pixels) {
        self.remaining += distance;
    }

    pub(super) fn next_step(&mut self) -> Option<Pixels> {
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

    pub(super) fn cancel_motion(&mut self) {
        self.remaining = Pixels::ZERO;
    }
}

pub(crate) fn smooth_scrolling_enabled(cx: &App) -> bool {
    crate::preferences::get(cx).smooth_chat_scrolling
}

pub(super) fn smooth_scroll_animation_enabled(window: &Window, cx: &App) -> bool {
    window.is_window_active() && smooth_scrolling_enabled(cx)
}

pub(crate) fn set_smooth_scrolling(enabled: bool, cx: &mut App) {
    if smooth_scrolling_enabled(cx) == enabled {
        return;
    }
    crate::preferences::update(cx, |prefs| prefs.smooth_chat_scrolling = enabled);
    cx.refresh_windows();
}
