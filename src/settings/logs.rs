//! Diagnostic log viewer for the settings window.
//!
//! File I/O and line parsing run off the UI thread. Render formats the stored
//! snapshot for the current UI language; a generation counter drops stale
//! background results. Editor `set_value` is skipped when the raw fingerprint
//! and language are unchanged so selection and scroll survive the refresh.

use std::{path::PathBuf, sync::Arc, time::Duration};

use chrono::{DateTime, Datelike as _, Local, Timelike as _};
use gpui::{
    AnyElement, AppContext as _, Context, Edges, Entity, HighlightStyle, Hsla, IntoElement,
    ParentElement as _, Pixels, Point, Render, Styled as _, Task, Window, div, point, px, relative,
};
use gpui_component::{
    ActiveTheme as _,
    input::{EditorState, TextDecoration, TextDecorationCollection},
    v_flex,
};
use rust_i18n::t;

use crate::logging::{self, LogLevel, ParsedLogLine};
use crate::preferences::Language;

const MAX_DISPLAY_LINES: usize = 4_000;
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const SCROLL_TO_END_Y: Pixels = px(-1_000_000.);
const BOTTOM_EPSILON: Pixels = px(8.);
/// Inset inside the bordered viewer. The styled `Editor` wrapper would overwrite
/// this with Medium input padding every paint, so the page renders `EditorState`
/// directly and keeps this value.
const VIEWER_PAD: Pixels = px(4.);
const VIEWER_PADDINGS: Edges<Pixels> = Edges {
    top: VIEWER_PAD,
    right: VIEWER_PAD,
    bottom: VIEWER_PAD,
    left: VIEWER_PAD,
};

#[derive(Clone)]
enum LogViewState {
    Loading,
    Unavailable,
    Empty,
    Failed,
    Ready {
        rows: Arc<Vec<SnapshotRow>>,
        raw: String,
    },
}

#[derive(Clone)]
struct SnapshotRow {
    timestamp: Option<String>,
    level: Option<LogLevel>,
    rest: String,
}

#[derive(Clone, Copy, PartialEq)]
struct LogPalette {
    danger: Hsla,
    warning: Hsla,
    accent: Hsla,
    muted: Hsla,
}

impl LogPalette {
    fn capture(cx: &Context<LogsPage>) -> Self {
        let theme = cx.theme();
        Self {
            danger: theme.danger,
            warning: theme.warning,
            accent: theme.accent,
            muted: theme.muted_foreground,
        }
    }

    fn level_color(self, level: LogLevel) -> Hsla {
        match level {
            LogLevel::Error => self.danger,
            LogLevel::Warn => self.warning,
            LogLevel::Info => self.accent,
        }
    }
}

pub(super) struct LogsPage {
    snapshot: LogViewState,
    generation: u64,
    visible: bool,
    pending_scroll_to_end: bool,
    follow_tail: bool,
    last_bottom_y: Option<Pixels>,
    last_raw: Option<String>,
    last_language: Option<Language>,
    last_palette: Option<LogPalette>,
    editor: Entity<EditorState>,
    decorations: TextDecorationCollection,
    _load_task: Option<Task<()>>,
    _refresh_task: Option<Task<()>>,
}

