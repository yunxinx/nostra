//! Markdown fenced-code rendering and its application-wide display preferences.

use std::{ops::Range, sync::Arc};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Axis, Background, Entity, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement as _, ParentElement as _, Rgba, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
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

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MarkdownPerfProbe {
    pub(crate) text_view_builds: usize,
    pub(crate) code_block_renders: usize,
    pub(crate) code_text_elements: usize,
}

#[cfg(test)]
thread_local! {
    static PERF_PROBE: std::cell::Cell<MarkdownPerfProbe> = const {
        std::cell::Cell::new(MarkdownPerfProbe {
            text_view_builds: 0,
            code_block_renders: 0,
            code_text_elements: 0,
        })
    };
}

#[cfg(test)]
fn update_perf_probe(update: impl FnOnce(&mut MarkdownPerfProbe)) {
    PERF_PROBE.with(|probe| {
        let mut snapshot = probe.get();
        update(&mut snapshot);
        probe.set(snapshot);
    });
}

#[cfg(test)]
pub(crate) fn reset_perf_probe() {
    PERF_PROBE.with(|probe| probe.set(MarkdownPerfProbe::default()));
}

#[cfg(test)]
pub(crate) fn perf_probe() -> MarkdownPerfProbe {
    PERF_PROBE.get()
}

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

#[derive(Clone)]
struct HighlightCache {
    code: SharedString,
    language: Option<SharedString>,
    theme: Arc<HighlightTheme>,
    styles: Arc<[(Range<usize>, HighlightStyle)]>,
    line_count: usize,
    line_numbers: SharedString,
}

impl HighlightCache {
    fn new(code: &FencedCode, cx: &App) -> Self {
        let theme = cx.theme().highlight_theme.clone();
        let language = normalized_language_id(code.language.as_deref());
        let mut highlighter = SyntaxHighlighter::new(&language);
        let rope = Rope::from(code.code.as_ref());
        highlighter.update(None, &rope, None);
        let styles: Arc<[(Range<usize>, HighlightStyle)]> =
            highlighter.styles(&(0..code.code.len()), &theme).into();
        let line_count = code.code.split('\n').count();
        let number_width = line_count.max(1).to_string().len();
        let line_numbers = (1..=line_count)
            .map(|number| format!("{number:>number_width$}"))
            .collect::<Vec<_>>()
            .join("\n")
            .into();

        Self {
            code: code.code.clone(),
            language: code.language.clone(),
            theme,
            styles,
            line_count,
            line_numbers,
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
        cache.update(cx, |cache, cx| *cache = HighlightCache::new(code, cx));
    }
    let styles = cache.read(cx).styles.clone();
    let line_count = cache.read(cx).line_count;
    let line_numbers = cache.read(cx).line_numbers.clone();

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
}
