//! Contrast floors for colours a theme cannot be trusted to get right.
//!
//! A bundled theme only has to define its palette; it does not know which of
//! its colours this app puts next to each other. Where a layout relies on two
//! colours being *told apart* — adjacent surfaces with no divider, body text
//! on a panel — the pairing is a decision of this app, so the floor belongs
//! here rather than in each theme file. Every helper keeps the theme's own
//! colour when it already clears the floor and only nudges lightness when it
//! does not, so a palette's character survives.

use gpui::{App, Background, Hsla, Rgba};
use gpui_component::ActiveTheme as _;

/// Smallest WCAG ratio at which a block nested on a panel — a row tint, a
/// message bubble, a code block — still reads as its own surface.
pub(crate) const MIN_NESTED_SURFACE_CONTRAST: f32 = 1.2;
/// Floor for the seam between the sidebar and the conversation pane. Higher
/// than a nested block: those have their own shape and padding to help, while
/// two full-height panes give the eye nothing but the seam.
pub(crate) const MIN_PANE_SURFACE_CONTRAST: f32 = 1.35;
/// Floor for an outline that carries meaning (an error card's edge). A hairline
/// covers far fewer pixels than a fill, so it needs the most of the three.
pub(crate) const MIN_OUTLINE_CONTRAST: f32 = 1.5;
/// Floor for text against whatever it lands on: WCAG AAA for body text. AA
/// (4.5) is the legal minimum for prose nobody has to enjoy; this app's own
/// surfaces sit closer to their text than a bare window background does, so
/// the enhanced level is the one worth holding.
pub(crate) const MIN_BODY_TEXT_CONTRAST: f32 = 7.;

