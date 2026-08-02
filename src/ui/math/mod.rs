//! Native Markdown math rendering backed by RaTeX.
//!
//! Formula images are atomic items in gpui-component's public `InlineFlow`, so
//! they share its shaping, Unicode wrapping, baseline, hit-testing, selection,
//! and plain-text copy behavior with surrounding Markdown text.

mod parse;
mod render;

use std::ops::Range;

use gpui::{
    AnyElement, App, ElementId, FontStyle, FontWeight, HighlightStyle, InteractiveElement as _,
    IntoElement, ObjectFit, ParentElement, ScrollHandle, SharedString, StrikethroughStyle,
    Styled as _, StyledImage as _, TextStyle, UnderlineStyle, Window, div, img, px, relative,
};
use gpui_component::{
    ActiveTheme as _,
    scroll::horizontal_scroll_area,
    text::{
        InlineFlow, InlineFlowItem, InlineFlowState, InlineMetrics, MarkdownNode,
        MarkdownParseContext, MarkdownPlugin, markdown_ast,
    },
    v_flex,
};

use self::{
    parse::{
        MathBlockSegment, MathMarkRange, MathSegment, block_math_segments, inline_math_segments,
        supports_inline_math_node,
    },
    render::{FormulaRequest, FormulaStyle, cached_formula},
};

const NODE_NAME: &str = "nostra-math";
const DISPLAY_FALLBACK_LINE_HEIGHT: f32 = 1.2;
const DISPLAY_FALLBACK_SCALE: f32 = 1.18;
const MIN_DISPLAY_FALLBACK_SIZE: f32 = 12.0;

pub(super) fn contains_math_syntax(source: &str) -> bool {
    parse::contains_math_syntax(source)
}

pub(super) fn protect_display_math_for_markdown(source: &str) -> std::borrow::Cow<'_, str> {
    parse::protect_display_math_for_markdown(source)
}

#[derive(Clone)]
pub(crate) struct MathPlugin {
    owner_id: u64,
    source_offset: usize,
    style: crate::ui::markdown::SharedTextViewStyle,
    source: crate::ui::markdown::SharedMarkdownSource,
}

