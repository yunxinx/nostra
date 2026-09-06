//! Typography presets for transcript row content.
//!
//! Every `TextViewStyle` a row renderer applies is built here so heading
//! scales, paragraph gaps, and table/inline-code treatment live in one place
//! without style constants in renderer files. Colors come from the
//! active theme through `appearance::contrast` derivations, never hardcoded.
//!
//! The component-level [`TextViewStyle`] folds onto the values the active
//! theme already derived, so a field left at its default keeps the themed
//! treatment; only the transcript-specific decisions are set here.

use std::sync::Arc;

use gpui::{App, HighlightStyle, Pixels, px, rems};
use gpui_component::{ActiveTheme as _, text::TextViewStyle};

use crate::appearance::contrast;

/// Visible height of the streaming reasoning preview, in lines of body text.
/// The preview's outer height is exactly this many lines for the whole
/// stream, which keeps the prose below it from moving.
pub(crate) const PREVIEW_LINES: f32 = 6.;

/// Line-count floor of the expanded reasoning height cap, in lines of body
/// text. The cap is an upper bound: content at or below it renders at its
/// natural height, taller content scrolls inside a viewport of exactly the
/// cap (PRD R3).
pub(crate) const BUDGET_MIN_LINES: f32 = 12.;

/// Fraction of the conversation viewport the expanded reasoning height cap
/// may occupy when that is taller than [`BUDGET_MIN_LINES`] lines.
pub(crate) const BUDGET_VIEWPORT_RATIO: f32 = 0.45;

/// Height cap of an expanded reasoning body: the larger of the line-count
/// floor and the viewport share. Tool-result viewports share the same
/// budget scale.
pub(crate) fn reasoning_cap(line_height: Pixels, viewport_height: Pixels) -> Pixels {
    (line_height * BUDGET_MIN_LINES).max(viewport_height * BUDGET_VIEWPORT_RATIO)
}

/// Conservative characters per body-text line for height pre-estimates:
/// shared by the row-height estimator and the reasoning expand pre-check so
/// both agree on how much text a line holds.
pub(crate) const ESTIMATE_CHARS_PER_LINE: f32 = 62.;

/// Tool results above this many bytes use the budgeted, internally
/// scrollable viewport instead of natural height.
pub(crate) const RESULT_BUDGET_BYTES: usize = 8 * 1024;

/// A natural-height prose body switches to the fork's windowed block layout
/// past either threshold: source ≥ 64 KiB, or ≥ 300 blocks for sources the
/// byte gate would miss (many short paragraphs cost more to lay out than one
/// block of the same bytes). An expanded reasoning body whose source crosses
/// these thresholds skips natural height entirely and uses the clamped,
/// internally scrollable viewport, which bounds per-frame layout work by its
/// own viewport size.
pub(crate) const WINDOWED_SOURCE_BYTES: usize = 64 * 1024;

/// Block count at which a natural-height row body renders through the
/// windowed block layout regardless of byte size.
pub(crate) const WINDOWED_SOURCE_BLOCKS: usize = 300;

/// Whether a natural-height row body renders through the windowed block
/// layout. Source length and the observed parsed block count determine the
/// requested mode; the component separately reports actual layout completion.
pub(crate) fn windowed_body(source_len: usize, block_count: usize) -> bool {
    source_len >= WINDOWED_SOURCE_BYTES || block_count >= WINDOWED_SOURCE_BLOCKS
}

/// Heading scale for transcript prose: h1 1.5×, h2 1.3×, h3 1.15× the base,
/// deeper headings at the base size.
fn heading_scale() -> Arc<dyn Fn(u8, Pixels) -> Pixels + Send + Sync + 'static> {
    Arc::new(|level, base| match level {
        1 => base * 1.5,
        2 => base * 1.3,
        3 => base * 1.15,
        _ => base,
    })
}

/// Inline-code background: the theme's muted fill, raised to the nested
/// surface floor so the code text stays readable in every bundled theme.
fn inline_code(cx: &App) -> HighlightStyle {
    HighlightStyle {
        background_color: Some(contrast::pane_block(cx.theme().muted, cx)),
        ..Default::default()
    }
}

/// Preset for assistant/user message bodies: relaxed paragraph rhythm, the
/// 1.5/1.3/1.15 heading scale, and a muted inline-code fill.
pub(crate) fn prose(cx: &App) -> TextViewStyle {
    TextViewStyle {
        paragraph_gap: rems(1.),
        heading_base_font_size: px(14.),
        heading_font_size: Some(heading_scale()),
        inline_code: inline_code(cx),
        ..Default::default()
    }
}

/// Preset for reasoning bodies: same treatments as [`prose`], but a tighter
/// paragraph gap — reasoning arrives as many short paragraphs and the loose
/// default spends the height budget on whitespace.
pub(crate) fn reasoning(cx: &App) -> TextViewStyle {
    TextViewStyle {
        paragraph_gap: rems(0.5),
        heading_base_font_size: px(14.),
        heading_font_size: Some(heading_scale()),
        inline_code: inline_code(cx),
        ..Default::default()
    }
}
