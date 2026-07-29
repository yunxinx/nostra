//! Upstream failure card for a chat turn.
//!
//! A failed turn used to render as a markdown blockquote holding a localized
//! sentence and nostra's own request id — which told the user nothing the
//! status bar didn't already say. This module shows what the provider actually
//! returned: the status line it can be identified by, and the captured response
//! body as a syntax-highlighted, copyable code block.
//!
//! The body arrives as [`GatewayError::upstream_body`] and is **not** redacted
//! (see `llm::error`), so it belongs here in the view and nowhere else.

// Imported by name rather than glob: `gpui::*` exports a `test` macro that
// shadows the built-in `#[test]` attribute in this module's test submodule.
use gpui::{
    AnyElement, App, AppContext as _, ElementId, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, transparent_white,
};
use gpui_component::{
    ActiveTheme, Colorize as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    clipboard::Clipboard,
    h_flex,
    text::{TextView, TextViewState},
    v_flex,
};
use rust_i18n::t;

use crate::llm::{ErrorKind, GatewayError};

/// Bodies longer than this collapse by default. Sized so a typical provider
/// error object (`{"error": {message, type, param, code}}`) stays fully visible
/// while an HTML proxy page or a validation dump does not push the composer off
/// screen.
const COLLAPSE_LINE_THRESHOLD: usize = 16;

/// Single-line proxy pages and minified payloads still wrap into many visual
/// lines. Collapse them by display size as well as logical line count so a
/// provider-controlled response cannot dominate the transcript on first paint.
const COLLAPSE_BYTE_THRESHOLD: usize = 4 * 1024;

/// Rendering has a smaller budget than capture. HTTP diagnostics are already
/// bounded to 64 KiB, while one SSE event may be as large as 1 MiB. Keeping the
/// complete captured text in `raw_body` preserves exact copy behavior; limiting
/// only the syntax-highlighted preview prevents a provider-controlled frame
/// from synchronously expanding into a much larger indented Markdown document
/// on the UI thread.
const MAX_DISPLAY_SOURCE_BYTES: usize = 32 * 1024;
const MAX_FORMATTED_BODY_BYTES: usize = 128 * 1024;

/// A turn's failure, prepared for rendering.
///
/// Built once when the generation fails — `body` is a `TextViewState` entity, so
/// it must not be constructed during a render pass.
pub struct TurnError {
    /// Non-localized headline inputs. The actual text is resolved in `render`
    /// so existing cards follow live locale changes.
    kind: ErrorKind,
    status: Option<u16>,
    /// Allowlisted provider code, shown under the headline when present.
    code: Option<SharedString>,
    /// Nostra request identifier used to correlate this visible failure with
    /// diagnostics and the bounded metrics record.
    request_id: Option<SharedString>,
    /// Verbatim upstream body, retained for the clipboard.
    raw_body: Option<SharedString>,
    /// The fenced-code-block markdown handed to `body`. Kept because a code
    /// block's syntax colors are baked in at parse time (see
    /// [`TurnError::refresh_highlight`]) and re-parsing needs the source again —
    /// `TextViewState` does not expose it.
    markdown: Option<SharedString>,
    /// Markdown-wrapped body rendered as a fenced code block, or `None` when the
    /// failure carried no upstream text (a local config or connect failure).
    body: Option<Entity<TextViewState>>,
    /// Whether the body is long enough to warrant a collapse toggle.
    collapsible: bool,
    /// Whether the rendered preview is shorter than the copyable raw response.
    preview_truncated: bool,
}

impl TurnError {
    pub fn new(mut error: GatewayError, cx: &mut App) -> Self {
        let code = error.provider_code.clone().map(SharedString::from);
        let request_id = error.request_id.clone().map(SharedString::from);
        let Some(raw) = error.take_upstream_body() else {
            return Self {
                kind: error.kind,
                status: error.status,
                code,
                request_id,
                raw_body: None,
                markdown: None,
                body: None,
                collapsible: false,
                preview_truncated: false,
            };
        };

        let (source, source_truncated) = truncate_utf8(&raw, MAX_DISPLAY_SOURCE_BYTES);
        let (display, format_truncated) = pretty_json(source, MAX_FORMATTED_BODY_BYTES);
        let collapsible = display.lines().count() > COLLAPSE_LINE_THRESHOLD
            || display.len() > COLLAPSE_BYTE_THRESHOLD;
        let markdown: SharedString = fenced_block(&display, language_tag(source)).into();
        let body = cx.new(|cx| TextViewState::markdown(&markdown, cx));
        Self {
            kind: error.kind,
            status: error.status,
            code,
            // The clipboard carries the captured response text, not the
            // re-indented display form.
            request_id,
            // Move the complete bounded capture into the clipboard state. It is
            // deliberately not redacted: this is the provider response the user
            // asked to inspect, kept out of Debug, metrics, and canonical replay.
            raw_body: Some(raw.into()),
            markdown: Some(markdown),
            body: Some(body),
            collapsible,
            preview_truncated: source_truncated || format_truncated,
        }
    }

