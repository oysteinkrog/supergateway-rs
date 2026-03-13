
//! Minimal blocking HTTP/1.1 server helpers.
//!
//! Used by all gateway server modes (SSE, WS, stateless HTTP, stateful HTTP)
//! to bind TCP listeners and serve connections without async overhead.
//!
//! Client modes (SSE→stdio, HTTP→stdio) also use the [`blocking_http`] helpers.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

// ─── Request ────────────────────────────────────────────────────────────

/// A parsed HTTP/1.1 request from a raw TCP connection.
pub struct HttpRequest {
    /// HTTP method (GET, POST, DELETE, OPTIONS, …) in uppercase.
    pub method: String,
    /// Request path without query string (e.g. `/sse`).
    pub path: String,
    /// Raw query string after `?` (empty if absent).
    pub query: String,
    /// Request headers, all keys lowercase.
    pub headers: HashMap<String, String>,
    /// Request body bytes (sized by Content-Length).
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// Get a request header by name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// Get a URL query parameter by name.
    pub fn query_param(&self, key: &str) -> Option<&str> {
        for part in self.query.split('&') {
            let mut kv = part.splitn(2, '=');
            if kv.next() == Some(key) {
                return kv.next();
            }
        }
        None
    }
}

// ─── Parsing ────────────────────────────────────────────────────────────

/// Parse an HTTP/1.1 request from a TcpStream.
///
/// Returns `None` on parse error, EOF, or if the request line is malformed.
pub fn parse_request(stream: &TcpStream) -> Option<HttpRequest> {
    let cloned = stream.try_clone().ok()?;
    let mut reader = BufReader::new(cloned);

    // Request line
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let request_line = request_line.trim();
    if request_line.is_empty() {
        return None;
    }

    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next()?.to_uppercase();
    let full_path = parts.next()?;

    // Split path and query string
    let (path, query) = if let Some(q) = full_path.find('?') {
        (full_path[..q].to_string(), full_path[q + 1..].to_string())
    } else {
        (full_path.to_string(), String::new())
    };

    // Headers
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(colon) = trimmed.find(':') {
            let name = trimmed[..colon].trim().to_lowercase();
            let value = trimmed[colon + 1..].trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(name, value);
        }
    }

    // Body
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        use std::io::Read;
        let _ = reader.read_exact(&mut body);
    }

    Some(HttpRequest { method, path, query, headers, body })
}

// ─── Response writing ───────────────────────────────────────────────────

/// Write a complete HTTP/1.1 response (with Content-Length).
pub fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> io::Result<()> {
    let mut buf = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in headers {
        buf.push_str(k);
        buf.push_str(": ");
        buf.push_str(v);
        buf.push_str("\r\n");
    }
    buf.push_str(&format!("Content-Length: {}\r\n", body.len()));
    buf.push_str("Connection: close\r\n");
    buf.push_str("\r\n");
    stream.write_all(buf.as_bytes())?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    stream.flush()
}

/// Write HTTP/1.1 SSE response headers (no Content-Length — streaming).
///
/// After calling this, write SSE event text directly to the stream.
pub fn write_sse_response_headers(
    stream: &mut TcpStream,
    headers: &[(String, String)],
) -> io::Result<()> {
    let mut buf = "HTTP/1.1 200 OK\r\n".to_string();
    for (k, v) in headers {
        buf.push_str(k);
        buf.push_str(": ");
        buf.push_str(v);
        buf.push_str("\r\n");
    }
    buf.push_str("\r\n");
    stream.write_all(buf.as_bytes())?;
    stream.flush()
}

/// Write a plain-text error response and close.
pub fn write_error(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
    let headers = vec![("Content-Type".to_string(), "text/plain".to_string())];
    let _ = write_response(stream, status, reason, &headers, body.as_bytes());
}

// ─── WebSocket helpers ──────────────────────────────────────────────────

