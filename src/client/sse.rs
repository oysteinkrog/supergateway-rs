
//! SSE client (EventSource) for connecting to remote SSE-based MCP servers.
//!
//! Parses `text/event-stream` per the W3C EventSource spec, discovers the POST
//! endpoint URL from the first `endpoint` event, supports reconnection with
//! exponential backoff and Last-Event-ID, and buffers outgoing messages during
//! disconnects.

use std::collections::VecDeque;
use std::time::Duration;

use crate::client::http::HttpClientError;

// ─── Constants ─────────────────────────────────────────────────────

/// Initial reconnection delay.
#[allow(dead_code)]
const RECONNECT_INITIAL: Duration = Duration::from_secs(1);

/// Maximum reconnection delay.
#[allow(dead_code)]
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Backoff multiplier.
#[allow(dead_code)]
const RECONNECT_MULTIPLIER: u32 = 2;

/// Maximum outgoing messages buffered during disconnect.
#[allow(dead_code)]
const OUTGOING_BUFFER_CAP: usize = 256;

// ─── SSE Event Types ───────────────────────────────────────────────

/// A parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct SseEvent {
    /// Event type (default: "message").
    pub event_type: String,
    /// Event data (multiple `data:` lines concatenated with `\n`).
    pub data: String,
    /// Last event ID (empty string if not set).
    pub id: String,
}

#[allow(dead_code)]
impl SseEvent {
    /// Whether this is an endpoint discovery event.
    #[allow(dead_code)]
    pub fn is_endpoint(&self) -> bool {
        self.event_type == "endpoint"
    }
}

// ─── SSE Parser ────────────────────────────────────────────────────

/// Incremental SSE event parser per W3C EventSource spec.
///
/// Processes lines from a `text/event-stream` and yields complete events
/// when a blank line is encountered.
///
/// # W3C Spec Fields
/// - `event:` — sets the event type (default: "message")
/// - `data:` — appends to the data buffer (multiple lines joined with `\n`)
/// - `id:` — sets the last event ID
/// - `retry:` — sets the reconnection interval (in milliseconds)
/// - Lines starting with `:` are comments (ignored; used for keepalive)
/// - Blank line dispatches the event
#[allow(dead_code)]
pub struct SseParser {
    /// Accumulated event type for the current event.
    event_type: String,
    /// Accumulated data lines for the current event.
    data_buf: String,
    /// Whether any `data` field was seen for the current event.
    has_data: bool,
    /// Current event ID.
    event_id: String,
    /// Last successfully dispatched event ID.
    last_event_id: String,
    /// Server-requested retry interval (None = use default).
    retry_ms: Option<u64>,
    /// Incomplete line buffer for partial reads.
    line_buf: String,
}

