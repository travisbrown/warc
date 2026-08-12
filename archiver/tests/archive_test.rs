//! End-to-end archiving tests against a local HTTP server serving canned responses.

use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use libflate::gzip;
use warc::{RecordType, WarcHeader};
use warc_archiver::client::{Archiver, Error};
use warc_archiver::config::{Config, IndexFormat};
use warc_wacz::cdxj;
use warc_wacz::digest::Sha256Digest;
use warc_wacz::reader::WaczReader;

/// A simple HTTP/1.1 response with a text body.
fn plain(status: &str, headers: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\n{headers}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A canned HTTP/1.1 response for a request path.
fn respond(path: &str) -> Vec<u8> {
    // Redirects to an address that refuses connections carry the target port in the path.
    if let Some(port) = path.strip_prefix("/dead/") {
        return plain(
            "302 Found",
            &format!("location: http://127.0.0.1:{port}/"),
            "",
        );
    }

    match path {
        "/" => plain("200 OK", "content-type: text/html", "<html>home</html>"),
        "/redirect" => plain(
            "302 Found",
            "content-type: text/plain\r\nlocation: /target",
            "",
        ),
        "/target" => plain(
            "200 OK",
            "content-type: text/plain; charset=utf-8",
            "arrived",
        ),
        "/loop" => plain(
            "302 Found",
            "content-type: text/plain\r\nlocation: /loop",
            "",
        ),
        "/bad-target" => plain(
            "302 Found",
            "content-type: text/plain\r\nlocation: ftp://127.0.0.1/file",
            "",
        ),
        "/multiple-choices" => plain(
            "300 Multiple Choices",
            "content-type: text/plain\r\nlocation: /target",
            "list",
        ),
        "/nonstandard" => plain("520 Origin Error", "content-type: text/plain", "err"),
        "/cookies" => plain(
            "200 OK",
            "content-type: text/plain\r\nset-cookie: a=1\r\nset-cookie: b=2",
            "ok",
        ),
        "/slow" => {
            thread::sleep(Duration::from_millis(500));
            plain("200 OK", "content-type: text/plain", "late")
        }
        // A chunked body, so that de-chunking is exercised against a real wire exchange.
        "/chunked" => b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
                        transfer-encoding: chunked\r\nconnection: close\r\n\r\n\
                        6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n"
            .to_vec(),
        // A bodiless response whose headers describe the entity that was not sent.
        "/not-modified" => b"HTTP/1.1 304 Not Modified\r\netag: \"abc\"\r\n\
                             content-length: 42\r\nlocation: /target\r\n\
                             connection: close\r\n\r\n"
            .to_vec(),
        "/binary" => {
            let body = (0u8..=255).collect::<Vec<_>>();
            let mut response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(&body);
            response
        }
        _ => plain("404 Not Found", "content-type: text/plain", "gone"),
    }
}

/// Serve the given number of connections on an ephemeral local port, returning the raw bytes of
/// each request as received.
fn serve(connections: usize) -> std::io::Result<(u16, thread::JoinHandle<Vec<Vec<u8>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    let handle = thread::spawn(move || {
        let mut requests = Vec::with_capacity(connections);

        for _ in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                return requests;
            };

            let mut head = Vec::new();
            let mut buffer = [0; 4096];

            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => head.extend_from_slice(&buffer[..read]),
                }
            }

            let request = String::from_utf8_lossy(&head);
            let path = request.split(' ').nth(1).unwrap_or("/").to_owned();
            let _ = stream.write_all(&respond(&path));
            requests.push(head);
        }

        requests
    });

    Ok((port, handle))
}

