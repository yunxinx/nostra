//! Row renderer for the turn failure card (PRD R4, design contract 3).
//!
//! A failed turn used to render as a markdown blockquote holding a localized
//! sentence and nostra's own request id — which told the user nothing the
//! status bar didn't already say. This renderer shows what the provider
//! actually returned: the status line it can be identified by, and the
//! captured response body as a syntax-highlighted, copyable code block whose
//! `MarkdownBody` is created lazily on first expand and released on collapse.
//!
//! The body arrives as [`GatewayError::upstream_body`] and is **not** redacted
//! (see `llm::error`), so it belongs here in the view and nowhere else.

use gpui::{
    AnyElement, App, InteractiveElement as _, IntoElement, ParentElement as _, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _,
    transparent_white,
};
use gpui_component::{
    ActiveTheme as _, Colorize as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    clipboard::Clipboard,
    h_flex, v_flex,
};
use rust_i18n::t;

use crate::appearance::contrast;
use crate::chat::projection::{DisclosureState, RowKind};
use crate::llm::{ErrorKind, GatewayError};
use crate::ui::markdown::{MarkdownBody, MarkdownPresentation};

use super::typography;
use super::{
    DisclosureTarget, MaterializeContext, RowAction, RowChange, RowRenderContext, RowRenderer,
};

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
pub(crate) const MAX_DISPLAY_SOURCE_BYTES: usize = 32 * 1024;
const MAX_FORMATTED_BODY_BYTES: usize = 128 * 1024;

pub(crate) struct TurnErrorRenderer {
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
    /// Markdown for the display block (bounded, re-indented, fenced).
    display_markdown: Option<SharedString>,
    /// Whether the body is long enough to warrant a collapse toggle.
    collapsible: bool,
    /// Whether the rendered preview is shorter than the copyable raw response.
    preview_truncated: bool,
    /// Whether the raw response body is currently unfolded. A card without a
    /// collapse toggle (a short body) starts and stays expanded; a
    /// collapsible card starts collapsed.
    expanded: bool,
    /// The body entity, created lazily on first expand and released on
    /// collapse (the same materialization-window rule as activity rows).
    body: Option<MarkdownBody>,
    owner_id: u64,
    presentation: Option<MarkdownPresentation>,
    materialized: bool,
}

impl TurnErrorRenderer {
    pub(crate) fn new() -> Self {
        Self {
            kind: ErrorKind::Provider,
            status: None,
            code: None,
            request_id: None,
            raw_body: None,
            display_markdown: None,
            collapsible: false,
            preview_truncated: false,
            expanded: false,
            body: None,
            owner_id: 0,
            presentation: None,
            materialized: false,
        }
    }

    /// Capture the error's fields and prepare the (bounded) display markdown.
    /// No entity is built here: the body is created on expand, update phase.
    fn seed(&mut self, error: &GatewayError) {
        self.kind = error.kind;
        self.status = error.status;
        self.code = error.provider_code.clone().map(SharedString::from);
        self.request_id = error.request_id.clone().map(SharedString::from);
        self.collapsible = false;
        self.preview_truncated = false;
        self.display_markdown = None;
        let Some(raw) = error.upstream_body() else {
            self.raw_body = None;
            self.expanded = false;
            return;
        };

        let (source, source_truncated) = truncate_utf8(raw, MAX_DISPLAY_SOURCE_BYTES);
        let (display, format_truncated) = pretty_json(source, MAX_FORMATTED_BODY_BYTES);
        self.collapsible = display.lines().count() > COLLAPSE_LINE_THRESHOLD
            || display.len() > COLLAPSE_BYTE_THRESHOLD;
        self.preview_truncated = source_truncated || format_truncated;
        self.display_markdown = Some(fenced_block(&display, language_tag(source)).into());
        // The clipboard carries the captured response text, not the
        // re-indented display form. It is deliberately not redacted: this is
        // the provider response the user asked to inspect, kept out of Debug,
        // metrics, and canonical replay.
        self.raw_body = Some(raw.into());
        // A short body has no toggle, so it renders expanded from the start.
        // Re-seeding the same error (window re-entry) keeps the user's
        // expansion choice instead of collapsing it again; only a genuinely
        // different body resets the form.
        if self.raw_body.as_deref() != Some(raw) {
            self.expanded = !self.collapsible;
        }
    }

