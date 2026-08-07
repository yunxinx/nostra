//! Markdown fenced-code rendering and its application-wide display preferences.

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
    scroll::{ScrollableMask, Scrollbar, ScrollbarShow},
    text::{
        MarkdownExtensions, MarkdownNode, SelectableText, SelectableTextState, TextView,
        TextViewState, TextViewStyle, markdown_ast,
    },
    v_flex,
};
use rust_i18n::t;

use crate::preferences;

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

fn is_fenced_code_source(source: &str) -> bool {
    let first_line = source.lines().next().unwrap_or_default().as_bytes();
    let indentation = first_line.iter().take_while(|byte| **byte == b' ').count();

    // markdown-rs represents fenced and four-space-indented code with the
    // same Node::Code variant. CommonMark permits at most three leading spaces
    // before a fence opener, so inspecting the original first line is the only
    // reliable way for this extension to avoid claiming indented code blocks.
    if indentation > 3 {
        return false;
    }

    let Some(&marker) = first_line.get(indentation) else {
        return false;
    };
    if marker != b'`' && marker != b'~' {
        return false;
    }

    first_line[indentation..]
        .iter()
        .take_while(|byte| **byte == marker)
        .count()
        >= 3
}

#[derive(Clone)]
struct FencedCode {
    code: SharedString,
    language: Option<SharedString>,
    start: usize,
}

pub(crate) fn extensions(owner_id: u64, source_offset: usize) -> MarkdownExtensions {
    let extensions = MarkdownExtensions::default().cjk_emphasis_compatibility();
    super::math::extend(extensions, owner_id, source_offset)
        .block_parser(move |node, cx| {
            let markdown_ast::Node::Code(code) = node else {
                return None;
            };
            let source = cx.node_source(node)?;
            if !is_fenced_code_source(source) {
                // Returning None preserves gpui-component's native rendering
                // for indented code instead of attaching fenced-only controls.
                return None;
            }
            // Nested Markdown fragments parse a sliced source whose local
            // offset starts at zero. Add the fragment's document-space base so
            // code and formula state keys remain unique and stable across the
            // complete streamed message.
            let start = source_offset + cx.node_range(node)?.start;
            let data = FencedCode {
                code: code.value.clone().into(),
                language: code
                    .lang
                    .as_ref()
                    .filter(|language| !language.is_empty())
                    .cloned()
                    .map(Into::into),
                start,
            };
            Some(
                MarkdownNode::new(NODE_NAME, data)
                    .text(code.value.clone())
                    .markdown(source.to_string())
                    .selectable_text_state(SelectableTextState::new(code.value.clone())),
            )
        })
        .block_renderer(NODE_NAME, move |node, window, cx| {
            let Some(code) = node.data::<FencedCode>() else {
                return div().into_any_element();
            };
            render(
                code,
                node.attached_selectable_text_state(),
                owner_id,
                window,
                cx,
            )
        })
}

/// Byte-range → syntax-style mappings for one fenced code block.
type CodeStyles = Arc<[(Range<usize>, HighlightStyle)]>;

struct HighlightCache {
    code: SharedString,
    /// Normalized syntax language ID used for cache matching and highlighting.
    language: String,
    theme: Arc<HighlightTheme>,
    /// Syntax highlight ranges. `None` while a long block is being highlighted
    /// on a background thread; the render path shows a plain-text placeholder
    /// (structure/line numbers/controls intact) until the worker finishes.
    styles: Option<CodeStyles>,
    line_count: usize,
    line_numbers: SharedString,
    /// Bumped on every rebuild so a stale background result is discarded.
    generation: u64,
    /// The in-flight background highlight task. Keeping it here prevents a new
    /// task from being spawned on every frame. On a cache rebuild this task is
    /// dropped, which cancels a worker that has not started yet and discards
    /// the result of one already running (a synchronous tree-sitter parse
    /// cannot be preempted mid-poll). Either way the new generation immediately
    /// starts a fresh worker instead of waiting on a stale result.
    highlight_task: Option<Task<()>>,
    /// Generation owned by `highlight_task`; prevents a stale callback from
    /// clearing a task that was started for a newer cache generation.
    highlight_task_generation: Option<u64>,
}

impl HighlightCache {
    fn new(code: &FencedCode, cx: &App) -> Self {
        let theme = cx.theme().highlight_theme.clone();
        let language = normalized_language_id(code.language.as_deref());
        // Short blocks are highlighted synchronously so the first paint is
        // complete (no flash); long blocks render a placeholder and defer the
        // work to the background worker started in `render`.
        let styles = if code.code.len() <= BG_HIGHLIGHT_BYTES {
            Some(compute_code_styles(
                code.code.as_ref(),
                &language,
                theme.as_ref(),
            ))
        } else {
            None
        };
        let line_count = code.code.split('\n').count();
        let number_width = line_count.max(1).to_string().len();
        let line_numbers = (1..=line_count)
            .map(|number| format!("{number:>number_width$}"))
            .collect::<Vec<_>>()
            .join("\n")
            .into();

        Self {
            code: code.code.clone(),
            language,
            theme,
            styles,
            line_count,
            line_numbers,
            generation: 0,
            highlight_task: None,
            highlight_task_generation: None,
        }
    }

    fn replace(&mut self, code: &FencedCode, cx: &App) {
        let generation = self.generation.wrapping_add(1);
        // Dropping the previous task cancels a worker that has not started yet
        // and discards the result of one already running (a synchronous
        // tree-sitter parse cannot be preempted mid-poll). Either way the
        // current generation starts a fresh worker on the next render instead
        // of waiting on (and discarding) a stale result; `generation` still
        // guards against a stale result that arrives late.
        *self = Self::new(code, cx);
        self.generation = generation;
    }