#[test]
fn archive_and_read_back() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(4)?;
    let urls = [
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/redirect"),
        format!("http://127.0.0.1:{port}/missing"),
    ];

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive(&urls, Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(
        summary
            .captures
            .iter()
            .map(|capture| (capture.status, capture.redirects))
            .collect::<Vec<_>>(),
        vec![(200, 0), (200, 1), (404, 0)]
    );

    let mut reader = WaczReader::new(Cursor::new(&bytes))?;

    assert!(reader.verify()?.is_success());

    let package = reader.data_package()?;

    assert_eq!(package.main_page_url.as_deref(), Some(urls[0].as_str()));

    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        pages
            .iter()
            .map(|page| page.url.as_ref())
            .collect::<Vec<_>>(),
        urls.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(pages[1].size, Some("arrived".len() as u64));
    // Every page receives a synthetic id of the default 24-character length.
    assert!(
        pages
            .iter()
            .all(|page| page.id.as_deref().is_some_and(|id| id.len() == 24))
    );

    // One warcinfo record plus a response and request record for each of the four exchanges.
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(records.len(), 9);
    assert_eq!(records[0].warc_type(), &RecordType::WarcInfo);

    // WARC 1.1 compliance of every emitted record: version, bracketed record ids, and target
    // URIs written bare (the 1.1 change from the bracketed 1.0 form).
    for record in &records {
        assert_eq!(record.warc_version(), "1.1");
        assert!(record.warc_id().starts_with('<') && record.warc_id().ends_with('>'));
        if let Some(target) = record.header(WarcHeader::TargetURI) {
            assert!(!target.starts_with('<'));
        }
    }

    // The warcinfo record carries its recommended fields and none of its prohibited ones.
    let warcinfo = &records[0];
    assert_eq!(
        warcinfo.header(WarcHeader::ContentType).as_deref(),
        Some("application/warc-fields")
    );
    assert_eq!(
        warcinfo.header(WarcHeader::Filename).as_deref(),
        Some("data.warc.gz")
    );
    assert!(warcinfo.header(WarcHeader::TargetURI).is_none());

    let response = &records[1];
    let request = &records[2];

    assert_eq!(response.warc_type(), &RecordType::Response);
    assert_eq!(
        response.header(WarcHeader::TargetURI).as_deref(),
        Some(urls[0].as_str())
    );
    assert!(response.body().ends_with(b"<html>home</html>"));

    assert_eq!(
        response.header(WarcHeader::ContentType).as_deref(),
        Some("application/http;msgtype=response")
    );
    assert!(
        response
            .header(WarcHeader::PayloadDigest)
            .is_some_and(|digest| digest.starts_with("sha256:"))
    );
    assert_eq!(
        response.header(WarcHeader::IPAddress).as_deref(),
        Some("127.0.0.1")
    );
    assert_eq!(
        response.header(WarcHeader::WarcInfoID).as_deref(),
        Some(warcinfo.warc_id())
    );

    assert_eq!(request.warc_type(), &RecordType::Request);
    assert_eq!(
        request.header(WarcHeader::ConcurrentTo).as_deref(),
        Some(response.warc_id())
    );
    assert_eq!(
        request.header(WarcHeader::ContentType).as_deref(),
        Some("application/http;msgtype=request")
    );
    // Records of one capture event share a single WARC-Date.
    assert_eq!(
        response.header(WarcHeader::Date),
        request.header(WarcHeader::Date)
    );

    let request_message = String::from_utf8(request.body().to_vec())?;

    assert!(request_message.starts_with("GET / HTTP/1.1\r\n"));
    assert!(request_message.contains(&format!("host: 127.0.0.1:{port}\r\n")));
    assert!(request_message.contains("user-agent: warc-archiver/"));

    // The redirect chain is recorded hop by hop.
    assert_eq!(
        records[3].header(WarcHeader::TargetURI).as_deref(),
        Some(urls[1].as_str())
    );
    assert_eq!(
        records[5].header(WarcHeader::TargetURI).as_deref(),
        Some(format!("http://127.0.0.1:{port}/target").as_str())
    );

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 4);
    assert!(items.is_sorted_by_key(|item| item.key.clone()));
    assert_eq!(
        items
            .iter()
            .find(|item| item.fields.url == urls[2])
            .and_then(|item| item.fields.status),
        Some(404)
    );

    // Every index entry's offset and length must frame exactly one complete gzip member,
    // decompressible on its own, holding one parseable response record.
    let mut warc_bytes = Vec::new();
    zip::ZipArchive::new(Cursor::new(&bytes))?
        .by_name("archive/data.warc.gz")?
        .read_to_end(&mut warc_bytes)?;

    for item in &items {
        assert_eq!(item.fields.filename.as_deref(), Some("data.warc.gz"));

        let offset = usize::try_from(item.fields.offset.expect("offset should be indexed"))?;
        let length = usize::try_from(item.fields.length.expect("length should be indexed"))?;

        // The record digest covers exactly the framed range of stored (compressed) bytes.
        assert_eq!(
            item.fields.record_digest,
            Some(Sha256Digest::compute(&warc_bytes[offset..offset + length]))
        );

        let mut decompressed = Vec::new();
        gzip::Decoder::new(&warc_bytes[offset..offset + length])?.read_to_end(&mut decompressed)?;

        let framed = warc::WarcReader::new(decompressed.as_slice())
            .iter_records()
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(framed.len(), 1);
        assert_eq!(framed[0].warc_type(), &RecordType::Response);
        assert_eq!(
            framed[0].header(WarcHeader::TargetURI).as_deref(),
            Some(item.fields.url.as_ref())
        );
    }

    Ok(())
}

