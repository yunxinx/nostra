//! Structured gateway errors and captured upstream response text.
//!
//! Errors carry two tiers of provider detail with different exposure rules:
//!
//! - `message` / `provider_code` are *safe*: a fixed internal string plus an
//!   allowlisted token. They may reach metrics, logs, and `Debug` freely.
//! - `upstream_body` is the upstream response text captured by the transport or
//!   protocol adapter, kept so the UI can show what the provider actually said
//!   instead of a bare status code. It is deliberately excluded from `Debug`
//!   (see the impl below) and stripped before an outcome is recorded in metrics,
//!   so the response text is retained only by the failed turn's view. Lifecycle
//!   observers receive the live outcome unchanged so the GPUI bridge can render
//!   it; observer implementations must not persist the field (see
//!   `OutcomeObserver`). The body is deliberately not redacted.

use std::fmt;

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Configuration,
    Transport,
    Http,
    Protocol,
    Provider,
}

#[derive(Clone, PartialEq, Eq)]
pub struct GatewayError {
    pub kind: ErrorKind,
    pub message: String,
    pub request_id: Option<String>,
    pub status: Option<u16>,
    pub provider_code: Option<String>,
    pub retryable: bool,
    pub output_started: bool,
    /// Captured upstream response text that explains the failure: a bounded HTTP
    /// error body or the SSE frame carrying the provider's `error` object.
    /// `None` when the failure originated locally or the captured body was empty.
    ///
    /// Not redacted: this is the diagnostic the user asked to see. Never put it
    /// in a log line, a metrics record, or a `Debug` rendering — display it in
    /// the UI, and nowhere else.
    upstream_body: Option<String>,
}

impl GatewayError {
    pub fn configuration(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Configuration,
            message: message.into(),
            request_id: None,
            status: None,
            provider_code: None,
            retryable: false,
            output_started: false,
            upstream_body: None,
        }
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Protocol,
            message: message.into(),
            request_id: None,
            status: None,
            provider_code: None,
            retryable: false,
            output_started: false,
            upstream_body: None,
        }
    }

    pub fn provider(message: impl Into<String>, code: Option<String>) -> Self {
        Self {
            kind: ErrorKind::Provider,
            message: message.into(),
            request_id: None,
            status: None,
            provider_code: code,
            retryable: false,
            output_started: false,
            upstream_body: None,
        }
    }

    /// Build an HTTP failure from safe structured fields. Captured response text
    /// is attached separately through [`Self::with_upstream_body`], keeping the
    /// unredacted UI-only payload behind this module's boundary.
    pub(crate) fn http(status: u16, provider_code: Option<String>) -> Self {
        Self {
            kind: ErrorKind::Http,
            message: format!("Provider returned HTTP {status}."),
            request_id: None,
            status: Some(status),
            provider_code,
            retryable: status == 429 || status >= 500,
            output_started: false,
            upstream_body: None,
        }
    }

    /// Build a local transport/protocol failure with no captured upstream body.
    pub(crate) fn external(
        kind: ErrorKind,
        message: impl Into<String>,
        status: Option<u16>,
        retryable: bool,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            request_id: None,
            status,
            provider_code: None,
            retryable,
            output_started: false,
            upstream_body: None,
        }
    }

    /// Attach captured upstream response text without rewriting its contents.
    /// Empty or whitespace-only input leaves the error without a body.
    pub(crate) fn with_upstream_body(mut self, body: impl Into<String>) -> Self {
        let body = body.into();
        self.upstream_body = (!body.trim().is_empty()).then_some(body);
        self
    }

    pub fn safe_message(&self) -> &str {
        &self.message
    }

    /// The captured upstream response text, if the provider sent any.
    pub fn upstream_body(&self) -> Option<&str> {
        self.upstream_body.as_deref()
    }

    /// Move the captured response into the failed turn's UI state. The contents
    /// remain byte-for-byte unchanged; taking ownership only avoids retaining a
    /// second large allocation in the terminal outcome.
    pub(crate) fn take_upstream_body(&mut self) -> Option<String> {
        self.upstream_body.take()
    }

    /// Clone the safe observability tier while deliberately excluding the raw
    /// provider response. Storage boundaries use this instead of cloning and
    /// then clearing sensitive, potentially large text.
    pub(crate) fn storage_safe_clone(&self) -> Self {
        Self {
            kind: self.kind,
            message: self.message.clone(),
            request_id: self.request_id.clone(),
            status: self.status,
            provider_code: self.provider_code.clone(),
            retryable: self.retryable,
            output_started: self.output_started,
            upstream_body: None,
        }
    }
}

impl fmt::Debug for GatewayError {
    /// Renders the safe fields only. `upstream_body` is reported as a byte count
    /// because `Debug` output is what ends up in logs and panic messages, and a
    /// raw provider body can echo the prompt or an `Authorization` header. The
    /// UI reads the body through [`GatewayError::upstream_body`] instead.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("request_id", &self.request_id)
            .field("status", &self.status)
            .field("provider_code", &self.provider_code)
            .field("retryable", &self.retryable)
            .field("output_started", &self.output_started)
            .field(
                "upstream_body",
                &self.upstream_body.as_ref().map_or_else(
                    || "None".to_string(),
                    |body| format!("[{} bytes redacted]", body.len()),
                ),
            )
            .finish()
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GatewayError {}

pub(crate) fn allowlisted_provider_token(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')))
    .then(|| value.to_string())
}

pub(crate) fn allowlisted_provider_code(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .and_then(allowlisted_provider_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_upstream_body_is_not_rewritten() {
        let error = GatewayError::provider("failed", None);
        assert_eq!(error.clone().with_upstream_body("").upstream_body(), None);
        assert_eq!(
            error.clone().with_upstream_body("  \n\t ").upstream_body(),
            None,
            "a whitespace-only response has nothing useful to render"
        );
        assert_eq!(
            error.with_upstream_body("  {\"a\":1}\n").upstream_body(),
            Some("  {\"a\":1}\n"),
            "captured whitespace is part of the response"
        );
    }

    #[test]
    fn debug_reports_the_body_size_without_its_contents() {
        let error = GatewayError::provider("failed", None)
            .with_upstream_body(r#"{"key":"sk-secret-value"}"#);
        let rendered = format!("{error:?}");
        assert!(!rendered.contains("sk-secret-value"), "{rendered}");
        assert!(rendered.contains("25 bytes redacted"), "{rendered}");
    }
}