    /// Install `styles`, but only if they belong to the current generation.
    /// Returns `false` when `generation` is stale (the result is discarded),
    /// so callers can skip the repaint that would otherwise be a no-op.
    fn try_apply_styles(&mut self, generation: u64, styles: CodeStyles) -> bool {
        if self.generation != generation {
            return false;
        }
        self.styles = Some(styles);
        true
    }

    fn matches(&self, code: &FencedCode, cx: &App) -> bool {
        // `language` is already normalized; compare the raw input case-insensitively
        // to avoid allocating another normalized string on every render.
        self.code == code.code
            && self
                .language
                .eq_ignore_ascii_case(code.language.as_deref().unwrap_or("text"))
            && self.theme == cx.theme().highlight_theme
    }
}

fn normalized_language_id(language: Option<&str>) -> String {
    language.unwrap_or("text").to_ascii_lowercase()
}

/// Run tree-sitter syntax highlighting for a complete code block. Invoked
/// synchronously for short blocks in `HighlightCache::new` and on a background
/// thread for long blocks (see `render`), so it must depend only on the
/// thread-safe `LanguageRegistry::singleton()` and never on UI-thread state.
fn compute_code_styles(code: &str, language: &str, theme: &HighlightTheme) -> CodeStyles {
    parse_code_styles(code, language, theme, |code, language, theme| {
        let mut highlighter = SyntaxHighlighter::new(language);
        let rope = Rope::from(code);
        highlighter.update(None, &rope, None);
        highlighter.styles(&(0..code.len()), theme).into()
    })
}

/// Run `parse` under a panic guard and degrade to an empty style list on panic.
///
/// tree-sitter can panic on pathological input in streamed/AI-generated
/// content. A panic here would otherwise propagate to the caller: for a short
/// block it would take down the render path, and for a long block it would
/// cross the background `await` into the foreground task. An empty result
/// still renders the block as plain text with its structure, line numbers,
/// and controls intact. `SyntaxHighlighter` is rebuilt per call, so a panicked
/// parser is never reused.
fn parse_code_styles(
    code: &str,
    language: &str,
    theme: &HighlightTheme,
    parse: impl FnOnce(&str, &str, &HighlightTheme) -> CodeStyles,
) -> CodeStyles {
    // This closure only receives immutable inputs and no shared mutable state.
    // Keep that UnwindSafe property if the parser is extended.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parse(code, language, theme)
    }))
    .unwrap_or_default()
}

#[cfg(test)]
struct HighlightedLine {
    styles: Vec<(Range<usize>, HighlightStyle)>,
}

#[derive(Clone, Copy)]
struct WrapState {
    enabled: bool,
    global_revision: u64,
}

impl WrapState {
    fn new(enabled: bool, global_revision: u64) -> Self {
        Self {
            enabled,
            global_revision,
        }
    }
}

#[cfg(test)]
fn highlighted_lines(
    code: &str,
    styles: &[(Range<usize>, HighlightStyle)],
) -> Arc<[HighlightedLine]> {
    let mut style_start = 0;
    let mut line_start = 0;
    code.split('\n')
        .map(|line| {
            let line_end = line_start + line.len();
            while style_start < styles.len() && styles[style_start].0.end <= line_start {
                style_start += 1;
            }
            let line_styles = styles
                .iter()
                .skip(style_start)
                .take_while(|(range, _)| range.start < line_end)
                .filter_map(|(range, style)| {
                    let clipped_start = range.start.max(line_start);
                    let clipped_end = range.end.min(line_end);
                    (clipped_start < clipped_end)
                        .then_some((clipped_start - line_start..clipped_end - line_start, *style))
                })
                .collect();
            line_start = line_end.saturating_add(1);
            HighlightedLine {
                styles: line_styles,
            }
        })
        .collect()
}

fn code_text_element(
    owner_id: u64,
    start: usize,
    state: SelectableTextState,
    text: SharedString,
    styles: Arc<[(Range<usize>, HighlightStyle)]>,
    line_number_gutter: Option<(gpui::Pixels, Hsla)>,
) -> SelectableText {
    #[cfg(test)]
    update_perf_probe(|probe| probe.code_text_elements += 1);

    let text = SelectableText::with_text(
        format!("markdown-code-text-{owner_id}-{start}"),
        state,
        text,
        styles.iter().cloned(),
    );
    if let Some((right_margin, color)) = line_number_gutter {
        text.line_number_gutter(right_margin, color)
    } else {
        text
    }
}

