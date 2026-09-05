//! Row renderer for one paired tool call + result (PRD R2, design contract 2).
//!
//! The header is a full-width clickable row: tool icon, call name, status
//! (running shimmer / completed / failed), the observed duration when both
//! endpoints are known, and an expansion chevron. The body stays folded by
//! default; opening it reveals the call arguments as a fenced, highlighted
//! JSON block (independently foldable) and the tool result. Both content
//! bodies are lazy: the arguments body is created the first time the
//! arguments section opens, the result body the first time the row opens,
//! and both are dropped again when their section closes — the same
//! materialization-window rule the reasoning row follows.
//!
//! A row for an unpaired `ToolResult` part (the standalone `Role::Tool`
//! turn) keeps the P1 muted result card with an eager body: it has no call
//! header to fold.
//!
//! Results larger than [`typography::RESULT_BUDGET_BYTES`] render inside the
//! reasoning row's budgeted, internally scrollable viewport, replaying wheel
//! input through [`RowAction::ReplayNestedScroll`].

use std::time::Instant;

use gpui::{
    AnyElement, App, ElementId, InteractiveElement as _, IntoElement, ListState,
    ParentElement as _, ScrollWheelEvent, SharedString, Styled as _, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    shimmer::ShimmerText,
    v_flex,
};
use rust_i18n::t;

use crate::appearance::contrast;
use crate::chat::SmoothScrollState;
use crate::chat::projection::{ActivityDisclosure, DisclosureState, RowKind};
use crate::chat::transcript::PartSource;
use crate::llm::ToolResult;
use crate::ui::markdown::{MarkdownBody, MarkdownPresentation};

use super::turn_error::{MAX_DISPLAY_SOURCE_BYTES, fenced_block, language_tag, pretty_json};
use super::{
    DisclosureTarget, MaterializeContext, NestedScrollReplay, RowAction, RowChange,
    RowRenderContext, RowRenderer, typography,
};

/// Where a tool call stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivityStatus {
    Running,
    Completed,
    Failed,
}

pub(crate) struct ToolActivityRenderer {
    /// Stable element-id base: the part's ui id survives list splices.
    ui_id: u64,
    name: String,
    /// Raw arguments exactly as the model sent them.
    arguments: Option<String>,
    /// The paired result, once the transcript learned about it.
    result: Option<ToolResult>,
    /// This row renders an unpaired `ToolResult` part (P1 muted card).
    is_result_row: bool,
    /// Stamped when the row materializes as a live streaming call; the
    /// finish is inferred when the paired result is applied. A row that
    /// materializes after the call already finished never knows its start,
    /// so it shows no duration.
    started_at: Option<Instant>,
    finished_at: Option<Instant>,
    disclosure: ActivityDisclosure,
    /// Lazily created bodies (see the module header).
    arguments_body: Option<MarkdownBody>,
    result_body: Option<MarkdownBody>,
    /// What each body currently holds, for change detection (the bodies own
    /// the authoritative content; these mirrors never render).
    arguments_rendered: Option<String>,
    result_rendered: Option<String>,
    /// Handle onto the budgeted result viewport's retained list.
    scroll: Option<ListState>,
    follow: bool,
    smooth: SmoothScrollState,
    owner_id: u64,
    presentation: Option<MarkdownPresentation>,
    materialized: bool,
}

impl ToolActivityRenderer {
    pub(crate) fn new() -> Self {
        Self {
            ui_id: 0,
            name: String::new(),
            arguments: None,
            result: None,
            is_result_row: false,
            started_at: None,
            finished_at: None,
            disclosure: ActivityDisclosure::Collapsed,
            arguments_body: None,
            result_body: None,
            arguments_rendered: None,
            result_rendered: None,
            scroll: None,
            follow: false,
            smooth: SmoothScrollState::default(),
            owner_id: 0,
            presentation: None,
            materialized: false,
        }
    }

    fn status(&self) -> ActivityStatus {
        match &self.result {
            Some(result) if result.is_error => ActivityStatus::Failed,
            Some(_) => ActivityStatus::Completed,
            None => ActivityStatus::Running,
        }
    }