/// Compute the `Sec-WebSocket-Accept` header value.
///
/// SHA1(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11") encoded as base64.
pub fn ws_accept_key(client_key: &str) -> String {
    let mut input = client_key.to_string();
    input.push_str("258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let digest = sha1_bytes(input.as_bytes());
    base64_encode(&digest)
}

/// Minimal SHA1 implementation (RFC 3174).
fn sha1_bytes(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let bit_len = (data.len() as u64) * 8;

    // Pad the message
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0x00);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit block
    for block in padded.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for i in 0..80 {
            let (f, k) = if i < 20 {
                ((b & c) | (!b & d), 0x5A827999u32)
            } else if i < 40 {
                (b ^ c ^ d, 0x6ED9EBA1)
            } else if i < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1BBCDC)
            } else {
                (b ^ c ^ d, 0xCA62C1D6)
            };
            let temp = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(w[i]);
            e = d; d = c; c = b.rotate_left(30); b = a; a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, chunk) in out.chunks_mut(4).enumerate() {
        chunk.copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

/// Minimal base64 encoder.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize]);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize]);
        out.push(if chunk.len() > 1 { ALPHABET[((n >> 6) & 0x3F) as usize] } else { b'=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 0x3F) as usize] } else { b'=' });
    }
    String::from_utf8(out).unwrap()
}

// ─── WebSocket frame I/O ────────────────────────────────────────────────

/// Read one unmasked WebSocket text or binary frame.
///
/// Returns the frame payload as a string, or `None` on error / close frame.
pub fn ws_read_frame(stream: &mut TcpStream) -> Option<WsFrame> {
    use std::io::Read;

    let mut header = [0u8; 2];
    stream.read_exact(&mut header).ok()?;

    let fin = (header[0] & 0x80) != 0;
    let opcode = header[0] & 0x0F;
    let masked = (header[1] & 0x80) != 0;
    let len7 = (header[1] & 0x7F) as usize;

    let payload_len = match len7 {
        126 => {
            let mut l = [0u8; 2];
            stream.read_exact(&mut l).ok()?;
            u16::from_be_bytes(l) as usize
        }
        127 => {
            let mut l = [0u8; 8];
            stream.read_exact(&mut l).ok()?;
            u64::from_be_bytes(l) as usize
        }
        n => n,
    };

    // Mask key (only for client→server frames)
    let mask_key = if masked {
        let mut key = [0u8; 4];
        stream.read_exact(&mut key).ok()?;
        Some(key)
    } else {
        None
    };

    // Payload
    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).ok()?;

    if let Some(key) = mask_key {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= key[i % 4];
        }
    }

    Some(WsFrame { fin, opcode, payload })
}

/// A decoded WebSocket frame.
pub struct WsFrame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

impl WsFrame {
    /// Returns true for connection close frame (opcode 0x8).
    pub fn is_close(&self) -> bool { self.opcode == 0x8 }
    /// Returns true for text frame (opcode 0x1).
    pub fn is_text(&self) -> bool { self.opcode == 0x1 }
    /// Returns true for ping frame (opcode 0x9).
    pub fn is_ping(&self) -> bool { self.opcode == 0x9 }
}

