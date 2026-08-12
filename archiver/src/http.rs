//! Reconstruction of HTTP/1.1 messages for WARC record bodies.
//!
//! The client speaks HTTP/1.1 only, so the messages rendered here match what crossed the wire:
//! requests list exactly the headers the client sends, and responses replay the received status
//! line and headers. The only adjustments are to the framing headers of responses, since bodies
//! are recorded after de-chunking: `Transfer-Encoding` is dropped and `Content-Length` is
//! rewritten to the recorded body length.

use reqwest::header::{CONTENT_LENGTH, HeaderMap, TRANSFER_ENCODING};
use reqwest::{StatusCode, Version};
use url::Url;

/// Render the request line and headers of a `GET` request as sent by the client.
pub fn render_request(url: &Url, host: &str, user_agent: &str) -> Vec<u8> {
    let mut message = format!("GET {}", url.path());

    if let Some(query) = url.query() {
        message.push('?');
        message.push_str(query);
    }

    message.push_str(" HTTP/1.1\r\nhost: ");
    message.push_str(host);

    // Non-default ports appear in the `Host` header, matching what the HTTP layer sends.
    if let Some(port) = url.port() {
        message.push(':');
        message.push_str(&port.to_string());
    }

    message.push_str("\r\nuser-agent: ");
    message.push_str(user_agent);
    message.push_str("\r\naccept: */*\r\n\r\n");

    message.into_bytes()
}

/// Render a full HTTP response message from its received parts and de-chunked body.
pub fn render_response(
    version: Version,
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> Vec<u8> {
    let mut message = Vec::with_capacity(body.len() + 512);

    message.extend_from_slice(version_line(version).as_bytes());
    message.extend_from_slice(b" ");
    message.extend_from_slice(status.as_str().as_bytes());

    if let Some(reason) = status.canonical_reason() {
        message.extend_from_slice(b" ");
        message.extend_from_slice(reason.as_bytes());
    }

    message.extend_from_slice(b"\r\n");

    for (name, value) in headers {
        if name == TRANSFER_ENCODING || name == CONTENT_LENGTH {
            continue;
        }

        message.extend_from_slice(name.as_str().as_bytes());
        message.extend_from_slice(b": ");
        message.extend_from_slice(value.as_bytes());
        message.extend_from_slice(b"\r\n");
    }

    message.extend_from_slice(format!("content-length: {}\r\n\r\n", body.len()).as_bytes());
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
              host: www.example.com:8080\r\n\
              user-agent: test/1.0\r\n\
              accept: */*\r\n\r\n"
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
              host: www.example.com\r\n\
              user-agent: test/1.0\r\n\
              accept: */*\r\n\r\n"
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
}