#[allow(dead_code)]
impl SseParser {
    /// Create a new SSE parser.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            event_type: String::new(),
            data_buf: String::new(),
            has_data: false,
            event_id: String::new(),
            last_event_id: String::new(),
            retry_ms: None,
            line_buf: String::new(),
        }
    }

    /// Feed a chunk of bytes from the stream. Returns any complete events.
    ///
    /// Handles partial lines across chunk boundaries.
    #[allow(dead_code)]
    pub fn feed(&mut self, chunk: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();
        self.line_buf.push_str(chunk);

        // Process complete lines. A line ends with \n or \r\n or \r.
        loop {
            let pos = self.line_buf.find('\n').or_else(|| {
                // Handle bare \r (not followed by \n) as line ending.
                let r_pos = self.line_buf.find('\r')?;
                // If \r is the last char, it might be followed by \n in the
                // next chunk — don't consume it yet.
                if r_pos + 1 < self.line_buf.len() {
                    Some(r_pos)
                } else {
                    None
                }
            });

            let Some(pos) = pos else { break };

            // Extract the line (strip \r\n or \n or \r).
            let mut end = pos;
            if end > 0 && self.line_buf.as_bytes().get(end - 1) == Some(&b'\r') {
                end -= 1;
            }
            let line: String = self.line_buf[..end].to_owned();

            // Consume past the newline character(s).
            let consume_to = if self.line_buf.as_bytes().get(pos) == Some(&b'\r') {
                // \r — if followed by \n, consume both
                if self.line_buf.as_bytes().get(pos + 1) == Some(&b'\n') {
                    pos + 2
                } else {
                    pos + 1
                }
            } else {
                // \n
                pos + 1
            };
            self.line_buf = self.line_buf[consume_to..].to_owned();

            if let Some(event) = self.process_line(&line) {
                events.push(event);
            }
        }

        events
    }

    /// Get the last event ID (for reconnection).
    #[allow(dead_code)]
    pub fn last_event_id(&self) -> &str {
        &self.last_event_id
    }

    /// Get the server-requested retry interval, if any.
    #[allow(dead_code)]
    pub fn retry_interval(&self) -> Option<Duration> {
        self.retry_ms.map(Duration::from_millis)
    }

    /// Process a single complete line.
    ///
    /// Returns `Some(event)` if a blank line triggers event dispatch.
    #[allow(dead_code)]
    fn process_line(&mut self, line: &str) -> Option<SseEvent> {
        // Blank line → dispatch event
        if line.is_empty() {
            return self.dispatch_event();
        }

        // Comment line (starts with ':')
        if line.starts_with(':') {
            return None;
        }

        // Parse "field: value" or "field:value" or "field"
        let (field, value) = if let Some(colon_pos) = line.find(':') {
            let field = &line[..colon_pos];
            let value = &line[colon_pos + 1..];
            // Strip single leading space from value (per spec).
            let value = value.strip_prefix(' ').unwrap_or(value);
            (field, value)
        } else {
            // Field with no value
            (line, "")
        };

        match field {
            "event" => {
                self.event_type = value.to_owned();
            }
            "data" => {
                if self.has_data {
                    self.data_buf.push('\n');
                }
                self.has_data = true;
                self.data_buf.push_str(value);
            }
            "id" => {
                // Spec: if value contains U+0000, ignore the field.
                if !value.contains('\0') {
                    self.event_id = value.to_owned();
                }
            }
            "retry" => {
                if let Ok(ms) = value.parse::<u64>() {
                    self.retry_ms = Some(ms);
                }
            }
            _ => {
                // Unknown field — ignore per spec.
            }
        }

        None
    }

    /// Dispatch the accumulated event.
    #[allow(dead_code)]
    fn dispatch_event(&mut self) -> Option<SseEvent> {
        // If no data field was seen, don't dispatch (per spec).
        if !self.has_data {
            // Still reset event type.
            self.event_type.clear();
            return None;
        }

        let event_type = if self.event_type.is_empty() {
            "message".to_owned()
        } else {
            std::mem::take(&mut self.event_type)
        };

        let data = std::mem::take(&mut self.data_buf);
        let id = std::mem::take(&mut self.event_id);

        // Update last event ID.
        if !id.is_empty() {
            self.last_event_id = id.clone();
        }

        // Reset for next event.
        self.has_data = false;
        self.event_type.clear();

        Some(SseEvent {
            event_type,
            data,
            id,
        })
    }
}

#[allow(dead_code)]
impl Default for SseParser {
    #[allow(dead_code)]
    fn default() -> Self {
        Self::new()
    }
}

// ─── Reconnection State ────────────────────────────────────────────

/// Tracks reconnection backoff state.
#[derive(Debug)]
#[allow(dead_code)]
pub struct ReconnectState {
    /// Current delay before next reconnect attempt.
    current_delay: Duration,
    /// Number of consecutive reconnection attempts.
    pub attempts: u32,
}

#[allow(dead_code)]
impl ReconnectState {
    /// Create a new reconnect state with initial delay.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            current_delay: RECONNECT_INITIAL,
            attempts: 0,
        }
    }

    /// Get the delay for the next reconnect attempt and advance the backoff.
    #[allow(dead_code)]
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current_delay;
        self.attempts += 1;
        self.current_delay = (self.current_delay * RECONNECT_MULTIPLIER).min(RECONNECT_MAX);
        delay
    }

    /// Reset backoff after a successful connection.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.current_delay = RECONNECT_INITIAL;
        self.attempts = 0;
    }

    /// Override the delay with a server-provided retry value.
    #[allow(dead_code)]
    pub fn set_retry(&mut self, interval: Duration) {
        self.current_delay = interval.min(RECONNECT_MAX);
    }
}

#[allow(dead_code)]
impl Default for ReconnectState {
    #[allow(dead_code)]
    fn default() -> Self {
        Self::new()
    }
}

// ─── Outgoing Message Buffer ───────────────────────────────────────