    /// Re-parse the body so its code block picks up the active theme's syntax
    /// colors.
    ///
    /// Markdown code blocks capture an `Arc<HighlightTheme>` when they are parsed
    /// (`text::node::CodeBlock::new`) and memoize their styles from it, so a
    /// theme switch alone leaves them painted in the old palette — unlike the
    /// `Input` code editor, which resolves `cx.theme().highlight_theme` at paint
    /// time. `TextViewState::set_text` ignores an unchanged value, so refreshing
    /// must replace the state entity. This also drops parse tasks tied to the old
    /// theme while keeping entity creation outside render.
    pub fn refresh_highlight(&mut self, cx: &mut App) -> bool {
        let Some(markdown) = self.markdown.clone() else {
            return false;
        };
        self.body = Some(cx.new(|cx| TextViewState::markdown(&markdown, cx)));
        true
    }

    #[cfg(test)]
    pub(crate) fn body_entity_id(&self) -> Option<gpui::EntityId> {
        self.body.as_ref().map(Entity::entity_id)
    }

    #[cfg(test)]
    pub(crate) fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }
}

/// Render a turn's failure card. `index` disambiguates element ids across the
/// transcript so collapse state and the copy button stay per-message.
pub fn render(error: &TurnError, index: usize, window: &mut Window, cx: &mut App) -> AnyElement {
    // Theme values are copied out before `cx` is borrowed mutably for the
    // collapse state below.
    let (danger, muted_foreground, radius_lg, mono_font_family) = {
        let theme = cx.theme();
        (
            theme.danger,
            theme.muted_foreground,
            theme.radius_lg,
            theme.mono_font_family.clone(),
        )
    };
    // Same tinting recipe as the component library's error Alert, so the card
    // reads as part of the same visual system.
    let surface = danger.mix_oklab(transparent_white(), 0.04);
    let edge = danger.mix_oklab(transparent_white(), 0.3);

    // Collapse state lives in window-keyed state so it survives re-renders
    // without the transcript owning a flag per message.
    let expanded_state = if error.collapsible {
        Some(window.use_keyed_state(
            ElementId::NamedInteger("turn-error".into(), index as u64),
            cx,
            |_, _| false,
        ))
    } else {
        None
    };
    let expanded = expanded_state.as_ref().is_none_or(|state| *state.read(cx));
    let locale = rust_i18n::locale();

    v_flex()
        .id(ElementId::NamedInteger(
            "turn-error-card".into(),
            index as u64,
        ))
        .role(Role::Alert)
        .w_full()
        .rounded(radius_lg)
        .border_1()
        .border_color(edge)
        .bg(surface)
        .overflow_hidden()
        .child(
            h_flex()
                .w_full()
                .px_3()
                .py_2()
                .gap_2()
                .items_start()
                .child(
                    v_flex()
                        .flex_1()
                        .gap_0p5()
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .text_color(danger)
                                .child(headline(error.kind, error.status, &locale)),
                        )
                        .when_some(error.code.clone(), |this, code| {
                            this.child(
                                div()
                                    .text_xs()
                                    .font_family(mono_font_family.clone())
                                    .text_color(muted_foreground)
                                    .child(code),
                            )
                        })
                        .when_some(error.request_id.clone(), |this, request_id| {
                            this.child(
                                div()
                                    .text_xs()
                                    .font_family(mono_font_family.clone())
                                    .text_color(muted_foreground)
                                    .child(format!(
                                        "{}: {request_id}",
                                        t!("chat.error.request_id")
                                    )),
                            )
                        }),
                )
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .gap_1()
                        .when_some(expanded_state.clone(), |this, state| {
                            this.child(
                                Button::new(("turn-error-toggle", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(if expanded {
                                        IconName::ChevronUp
                                    } else {
                                        IconName::ChevronDown
                                    })
                                    .tooltip(if expanded {
                                        t!("chat.error.collapse").to_string()
                                    } else {
                                        t!("chat.error.expand").to_string()
                                    })
                                    .on_click(move |_, _, cx| {
                                        state.update(cx, |expanded, cx| {
                                            *expanded = !*expanded;
                                            cx.notify();
                                        });
                                    }),
                            )
                        })
                        .when_some(error.raw_body.clone(), |this, raw| {
                            this.child(
                                Clipboard::new(("turn-error-copy", index))
                                    .value(raw)
                                    .tooltip(t!("chat.error.copy").to_string()),
                            )
                        }),
                ),
        )
        .when_some(error.body.clone().filter(|_| expanded), |this, body| {
            this.child(
                v_flex()
                    .w_full()
                    .px_3()
                    .pb_3()
                    .gap_1()
                    // The fenced block brings its own muted surface and
                    // padding, so the card only supplies the outer inset.
                    .child(TextView::new(&body).selectable(true))
                    .when(error.preview_truncated, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(muted_foreground)
                                .child(t!("chat.error.preview_truncated").to_string()),
                        )
                    }),
            )
        })
        .into_any_element()
}