/// Write an unmasked WebSocket text frame (server→client).
pub fn ws_write_text_frame(stream: &mut TcpStream, payload: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(10 + payload.len());
    buf.push(0x81); // FIN + text opcode
    if payload.len() < 126 {
        buf.push(payload.len() as u8);
    } else if payload.len() < 65536 {
        buf.push(126);
        buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        buf.push(127);
        buf.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    buf.extend_from_slice(payload);
    stream.write_all(&buf)?;
    stream.flush()
}

/// Write a WebSocket close frame.
pub fn ws_write_close_frame(stream: &mut TcpStream, code: u16, reason: &str) {
    let mut payload = Vec::with_capacity(2 + reason.len());
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(reason.as_bytes());
    let mut buf = vec![0x88u8]; // FIN + close
    buf.push(payload.len() as u8);
    buf.extend_from_slice(&payload);
    let _ = stream.write_all(&buf);
    let _ = stream.flush();
}

/// Write a pong frame in response to a ping.
pub fn ws_write_pong_frame(stream: &mut TcpStream, payload: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(2 + payload.len());
    buf.push(0x8A); // FIN + pong opcode
    buf.push(payload.len() as u8);
    buf.extend_from_slice(payload);
    stream.write_all(&buf)?;
    stream.flush()
}

// ─── Blocking HTTP client ───────────────────────────────────────────────

/// Parsed URL for a blocking HTTP connection.
pub struct ParsedHttpUrl {
    pub host: String,
    pub port: u16,
    pub path_and_query: String,
}

impl ParsedHttpUrl {
    /// Parse `http://host[:port]/path?query` into its parts.
    pub fn parse(url: &str) -> Option<Self> {
        let url = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
        let (host_port, path) = if let Some(slash) = url.find('/') {
            (&url[..slash], &url[slash..])
        } else {
            (url, "/")
        };
        let (host, port) = if let Some(colon) = host_port.find(':') {
            (&host_port[..colon], host_port[colon + 1..].parse().ok()?)
        } else {
            (host_port, 80u16)
        };
        Some(Self {
            host: host.to_string(),
            port,
            path_and_query: path.to_string(),
        })
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Return the Host header value: includes port only when non-standard.
    pub fn host_header(&self) -> String {
        if self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Response from a blocking HTTP request.
pub struct HttpResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// Make a blocking HTTP POST request.
pub fn http_post(
    url: &str,
    custom_headers: &[(String, String)],
    body: &[u8],
) -> io::Result<HttpResponse> {
    let parsed = ParsedHttpUrl::parse(url)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid URL"))?;

    let stream = TcpStream::connect(parsed.addr())?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut writer = stream.try_clone()?;

    let mut req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n",
        parsed.path_and_query, parsed.host_header(), body.len()
    );
    for (k, v) in custom_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    writer.write_all(req.as_bytes())?;
    writer.write_all(body)?;
    writer.flush()?;

    read_http_response(stream)
}

/// Open a blocking GET SSE stream, returning (stream, status, headers).
pub fn http_get_sse(
    url: &str,
    custom_headers: &[(String, String)],
    last_event_id: Option<&str>,
) -> io::Result<(TcpStream, u16, HashMap<String, String>)> {
    let parsed = ParsedHttpUrl::parse(url)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid URL"))?;

    let stream = TcpStream::connect(parsed.addr())?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut writer = stream.try_clone()?;

    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n",
        parsed.path_and_query, parsed.host_header()
    );
    if let Some(id) = last_event_id {
        if !id.is_empty() {
            req.push_str(&format!("Last-Event-ID: {id}\r\n"));
        }
    }
    for (k, v) in custom_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: keep-alive\r\n\r\n");
    writer.write_all(req.as_bytes())?;
    writer.flush()?;

    // Read response headers
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status_line = status_line.trim();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    let mut resp_headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(colon) = trimmed.find(':') {
            let name = trimmed[..colon].trim().to_lowercase();
            let value = trimmed[colon + 1..].trim().to_string();
            resp_headers.insert(name, value);
        }
    }

    Ok((stream, status, resp_headers))
}

/// Make a blocking HTTP DELETE request.
pub fn http_delete(url: &str, custom_headers: &[(String, String)]) -> io::Result<HttpResponse> {
    let parsed = ParsedHttpUrl::parse(url)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid URL"))?;

    let stream = TcpStream::connect(parsed.addr())?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = stream.try_clone()?;

    let mut req = format!(
        "DELETE {} HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\n",
        parsed.path_and_query, parsed.host_header()
    );
    for (k, v) in custom_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    writer.write_all(req.as_bytes())?;
    writer.flush()?;

    read_http_response(stream)
}

fn read_http_response(stream: TcpStream) -> io::Result<HttpResponse> {
    use std::io::Read;
    let mut reader = BufReader::new(stream);

    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status_line = status_line.trim();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    let mut headers = HashMap::new();
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(colon) = trimmed.find(':') {
            let name = trimmed[..colon].trim().to_lowercase();
            let value = trimmed[colon + 1..].trim().to_string();
            if name == "content-length" {
                content_length = value.parse().ok();
            }
            headers.insert(name, value);
        }
    }

    let body = if let Some(len) = content_length {
        let mut body = vec![0u8; len];
        let _ = reader.read_exact(&mut body);
        body
    } else {
        let mut body = Vec::new();
        let _ = reader.read_to_end(&mut body);
        body
    };

    Ok(HttpResponse { status, headers, body })
}