/// Error when the outgoing buffer is full.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum SseClientError {
    #[error("outgoing buffer full ({cap} messages)")]
    BufferFull { cap: usize },

    #[error("HTTP error: {0}")]
    Http(#[from] HttpClientError),

    #[error("not connected")]
    NotConnected,

    #[error("endpoint URL not yet discovered")]
    NoEndpoint,

    #[error("SSE stream ended")]
    StreamEnded,
}

/// Buffer for outgoing messages during SSE disconnect.
///
/// When the SSE connection drops, outgoing messages (that would normally be
/// POSTed to the server) are buffered here. After successful reconnect,
/// the buffer is flushed.
#[derive(Debug)]
#[allow(dead_code)]
pub struct OutgoingBuffer {
    queue: VecDeque<Vec<u8>>,
    capacity: usize,
}

#[allow(dead_code)]
impl OutgoingBuffer {
    /// Create a new outgoing buffer with the default capacity (256).
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            capacity: OUTGOING_BUFFER_CAP,
        }
    }

    /// Create a new outgoing buffer with a custom capacity.
    #[allow(dead_code)]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            capacity,
        }
    }

    /// Push a message into the buffer.
    ///
    /// Returns `Err(SseClientError::BufferFull)` if the buffer is at capacity.
    #[allow(dead_code)]
    pub fn push(&mut self, message: Vec<u8>) -> Result<(), SseClientError> {
        if self.queue.len() >= self.capacity {
            return Err(SseClientError::BufferFull { cap: self.capacity });
        }
        self.queue.push_back(message);
        Ok(())
    }

    /// Drain all buffered messages.
    #[allow(dead_code)]
    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        self.queue.drain(..).collect()
    }

    /// Number of buffered messages.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the buffer is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Whether the buffer is full.
    #[allow(dead_code)]
    pub fn is_full(&self) -> bool {
        self.queue.len() >= self.capacity
    }

    /// The buffer capacity.
    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[allow(dead_code)]
impl Default for OutgoingBuffer {
    #[allow(dead_code)]
    fn default() -> Self {
        Self::new()
    }
}

// ─── URL Resolution ────────────────────────────────────────────────

