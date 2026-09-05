//! Typography presets for transcript row content (PRD R7).
//!
//! Every `TextViewStyle` a row renderer applies is built here so heading
//! scales, paragraph gaps, and table/inline-code treatment live in one place
//! (AC7: no style constants in the renderer files). Colors come from the
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
/// stream, which is what keeps the prose below it from moving (AC1).
pub(crate) const PREVIEW_LINES: f32 = 6.;

/// Minimum height of a budgeted reasoning viewport, in lines of body text.
pub(crate) const BUDGET_MIN_LINES: f32 = 12.;

/// Fraction of the conversation viewport a budgeted reasoning viewport may
/// occupy when that is taller than [`BUDGET_MIN_LINES`].
pub(crate) const BUDGET_VIEWPORT_RATIO: f32 = 0.45;

/// Tool results above this many bytes use the budgeted, internally
/// scrollable viewport instead of natural height.
pub(crate) const RESULT_BUDGET_BYTES: usize = 8 * 1024;

/// A natural-height prose or reasoning-full body switches to the fork's
/// windowed block layout past either threshold (P4 PRD R5): source ≥ 64 KiB,
/// or ≥ 300 blocks for sources the byte gate would miss (many short
/// paragraphs cost more to lay out than one block of the same bytes).
pub(crate) const WINDOWED_SOURCE_BYTES: usize = 64 * 1024;

/// Block count at which a natural-height row body renders through the
/// windowed block layout regardless of byte size.
pub(crate) const WINDOWED_SOURCE_BLOCKS: usize = 300;

/// Whether a natural-height row body renders through the windowed block
/// layout. Evaluated per frame from the renderer's authoritative source, so a
/// stream crossing the threshold flips to windowed on the next paint and the
/// fork's late-enable alignment picks it up without a reset.
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
