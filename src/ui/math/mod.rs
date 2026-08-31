//! Native Markdown math rendering backed by RaTeX.
//!
//! gpui-component retains the original Markdown source and owns the native
//! paragraph flow. Nostra contributes only its delimiter policy and atomic
//! formula renderer, so marks, links, images, headings, hard breaks, wrapping,
//! selection, and copying continue through the component's standard path.

mod parse;
mod render;

use gpui::{
    AnyElement, App, ElementId, FontStyle, FontWeight, HighlightStyle, InteractiveElement as _,
    IntoElement as _, ObjectFit, ParentElement as _, ScrollHandle, SharedString, Styled as _,
    StyledImage as _, TextStyle, Window, div, img, px, relative,
};
use gpui_component::{
    scroll::horizontal_scroll_area,
    text::{
        InlineFlow, InlineFlowItem, InlineFlowState, InlineMetrics, MarkdownExtensions,
        MarkdownInline, MarkdownInlineRenderContext, MarkdownNode, MarkdownParseContext,
        markdown_ast,
    },
};
use rust_i18n::t;

use crate::{
    runtime::ContributionId,
    ui::markdown::{
        MarkdownExtensionContext, MarkdownExtensionDefinition, MarkdownExtensionInstaller,
    },
};

use self::{
    parse::RecognizedFormula,
    render::{FormulaRequest, FormulaStyle, RenderedFormula, cached_formula},
};

#[cfg(test)]
pub(crate) use self::render::{formula_cache_snapshot, formula_cache_snapshots};

const NODE_NAME: &str = "nostra-math";
const LITERAL_NODE_NAME: &str = "nostra-math-literal";
pub(crate) const MATH_EXTENSION_ID: ContributionId = ContributionId::new("nostra.markdown.math");
const MATH_EXTENSION_ORDER: u32 = 20;
const DISPLAY_FALLBACK_LINE_HEIGHT: f32 = 1.2;
const DISPLAY_FALLBACK_SCALE: f32 = 1.18;
const MIN_DISPLAY_FALLBACK_SIZE: f32 = 12.0;

#[derive(Clone, Debug, PartialEq, Eq)]
struct MathFormula {
    source: String,
    plain_text: String,
    start: usize,
    display: bool,
}

impl MathFormula {
    fn from_recognized(
        formula: RecognizedFormula,
        node_start: usize,
        source_offset: usize,
    ) -> Self {
        Self {
            source: formula.source,
            plain_text: formula.plain_text,
            start: source_offset + node_start + formula.relative_start,
            display: formula.display,
        }
    }
}

pub(crate) fn markdown_contribution() -> MarkdownExtensionDefinition {
    MarkdownExtensionDefinition::new(
        MATH_EXTENSION_ID,
        MATH_EXTENSION_ORDER,
        MarkdownExtensionInstaller::new(|extensions, context: &MarkdownExtensionContext| {
            extend(extensions, context.owner_id(), context.source_offset())
        }),
    )
}

pub(super) fn extend(
    extensions: MarkdownExtensions,
    owner_id: u64,
    source_offset: usize,
) -> MarkdownExtensions {
    extensions
        .parse_options(|options| {
            options.constructs.math_text = true;
            options.constructs.math_flow = true;
            // Nostra's stricter scanner encodes accepted `$...$` spans as
            // private InlineCode nodes. Leaving markdown-rs's broad single-
            // dollar pairing enabled would let rejected currency or a pair
            // crossing native Markdown nodes consume structure before this
            // extension can validate it.
            options.math_text_single_dollar = false;
        })
        .try_prepare_source(parse::try_prepare_math_source)
        .parse_error_formatter(|_| t!("chat.error.markdown").to_string())
        .inline_parser(move |node, cx| parse_inline(node, cx, source_offset))
        .inline_renderer(NODE_NAME, move |node, context, window, cx| {
            render_inline(node, context, owner_id, window, cx)
        })
        .block_parser(move |node, cx| parse_display(node, cx, source_offset))
        .block_renderer(NODE_NAME, move |node, window, cx| {
            render_display(node, owner_id, window, cx)
        })
        .block_renderer(LITERAL_NODE_NAME, |node, _, _| {
            // An unfinished flow-math opener is kept as a literal custom
            // block until the closing delimiter arrives. This preserves the
            // streamed source and avoids the native empty Math/CodeBlock
            // fallback dropping the delimiter from selection and copy.
            div().child(node.as_text().to_string()).into_any_element()
        })
}

