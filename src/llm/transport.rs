//! HTTP transport, bounded SSE decoding, cancellation, and conservative retry.
//!
//! The transport knows credentials and bytes but not canonical messages. A
//! request may retry once only before any SSE data escapes to the protocol layer.

use std::{sync::Arc, time::Duration};

use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request, Response, http::HeaderMap};

use super::{ErrorKind, GatewayError, SecretString, error::allowlisted_provider_token};

pub const DEFAULT_MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;
const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(250);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

pub(crate) struct TransportRequest {
    pub url: String,
    pub api_key: SecretString,
    pub body: Vec<u8>,
}

pub(crate) enum TransportEvent {
    Attempt(u32),
    UpstreamResponse(u16),
    SseData(String),
}

#[derive(Clone)]
pub struct HttpTransport {
    client: Arc<dyn HttpClient>,
}

impl HttpTransport {
    pub fn new(client: Arc<dyn HttpClient>) -> Self {
        Self { client }
    }

    pub(crate) async fn stream(
        &self,
        request: &TransportRequest,
        mut on_event: impl FnMut(TransportEvent) -> bool,
    ) -> Result<(), GatewayError> {
        // `emitted_data` is the retry boundary: once the adapter could have
        // observed a frame, replaying the HTTP request would duplicate output.
        'attempts: for attempt in 1..=2 {
            if !on_event(TransportEvent::Attempt(attempt)) {
                return Ok(());
            }
            let mut response = match self.send(request).await {
                Ok(response) => response,
                Err(_) if attempt == 1 => {
                    async_io::Timer::after(DEFAULT_RETRY_DELAY).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let status = response.status().as_u16();
            if !on_event(TransportEvent::UpstreamResponse(status)) {
                return Ok(());
            }
            if !response.status().is_success() {
                let retry_delay = retry_delay(response.headers());
                let error = read_http_error(&mut response).await;
                if attempt == 1 && error.retryable {
                    async_io::Timer::after(retry_delay.unwrap_or(DEFAULT_RETRY_DELAY)).await;
                    continue;
                }
                return Err(error);
            }

            let mut decoder = SseDecoder::new(DEFAULT_MAX_SSE_EVENT_BYTES);
            let mut emitted_data = false;
            let mut buffer = [0_u8; 8192];
            loop {
                match response.body_mut().read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(read) => {
                        for frame in decoder.push(&buffer[..read])? {
                            emitted_data = true;
                            if !on_event(TransportEvent::SseData(frame)) {
                                return Ok(());
                            }
                        }
                    }
                    Err(_) if attempt == 1 && !emitted_data => {
                        async_io::Timer::after(DEFAULT_RETRY_DELAY).await;
                        continue 'attempts;
                    }
                    Err(_) => {
                        return Err(external_error(
                            ErrorKind::Transport,
                            "Provider stream was interrupted.",
                            None,
                            true,
                        ));
                    }
                }
            }
            if attempt == 1 && !emitted_data {
                // Empty EOF and a partial first event are safe to retry because no data escaped.
                async_io::Timer::after(DEFAULT_RETRY_DELAY).await;
                continue;
            }
            for frame in decoder.finish()? {
                if !on_event(TransportEvent::SseData(frame)) {
                    return Ok(());
                }
            }
            return Ok(());
        }
        Err(external_error(
            ErrorKind::Transport,
            "Unable to connect to the provider.",
            None,
            true,
        ))
    }

    async fn send(&self, request: &TransportRequest) -> Result<Response<AsyncBody>, GatewayError> {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(&request.url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream");
        if !request.api_key.is_empty() {
            builder = builder.header(
                "Authorization",
                format!("Bearer {}", request.api_key.expose()),
            );
        }
        let request = builder
            .body(AsyncBody::from(request.body.clone()))
            .map_err(|_| GatewayError::protocol("invalid provider request"))?;
        self.client.send(request).await.map_err(|_| {
            external_error(
                ErrorKind::Transport,
                "Unable to connect to the provider.",
                None,
                true,
            )
        })
    }
}

async fn read_http_error(response: &mut Response<AsyncBody>) -> GatewayError {
    let status = response.status().as_u16();
    let mut body = Vec::new();
    let _ = response
        .body_mut()
        .take(MAX_ERROR_BODY_BYTES)
        .read_to_end(&mut body)
        .await;
    // Two separate extractions from the same bytes: an allowlisted code for
    // metrics and logs, and the captured response text for the UI. The body is
    // kept because a bare status code does not tell the user which quota they
    // hit, which field was rejected, or which key was refused.
    let provider_code = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/code")
                .or_else(|| value.get("code"))
                .and_then(serde_json::Value::as_str)
                .and_then(allowlisted_provider_token)
        });
    let error = GatewayError::http(status, provider_code);
    // Lossy decode: a non-UTF-8 error body (a mislabelled proxy page, say) is
    // still worth showing as text rather than degrading to "HTTP 502".
    error.with_upstream_body(String::from_utf8_lossy(&body).into_owned())
}