/// Localized one-liner for the failure. HTTP failures name their status because
/// that is the number a user matches against provider docs; the other kinds have
/// no upstream number to quote.
fn headline(kind: ErrorKind, status: Option<u16>, locale: &str) -> String {
    match kind {
        ErrorKind::Configuration => t!("chat.error.configuration", locale = locale).to_string(),
        ErrorKind::Transport => t!("chat.error.connection", locale = locale).to_string(),
        ErrorKind::Http => status.map_or_else(
            || t!("chat.error.provider", locale = locale).to_string(),
            |status| t!("chat.error.http", locale = locale, status = status).to_string(),
        ),
        ErrorKind::Protocol => t!("chat.error.interrupted", locale = locale).to_string(),
        ErrorKind::Provider => t!("chat.error.provider", locale = locale).to_string(),
    }
}

/// `json` when the body parses as JSON, so the highlighter engages; no tag
/// otherwise (an HTML error page or a plain-text proxy message).
fn language_tag(body: &str) -> Option<&'static str> {
    serde_json::from_str::<serde_json::Value>(body)
        .is_ok()
        .then_some("json")
}

/// Wrap `body` in a fenced code block whose fence is longer than any backtick
/// run inside it, so a body containing ``` cannot terminate its own block.
fn fenced_block(body: &str, lang: Option<&str>) -> String {
    let longest_run = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest_run.max(2) + 1);
    format!("{fence}{}\n{body}\n{fence}", lang.unwrap_or(""))
}