fn parse_inline(
    node: &markdown_ast::Node,
    cx: &MarkdownParseContext<'_>,
    source_offset: usize,
) -> Option<MarkdownNode> {
    match node {
        markdown_ast::Node::InlineMath(_) => {
            let original = cx.node_source(node)?;
            parse_inline_formula_node(node, cx, source_offset).or_else(|| {
                Some(
                    MarkdownNode::new(LITERAL_NODE_NAME, ())
                        .text(original.to_string())
                        .markdown(original.to_string()),
                )
            })
        }
        markdown_ast::Node::InlineCode(_) => {
            let original = cx.node_source(node)?;
            let prepared = cx.prepared_node_source(node)?;
            if !parse::is_prepared_single_dollar_formula(original, prepared) {
                return None;
            }
            parse_inline_formula_node(node, cx, source_offset)
        }
        markdown_ast::Node::Text(text) => {
            let original = cx.node_source(node)?;
            let prepared = cx.prepared_node_source(node)?;
            let restored = parse::restore_prepared_literal(original, prepared, &text.value)?;
            Some(
                MarkdownNode::new(LITERAL_NODE_NAME, ())
                    .text(restored)
                    .markdown(original.to_string()),
            )
        }
        _ => None,
    }
}

fn parse_inline_formula_node(
    node: &markdown_ast::Node,
    cx: &MarkdownParseContext<'_>,
    source_offset: usize,
) -> Option<MarkdownNode> {
    let original = cx.node_source(node)?;
    let position = node.position()?;
    let recognized =
        parse::inline_formula_in_context(cx.source(), position.start.offset..position.end.offset)?;
    let formula =
        MathFormula::from_recognized(recognized, cx.node_range(node)?.start, source_offset);
    Some(
        MarkdownNode::new(NODE_NAME, formula.clone())
            .text(formula.plain_text.clone())
            .markdown(original.to_string()),
    )
}

fn parse_display(
    node: &markdown_ast::Node,
    cx: &MarkdownParseContext<'_>,
    source_offset: usize,
) -> Option<MarkdownNode> {
    let (original, ast_value) = match node {
        markdown_ast::Node::Math(math) => (cx.node_source(node)?, math.value.as_str()),
        markdown_ast::Node::Paragraph(paragraph) => {
            let [markdown_ast::Node::InlineMath(math)] = paragraph.children.as_slice() else {
                return None;
            };
            (cx.node_source(node)?, math.value.as_str())
        }
        _ => return None,
    };
    let Some(recognized) = parse::display_formula_from_ast(original, ast_value) else {
        return Some(
            MarkdownNode::new(LITERAL_NODE_NAME, ())
                .text(original.to_string())
                .markdown(original.to_string()),
        );
    };
    let formula =
        MathFormula::from_recognized(recognized, cx.node_range(node)?.start, source_offset);
    Some(display_markdown_node(formula))
}

fn display_markdown_node(formula: MathFormula) -> MarkdownNode {
    let markdown = formula.plain_text.clone();
    MarkdownNode::new(NODE_NAME, formula)
        .text(markdown.clone())
        .markdown(markdown)
        .inline_flow_state(InlineFlowState::default())
}

fn render_inline(
    node: &MarkdownNode,
    context: &MarkdownInlineRenderContext,
    owner_id: u64,
    window: &mut Window,
    cx: &mut App,
) -> Option<MarkdownInline> {
    let formula = node.data::<MathFormula>()?;
    let text_style = context.text_style();
    let font_size = f32::from(text_style.font_size.to_pixels(window.rem_size()));
    let rendered = cached_formula(
        FormulaRequest {
            source: &formula.source,
            inline: !formula.display,
            style: inherited_formula_style(text_style),
            start: formula.start,
            owner_id,
            font_size,
            color: text_style.color,
        },
        window,
        cx,
    )?;
    let selector = format!("markdown-math-{owner_id}-{}", formula.start);
    Some(MarkdownInline::new(
        InlineMetrics::new(rendered.width, rendered.ascent, rendered.descent),
        rendered_formula_element(&rendered, selector),
    ))
}

fn render_display(
    node: &MarkdownNode,
    owner_id: u64,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let Some(formula) = node.data::<MathFormula>() else {
        return div().into_any_element();
    };
    let text_style = window.text_style();
    let font_size = f32::from(text_style.font_size.to_pixels(window.rem_size()));
    render_display_formula_row(
        DisplayFormulaRow {
            formula,
            owner_id,
            font_size,
            inherited_style: inherited_formula_style(&text_style),
            flow_state: node
                .attached_inline_flow_state()
                .cloned()
                .unwrap_or_default(),
        },
        window,
        cx,
    )
}

struct DisplayFormulaRow<'a> {
    formula: &'a MathFormula,
    owner_id: u64,
    font_size: f32,
    inherited_style: FormulaStyle,
    flow_state: InlineFlowState,
}