fn retry_delay(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .map(|delay| delay.min(MAX_RETRY_DELAY))
}

fn external_error(
    kind: ErrorKind,
    message: impl Into<String>,
    status: Option<u16>,
    retryable: bool,
) -> GatewayError {
    // Local failures (connect, decode, stream reset) have no upstream text.
    GatewayError::external(kind, message, status, retryable)
}

#[derive(Debug)]
struct SseDecoder {
    buffer: Vec<u8>,
    max_event_bytes: usize,
}

impl SseDecoder {
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_event_bytes,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, GatewayError> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some((end, delimiter_len)) = find_event_boundary(&self.buffer) {
            if end > self.max_event_bytes {
                return Err(GatewayError::protocol(
                    "SSE event exceeds configured size limit",
                ));
            }
            let frame = self.buffer.drain(..end).collect::<Vec<_>>();
            self.buffer.drain(..delimiter_len);
            if let Some(data) = decode_frame(&frame)? {
                events.push(data);
            }
        }
        if self.buffer.len() > self.max_event_bytes {
            return Err(GatewayError::protocol(
                "SSE event exceeds configured size limit",
            ));
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<String>, GatewayError> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }
        let frame = std::mem::take(&mut self.buffer);
        Ok(decode_frame(&frame)?.into_iter().collect())
    }
}

fn find_event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    for index in 0..bytes.len().saturating_sub(1) {
        if bytes[index..].starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if bytes[index..].starts_with(b"\n\n") {
            return Some((index, 2));
        }
    }
    None
}

