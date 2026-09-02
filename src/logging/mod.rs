//! Application diagnostics independent from user data persistence.
//!
//! Session JSONL files and the SQLite catalog are part of Nostra's product
//! data contract. Diagnostics have a different lifetime and recovery policy:
//! they are best-effort, bounded, and safe to discard. This module therefore
//! writes a separate rotating text log under the user's Nostra config
//! directory instead of adding diagnostic rows to the session catalog.

mod writer;

use std::{
    fmt::Display,
    path::PathBuf,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

use chrono::{SecondsFormat, Utc};
use writer::{EnqueueError, LogConfig, LogHandle};

const LOG_FILE_NAME: &str = "nostra.log";
const LOG_QUEUE_CAPACITY: usize = 256;
const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
const LOG_BACKUPS: usize = 2;
const LOG_RECORD_MAX_BYTES: usize = 8 * 1024;
const TRUNCATED_SUFFIX: &str = "… [truncated]";

static LOGGER: OnceLock<Option<LogHandle>> = OnceLock::new();
static MAX_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Warn as u8);
static QUEUE_OVERFLOW_REPORTED: AtomicBool = AtomicBool::new(false);
static LOGGER_DISCONNECTED_REPORTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FallbackNotice {
    QueueFull,
    Disconnected,
}

/// Severity used by the small application-wide diagnostics API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum LogLevel {
    Error,
    Warn,
    Info,
}

impl LogLevel {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
        }
    }
}

/// Start the process-wide diagnostic writer once.
///
/// A failure to create the log directory/file is deliberately non-fatal. The
/// initialization error is reported once on stderr, while later records are
/// dropped so an unavailable sink cannot add repeated synchronous output to a
/// UI or persistence failure path.
pub(crate) fn init() {
    let _ = LOGGER.get_or_init(|| {
        let Some(root) = crate::paths::nostra_config_dir() else {
            eprintln!("nostra logger unavailable: no config directory");
            return None;
        };
        let config = LogConfig {
            path: root.join("logs").join(LOG_FILE_NAME),
            queue_capacity: LOG_QUEUE_CAPACITY,
            max_bytes: LOG_MAX_BYTES,
            backups: LOG_BACKUPS,
        };
        match LogHandle::start(config) {
            Ok(handle) => Some(handle),
            Err(error) => {
                eprintln!("nostra logger unavailable: {error}");
                None
            }
        }
    });
}

/// Flush and stop the process-wide writer with a bounded wait at shutdown.
pub(crate) fn shutdown() {
    if let Some(Some(logger)) = LOGGER.get() {
        logger.shutdown();
    }
}

/// Select the only user-facing verbosity choice.
///
/// The safe default records warnings and errors. Enabling detailed
/// diagnostics adds sparse lifecycle information; it still never enables
/// request/response payload capture or per-token/per-message tracing.
pub(crate) fn set_detailed(enabled: bool) {
    MAX_LEVEL.store(configured_level(enabled) as u8, Ordering::Release);
}

const fn configured_level(detailed: bool) -> LogLevel {
    if detailed {
        LogLevel::Info
    } else {
        LogLevel::Warn
    }
}

/// Record a diagnostic event without making the caller perform file I/O.
///
/// Messages are flattened and bounded before entering the queue. Callers
/// should pass operation context and redacted errors, never prompts, API keys,
/// or raw provider response bodies.
pub(crate) fn record(level: LogLevel, component: &'static str, message: impl Display) {
    if level as u8 > MAX_LEVEL.load(Ordering::Acquire) {
        return;
    }
    let line = format!("{} {component}: {message}", level.as_str());
    let Some(Some(logger)) = LOGGER.get() else {
        return;
    };
    if let Some(notice) = fallback_notice(
        logger.try_record(line, level == LogLevel::Error),
        &QUEUE_OVERFLOW_REPORTED,
        &LOGGER_DISCONNECTED_REPORTED,
    ) {
        match notice {
            FallbackNotice::QueueFull => {
                eprintln!("nostra logger queue full; dropping diagnostic events");
            }
            FallbackNotice::Disconnected => {
                eprintln!("nostra logger stopped; dropping diagnostic events");
            }
        }
    }
}

fn fallback_notice(
    result: Result<(), EnqueueError>,
    overflow_reported: &AtomicBool,
    disconnected_reported: &AtomicBool,
) -> Option<FallbackNotice> {
    match result {
        Ok(()) => None,
        // Diagnostics must never back-pressure a UI or persistence path. A
        // process emits each fallback warning at most once; a single freed
        // queue slot is not evidence that sustained pressure has ended.
        Err(EnqueueError::Full) if !overflow_reported.swap(true, Ordering::AcqRel) => {
            Some(FallbackNotice::QueueFull)
        }
        Err(EnqueueError::Disconnected) if !disconnected_reported.swap(true, Ordering::AcqRel) => {
            Some(FallbackNotice::Disconnected)
        }
        Err(EnqueueError::Full | EnqueueError::Disconnected) => None,
    }
}

