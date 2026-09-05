//! Wrapper renderer for the turn failure card.
//!
//! [`TurnError`] owns a `TextViewState` entity, so it is built here at
//! materialize time from the turn's captured [`GatewayError`] — the same
//! update-phase construction rule as P1.

use gpui::{AnyElement, App, IntoElement, Window};

use crate::chat::error_card;
use crate::chat::projection::RowKind;

use super::{MaterializeContext, RowChange, RowRenderContext, RowRenderer};

pub(crate) struct TurnErrorRenderer {
    error: Option<error_card::TurnError>,
    materialized: bool,
}

impl TurnErrorRenderer {
    pub(crate) fn new() -> Self {
        Self {
            error: None,
            materialized: false,
        }
    }
}

impl TurnErrorRenderer {
    #[cfg(test)]
    pub(crate) fn error_for_test(&self) -> Option<&crate::chat::error_card::TurnError> {
        self.error.as_ref()
    }
}

impl RowRenderer for TurnErrorRenderer {
    fn kind(&self) -> RowKind {
        RowKind::TurnError
    }

    fn materialize(&mut self, ctx: &MaterializeContext, cx: &mut App) {
        self.error = ctx
            .error
            .map(|error| error_card::TurnError::new(error.clone(), cx));
        self.materialized = true;
    }

    fn release(&mut self, _cx: &mut App) {
        self.error = None;
        self.materialized = false;
    }

    fn is_materialized(&self) -> bool {
        self.materialized
    }

    fn apply(&mut self, change: &RowChange, ctx: &MaterializeContext, cx: &mut App) {
        if let RowChange::Replace = change {
            // P1 rebuilt the card on turn replacement; the keyed collapse
            // state lives in the window so it survives the rebuild.
            self.error = ctx
                .error
                .map(|error| error_card::TurnError::new(error.clone(), cx));
            self.materialized = true;
        }
    }

    fn render(&self, ctx: &RowRenderContext, window: &mut Window, cx: &mut App) -> AnyElement {
        match self.error.as_ref() {
            Some(error) => error_card::render(error, ctx.row_id.turn.as_u64(), window, cx),
            None => gpui::div().into_any_element(),
        }
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
