//! Fenced-code extension, retained highlight cache, and code-block layout.

use crate::runtime::ContributionId;

use super::*;

pub(super) fn is_fenced_code_source(source: &str) -> bool {
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
pub(super) struct FencedCode {
    pub(super) code: SharedString,
    pub(super) language: Option<SharedString>,
    pub(super) start: usize,
}

const FENCED_CODE_EXTENSION_ID: ContributionId = ContributionId::new("nostra.markdown.fenced-code");
const FENCED_CODE_EXTENSION_ORDER: u32 = 30;

pub(crate) fn fenced_code_contribution() -> MarkdownExtensionDefinition {
    MarkdownExtensionDefinition::new(
        FENCED_CODE_EXTENSION_ID,
        FENCED_CODE_EXTENSION_ORDER,
        MarkdownExtensionInstaller::new(install_fenced_code),
    )
}

fn install_fenced_code(
    extensions: MarkdownExtensions,
    context: &MarkdownExtensionContext,
) -> MarkdownExtensions {
    let owner_id = context.owner_id();
    let source_offset = context.source_offset();
    let preference_state = context.preference_state().clone();
    extensions
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
                preference_state.clone(),
                window,
                cx,
            )
        })
}

/// Byte-range → syntax-style mappings for one fenced code block.
pub(super) type CodeStyles = Arc<[(Range<usize>, HighlightStyle)]>;

pub(super) struct HighlightCache {
    code: SharedString,
    /// Normalized syntax language ID used for cache matching and highlighting.
    language: String,
    pub(super) theme: Arc<HighlightTheme>,
    /// Syntax highlight ranges. `None` while a long block is being highlighted
    /// on a background thread; the render path shows a plain-text placeholder
    /// (structure/line numbers/controls intact) until the worker finishes.
    pub(super) styles: Option<CodeStyles>,
    line_count: usize,
    line_numbers: SharedString,
    /// Bumped on every rebuild so a stale background result is discarded.
    pub(super) generation: u64,
    /// The in-flight background highlight task. Keeping it here prevents a new
    /// task from being spawned on every frame. On a cache rebuild this task is
    /// dropped, which cancels a worker that has not started yet and discards
    /// the result of one already running (a synchronous tree-sitter parse
    /// cannot be preempted mid-poll). Either way the new generation immediately
    /// starts a fresh worker instead of waiting on a stale result.
    pub(super) highlight_task: Option<Task<()>>,
    /// Generation owned by `highlight_task`; prevents a stale callback from
    /// clearing a task that was started for a newer cache generation.
    pub(super) highlight_task_generation: Option<u64>,
}

impl HighlightCache {
    pub(super) fn new(code: &FencedCode, cx: &App) -> Self {
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

    pub(super) fn replace(&mut self, code: &FencedCode, cx: &App) {
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
    pub(super) fn try_apply_styles(&mut self, generation: u64, styles: CodeStyles) -> bool {
        if self.generation != generation {
            return false;
        }
        self.styles = Some(styles);
        true
    }

    pub(super) fn matches(&self, code: &FencedCode, cx: &App) -> bool {
        // `language` is already normalized; compare the raw input case-insensitively
        // to avoid allocating another normalized string on every render.
        self.code == code.code
            && self
                .language
                .eq_ignore_ascii_case(code.language.as_deref().unwrap_or("text"))
            && self.theme == cx.theme().highlight_theme
    }
}

pub(super) fn normalized_language_id(language: Option<&str>) -> String {
    language.unwrap_or("text").to_ascii_lowercase()
}

/// Run tree-sitter syntax highlighting for a complete code block. Invoked
/// synchronously for short blocks in `HighlightCache::new` and on a background
/// thread for long blocks (see `render`), so it must depend only on the
/// thread-safe `LanguageRegistry::singleton()` and never on UI-thread state.
pub(super) fn compute_code_styles(
    code: &str,
    language: &str,
    theme: &HighlightTheme,
) -> CodeStyles {
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
pub(super) fn parse_code_styles(
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
pub(super) struct HighlightedLine {
    pub(super) styles: Vec<(Range<usize>, HighlightStyle)>,
}

#[derive(Clone, Copy)]
pub(super) struct WrapState {
    pub(super) enabled: bool,
    pub(super) global_revision: u64,
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
pub(super) fn highlighted_lines(
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

pub(super) fn code_text_element(
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

pub(super) fn render(
    code: &FencedCode,
    attached_text_state: Option<&SelectableTextState>,
    owner_id: u64,
    preference_state: PreferenceState,
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

    let (global_wrap, global_wrap_revision, show_line_numbers) = {
        let preferences = match preference_state.lock() {
            Ok(preferences) => preferences,
            Err(poisoned) => poisoned.into_inner(),
        };
        (
            preferences.code_block_wrap,
            preferences.code_block_wrap_revision,
            preferences.code_block_line_numbers,
        )
    };
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
                                .mode(ScrollbarMode::Always),
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
pub(super) fn spawn_background_highlight(
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
pub(super) struct CodeSurface {
    pub(super) background: Background,
    pub(super) color: Hsla,
}

pub(super) fn code_header_surface(cx: &App) -> CodeSurface {
    let body = opaque_surface(cx.theme().background, cx.theme().muted);
    themed_distinct_surface(
        cx.theme().tokens.secondary.background,
        opaque_surface(body, cx.theme().secondary),
        body,
        cx.theme().is_dark(),
    )
}

pub(super) fn wrap_toggle_surface(wrap: bool, cx: &App) -> Option<CodeSurface> {
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

pub(super) fn opaque_surface(background: Hsla, surface: Hsla) -> Hsla {
    background.blend(surface).alpha(1.)
}

pub(super) fn themed_distinct_surface(
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

pub(super) fn surface_contrast(a: Hsla, b: Hsla) -> f32 {
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
