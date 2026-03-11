// Public API consumed by downstream beads — suppress dead_code until wired up.
#![allow(dead_code)]

use std::io::{self, BufRead, BufReader, Read, Write};

use crate::jsonrpc::{self, Parsed, RawMessage};
use crate::observe::{Logger, Metrics};

/// Maximum size of a single complete JSON-RPC message (D-101).
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Default partial-line buffer limit per child (D-106).
pub const DEFAULT_PARTIAL_BUFFER: usize = 64 * 1024 * 1024;

/// Stderr line truncation limit (US-005 spec).
pub const STDERR_MAX_LINE: usize = 4 * 1024;

/// Preview length for error logging.
const ERROR_PREVIEW_LEN: usize = 200;

/// Codec errors. `InvalidUtf8` and `BufferOverflow` are fatal — caller must
/// kill the child/session and return JSON-RPC -32603 to in-flight requests.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("invalid UTF-8 on stdout (protocol violation)")]
    InvalidUtf8,

    #[error("partial buffer overflow ({size} bytes exceeds {limit} byte limit)")]
    BufferOverflow { size: usize, limit: usize },

    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Line-delimited codec for reading JSON-RPC messages from a child's stdout.
///
/// Reads newline-delimited lines, validates UTF-8, enforces size limits,
/// and parses each line as a JSON-RPC message or batch.
pub struct StdoutCodec<R> {
    reader: BufReader<R>,
    max_buffer: usize,
    buf: Vec<u8>,
}

impl<R: Read> StdoutCodec<R> {
    /// Create a new stdout codec.
    ///
    /// `max_buffer` is the per-child partial-line buffer limit (typically
    /// [`DEFAULT_PARTIAL_BUFFER`] = 64MB).
    pub fn new(reader: R, max_buffer: usize) -> Self {
        Self {
            reader: BufReader::new(reader),
            max_buffer,
            buf: Vec::new(),
        }
    }

    /// Read raw bytes until `\n` or EOF. Returns `Ok(None)` on clean EOF
    /// (no trailing bytes). Returns the line bytes *without* the `\n` delimiter.
    fn read_line_raw(&mut self) -> Result<Option<Vec<u8>>, CodecError> {
        self.buf.clear();
        loop {
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(std::mem::take(&mut self.buf)));
            }

            if let Some(pos) = available.iter().position(|&b| b == b'\n') {
                self.buf.extend_from_slice(&available[..pos]);
                self.reader.consume(pos + 1);
                return Ok(Some(std::mem::take(&mut self.buf)));
            }

            let len = available.len();
            self.buf.extend_from_slice(available);
            self.reader.consume(len);

            if self.buf.len() > self.max_buffer {
                return Err(CodecError::BufferOverflow {
                    size: self.buf.len(),
                    limit: self.max_buffer,
                });
            }
        }
    }

    /// Read the next JSON-RPC message (or batch).
    ///
    /// Returns `Ok(None)` on EOF. Fatal errors (`InvalidUtf8`, `BufferOverflow`)
    /// require the caller to kill the child/session. Parse errors and oversized
    /// messages (>16MB) are logged, counted in `decode_errors`, and skipped.
    pub fn read_message(
        &mut self,
        metrics: &Metrics,
        logger: &Logger,
    ) -> Result<Option<Parsed>, CodecError> {
        loop {
            let raw = match self.read_line_raw()? {
                Some(bytes) => bytes,
                None => return Ok(None),
            };

            // Strip trailing \r (Windows compat)
            let raw = if raw.last() == Some(&b'\r') {
                &raw[..raw.len() - 1]
            } else {
                &raw[..]
            };

            // Skip empty / whitespace-only lines
            if raw.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }

            // Validate UTF-8 — fatal on stdout
            let line = match std::str::from_utf8(raw) {
                Ok(s) => s,
                Err(_) => {
                    Metrics::inc(&metrics.decode_errors);
                    logger.error("invalid UTF-8 on stdout — killing child");
                    return Err(CodecError::InvalidUtf8);
                }
            };

            // Per-message size limit (16MB)
            if line.len() > MAX_MESSAGE_SIZE {
                Metrics::inc(&metrics.decode_errors);
                let preview: String = line.chars().take(ERROR_PREVIEW_LEN).collect();
                logger.error(&format!(
                    "message exceeds 16MB limit ({} bytes), skipping: {preview}",
                    line.len()
                ));
                continue;
            }

            // Parse JSON-RPC
            match jsonrpc::parse_line(line) {
                Ok(parsed) => return Ok(Some(parsed)),
                Err(e) => {
                    Metrics::inc(&metrics.decode_errors);
                    let preview: String = line.chars().take(ERROR_PREVIEW_LEN).collect();
                    logger.error(&format!("JSON parse error: {e} — skipping: {preview}"));
                    continue;
                }
            }
        }
    }
}

