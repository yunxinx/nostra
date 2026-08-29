//! Markdown fenced-code rendering and its application-wide display preferences.

mod code_block;

use std::{ops::Range, sync::Arc};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Axis, Background, Entity, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement as _, ParentElement as _, Rgba, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, Rope, Sizable as _,
    button::Toggle,
    clipboard::Clipboard,
    h_flex,
    highlighter::{HighlightTheme, SyntaxHighlighter},
    scroll::{ScrollableMask, Scrollbar, ScrollbarMode},
    text::{
        MarkdownExtensions, MarkdownNode, SelectableText, SelectableTextState, TextView,
        TextViewState, TextViewStyle, markdown_ast,
    },
    v_flex,
};
use rust_i18n::t;

use crate::preferences;

use self::code_block::extensions;
#[cfg(test)]
use self::code_block::*;

const NODE_NAME: &str = "nostra-fenced-code";
const MIN_ADJACENT_SURFACE_CONTRAST: f32 = 1.2;

/// Code blocks at or below this many bytes are highlighted synchronously on
/// the render path: no perceptible delay and no flash from a placeholder. Larger
/// blocks defer syntax highlighting to a background thread and render a
/// plain-text placeholder until the worker finishes.
const BG_HIGHLIGHT_BYTES: usize = 16 * 1024;

/// Generates a `thread_local!` counter probe and its snapshot-modify-write-back
/// accessors. `state` is a `thread_local` cell name, `update`/`reset`/`get` the
/// accessor names, and `ty` the probe struct (which must implement `Copy` and
/// `Default`). Kept as a macro so the perf and background-highlight probes
/// share one pattern instead of near-identical copies.
#[cfg(test)]
macro_rules! define_probe {
    ($state:ident, $update:ident, $reset:ident, $get:ident, $ty:ty) => {
        thread_local! {
            static $state: std::cell::Cell<$ty> = std::cell::Cell::new(<$ty>::default());
        }

        fn $update(update: impl FnOnce(&mut $ty)) {
            $state.with(|probe| {
                let mut snapshot = probe.get();
                update(&mut snapshot);
                probe.set(snapshot);
            });
        }

        pub(crate) fn $reset() {
            $state.with(|probe| probe.set(<$ty>::default()));
        }

        pub(crate) fn $get() -> $ty {
            $state.get()
        }
    };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MarkdownPerfProbe {
    pub(crate) text_view_builds: usize,
    pub(crate) code_block_renders: usize,
    pub(crate) code_text_elements: usize,
}

#[cfg(test)]
define_probe!(
    PERF_PROBE,
    update_perf_probe,
    reset_perf_probe,
    perf_probe,
    MarkdownPerfProbe
);

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BackgroundHighlightProbe {
    /// Times `render` deferred a long code block to a background highlight
    /// worker. A short block never increments this; a long block increments it
    /// exactly once per cache build (the `highlight_task` guard prevents
    /// re-spawning).
    pub(crate) background_spawns: usize,
    /// Times a worker installed styles for the current cache generation.
    pub(crate) background_installs: usize,
    /// Number of styles installed by the most recent successful worker.
    pub(crate) last_style_count: usize,
    /// Generation carried by the most recent successful worker.
    pub(crate) last_generation: Option<u64>,
}

#[cfg(test)]
define_probe!(
    BACKGROUND_PROBE,
    update_background_probe,
    reset_background_probe,
    background_probe,
    BackgroundHighlightProbe
);

/// A Markdown body and the stable extension registry that renders its fenced
/// code. Keeping the registry beside the state prevents a new extension
/// revision from forcing a full Markdown reparse on every frame.
pub(crate) struct MarkdownBody {
    state: Entity<TextViewState>,
    extensions: MarkdownExtensions,
}

impl MarkdownBody {
    pub(crate) fn new(source: &str, owner_id: u64, cx: &mut App) -> Self {
        Self {
            state: cx.new(|cx| TextViewState::markdown_with_lazy_scroll_measurement(source, cx)),
            extensions: extensions(owner_id, 0),
        }
    }

    pub(crate) fn push_str(&mut self, delta: &str, cx: &mut App) {
        if delta.is_empty() {
            return;
        }
        self.state.update(cx, |state, cx| state.push_str(delta, cx));
    }

    pub(crate) fn set_text(&mut self, source: &str, cx: &mut App) {
        self.state
            .update(cx, |state, cx| state.set_text(source, cx));
    }

    pub(crate) fn text_view(&self, style: TextViewStyle) -> TextView {
        #[cfg(test)]
        update_perf_probe(|probe| probe.text_view_builds += 1);

        TextView::new(&self.state)
            .selectable(true)
            .style(style)
            .markdown_extensions(self.extensions.clone())
    }

    pub(crate) fn scrollable_text_view(&self, style: TextViewStyle) -> TextView {
        self.text_view(style).scrollable(true)
    }

    pub(crate) fn scroll_state(&self, cx: &App) -> gpui::ListState {
        self.state.read(cx).scroll_state()
    }

    #[cfg(test)]
    pub(crate) fn entity_id(&self) -> gpui::EntityId {
        self.state.entity_id()
    }

    #[cfg(test)]
    pub(crate) fn select_all_text(&self, cx: &mut App) -> String {
        self.state.update(cx, |state, cx| state.select_all(cx));
        self.state.read(cx).selected_text()
    }
}

pub(crate) fn global_wrap_enabled(cx: &App) -> bool {
    preferences::get(cx).code_block_wrap
}

pub(crate) fn line_numbers_enabled(cx: &App) -> bool {
    preferences::get(cx).code_block_line_numbers
}

pub(crate) fn user_message_markdown_enabled(cx: &App) -> bool {
    preferences::get(cx).user_message_markdown
}

pub(crate) fn set_user_message_markdown(enabled: bool, cx: &mut App) {
    if user_message_markdown_enabled(cx) == enabled {
        return;
    }
    preferences::update(cx, |prefs| prefs.user_message_markdown = enabled);
    cx.refresh_windows();
}

pub(crate) fn set_global_wrap(enabled: bool, cx: &mut App) {
    if global_wrap_enabled(cx) == enabled {
        return;
    }
    preferences::update(cx, |prefs| reset_global_wrap(prefs, enabled));
    cx.refresh_windows();
}

fn reset_global_wrap(prefs: &mut preferences::Preferences, enabled: bool) {
    prefs.code_block_wrap = enabled;
    prefs.code_block_wrap_revision = prefs.code_block_wrap_revision.wrapping_add(1);
}

#[cfg(test)]
pub(crate) fn set_global_wrap_in_memory(enabled: bool, cx: &mut App) {
    if global_wrap_enabled(cx) == enabled {
        return;
    }
    preferences::update_in_memory(cx, |prefs| reset_global_wrap(prefs, enabled));
    cx.refresh_windows();
}

pub(crate) fn set_line_numbers(enabled: bool, cx: &mut App) {
    if line_numbers_enabled(cx) == enabled {
        return;
    }
    preferences::update(cx, |prefs| prefs.code_block_line_numbers = enabled);
    cx.refresh_windows();
}

#[cfg(test)]
mod tests;
