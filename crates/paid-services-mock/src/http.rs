//! Minimal HTTP/1.1 framing for the mock node and its control client.
//!
//! This duplicates the sidecar's reader on purpose: the sidecar must not depend on the mock,
//! and the mock must not depend on the sidecar, whose tests use the mock.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_CONTROL_BODY_BYTES: usize = 1024 * 1024;

/// One HTTP/1.1 message.
pub struct HttpMessage {
    /// The request or status line.
    pub start_line: String,
    /// Header names and values.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl HttpMessage {
    /// The first header with this name, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Reads one HTTP/1.1 message with a `Content-Length` body, within fixed bounds.
pub fn read_http_message(stream: &mut impl Read, max_body: usize) -> io::Result<HttpMessage> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(invalid("HTTP header is too large"));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed inside the HTTP header",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head =
        std::str::from_utf8(&buf[..header_end]).map_err(|_| invalid("header is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let start_line = lines.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for line in lines {
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| invalid(format!("bad header line {line:?}")))?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let message = HttpMessage {
        start_line,
        headers,
        body: Vec::new(),
    };
    let len: usize = message
        .header("content-length")
        .ok_or_else(|| invalid("missing Content-Length"))?
        .parse()
        .map_err(|_| invalid("bad Content-Length"))?;
    if len > max_body {
        return Err(invalid("HTTP body is too large"));
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed inside the HTTP body",
            ));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    if body.len() != len {
        return Err(invalid("body length differs from Content-Length"));
    }
    Ok(HttpMessage { body, ..message })
}

/// Writes an HTTP/1.1 response with a JSON body.
pub fn write_http_response(
    stream: &mut impl Write,
    status: u16,
    reason: &str,
    body: &[u8],
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Calls a method on a mock node over loopback with its cookie, for the control commands.
pub fn call(addr: SocketAddr, cookie: &str, method: &str, params: Value) -> Result<Value, String> {
    if !addr.ip().is_loopback() {
        return Err(format!("{addr} is not a loopback address"));
    }
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let timeout = Duration::from_secs(10);
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Basic {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        STANDARD.encode(cookie.trim()),
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.write_all(body.as_bytes()))
        .map_err(|e| e.to_string())?;
    let message =
        read_http_message(&mut stream, MAX_CONTROL_BODY_BYTES).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_slice(&message.body).map_err(|e| e.to_string())?;
    if let Some(err) = value.get("error").filter(|e| !e.is_null()) {
        return Err(format!("node error: {err}"));
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| format!("unexpected answer: {value}"))
}