    fn clear(&mut self) {
        self.code = None;
        self.request_id = None;
        self.raw_body = None;
        self.display_markdown = None;
        self.collapsible = false;
        self.preview_truncated = false;
        self.expanded = false;
    }

    /// Build the body entity from the display markdown. Update phase only.
    fn build_body(&mut self, cx: &mut App) {
        self.body = None;
        if !self.expanded {
            return;
        }
        let Some(markdown) = self.display_markdown.clone() else {
            return;
        };
        let Some(presentation) = self.presentation.as_ref() else {
            return;
        };
        self.body = Some(MarkdownBody::new_with_presentation(
            &markdown,
            self.owner_id,
            presentation,
            cx,
        ));
    }
}

impl TurnErrorRenderer {
    #[cfg(test)]
    pub(crate) fn body_entity_id(&self) -> Option<gpui::EntityId> {
        self.body.as_ref().map(MarkdownBody::entity_id)
    }

    #[cfg(test)]
    pub(crate) fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn raw_body(&self) -> Option<&str> {
        self.raw_body.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn is_expanded(&self) -> bool {
        self.expanded
    }
}

impl RowRenderer for TurnErrorRenderer {
    fn kind(&self) -> RowKind {
        RowKind::TurnError
    }

    fn materialize(&mut self, ctx: &MaterializeContext, cx: &mut App) {
        self.owner_id = ctx.owner_id;
        self.presentation = Some(ctx.presentation.clone());
        match ctx.error {
            Some(error) => self.seed(error),
            None => self.clear(),
        }
        self.build_body(cx);
        self.materialized = true;
    }

    fn release(&mut self, _cx: &mut App) {
        // The body entity goes; whether the user had it open stays, so a row
        // that leaves and re-enters the retain zone keeps its form.
        self.body = None;
        self.materialized = false;
    }

    fn is_materialized(&self) -> bool {
        self.materialized
    }

    fn apply(&mut self, change: &RowChange, ctx: &MaterializeContext, cx: &mut App) {
        if let RowChange::Replace = change {
            self.owner_id = ctx.owner_id;
            self.presentation = Some(ctx.presentation.clone());
            match ctx.error {
                Some(error) => self.seed(error),
                None => self.clear(),
            }
            self.build_body(cx);
            self.materialized = true;
        }
    }

    fn render(&self, ctx: &RowRenderContext, _window: &mut Window, cx: &mut App) -> AnyElement {
        self.render_card(ctx, cx)
    }

    fn toggle_disclosure(&mut self, target: DisclosureTarget, cx: &mut App) {
        if target != DisclosureTarget::ErrorBody || !self.collapsible {
            return;
        }
        self.expanded = !self.expanded;
        if self.expanded {
            self.build_body(cx);
        } else {
            // Collapse releases the entity (materialization-window rule).
            self.body = None;
        }
    }