fn render_display_formula_row(
    row: DisplayFormulaRow<'_>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let DisplayFormulaRow {
        formula,
        owner_id,
        font_size,
        inherited_style,
        flow_state,
    } = row;
    let key: SharedString = format!("markdown-math-scroll-{owner_id}-{}", formula.start).into();
    let scroll_handle = window
        .use_keyed_state(key, cx, |_, _| ScrollHandle::default())
        .read(cx)
        .clone();
    let style = gpui::StyleRefinement::default();
    let color = window.text_style().color;
    let rendered = cached_formula(
        FormulaRequest {
            source: &formula.source,
            inline: false,
            style: inherited_style,
            start: formula.start,
            owner_id,
            font_size,
            color,
        },
        window,
        cx,
    );
    let (item, formula_width) = if let Some(rendered) = rendered {
        let selector = format!("markdown-math-{owner_id}-{}", formula.start);
        (
            InlineFlowItem::custom(
                formula.plain_text.clone(),
                InlineMetrics::new(rendered.width, rendered.ascent, rendered.descent),
                rendered_formula_element(&rendered, selector),
            ),
            Some(rendered.width),
        )
    } else {
        (
            InlineFlowItem::text(formula.plain_text.clone()).highlights(vec![(
                0..formula.plain_text.len(),
                HighlightStyle {
                    color: Some(color),
                    font_style: Some(FontStyle::Italic),
                    ..HighlightStyle::default()
                },
            )]),
            None,
        )
    };
    let flow_id: SharedString =
        format!("markdown-math-block-flow-{owner_id}-{}", formula.start).into();
    let flow = InlineFlow::new(ElementId::Name(flow_id), flow_state, vec![item]);
    let mut track = div().min_w_full().flex().justify_center();
    if let Some(formula_width) = formula_width {
        // A definite intrinsic track width creates real scroll overflow; paint
        // overflow alone would leave ScrollHandle with a zero max offset.
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
        .debug_selector(move || format!("markdown-math-block-row-{owner_id}-{}", formula.start))
        .child(horizontal_scroll_area(
            ElementId::Name(
                format!("markdown-math-block-scroll-{owner_id}-{}", formula.start).into(),
            ),
            &scroll_handle,
            &style,
            track.child(flow),
        ))
        .into_any_element()
}

fn rendered_formula_element(rendered: &RenderedFormula, selector: String) -> AnyElement {
    div()
        .id(format!("{selector}-content"))
        .flex_none()
        .debug_selector(move || selector.clone())
        .w(rendered.width)
        .h(rendered.height)
        .child(
            img(rendered.image.clone())
                .object_fit(ObjectFit::Contain)
                .w_full()
                .h_full(),
        )
        .into_any_element()
}

fn inherited_formula_style(inherited: &TextStyle) -> FormulaStyle {
    FormulaStyle {
        bold: inherited.font_weight >= FontWeight::SEMIBOLD,
        italic: inherited.font_style != FontStyle::Normal,
        strikethrough: inherited.strikethrough.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_native_typography_drives_formula_style() {
        let inherited = TextStyle {
            font_weight: FontWeight::SEMIBOLD,
            font_style: FontStyle::Oblique,
            strikethrough: Some(gpui::StrikethroughStyle::default()),
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
    }

    #[test]
    fn nested_display_node_exports_container_free_markdown() {
        for raw in ["$$\n> x\n> $$", "$$\n  x\n  $$"] {
            let formula = parse::display_formula_from_ast(raw, "x").expect("display formula");
            let node = display_markdown_node(MathFormula::from_recognized(formula, 0, 0));
            assert_eq!(node.as_markdown(), "$$\nx\n$$", "source: {raw:?}");
        }
    }

    #[test]
    fn contextual_inline_start_is_added_exactly_once() {
        let source = "prefix $x$ suffix";
        let node_start = source.find("$x$").expect("formula start");
        let node_range = node_start..node_start + "$x$".len();
        let recognized = parse::inline_formula_in_context(source, node_range)
            .expect("contextual inline formula");
        assert_eq!(recognized.relative_start, 0);

        let source_offset = 17;
        let formula = MathFormula::from_recognized(recognized, node_start, source_offset);
        assert_eq!(formula.start, source_offset + node_start);
    }

    #[test]
    fn markdown_parse_error_message_resolves_in_every_locale() {
        for locale in ["en", "zh-CN"] {
            let resolved = t!("chat.error.markdown", locale = locale).to_string();
            assert!(!resolved.contains("chat.error.markdown"));
        }
    }
}