    /// The display markdown for one raw arguments string: re-indented when it
    /// parses as JSON, fenced so the syntax highlighter engages.
    fn arguments_markdown(arguments: &str) -> String {
        let (display, _) = pretty_json(arguments, MAX_DISPLAY_SOURCE_BYTES);
        fenced_block(&display, language_tag(arguments))
    }

    /// Whether this row wants a result body right now: result rows always
    /// (they have no header to fold), activity rows only while open.
    fn wants_result_body(&self) -> bool {
        self.is_result_row || self.disclosure != ActivityDisclosure::Collapsed
    }

    /// Create or refresh the lazy bodies from the current content. Update
    /// phase only (materialize / apply / toggle).
    fn sync_bodies(&mut self, cx: &mut App) {
        let Some(presentation) = self.presentation.clone() else {
            return;
        };
        let presentation = &presentation;

        // Arguments section, only while the row is open and the section is.
        if self.disclosure.arguments_open()
            && let Some(arguments) = self.arguments.as_deref().filter(|a| !a.is_empty())
        {
            let markdown = Self::arguments_markdown(arguments);
            if let Some(body) = self.arguments_body.as_mut() {
                if self.arguments_rendered.as_deref() != Some(markdown.as_str()) {
                    body.set_text(&markdown, cx);
                    self.arguments_rendered = Some(markdown);
                }
            } else {
                self.arguments_body = Some(MarkdownBody::new_with_presentation(
                    &markdown,
                    self.owner_id,
                    presentation,
                    cx,
                ));
                self.arguments_rendered = Some(markdown);
            }
        } else {
            self.arguments_body = None;
            self.arguments_rendered = None;
        }

        // Result body.
        let wanted = self.wants_result_body();
        let content = self
            .result
            .as_ref()
            .map(|result| result.content.clone())
            .unwrap_or_default();
        if wanted && !content.is_empty() {
            if let Some(body) = self.result_body.as_mut() {
                if self.result_rendered.as_deref() != Some(content.as_str()) {
                    body.set_text(&content, cx);
                    self.result_rendered = Some(content);
                }
            } else {
                let body =
                    MarkdownBody::new_with_presentation(&content, self.owner_id, presentation, cx);
                self.result_rendered = Some(content);
                self.scroll = Some(body.scroll_state(cx));
                self.result_body = Some(body);
            }
        } else {
            self.result_body = None;
            self.result_rendered = None;
            self.scroll = None;
            self.smooth.cancel_motion();
        }
    }

    fn open(&mut self, cx: &mut App) {
        self.disclosure = ActivityDisclosure::Open {
            arguments_open: true,
        };
        self.sync_bodies(cx);
    }

    fn close(&mut self) {
        self.disclosure = ActivityDisclosure::Collapsed;
        self.arguments_body = None;
        self.arguments_rendered = None;
        self.result_body = None;
        self.result_rendered = None;
        self.scroll = None;
        self.smooth.cancel_motion();
    }

    fn duration_label(&self) -> Option<String> {
        let started = self.started_at?;
        let finished = self.finished_at?;
        // One decimal, floored at 0.1s: a sub-100ms tool call is real but
        // "0.0s" reads as a rendering bug.
        let seconds = finished.duration_since(started).as_secs_f64().max(0.1);
        Some(format!("{seconds:.1}s"))
    }
}

impl ToolActivityRenderer {
    #[cfg(test)]
    pub(crate) fn result_body_entity_id(&self) -> Option<gpui::EntityId> {
        self.result_body.as_ref().map(MarkdownBody::entity_id)
    }

    #[cfg(test)]
    pub(crate) fn result_body_owner_for_test(&self) -> Option<u64> {
        self.result_body.as_ref().map(MarkdownBody::owner_id)
    }

    #[cfg(test)]
    pub(crate) fn arguments_body_entity_id(&self) -> Option<gpui::EntityId> {
        self.arguments_body.as_ref().map(MarkdownBody::entity_id)
    }

    #[cfg(test)]
    pub(crate) fn status_for_test(&self) -> ActivityStatus {
        self.status()
    }
}