    fn disclosure(&self) -> DisclosureState {
        DisclosureState::default()
    }

    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl TurnErrorRenderer {
    fn render_card(&self, ctx: &RowRenderContext, cx: &mut App) -> AnyElement {
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
        // The headline is the card's only text on the washed fill; derive it
        // so every theme meets the body-text floor against the fill.
        let headline_color = contrast::text_on(danger, surface, cx);
        // The fill is a deliberate wash, so the border is what delimits the card:
        // it is the one part that must not fade into the pane.
        let edge = contrast::pane_outline(danger.mix_oklab(transparent_white(), 0.3), cx);
        let expanded = self.expanded;
        let locale = rust_i18n::locale();
        let row_id = ctx.row_id;

        v_flex()
            .id(("turn-error-card", row_id.turn.as_u64()))
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
                                    .text_color(headline_color)
                                    .child(headline(self.kind, self.status, &locale)),
                            )
                            .when_some(self.code.clone(), |this, code| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .font_family(mono_font_family.clone())
                                        .text_color(muted_foreground)
                                        .child(code),
                                )
                            })
                            .when_some(self.request_id.clone(), |this, request_id| {
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
                            .when(self.collapsible, |this| {
                                let (icon, tooltip) = if expanded {
                                    (IconName::ChevronUp, t!("chat.error.collapse"))
                                } else {
                                    (IconName::ChevronDown, t!("chat.error.expand"))
                                };
                                this.child(self.toggle_button(
                                    ctx.dispatch.clone(),
                                    row_id,
                                    icon,
                                    tooltip.to_string(),
                                ))
                            })
                            .when_some(self.raw_body.clone(), |this, raw| {
                                let copy_selector = format!("{}-copy", row_id.debug_name());
                                this.child(
                                    div().debug_selector(move || copy_selector).child(
                                        Clipboard::new(("turn-error-copy", row_id.turn.as_u64()))
                                            .value(raw)
                                            .tooltip(t!("chat.error.copy").to_string()),
                                    ),
                                )
                            }),
                    ),
            )
            .when_some(
                self.body.as_ref().filter(|_| self.expanded),
                |this, body| {
                    this.child(
                        v_flex()
                            .w_full()
                            .px_3()
                            .pb_3()
                            .gap_1()
                            // The fenced block brings its own muted surface and
                            // padding, so the card only supplies the outer inset.
                            .child(body.text_view(typography::prose(cx)))
                            .when(self.preview_truncated, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(muted_foreground)
                                        .child(t!("chat.error.preview_truncated").to_string()),
                                )
                            }),
                    )
                },
            )
            .into_any_element()
    }

    /// The fold toggle. It routes through the row-action dispatch so the
    /// body's lazy entity lifecycle stays in update phase.
    fn toggle_button(
        &self,
        dispatch: super::RowActionDispatch,
        row_id: crate::chat::projection::RowId,
        icon: IconName,
        tooltip: String,
    ) -> impl IntoElement {
        Button::new(("turn-error-toggle", row_id.turn.as_u64()))
            .ghost()
            .xsmall()
            .icon(icon)
            .tooltip(tooltip)
            .on_click(move |_, window, cx| {
                dispatch.send(
                    RowAction::ToggleDisclosure {
                        row_id,
                        target: DisclosureTarget::ErrorBody,
                    },
                    window,
                    cx,
                );
            })
    }
}

/// Localized one-liner for the failure. HTTP failures name their status because
/// that is the number a user matches against provider docs; the other kinds have
/// no upstream number to quote.
pub(crate) fn headline(kind: ErrorKind, status: Option<u16>, locale: &str) -> String {
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
pub(crate) fn language_tag(body: &str) -> Option<&'static str> {
    serde_json::from_str::<serde_json::Value>(body)
        .is_ok()
        .then_some("json")
}