fn render(
    code: &FencedCode,
    attached_text_state: Option<&SelectableTextState>,
    owner_id: u64,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    #[cfg(test)]
    update_perf_probe(|probe| probe.code_block_renders += 1);

    let cache_id: SharedString =
        format!("markdown-code-highlight-{owner_id}-{}", code.start).into();
    let cache = window.use_keyed_state(cache_id, cx, |_, cx| HighlightCache::new(code, cx));
    if !cache.read(cx).matches(code, cx) {
        cache.update(cx, |cache, cx| cache.replace(code, cx));
    }
    spawn_background_highlight(&cache, code, window, cx);
    let (styles, line_count, line_numbers) = cache.read_with(cx, |cache, _| {
        (
            cache.styles.as_ref().cloned().unwrap_or_default(),
            cache.line_count,
            cache.line_numbers.clone(),
        )
    });

    let global_wrap = global_wrap_enabled(cx);
    let global_wrap_revision = preferences::get(cx).code_block_wrap_revision;
    let wrap_state_id: SharedString =
        format!("markdown-code-wrap-state-{owner_id}-{}", code.start).into();
    let wrap_state = window.use_keyed_state(wrap_state_id, cx, |_, _| {
        WrapState::new(global_wrap, global_wrap_revision)
    });
    let stored_wrap = *wrap_state.read(cx);
    let wrap = if stored_wrap.global_revision == global_wrap_revision {
        stored_wrap.enabled
    } else {
        global_wrap
    };
    let show_line_numbers = line_numbers_enabled(cx);
    let number_width = line_count.max(1).to_string().len();
    let text_state = attached_text_state
        .cloned()
        .unwrap_or_else(|| SelectableTextState::new(code.code.clone()));
    let line_number_color = cx
        .theme()
        .highlight_theme
        .style
        .editor_line_number
        .unwrap_or(cx.theme().muted_foreground);
    let gutter_character_width = cx.theme().mono_font_size * 0.62;
    let gutter_margin = px(12.);
    let gutter_width = gutter_character_width * number_width + gutter_margin;

    let mut has_horizontal_overflow = false;
    let code_content = if wrap && show_line_numbers {
        h_flex()
            .w_full()
            .min_w_0()
            .items_start()
            .child(div().w(gutter_width).flex_none().debug_selector(move || {
                format!("markdown-code-line-number-{owner_id}-{}-0", code.start)
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .whitespace_normal()
                    .debug_selector(move || {
                        format!("markdown-code-line-{owner_id}-{}-0", code.start)
                    })
                    .child(code_text_element(
                        owner_id,
                        code.start,
                        text_state.clone(),
                        code.code.clone(),
                        styles.clone(),
                        Some((gutter_margin, line_number_color)),
                    )),
            )
            .into_any_element()
    } else if wrap {
        div()
            .w_full()
            .min_w_0()
            .whitespace_normal()
            .debug_selector(move || format!("markdown-code-line-{owner_id}-{}-0", code.start))
            .child(code_text_element(
                owner_id,
                code.start,
                text_state.clone(),
                code.code.clone(),
                styles.clone(),
                None,
            ))
            .into_any_element()
    } else {
        let scroll_state_id: SharedString =
            format!("markdown-code-scroll-state-{owner_id}-{}", code.start).into();
        let scroll_handle = window
            .use_keyed_state(scroll_state_id, cx, |_, _| gpui::ScrollHandle::new())
            .read(cx)
            .clone();
        let overflow_state_id: SharedString =
            format!("markdown-code-overflow-state-{owner_id}-{}", code.start).into();
        let overflow_state = window.use_keyed_state(overflow_state_id, cx, |_, _| false);
        has_horizontal_overflow = *overflow_state.read(cx);
        let scrollbar_id: SharedString =
            format!("markdown-code-scrollbar-control-{owner_id}-{}", code.start).into();
        let overflow_probe = {
            let scroll_handle = scroll_handle.clone();
            let overflow_state = overflow_state.clone();
            move |_, window: &mut Window, cx: &mut App| {
                let measured_overflow = scroll_handle.max_offset().x > px(0.);
                if *overflow_state.read(cx) == measured_overflow {
                    return;
                }
                overflow_state.update(cx, |overflow, _| *overflow = measured_overflow);
                window.refresh();
            }
        };
        let viewport = v_flex()
            .id(format!("markdown-code-scroll-{owner_id}-{}", code.start))
            .debug_selector(move || format!("markdown-code-scroll-{owner_id}-{}", code.start))
            .relative()
            .flex_1()
            .min_w_0()
            .gap(px(0.))
            .child(
                div()
                    .w_full()
                    .relative()
                    .child(
                        div()
                            .id(format!(
                                "markdown-code-scroll-track-{owner_id}-{}",
                                code.start
                            ))
                            .size_full()
                            .flex()
                            .flex_row()
                            .relative()
                            .overflow_hidden()
                            .track_scroll(&scroll_handle)
                            .child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .debug_selector(move || {
                                        format!("markdown-code-line-{owner_id}-{}-0", code.start)
                                    })
                                    .child(code_text_element(
                                        owner_id,
                                        code.start,
                                        text_state,
                                        code.code.clone(),
                                        styles,
                                        None,
                                    )),
                            ),
                    )
                    // GPUI's native horizontal overflow does not consume wheel
                    // events, so an outer vertical scroller can reinterpret
                    // their x delta. The mask owns horizontal gestures while
                    // leaving vertical input available to the transcript.
                    .child(ScrollableMask::new(Axis::Horizontal, &scroll_handle))
                    .on_children_prepainted(overflow_probe),
            )
            .when(has_horizontal_overflow, |this| {
                this.child(
                    div()
                        .mx(px(1.))
                        .flex_none()
                        .h(px(16.))
                        .relative()
                        .debug_selector(move || {
                            format!("markdown-code-scrollbar-{owner_id}-{}", code.start)
                        })
                        .child(
                            Scrollbar::horizontal(&scroll_handle)
                                .id(scrollbar_id)
                                .scrollbar_show(ScrollbarShow::Always),
                        ),
                )
            });
        h_flex()
            .w_full()
            .min_w_0()
            .items_start()
            .when(show_line_numbers, |this| {
                this.child(
                    div()
                        .flex_none()
                        .pr_3()
                        .whitespace_nowrap()
                        .text_color(line_number_color)
                        .debug_selector(move || {
                            format!("markdown-code-line-number-{owner_id}-{}-0", code.start)
                        })
                        .child(line_numbers),
                )
            })
            .child(viewport)
            .into_any_element()
    };

    let wrap_id: SharedString = format!("markdown-code-wrap-{owner_id}-{}", code.start).into();
    let copy_id: SharedString = format!("markdown-code-copy-{owner_id}-{}", code.start).into();
    let language_selector = move || format!("markdown-code-language-{owner_id}-{}", code.start);
    let header_selector = move || format!("markdown-code-header-{owner_id}-{}", code.start);
    let wrap_selector = move || format!("markdown-code-wrap-{owner_id}-{}", code.start);
    let copy_selector = move || format!("markdown-code-copy-{owner_id}-{}", code.start);
    let header_surface = code_header_surface(cx);

    v_flex()
        .debug_selector(move || format!("markdown-code-block-{owner_id}-{}", code.start))
        .w_full()
        .min_w_0()
        .rounded(cx.theme().radius)
        .overflow_hidden()
        .bg(cx.theme().tokens.muted)
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(cx.theme().mono_font_size)
        .child(
            h_flex()
                .debug_selector(header_selector)
                .w_full()
                .min_w_0()
                .items_center()
                .gap_1()
                .px_3()
                .pt(px(6.))
                .pb(px(6.))
                .rounded_tl(cx.theme().radius)
                .rounded_tr(cx.theme().radius)
                .bg(header_surface.background)
                .child(div().flex_1().min_w_0().when_some(
                    code.language.clone(),
                    |this, language| {
                        this.child(
                            div()
                                .max_w(px(160.))
                                .truncate()
                                .text_xs()
                                .text_color(cx.theme().foreground)
                                .debug_selector(language_selector)
                                .child(language),
                        )
                    },
                ))
                .child(
                    div().flex_none().debug_selector(wrap_selector).child(
                        Toggle::new(wrap_id)
                            .xsmall()
                            .checked(wrap)
                            .when_some(wrap_toggle_surface(wrap, cx), |this, surface| {
                                this.bg(surface.background)
                            })
                            .icon(Icon::default().path("icons/wrap-text.svg"))
                            .tooltip(t!("chat.code.wrap").to_string())
                            .on_click({
                                let wrap_state = wrap_state.clone();
                                move |checked, _, cx| {
                                    wrap_state.update(cx, |state, cx| {
                                        if state.enabled == *checked
                                            && state.global_revision == global_wrap_revision
                                        {
                                            return;
                                        }
                                        *state = WrapState::new(*checked, global_wrap_revision);
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
                )
                .child(
                    div().flex_none().debug_selector(copy_selector).child(
                        Clipboard::new(copy_id)
                            .value(code.code.clone())
                            .tooltip(t!("chat.code.copy").to_string()),
                    ),
                ),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .px_3()
                .pt(px(6.))
                .pb(if wrap || !has_horizontal_overflow {
                    px(12.)
                } else {
                    px(2.)
                })
                .child(code_content),
        )
        .into_any_element()
}

/// A long block (styles deferred) is highlighted on a background thread so the
/// first paint never blocks on tree-sitter. The cached task guards against
/// spawning a new worker on every frame; `generation` guards against a stale
/// result overwriting newer content. Placeholder rendering uses an empty style
/// list until the worker finishes.
fn spawn_background_highlight(
    cache: &Entity<HighlightCache>,
    code: &FencedCode,
    window: &mut Window,
    cx: &mut App,
) {
    let (styles_ready, task_ready, generation, theme, language) =
        cache.read_with(cx, |cache, _| {
            (
                cache.styles.is_some(),
                cache.highlight_task.is_some(),
                cache.generation,
                cache.theme.clone(),
                cache.language.clone(),
            )
        });
    if styles_ready || task_ready {
        return;
    }
    #[cfg(test)]
    update_background_probe(|probe| probe.background_spawns += 1);
    let weak_cache = cache.downgrade();
    let code_text = code.code.clone();
    let background = cx.background_spawn(async move {
        compute_code_styles(code_text.as_ref(), &language, theme.as_ref())
    });
    // Publish the generation before scheduling the foreground continuation so
    // an executor that polls a newly spawned task immediately still passes the
    // stale-result guard below. The task handle is installed after spawn.
    cache.update(cx, |cache, _| {
        cache.highlight_task_generation = Some(generation);
    });
    let task = window.spawn(cx, async move |async_cx| {
        let styles = background.await;
        // The window (and its keyed-state cache) may already be gone; a
        // dropped result is expected there, so an empty update is fine.
        let _ = async_cx.update(|_, cx| {
            let _ = weak_cache.update(cx, |cache, cache_cx| {
                if cache.highlight_task_generation == Some(generation) {
                    cache.highlight_task = None;
                    cache.highlight_task_generation = None;
                    #[cfg(test)]
                    let style_count = styles.len();
                    if cache.try_apply_styles(generation, styles) {
                        #[cfg(test)]
                        update_background_probe(|probe| {
                            probe.background_installs += 1;
                            probe.last_style_count = style_count;
                            probe.last_generation = Some(generation);
                        });
                        // Only a result that installs styles changes the
                        // rendered block; a stale result is discarded and
                        // needs no repaint.
                        cache_cx.notify();
                    }
                }
            });
        });
    });
    // The generation was registered before spawn; keep the task handle alive so
    // subsequent frames do not start duplicate workers.
    cache.update(cx, |cache, _| {
        cache.highlight_task = Some(task);
    });
}

#[derive(Clone, Copy)]
struct CodeSurface {
    background: Background,
    color: Hsla,
}

fn code_header_surface(cx: &App) -> CodeSurface {
    let body = opaque_surface(cx.theme().background, cx.theme().muted);
    themed_distinct_surface(
        cx.theme().tokens.secondary.background,
        opaque_surface(body, cx.theme().secondary),
        body,
        cx.theme().is_dark(),
    )
}

fn wrap_toggle_surface(wrap: bool, cx: &App) -> Option<CodeSurface> {
    wrap.then(|| {
        let header = code_header_surface(cx);
        themed_distinct_surface(
            cx.theme().tokens.secondary_active.background,
            opaque_surface(header.color, cx.theme().secondary_active),
            header.color,
            cx.theme().is_dark(),
        )
    })
}

fn opaque_surface(background: Hsla, surface: Hsla) -> Hsla {
    background.blend(surface).alpha(1.)
}

fn themed_distinct_surface(
    preferred_background: Background,
    mut preferred_color: Hsla,
    reference: Hsla,
    dark: bool,
) -> CodeSurface {
    if surface_contrast(preferred_color, reference) >= MIN_ADJACENT_SURFACE_CONTRAST {
        return CodeSurface {
            background: preferred_background,
            color: preferred_color,
        };
    }

    let step = if dark { 0.01 } else { -0.01 };
    while surface_contrast(preferred_color, reference) < MIN_ADJACENT_SURFACE_CONTRAST {
        let next_lightness = (preferred_color.l + step).clamp(0., 1.);
        if next_lightness == preferred_color.l {
            break;
        }
        preferred_color.l = next_lightness;
    }
    CodeSurface {
        background: preferred_color.into(),
        color: preferred_color,
    }
}

fn surface_contrast(a: Hsla, b: Hsla) -> f32 {
    let luminance = |color: Hsla| {
        let rgb = Rgba::from(color);
        let channel = |value: f32| {
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
    };
    let (a, b) = (luminance(a), luminance(b));
    let (lighter, darker) = if a > b { (a, b) } else { (b, a) };
    (lighter + 0.05) / (darker + 0.05)
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
mod tests {
    use gpui::{
        Context, IntoElement, Modifiers, MouseButton, Render, TestAppContext, VisualTestContext,
        point,
    };
    use gpui_component::{ActiveTheme as _, Root, WindowExt as _};

    use super::*;

    struct CodeSelectionTestRoot {
        body: MarkdownBody,
    }

    impl Render for CodeSelectionTestRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(480.))
                .child(self.body.text_view(TextViewStyle::default()))
        }
    }

    fn init_markdown_test(cx: &mut TestAppContext) {
        let prefs = preferences::Preferences::default();
        cx.update(|cx| {
            gpui_component::init(cx);
            preferences::init_global(prefs.clone(), cx);
            crate::appearance::theme::init(&prefs, cx);
        });
    }

    fn assert_fenced_code_drag_copy(cx: &mut TestAppContext, wrap: bool) {
        const OWNER_ID: u64 = 7;
        const SOURCE: &str = "```text\nfirst 你好\n\n🙂 third\n```";
        const CODE: &str = "first 你好\n\n🙂 third";

        init_markdown_test(cx);
        cx.update(|cx| {
            set_global_wrap_in_memory(wrap, cx);
            preferences::update_in_memory(cx, |prefs| {
                prefs.code_block_line_numbers = true;
            });
        });
        let (_, cx) = cx.add_window_view(|window, cx| {
            let content = cx.new(|cx| CodeSelectionTestRoot {
                body: MarkdownBody::new(SOURCE, OWNER_ID, cx),
            });
            Root::new(content, window, cx)
        });
        let cx: &mut VisualTestContext = cx;

        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let code = cx
            .debug_bounds("markdown-code-line-7-0-0")
            .expect("continuous code bounds");
        let logical_line_height = code.size.height / 3.;
        assert!(logical_line_height > px(0.));

        let start = point(code.left() + px(1.), code.top() + logical_line_height / 2.);
        let end = point(
            code.right() - px(1.),
            code.bottom() - logical_line_height / 2.,
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());

        let selected = cx.update(|window, cx| window.selected_text(cx));
        assert_eq!(selected.trim_end_matches('\n'), CODE);

        cx.dispatch_action(gpui_component::input::Copy);
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(CODE.to_string()),
            "drag-copy must preserve Unicode and empty lines without painted line numbers"
        );
    }

    #[gpui::test]
    fn nowrap_fenced_code_drag_copy_preserves_lines_and_empty_line_height(cx: &mut TestAppContext) {
        assert_fenced_code_drag_copy(cx, false);
    }

    #[gpui::test]
    fn wrapped_fenced_code_drag_copy_preserves_lines_and_empty_line_height(
        cx: &mut TestAppContext,
    ) {
        assert_fenced_code_drag_copy(cx, true);
    }

    #[gpui::test]
    fn code_block_surfaces_remain_distinct_in_every_theme(cx: &mut TestAppContext) {
        use gpui_component::{Theme, ThemeMode};

        let prefs = preferences::Preferences::default();
        cx.update(|cx| {
            gpui_component::init(cx);
            preferences::init_global(prefs.clone(), cx);
            crate::appearance::theme::init(&prefs, cx);

            for dark in [true, false] {
                Theme::change(
                    if dark {
                        ThemeMode::Dark
                    } else {
                        ThemeMode::Light
                    },
                    None,
                    cx,
                );

                for name in crate::appearance::theme::theme_names(dark, cx) {
                    crate::appearance::theme::select_theme_for_test(name.as_ref(), cx);

                    let body = opaque_surface(cx.theme().background, cx.theme().muted);
                    let header = code_header_surface(cx);
                    let active_wrap = wrap_toggle_surface(true, cx).expect("active surface");
                    assert!(wrap_toggle_surface(false, cx).is_none());
                    assert!(
                        surface_contrast(header.color, body) >= MIN_ADJACENT_SURFACE_CONTRAST,
                        "{name}: code header must stand out from the code body"
                    );
                    assert!(
                        surface_contrast(active_wrap.color, header.color)
                            >= MIN_ADJACENT_SURFACE_CONTRAST,
                        "{name}: active wrap control must stand out from the code header"
                    );
                }
            }
        });
    }

    #[test]
    fn language_identifiers_are_ascii_case_insensitive() {
        let registry = gpui_component::highlighter::LanguageRegistry::singleton();
        for language in [
            "Python",
            "PYTHON",
            "Py",
            "PY",
            "Rust",
            "RS",
            "JavaScript",
            "JAVASCRIPT",
            "TypeScript",
            "TYPESCRIPT",
        ] {
            assert!(
                registry
                    .language(&normalized_language_id(Some(language)))
                    .is_some(),
                "{language} must resolve through the case-insensitive normalizer"
            );
        }

        let code = "name = 'world'\nprint(f'hello {name}')";
        let mut highlighter = SyntaxHighlighter::new(&normalized_language_id(Some("PYTHON")));
        let rope = Rope::from(code);
        assert!(highlighter.update(None, &rope, None));
        assert!(
            !highlighter
                .styles(&(0..code.len()), &HighlightTheme::default_dark())
                .is_empty(),
            "uppercase Python must produce syntax styles rather than plain text"
        );
    }

    #[gpui::test]
    fn highlight_cache_matches_language_identifiers_case_insensitively(cx: &mut TestAppContext) {
        init_markdown_test(cx);
        let original = FencedCode {
            code: "print('hello')".into(),
            language: Some("Python".into()),
            start: 0,
        };
        let casing_changed = FencedCode {
            language: Some("PYTHON".into()),
            ..original.clone()
        };
        let cache = cx.update(|cx| HighlightCache::new(&original, cx));

        assert!(cx.update(|cx| cache.matches(&casing_changed, cx)));
    }

    #[test]
    fn large_mixed_case_languages_produce_syntax_styles() {
        let theme = HighlightTheme::default_dark();
        for (language, line) in [
            ("PYTHON", "name = 'world'\nprint(name)\n"),
            ("Rust", "fn main() { let value: usize = 42; }\n"),
        ] {
            let code = line.repeat(1000);
            assert!(code.len() > BG_HIGHLIGHT_BYTES);
            let styles =
                compute_code_styles(&code, &normalized_language_id(Some(language)), &theme);
            assert!(
                styles.len() > 1,
                "large {language} code must produce syntax styles"
            );
        }
    }

    /// A panicking highlighter (tree-sitter does panic on pathological streamed
    /// input) must degrade to an empty style list instead of propagating the
    /// panic — the block then renders as plain text rather than crashing the
    /// render path (short block) or the foreground task (long block).
    #[test]
    fn compute_code_styles_recovers_from_panicking_highlighter() {
        let theme = HighlightTheme::default_dark();
        let styles = parse_code_styles(
            "fn main() { let value: usize = 42; }",
            "rust",
            &theme,
            |_, _, _| {
                panic!("tree-sitter panicked on pathological input");
            },
        );
        assert!(
            styles.is_empty(),
            "a panicking highlighter must degrade to an empty style list"
        );
    }

    #[test]
    fn distinguishes_fenced_code_from_indented_code_source() {
        for source in ["```rust\nfn main() {}\n```", "   ~~~py\nprint(1)\n   ~~~"] {
            assert!(is_fenced_code_source(source), "fenced source: {source:?}");
        }
        for source in [
            "    print('indented')",
            "    ```not-a-fence",
            "\tprint('tab-indented')",
        ] {
            assert!(
                !is_fenced_code_source(source),
                "indented source: {source:?}"
            );
        }
    }

    #[gpui::test]
    fn streamed_extension_syntax_stays_in_the_authoritative_text_state(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut body = cx.update(|cx| MarkdownBody::new("prefix `", 1, cx));

        cx.update(|cx| body.push_str("`", cx));
        cx.update(|cx| body.push_str("`rust\nfn main() {}\n```\n", cx));

        cx.update(|cx| body.push_str("$", cx));
        cx.update(|cx| body.push_str("x", cx));
        cx.update(|cx| body.push_str("$", cx));
        cx.run_until_parked();

        let selected = cx.update(|cx| body.select_all_text(cx));
        assert!(selected.contains("fn main() {}"));
        assert!(selected.contains("$x$"));
    }

    #[gpui::test]
    fn replacement_then_append_uses_the_replacement_as_its_worker_baseline(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let mut body = cx.update(|cx| MarkdownBody::new("old", 1, cx));

        cx.update(|cx| body.set_text("new $x$", cx));
        cx.update(|cx| body.push_str(" tail", cx));
        cx.run_until_parked();

        assert_eq!(
            cx.update(|cx| body.select_all_text(cx)).trim(),
            "new $x$ tail"
        );
    }

    #[test]
    fn ignores_highlights_that_end_before_later_lines() {
        let lines = highlighted_lines("first\nsecond", &[(0..5, HighlightStyle::default())]);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].styles.len(), 1);
        assert_eq!(lines[0].styles[0].0, 0..5);
        assert!(lines[1].styles.is_empty());
    }

    #[test]
    fn clips_highlights_that_span_multiple_lines() {
        let lines = highlighted_lines("abc\ndef", &[(1..6, HighlightStyle::default())]);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].styles[0].0, 1..3);
        assert_eq!(lines[1].styles[0].0, 0..2);
    }

    #[test]
    fn projects_ordered_highlights_across_empty_and_unstyled_lines() {
        let first = HighlightStyle::default();
        let last = HighlightStyle::default();
        let lines = highlighted_lines("ab\n\nplain\nxyz", &[(0..2, first), (10..13, last)]);

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].styles, vec![(0..2, first)]);
        assert!(lines[1].styles.is_empty());
        assert!(lines[2].styles.is_empty());
        assert_eq!(lines[3].styles, vec![(0..3, last)]);
    }

    /// P0 spike: prove the syntax-highlighting chain can run on a background
    /// thread. `SyntaxHighlighter` owns a tree-sitter `Parser` (not `Send`), so
    /// we must construct it inside the worker thread rather than move it across.
    /// This test asserts that (a) a fresh highlighter can be built and driven
    /// purely from the thread-safe `LanguageRegistry::singleton()`, and (b) the
    /// produced `Vec<(Range, HighlightStyle)>` is `Send` and can cross back.
    #[test]
    fn syntax_highlighter_runs_off_thread_and_returns_send_styles() {
        let code = Rope::from("fn main() -> usize { 42 }");
        let styles = std::thread::spawn(move || {
            let mut highlighter = SyntaxHighlighter::new("rust");
            highlighter.update(None, &code, None);
            highlighter.styles(&(0..code.len()), &HighlightTheme::default_dark())
        })
        .join()
        .expect("worker thread");

        assert!(
            !styles.is_empty(),
            "rust fenced code must yield syntax styles off the main thread"
        );
    }

    /// P0 spike: the current theme's `HighlightTheme` is passed from the main
    /// thread into the background worker (`Arc<HighlightTheme>`), so the whole
    /// `Arc<HighlightTheme>` must be `Send` (i.e. `HighlightTheme: Send + Sync`).
    #[test]
    fn highlighted_theme_arc_is_send_across_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<Arc<HighlightTheme>>();
    }

    /// A short code block (≤ `BG_HIGHLIGHT_BYTES`) is highlighted synchronously:
    /// the background worker is never spawned, so the first paint is complete
    /// and there is no placeholder flash.
    #[gpui::test]
    fn short_code_block_highlights_synchronously_without_background(cx: &mut TestAppContext) {
        init_markdown_test(cx);
        reset_background_probe();
        let (_, cx) = cx.add_window_view(|window, cx| {
            let content = cx.new(|cx| CodeSelectionTestRoot {
                body: MarkdownBody::new("```rust\nlet x = 1;\n```", 7, cx),
            });
            Root::new(content, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert_eq!(
            background_probe().background_spawns,
            0,
            "short code block must highlight synchronously, not on a background thread"
        );
    }

    /// The byte threshold is inclusive for synchronous highlighting: a block of
    /// exactly `BG_HIGHLIGHT_BYTES` renders synchronously, while one byte over
    /// defers to the background worker. This pins the boundary so a future edit
    /// cannot silently move it.
    #[gpui::test]
    fn background_threshold_boundary_is_inclusive_for_sync(cx: &mut TestAppContext) {
        init_markdown_test(cx);
        let at_threshold = FencedCode {
            code: "x".repeat(BG_HIGHLIGHT_BYTES).into(),
            language: Some("text".into()),
            start: 0,
        };
        let over_threshold = FencedCode {
            code: "x".repeat(BG_HIGHLIGHT_BYTES + 1).into(),
            language: Some("text".into()),
            start: 0,
        };
        let at_cache = cx.update(|cx| HighlightCache::new(&at_threshold, cx));
        let over_cache = cx.update(|cx| HighlightCache::new(&over_threshold, cx));
        assert!(
            at_cache.styles.is_some(),
            "a block at exactly the byte threshold must highlight synchronously"
        );
        assert!(
            over_cache.styles.is_none(),
            "a block one byte over the threshold must defer to the background worker"
        );
    }

    /// A long code block (> `BG_HIGHLIGHT_BYTES`) defers highlighting to a
    /// background worker exactly once; once the worker installs styles, a
    /// subsequent frame must not spawn any new worker. The uppercase language
    /// exercises the same normalization used by the background path.
    #[gpui::test]
    fn long_code_block_defers_highlight_to_background_once(cx: &mut TestAppContext) {
        init_markdown_test(cx);
        reset_background_probe();
        // ~38 bytes/line × 1000 lines ≈ 38 KiB > BG_HIGHLIGHT_BYTES.
        let source = format!(
            "```PYTHON\n{}\n```",
            "name = 'world'\nprint(f'hello {name}')\n".repeat(1000)
        );
        let (_, cx) = cx.add_window_view(|window, cx| {
            let content = cx.new(|cx| CodeSelectionTestRoot {
                body: MarkdownBody::new(&source, 7, cx),
            });
            Root::new(content, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert_eq!(
            background_probe().background_spawns,
            1,
            "long code block must defer to a background highlight worker"
        );

        // Drive the worker to completion, then re-render: styles are now
        // present, so no second worker is spawned.
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert_eq!(
            background_probe().background_spawns,
            1,
            "once background styles are installed, no re-spawn may occur"
        );
        let probe = background_probe();
        assert_eq!(probe.background_installs, 1);
        assert_eq!(probe.last_generation, Some(0));
        assert!(
            probe.last_style_count > 1,
            "a completed uppercase-Python worker must install syntax styles"
        );
    }

    /// Replacing a long block while its worker is in flight drops (cancels)
    /// the stale task, so the render path spawns a fresh worker for the new
    /// generation instead of waiting on the old one. A result from the old
    /// generation is discarded; only the current generation may install.
    #[gpui::test]
    fn cache_replacement_drops_stale_task_and_discards_stale_result(cx: &mut TestAppContext) {
        init_markdown_test(cx);
        reset_background_probe();
        let old_source = FencedCode {
            code: "fn main() { let value = 0; }".repeat(800).into(),
            language: Some("Rust".into()),
            start: 0,
        };
        let new_source = FencedCode {
            code: "fn main() { let value = 1; }".repeat(800).into(),
            language: Some("Rust".into()),
            start: 0,
        };
        let mut cache = cx.update(|cx| HighlightCache::new(&old_source, cx));
        cache.highlight_task = Some(Task::ready(()));
        cache.highlight_task_generation = Some(cache.generation);
        let old_generation = cache.generation;
        cx.update(|cx| cache.replace(&new_source, cx));

        assert_eq!(cache.generation, old_generation + 1);
        // The stale task and its generation guard are dropped on rebuild, so
        // the next render will spawn a fresh worker for the new generation.
        assert!(cache.highlight_task.is_none());
        assert!(cache.highlight_task_generation.is_none());
        assert!(cache.styles.is_none());

        // A late result from the old generation is discarded by the guard.
        let stale_styles: CodeStyles = vec![(0..3, HighlightStyle::default())].into();
        assert!(!cache.try_apply_styles(old_generation, stale_styles));
        assert!(cache.styles.is_none());

        // A result for the current generation is accepted.
        let theme = cache.theme.clone();
        let fresh_styles = compute_code_styles(
            new_source.code.as_ref(),
            &normalized_language_id(new_source.language.as_deref()),
            theme.as_ref(),
        );
        assert!(cache.try_apply_styles(cache.generation, fresh_styles));
        assert!(cache.styles.is_some());
    }

    /// Streaming growth end to end through the real render path: once a long
    /// block's content changes, the replaced cache no longer matches, so the
    /// render path starts a fresh background worker for the new generation and
    /// installs that generation's styles when it completes, without further
    /// re-spawning. The drop-on-rebuild semantics of `replace` are asserted
    /// deterministically by `cache_replacement_drops_stale_task_and_discards_stale_result`;
    /// this test confirms the integrated render path restarts the worker and
    /// lets the replacement generation's styles win.
    #[gpui::test]
    fn replacing_a_long_block_restarts_the_background_worker_for_the_new_generation(
        cx: &mut TestAppContext,
    ) {
        init_markdown_test(cx);
        reset_background_probe();
        const OWNER_ID: u64 = 7;
        let old_source = format!(
            "```python\n{}\n```",
            "name = 'old'\nprint(f'hello {name}')\n".repeat(600)
        );
        let new_source = format!(
            "```python\n{}\n```",
            "name = 'new'\nprint(f'hello {name}')\n".repeat(600)
        );
        let content = cx.update(|cx| {
            cx.new(|cx| CodeSelectionTestRoot {
                body: MarkdownBody::new(&old_source, OWNER_ID, cx),
            })
        });
        let (_, cx) = cx.add_window_view(|window, cx| Root::new(content.clone(), window, cx));
        let cx: &mut VisualTestContext = cx;

        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert_eq!(
            background_probe().background_spawns,
            1,
            "first long block defers to a background worker"
        );

        // Change the block content (streaming growth): the cache no longer
        // matches, so the stale worker must be dropped and a fresh worker
        // started for the new generation. Before this fix the stale task was
        // retained, so no new worker was spawned here and the block stayed as
        // a placeholder until a later frame.
        cx.update(|window, cx| {
            content.update(cx, |root, cx| root.body.set_text(&new_source, cx));
            let _ = window.draw(cx);
        });
        assert_eq!(
            background_probe().background_spawns,
            2,
            "a content change must drop the stale worker and spawn a fresh one"
        );

        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let probe = background_probe();
        assert_eq!(
            probe.background_spawns, 2,
            "once the replacement's styles are installed, no further worker may spawn"
        );
        assert_eq!(
            probe.last_generation,
            Some(1),
            "the replacement generation installs its styles"
        );
        assert!(
            probe.last_style_count > 1,
            "the replacement worker must install real syntax styles"
        );
    }

    /// Italic comments and bold keywords are present in the configured themes.
    /// Installing those styles must not change the wrapped code block's bounds
    /// relative to its plain-text placeholder.
    #[gpui::test]
    fn long_wrapped_placeholder_keeps_code_bounds(cx: &mut TestAppContext) {
        init_markdown_test(cx);
        reset_background_probe();
        cx.update(|cx| {
            set_global_wrap_in_memory(true, cx);
            preferences::update_in_memory(cx, |prefs| prefs.code_block_line_numbers = true);
        });
        let source = format!(
            "```Rust\n{}\n```",
            ("// this is a deliberately long comment that exercises wrapped italic text\n")
                .repeat(600)
        );
        let (_, cx) = cx.add_window_view(|window, cx| {
            let content = cx.new(|cx| CodeSelectionTestRoot {
                body: MarkdownBody::new(&source, 7, cx),
            });
            Root::new(content, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let placeholder = cx
            .debug_bounds("markdown-code-line-7-0-0")
            .expect("wrapped code placeholder bounds");

        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let highlighted = cx
            .debug_bounds("markdown-code-line-7-0-0")
            .expect("wrapped highlighted code bounds");
        assert_eq!(
            placeholder.size, highlighted.size,
            "background styles must not change wrapped code layout"
        );
    }
}