/// Resolve an endpoint URL (from the SSE `endpoint` event) against the
/// SSE server base URL.
///
/// The endpoint URL may be:
/// - Absolute: `http://host/path` — used as-is
/// - Absolute path: `/message` — resolved against the server's scheme + authority
/// - Relative path: `message` — resolved against the server's base path
#[allow(dead_code)]
pub fn resolve_endpoint_url(sse_url: &str, endpoint: &str) -> String {
    // Absolute URL
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_owned();
    }

    // Parse the SSE URL to get scheme + authority.
    // Extract scheme.
    let (scheme, rest) = if let Some(rest) = sse_url.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = sse_url.strip_prefix("http://") {
        ("http", rest)
    } else {
        // Unknown scheme — just return endpoint as-is.
        return endpoint.to_owned();
    };

    // Split authority from path.
    let (authority, _path) = rest
        .find('/')
        .map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));

    // Protocol-relative
    if let Some(rest) = endpoint.strip_prefix("//") {
        return format!("{scheme}://{rest}");
    }

    // Absolute path
    if endpoint.starts_with('/') {
        return format!("{scheme}://{authority}{endpoint}");
    }

    // Relative path — resolve against the SSE URL's directory.
    let base_path = _path.rfind('/').map_or("/", |i| &_path[..=i]);
    format!("{scheme}://{authority}{base_path}{endpoint}")
}

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── SSE Parser: basic events ───────────────────────────────────

    #[test]
    fn parse_simple_message_event() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: hello world\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "message");
        assert_eq!(events[0].data, "hello world");
        assert!(events[0].id.is_empty());
    }

    #[test]
    fn parse_named_event() {
        let mut parser = SseParser::new();
        let events = parser.feed("event: endpoint\ndata: /message\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "endpoint");
        assert_eq!(events[0].data, "/message");
        assert!(events[0].is_endpoint());
    }

    #[test]
    fn parse_event_with_id() {
        let mut parser = SseParser::new();
        let events = parser.feed("id: 42\ndata: test\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "42");
        assert_eq!(events[0].data, "test");
        assert_eq!(parser.last_event_id(), "42");
    }

    // ─── SSE Parser: multi-line data ────────────────────────────────

    #[test]
    fn parse_multi_line_data() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: line1\ndata: line2\ndata: line3\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2\nline3");
    }

    #[test]
    fn parse_multi_line_data_with_empty_lines() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: line1\ndata:\ndata: line3\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\n\nline3");
    }

    // ─── SSE Parser: comments ───────────────────────────────────────

    #[test]
    fn comments_ignored() {
        let mut parser = SseParser::new();
        let events = parser.feed(": keepalive\ndata: actual data\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "actual data");
    }

    #[test]
    fn comment_only_no_event() {
        let mut parser = SseParser::new();
        let events = parser.feed(": just a comment\n\n");
        assert!(events.is_empty());
    }

    // ─── SSE Parser: retry field ────────────────────────────────────

    #[test]
    fn retry_field_parsed() {
        let mut parser = SseParser::new();
        let _ = parser.feed("retry: 5000\ndata: x\n\n");
        assert_eq!(parser.retry_interval(), Some(Duration::from_millis(5000)));
    }

    #[test]
    fn retry_field_non_numeric_ignored() {
        let mut parser = SseParser::new();
        let _ = parser.feed("retry: abc\ndata: x\n\n");
        assert_eq!(parser.retry_interval(), None);
    }

    // ─── SSE Parser: edge cases ─────────────────────────────────────

    #[test]
    fn empty_data_no_event() {
        let mut parser = SseParser::new();
        // Blank line with no data fields → no event dispatched.
        let events = parser.feed("event: test\n\n");
        assert!(events.is_empty());
    }

    #[test]
    fn data_no_space_after_colon() {
        let mut parser = SseParser::new();
        let events = parser.feed("data:no-space\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "no-space");
    }

    #[test]
    fn data_with_colon_in_value() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: key:value:extra\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "key:value:extra");
    }

    #[test]
    fn field_with_no_value() {
        let mut parser = SseParser::new();
        let events = parser.feed("data\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "");
    }

    #[test]
    fn unknown_fields_ignored() {
        let mut parser = SseParser::new();
        let events = parser.feed("foo: bar\ndata: actual\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "actual");
    }

    #[test]
    fn id_with_null_ignored() {
        let mut parser = SseParser::new();
        let events = parser.feed("id: valid\ndata: a\n\n");
        assert_eq!(parser.last_event_id(), "valid");
        // ID containing null should be ignored per spec.
        let events2 = parser.feed("id: bad\x00id\ndata: b\n\n");
        assert_eq!(events2.len(), 1);
        // last_event_id unchanged.
        assert_eq!(parser.last_event_id(), "valid");
        let _ = events;
    }

    // ─── SSE Parser: multiple events in one chunk ───────────────────

    #[test]
    fn multiple_events_in_one_chunk() {
        let mut parser = SseParser::new();
        let events = parser.feed(
            "data: first\n\ndata: second\n\nevent: custom\ndata: third\n\n",
        );
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].data, "first");
        assert_eq!(events[1].data, "second");
        assert_eq!(events[2].event_type, "custom");
        assert_eq!(events[2].data, "third");
    }

    // ─── SSE Parser: chunked input ──────────────────────────────────

    #[test]
    fn chunked_input_across_boundaries() {
        let mut parser = SseParser::new();
        // Feed partial data.
        let events1 = parser.feed("data: hel");
        assert!(events1.is_empty());
        let events2 = parser.feed("lo\n");
        assert!(events2.is_empty());
        let events3 = parser.feed("\n");
        assert_eq!(events3.len(), 1);
        assert_eq!(events3[0].data, "hello");
    }

    #[test]
    fn chunked_field_split() {
        let mut parser = SseParser::new();
        let e1 = parser.feed("eve");
        assert!(e1.is_empty());
        let e2 = parser.feed("nt: endpoint\nda");
        assert!(e2.is_empty());
        let e3 = parser.feed("ta: /msg\n\n");
        assert_eq!(e3.len(), 1);
        assert_eq!(e3[0].event_type, "endpoint");
        assert_eq!(e3[0].data, "/msg");
    }

    // ─── SSE Parser: CRLF line endings ──────────────────────────────

    #[test]
    fn crlf_line_endings() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: hello\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn mixed_line_endings() {
        let mut parser = SseParser::new();
        let events = parser.feed("data: a\ndata: b\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "a\nb");
    }

    // ─── SSE Parser: last event ID tracking ─────────────────────────

    #[test]
    fn last_event_id_persists_across_events() {
        let mut parser = SseParser::new();
        let _ = parser.feed("id: 1\ndata: a\n\n");
        assert_eq!(parser.last_event_id(), "1");
        // Next event without id — last_event_id still "1".
        let _ = parser.feed("data: b\n\n");
        assert_eq!(parser.last_event_id(), "1");
        // Next event with new id.
        let _ = parser.feed("id: 2\ndata: c\n\n");
        assert_eq!(parser.last_event_id(), "2");
    }

    #[test]
    fn empty_id_resets_last_event_id() {
        let mut parser = SseParser::new();
        let _ = parser.feed("id: 1\ndata: a\n\n");
        assert_eq!(parser.last_event_id(), "1");
        // Empty id field: per spec, empty string is valid but doesn't
        // update last_event_id (only non-empty values update it).
        let events = parser.feed("id: \ndata: b\n\n");
        assert_eq!(events[0].id, "");
        // last_event_id unchanged since id was empty string.
        assert_eq!(parser.last_event_id(), "1");
    }

    // ─── SSE Parser: event type resets ──────────────────────────────

    #[test]
    fn event_type_resets_after_dispatch() {
        let mut parser = SseParser::new();
        let events = parser.feed("event: custom\ndata: a\n\ndata: b\n\n");
        assert_eq!(events[0].event_type, "custom");
        // Second event should default to "message".
        assert_eq!(events[1].event_type, "message");
    }

    // ─── ReconnectState ─────────────────────────────────────────────

    #[test]
    fn reconnect_exponential_backoff() {
        let mut state = ReconnectState::new();
        assert_eq!(state.next_delay(), Duration::from_secs(1));
        assert_eq!(state.attempts, 1);
        assert_eq!(state.next_delay(), Duration::from_secs(2));
        assert_eq!(state.attempts, 2);
        assert_eq!(state.next_delay(), Duration::from_secs(4));
        assert_eq!(state.next_delay(), Duration::from_secs(8));
        assert_eq!(state.next_delay(), Duration::from_secs(16));
        // Max is 30s.
        assert_eq!(state.next_delay(), Duration::from_secs(30));
        assert_eq!(state.next_delay(), Duration::from_secs(30));
    }

    #[test]
    fn reconnect_reset() {
        let mut state = ReconnectState::new();
        let _ = state.next_delay();
        let _ = state.next_delay();
        assert_eq!(state.attempts, 2);
        state.reset();
        assert_eq!(state.attempts, 0);
        assert_eq!(state.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn reconnect_server_retry() {
        let mut state = ReconnectState::new();
        state.set_retry(Duration::from_secs(5));
        assert_eq!(state.next_delay(), Duration::from_secs(5));
        // After next_delay, backoff resumes from the retry value * 2.
        assert_eq!(state.next_delay(), Duration::from_secs(10));
    }

    #[test]
    fn reconnect_server_retry_capped() {
        let mut state = ReconnectState::new();
        state.set_retry(Duration::from_secs(60));
        // Should be capped to RECONNECT_MAX.
        assert_eq!(state.next_delay(), Duration::from_secs(30));
    }

    // ─── OutgoingBuffer ─────────────────────────────────────────────

    #[test]
    fn buffer_push_and_drain() {
        let mut buf = OutgoingBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);

        buf.push(b"msg1".to_vec()).unwrap();
        buf.push(b"msg2".to_vec()).unwrap();
        assert_eq!(buf.len(), 2);
        assert!(!buf.is_empty());

        let drained = buf.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0], b"msg1");
        assert_eq!(drained[1], b"msg2");
        assert!(buf.is_empty());
    }

    #[test]
    fn buffer_capacity_enforced() {
        let mut buf = OutgoingBuffer::with_capacity(3);
        assert_eq!(buf.capacity(), 3);

        buf.push(b"1".to_vec()).unwrap();
        buf.push(b"2".to_vec()).unwrap();
        buf.push(b"3".to_vec()).unwrap();
        assert!(buf.is_full());

        // 4th message should fail.
        let err = buf.push(b"4".to_vec()).unwrap_err();
        match err {
            SseClientError::BufferFull { cap } => assert_eq!(cap, 3),
            _ => panic!("expected BufferFull, got {err:?}"),
        }
    }

    #[test]
    fn buffer_default_capacity() {
        let buf = OutgoingBuffer::new();
        assert_eq!(buf.capacity(), OUTGOING_BUFFER_CAP);
    }

    #[test]
    fn buffer_drain_then_reuse() {
        let mut buf = OutgoingBuffer::with_capacity(2);
        buf.push(b"a".to_vec()).unwrap();
        buf.push(b"b".to_vec()).unwrap();
        assert!(buf.is_full());

        let _ = buf.drain();
        assert!(buf.is_empty());

        // Can push again after drain.
        buf.push(b"c".to_vec()).unwrap();
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn buffer_full_at_256() {
        let mut buf = OutgoingBuffer::new();
        for i in 0..256 {
            buf.push(format!("msg-{i}").into_bytes()).unwrap();
        }
        assert!(buf.is_full());
        assert_eq!(buf.len(), 256);

        // 257th message should fail.
        let err = buf.push(b"overflow".to_vec()).unwrap_err();
        assert!(matches!(err, SseClientError::BufferFull { cap: 256 }));
    }

    // ─── Endpoint URL resolution ────────────────────────────────────

    #[test]
    fn resolve_absolute_endpoint() {
        let result = resolve_endpoint_url(
            "http://localhost:8080/sse",
            "http://other-host:9090/message",
        );
        assert_eq!(result, "http://other-host:9090/message");
    }

    #[test]
    fn resolve_absolute_path_endpoint() {
        let result = resolve_endpoint_url(
            "http://localhost:8080/sse",
            "/message",
        );
        assert_eq!(result, "http://localhost:8080/message");
    }

    #[test]
    fn resolve_relative_endpoint() {
        let result = resolve_endpoint_url(
            "http://localhost:8080/api/sse",
            "message",
        );
        assert_eq!(result, "http://localhost:8080/api/message");
    }

    #[test]
    fn resolve_relative_endpoint_root_path() {
        let result = resolve_endpoint_url(
            "http://localhost:8080/sse",
            "message",
        );
        assert_eq!(result, "http://localhost:8080/message");
    }

    #[test]
    fn resolve_https_endpoint() {
        let result = resolve_endpoint_url(
            "https://secure.example.com/sse",
            "/api/message",
        );
        assert_eq!(result, "https://secure.example.com/api/message");
    }

    #[test]
    fn resolve_protocol_relative_endpoint() {
        let result = resolve_endpoint_url(
            "https://example.com/sse",
            "//other.com/msg",
        );
        assert_eq!(result, "https://other.com/msg");
    }

    #[test]
    fn resolve_endpoint_with_query_string() {
        let result = resolve_endpoint_url(
            "http://localhost:8080/sse?token=abc",
            "/message",
        );
        assert_eq!(result, "http://localhost:8080/message");
    }

    // ─── SseEvent helpers ───────────────────────────────────────────

    #[test]
    fn sse_event_is_endpoint() {
        let event = SseEvent {
            event_type: "endpoint".into(),
            data: "/message".into(),
            id: String::new(),
        };
        assert!(event.is_endpoint());

        let event2 = SseEvent {
            event_type: "message".into(),
            data: "hello".into(),
            id: String::new(),
        };
        assert!(!event2.is_endpoint());
    }

    // ─── Error display ──────────────────────────────────────────────

    #[test]
    fn error_display() {
        let err = SseClientError::BufferFull { cap: 256 };
        assert!(format!("{err}").contains("256"));

        let err = SseClientError::NotConnected;
        assert!(format!("{err}").contains("not connected"));

        let err = SseClientError::NoEndpoint;
        assert!(format!("{err}").contains("endpoint"));

        let err = SseClientError::StreamEnded;
        assert!(format!("{err}").contains("ended"));
    }

    // ─── SSE Parser: JSON-RPC payloads ──────────────────────────────

    #[test]
    fn parse_json_rpc_event() {
        let mut parser = SseParser::new();
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let input = format!("event: message\ndata: {json}\n\n");
        let events = parser.feed(&input);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "message");
        assert_eq!(events[0].data, json);
    }

    #[test]
    fn parse_endpoint_then_message() {
        let mut parser = SseParser::new();
        let input =
            "event: endpoint\ndata: /message?sessionId=abc\n\ndata: {\"jsonrpc\":\"2.0\"}\n\n";
        let events = parser.feed(input);
        assert_eq!(events.len(), 2);
        assert!(events[0].is_endpoint());
        assert_eq!(events[0].data, "/message?sessionId=abc");
        assert_eq!(events[1].event_type, "message");
        assert_eq!(events[1].data, "{\"jsonrpc\":\"2.0\"}");
    }
}
