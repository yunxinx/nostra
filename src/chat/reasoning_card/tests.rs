use super::*;

/// Both label states must resolve in every shipped locale — a missing key
/// renders as the raw dotted path in the UI.
#[test]
fn label_keys_resolve_in_every_locale() {
    for locale in ["en", "zh-CN"] {
        for key in [
            "chat.reasoning.streaming",
            "chat.reasoning.completed",
            "chat.reasoning.collapse",
            "chat.reasoning.expand",
            "chat.reasoning.copy",
        ] {
            let resolved = t!(key, locale = locale).to_string();
            assert!(!resolved.contains(key), "{key} unresolved for {locale}");
        }
        let finished = t!("chat.reasoning.finished", locale = locale, duration = "3.2").to_string();
        assert!(finished.contains("3.2"), "{finished} for {locale}");
    }
}

/// WCAG relative luminance, per the sRGB definition.
fn luminance(color: gpui::Hsla) -> f32 {
    let rgb = gpui::Rgba::from(color);
    let channel = |c: f32| {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
}

fn contrast_ratio(a: gpui::Hsla, b: gpui::Hsla) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    let (lighter, darker) = if a > b { (a, b) } else { (b, a) };
    (lighter + 0.05) / (darker + 0.05)
}

/// The expanded body is prose the user is expected to actually read, so its
/// colour pairing has to clear the WCAG AA floor for body text in every
/// bundled theme — not just the one that happened to be active while this was
/// being built.
///
/// This is a real regression test: pairing `muted_foreground` with the card
/// surface measured 2.8:1 under Nostra Dark, and the card is outlined rather
/// than filled precisely so the body sits on `background` where the contrast
/// budget is widest.
#[gpui::test]
fn reasoning_body_clears_wcag_aa_in_every_bundled_theme(cx: &mut gpui::TestAppContext) {
    use gpui_component::{Theme, ThemeMode};

    const AA_BODY_TEXT: f32 = 4.5;

    cx.update(|cx| {
        gpui_component::init(cx);
        crate::preferences::init_global(crate::preferences::Preferences::default(), cx);
        crate::appearance::theme::init(&crate::preferences::Preferences::default(), cx);

        for dark in [true, false] {
            // `Theme::change` rather than `theme::set_mode`, which would
            // persist to the user's real configuration directory.
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
                // Palette coverage must not pass through the production
                // preference writer: tests may run beside the real app or
                // in parallel with other theme tests.
                crate::appearance::theme::select_theme_for_test(name.as_ref(), cx);

                // What `render` actually pairs: the body's own colour against
                // the surface it sits on, which for an outline-only card is
                // the window background.
                let ratio = contrast_ratio(cx.theme().group_box_foreground, cx.theme().background);
                assert!(
                    ratio >= AA_BODY_TEXT,
                    "{name}: reasoning body contrast is {ratio:.2}:1, below the \
                     {AA_BODY_TEXT}:1 WCAG AA floor for body text"
                );
            }
        }
    });
}

#[gpui::test]
fn label_reports_thinking_while_streaming_then_the_banked_duration(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx.add_empty_window();
    let mut trace = window.update(|_, cx| ReasoningTrace::new(1, cx));

    assert_eq!(
        trace.label(false),
        t!("chat.reasoning.streaming").to_string()
    );

    trace.finish();
    let finished = trace.label(true);
    assert_ne!(finished, t!("chat.reasoning.streaming").to_string());
    assert!(
        finished.contains("0.1"),
        "a sub-100ms trace floors at 0.1 rather than reading as zero: {finished}"
    );
}

#[gpui::test]
fn finish_is_idempotent_and_preserves_user_disclosure(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx.add_empty_window();
    let mut trace = window.update(|_, cx| ReasoningTrace::new(1, cx));

    trace.finish();
    let elapsed = trace.elapsed;
    assert!(trace.started_at.is_none(), "the clock was stopped");

    // Closing an already-closed trace must not bank more time, and must not
    // re-collapse a card the user has since opened.
    trace.toggle();
    trace.finish();
    assert_eq!(trace.elapsed, elapsed);
    assert!(trace.is_expanded(), "user intent is preserved");
}

#[gpui::test]
fn terminal_only_block_uses_an_untimed_completion_label(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx.add_empty_window();
    let trace = window.update(|_, cx| ReasoningTrace::completed("complete block".into(), 1, cx));

    assert_eq!(
        trace.label(true),
        t!("chat.reasoning.completed").to_string()
    );
}

#[gpui::test]
fn empty_deltas_do_not_start_or_change_timing(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx.add_empty_window();
    let mut trace = window.update(|_, cx| ReasoningTrace::new(1, cx));
    trace.finish();
    let elapsed = trace.elapsed;

    window.update(|_, cx| trace.push("", cx));
    assert!(
        trace.started_at.is_none(),
        "an empty delta has no lifecycle meaning"
    );
    assert_eq!(trace.elapsed, elapsed);
}