/// Write a JSON-RPC message: serialize + `\n` + flush.
///
/// Caller must hold a mutex for shared-child modes (SSE, WS, stateful HTTP).
/// See bead notes on D-012 for mutex ownership details.
pub fn write_message<W: Write>(writer: &mut W, msg: &RawMessage) -> io::Result<()> {
    let json =
        serde_json::to_string(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    writer.write_all(json.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Read a line from stderr with lossy UTF-8 decode.
///
/// Invalid UTF-8 bytes are replaced with U+FFFD. Never returns a fatal error
/// for encoding issues. Lines longer than `max_line` are truncated. Returns
/// `Ok(None)` on EOF.
///
/// The caller should create the `BufReader` with an appropriate capacity
/// (typically 64KB for stderr).
pub fn read_stderr_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_line: usize,
) -> io::Result<Option<String>> {
    buf.clear();
    let n = reader.read_until(b'\n', buf)?;
    if n == 0 {
        return Ok(None);
    }
    // Strip trailing newline
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    // Strip trailing carriage return
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    // Truncate to max_line
    buf.truncate(max_line);
    Ok(Some(String::from_utf8_lossy(buf).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{LogLevel, OutputTransport};
    use std::io::Cursor;
    use std::sync::atomic::Ordering;

    fn test_metrics() -> std::sync::Arc<Metrics> {
        Metrics::new()
    }

    fn test_logger() -> Logger {
        Logger::buffered(OutputTransport::Sse, LogLevel::Debug)
    }

    // ─── StdoutCodec: single message ──────────────────────────────────────

    #[test]
    fn single_message() {
        let data = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
        let mut codec = StdoutCodec::new(Cursor::new(data.to_vec()), DEFAULT_PARTIAL_BUFFER);
        let metrics = test_metrics();
        let logger = test_logger();

        let msg = codec.read_message(&metrics, &logger).unwrap().unwrap();
        assert!(matches!(msg, Parsed::Single(ref m) if m.is_request()));
        assert!(codec.read_message(&metrics, &logger).unwrap().is_none());
    }

    // ─── StdoutCodec: batch ────────────────────────────────────────────────

    #[test]
    fn batch_message() {
        let data =
            b"[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"a\"},{\"jsonrpc\":\"2.0\",\"method\":\"b\"}]\n";
        let mut codec = StdoutCodec::new(Cursor::new(data.to_vec()), DEFAULT_PARTIAL_BUFFER);
        let metrics = test_metrics();
        let logger = test_logger();

        match codec.read_message(&metrics, &logger).unwrap().unwrap() {
            Parsed::Batch(msgs) => {
                assert_eq!(msgs.len(), 2);
                assert!(msgs[0].is_request());
                assert!(msgs[1].is_notification());
            }
            _ => panic!("expected batch"),
        }
    }

    // ─── StdoutCodec: empty lines skipped ──────────────────────────────────

    #[test]
    fn empty_lines_skipped() {
        let data = b"\n\n  \n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\n";
        let mut codec = StdoutCodec::new(Cursor::new(data.to_vec()), DEFAULT_PARTIAL_BUFFER);
        let metrics = test_metrics();
        let logger = test_logger();

        let msg = codec.read_message(&metrics, &logger).unwrap().unwrap();
        assert!(matches!(msg, Parsed::Single(_)));
        assert!(codec.read_message(&metrics, &logger).unwrap().is_none());
    }

    // ─── StdoutCodec: \r\n line endings ────────────────────────────────────

    #[test]
    fn crlf_line_endings() {
        let data = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\r\n";
        let mut codec = StdoutCodec::new(Cursor::new(data.to_vec()), DEFAULT_PARTIAL_BUFFER);
        let metrics = test_metrics();
        let logger = test_logger();

        let msg = codec.read_message(&metrics, &logger).unwrap().unwrap();
        assert!(matches!(msg, Parsed::Single(ref m) if m.is_request()));
    }

    // ─── StdoutCodec: oversized message skipped ────────────────────────────

    #[test]
    fn oversized_message_skipped() {
        let big = "x".repeat(MAX_MESSAGE_SIZE + 1);
        let valid = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}";
        let data = format!("{big}\n{valid}\n");
        let mut codec = StdoutCodec::new(
            Cursor::new(data.into_bytes()),
            DEFAULT_PARTIAL_BUFFER,
        );
        let metrics = test_metrics();
        let logger = test_logger();

        // Oversized line skipped; we get the valid message
        let msg = codec.read_message(&metrics, &logger).unwrap().unwrap();
        assert!(matches!(msg, Parsed::Single(ref m) if m.is_request()));
        assert_eq!(metrics.decode_errors.load(Ordering::Relaxed), 1);
        let log_output = logger.get_output();
        assert!(log_output.contains("exceeds 16MB limit"));
    }

    // ─── StdoutCodec: parse error skipped ──────────────────────────────────

    #[test]
    fn parse_error_skipped() {
        let data = b"not json at all\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
        let mut codec = StdoutCodec::new(Cursor::new(data.to_vec()), DEFAULT_PARTIAL_BUFFER);
        let metrics = test_metrics();
        let logger = test_logger();

        let msg = codec.read_message(&metrics, &logger).unwrap().unwrap();
        assert!(matches!(msg, Parsed::Single(ref m) if m.is_request()));
        assert_eq!(metrics.decode_errors.load(Ordering::Relaxed), 1);

        let log_output = logger.get_output();
        assert!(log_output.contains("JSON parse error"));
        assert!(log_output.contains("not json at all"));
    }

    // ─── StdoutCodec: partial buffer overflow ──────────────────────────────

    #[test]
    fn partial_buffer_overflow() {
        // 1024 bytes with no newline, max_buffer = 512
        let data = vec![b'a'; 1024];
        let mut codec = StdoutCodec::new(Cursor::new(data), 512);
        let metrics = test_metrics();
        let logger = test_logger();

        match codec.read_message(&metrics, &logger) {
            Err(CodecError::BufferOverflow { size, limit }) => {
                assert!(size > 512);
                assert_eq!(limit, 512);
            }
            other => panic!("expected BufferOverflow, got {other:?}"),
        }
    }

    // ─── StdoutCodec: invalid UTF-8 on stdout is fatal ─────────────────────

    #[test]
    fn invalid_utf8_stdout_fatal() {
        let mut data = Vec::new();
        data.extend_from_slice(b"\xff\xfe invalid utf8");
        data.push(b'\n');
        let mut codec = StdoutCodec::new(Cursor::new(data), DEFAULT_PARTIAL_BUFFER);
        let metrics = test_metrics();
        let logger = test_logger();

        match codec.read_message(&metrics, &logger) {
            Err(CodecError::InvalidUtf8) => {}
            other => panic!("expected InvalidUtf8, got {other:?}"),
        }
        assert_eq!(metrics.decode_errors.load(Ordering::Relaxed), 1);
    }

    // ─── Stderr: lossy UTF-8 decode ─────────────────────────────────────────

    #[test]
    fn stderr_lossy_utf8() {
        let mut data = Vec::new();
        data.extend_from_slice(b"hello \xff world\n");
        let mut reader = BufReader::new(Cursor::new(data));
        let mut buf = Vec::new();

        let line = read_stderr_line(&mut reader, &mut buf, STDERR_MAX_LINE)
            .unwrap()
            .unwrap();
        assert!(line.contains("hello"));
        assert!(line.contains('\u{FFFD}'));
        assert!(line.contains("world"));
    }

    #[test]
    fn stderr_line_truncation() {
        let long_line = "a".repeat(8192);
        let data = format!("{long_line}\n").into_bytes();
        let mut reader = BufReader::new(Cursor::new(data));
        let mut buf = Vec::new();

        let line = read_stderr_line(&mut reader, &mut buf, STDERR_MAX_LINE)
            .unwrap()
            .unwrap();
        assert_eq!(line.len(), STDERR_MAX_LINE);
    }

    #[test]
    fn stderr_crlf() {
        let data = b"stderr output\r\n";
        let mut reader = BufReader::new(Cursor::new(data.to_vec()));
        let mut buf = Vec::new();

        let line = read_stderr_line(&mut reader, &mut buf, STDERR_MAX_LINE)
            .unwrap()
            .unwrap();
        assert_eq!(line, "stderr output");
    }

    // ─── EOF handling ───────────────────────────────────────────────────────

    #[test]
    fn eof_with_trailing_newline() {
        let data = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
        let mut codec = StdoutCodec::new(Cursor::new(data.to_vec()), DEFAULT_PARTIAL_BUFFER);
        let metrics = test_metrics();
        let logger = test_logger();

        assert!(codec.read_message(&metrics, &logger).unwrap().is_some());
        assert!(codec.read_message(&metrics, &logger).unwrap().is_none());
    }

    #[test]
    fn eof_without_trailing_newline() {
        let data = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}";
        let mut codec = StdoutCodec::new(Cursor::new(data.to_vec()), DEFAULT_PARTIAL_BUFFER);
        let metrics = test_metrics();
        let logger = test_logger();

        let msg = codec.read_message(&metrics, &logger).unwrap().unwrap();
        assert!(matches!(msg, Parsed::Single(ref m) if m.is_request()));
        assert!(codec.read_message(&metrics, &logger).unwrap().is_none());
    }

    #[test]
    fn eof_trailing_invalid_discarded() {
        let data = b"not valid json";
        let mut codec = StdoutCodec::new(Cursor::new(data.to_vec()), DEFAULT_PARTIAL_BUFFER);
        let metrics = test_metrics();
        let logger = test_logger();

        // Invalid trailing data is skipped (parse error), then EOF
        assert!(codec.read_message(&metrics, &logger).unwrap().is_none());
        assert_eq!(metrics.decode_errors.load(Ordering::Relaxed), 1);
    }

    // ─── write_message ──────────────────────────────────────────────────────

    #[test]
    fn write_message_format() {
        let msg = RawMessage {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::value::RawValue::from_string("1".into()).unwrap()),
            method: Some("ping".into()),
            params: None,
            result: None,
            error: None,
            ..Default::default()
        };
        let mut out = Vec::new();
        write_message(&mut out, &msg).unwrap();
        let output = String::from_utf8(out).unwrap();
        assert!(output.ends_with('\n'));
        assert!(output.contains("\"jsonrpc\":\"2.0\""));
        assert!(output.contains("\"method\":\"ping\""));
    }

    // ─── Multiple messages ──────────────────────────────────────────────────

    #[test]
    fn multiple_messages() {
        let data = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"a\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"b\"}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"c\"}\n",
        );
        let mut codec = StdoutCodec::new(
            Cursor::new(data.as_bytes().to_vec()),
            DEFAULT_PARTIAL_BUFFER,
        );
        let metrics = test_metrics();
        let logger = test_logger();

        let m1 = codec.read_message(&metrics, &logger).unwrap().unwrap();
        assert!(matches!(m1, Parsed::Single(ref m) if m.method_str() == Some("a")));

        let m2 = codec.read_message(&metrics, &logger).unwrap().unwrap();
        assert!(matches!(m2, Parsed::Single(ref m) if m.method_str() == Some("b")));

        let m3 = codec.read_message(&metrics, &logger).unwrap().unwrap();
        assert!(matches!(m3, Parsed::Single(ref m) if m.is_notification()));

        assert!(codec.read_message(&metrics, &logger).unwrap().is_none());
    }

    // ─── Partial UTF-8 at chunk boundaries ──────────────────────────────────

    #[test]
    fn partial_utf8_at_boundary() {
        // Multi-byte UTF-8: é = 0xC3 0xA9, correctly handled when
        // split across BufReader fill_buf() calls.
        let msg = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"café\"}\n";
        let mut codec = StdoutCodec::new(
            Cursor::new(msg.as_bytes().to_vec()),
            DEFAULT_PARTIAL_BUFFER,
        );
        let metrics = test_metrics();
        let logger = test_logger();

        let parsed = codec.read_message(&metrics, &logger).unwrap().unwrap();
        match parsed {
            Parsed::Single(m) => assert_eq!(m.method_str(), Some("café")),
            _ => panic!("expected single"),
        }
    }

    // ─── Stderr EOF ─────────────────────────────────────────────────────────

    #[test]
    fn stderr_eof() {
        let mut reader = BufReader::new(Cursor::new(Vec::<u8>::new()));
        let mut buf = Vec::new();
        assert!(read_stderr_line(&mut reader, &mut buf, STDERR_MAX_LINE)
            .unwrap()
            .is_none());
    }

    #[test]
    fn stderr_eof_without_newline() {
        let data = b"trailing stderr";
        let mut reader = BufReader::new(Cursor::new(data.to_vec()));
        let mut buf = Vec::new();

        let line = read_stderr_line(&mut reader, &mut buf, STDERR_MAX_LINE)
            .unwrap()
            .unwrap();
        assert_eq!(line, "trailing stderr");
    }
}