#[test]
fn archive_with_plain_warc_member() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/");

    let archiver = Archiver::new(Config {
        gzip_warc: false,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let mut reader = WaczReader::new(Cursor::new(&bytes))?;

    assert!(reader.verify()?.is_success());

    let records = reader
        .warc("archive/data.warc")?
        .iter_records()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(records.len(), 3);

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].fields.filename.as_deref(), Some("data.warc"));

    // The offset and length frame the uncompressed response record directly.
    let mut warc_bytes = Vec::new();
    zip::ZipArchive::new(Cursor::new(&bytes))?
        .by_name("archive/data.warc")?
        .read_to_end(&mut warc_bytes)?;

    let offset = usize::try_from(items[0].fields.offset.expect("offset should be indexed"))?;
    let length = usize::try_from(items[0].fields.length.expect("length should be indexed"))?;

    // The record digest covers exactly the framed range of stored (plain) bytes.
    assert_eq!(
        items[0].fields.record_digest,
        Some(Sha256Digest::compute(&warc_bytes[offset..offset + length]))
    );

    let framed = warc::WarcReader::new(&warc_bytes[offset..offset + length])
        .iter_records()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(framed.len(), 1);
    assert_eq!(framed[0].warc_type(), &RecordType::Response);

    Ok(())
}

#[test]
fn archive_with_compressed_index() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/");

    let archiver = Archiver::new(Config {
        index_format: IndexFormat::zipnum(),
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let mut reader = WaczReader::new(Cursor::new(&bytes))?;

    assert!(reader.verify()?.is_success());

    // The compressed data member holds the full index; the summary locates its single block.
    let items = reader
        .index("indexes/index.cdx.gz")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].fields.url.as_ref(), url);

    let mut summary_member = String::new();
    zip::ZipArchive::new(Cursor::new(&bytes))?
        .by_name("indexes/index.idx")?
        .read_to_string(&mut summary_member)?;

    let lines = summary_member.lines().collect::<Vec<_>>();

    assert_eq!(
        lines[0],
        "!meta 0 {\"format\": \"cdxj-gzip-1.0\", \"filename\": \"index.cdx.gz\"}"
    );
    assert_eq!(lines.len(), 2);
    assert!(lines[1].starts_with(&format!("{} ", items[0].key)));

    Ok(())
}

#[test]
fn archive_records_unreachable_urls_as_failures() -> Result<(), Box<dyn std::error::Error>> {
    // Bind and immediately drop a listener so that the port refuses connections.
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;

    assert!(!summary.is_complete());
    assert!(summary.captures.is_empty());
    assert_eq!(summary.failures.len(), 1);
    assert_eq!(summary.failures[0].url, url);

    // The collection is still written and internally consistent.
    let mut reader = WaczReader::new(Cursor::new(&bytes))?;

    assert!(reader.verify()?.is_success());
    assert_eq!(reader.pages()?.count(), 0);

    Ok(())
}

#[test]
fn archive_stops_following_at_the_redirect_limit() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/redirect");

    let archiver = Archiver::new(Config {
        max_redirects: 0,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 302);
    assert_eq!(summary.captures[0].redirects, 0);

    let mut reader = WaczReader::new(Cursor::new(&bytes))?;
    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, cdxj::search_key(&url)?);

    Ok(())
}

#[test]
fn archive_to_path_refuses_an_existing_output() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("test.wacz");
    std::fs::write(&path, b"existing")?;

    let archiver = Archiver::new(Config::default())?;

    assert!(archiver.archive_to_path::<_, _, &str>([], &path).is_err());

    Ok(())
}

