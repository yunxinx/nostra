//! Markdown fenced-code rendering and its application-wide display preferences.

use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Axis, Background, Entity, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement as _, ParentElement as _, Rgba, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, Rope, Sizable as _,
    button::Toggle,
    clipboard::Clipboard,
    h_flex,
    highlighter::{HighlightTheme, SyntaxHighlighter},
    scroll::{ScrollableMask, Scrollbar, ScrollbarShow},
    text::{
        MarkdownExtensions, MarkdownNode, TextView, TextViewState, TextViewStyle, markdown_ast,
    },
    v_flex,
};
use rust_i18n::t;

use crate::preferences;

const NODE_NAME: &str = "nostra-fenced-code";
const MIN_ADJACENT_SURFACE_CONTRAST: f32 = 1.2;

/// A Markdown body and the stable extension registry that renders its fenced
/// code. Keeping the registry beside the state prevents a new extension
/// revision from forcing a full Markdown reparse on every frame.
pub(crate) struct MarkdownBody {
    state: Entity<TextViewState>,
    extensions: MarkdownExtensions,
    style: SharedTextViewStyle,
    original_source: SharedMarkdownSource,
    source: String,
    extension_syntax_seen: bool,
}

pub(crate) type SharedTextViewStyle = Arc<Mutex<TextViewStyle>>;
pub(crate) type SharedMarkdownSource = Arc<Mutex<String>>;

impl MarkdownBody {
    pub(crate) fn new(source: &str, owner_id: u64, cx: &mut App) -> Self {
        let style = Arc::new(Mutex::new(TextViewStyle::default()));
        let original_source = Arc::new(Mutex::new(source.to_string()));
        let parse_source = super::math::protect_display_math_for_markdown(source);
        Self {
            state: cx.new(|cx| TextViewState::markdown(&parse_source, cx)),
            extensions: extensions(owner_id, 0, style.clone(), original_source.clone()),
            style,
            original_source,
            source: source.to_string(),
            extension_syntax_seen: contains_extension_syntax(source),
        }
    }

    pub(crate) fn push_str(&mut self, delta: &str, cx: &mut App) {
        if delta.is_empty() {
            return;
        }
        let previous_len = self.source.len();
        self.source.push_str(delta);
        if let Ok(mut original) = self.original_source.lock() {
            original.clear();
            original.push_str(&self.source);
        }
        if !self.extension_syntax_seen {
            // A code fence or math delimiter can be split across streamed
            // deltas, so checking `delta` alone is insufficient. Only the
            // final two bytes of the previous source can participate in a new
            // three-byte code fence; two-byte math delimiters need less.
            // Scanning that bounded overlap keeps detection O(total bytes)
            // instead of rescanning the complete response every frame.
            let scan_start = previous_len.saturating_sub(2);
            let fence_seen = contains_fence_marker_bytes(&self.source.as_bytes()[scan_start..]);
            // A complete inline/display formula can only become valid in a
            // delta that contributes a dollar or backslash delimiter. Scan the
            // authoritative source only on those uncommon boundaries. This
            // avoids both quadratic rescans of ordinary prose and permanently
            // switching currency-only messages to synchronous full parsing.
            let math_seen = delta
                .as_bytes()
                .iter()
                .any(|byte| matches!(byte, b'$' | b'\\'))
                && super::math::contains_math_syntax(&self.source);
            self.extension_syntax_seen = fence_seen || math_seen;
        }
        self.state.update(cx, |state, cx| {
            if self.extension_syntax_seen {
                // gpui-component's background append parser keeps a document
                // snapshot that is independent from synchronous extension
                // reparses. Once custom code or math syntax appears, full
                // replacements prevent that stale snapshot from restoring
                // native nodes. The assistant bridge already coalesces provider
                // deltas into one update per frame (or 32 events), bounding
                // this unavoidable repair to one parse per UI batch.
                let parse_source = super::math::protect_display_math_for_markdown(&self.source);
                state.set_text(&parse_source, cx);
            } else {
                state.push_str(delta, cx);
            }
        });
    }

    pub(crate) fn set_text(&mut self, source: &str, cx: &mut App) {
        self.source.clear();
        self.source.push_str(source);
        if let Ok(mut original) = self.original_source.lock() {
            original.clear();
            original.push_str(source);
        }
        // Full snapshots are allowed to replace earlier streamed content, so
        // derive the mode from the authoritative replacement rather than
        // retaining a marker that may no longer exist.
        self.extension_syntax_seen = contains_extension_syntax(source);
        let parse_source = super::math::protect_display_math_for_markdown(source);
        self.state
            .update(cx, |state, cx| state.set_text(&parse_source, cx));
    }

