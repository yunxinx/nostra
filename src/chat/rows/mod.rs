//! Row renderers: one self-contained wrapper per projected row kind.
//!
//! A renderer owns only the content entities it creates itself
//! (`MarkdownBody`, `ReasoningTrace`, error cards). Every round trip back to
//! the view — hover, measurement, disclosure, nested scrolling, clipboard —
//! goes through [`RowAction`] and [`RowActionDispatch`], which holds a *weak*
//! handle. No renderer struct may contain an `Entity<ChatView>` or an
//! `Entity<Transcript>`; materialization happens only in update-phase
//! (`sync_window` / deferred handlers), never inside a render closure.

mod prose;
mod reasoning;
mod tool_activity;
mod turn_actions;
mod turn_error;
mod user_bubble;

pub(crate) use self::prose::ProseRenderer;
pub(crate) use self::reasoning::ReasoningRenderer;
pub(crate) use self::tool_activity::ToolActivityRenderer;
pub(crate) use self::turn_actions::TurnActionsRenderer;
pub(crate) use self::turn_error::TurnErrorRenderer;
pub(crate) use self::user_bubble::UserBubbleRenderer;

use gpui::{AnyElement, App, Pixels, Point, SharedString, Window};

use crate::chat::projection::{DisclosureState, RowId, RowKind};
use crate::chat::transcript::{Part, PartSource, Role, Transcript, TurnId};
use crate::llm::{GatewayError, ToolResult};
use crate::ui::markdown::MarkdownPresentation;

use super::{ChatView, ReasoningTrace};

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
    /// Member count for a collapsed tool-activity group header.
    pub(crate) group_count: usize,
    /// The expanded tool-activity group this row belongs to. Declared on
    /// every member row while the group is expanded.
    pub(crate) group: Option<RowId>,
    /// Whether this row leads its expanded group: it renders the collapse
    /// affordance, sending [`RowAction::ToggleDisclosure`] for `group`.
    pub(crate) group_leader: bool,
    pub(crate) dispatch: RowActionDispatch,
}

/// Actions a renderer can send back to the owning view.
#[derive(Debug)]
pub(crate) enum RowAction {
    /// Toggle a foldable row (reasoning card / activity group).
    ToggleDisclosure { row_id: RowId },
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

    /// Toggle foldable content; `Some(relative_position)` must be applied to
    /// a virtualized viewport after the next layout.
    fn toggle_disclosure(&mut self, cx: &mut App) -> Option<f32> {
        let _ = cx;
        None
    }

    /// Viewport state for renderers that scroll independently (reasoning
    /// cards). The view drives easing through it.
    fn nested_scroll_trace(&mut self) -> Option<&mut ReasoningTrace> {
        None
    }

    /// Test access to the concrete renderer.
    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any;

    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// Static dispatch from a row kind to its wrapper renderer. The `match` must
/// stay exhaustive over [`RowKind`] (AC7).
pub(crate) fn renderer_for(kind: RowKind) -> Box<dyn RowRenderer> {
    match kind {
        RowKind::UserBubble => Box::new(UserBubbleRenderer::new()),
        RowKind::AssistantProse => Box::new(ProseRenderer::new()),
        RowKind::Reasoning => Box::new(ReasoningRenderer::new()),
        RowKind::ToolActivity => Box::new(ToolActivityRenderer::new()),
        RowKind::ToolActivityGroup => Box::new(ToolActivityRenderer::new_group()),
        RowKind::TurnError => Box::new(TurnErrorRenderer::new()),
        RowKind::TurnActions => Box::new(TurnActionsRenderer::new()),
    }
}