/// Re-indent JSON for reading, preserving key order and every scalar exactly as
/// received.
///
/// Deliberately a text-level pass rather than `serde_json::to_string_pretty` on
/// a parsed `Value`: the default `Value` map reorders keys alphabetically, which
/// would misrepresent the response. Non-JSON input is returned untouched.
fn pretty_json(body: &str, max_output_bytes: usize) -> (String, bool) {
    if serde_json::from_str::<serde_json::Value>(body).is_err() {
        return (body.to_string(), false);
    }
    // Already multi-line: the provider (or a proxy) formatted it, so respect that.
    if body.contains('\n') {
        return (body.to_string(), false);
    }

    let mut out = String::with_capacity((body.len() * 2).min(max_output_bytes));
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = body.chars().peekable();

    while let Some(ch) = chars.next() {
        if out.len() + ch.len_utf8() > max_output_bytes {
            return (out, true);
        }
        if in_string {
            out.push(ch);
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '{' | '[' => {
                out.push(ch);
                // Keep `{}` and `[]` on one line instead of splitting them open.
                let closer = if ch == '{' { '}' } else { ']' };
                if chars.peek() == Some(&closer) {
                    chars.next();
                    if !push_char_within_budget(&mut out, closer, max_output_bytes) {
                        return (out, true);
                    }
                } else {
                    depth += 1;
                    if !newline_indent(&mut out, depth, None, max_output_bytes) {
                        return (out, true);
                    }
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                if !newline_indent(&mut out, depth, Some(ch), max_output_bytes) {
                    return (out, true);
                }
            }
            ',' => {
                out.push(ch);
                if !newline_indent(&mut out, depth, None, max_output_bytes) {
                    return (out, true);
                }
            }
            ':' if out.len() + 2 <= max_output_bytes => out.push_str(": "),
            ':' => return (out, true),
            c if c.is_ascii_whitespace() => {}
            c => out.push(c),
        }
    }
    (out, false)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (&str, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    (&value[..floor_char_boundary(value, max_bytes)], true)
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn push_char_within_budget(out: &mut String, ch: char, max_output_bytes: usize) -> bool {
    if out.len().saturating_add(ch.len_utf8()) > max_output_bytes {
        return false;
    }
    out.push(ch);
    true
}

fn newline_indent(
    out: &mut String,
    depth: usize,
    suffix: Option<char>,
    max_output_bytes: usize,
) -> bool {
    let Some(required) = depth
        .checked_mul(2)
        .and_then(|indent| indent.checked_add(1))
        .and_then(|required| required.checked_add(suffix.map_or(0, |suffix| suffix.len_utf8())))
    else {
        return false;
    };
    if out.len().saturating_add(required) > max_output_bytes {
        return false;
    }
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
    if let Some(suffix) = suffix {
        out.push(suffix);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_json_preserves_key_order_and_scalars() {
        let (pretty, truncated) = pretty_json(
            r#"{"zebra":1,"alpha":{"nested":"v"},"code":"rate_limit"}"#,
            MAX_FORMATTED_BODY_BYTES,
        );
        assert!(!truncated);
        let zebra = pretty.find("zebra").expect("zebra key");
        let alpha = pretty.find("alpha").expect("alpha key");
        assert!(
            zebra < alpha,
            "serde's Value would sort these; upstream order must survive:\n{pretty}"
        );
        assert!(pretty.contains("\"code\": \"rate_limit\""));
    }

    #[test]
    fn pretty_json_leaves_string_contents_untouched() {
        // Braces, colons, and commas inside a string must not trigger indenting.
        let (pretty, truncated) = pretty_json(
            r#"{"message":"limit {a}, use x: y","q":1}"#,
            MAX_FORMATTED_BODY_BYTES,
        );
        assert!(!truncated);
        assert!(
            pretty.contains(r#""message": "limit {a}, use x: y""#),
            "{pretty}"
        );
    }

    #[test]
    fn pretty_json_handles_escaped_quotes_and_empty_containers() {
        let (pretty, truncated) = pretty_json(
            r#"{"a":"say \"hi\"","b":{},"c":[]}"#,
            MAX_FORMATTED_BODY_BYTES,
        );
        assert!(!truncated);
        assert!(pretty.contains(r#""say \"hi\"""#), "{pretty}");
        assert!(pretty.contains("\"b\": {}"), "{pretty}");
        assert!(pretty.contains("\"c\": []"), "{pretty}");
    }

    #[test]
    fn pretty_json_never_writes_a_closer_past_the_output_budget() {
        for (body, budget) in [("{}", 1), ("[0]", 6)] {
            let (pretty, truncated) = pretty_json(body, budget);
            assert!(truncated, "{body} must be truncated at {budget} bytes");
            assert!(
                pretty.len() <= budget,
                "{body} produced {} bytes with a {budget}-byte budget: {pretty:?}",
                pretty.len()
            );
        }
    }

    #[test]
    fn non_json_bodies_pass_through_untagged() {
        let html = "<html><body>502 Bad Gateway</body></html>";
        assert_eq!(
            pretty_json(html, MAX_FORMATTED_BODY_BYTES),
            (html.into(), false)
        );
        assert_eq!(language_tag(html), None);
        assert_eq!(language_tag(r#"{"ok":true}"#), Some("json"));
    }

    #[test]
    fn already_formatted_json_is_left_alone() {
        let formatted = "{\n  \"error\": true\n}";
        assert_eq!(
            pretty_json(formatted, MAX_FORMATTED_BODY_BYTES),
            (formatted.into(), false)
        );
    }

    #[gpui::test]
    fn long_single_line_response_collapses_without_changing_the_raw_copy(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let window = cx.add_empty_window();
        let raw = format!("<html><body>{}</body></html>", "x".repeat(8 * 1024));
        let mut error = crate::llm::GatewayError::http(502, None).with_upstream_body(raw.clone());
        error.request_id = Some("nostra-1".into());

        let turn_error = window.update(|_, cx| TurnError::new(error, cx));

        assert!(
            turn_error.collapsible,
            "long wrapped content starts collapsed"
        );
        assert_eq!(
            turn_error.raw_body.as_deref(),
            Some(raw.as_str()),
            "collapse and preview limits must not rewrite the copyable response"
        );
    }

    #[test]
    fn fence_outgrows_backtick_runs_in_the_body() {
        let body = "text with ``` inside";
        let block = fenced_block(body, Some("json"));
        assert!(block.starts_with("````json\n"), "{block}");
        assert!(block.ends_with("\n````"), "{block}");
        // A plain body gets no language tag, but still a valid fence.
        assert!(fenced_block("plain", None).starts_with("```\n"));
    }

    /// The card is only useful if the highlighter is compiled in. Without the
    /// gpui-component `tree-sitter` feature the whole `highlighter::Language`
    /// enum is `#[cfg]`-ed out and replaced by a stub, so naming `Language::Json`
    /// fails the build — which is the point. Easy to lose when bumping the
    /// dependency, and the symptom (unstyled monospace) is quiet.
    #[test]
    fn json_highlighting_is_compiled_in() {
        assert_eq!(gpui_component::highlighter::Language::Json.name(), "json");
    }

    #[gpui::test]
    fn http_failure_builds_a_collapsible_card_with_a_copyable_raw_body(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let window = cx.add_empty_window();

        // A body long enough to cross the collapse threshold once re-indented.
        let raw = r#"{"error":{"message":"Rate limit reached for gpt-4o","type":"requests","param":null,"code":"rate_limit_exceeded","details":{"limit":10000,"used":10000,"reset":"60s","scope":"organization","plan":"tier-1","window":"1m","retry_after":60,"bucket":"rpm"}}}"#;
        let mut error = crate::llm::GatewayError::http(429, Some("rate_limit_exceeded".into()))
            .with_upstream_body(raw);
        error.request_id = Some("nostra-1".into());

        let turn_error = window.update(|_, cx| TurnError::new(error, cx));

        assert!(turn_error.body.is_some(), "body entity was built");
        assert!(
            turn_error.collapsible,
            "a body past the line threshold offers a collapse toggle"
        );
        // The clipboard gets the untouched captured text, not the display form.
        assert_eq!(turn_error.raw_body.as_deref(), Some(raw));
        assert_eq!(turn_error.request_id(), Some("nostra-1"));
        assert!(headline(turn_error.kind, turn_error.status, "en").contains("429"));

        // The markdown handed to the TextView is a json-tagged fence, so the
        // highlighter engages. (`TextViewState` does not expose its source text,
        // so this asserts on the same composition the constructor performs.)
        let (display, truncated) = pretty_json(raw, MAX_FORMATTED_BODY_BYTES);
        assert!(!truncated);
        let markdown = fenced_block(&display, language_tag(raw));
        assert!(markdown.starts_with("```json\n"), "{markdown}");
        assert!(
            markdown.contains("\"code\": \"rate_limit_exceeded\""),
            "{markdown}"
        );

        // `render` itself is exercised through a real `ChatView` pass — see
        // `chat::tests::failed_turn_renders_the_upstream_error_card`. It reads
        // window-keyed collapse state, which requires a rendering view on the
        // stack and so cannot be driven from a standalone element draw.
    }

    #[gpui::test]
    fn local_failure_renders_headline_only(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_empty_window();

        // A configuration failure never reached the provider, so there is no body.
        let error = crate::llm::GatewayError::configuration("model selection is unavailable");
        let turn_error = window.update(|_, cx| TurnError::new(error, cx));

        assert!(turn_error.body.is_none());
        assert!(turn_error.raw_body.is_none());
        assert!(!turn_error.collapsible, "nothing to collapse");
        assert!(!headline(turn_error.kind, turn_error.status, "en").is_empty());
    }

    #[test]
    fn preview_limits_preserve_utf8_and_bound_formatted_output() {
        let raw = format!(
            r#"{{"message":"{}"}}"#,
            "界".repeat(MAX_DISPLAY_SOURCE_BYTES)
        );
        let (source, source_truncated) = truncate_utf8(&raw, MAX_DISPLAY_SOURCE_BYTES);
        assert!(source_truncated);
        assert!(source.is_char_boundary(source.len()));
        assert!(source.len() <= MAX_DISPLAY_SOURCE_BYTES);

        let nested = format!("{}0{}", "[".repeat(120), "]".repeat(120));
        let (formatted, formatted_truncated) = pretty_json(&nested, 64);
        assert!(formatted_truncated);
        assert!(formatted.len() <= 64);
        assert!(formatted.is_char_boundary(formatted.len()));
    }

    #[test]
    fn headline_resolves_from_the_locale_supplied_by_each_render() {
        let english = headline(ErrorKind::Http, Some(429), "en");
        let chinese = headline(ErrorKind::Http, Some(429), "zh-CN");

        assert_ne!(english, chinese);
        assert!(english.contains("429"));
        assert!(chinese.contains("429"));
    }
}