    pub(crate) fn text_view(&self, style: TextViewStyle) -> TextView {
        // Block renderers run later during TextView layout and do not receive
        // the caller's `TextViewStyle`. Keep a shared snapshot beside the
        // stable extension registry so nested Markdown around display formulas
        // inherits paragraph gaps and future style customizations.
        if let Ok(mut current) = self.style.lock() {
            *current = style.clone();
        }
        TextView::new(&self.state)
            .selectable(true)
            .style(style)
            .markdown_extensions(self.extensions.clone())
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

fn contains_extension_syntax(source: &str) -> bool {
    contains_fence_marker_bytes(source.as_bytes()) || super::math::contains_math_syntax(source)
}

fn contains_fence_marker_bytes(source: &[u8]) -> bool {
    // Work on bytes because Markdown fence delimiters are ASCII. This also
    // lets the streaming caller begin at an arbitrary two-byte overlap without
    // manufacturing a potentially invalid UTF-8 boundary after CJK text.
    source
        .windows(3)
        .any(|window| window == b"```" || window == b"~~~")
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

pub(crate) fn extensions(
    owner_id: u64,
    source_offset: usize,
    style: SharedTextViewStyle,
    source: SharedMarkdownSource,
) -> MarkdownExtensions {
    MarkdownExtensions::default()
        .plugin(super::math::MathPlugin::new(
            owner_id,
            source_offset,
            style,
            source,
        ))
        .block_parser(move |node, cx| {
            let markdown_ast::Node::Code(code) = node else {
                return None;
            };
            let position = code.position.as_ref()?;
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
            let start = source_offset + cx.offset() + position.start.offset;
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
                    .markdown(source.to_string()),
            )
        })
        .block_renderer(NODE_NAME, move |node, window, cx| {
            let Some(code) = node.data::<FencedCode>() else {
                return div().into_any_element();
            };
            render(code, owner_id, window, cx)
        })
}

#[derive(Clone)]
struct HighlightCache {
    code: SharedString,
    language: Option<SharedString>,
    theme: Arc<HighlightTheme>,
    lines: Arc<[HighlightedLine]>,
}

impl HighlightCache {
    fn new(code: &FencedCode, cx: &App) -> Self {
        let theme = cx.theme().highlight_theme.clone();
        let language = normalized_language_id(code.language.as_deref());
        let mut highlighter = SyntaxHighlighter::new(&language);
        let rope = Rope::from(code.code.as_ref());
        highlighter.update(None, &rope, None);
        let styles = highlighter.styles(&(0..code.code.len()), &theme);
        let lines = highlighted_lines(&code.code, &styles);

        Self {
            code: code.code.clone(),
            language: code.language.clone(),
            theme,
            lines,
        }
    }

    fn matches(&self, code: &FencedCode, cx: &App) -> bool {
        self.code == code.code
            && self.language == code.language
            && self.theme == cx.theme().highlight_theme
    }
}

fn normalized_language_id(language: Option<&str>) -> String {
    language.unwrap_or("text").to_ascii_lowercase()
}

struct HighlightedLine {
    text: SharedString,
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

fn highlighted_lines(
    code: &str,
    styles: &[(Range<usize>, HighlightStyle)],
) -> Arc<[HighlightedLine]> {
    // SyntaxHighlighter returns non-overlapping ranges ordered by byte offset.
    // Advance one monotonic cursor as line starts move forward so every style
    // is skipped at most once; a per-line scan of the complete style list
    // would turn ordinary multi-line code into O(lines * styles) render work.
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
                    if clipped_start >= clipped_end {
                        return None;
                    }
                    Some((clipped_start - line_start..clipped_end - line_start, *style))
                })
                .collect();
            let highlighted = HighlightedLine {
                text: line.to_string().into(),
                styles: line_styles,
            };
            line_start = line_end.saturating_add(1);
            highlighted
        })
        .collect()
}

fn styled_line(line: &HighlightedLine) -> StyledText {
    // `StyledText` owns its delayed highlights, so each rendered line must
    // materialize that small Vec. Borrowing the cached projection here avoids
    // first cloning every line and all of their highlight Vecs on each render.
    StyledText::new(line.text.clone()).with_highlights(line.styles.iter().cloned())
}