fn decode_frame(frame: &[u8]) -> Result<Option<String>, GatewayError> {
    let text = std::str::from_utf8(frame)
        .map_err(|_| GatewayError::protocol("SSE event contains invalid UTF-8"))?;
    let mut data = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') || line.is_empty() {
            continue;
        }
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    Ok((!data.is_empty()).then(|| data.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        io,
        pin::Pin,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };

    use futures::{AsyncRead, future::BoxFuture};
    use http_client::{Url, http::HeaderValue};

    enum ResponseSpec {
        Status {
            status: u16,
            body: &'static str,
            retry_after: Option<&'static str>,
        },
        ErrorAfterData,
    }

    struct TestClient {
        responses: Mutex<VecDeque<ResponseSpec>>,
        calls: AtomicUsize,
    }

    impl TestClient {
        fn new(responses: impl IntoIterator<Item = ResponseSpec>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into_iter().collect()),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl HttpClient for TestClient {
        fn user_agent(&self) -> Option<&HeaderValue> {
            None
        }

        fn proxy(&self) -> Option<&Url> {
            None
        }

        fn send(
            &self,
            _: Request<AsyncBody>,
        ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let response = self
                .responses
                .lock()
                .expect("response queue lock")
                .pop_front()
                .expect("queued response");
            Box::pin(async move {
                match response {
                    ResponseSpec::Status {
                        status,
                        body,
                        retry_after,
                    } => {
                        let mut builder = Response::builder().status(status);
                        if let Some(retry_after) = retry_after {
                            builder = builder.header("Retry-After", retry_after);
                        }
                        Ok(builder.body(AsyncBody::from(body))?)
                    }
                    ResponseSpec::ErrorAfterData => Ok(Response::builder()
                        .status(200)
                        .body(AsyncBody::from_reader(ErrorAfterData { sent: false }))?),
                }
            })
        }
    }

    struct ErrorAfterData {
        sent: bool,
    }

    impl AsyncRead for ErrorAfterData {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            if self.sent {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "test stream reset",
                )));
            }
            self.sent = true;
            let data = b"data: {\"chunk\":true}\n\n";
            buffer[..data.len()].copy_from_slice(data);
            Poll::Ready(Ok(data.len()))
        }
    }

    fn request() -> TransportRequest {
        TransportRequest {
            url: "https://example.com/v1/responses".into(),
            api_key: SecretString::new("secret"),
            body: b"{}".to_vec(),
        }
    }

    #[test]
    fn decodes_fragmented_crlf_comments_and_multiline_data() {
        let mut decoder = SseDecoder::new(128);
        assert!(
            decoder
                .push(b": ping\r\ndata: one\r\ndata: tw")
                .expect("valid")
                .is_empty()
        );
        assert_eq!(decoder.push(b"o\r\n\r\n").expect("valid"), vec!["one\ntwo"]);
    }

    #[test]
    fn rejects_oversized_unterminated_event() {
        let mut decoder = SseDecoder::new(4);
        assert!(decoder.push(b"12345").is_err());
    }

    #[test]
    fn http_error_retains_the_captured_body_but_keeps_it_out_of_debug() {
        let body = r#"{"error":{"code":"bad_request","message":"secret echoed"}}"#;
        let client = TestClient::new([ResponseSpec::Status {
            status: 400,
            body,
            retry_after: None,
        }]);
        let error =
            futures::executor::block_on(HttpTransport::new(client).stream(&request(), |_| true))
                .expect_err("HTTP failure");
        assert_eq!(error.status, Some(400));
        assert_eq!(error.provider_code.as_deref(), Some("bad_request"));
        // The body is the diagnostic the UI shows.
        assert_eq!(error.upstream_body(), Some(body));
        // ...but Debug (i.e. logs) still must not carry it.
        assert!(!format!("{error:?}").contains("secret echoed"));
    }

    /// 403 rather than a 5xx: a retryable status would consume a second attempt,
    /// and this test is about the body, not the retry ladder.
    #[test]
    fn non_json_error_body_survives_as_text() {
        let client = TestClient::new([ResponseSpec::Status {
            status: 403,
            body: "<html><body>403 Forbidden</body></html>",
            retry_after: None,
        }]);
        let error =
            futures::executor::block_on(HttpTransport::new(client).stream(&request(), |_| true))
                .expect_err("HTTP failure");
        assert_eq!(
            error.upstream_body(),
            Some("<html><body>403 Forbidden</body></html>")
        );
        // No JSON `code` to extract, so the safe tier stays empty.
        assert_eq!(error.provider_code, None);
    }

    #[test]
    fn empty_error_body_leaves_no_upstream_text() {
        let client = TestClient::new([ResponseSpec::Status {
            status: 401,
            body: "   \n  ",
            retry_after: None,
        }]);
        let error =
            futures::executor::block_on(HttpTransport::new(client).stream(&request(), |_| true))
                .expect_err("HTTP failure");
        assert_eq!(error.upstream_body(), None);
    }

    #[test]
    fn http_error_body_capture_stops_at_the_configured_byte_limit() {
        let body = "x".repeat(MAX_ERROR_BODY_BYTES as usize + 17);
        let mut response = Response::builder()
            .status(400)
            .body(AsyncBody::from(body))
            .expect("test response");

        let error = futures::executor::block_on(read_http_error(&mut response));
        let captured = error.upstream_body().expect("captured body");
        assert_eq!(captured.len(), MAX_ERROR_BODY_BYTES as usize);
        assert!(captured.bytes().all(|byte| byte == b'x'));
    }

    #[test]
    fn retries_retryable_status_before_first_data() {
        let client = TestClient::new([
            ResponseSpec::Status {
                status: 429,
                body: r#"{"error":{"code":"rate_limit"}}"#,
                retry_after: Some("0"),
            },
            ResponseSpec::Status {
                status: 200,
                body: "data: [DONE]\n\n",
                retry_after: None,
            },
        ]);
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();
        futures::executor::block_on(HttpTransport::new(client.clone()).stream(
            &request(),
            move |event| {
                if let TransportEvent::Attempt(attempt) = event {
                    observed.lock().expect("attempt lock").push(attempt);
                }
                true
            },
        ))
        .expect("retried stream");
        assert_eq!(client.calls.load(Ordering::Relaxed), 2);
        assert_eq!(*attempts.lock().expect("attempt lock"), vec![1, 2]);
    }

    #[test]
    fn never_retries_after_sse_data_was_emitted() {
        let client = TestClient::new([ResponseSpec::ErrorAfterData]);
        let error = futures::executor::block_on(
            HttpTransport::new(client.clone()).stream(&request(), |_| true),
        )
        .expect_err("stream reset");
        assert_eq!(error.kind, ErrorKind::Transport);
        assert_eq!(client.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn consumer_can_cancel_before_request_is_sent() {
        let client = TestClient::new([ResponseSpec::Status {
            status: 200,
            body: "data: [DONE]\n\n",
            retry_after: None,
        }]);
        futures::executor::block_on(
            HttpTransport::new(client.clone()).stream(&request(), |_| false),
        )
        .expect("cancelled cleanly");
        assert_eq!(client.calls.load(Ordering::Relaxed), 0);
    }
}