fn encode_record(line: String) -> String {
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let prefix = format!("{timestamp} ");
    let body_budget = LOG_RECORD_MAX_BYTES.saturating_sub(prefix.len() + 1);
    let body = bounded_text(sanitize(&line), body_budget);
    let mut record = String::with_capacity(prefix.len() + body.len() + 1);
    record.push_str(&prefix);
    record.push_str(&body);
    record.push('\n');
    record
}

fn sanitize(line: &str) -> String {
    line.replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

fn bounded_text(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes.saturating_sub(TRUNCATED_SUFFIX.len());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.truncate(end);
    text.push_str(TRUNCATED_SUFFIX);
    text
}

pub(crate) fn info(component: &'static str, message: impl Display) {
    record(LogLevel::Info, component, message);
}

pub(crate) fn warn(component: &'static str, message: impl Display) {
    record(LogLevel::Warn, component, message);
}

pub(crate) fn error(component: &'static str, message: impl Display) {
    record(LogLevel::Error, component, message);
}

/// Candidate log files, oldest backup first, then the active file.
pub(crate) fn log_file_paths() -> Option<Vec<PathBuf>> {
    let active = crate::paths::nostra_config_dir()?
        .join("logs")
        .join(LOG_FILE_NAME);
    let mut paths: Vec<PathBuf> = (1..=LOG_BACKUPS)
        .rev()
        .map(|index| writer::backup_path(&active, index))
        .collect();
    paths.push(active);
    Some(paths)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedLogLine {
    pub timestamp: Option<String>,
    pub level: Option<LogLevel>,
    pub rest: String,
}

pub(crate) fn parse_log_line(line: &str) -> ParsedLogLine {
    let line = line.trim_end_matches(['\n', '\r']);
    let Some((timestamp, after_ts)) = line.split_once(' ') else {
        return ParsedLogLine {
            timestamp: None,
            level: None,
            rest: line.to_string(),
        };
    };
    if !timestamp.contains('T') {
        return ParsedLogLine {
            timestamp: None,
            level: None,
            rest: line.to_string(),
        };
    }
    let Some((level_token, rest)) = after_ts.split_once(' ') else {
        return ParsedLogLine {
            timestamp: Some(timestamp.to_string()),
            level: None,
            rest: after_ts.to_string(),
        };
    };
    let level = match level_token {
        "ERROR" => Some(LogLevel::Error),
        "WARN" => Some(LogLevel::Warn),
        "INFO" => Some(LogLevel::Info),
        _ => None,
    };
    if level.is_none() {
        return ParsedLogLine {
            timestamp: Some(timestamp.to_string()),
            level: None,
            rest: after_ts.to_string(),
        };
    }
    ParsedLogLine {
        timestamp: Some(timestamp.to_string()),
        level,
        rest: rest.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn level_names_are_stable() {
        assert_eq!(LogLevel::Error.as_str(), "ERROR");
        assert_eq!(LogLevel::Warn.as_str(), "WARN");
        assert_eq!(LogLevel::Info.as_str(), "INFO");
    }

    #[test]
    fn parse_log_line_reads_the_current_record_shape() {
        let record = encode_record("ERROR providers: boom".into());
        let parsed = parse_log_line(record.trim_end());
        assert!(
            parsed
                .timestamp
                .as_ref()
                .is_some_and(|timestamp| timestamp.contains('T')),
            "{parsed:?}"
        );
        assert_eq!(parsed.level, Some(LogLevel::Error));
        assert_eq!(parsed.rest, "providers: boom");
    }

    #[test]
    fn parse_log_line_keeps_malformed_rows() {
        let parsed = parse_log_line("not a log line");
        assert_eq!(
            parsed,
            ParsedLogLine {
                timestamp: None,
                level: None,
                rest: "not a log line".into(),
            }
        );
    }

    #[test]
    fn safe_default_is_warning_and_detailed_mode_adds_info() {
        assert_eq!(configured_level(false), LogLevel::Warn);
        assert_eq!(configured_level(true), LogLevel::Info);
        assert!(LogLevel::Error <= configured_level(false));
        assert!(LogLevel::Info > configured_level(false));
    }

    #[test]
    fn successful_enqueue_does_not_rearm_the_overflow_notice() {
        let overflow_reported = AtomicBool::new(false);
        let disconnected_reported = AtomicBool::new(false);

        assert_eq!(
            fallback_notice(
                Err(EnqueueError::Full),
                &overflow_reported,
                &disconnected_reported,
            ),
            Some(FallbackNotice::QueueFull)
        );
        assert_eq!(
            fallback_notice(Ok(()), &overflow_reported, &disconnected_reported),
            None
        );
        assert_eq!(
            fallback_notice(
                Err(EnqueueError::Full),
                &overflow_reported,
                &disconnected_reported,
            ),
            None,
            "one transient success must not rearm stderr flooding under sustained pressure"
        );
    }

    #[test]
    fn unavailable_logger_reports_initialization_once_without_replaying_records_to_stderr() {
        const CHILD_MARKER: &str = "NOSTRA_LOGGER_UNAVAILABLE_CHILD";
        const TEST_NAME: &str = "logging::tests::unavailable_logger_reports_initialization_once_without_replaying_records_to_stderr";

        if std::env::var_os(CHILD_MARKER).is_some() {
            init();
            error("logging.test", "first unavailable record");
            error("logging.test", "second unavailable record");
            return;
        }

        let directory = tempdir().expect("temporary logger failure root");
        let blocked_config_root = directory.path().join("config-file");
        fs::write(&blocked_config_root, b"not a directory").expect("create path blocker");
        let output = Command::new(std::env::current_exe().expect("current test binary"))
            .arg("--exact")
            .arg(TEST_NAME)
            .arg("--nocapture")
            .env(CHILD_MARKER, "1")
            .env("XDG_CONFIG_HOME", &blocked_config_root)
            .output()
            .expect("run isolated logger failure probe");

        assert!(
            output.status.success(),
            "isolated logger failure probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).expect("logger stderr is UTF-8");
        assert_eq!(
            stderr.matches("nostra logger unavailable:").count(),
            1,
            "logger initialization should report its unavailable sink exactly once: {stderr}"
        );
        assert!(!stderr.contains("first unavailable record"));
        assert!(!stderr.contains("second unavailable record"));
    }

    #[test]
    fn records_are_bounded_without_splitting_utf8() {
        let record = encode_record("界".repeat(LOG_RECORD_MAX_BYTES));
        assert!(record.len() <= LOG_RECORD_MAX_BYTES);
        assert!(record.trim_end().ends_with(TRUNCATED_SUFFIX));
        assert!(std::str::from_utf8(record.as_bytes()).is_ok());
    }

    #[test]
    fn physical_records_stay_within_the_byte_budget_after_escaping() {
        let directory = tempdir().expect("temporary log directory");
        let path = directory.path().join("nostra.log");
        let logger = LogHandle::start(LogConfig {
            path: path.clone(),
            queue_capacity: 8,
            max_bytes: 64 * 1024,
            backups: 2,
        })
        .expect("start logger");

        let line = format!("WARN test: {}", "\\".repeat(LOG_RECORD_MAX_BYTES));
        logger
            .try_record(line, true)
            .expect("enqueue escaped record");
        logger.shutdown();

        let bytes = fs::read(path).expect("read physical log record");
        assert!(
            bytes.len() <= LOG_RECORD_MAX_BYTES,
            "one physical record must stay within the configured 8 KiB budget"
        );
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    }

    #[test]
    fn writer_persists_records_and_flattens_multiline_messages() {
        let directory = tempdir().expect("temporary log directory");
        let path = directory.path().join("logs").join("nostra.log");
        let logger = LogHandle::start(LogConfig {
            path: path.clone(),
            queue_capacity: 8,
            max_bytes: 1024,
            backups: 2,
        })
        .expect("start logger");

        logger
            .try_record(
                "ERROR persistence: first line\nsecond line\r".to_string(),
                true,
            )
            .expect("enqueue log");
        logger.shutdown();

        let contents = fs::read_to_string(path).expect("read log");
        assert!(contents.contains("ERROR persistence: first line\\nsecond line\\r"));
        assert!(contents.ends_with('\n'));
    }

    #[test]
    fn writer_rotates_before_the_next_oversized_record() {
        let directory = tempdir().expect("temporary log directory");
        let path = directory.path().join("nostra.log");
        let logger = LogHandle::start(LogConfig {
            path: path.clone(),
            queue_capacity: 8,
            max_bytes: 32,
            backups: 2,
        })
        .expect("start logger");

        logger
            .try_record("INFO test: 1234567890".to_string(), false)
            .expect("enqueue first");
        logger
            .try_record("INFO test: abcdefghij".to_string(), false)
            .expect("enqueue second");
        logger.shutdown();

        let main = fs::read_to_string(&path).expect("read current log");
        let backup = fs::read_to_string(path.with_file_name("nostra.log.1"));
        assert!(backup.is_ok(), "rotation should create a first backup");
        assert!(main.contains("abcdefghij") || backup.unwrap().contains("abcdefghij"));
    }
}