impl RowRenderer for ToolActivityRenderer {
    fn kind(&self) -> RowKind {
        RowKind::ToolActivity
    }

    fn materialize(&mut self, ctx: &MaterializeContext, cx: &mut App) {
        self.owner_id = ctx.owner_id;
        self.presentation = Some(ctx.presentation.clone());
        self.ui_id = ctx.row_id.part.as_u64();
        match ctx.part {
            Some(part) if matches!(&part.source, PartSource::ToolCall { .. }) => {
                self.is_result_row = false;
                if let PartSource::ToolCall {
                    name, tool_call, ..
                } = &part.source
                {
                    self.name = name.clone();
                    self.arguments = tool_call
                        .as_ref()
                        .map(|call| call.raw_arguments.clone())
                        .filter(|args| !args.is_empty());
                }
                self.result = ctx.paired_result.cloned();
                if self.result.is_none() && !part.finished {
                    // A live call: the clock starts now. A call restored from
                    // storage keeps `started_at = None` and stays honest
                    // about not knowing how long it ran. A finished call
                    // re-materializing here (window re-entry) does not stamp
                    // `finished_at` either: the result likely arrived while
                    // the row was released, and billing the off-screen time
                    // to the tool would inflate the duration.
                    if self.started_at.is_none() {
                        self.started_at = Some(Instant::now());
                    }
                }
            }
            Some(part) if matches!(&part.source, PartSource::ToolResult(_)) => {
                // Unpaired result row: the muted card with an eager body.
                self.is_result_row = true;
                self.name.clear();
                self.arguments = None;
                self.started_at = None;
                self.finished_at = None;
                self.result = ctx.paired_result.cloned();
            }
            _ => {
                self.is_result_row = false;
                self.name.clear();
                self.arguments = None;
                self.result = None;
                self.started_at = None;
                self.finished_at = None;
            }
        }
        self.sync_bodies(cx);
        self.materialized = true;
    }

    fn release(&mut self, _cx: &mut App) {
        // Entities go, disclosure stays: the projection (and this struct)
        // re-open the same bodies on the next materialization.
        self.arguments_body = None;
        self.arguments_rendered = None;
        self.result_body = None;
        self.result_rendered = None;
        self.scroll = None;
        self.smooth.cancel_motion();
        self.materialized = false;
    }

    fn is_materialized(&self) -> bool {
        self.materialized
    }

    fn apply(&mut self, change: &RowChange, ctx: &MaterializeContext, cx: &mut App) {
        match change {
            RowChange::Replace => {
                match ctx.part {
                    Some(part) if matches!(&part.source, PartSource::ToolCall { .. }) => {
                        if let PartSource::ToolCall {
                            name, tool_call, ..
                        } = &part.source
                        {
                            self.name = name.clone();
                            self.arguments = tool_call
                                .as_ref()
                                .map(|call| call.raw_arguments.clone())
                                .filter(|args| !args.is_empty());
                        }
                        let next = ctx.paired_result.cloned();
                        if next.is_some() && self.result.is_none() {
                            // Result insertion: infer the finish endpoint.
                            if self.started_at.is_some() && self.finished_at.is_none() {
                                self.finished_at = Some(Instant::now());
                            }
                        }
                        self.result = next;
                    }
                    Some(part) if matches!(&part.source, PartSource::ToolResult(_)) => {
                        self.is_result_row = true;
                        self.result = ctx.paired_result.cloned();
                    }
                    _ => {}
                }
                self.sync_bodies(cx);
                self.materialized = true;
            }
            RowChange::Append { .. } | RowChange::Finished => {}
        }
    }

    fn render(&self, ctx: &RowRenderContext, window: &mut Window, cx: &mut App) -> AnyElement {
        // A waiting turn hides its rows behind the shimmer (P1 parity).
        if ctx.waiting {
            return div().into_any_element();
        }
        if self.is_result_row {
            return self.render_result_card(ctx, cx);
        }
        v_flex()
            .w_full()
            // The first member of an expanded group carries the affordance
            // that re-folds it: the collapsed header's toggle is gone while
            // the group is expanded, so this is the only way back.
            .children(
                ctx.group
                    .filter(|_| ctx.group_leader)
                    .map(|group| self.render_group_collapse(group, ctx, cx)),
            )
            .child(self.render_header(ctx, window, cx))
            .when(self.disclosure != ActivityDisclosure::Collapsed, |this| {
                this.child(self.render_body(ctx, window, cx))
            })
            .into_any_element()
    }

