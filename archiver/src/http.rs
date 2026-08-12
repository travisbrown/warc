//! Reconstruction of HTTP/1.1 messages for WARC record bodies.
//!
//! The client speaks HTTP/1.1 only, so the messages rendered here match what crossed the wire,
//! with three caveats inherent to reconstructing messages from parsed form: header names are
//! recorded in the lowercased form the HTTP client normalizes them to, response reason phrases
//! are the canonical text for the status code rather than the bytes the server sent, and the
//! framing headers of responses with a body are rewritten because bodies are recorded after
//! de-chunking (`Transfer-Encoding` is dropped and `Content-Length` is rewritten to the
//! recorded body length). Responses that cannot carry a body keep their headers as received.

use reqwest::header::{CONTENT_LENGTH, HeaderMap, TRANSFER_ENCODING};
use reqwest::{StatusCode, Version};
use url::Url;

/// Render the request line and headers of a `GET` request as sent by the client.
///
/// The header order matches the wire: the client's configured headers first, then the `Host`
/// header the HTTP layer appends (verified byte for byte against a live exchange in the
/// integration tests).
pub fn render_request(url: &Url, host: &str, user_agent: &str) -> Vec<u8> {
    let mut message = format!("GET {}", url.path());

    if let Some(query) = url.query() {
        message.push('?');
        message.push_str(query);
    }

    message.push_str(" HTTP/1.1\r\naccept: */*\r\nuser-agent: ");
    message.push_str(user_agent);
    message.push_str("\r\nhost: ");
    message.push_str(host);

    // Non-default ports appear in the `Host` header, matching what the HTTP layer sends.
    if let Some(port) = url.port() {
        message.push(':');
        message.push_str(&port.to_string());
    }

    message.push_str("\r\n\r\n");

    message.into_bytes()
}

/// Render a full HTTP response message from its received parts and de-chunked body.
pub fn render_response(
    version: Version,
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> Vec<u8> {
    // Informational, `204 No Content`, and `304 Not Modified` responses never carry a body, so
    // their headers are preserved exactly as received: a `Content-Length` on a `304` describes
    // the entity that would have been sent, and rewriting it (or fabricating a zero) would
    // misrepresent the exchange.
    let bodiless = matches!(status.as_u16(), 100..=199 | 204 | 304);

    let mut message = Vec::with_capacity(body.len() + 512);

    message.extend_from_slice(version_line(version).as_bytes());
    message.extend_from_slice(b" ");
    message.extend_from_slice(status.as_str().as_bytes());

    // The space after the status code is mandatory even when the reason phrase is empty
    // (RFC 9112: `status-line = HTTP-version SP status-code SP [ reason-phrase ]`), so codes
    // without a canonical reason are rendered with an empty phrase after the space.
    message.extend_from_slice(b" ");

    if let Some(reason) = status.canonical_reason() {
        message.extend_from_slice(reason.as_bytes());
    }

    message.extend_from_slice(b"\r\n");

    for (name, value) in headers {
        if !bodiless && (name == TRANSFER_ENCODING || name == CONTENT_LENGTH) {
            continue;
        }

        message.extend_from_slice(name.as_str().as_bytes());
        message.extend_from_slice(b": ");
        message.extend_from_slice(value.as_bytes());
        message.extend_from_slice(b"\r\n");
    }

    if bodiless {
        message.extend_from_slice(b"\r\n");
    } else {
        message.extend_from_slice(format!("content-length: {}\r\n\r\n", body.len()).as_bytes());
    }

    message.extend_from_slice(body);

    message
}

/// The status line protocol token for an HTTP version.
const fn version_line(version: Version) -> &'static str {
    // `Version` is an opaque type with associated constants rather than an enum, so a catch-all
    // arm is unavoidable; the client only speaks HTTP/1.x.
    match version {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    }
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    #[test]
    fn render_request_includes_query_and_port() -> Result<(), Box<dyn std::error::Error>> {
        let url = Url::parse("http://www.example.com:8080/path?b=2&a=1")?;
        let message = render_request(&url, "www.example.com", "test/1.0");

        assert_eq!(
            message,
            b"GET /path?b=2&a=1 HTTP/1.1\r\n\
              accept: */*\r\n\
              user-agent: test/1.0\r\n\
              host: www.example.com:8080\r\n\r\n"
        );

        Ok(())
    }

    #[test]
    fn render_request_omits_default_port() -> Result<(), Box<dyn std::error::Error>> {
        let url = Url::parse("https://www.example.com/")?;
        let message = render_request(&url, "www.example.com", "test/1.0");

        assert_eq!(
            message,
            b"GET / HTTP/1.1\r\n\
              accept: */*\r\n\
              user-agent: test/1.0\r\n\
              host: www.example.com\r\n\r\n"
        );

        Ok(())
    }

    #[test]
    fn render_response_rewrites_framing_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("text/plain"));
        headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        headers.insert("content-length", HeaderValue::from_static("999"));

        let message = render_response(Version::HTTP_11, StatusCode::OK, &headers, b"hello");

        assert_eq!(
            message,
            b"HTTP/1.1 200 OK\r\n\
              content-type: text/plain\r\n\
              content-length: 5\r\n\r\n\
              hello"
        );
    }

    #[test]
    fn render_response_keeps_the_mandatory_space_without_a_reason_phrase()
    -> Result<(), Box<dyn std::error::Error>> {
        let status = StatusCode::from_u16(520)?;
        let message = render_response(Version::HTTP_11, status, &HeaderMap::new(), b"");

        assert_eq!(message, b"HTTP/1.1 520 \r\ncontent-length: 0\r\n\r\n");

        Ok(())
    }

    #[test]
    fn render_response_preserves_bodiless_response_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("etag", HeaderValue::from_static("\"abc\""));
        headers.insert("content-length", HeaderValue::from_static("42"));

        let message = render_response(Version::HTTP_11, StatusCode::NOT_MODIFIED, &headers, b"");

        assert_eq!(
            message,
            b"HTTP/1.1 304 Not Modified\r\n\
              etag: \"abc\"\r\n\
              content-length: 42\r\n\r\n"
        );
    }
}