#[test]
fn archive_to_path_writes_a_collection() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("test.wacz");

    let archiver = Archiver::new(Config::default())?;
    let summary = archiver.archive_to_path([format!("http://127.0.0.1:{port}/")], &path)?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let mut reader = WaczReader::new(std::fs::File::open(&path)?)?;

    assert!(reader.verify()?.is_success());

    Ok(())
}

#[test]
fn recorded_request_matches_the_wire_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/");

    let archiver = Archiver::new(Config {
        user_agent: "fidelity-test/1.0".into(),
        gzip_warc: false,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    let requests = server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let mut reader = WaczReader::new(Cursor::new(&bytes))?;
    let records = reader
        .warc("archive/data.warc")?
        .iter_records()
        .collect::<Result<Vec<_>, _>>()?;

    // The request record replays the received request byte for byte.
    assert_eq!(records[2].warc_type(), &RecordType::Request);
    assert_eq!(records[2].body(), requests[0].as_slice());

    Ok(())
}

#[test]
fn archive_records_chunked_responses_dechunked() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/chunked");

    let archiver = Archiver::new(Config {
        gzip_warc: false,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].size, "hello world".len() as u64);

    let mut reader = WaczReader::new(Cursor::new(&bytes))?;
    let records = reader
        .warc("archive/data.warc")?
        .iter_records()
        .collect::<Result<Vec<_>, _>>()?;

    let message = String::from_utf8(records[1].body().to_vec())?;

    assert!(message.ends_with("\r\n\r\nhello world"));
    assert!(message.contains("content-length: 11\r\n"));
    assert!(!message.contains("transfer-encoding"));

    Ok(())
}

#[test]
fn archive_rejects_credentialed_urls_without_leaking_the_secret()
-> Result<(), Box<dyn std::error::Error>> {
    // Nothing listens on the port: the URL is rejected before any request is made.
    let url = "http://user:secret@127.0.0.1:9/";

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([url], Cursor::new(&mut bytes))?;

    assert!(!summary.is_complete());
    assert!(matches!(
        summary.failures[0].error,
        Error::CredentialedUrl(_)
    ));
    assert!(!summary.failures[0].error.to_string().contains("secret"));
    assert!(!summary.failures[0].error.to_string().contains("user"));

    Ok(())
}

#[test]
fn archive_records_hops_captured_before_a_failure() -> Result<(), Box<dyn std::error::Error>> {
    // Bind and immediately drop a listener so that the redirect target refuses connections.
    let dead_port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/dead/{dead_port}");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(!summary.is_complete());
    assert!(summary.captures.is_empty());
    assert_eq!(summary.failures[0].url, url);

    let mut reader = WaczReader::new(Cursor::new(&bytes))?;

    // The URL failed, so it gets no page entry, but the hop captured before the failure is
    // still recorded and indexed.
    assert_eq!(reader.pages()?.count(), 0);

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].fields.status, Some(302));

    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(records.len(), 3);
    assert_eq!(records[1].warc_type(), &RecordType::Response);

    Ok(())
}

#[test]
fn archive_treats_multiple_choices_and_not_modified_as_final()
-> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(2)?;
    let urls = [
        format!("http://127.0.0.1:{port}/multiple-choices"),
        format!("http://127.0.0.1:{port}/not-modified"),
    ];

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive(&urls, Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    // Neither response is followed, despite the redirection-class status and location header.
    assert!(summary.is_complete());
    assert_eq!(
        summary
            .captures
            .iter()
            .map(|capture| (capture.status, capture.redirects))
            .collect::<Vec<_>>(),
        vec![(300, 0), (304, 0)]
    );

    let mut reader = WaczReader::new(Cursor::new(&bytes))?;
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records()
        .collect::<Result<Vec<_>, _>>()?;

    // The bodiless 304 keeps its headers exactly as received, with no fabricated zero
    // content-length replacing the one describing the entity that was not sent.
    let message = String::from_utf8(records[3].body().to_vec())?;

    assert!(message.starts_with("HTTP/1.1 304 Not Modified\r\n"));
    assert!(message.contains("content-length: 42\r\n"));
    assert!(!message.contains("content-length: 0"));
    assert!(message.ends_with("\r\n\r\n"));

    Ok(())
}

