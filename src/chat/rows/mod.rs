//! Row renderers: one self-contained wrapper per projected row kind.
//!
//! A renderer owns only the content entities it creates itself
//! (`MarkdownBody` and its keyed views). Every round trip back to
//! the view — hover, measurement, disclosure, nested scrolling, clipboard —
//! goes through [`RowAction`] and [`RowActionDispatch`], which holds a *weak*
//! handle. No renderer struct may contain an `Entity<ChatView>` or an
//! `Entity<Transcript>`; materialization happens only in update-phase
//! (`sync_window` / deferred handlers), never inside a render closure.

mod prose;
mod reasoning;
mod tool_activity;
mod tool_activity_group;
mod turn_actions;
mod turn_error;
pub(crate) mod typography;
mod user_bubble;

#[cfg(test)]
mod tests;

pub(crate) use self::prose::ProseRenderer;
pub(crate) use self::reasoning::ReasoningRenderer;
pub(crate) use self::tool_activity::ToolActivityRenderer;
pub(crate) use self::tool_activity_group::ToolActivityGroupRenderer;
pub(crate) use self::turn_actions::TurnActionsRenderer;
pub(crate) use self::turn_error::TurnErrorRenderer;
pub(crate) use self::user_bubble::UserBubbleRenderer;

use gpui::{AnyElement, App, ListState, Pixels, Point, SharedString, Window};

use crate::chat::SmoothScrollState;
use crate::chat::projection::{DisclosureState, RowId, RowKind};
use crate::chat::transcript::{Part, PartSource, Role, Transcript, TurnId};
use crate::llm::{GatewayError, ToolResult};
use crate::ui::markdown::MarkdownPresentation;

use super::ChatView;

/// What a renderer should do with a content change.
#[derive(Debug)]
pub(crate) enum RowChange<'a> {
    /// Streaming append for the row's part.
    Append { delta: &'a str },
    /// The part reached its stream boundary (markdown finish).
    Finished,
    /// Authoritative replacement; the fresh content is re-read from
    /// `ctx`. The renderer keeps its entity when the part identity survived.
    Replace,
}

/// Read-only inputs a renderer needs to build or refresh its content.
pub(crate) struct MaterializeContext<'a> {
    pub(crate) row_id: RowId,
    pub(crate) part: Option<&'a Part>,
    /// Content of the tool result paired with this row's call, if any.
    pub(crate) paired_result: Option<&'a ToolResult>,
    pub(crate) error: Option<&'a GatewayError>,
    pub(crate) presentation: &'a MarkdownPresentation,
    pub(crate) user_message_markdown: bool,
    pub(crate) owner_id: u64,
    /// Whether the immediately-following `Append` event replays this part's
    /// content from the beginning. True only for the row of a part a live
    /// `PartInserted` just created: the transcript part already carries the
    /// triggering delta by the time the insert is handled, so unfinished
    /// prose/reasoning renderers must seed empty and let the Append rebuild
    /// the text. Every other materialization (window sync, cold restore,
    /// first layout) re-reads the authoritative accumulated content instead.
    pub(crate) append_replays_part: bool,
}