fn render(code: &FencedCode, owner_id: u64, window: &mut Window, cx: &mut App) -> AnyElement {
    let cache_id: SharedString =
        format!("markdown-code-highlight-{owner_id}-{}", code.start).into();
    let cache = window.use_keyed_state(cache_id, cx, |_, cx| HighlightCache::new(code, cx));
    if !cache.read(cx).matches(code, cx) {
        cache.update(cx, |cache, cx| *cache = HighlightCache::new(code, cx));
    }
    // Keep an immutable snapshot alive while the element tree is assembled.
    // Cloning this Arc is constant-time; cloning the former Vec here deep-copied
    // every line and its token styles on every repaint.
    let lines = cache.read(cx).lines.clone();

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
    let line_count = lines.len();
    let number_width = line_count.max(1).to_string().len();
    let line_number_color = cx
        .theme()
        .highlight_theme
        .style
        .editor_line_number
        .unwrap_or(cx.theme().muted_foreground);

    let mut has_horizontal_overflow = false;
    let code_content = if wrap {
        v_flex()
            .w_full()
            .min_w_0()
            .children(lines.iter().enumerate().map(|(index, line)| {
                h_flex()
                    .debug_selector(move || {
                        format!("markdown-code-line-{owner_id}-{}-{index}", code.start)
                    })
                    .w_full()
                    .min_w_0()
                    .items_start()
                    .when(show_line_numbers, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .pr_3()
                                .text_color(line_number_color)
                                .debug_selector(move || {
                                    format!(
                                        "markdown-code-line-number-{owner_id}-{}-{index}",
                                        code.start
                                    )
                                })
                                .child(format!("{:>number_width$}", index + 1)),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .child(styled_line(line)),
                    )
            }))
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
                            .child(v_flex().flex_none().children(lines.iter().enumerate().map(
                                |(index, line)| {
                                    div()
                                        .debug_selector(move || {
                                            format!(
                                                "markdown-code-line-{owner_id}-{}-{index}",
                                                code.start
                                            )
                                        })
                                        .whitespace_nowrap()
                                        .child(styled_line(line))
                                },
                            ))),
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
                    v_flex()
                        .flex_none()
                        .pr_3()
                        .text_color(line_number_color)
                        .debug_selector(move || {
                            format!("markdown-code-line-numbers-{owner_id}-{}", code.start)
                        })
                        .children((1..=line_count).map(|number| {
                            div()
                                .debug_selector(move || {
                                    format!(
                                        "markdown-code-line-number-{owner_id}-{}-{}",
                                        code.start,
                                        number - 1
                                    )
                                })
                                .child(format!("{number:>number_width$}"))
                        })),
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
    use gpui::TestAppContext;
    use gpui_component::ActiveTheme as _;

    use super::*;

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

    #[test]
    fn detects_backtick_and_tilde_fences() {
        assert!(contains_fence_marker_bytes(b"before\n```Python\n"));
        assert!(contains_fence_marker_bytes(b"before\n~~~RS\n"));
        assert!(!contains_fence_marker_bytes(b"ordinary `inline` code"));
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
    fn streamed_fence_detection_handles_markers_split_across_deltas(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut body = cx.update(|cx| MarkdownBody::new("prefix `", 1, cx));

        cx.update(|cx| body.push_str("`", cx));
        assert!(!body.extension_syntax_seen);
        cx.update(|cx| body.push_str("`rust\nfn main() {}", cx));
        assert!(
            body.extension_syntax_seen,
            "the two-byte overlap must detect an opener split over three batches"
        );
    }

    #[gpui::test]
    fn authoritative_replacement_recomputes_fence_mode(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut body = cx.update(|cx| MarkdownBody::new("```rust\ncode\n```", 1, cx));
        assert!(body.extension_syntax_seen);

        cx.update(|cx| body.set_text("plain replacement\n", cx));
        assert!(
            !body.extension_syntax_seen,
            "a terminal snapshot without a fence must return to incremental prose parsing"
        );
    }

    #[gpui::test]
    fn streamed_math_detection_handles_split_delimiters(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut body = cx.update(|cx| MarkdownBody::new("price ", 1, cx));

        cx.update(|cx| body.push_str("$", cx));
        assert!(!body.extension_syntax_seen);
        cx.update(|cx| body.push_str("x", cx));
        assert!(!body.extension_syntax_seen);
        cx.update(|cx| body.push_str("$", cx));
        assert!(body.extension_syntax_seen);

        cx.update(|cx| body.set_text("plain replacement\n", cx));
        assert!(!body.extension_syntax_seen);
        cx.update(|cx| body.push_str(r"\", cx));
        assert!(!body.extension_syntax_seen);
        cx.update(|cx| body.push_str("[", cx));
        assert!(!body.extension_syntax_seen);
        cx.update(|cx| body.push_str("x", cx));
        assert!(!body.extension_syntax_seen);
        cx.update(|cx| body.push_str(r"\]", cx));
        assert!(
            body.extension_syntax_seen,
            "split bracket delimiters must switch modes once the formula closes"
        );

        cx.update(|cx| body.set_text("Costs $5 and $10 today", cx));
        assert!(
            !body.extension_syntax_seen,
            "currency must remain on the incremental prose path"
        );
    }

    #[gpui::test]
    fn streamed_display_math_refreshes_the_original_source_before_protected_parsing(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let mut body = cx.update(|cx| MarkdownBody::new("prefix\n\n$$\n", 1, cx));

        cx.update(|cx| body.push_str("=\n", cx));
        assert!(matches!(
            crate::ui::math::protect_display_math_for_markdown(&body.source),
            std::borrow::Cow::Borrowed(_)
        ));
        cx.update(|cx| body.push_str("$$", cx));

        let original = body
            .original_source
            .lock()
            .expect("test source lock")
            .clone();
        assert_eq!(original, "prefix\n\n$$\n=\n$$");
        assert_ne!(
            crate::ui::math::protect_display_math_for_markdown(&original).as_ref(),
            original,
            "the completed fence must use the protected TextView parse source"
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
}