impl LogsPage {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .line_number(false)
                .folding(false)
                .indent_guides(false)
                .soft_wrap(false)
                .scroll_beyond_last_line(Some(0))
                .cursor_surrounding_lines(Some(0));
            state.set_readonly(true, cx);
            state.set_editor_paddings(VIEWER_PADDINGS);
            state
        });
        let decorations = editor.update(cx, |state, cx| {
            state.create_decorations_collection(Vec::new(), cx)
        });
        Self {
            snapshot: LogViewState::Loading,
            generation: 0,
            visible: false,
            pending_scroll_to_end: false,
            follow_tail: true,
            last_bottom_y: None,
            last_raw: None,
            last_language: None,
            last_palette: None,
            editor,
            decorations,
            _load_task: None,
            _refresh_task: None,
        }
    }

    pub(super) fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.pending_scroll_to_end = true;
            self.follow_tail = true;
            self.queue_load(cx);
            self.start_refresh(cx);
        } else {
            self._load_task = None;
            self._refresh_task = None;
        }
        cx.notify();
    }

    fn start_refresh(&mut self, cx: &mut Context<Self>) {
        self._refresh_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                if this
                    .update(cx, |this, cx| {
                        if this.visible {
                            this.queue_load(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn queue_load(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        let paths = logging::log_file_paths();
        let background = cx.background_spawn(async move { snapshot_from_files(paths) });
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let snapshot = background.await;
            this.update(cx, |this, cx| this.apply(generation, snapshot, cx))
                .ok();
        }));
    }

    fn apply(&mut self, generation: u64, snapshot: LogViewState, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        if matches!(self.snapshot, LogViewState::Ready { .. }) {
            let offset = self.editor.read(cx).scroll_offset();
            self.update_follow_from_offset(offset, cx);
        }
        self.snapshot = snapshot;
        cx.notify();
    }

    fn sync_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let language = crate::i18n::current(cx);
        let palette = LogPalette::capture(cx);
        let unchanged = match &self.snapshot {
            LogViewState::Ready { raw, .. } => {
                self.last_raw.as_deref() == Some(raw.as_str())
                    && self.last_language == Some(language)
            }
            _ => return,
        };

        if unchanged {
            if self.pending_scroll_to_end {
                self.scroll_editor_to_end(cx);
                self.pending_scroll_to_end = false;
                self.follow_tail = true;
                self.last_bottom_y = None;
            } else {
                let offset = self.editor.read(cx).scroll_offset();
                self.update_follow_from_offset(offset, cx);
            }
            if self.last_palette != Some(palette) {
                if let LogViewState::Ready { rows, .. } = &self.snapshot {
                    let (_, decorations) = format_log_buffer(rows, language, &palette);
                    self.decorations.set(decorations, cx);
                }
                self.last_palette = Some(palette);
            }
            return;
        }

        let (rows, raw) = match &self.snapshot {
            LogViewState::Ready { rows, raw } => (Arc::clone(rows), raw.clone()),
            _ => return,
        };
        let (buffer, decorations) = format_log_buffer(&rows, language, &palette);
        let pending_end = self.pending_scroll_to_end;
        let follow_tail = self.follow_tail;
        let last_bottom = self.last_bottom_y;
        let scrolled_to_end = self.editor.update(cx, |editor, cx| {
            let previous = editor.scroll_offset();
            let at_bottom = pending_end
                || follow_tail
                || last_bottom.is_some_and(|bottom| previous.y <= bottom + BOTTOM_EPSILON);
            editor.set_value(buffer, window, cx);
            if at_bottom {
                editor.set_scroll_offset(point(previous.x, SCROLL_TO_END_Y), cx);
            } else {
                editor.set_scroll_offset(previous, cx);
            }
            at_bottom
        });
        self.pending_scroll_to_end = false;
        self.follow_tail = scrolled_to_end;
        // Layout clamps the deferred offset on the next paint. Recapture the
        // real bottom then so rotation/shrink cannot keep a stale more-negative Y.
        self.last_bottom_y = None;
        self.decorations.set(decorations, cx);
        self.last_raw = Some(raw);
        self.last_language = Some(language);
        self.last_palette = Some(palette);
    }

    fn scroll_editor_to_end(&mut self, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let x = editor.scroll_offset().x;
            editor.set_scroll_offset(point(x, SCROLL_TO_END_Y), cx);
        });
    }

    fn update_follow_from_offset(&mut self, offset: Point<Pixels>, cx: &Context<Self>) {
        let last_line_visible = self.last_line_is_visible(cx);
        match self.last_bottom_y {
            Some(bottom_y) => {
                self.follow_tail = last_line_visible || offset.y <= bottom_y + BOTTOM_EPSILON;
                if self.follow_tail {
                    self.last_bottom_y = Some(offset.y);
                }
            }
            None => {
                if last_line_visible {
                    self.last_bottom_y = Some(offset.y);
                    self.follow_tail = true;
                } else if self.editor.read(cx).visible_row_range().is_some() {
                    self.follow_tail = false;
                }
            }
        }
    }

    fn last_line_is_visible(&self, cx: &Context<Self>) -> bool {
        let LogViewState::Ready { rows, .. } = &self.snapshot else {
            return false;
        };
        let Some(last) = rows.len().checked_sub(1) else {
            return true;
        };
        self.editor
            .read(cx)
            .visible_row_range()
            .is_some_and(|visible| visible.end > last)
    }
}