impl<'a> MaterializeContext<'a> {
    pub(crate) fn prose_text(&self) -> Option<&'a str> {
        self.part.and_then(|part| match &part.source {
            PartSource::Prose { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }

    pub(crate) fn reasoning_display(&self) -> Option<&'a str> {
        self.part.and_then(|part| match &part.source {
            PartSource::Reasoning { reasoning, .. } => Some(reasoning.display.as_str()),
            _ => None,
        })
    }
}

/// Everything a renderer's `render` may read, plus the dispatch back to the
/// view. Built per row per frame by the view; renderers never store it.
pub(crate) struct RowRenderContext {
    pub(crate) row_id: RowId,
    pub(crate) role: Role,
    /// The pointer is over some row of this row's turn.
    pub(crate) turn_hovered: bool,
    /// This row's turn renders the wait shimmer instead of its rows; the
    /// view renders the shimmer itself, and renderers render nothing.
    pub(crate) waiting: bool,
    /// Height of the conversation viewport, recorded by the view at
    /// prepaint. Viewport-relative height budgets (reasoning) resolve
    /// against it.
    pub(crate) viewport_height: Pixels,
    /// Member count for a collapsed tool-activity group header.
    pub(crate) group_count: usize,
    /// The most recent member's name for a collapsed tool-activity group
    /// header ("… · Latest: <name>").
    pub(crate) group_latest: Option<SharedString>,
    /// The expanded tool-activity group this row belongs to. Declared on
    /// every member row while the group is expanded.
    pub(crate) group: Option<RowId>,
    /// Whether this row leads its expanded group: it renders the collapse
    /// affordance, sending [`RowAction::ToggleDisclosure`] for `group`.
    pub(crate) group_leader: bool,
    pub(crate) dispatch: RowActionDispatch,
}

/// Which foldable surface a [`RowAction::ToggleDisclosure`] targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DisclosureTarget {
    /// The reasoning trigger row: collapsed chip ↔ budgeted viewport.
    Reasoning,
    /// The reasoning full-text toggle: budgeted viewport ↔ natural height.
    ReasoningFull,
    /// A tool activity row's body.
    Activity,
    /// A tool activity row's arguments section, inside an open body.
    ActivityArguments,
    /// A tool-activity group row (expand/collapse the member list).
    Group,
    /// A turn error row's raw response body.
    ErrorBody,
}

/// Actions a renderer can send back to the owning view.
#[derive(Debug)]
pub(crate) enum RowAction {
    /// Toggle a foldable row surface.
    ToggleDisclosure {
        row_id: RowId,
        target: DisclosureTarget,
    },
    /// A renderer with its own viewport received wheel input; the view owns
    /// the easing replay and the nested scroll boundary.
    ReplayNestedScroll {
        row_id: RowId,
        anchor: Point<Pixels>,
        /// Signed pixel delta of the wheel gesture.
        dy: Pixels,
        /// Whether the delta was pixel-precise (native path, no easing).
        precise: bool,
    },
    /// The row's wrapper was painted at `height`.
    Measured {
        row_id: RowId,
        height: Pixels,
        settled: bool,
    },
    /// The pointer entered (`entered: true`) or left a row of `turn`.
    /// Directional on purpose: gpui re-evaluates every hover element in one
    /// mouse-move, so a bare `Option<TurnId>` collapses "which row left" into
    /// `None` and older rows clear newer ones' hover.
    HoverTurn { turn: TurnId, entered: bool },
}

/// Weak handle back to the owning view. Renderers clone this into element
/// closures; upgrading happens at event time, never during render.
#[derive(Clone)]
pub(crate) struct RowActionDispatch(gpui::WeakEntity<ChatView>);

impl RowActionDispatch {
    pub(crate) fn new(view: gpui::WeakEntity<ChatView>) -> Self {
        Self(view)
    }

    pub(crate) fn send(&self, action: RowAction, window: &mut Window, cx: &mut App) {
        let Some(view) = self.0.upgrade() else {
            return;
        };
        view.update(cx, |view, cx| view.handle_row_action(action, window, cx));
    }

    /// Resolve the copyable text for `row_id` at click time, so the clipboard
    /// always reflects the latest transcript state.
    pub(crate) fn clipboard_value(&self, row_id: RowId, cx: &mut App) -> SharedString {
        let Some(view) = self.0.upgrade() else {
            return SharedString::default();
        };
        view.read(cx)
            .copy_source_for(row_id, cx)
            .unwrap_or_default()
    }
}

/// The behavior every row kind implements. Exhaustiveness is enforced by
/// [`renderer_for`]'s `match` on [`RowKind`].
pub(crate) trait RowRenderer {
    fn kind(&self) -> RowKind;

    /// Create content entities from the transcript-backed context. Called
    /// only from update-phase window synchronization.
    fn materialize(&mut self, ctx: &MaterializeContext, cx: &mut App);

