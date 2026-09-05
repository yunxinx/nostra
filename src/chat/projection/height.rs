//! Row height cache: estimates, measurement keys, and confidence.
//!
//! Heights feed the retained [`ListState`](gpui::ListState) estimates so the
//! scrollbar thumb is sized from the first frame without `measure_all()`.
//! Measured heights are cached per [`MeasurementKey`]; a mismatched key or a
//! stale content revision falls back to the estimate and schedules a
//! remeasure.

use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{Pixels, px};

use super::RowKind;

/// Process-wide theme revision, bumped whenever the active theme changes so
/// cached measurements taken under another theme are invalidated.
static THEME_REVISION: AtomicU64 = AtomicU64::new(0);

/// Bump the theme revision (called by `appearance::theme` on mode or palette
/// changes).
pub(crate) fn note_theme_changed() {
    THEME_REVISION.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn current_theme_revision() -> u64 {
    THEME_REVISION.load(Ordering::Relaxed)
}

impl TypographySnapshot {
    /// Width bucket a measurement/estimate belongs to, in 16 px steps.
    pub(crate) fn width_bucket(width: Pixels) -> u16 {
        (width.as_f32() / WIDTH_BUCKET_PX).round().max(0.) as u16
    }
}

/// Column width bucket (16 px) recorded with every measurement.
pub(crate) const WIDTH_BUCKET_PX: f32 = 16.;

/// Typographic inputs an estimate or measurement was taken against.
///
/// `typography_revision` advances when line height / font size change and
/// `theme_revision` when the active theme changes, so a cached height taken
/// under different typography is rejected instead of silently reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TypographySnapshot {
    pub(crate) line_height: Pixels,
    pub(crate) font_size: Pixels,
    pub(crate) typography_revision: u64,
    pub(crate) theme_revision: u64,
}

impl TypographySnapshot {
    pub(crate) fn measurement_key(&self, width_bucket: u16) -> MeasurementKey {
        MeasurementKey {
            width_bucket,
            typography_revision: self.typography_revision,
            theme_revision: self.theme_revision,
        }
    }
}

/// Identity of the conditions a height was measured under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MeasurementKey {
    pub(crate) width_bucket: u16,
    pub(crate) typography_revision: u64,
    pub(crate) theme_revision: u64,
}

/// How trustworthy a cached measurement is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Confidence {
    /// Measured while the row's content could still change (streaming).
    Measured,
    /// Measured with fully settled content; safe as a cold-restore
    /// first-frame placeholder.
    Settled,
}

/// One recorded measurement plus the conditions it was taken under.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Measured {
    pub(crate) height: Pixels,
    pub(crate) key: MeasurementKey,
    pub(crate) content_revision: u64,
    pub(crate) confidence: Confidence,
}

/// Estimate + optional measurement for one projected row.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowHeight {
    pub(crate) estimated: Pixels,
    pub(crate) measured: Option<Measured>,
}

impl RowHeight {
    #[cfg(test)]
    pub(crate) fn new(estimated: Pixels) -> Self {
        Self {
            estimated,
            measured: None,
        }
    }

    /// The height to use right now: the cached measurement when it still
    /// matches the current key and content revision, the estimate otherwise.
    pub(crate) fn effective(&self, key: &MeasurementKey, content_revision: u64) -> Pixels {
        match self.measured {
            Some(measured)
                if measured.key == *key && measured.content_revision == content_revision =>
            {
                measured.height
            }
            _ => self.estimated,
        }
    }

    /// The key the cached measurement was taken under, if any. Cold restore
    /// derives its hint key from the saved measurements (the current viewport
    /// is not laid out yet and may differ from the cold session's width).
    pub(crate) fn measured_key(&self) -> Option<MeasurementKey> {
        self.measured.as_ref().map(|measured| measured.key)
    }

    /// Whether the cached measurement may serve as a cold-restore first
    /// frame placeholder.
    pub(crate) fn settled_height(&self, key: &MeasurementKey) -> Option<Pixels> {
        self.measured
            .filter(|measured| measured.key == *key && measured.confidence == Confidence::Settled)
            .map(|measured| measured.height)
    }

    pub(crate) fn record(&mut self, measured: Measured) {
        self.measured = Some(measured);
    }

    /// Drop the cached measurement (typography or content changed).
    pub(crate) fn invalidate(&mut self) {
        self.measured = None;
    }

    /// Size estimate for one row, derived from the row kind and the length of
    /// its source text.
    ///
    /// Deliberately conservative rather than exact: the retained list replaces
    /// estimates with real measurements as rows render, and a wrong estimate
    /// only shifts the scrollbar thumb before the row is first drawn.
    pub(crate) fn estimate(
        kind: RowKind,
        source_len: usize,
        block_hint: usize,
        typography: &TypographySnapshot,
    ) -> Pixels {
        let line = typography.line_height;
        let text_lines = |chars: usize, chars_per_line: f32| -> u32 {
            (((chars as f32) / chars_per_line.max(1.)).ceil() as u32).max(1)
        };
        match kind {
            // One row of text inside the bubble padding.
            RowKind::UserBubble => line * text_lines(source_len, 56.) as f32 + px(12.),
            RowKind::AssistantProse => {
                let blocks = block_hint.max(1) as f32;
                line * text_lines(source_len, 62.) as f32 + px(8.) * (blocks - 1.).clamp(0., 20.)
            }
            // Trigger chip plus the seven-line visible budget.
            RowKind::Reasoning => {
                line + px(8.) + line * text_lines(source_len.min(4 * 1024), 62.) as f32
            }
            RowKind::ToolActivity => line + px(6.),
            RowKind::ToolActivityGroup => line + px(6.),
            // Headline plus a couple of preview lines for the fenced body.
            RowKind::TurnError => line * 3. + px(40.),
            RowKind::TurnActions => px(32.),
        }
    }
}