impl Render for LogsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_editor(window, cx);

        let inner = match &self.snapshot {
            LogViewState::Loading => status_message(t!("settings.logs.loading").to_string(), cx),
            LogViewState::Unavailable => {
                status_message(t!("settings.logs.unavailable").to_string(), cx)
            }
            LogViewState::Empty => status_message(t!("settings.logs.empty").to_string(), cx),
            LogViewState::Failed => status_message(t!("settings.logs.read_failed").to_string(), cx),
            LogViewState::Ready { .. } => {
                self.editor.update(cx, |state, _| {
                    state.set_editor_paddings(VIEWER_PADDINGS);
                });
                div()
                    .size_full()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(cx.theme().mono_font_size)
                    .line_height(relative(1.5))
                    .child(self.editor.clone())
                    .into_any_element()
            }
        };

        v_flex().size_full().child(
            div().flex_1().min_h_0().p_4().child(
                v_flex()
                    .size_full()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().background)
                    .overflow_hidden()
                    .child(inner),
            ),
        )
    }
}

fn status_message(text: String, cx: &Context<LogsPage>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

fn snapshot_from_files(paths: Option<Vec<PathBuf>>) -> LogViewState {
    let Some(paths) = paths else {
        return LogViewState::Unavailable;
    };
    let mut chunks = Vec::new();
    let mut saw_file = false;
    for path in paths {
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                saw_file = true;
                chunks.push(contents);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return LogViewState::Failed,
        }
    }
    if !saw_file {
        return LogViewState::Empty;
    }
    let raw_lines = last_nonempty_lines(&chunks, MAX_DISPLAY_LINES);
    if raw_lines.is_empty() {
        return LogViewState::Empty;
    }
    let raw = raw_lines.join("\n");
    let rows = raw_lines
        .iter()
        .map(|line| snapshot_row(logging::parse_log_line(line)))
        .collect();
    LogViewState::Ready {
        rows: Arc::new(rows),
        raw,
    }
}

fn snapshot_row(parsed: ParsedLogLine) -> SnapshotRow {
    SnapshotRow {
        timestamp: parsed.timestamp,
        level: parsed.level,
        rest: parsed.rest,
    }
}

fn last_nonempty_lines(chunks: &[String], max: usize) -> Vec<String> {
    let mut lines: Vec<String> = chunks
        .iter()
        .flat_map(|chunk| chunk.lines())
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    let overflow = lines.len().saturating_sub(max);
    if overflow > 0 {
        lines.drain(..overflow);
    }
    lines
}

fn format_log_buffer(
    rows: &[SnapshotRow],
    language: Language,
    palette: &LogPalette,
) -> (String, Vec<TextDecoration>) {
    let mut buffer = String::new();
    let mut decorations = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            buffer.push('\n');
        }
        append_formatted_row(row, language, palette, &mut buffer, &mut decorations);
    }
    (buffer, decorations)
}

fn append_formatted_row(
    row: &SnapshotRow,
    language: Language,
    palette: &LogPalette,
    buffer: &mut String,
    decorations: &mut Vec<TextDecoration>,
) {
    if let Some(token) = &row.timestamp {
        let start = buffer.len();
        match parsed_local_timestamp(token) {
            Some(local) => {
                buffer.push_str(&format_local_timestamp(local, language));
                decorations.push(color_decoration(start..buffer.len(), palette.muted));
            }
            None => buffer.push_str(token),
        }
        buffer.push(' ');
    }

    if let Some(level) = row.level {
        let start = buffer.len();
        buffer.push_str(level.as_str());
        decorations.push(color_decoration(
            start..buffer.len(),
            palette.level_color(level),
        ));
        buffer.push(' ');
        buffer.push_str(&row.rest);
    } else {
        buffer.push_str(&row.rest);
    }
}