impl MathPlugin {
    pub(crate) fn new(
        owner_id: u64,
        source_offset: usize,
        style: crate::ui::markdown::SharedTextViewStyle,
        source: crate::ui::markdown::SharedMarkdownSource,
    ) -> Self {
        Self {
            owner_id,
            source_offset,
            style,
            source,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MathNode {
    Paragraph {
        segments: Vec<MathSegment>,
    },
    Heading {
        level: u8,
        segments: Vec<MathSegment>,
    },
    Blocks {
        segments: Vec<MathBlockSegment>,
    },
}

impl MarkdownPlugin for MathPlugin {
    fn is_block(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        NODE_NAME
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        cx: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let heading_level = match node {
            markdown_ast::Node::Paragraph(_) => None,
            markdown_ast::Node::Heading(heading) => Some(heading.depth),
            _ => return None,
        };
        if !supports_inline_math_node(node) {
            return None;
        }
        let position = node.position()?;
        let local_start = cx.offset() + position.start.offset;
        let local_end = cx.offset() + position.end.offset;
        // TextView parses a length-preserving protected view of display math.
        // Resolve the mdast span against the independently retained original
        // source so RaTeX never sees the masking spaces. `cx.offset()` matters
        // for gpui-component's append parser; `source_offset` is added only to
        // document identities, not when slicing a nested fragment's source.
        let source = self
            .source
            .lock()
            .ok()
            .and_then(|source| source.get(local_start..local_end).map(str::to_string))
            .or_else(|| cx.node_source(node).map(str::to_string))?;
        let start = self.source_offset + local_start;

        if heading_level.is_none()
            && let Some(segments) = block_math_segments(&source, start)
        {
            let text = block_math_segments_text(&segments);
            let flow_states = segments
                .iter()
                .map(|segment| (InlineFlowState::default(), segment.breaks_before()))
                .collect::<Vec<_>>();
            return Some(
                MarkdownNode::new(NODE_NAME, MathNode::Blocks { segments })
                    .text(text)
                    .markdown(source)
                    .inline_flow_states_with_breaks(flow_states),
            );
        }

        inline_math_segments(&source).map(|segments| {
            // Inline parsing reports offsets relative to this paragraph. Cache
            // identities and debug selectors need document-space offsets so
            // streaming appends do not invalidate preceding formulas.
            let segments = segments
                .into_iter()
                .map(|segment| match segment {
                    MathSegment::Text { text, marks } => MathSegment::Text { text, marks },
                    MathSegment::Image { url, title, link } => {
                        MathSegment::Image { url, title, link }
                    }
                    MathSegment::Formula {
                        source,
                        plain_text,
                        start: offset,
                        display,
                        marks,
                    } => MathSegment::Formula {
                        source,
                        plain_text,
                        start: start + offset,
                        display,
                        marks,
                    },
                })
                .collect::<Vec<_>>();
            let text = math_segments_text(&segments);
            let math = heading_level.map_or_else(
                || MathNode::Paragraph {
                    segments: segments.clone(),
                },
                |level| MathNode::Heading {
                    level,
                    segments: segments.clone(),
                },
            );
            let mut node = MarkdownNode::new(NODE_NAME, math)
                .text(text)
                .markdown(source)
                .inline_flow_state(InlineFlowState::default());
            if let Some(level) = heading_level {
                node = node.heading(level);
            }
            node
        })
    }

    fn render(&self, node: &MarkdownNode, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Some(math) = node.data::<MathNode>() else {
            return div().into_any_element();
        };
        let text_style = window.text_style();
        let font_size = f32::from(text_style.font_size.to_pixels(window.rem_size()));
        let typography = FormulaTypography {
            font_size,
            inherited_style: inherited_formula_style(&text_style),
        };

        match math {
            MathNode::Paragraph { segments } => render_inline_segments(
                segments,
                self.owner_id,
                typography,
                node.attached_inline_flow_state()
                    .cloned()
                    .unwrap_or_default(),
                math_segments_start(segments),
                window,
                cx,
            ),
            MathNode::Heading { level, segments } => {
                let style = self
                    .style
                    .lock()
                    .map(|style| style.clone())
                    .unwrap_or_default();
                let heading = style.heading_style(*level);
                render_inline_segments(
                    segments,
                    self.owner_id,
                    FormulaTypography {
                        font_size: f32::from(heading.font_size),
                        inherited_style: formula_style_for_heading(
                            typography.inherited_style,
                            heading.font_weight,
                        ),
                    },
                    node.attached_inline_flow_state()
                        .cloned()
                        .unwrap_or_default(),
                    math_segments_start(segments),
                    window,
                    cx,
                )
            }
            MathNode::Blocks { segments } => {
                let states = node.attached_inline_flow_states();
                let children = segments
                    .iter()
                    .enumerate()
                    .map(|(segment_ix, segment)| {
                        let state = states.get(segment_ix).cloned().unwrap_or_default();
                        match segment {
                            MathBlockSegment::Inline {
                                segments, start, ..
                            } => render_inline_segments(
                                segments,
                                self.owner_id,
                                typography,
                                state,
                                *start,
                                window,
                                cx,
                            ),
                            MathBlockSegment::Formula {
                                source,
                                plain_text,
                                start,
                                marks,
                                ..
                            } => render_display_formula_row(
                                DisplayFormulaRow {
                                    source,
                                    plain_text,
                                    start: *start,
                                    marks,
                                    owner_id: self.owner_id,
                                    font_size,
                                    inherited_style: typography.inherited_style,
                                    flow_state: state,
                                },
                                window,
                                cx,
                            ),
                        }
                    })
                    .collect::<Vec<_>>();
                v_flex()
                    .w_full()
                    .min_w_0()
                    .children(children)
                    .into_any_element()
            }
        }
    }
}

#[derive(Clone, Copy)]
struct FormulaTypography {
    font_size: f32,
    inherited_style: FormulaStyle,
}

fn render_inline_segments(
    segments: &[MathSegment],
    owner_id: u64,
    typography: FormulaTypography,
    flow_state: InlineFlowState,
    flow_start: usize,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let items = segments
        .iter()
        .map(|segment| match segment {
            MathSegment::Text { text, marks } => {
                let mut item =
                    InlineFlowItem::text(text.clone()).highlights(highlight_ranges(marks, cx));
                for (range, url) in link_ranges(marks) {
                    item = item.link(range, url);
                }
                item
            }
            MathSegment::Image { url, title, link } => {
                let item = InlineFlowItem::image(url.clone(), title.clone(), None, None);
                if let Some(link) = link {
                    item.image_link(link.clone())
                } else {
                    item
                }
            }
            MathSegment::Formula {
                source,
                plain_text,
                start,
                display,
                marks,
            } => {
                let color = if marks.link.is_some() {
                    cx.theme().link
                } else {
                    window.text_style().color
                };
                cached_formula(
                    FormulaRequest {
                        source,
                        inline: !display,
                        style: formula_style(marks, typography.inherited_style),
                        start: *start,
                        owner_id,
                        font_size: typography.font_size,
                        color,
                    },
                    window,
                    cx,
                )
                .map(|formula| {
                    let selector = format!("markdown-math-{owner_id}-{start}");
                    let mut item = InlineFlowItem::custom(
                        plain_text.clone(),
                        InlineMetrics::new(formula.width, formula.ascent, formula.descent),
                        div()
                            .id(format!("{selector}-content"))
                            .flex_none()
                            .debug_selector(move || selector.clone())
                            .w(formula.width)
                            .h(formula.height)
                            .child(
                                img(formula.image)
                                    .object_fit(ObjectFit::Contain)
                                    .w_full()
                                    .h_full(),
                            ),
                    );
                    if let Some(link) = &marks.link {
                        item = item.custom_link(link.clone());
                    }
                    item
                })
                .unwrap_or_else(|| {
                    let mut item = InlineFlowItem::text(plain_text.clone()).highlights(vec![(
                        0..plain_text.len(),
                        formula_fallback_highlight(marks, color, cx),
                    )]);
                    if let Some(link) = &marks.link {
                        item = item.link(0..plain_text.len(), link.clone());
                    }
                    item
                })
            }
        })
        .collect();
    let flow_id: SharedString = format!("markdown-math-flow-{owner_id}-{flow_start}").into();

    div()
        .w_full()
        .min_w_0()
        .debug_selector({
            let flow_id = flow_id.clone();
            move || flow_id.to_string()
        })
        .child(InlineFlow::new(ElementId::Name(flow_id), flow_state, items))
        .into_any_element()
}

fn math_segments_start(segments: &[MathSegment]) -> usize {
    segments
        .iter()
        .find_map(|segment| match segment {
            MathSegment::Formula { start, .. } => Some(*start),
            MathSegment::Text { .. } | MathSegment::Image { .. } => None,
        })
        .unwrap_or_default()
}

fn math_segments_text(segments: &[MathSegment]) -> String {
    let mut text = String::new();
    for segment in segments {
        match segment {
            MathSegment::Text { text: segment, .. } => text.push_str(segment),
            MathSegment::Formula { plain_text, .. } => text.push_str(plain_text),
            MathSegment::Image { .. } => {}
        }
    }
    text
}

fn block_math_segments_text(segments: &[MathBlockSegment]) -> String {
    let mut text = String::new();
    let mut has_visible_segment = false;
    let mut pending_breaks = 0;
    for segment in segments {
        if has_visible_segment {
            pending_breaks += segment.breaks_before();
        }
        let segment_text = match segment {
            MathBlockSegment::Inline { segments, .. } => math_segments_text(segments),
            MathBlockSegment::Formula { plain_text, .. } => plain_text.clone(),
        };
        if segment_text.is_empty() {
            continue;
        }
        if has_visible_segment {
            text.extend(std::iter::repeat_n('\n', pending_breaks));
        }
        text.push_str(&segment_text);
        has_visible_segment = true;
        pending_breaks = 0;
    }
    text
}

struct DisplayFormulaRow<'a> {
    source: &'a str,
    plain_text: &'a str,
    start: usize,
    marks: &'a parse::MathMarks,
    owner_id: u64,
    font_size: f32,
    inherited_style: FormulaStyle,
    flow_state: InlineFlowState,
}

fn render_display_formula_row(
    formula: DisplayFormulaRow<'_>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let DisplayFormulaRow {
        source,
        plain_text,
        start,
        marks,
        owner_id,
        font_size,
        inherited_style,
        flow_state,
    } = formula;
    let key: SharedString = format!("markdown-math-scroll-{owner_id}-{start}").into();
    let scroll_handle = window
        .use_keyed_state(key, cx, |_, _| ScrollHandle::default())
        .read(cx)
        .clone();
    let style = gpui::StyleRefinement::default();
    let color = if marks.link.is_some() {
        cx.theme().link
    } else {
        window.text_style().color
    };
    let rendered = cached_formula(
        FormulaRequest {
            source,
            inline: false,
            style: formula_style(marks, inherited_style),
            start,
            owner_id,
            font_size,
            color,
        },
        window,
        cx,
    );
    let (item, formula_width) = if let Some(formula) = rendered {
        let selector = format!("markdown-math-{owner_id}-{start}");
        let mut item = InlineFlowItem::custom(
            plain_text.to_string(),
            InlineMetrics::new(formula.width, formula.ascent, formula.descent),
            div()
                .id(format!("{selector}-content"))
                .flex_none()
                .debug_selector(move || selector.clone())
                .w(formula.width)
                .h(formula.height)
                .child(
                    img(formula.image)
                        .object_fit(ObjectFit::Contain)
                        .w_full()
                        .h_full(),
                ),
        );
        if let Some(link) = &marks.link {
            item = item.custom_link(link.clone());
        }
        (item, Some(formula.width))
    } else {
        let mut item = InlineFlowItem::text(plain_text.to_string()).highlights(vec![(
            0..plain_text.len(),
            formula_fallback_highlight(marks, color, cx),
        )]);
        if let Some(link) = &marks.link {
            item = item.link(0..plain_text.len(), link.clone());
        }
        (item, None)
    };
    let flow_id: SharedString = format!("markdown-math-block-flow-{owner_id}-{start}").into();
    let flow = InlineFlow::new(ElementId::Name(flow_id), flow_state, vec![item]);
    let mut track = div().min_w_full().flex().justify_center();
    if let Some(formula_width) = formula_width {
        // Size the scroll track to max(viewport, formula). Without the definite
        // intrinsic width, an oversized centered child merely paints beyond a
        // viewport-sized track and ScrollHandle observes no horizontal range.
        track = track.w(formula_width);
    }
    div()
        .w_full()
        .py_1()
        .line_height(relative(DISPLAY_FALLBACK_LINE_HEIGHT))
        .text_size(px(
            (font_size * DISPLAY_FALLBACK_SCALE).max(MIN_DISPLAY_FALLBACK_SIZE)
        ))
        .text_color(color)
        .debug_selector(move || format!("markdown-math-block-row-{owner_id}-{start}"))
        .child(horizontal_scroll_area(
            ElementId::Name(format!("markdown-math-block-scroll-{owner_id}-{start}").into()),
            &scroll_handle,
            &style,
            track.child(flow),
        ))
        .into_any_element()
}

fn link_ranges(marks: &[MathMarkRange]) -> Vec<(Range<usize>, String)> {
    marks
        .iter()
        .filter_map(|mark| {
            mark.marks
                .link
                .as_ref()
                .map(|url| (mark.range.clone(), url.clone()))
        })
        .collect()
}

fn highlight_ranges(marks: &[MathMarkRange], cx: &App) -> Vec<(Range<usize>, HighlightStyle)> {
    marks
        .iter()
        .map(|mark| (mark.range.clone(), highlight_for_marks(&mark.marks, cx)))
        .collect()
}

fn highlight_for_marks(marks: &parse::MathMarks, cx: &App) -> HighlightStyle {
    let mut highlight = HighlightStyle::default();
    if marks.bold {
        highlight.font_weight = Some(FontWeight::BOLD);
    }
    if marks.italic {
        highlight.font_style = Some(FontStyle::Italic);
    }
    if marks.strikethrough {
        highlight.strikethrough = Some(StrikethroughStyle {
            thickness: px(1.),
            ..Default::default()
        });
    }
    if marks.code {
        highlight.background_color = Some(cx.theme().accent);
    }
    if marks.link.is_some() {
        highlight.color = Some(cx.theme().link);
        highlight.underline = Some(UnderlineStyle {
            thickness: px(1.),
            ..Default::default()
        });
    }
    highlight
}

fn formula_fallback_highlight(
    marks: &parse::MathMarks,
    color: gpui::Hsla,
    cx: &App,
) -> HighlightStyle {
    let mut highlight = highlight_for_marks(marks, cx);
    highlight.color = Some(color);
    // Formula fallbacks remain visually distinguishable from surrounding prose
    // while retaining bold, strikethrough, code, and link decorations.
    highlight.font_style = Some(FontStyle::Italic);
    highlight
}

fn inherited_formula_style(inherited: &TextStyle) -> FormulaStyle {
    FormulaStyle {
        bold: inherited.font_weight >= FontWeight::SEMIBOLD,
        italic: inherited.font_style != FontStyle::Normal,
        strikethrough: inherited.strikethrough.is_some(),
    }
}

fn formula_style_for_heading(mut inherited: FormulaStyle, font_weight: FontWeight) -> FormulaStyle {
    inherited.bold |= font_weight >= FontWeight::SEMIBOLD;
    inherited
}

fn formula_style(marks: &parse::MathMarks, inherited: FormulaStyle) -> FormulaStyle {
    FormulaStyle {
        bold: marks.bold || inherited.bold,
        italic: marks.italic || inherited.italic,
        strikethrough: marks.strikethrough || inherited.strikethrough,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formula_style_merges_markdown_marks_with_inherited_heading_typography() {
        let inherited = TextStyle {
            font_weight: FontWeight::SEMIBOLD,
            font_style: FontStyle::Oblique,
            strikethrough: Some(StrikethroughStyle::default()),
            ..TextStyle::default()
        };
        assert_eq!(
            inherited_formula_style(&inherited),
            FormulaStyle {
                bold: true,
                italic: true,
                strikethrough: true,
            }
        );
        assert!(!formula_style_for_heading(FormulaStyle::default(), FontWeight::MEDIUM).bold);
        assert!(formula_style_for_heading(FormulaStyle::default(), FontWeight::SEMIBOLD).bold);
    }
}