    /// Drop content entities, keeping any state the projection already
    /// captured (heights, disclosure).
    fn release(&mut self, cx: &mut App);

    fn is_materialized(&self) -> bool;

    /// Apply a content change; `ctx` re-reads authoritative content for
    /// `Replace`.
    fn apply(&mut self, change: &RowChange, ctx: &MaterializeContext, cx: &mut App);

    /// Build the row element. Must not create entities, lock the transcript,
    /// or mutate view state.
    fn render(&self, ctx: &RowRenderContext, window: &mut Window, cx: &mut App) -> AnyElement;

    /// The text this row offers the transcript-level copy affordance.
    fn copy_source(&self, transcript: &Transcript) -> Option<SharedString> {
        let _ = transcript;
        None
    }

    /// Disclosure state to keep in the projection across release.
    fn disclosure(&self) -> DisclosureState {
        DisclosureState::default()
    }

    /// Restore a disclosure state captured by the projection.
    fn sync_disclosure(&mut self, disclosure: DisclosureState) {
        let _ = disclosure;
    }

    /// Toggle foldable content. `target` selects which surface of the row
    /// toggles; renderers without that surface ignore the call.
    fn toggle_disclosure(&mut self, target: DisclosureTarget, cx: &mut App) {
        let _ = (target, cx);
    }

    /// Viewport replay state for renderers with their own scrollable
    /// [`TextView`](crate::ui::markdown) (reasoning preview / budgeted
    /// viewport). The view owns the easing constants, the window-activation
    /// check, and the nested scroll boundary; the renderer owns the follow
    /// flag and the queued distance.
    fn nested_scroll_replay(&mut self) -> Option<NestedScrollReplay<'_>> {
        None
    }

    /// Whether the row's current form renders its body through the fork's
    /// windowed block layout (P4 PRD R5). A windowed body keeps unpainted
    /// blocks on estimated heights and the fork exposes no convergence
    /// signal, so while this is true the view must treat the row's outer
    /// measurement as unsettleable (`Confidence::Measured`): it may still
    /// move as more blocks paint, and must not serve as a cold-restore
    /// placeholder.
    fn is_windowed(&self, _cx: &App) -> bool {
        false
    }

    /// Test access to the concrete renderer.
    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any;

    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// Static dispatch from a row kind to its wrapper renderer. The `match` must
/// stay exhaustive over [`RowKind`].
pub(crate) fn renderer_for(kind: RowKind) -> Box<dyn RowRenderer> {
    match kind {
        RowKind::UserBubble => Box::new(UserBubbleRenderer::new()),
        RowKind::AssistantProse => Box::new(ProseRenderer::new()),
        RowKind::Reasoning => Box::new(ReasoningRenderer::new()),
        RowKind::ToolActivity => Box::new(ToolActivityRenderer::new()),
        RowKind::ToolActivityGroup => Box::new(ToolActivityGroupRenderer::new()),
        RowKind::TurnError => Box::new(TurnErrorRenderer::new()),
        RowKind::TurnActions => Box::new(TurnActionsRenderer::new()),
    }
}

/// Everything the view needs to replay one nested wheel gesture into a row's
/// own scrollable viewport. `scroll` is a handle onto the renderer's
/// [`ListState`](gpui::ListState) (a clone shares the retained state); the
/// `follow` flag and the queued smooth distance stay owned by the renderer so
/// release/restore keeps them with the projection.
pub(crate) struct NestedScrollReplay<'a> {
    pub(crate) scroll: ListState,
    pub(crate) follow: &'a mut bool,
    pub(crate) smooth: &'a mut SmoothScrollState,
}

impl NestedScrollReplay<'_> {
    /// Whether the viewport sits close enough to its end that a new delta
    /// should re-arm tail following (the transcript's stick semantics).
    pub(crate) fn near_bottom(&self) -> bool {
        self.scroll.max_offset_for_scrollbar().y + self.scroll.scroll_px_offset_for_scrollbar().y
            <= crate::chat::STICK_THRESHOLD
    }
}