#[test]
fn archive_renders_a_status_without_a_canonical_reason() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/nonstandard");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 520);

    let mut reader = WaczReader::new(Cursor::new(&bytes))?;
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records()
        .collect::<Result<Vec<_>, _>>()?;

    // The mandatory space after the status code is kept even with an empty reason phrase.
    let message = String::from_utf8(records[1].body().to_vec())?;

    assert!(message.starts_with("HTTP/1.1 520 \r\n"));

    Ok(())
}

#[test]
fn archive_preserves_repeated_set_cookie_headers() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/cookies");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let mut reader = WaczReader::new(Cursor::new(&bytes))?;
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records()
        .collect::<Result<Vec<_>, _>>()?;

    let message = String::from_utf8(records[1].body().to_vec())?;

    assert!(message.contains("set-cookie: a=1\r\n"));
    assert!(message.contains("set-cookie: b=2\r\n"));

    Ok(())
}

#[test]
fn archive_records_binary_bodies() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/binary");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].size, 256);

    let body = (0u8..=255).collect::<Vec<_>>();
    let mut reader = WaczReader::new(Cursor::new(&bytes))?;
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records()
        .collect::<Result<Vec<_>, _>>()?;

    assert!(records[1].body().ends_with(&body));
    assert_eq!(
        records[1].header(WarcHeader::PayloadDigest).as_deref(),
        Some(Sha256Digest::compute(&body).to_string().as_str())
    );

    Ok(())
}

#[test]
fn archive_records_timeouts_as_failures() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/slow");

    let archiver = Archiver::new(Config {
        timeout: Duration::from_millis(100),
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(!summary.is_complete());
    assert!(matches!(summary.failures[0].error, Error::Http(_)));

    Ok(())
}

#[test]
fn archive_stops_following_a_redirect_cycle() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(3)?;
    let url = format!("http://127.0.0.1:{port}/loop");

    let archiver = Archiver::new(Config {
        max_redirects: 2,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 302);
    assert_eq!(summary.captures[0].redirects, 2);

    let mut reader = WaczReader::new(Cursor::new(&bytes))?;
    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 3);

    Ok(())
}

#[test]
fn archive_records_an_unusable_redirect_target_as_final() -> Result<(), Box<dyn std::error::Error>>
{
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/bad-target");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 302);
    assert_eq!(summary.captures[0].redirects, 0);

    Ok(())
}

#[test]
fn archive_records_urls_without_a_host_as_failures() -> Result<(), Box<dyn std::error::Error>> {
    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive(["data:text/plain,hi"], Cursor::new(&mut bytes))?;

    assert!(!summary.is_complete());
    assert!(matches!(summary.failures[0].error, Error::MissingHost(_)));

    Ok(())
}

#[test]
fn new_rejects_an_invalid_user_agent() {
    let result = Archiver::new(Config {
        user_agent: "bad\r\nagent".into(),
        ..Config::default()
    });

    assert!(matches!(result, Err(Error::InvalidUserAgent(_))));
}

#[test]
fn archive_concurrently_preserves_input_order() -> Result<(), Box<dyn std::error::Error>> {
    let paths = [
        "/",
        "/target",
        "/missing",
        "/cookies",
        "/",
        "/nonstandard",
        "/target",
        "/",
    ];
    let (port, server) = serve(paths.len())?;
    let urls = paths
        .iter()
        .map(|path| format!("http://127.0.0.1:{port}{path}"))
        .collect::<Vec<_>>();

    let archiver = Archiver::new(Config {
        concurrency: 4,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive(&urls, Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(
        summary
            .captures
            .iter()
            .map(|capture| capture.url.as_str())
            .collect::<Vec<_>>(),
        urls.iter().map(String::as_str).collect::<Vec<_>>()
    );

    let mut reader = WaczReader::new(Cursor::new(&bytes))?;

    assert!(reader.verify()?.is_success());

    // Page entries follow input order, exactly as in a sequential run.
    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        pages
            .iter()
            .map(|page| page.url.as_ref())
            .collect::<Vec<_>>(),
        urls.iter().map(String::as_str).collect::<Vec<_>>()
    );

    Ok(())
}