    fn disclosure(&self) -> DisclosureState {
        DisclosureState {
            activity: self.disclosure,
            ..DisclosureState::default()
        }
    }

    fn sync_disclosure(&mut self, disclosure: DisclosureState) {
        self.disclosure = disclosure.activity;
    }

    fn toggle_disclosure(&mut self, target: DisclosureTarget, cx: &mut App) {
        match target {
            DisclosureTarget::Activity => {
                if self.disclosure == ActivityDisclosure::Collapsed {
                    self.open(cx);
                } else {
                    self.close();
                }
            }
            DisclosureTarget::ActivityArguments => {
                if let ActivityDisclosure::Open { arguments_open } = &mut self.disclosure {
                    *arguments_open = !*arguments_open;
                    self.sync_bodies(cx);
                }
            }
            _ => {}
        }
    }

    fn nested_scroll_replay(&mut self) -> Option<NestedScrollReplay<'_>> {
        let scroll = self.scroll.clone()?;
        Some(NestedScrollReplay {
            scroll,
            follow: &mut self.follow,
            smooth: &mut self.smooth,
        })
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

impl ToolActivityRenderer {
    /// The full-width clickable header: icon, name, status, duration, chevron.
    /// The library `Button` provides the complete "Custom Clickable Rows"
    /// contract — `Role::Button`, a keyed tab-stop `FocusHandle`, Enter/Space
    /// keyboard activation, and the desktop default cursor.
    fn render_header(
        &self,
        ctx: &RowRenderContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let theme = cx.theme();
        let header_bg = contrast::pane_block(theme.muted, cx);
        let header_text = contrast::text_on(theme.foreground, header_bg, cx);
        let muted_text = contrast::text_on(theme.muted_foreground, header_bg, cx);
        let radius = theme.radius;
        let row_id = ctx.row_id;
        let ui_id = self.ui_id;
        let name: SharedString = self.name.clone().into();
        let status = self.status();
        let open = self.disclosure != ActivityDisclosure::Collapsed;
        let duration = self.duration_label();

        let dispatch = ctx.dispatch.clone();
        let header_selector = format!("{}-header", row_id.debug_name());

        let status_element: AnyElement = match status {
            ActivityStatus::Running => div()
                .text_sm()
                .child(
                    ShimmerText::new(t!("chat.tool.running").to_string())
                        .id(("tool-activity-running", ui_id))
                        .text_color(muted_text),
                )
                .into_any_element(),
            ActivityStatus::Completed => div()
                .text_sm()
                .text_color(muted_text)
                .child(t!("chat.tool.completed").to_string())
                .into_any_element(),
            ActivityStatus::Failed => div()
                .text_sm()
                .text_color(theme.danger)
                .child(t!("chat.tool.failed").to_string())
                .into_any_element(),
        };

        // `Button` has no debug selector of its own, so the clickable header
        // is wrapped in the selector div tests click through.
        div()
            .debug_selector(move || header_selector)
            .child(
                Button::new(ElementId::NamedInteger(
                    "tool-activity-toggle".into(),
                    ui_id,
                ))
                .ghost()
                .w_full()
                .rounded(radius)
                .bg(header_bg)
                .accessibility_label(t!("chat.tool_requested", name = name.clone()).to_string())
                .on_click(move |_, window, cx| {
                    dispatch.send(
                        RowAction::ToggleDisclosure {
                            row_id,
                            target: DisclosureTarget::Activity,
                        },
                        window,
                        cx,
                    );
                })
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::default()
                                .path("icons/tool.svg")
                                .text_color(muted_text),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_shrink(1.)
                                .text_sm()
                                .font_medium()
                                .text_color(header_text)
                                .text_ellipsis()
                                .child(name),
                        )
                        .child(status_element)
                        .child(
                            h_flex()
                                .ml_auto()
                                .flex_none()
                                .items_center()
                                .gap_1()
                                .children(duration.map(|duration| {
                                    div().text_xs().text_color(muted_text).child(duration)
                                }))
                                .child(
                                    Icon::default()
                                        .path(if open {
                                            "icons/chevron-down.svg"
                                        } else {
                                            "icons/chevron-right.svg"
                                        })
                                        .text_color(muted_text),
                                ),
                        ),
                ),
            )
            .into_any_element()
    }

    /// The open body: arguments (independently foldable) and the result.
    fn render_body(&self, ctx: &RowRenderContext, window: &mut Window, cx: &mut App) -> AnyElement {
        let theme = cx.theme();
        let header_bg = contrast::pane_block(theme.muted, cx);
        // The body sits directly against the header with no divider, so its
        // surface must clear the nested-surface floor against the header fill.
        let body_surface = contrast::distinct_surface(
            theme.background.into(),
            theme.background,
            header_bg,
            contrast::MIN_NESTED_SURFACE_CONTRAST,
            theme.is_dark(),
        );
        let body_text = contrast::text_on(theme.group_box_foreground, body_surface.color, cx);
        let muted_text = contrast::text_on(theme.muted_foreground, body_surface.color, cx);
        let row_id = ctx.row_id;
        let ui_id = self.ui_id;
        let arguments_selector = format!("{}-arguments", row_id.debug_name());

        // The danger rail for an error result, derived up front: the
        // FluentBuilder closures below capture `cx` immutably, and a
        // `contrast::*` derivation needs `&mut App`.
        let error_rail = self
            .result
            .as_ref()
            .filter(|result| result.is_error && !result.content.is_empty())
            .map(|_| contrast::pane_outline(theme.danger, cx));

        let dispatch_arguments = ctx.dispatch.clone();
        let arguments_row_id = row_id;
        let arguments_open = self.disclosure.arguments_open();

        v_flex()
            .w_full()
            .mt_1()
            .gap_2()
            .rounded(theme.radius)
            .bg(body_surface.background)
            .p_2()
            .text_color(body_text)
            // Arguments section: a titled, independently foldable block.
            .when_some(self.arguments.as_ref(), |this, _| {
                this.child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            div().debug_selector(move || arguments_selector).child(
                                Button::new(ElementId::NamedInteger(
                                    "tool-activity-arguments".into(),
                                    ui_id,
                                ))
                                .ghost()
                                .xsmall()
                                .compact()
                                .label(t!("chat.tool.arguments").to_string())
                                .icon(if arguments_open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .on_click(move |_, window, cx| {
                                    dispatch_arguments.send(
                                        RowAction::ToggleDisclosure {
                                            row_id: arguments_row_id,
                                            target: DisclosureTarget::ActivityArguments,
                                        },
                                        window,
                                        cx,
                                    );
                                }),
                            ),
                        )
                        .when(arguments_open, |this| {
                            this.when_some(self.arguments_body.as_ref(), |this, body| {
                                this.child(
                                    body.text_view(typography::reasoning(cx))
                                        .text_color(body_text),
                                )
                            })
                        }),
                )
            })
            // Result section, with the danger rail on error results.
            .when_some(
                self.result
                    .as_ref()
                    .filter(|result| !result.content.is_empty()),
                |this, result| {
                    let rail = if result.is_error { error_rail } else { None };
                    this.child(
                        v_flex()
                            .w_full()
                            .min_w_0()
                            .gap_1()
                            .when_some(rail, |this, edge| {
                                this.border_l_2().border_color(edge).pl_2()
                            })
                            .children(self.render_result(ctx, window, cx, muted_text)),
                    )
                },
            )
            .into_any_element()
    }

    /// The result content: natural height, or the budgeted scrollable
    /// viewport once the source outgrows [`typography::RESULT_BUDGET_BYTES`].
    fn render_result(
        &self,
        ctx: &RowRenderContext,
        window: &mut Window,
        cx: &mut App,
        text_color: gpui::Hsla,
    ) -> Vec<AnyElement> {
        let Some(body) = self.result_body.as_ref() else {
            return Vec::new();
        };
        let over_budget = self
            .result
            .as_ref()
            .is_some_and(|result| result.content.len() > typography::RESULT_BUDGET_BYTES);
        if !over_budget {
            return vec![
                body.text_view(typography::prose(cx))
                    .text_color(text_color)
                    .into_any_element(),
            ];
        }
        let line_height = window.line_height();
        let budget_height = (line_height * typography::BUDGET_MIN_LINES)
            .max(ctx.viewport_height * typography::BUDGET_VIEWPORT_RATIO);
        let row_id = ctx.row_id;
        let ui_id = self.ui_id;
        // Painted-frame anchor for the eased replay (same contract as the
        // reasoning viewport).
        let anchor = body.scroll_state(cx).scroll_px_offset_for_scrollbar();
        let dispatch = ctx.dispatch.clone();
        let on_scroll = move |event: &ScrollWheelEvent, window: &mut Window, cx: &mut App| {
            dispatch.send(
                RowAction::ReplayNestedScroll {
                    row_id,
                    anchor,
                    dy: event.delta.pixel_delta(window.line_height()).y,
                    precise: event.delta.precise(),
                },
                window,
                cx,
            );
            cx.stop_propagation();
        };
        vec![
            div()
                .id(ElementId::NamedInteger("tool-result-body".into(), ui_id))
                .relative()
                .w_full()
                .h(budget_height)
                .on_scroll_wheel(on_scroll)
                .child(
                    div().size_full().min_w_0().child(
                        body.scrollable_text_view(typography::prose(cx))
                            .text_color(text_color),
                    ),
                )
                .into_any_element(),
        ]
    }

    /// P1 muted result card for an unpaired `ToolResult` part.
    fn render_result_card(&self, _ctx: &RowRenderContext, cx: &mut App) -> AnyElement {
        let muted_foreground = cx.theme().muted_foreground;
        let (radius_lg, muted) = {
            let theme = cx.theme();
            (theme.radius_lg, theme.muted)
        };
        let card = contrast::pane_block(muted, cx);
        let card_text = contrast::text_on(muted_foreground, card, cx);
        h_flex()
            .w_full()
            .justify_start()
            .child(
                div()
                    .min_w_0()
                    // Layout-structure constant (review-exempted): the P1 card
                    // cap, mirroring the user bubble so a long tool output
                    // cannot span the whole conversation column.
                    .max_w(gpui::px(560.))
                    .rounded(radius_lg)
                    .bg(card)
                    .text_color(card_text)
                    .px_3()
                    .py_1p5()
                    .when(!self.name.is_empty(), |this| {
                        this.child(
                            div().text_color(muted_foreground).child(
                                t!("chat.tool_requested", name = self.name.clone()).to_string(),
                            ),
                        )
                    })
                    .when_some(self.result_body.as_ref(), |this, body| {
                        this.child(body.text_view(typography::prose(cx)))
                    }),
            )
            .into_any_element()
    }

    fn render_group_collapse(
        &self,
        group: crate::chat::projection::RowId,
        ctx: &RowRenderContext,
        cx: &mut App,
    ) -> AnyElement {
        let dispatch = ctx.dispatch.clone();
        let row_id = ctx.row_id;
        let collapse_selector = format!("{}-collapse", row_id.debug_name());
        div()
            .text_color(cx.theme().muted_foreground)
            .debug_selector(move || collapse_selector)
            .child(
                Button::new(("tool-group-collapse", row_id.part.as_u64()))
                    .ghost()
                    .xsmall()
                    .compact()
                    .label(t!("chat.tool_group_collapse").to_string())
                    .tooltip(t!("chat.tool_group_collapse").to_string())
                    .on_click(move |_, window, cx| {
                        dispatch.send(
                            RowAction::ToggleDisclosure {
                                row_id: group,
                                target: DisclosureTarget::Group,
                            },
                            window,
                            cx,
                        );
                    }),
            )
            .into_any_element()
    }
}