/// Wrap `body` in a fenced code block whose fence is longer than any backtick
/// run inside it, so a body containing ``` cannot terminate its own block.
pub(crate) fn fenced_block(body: &str, lang: Option<&str>) -> String {
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
pub(crate) fn pretty_json(body: &str, max_output_bytes: usize) -> (String, bool) {
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

    #[test]
    fn fence_outgrows_backtick_runs_in_the_body() {
        let body = "text with ``` inside";
        let block = fenced_block(body, Some("json"));
        assert!(block.starts_with("````json\n"), "{block}");
        assert!(block.ends_with("\n````"), "{block}");
        // A plain body gets no language tag, but still a valid fence.
        assert!(fenced_block("plain", None).starts_with("```\n"));
    }

    /// The card is only useful if the highlighter is compiled in. Without a
    /// gpui-component Tree-sitter feature the whole `highlighter::Language` enum
    /// is replaced by a stub, so naming `Language::Json` fails the build. Easy to
    /// lose when bumping the dependency, and the symptom is quiet.
    #[test]
    fn json_highlighting_is_compiled_in() {
        assert_eq!(gpui_component::highlighter::Language::Json.name(), "json");
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

    /// Build a minimal materialize context around one error.
    fn error_context<'a>(
        error: &'a GatewayError,
        presentation: &'a MarkdownPresentation,
    ) -> MaterializeContext<'a> {
        MaterializeContext {
            row_id: crate::chat::projection::RowId::new(
                crate::chat::transcript::TurnId::from_u64_for_test(1),
                crate::chat::transcript::PartId::NONE,
                RowKind::TurnError,
            ),
            part: None,
            paired_result: None,
            error: Some(error),
            presentation,
            user_message_markdown: false,
            owner_id: 0,
            append_replays_part: false,
        }
    }

    fn presentation(cx: &App) -> MarkdownPresentation {
        MarkdownPresentation::for_test(cx)
    }

    #[gpui::test]
    fn long_single_line_response_collapses_without_changing_the_raw_copy(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        cx.update(|cx| {
            let raw = format!("<html><body>{}</body></html>", "x".repeat(8 * 1024));
            let error = crate::llm::GatewayError::http(502, None).with_upstream_body(raw.clone());

            let presentation = presentation(cx);
            let ctx = error_context(&error, &presentation);
            let mut renderer = TurnErrorRenderer::new();
            renderer.materialize(&ctx, cx);

            assert!(
                renderer.collapsible,
                "long wrapped content starts collapsed"
            );
            assert_eq!(
                renderer.raw_body(),
                Some(raw.as_str()),
                "collapse and preview limits must not rewrite the copyable response"
            );
            assert!(!renderer.is_expanded(), "starts collapsed");
            assert!(
                renderer.body_entity_id().is_none(),
                "the body entity is lazy: nothing is built before the first expand"
            );
        });
    }

    #[gpui::test]
    fn expanding_builds_the_body_and_collapsing_releases_it(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(|cx| {
            // Long enough to cross the collapse threshold, so the body has a
            // toggle and starts collapsed.
            let raw = format!(
                r#"{{"error":{{"message":"{}","code":"rate_limit_exceeded"}}}}"#,
                "x".repeat(4 * 1024)
            );
            let error = crate::llm::GatewayError::http(429, Some("rate_limit_exceeded".into()))
                .with_upstream_body(raw);

            let presentation = presentation(cx);
            let ctx = error_context(&error, &presentation);
            let mut renderer = TurnErrorRenderer::new();
            renderer.materialize(&ctx, cx);
            assert!(renderer.body_entity_id().is_none(), "lazy body");

            renderer.toggle_disclosure(DisclosureTarget::ErrorBody, cx);
            let open_body = renderer.body_entity_id().expect("body built on expand");

            renderer.toggle_disclosure(DisclosureTarget::ErrorBody, cx);
            assert!(
                renderer.body_entity_id().is_none(),
                "collapse releases the body entity"
            );

            renderer.toggle_disclosure(DisclosureTarget::ErrorBody, cx);
            let reopened = renderer.body_entity_id().expect("body rebuilt");
            assert_ne!(open_body, reopened, "a fresh entity, not the old one");
        });
    }

    #[gpui::test]
    fn http_failure_prepares_a_collapsible_card_with_a_copyable_raw_body(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        cx.update(|cx| {
            // A body long enough to cross the collapse threshold once re-indented.
            let raw = r#"{"error":{"message":"Rate limit reached for gpt-4o","type":"requests","param":null,"code":"rate_limit_exceeded","details":{"limit":10000,"used":10000,"reset":"60s","scope":"organization","plan":"tier-1","window":"1m","retry_after":60,"bucket":"rpm"}}}"#;
            let mut error =
                crate::llm::GatewayError::http(429, Some("rate_limit_exceeded".into()))
                    .with_upstream_body(raw);
            error.request_id = Some("nostra-1".into());

            let presentation = presentation(cx);
            let ctx = error_context(&error, &presentation);
            let mut renderer = TurnErrorRenderer::new();
            renderer.materialize(&ctx, cx);

            assert!(
                renderer.collapsible,
                "a body past the line threshold offers a collapse toggle"
            );
            // The clipboard gets the untouched captured text, not the display form.
            assert_eq!(renderer.raw_body(), Some(raw));
            assert_eq!(renderer.request_id(), Some("nostra-1"));
            assert!(headline(renderer.kind, renderer.status, "en").contains("429"));

            // The markdown handed to the body is a json-tagged fence, so the
            // highlighter engages.
            let markdown = renderer.display_markdown.clone().expect("display markdown");
            assert!(markdown.starts_with("```json\n"), "{markdown}");
            assert!(
                markdown.contains("\"code\": \"rate_limit_exceeded\""),
                "{markdown}"
            );
        });
    }

    #[gpui::test]
    fn local_failure_renders_headline_only(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(|cx| {
            // A configuration failure never reached the provider, so there is no body.
            let error = crate::llm::GatewayError::configuration("model selection is unavailable");

            let presentation = presentation(cx);
            let ctx = error_context(&error, &presentation);
            let mut renderer = TurnErrorRenderer::new();
            renderer.materialize(&ctx, cx);

            assert!(renderer.display_markdown.is_none());
            assert!(renderer.raw_body.is_none());
            assert!(!renderer.collapsible, "nothing to collapse");
            assert!(!headline(renderer.kind, renderer.status, "en").is_empty());
        });
    }
}