/// WCAG relative-luminance contrast ratio, from 1.0 (identical) upwards.
pub(crate) fn ratio(a: Hsla, b: Hsla) -> f32 {
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

/// Flatten a translucent theme colour onto what it is painted over, so its
/// contrast can be measured as the eye will see it.
pub(crate) fn opaque(base: Hsla, overlay: Hsla) -> Hsla {
    base.blend(overlay).alpha(1.)
}

/// A surface colour plus the background value to paint it with: the theme's
/// preferred `Background` survives untouched when it already clears the floor,
/// which keeps gradients and other non-solid tokens intact.
#[derive(Clone, Copy)]
pub(crate) struct Surface {
    pub(crate) background: Background,
    pub(crate) color: Hsla,
}

/// Keep `preferred` if it already stands apart from `reference`, otherwise walk
/// its lightness *away from `reference`* until it clears `floor`.
///
/// Away from the reference, not toward the palette's extreme: a tint derived
/// from another tint (a selection built on a hover) can sit on either side of
/// what it is measured against, and stepping the wrong way walks back through
/// it — which is how a selection ends up the same colour as the panel it is
/// supposed to stand out from. When the two are level, `dark` breaks the tie in
/// the direction the palette leaves room for.
pub(crate) fn distinct_surface(
    preferred: Background,
    mut preferred_color: Hsla,
    reference: Hsla,
    floor: f32,
    dark: bool,
) -> Surface {
    if ratio(preferred_color, reference) >= floor {
        return Surface {
            background: preferred,
            color: preferred_color,
        };
    }

    let lighter = if preferred_color.l == reference.l {
        dark
    } else {
        preferred_color.l > reference.l
    };
    let step = if lighter { 0.01 } else { -0.01 };
    while ratio(preferred_color, reference) < floor {
        let next_lightness = (preferred_color.l + step).clamp(0., 1.);
        if next_lightness == preferred_color.l {
            break;
        }
        preferred_color.l = next_lightness;
    }
    Surface {
        background: preferred_color.into(),
        color: preferred_color,
    }
}

/// Sidebar surface for a window whose panes meet without a divider.
///
/// The seam is the only thing separating the sidebar from the conversation
/// pane, so the two backgrounds have to differ on their own. Several bundled
/// palettes put `sidebar` within a hair of `background` (and a theme that
/// leaves it unset gets a fallback that is deliberately close), which erases
/// the seam.
pub(crate) fn sidebar_surface(cx: &App) -> Hsla {
    let pane = cx.theme().background;
    let preferred = opaque(pane, cx.theme().sidebar);
    distinct_surface(
        preferred.into(),
        preferred,
        pane,
        MIN_PANE_SURFACE_CONTRAST,
        cx.theme().is_dark(),
    )
    .color
}

/// Body-text colour for the sidebar.
///
/// A theme only has to define `foreground`; `sidebar.foreground` falls back to
/// it, so sidebar text ends up the same colour as transcript text — but it
/// lands on a surface that is closer to it than the main background is, and
/// [`sidebar_surface`] may have moved that surface closer still. Measuring the
/// floor against the surface the text actually lands on is what keeps the two
/// derivations from cancelling each other out.
pub(crate) fn sidebar_text(cx: &App) -> Hsla {
    text_on(cx.theme().sidebar_foreground, sidebar_surface(cx), cx)
}

/// Walk a text colour away from the surface it lands on until it clears the
/// body-text floor.
pub(crate) fn text_on(base: Hsla, surface: Hsla, cx: &App) -> Hsla {
    distinct_surface(
        base.into(),
        base,
        surface,
        MIN_BODY_TEXT_CONTRAST,
        cx.theme().is_dark(),
    )
    .color
}

/// A block that sits on the conversation pane with no border of its own — a
/// user turn's bubble, a code block. Its own shape helps, so it needs less
/// separation than the sidebar seam, but several palettes put these fills
/// within 1.07 of the pane, which leaves the block invisible.
pub(crate) fn pane_block(fill: Hsla, cx: &App) -> Hsla {
    let pane = cx.theme().background;
    let preferred = opaque(pane, fill);
    distinct_surface(
        preferred.into(),
        preferred,
        pane,
        MIN_NESTED_SURFACE_CONTRAST,
        cx.theme().is_dark(),
    )
    .color
}

/// An outline on the conversation pane that has to stay findable — the border
/// is what delimits a card whose fill is deliberately a faint wash.
pub(crate) fn pane_outline(edge: Hsla, cx: &App) -> Hsla {
    let pane = cx.theme().background;
    let preferred = opaque(pane, edge);
    distinct_surface(
        preferred.into(),
        preferred,
        pane,
        MIN_OUTLINE_CONTRAST,
        cx.theme().is_dark(),
    )
    .color
}

/// Push `tint` further from `surface` — the direction `tint` already sits in —
/// until the two tints are themselves `floor` apart.
///
/// This is not [`distinct_surface`] with `tint` as its own reference: there the
/// direction comes from the pair being measured, and a second tint has to keep
/// moving *outward* from the panel instead, or it walks back across it.
fn deepen(tint: Hsla, surface: Hsla, floor: f32) -> Hsla {
    let step = if tint.l >= surface.l { 0.01 } else { -0.01 };
    let mut deeper = tint;
    while ratio(deeper, tint) < floor {
        let next = (deeper.l + step).clamp(0., 1.);
        if next == deeper.l {
            break;
        }
        deeper.l = next;
    }
    deeper
}

/// The two tints a sidebar row can wear, with the text that reads on each.
#[derive(Clone, Copy)]
pub(crate) struct RowTints {
    pub(crate) hover: Hsla,
    pub(crate) hover_text: Hsla,
    pub(crate) selected: Hsla,
    pub(crate) selected_text: Hsla,
}

/// Hover and selection tints for a list row, derived so that three things stay
/// apart: each tint from the panel it sits on, and selection from hover —
/// pointing at a row must not look the same as having it open.
pub(crate) fn row_tints(
    surface: Hsla,
    accent: Hsla,
    text: Hsla,
    accent_text: Hsla,
    cx: &App,
) -> RowTints {
    let dark = cx.theme().is_dark();
    let accent = opaque(surface, accent);
    let hover = distinct_surface(
        accent.into(),
        accent,
        surface,
        MIN_NESTED_SURFACE_CONTRAST,
        dark,
    )
    .color;
    let selected = deepen(hover, surface, MIN_NESTED_SURFACE_CONTRAST);
    RowTints {
        hover,
        hover_text: text_on(text, hover, cx),
        selected,
        selected_text: text_on(accent_text, selected, cx),
    }
}

/// Row tints for the sidebar's own palette slots.
pub(crate) fn sidebar_row_tints(cx: &App) -> RowTints {
    row_tints(
        sidebar_surface(cx),
        cx.theme().sidebar_accent,
        cx.theme().sidebar_foreground,
        cx.theme().sidebar_accent_foreground,
        cx,
    )
}

/// Row tints for lists inside a popover, whose panel is `popover` rather than
/// the window background — measuring against the wrong surface would enforce
/// the floor against a colour the row never touches.
pub(crate) fn popover_row_tints(cx: &App) -> RowTints {
    row_tints(
        cx.theme().popover,
        cx.theme().accent,
        cx.theme().popover_foreground,
        cx.theme().accent_foreground,
        cx,
    )
}

/// Secondary sidebar text — section labels, hints, timestamps, spinners.
///
/// Derived from [`sidebar_text`] rather than from the raw theme colour so the
/// whole sidebar keeps one hierarchy: when the floors move the body text, the
/// quiet tiers move with it instead of drifting into the surface.
pub(crate) fn sidebar_muted_text(cx: &App, strength: f32) -> Hsla {
    sidebar_text(cx).opacity(strength)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::theme;
    use crate::preferences::{self, Preferences};
    use gpui::{TestAppContext, transparent_white};
    use gpui_component::{Colorize as _, Theme, ThemeMode};

    /// A floor is only reachable while there is lightness left to spend: a
    /// colour already at black or white cannot take the last step, and the
    /// derivation returns the best it can rather than looping forever.
    fn clears(reads: f32, derived: Hsla, floor: f32) -> bool {
        reads >= floor || derived.l == 0. || derived.l == 1.
    }

    /// A theme only occupies the slot matching its own appearance, so the mode
    /// has to move with the loop — otherwise every iteration measures whichever
    /// palette happens to be active and the check is vacuous.
    fn with_every_bundled_theme(cx: &mut TestAppContext, mut check: impl FnMut(&str, &App)) {
        cx.update(|cx| {
            gpui_component::init(cx);
            preferences::init_global(Preferences::default(), cx);
            theme::init(&Preferences::default(), cx);

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
                for name in theme::theme_names(dark, cx) {
                    theme::select_theme_for_test(name.as_ref(), cx);
                    check(name.as_ref(), cx);
                }
            }
        });
    }

    #[gpui::test]
    fn the_sidebar_seam_is_visible_in_every_bundled_theme(cx: &mut TestAppContext) {
        with_every_bundled_theme(cx, |name, cx| {
            let seam = ratio(sidebar_surface(cx), cx.theme().background);
            assert!(
                seam >= MIN_PANE_SURFACE_CONTRAST,
                "{name}: sidebar and conversation pane meet at {seam:.3}, below \
                 the {MIN_PANE_SURFACE_CONTRAST} floor, so the seam disappears"
            );
        });
    }

    /// The seam floor lightens the sidebar in dark mode, which eats into text
    /// contrast — so the text floor has to be measured against the surface the
    /// text really lands on, or the two derivations cancel out.
    #[gpui::test]
    fn sidebar_text_clears_its_floor_on_the_derived_surface_in_every_theme(
        cx: &mut TestAppContext,
    ) {
        with_every_bundled_theme(cx, |name, cx| {
            let surface = sidebar_surface(cx);
            let text = ratio(sidebar_text(cx), surface);
            assert!(
                clears(text, sidebar_text(cx), MIN_BODY_TEXT_CONTRAST),
                "{name}: sidebar text reads at {text:.2} on its own surface, \
                 below the {MIN_BODY_TEXT_CONTRAST} floor"
            );
            assert!(
                (0. ..=1.).contains(&sidebar_text(cx).l),
                "{name}: lightness stays in range"
            );
        });
    }

    /// Every tint a row can wear has to be tellable from the panel *and* from
    /// the other tint: hover looking like selection is as bad as neither
    /// showing at all.
    #[gpui::test]
    fn row_tints_separate_from_the_panel_and_from_each_other(cx: &mut TestAppContext) {
        with_every_bundled_theme(cx, |name, cx| {
            for (label, surface, tints) in [
                ("sidebar", sidebar_surface(cx), sidebar_row_tints(cx)),
                ("popover", cx.theme().popover, popover_row_tints(cx)),
            ] {
                let hover = ratio(tints.hover, surface);
                let selected = ratio(tints.selected, surface);
                assert!(
                    hover >= MIN_NESTED_SURFACE_CONTRAST,
                    "{name}/{label}: hover tint reads at {hover:.3} on its panel"
                );
                assert!(
                    selected > hover,
                    "{name}/{label}: selection ({selected:.3}) must sit further \
                     from the panel than hover ({hover:.3})"
                );
                let apart = ratio(tints.selected, tints.hover);
                assert!(
                    clears(apart, tints.selected, MIN_NESTED_SURFACE_CONTRAST),
                    "{name}/{label}: selection and hover are only {apart:.3} apart"
                );
                for (tier, tint, text) in [
                    ("hover", tints.hover, tints.hover_text),
                    ("selected", tints.selected, tints.selected_text),
                ] {
                    let reads = ratio(text, tint);
                    assert!(
                        clears(reads, text, MIN_BODY_TEXT_CONTRAST),
                        "{name}/{label}: text on the {tier} tint reads at {reads:.2}"
                    );
                }
            }
        });
    }

    /// Borderless blocks on the transcript (a user turn, a code block) and the
    /// outlines that delimit washed-out cards.
    #[gpui::test]
    fn transcript_blocks_and_outlines_hold_the_pane_in_every_theme(cx: &mut TestAppContext) {
        with_every_bundled_theme(cx, |name, cx| {
            let pane = cx.theme().background;
            for (label, fill, text_base) in [
                (
                    "user bubble",
                    cx.theme().secondary,
                    cx.theme().secondary_foreground,
                ),
                ("code block", cx.theme().muted, cx.theme().foreground),
            ] {
                let block = pane_block(fill, cx);
                let reads = ratio(block, pane);
                assert!(
                    reads >= MIN_NESTED_SURFACE_CONTRAST,
                    "{name}: the {label} reads at {reads:.3} on the pane"
                );
                let block_text = text_on(text_base, block, cx);
                let text = ratio(block_text, block);
                assert!(
                    clears(text, block_text, MIN_BODY_TEXT_CONTRAST),
                    "{name}: text on the {label} reads at {text:.2}"
                );
            }
            let outline = ratio(pane_outline(cx.theme().danger.opacity(0.3), cx), pane);
            assert!(
                outline >= MIN_OUTLINE_CONTRAST,
                "{name}: a card outline reads at {outline:.3} on the pane"
            );
        });
    }

    /// The transcript row pairings with no divider between them: the
    /// reasoning rail, the tool-activity header against its body, the step
    /// stack header, and the hover-revealed turn-actions bar. Every fill and
    /// outline goes through the same derivation the renderers call, so the
    /// assertion tracks the views instead of restating them.
    #[gpui::test]
    fn transcript_row_surfaces_hold_their_pairings_in_every_theme(cx: &mut TestAppContext) {
        with_every_bundled_theme(cx, |name, cx| {
            let theme = cx.theme();
            let pane = theme.background;

            // The reasoning rail is a bare 2px line against the pane.
            let rail = pane_outline(theme.border, cx);
            let rail_reads = ratio(rail, pane);
            assert!(
                rail_reads >= MIN_OUTLINE_CONTRAST,
                "{name}: the reasoning rail reads at {rail_reads:.3} on the pane"
            );

            // Tool-activity and step-stack headers, and the actions bar,
            // share one recipe: a muted block on the pane, body text on it.
            for (label, text_base) in [
                ("activity header", theme.foreground),
                ("group header", theme.foreground),
                ("actions bar", theme.muted_foreground),
            ] {
                let header = pane_block(theme.muted, cx);
                let header_reads = ratio(header, pane);
                assert!(
                    header_reads >= MIN_NESTED_SURFACE_CONTRAST,
                    "{name}: the {label} reads at {header_reads:.3} on the pane"
                );
                let header_text = text_on(text_base, header, cx);
                let text_reads = ratio(header_text, header);
                assert!(
                    clears(text_reads, header_text, MIN_BODY_TEXT_CONTRAST),
                    "{name}: text on the {label} reads at {text_reads:.2}"
                );

                // The body sits directly against the header with no divider.
                let body = distinct_surface(
                    theme.background.into(),
                    theme.background,
                    header,
                    MIN_NESTED_SURFACE_CONTRAST,
                    theme.is_dark(),
                );
                let body_reads = ratio(body.color, header);
                assert!(
                    body_reads >= MIN_NESTED_SURFACE_CONTRAST,
                    "{name}: the body against the {label} reads at {body_reads:.3}"
                );
                let body_text = text_on(theme.group_box_foreground, body.color, cx);
                let body_text_reads = ratio(body_text, body.color);
                assert!(
                    clears(body_text_reads, body_text, MIN_BODY_TEXT_CONTRAST),
                    "{name}: body text against the {label} reads at {body_text_reads:.2}"
                );
            }

            // The error row's outline uses the same tinting recipe as the
            // component library's error Alert.
            let error_edge = pane_outline(theme.danger.mix_oklab(transparent_white(), 0.3), cx);
            let error_reads = ratio(error_edge, pane);
            assert!(
                error_reads >= MIN_OUTLINE_CONTRAST,
                "{name}: the error row outline reads at {error_reads:.3} on the pane"
            );

            // The error headline sits on the washed card fill, derived the
            // way `TurnErrorRenderer` derives it.
            let error_surface = theme.danger.mix_oklab(transparent_white(), 0.04);
            let error_headline = text_on(theme.danger, error_surface, cx);
            let headline_reads = ratio(error_headline, error_surface);
            assert!(
                clears(headline_reads, error_headline, MIN_BODY_TEXT_CONTRAST),
                "{name}: the error headline reads at {headline_reads:.2} on the card fill"
            );
        });
    }

    /// Secondary tiers are relative to the body text, so they cannot end up
    /// closer to the surface than the body text is.
    #[gpui::test]
    fn muted_sidebar_text_stays_quieter_than_the_body_text(cx: &mut TestAppContext) {
        with_every_bundled_theme(cx, |name, cx| {
            let surface = sidebar_surface(cx);
            let body = ratio(sidebar_text(cx), surface);
            let muted = ratio(opaque(surface, sidebar_muted_text(cx, 0.6)), surface);
            assert!(
                muted < body,
                "{name}: muted sidebar text ({muted:.2}) must stay quieter \
                 than body text ({body:.2})"
            );
            assert!(muted > 1., "{name}: muted text must still be visible");
        });
    }

    /// A palette that already separates its panes must come through untouched:
    /// the floor is a floor, not a restyling.
    #[gpui::test]
    fn a_palette_that_already_separates_its_panes_is_left_alone(cx: &mut TestAppContext) {
        with_every_bundled_theme(cx, |name, cx| {
            let pane = cx.theme().background;
            let preferred = opaque(pane, cx.theme().sidebar);
            if ratio(preferred, pane) >= MIN_PANE_SURFACE_CONTRAST {
                assert_eq!(
                    sidebar_surface(cx),
                    preferred,
                    "{name}: an already-distinct sidebar colour must not be nudged"
                );
            }
        });
    }
}
