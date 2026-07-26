//! Structured gateway errors and allowlisted upstream diagnostics.
//!
//! Error values may reach the UI and metrics, so this module deliberately keeps
//! raw provider messages, credentials, and opaque replay payloads out of them.

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
        }
    }

    pub fn safe_message(&self) -> &str {
        &self.message
    }
}

impl fmt::Debug for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("request_id", &self.request_id)
            .field("status", &self.status)
            .field("provider_code", &self.provider_code)
            .field("retryable", &self.retryable)
            .field("output_started", &self.output_started)
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