fn color_decoration(range: std::ops::Range<usize>, color: Hsla) -> TextDecoration {
    TextDecoration::new(
        range,
        HighlightStyle {
            color: Some(color),
            ..HighlightStyle::default()
        },
    )
}

fn parsed_local_timestamp(token: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(token)
        .ok()
        .map(|parsed| parsed.with_timezone(&Local))
}

#[cfg(test)]
fn display_timestamp(token: &str, language: Language) -> String {
    parsed_local_timestamp(token)
        .map(|local| format_local_timestamp(local, language))
        .unwrap_or_else(|| token.to_string())
}

fn format_local_timestamp(local: DateTime<Local>, language: Language) -> String {
    match language {
        Language::ZhCn => format!(
            "{}年{}月{}日 {:02}:{:02}:{:02}",
            local.year(),
            local.month(),
            local.day(),
            local.hour(),
            local.minute(),
            local.second(),
        ),
        Language::En => {
            let (hour12, meridiem) = clock_12h(local.hour());
            format!(
                "{} {}, {}, {}:{:02}:{:02} {}",
                local.format("%b"),
                local.day(),
                local.year(),
                hour12,
                local.minute(),
                local.second(),
                meridiem,
            )
        }
    }
}

fn clock_12h(hour: u32) -> (u32, &'static str) {
    match hour {
        0 => (12, "AM"),
        1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        _ => (hour - 12, "PM"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    #[test]
    fn last_nonempty_lines_keeps_the_newest_rows() {
        let chunks = ["a\n\nb\n".to_string(), "c\nd\n".to_string()];
        assert_eq!(
            last_nonempty_lines(&chunks, 3),
            vec!["b".to_string(), "c".to_string(), "d".to_string()]
        );
    }

    #[test]
    fn snapshot_from_missing_files_is_empty() {
        let directory = tempfile::tempdir().expect("temp dir");
        let paths = vec![
            directory.path().join("nostra.log.2"),
            directory.path().join("nostra.log.1"),
            directory.path().join("nostra.log"),
        ];
        assert!(matches!(
            snapshot_from_files(Some(paths)),
            LogViewState::Empty
        ));
    }

    #[test]
    fn snapshot_from_files_concatenates_backups_then_active() {
        let directory = tempfile::tempdir().expect("temp dir");
        let older = directory.path().join("nostra.log.1");
        let active = directory.path().join("nostra.log");
        std::fs::write(&older, "old\n").expect("write backup");
        std::fs::write(&active, "new\n").expect("write active");
        match snapshot_from_files(Some(vec![older, active])) {
            LogViewState::Ready { raw, rows } => {
                assert_eq!(raw, "old\nnew");
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].rest, "old");
                assert_eq!(rows[1].rest, "new");
            }
            _ => panic!("expected ready snapshot"),
        }
    }

    #[test]
    fn snapshot_from_unreadable_path_is_failed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let blocked = directory.path().join("nostra.log");
        std::fs::create_dir(&blocked).expect("create directory blocker");
        assert!(matches!(
            snapshot_from_files(Some(vec![blocked])),
            LogViewState::Failed
        ));
    }

    #[test]
    fn snapshot_without_config_directory_is_unavailable() {
        assert!(matches!(
            snapshot_from_files(None),
            LogViewState::Unavailable
        ));
    }

    #[test]
    fn format_local_timestamp_uses_language_patterns() {
        let local = chrono::Local
            .with_ymd_and_hms(2026, 9, 2, 16, 30, 1)
            .single()
            .expect("unique local datetime");
        assert_eq!(
            format_local_timestamp(local, Language::ZhCn),
            "2026年9月2日 16:30:01"
        );
        assert_eq!(
            format_local_timestamp(local, Language::En),
            "Sep 2, 2026, 4:30:01 PM"
        );
    }

    #[test]
    fn unparseable_timestamp_is_kept_as_is() {
        assert_eq!(
            display_timestamp("not-a-rfc3339", Language::ZhCn),
            "not-a-rfc3339"
        );
        assert_eq!(
            display_timestamp("2026-99-99T99:99:99Z", Language::En),
            "2026-99-99T99:99:99Z"
        );
    }

    #[test]
    fn rfc3339_millis_are_dropped_in_display() {
        let formatted = display_timestamp("2026-09-02T16:30:01.123Z", Language::En);
        assert_ne!(formatted, "2026-09-02T16:30:01.123Z");
        assert!(!formatted.contains(".123"));
        assert!(formatted.contains("2026"));
    }

    #[test]
    fn clock_12h_covers_midnight_and_noon() {
        assert_eq!(clock_12h(0), (12, "AM"));
        assert_eq!(clock_12h(11), (11, "AM"));
        assert_eq!(clock_12h(12), (12, "PM"));
        assert_eq!(clock_12h(16), (4, "PM"));
        assert_eq!(clock_12h(23), (11, "PM"));
    }

    fn test_palette() -> LogPalette {
        LogPalette {
            danger: gpui::hsla(0.0, 0.8, 0.5, 1.0),
            warning: gpui::hsla(40.0, 0.8, 0.5, 1.0),
            accent: gpui::hsla(210.0, 0.8, 0.5, 1.0),
            muted: gpui::hsla(0.0, 0.0, 0.5, 1.0),
        }
    }

    #[test]
    fn format_log_buffer_decorates_timestamp_and_level() {
        let token = "2026-09-02T08:30:01.123Z";
        let rows = [SnapshotRow {
            timestamp: Some(token.to_string()),
            level: Some(LogLevel::Error),
            rest: "shell.window: boom".into(),
        }];
        let palette = test_palette();
        let (buffer, decorations) = format_log_buffer(&rows, Language::En, &palette);
        let time = display_timestamp(token, Language::En);
        assert!(buffer.starts_with(&time), "{buffer}");
        assert!(buffer.contains("ERROR shell.window: boom"));
        assert_eq!(decorations.len(), 2);
        assert_eq!(decorations[0].range, 0..time.len());
        assert_eq!(decorations[0].style.color, Some(palette.muted));
        let level_start = time.len() + 1;
        assert_eq!(
            decorations[1].range,
            level_start..level_start + "ERROR".len()
        );
        assert_eq!(decorations[1].style.color, Some(palette.danger));
    }

    #[test]
    fn format_log_buffer_skips_timestamp_color_when_unparseable() {
        let rows = [SnapshotRow {
            timestamp: Some("not-rfc3339".into()),
            level: Some(LogLevel::Warn),
            rest: "x".into(),
        }];
        let palette = test_palette();
        let (buffer, decorations) = format_log_buffer(&rows, Language::ZhCn, &palette);
        assert_eq!(buffer, "not-rfc3339 WARN x");
        assert_eq!(decorations.len(), 1);
        assert_eq!(
            decorations[0].range,
            "not-rfc3339 ".len().."not-rfc3339 WARN".len()
        );
        assert_eq!(decorations[0].style.color, Some(palette.warning));
    }

    #[test]
    fn format_log_buffer_leaves_malformed_rows_undecorated() {
        let rows = [SnapshotRow {
            timestamp: None,
            level: None,
            rest: "not a log line".into(),
        }];
        let (buffer, decorations) = format_log_buffer(&rows, Language::En, &test_palette());
        assert_eq!(buffer, "not a log line");
        assert!(decorations.is_empty());
    }

    #[test]
    fn log_page_labels_resolve_in_every_locale() {
        for locale in ["en", "zh-CN"] {
            for key in [
                "settings.page.logs",
                "settings.logs.loading",
                "settings.logs.empty",
                "settings.logs.unavailable",
                "settings.logs.read_failed",
            ] {
                let resolved = t!(key, locale = locale).to_string();
                assert!(!resolved.contains(key), "{key} unresolved for {locale}");
            }
        }
    }
}
