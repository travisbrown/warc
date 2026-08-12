//! End-to-end archiving tests against a local HTTP server serving canned responses.

use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::thread;

use libflate::gzip;
use warc::{RecordType, WarcHeader};
use warc_archiver::client::Archiver;
use warc_archiver::config::Config;
use warc_wacz::cdxj;
use warc_wacz::reader::WaczReader;
use warc_wacz::writer::IndexFormat;

/// A canned HTTP/1.1 response for a request path.
fn respond(path: &str) -> Vec<u8> {
    let (status, headers, body) = match path {
        "/" => ("200 OK", "content-type: text/html", "<html>home</html>"),
        "/redirect" => (
            "302 Found",
            "content-type: text/plain\r\nlocation: /target",
            "",
        ),
        "/target" => (
            "200 OK",
            "content-type: text/plain; charset=utf-8",
            "arrived",
        ),
        _ => ("404 Not Found", "content-type: text/plain", "gone"),
    };

    format!(
        "HTTP/1.1 {status}\r\n{headers}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Serve the given number of connections on an ephemeral local port.
fn serve(connections: usize) -> std::io::Result<(u16, thread::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    let handle = thread::spawn(move || {
        for _ in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
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
        }
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

    let response = &records[1];
    let request = &records[2];

    assert_eq!(response.warc_type(), &RecordType::Response);
    assert_eq!(
        response.header(WarcHeader::TargetURI).as_deref(),
        Some(urls[0].as_str())
    );
    assert!(response.body().ends_with(b"<html>home</html>"));

    assert_eq!(request.warc_type(), &RecordType::Request);
    assert_eq!(
        request.header(WarcHeader::ConcurrentTo).as_deref(),
        Some(response.warc_id())
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
