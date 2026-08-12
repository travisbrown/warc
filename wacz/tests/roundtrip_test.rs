//! A round trip through the WACZ writer and reader, covering plain and gzip WARC members.

use std::borrow::Cow;
use std::io::{Cursor, Read, Write};

use chrono::{TimeZone, Utc};
use libflate::gzip;
use warc::{RecordBuilder, RecordType, WarcHeader, WarcWriter};
use warc_wacz::cdxj;
use warc_wacz::digest::Sha256Digest;
use warc_wacz::pages;
use warc_wacz::pages::{Page, PageListHeader};
use warc_wacz::reader::WaczReader;
use warc_wacz::writer::{IndexFormat, PackageMetadata, WaczWriter, WriterConfig};

const URL: &str = "https://www.example.com/page";
const BODY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html>hello</html>";

/// Build the serialized bytes of a single-record WARC file for the test capture.
fn warc_bytes() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let record = RecordBuilder::default()
        .warc_type(RecordType::Response)
        .header(WarcHeader::TargetURI, URL)
        .body(BODY.to_vec())
        .build()?;

    let mut bytes = Vec::new();
    let mut writer = WarcWriter::new(&mut bytes);
    writer.write(&record)?;

    Ok(bytes)
}

/// Build a WACZ file in memory containing one WARC member, one index, and one page.
fn build_wacz(warc_name: &str, warc_data: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    writer.add_warc(warc_name, warc_data)?;

    let capture_time = Utc.with_ymd_and_hms(2020, 10, 7, 21, 22, 36).unwrap();

    let item = cdxj::Item {
        key: Cow::Owned(cdxj::search_key(URL)?),
        timestamp: capture_time.into(),
        fields: cdxj::Fields {
            url: Cow::Borrowed(URL),
            digest: None,
            mime: Some(Cow::Borrowed("text/html")),
            status: Some(200),
            offset: Some(0),
            length: Some(warc_data.len() as u64),
            filename: Some(Cow::Borrowed(warc_name)),
            record_digest: None,
            extra: serde_json::Map::new(),
        },
    };

    writer.add_index("index.cdx", [&item])?;

    let page = Page {
        url: Cow::Borrowed(URL),
        ts: capture_time,
        id: Some(Cow::Borrowed("1db0ef709a")),
        title: Some(Cow::Borrowed("Example Domain")),
        text: None,
        size: Some(BODY.len() as u64),
        extra: serde_json::Map::new(),
    };

    writer.add_pages(&PageListHeader::default(), [&page])?;

    let metadata = PackageMetadata {
        title: Some("Test collection".to_owned()),
        main_page_url: Some(URL.to_owned()),
        main_page_date: Some(capture_time),
        ..PackageMetadata::default()
    };

    Ok(writer.finish(metadata)?.into_inner())
}

/// Assert that a built WACZ file round trips through the reader.
fn assert_round_trip(warc_name: &str, warc_data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let wacz = build_wacz(warc_name, warc_data)?;
    let mut reader = WaczReader::new(Cursor::new(wacz))?;

    let package = reader.data_package()?;
    let warc_path = format!("archive/{warc_name}");

    assert_eq!(package.wacz_version, "1.1.1");
    assert_eq!(package.title.as_deref(), Some("Test collection"));
    assert_eq!(package.main_page_url.as_deref(), Some(URL));
    assert!(package.created.is_some());
    assert_eq!(package.resources.len(), 3);
    assert!(
        package
            .resources
            .iter()
            .any(|resource| resource.path == warc_path)
    );

    let digest = reader
        .data_package_digest()?
        .expect("digest file should be present");

    assert_eq!(digest.path, "datapackage.json");

    assert_eq!(
        reader.warc_paths().collect::<Vec<_>>(),
        vec![warc_path.clone()]
    );
    assert_eq!(
        reader.index_paths().collect::<Vec<_>>(),
        vec!["indexes/index.cdx"]
    );

    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].url, URL);
    assert_eq!(pages[0].title.as_deref(), Some("Example Domain"));

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "com,example,www)/page");
    assert_eq!(items[0].fields.filename.as_deref(), Some(warc_name));

    let records = reader
        .warc(&warc_path)?
        .iter_records()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].body(), BODY);
    assert_eq!(
        records[0].header(WarcHeader::TargetURI).as_deref(),
        Some(URL)
    );

    let verification = reader.verify()?;

    assert!(verification.is_success());
    // The three resources plus the manifest itself, which is covered by the digest file.
    assert_eq!(verification.verified.len(), 4);

    Ok(())
}

#[test]
fn round_trip_with_plain_warc_member() -> Result<(), Box<dyn std::error::Error>> {
    assert_round_trip("data.warc", &warc_bytes()?)
}

#[test]
fn round_trip_with_gzip_warc_member() -> Result<(), Box<dyn std::error::Error>> {
    let mut encoder = gzip::Encoder::new(Vec::new())?;
    encoder.write_all(&warc_bytes()?)?;
    let compressed = encoder.finish().into_result()?;

    assert_round_trip("data.warc.gz", &compressed)
}

/// Pages written without an identifier receive a synthetic one of the configured length;
/// explicitly supplied identifiers are preserved.
#[test]
fn synthetic_page_ids() -> Result<(), Box<dyn std::error::Error>> {
    let capture_time = Utc.with_ymd_and_hms(2020, 10, 7, 21, 22, 36).unwrap();
    let with_id = Page {
        url: Cow::Borrowed(URL),
        ts: capture_time,
        id: Some(Cow::Borrowed("explicit-id")),
        title: None,
        text: None,
        size: None,
        extra: serde_json::Map::new(),
    };
    let without_id = Page {
        url: Cow::Borrowed("https://www.example.com/other"),
        id: None,
        ..with_id.clone()
    };

    for (length, config) in [
        (24, WriterConfig::default()),
        (
            16,
            WriterConfig {
                page_id_length: 16,
                ..WriterConfig::default()
            },
        ),
    ] {
        let mut writer = WaczWriter::with_config(Cursor::new(Vec::new()), config);
        writer.add_pages(&PageListHeader::default(), [&with_id, &without_id])?;
        let wacz = writer.finish(PackageMetadata::default())?.into_inner();

        let mut reader = WaczReader::new(Cursor::new(wacz))?;
        let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;

        assert_eq!(pages[0].id.as_deref(), Some("explicit-id"));
        assert_eq!(
            pages[1].id.as_deref(),
            Some(pages::synthetic_id(&capture_time, &pages[1].url, length).as_str())
        );
        assert_eq!(pages[1].id.as_deref().map(str::len), Some(length));
    }

    Ok(())
}

/// A `ZipNum` index following the `py-wacz` layout: `index.cdx.gz` holds independent gzip members
/// of at most `lines` CDX lines each, and `index.idx` locates every block by offset, length,
/// and digest behind a `!meta` header line.
#[test]
fn zipnum_index() -> Result<(), Box<dyn std::error::Error>> {
    let capture_time = Utc.with_ymd_and_hms(2020, 10, 7, 21, 22, 36).unwrap();

    // Five items across a two-line block size: blocks of 2, 2, and 1 lines.
    let urls = (0..5)
        .map(|i| format!("https://www.example.com/page{i}"))
        .collect::<Vec<_>>();
    let items = urls
        .iter()
        .map(|url| {
            Ok(cdxj::Item {
                key: Cow::Owned(cdxj::search_key(url)?),
                timestamp: capture_time.into(),
                fields: cdxj::Fields {
                    url: Cow::Borrowed(url),
                    digest: None,
                    mime: Some(Cow::Borrowed("text/html")),
                    status: Some(200),
                    offset: Some(0),
                    length: Some(10),
                    filename: Some(Cow::Borrowed("data.warc.gz")),
                    record_digest: None,
                    extra: serde_json::Map::new(),
                },
            })
        })
        .collect::<Result<Vec<_>, cdxj::Error>>()?;

    let config = WriterConfig {
        index_format: IndexFormat::ZipNum { lines: 2 },
        ..WriterConfig::default()
    };
    let mut writer = WaczWriter::with_config(Cursor::new(Vec::new()), config);
    writer.add_index("index.cdx", &items)?;
    let wacz = writer.finish(PackageMetadata::default())?.into_inner();

    // The gzip data member is stored, the plain-text summary deflated, per the specification.
    let mut archive = zip::ZipArchive::new(Cursor::new(&wacz))?;
    assert_eq!(
        archive.by_name("indexes/index.cdx.gz")?.compression(),
        zip::CompressionMethod::Stored
    );
    assert_eq!(
        archive.by_name("indexes/index.idx")?.compression(),
        zip::CompressionMethod::Deflated
    );

    let mut summary = String::new();
    archive
        .by_name("indexes/index.idx")?
        .read_to_string(&mut summary)?;
    let mut data = Vec::new();
    archive
        .by_name("indexes/index.cdx.gz")?
        .read_to_end(&mut data)?;

    let summary_lines = summary.lines().collect::<Vec<_>>();

    assert_eq!(
        summary_lines[0],
        "!meta 0 {\"format\": \"cdxj-gzip-1.0\", \"filename\": \"index.cdx.gz\"}"
    );
    assert_eq!(summary_lines.len(), 4);

    // Each summary line locates a complete, independently decompressible gzip member.
    let mut expected_offset = 0;
    let mut block_line_counts = Vec::new();

    for line in &summary_lines[1..] {
        let brace = line.find('{').expect("summary line should hold JSON");
        let value = serde_json::from_str::<serde_json::Value>(&line[brace..])?;

        let offset = usize::try_from(value["offset"].as_u64().expect("offset"))?;
        let length = usize::try_from(value["length"].as_u64().expect("length"))?;
        assert_eq!(offset, expected_offset);
        expected_offset += length;

        let block = &data[offset..offset + length];
        assert_eq!(
            value["digest"].as_str().expect("digest"),
            Sha256Digest::compute(block).to_string()
        );

        let mut decoded = String::new();
        gzip::Decoder::new(block)?.read_to_string(&mut decoded)?;
        block_line_counts.push(decoded.lines().count());

        // The prefix is the search key and timestamp of the block's first line.
        assert!(decoded.starts_with(line[..brace].trim_end()));
    }

    assert_eq!(expected_offset, data.len());
    assert_eq!(block_line_counts, vec![2, 2, 1]);

    // The data member reads back as the full sorted index, and the manifest verifies.
    let mut reader = WaczReader::new(Cursor::new(&wacz))?;
    let read_items = reader
        .index("indexes/index.cdx.gz")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(read_items.len(), items.len());
    assert!(read_items.is_sorted_by_key(|item| item.key.clone()));
    assert!(reader.verify()?.is_success());

    Ok(())
}

/// The specification requires the `STORE` method for all `archive/` members and permits
/// `DEFLATE` only for plain-text members.
#[test]
fn spec_compression_methods() -> Result<(), Box<dyn std::error::Error>> {
    use zip::CompressionMethod;

    let expectations = [
        ("indexes/index.cdx", CompressionMethod::Deflated),
        ("pages/pages.jsonl", CompressionMethod::Deflated),
        ("datapackage.json", CompressionMethod::Deflated),
        ("datapackage-digest.json", CompressionMethod::Deflated),
    ];

    // A plain WARC member must be stored, not just a gzip one.
    let wacz = build_wacz("data.warc", &warc_bytes()?)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(wacz))?;
    assert_eq!(
        archive.by_name("archive/data.warc")?.compression(),
        CompressionMethod::Stored
    );
    for (name, expected) in expectations {
        assert_eq!(archive.by_name(name)?.compression(), expected, "{name}");
    }

    let mut encoder = gzip::Encoder::new(Vec::new())?;
    encoder.write_all(&warc_bytes()?)?;
    let compressed = encoder.finish().into_result()?;

    let wacz = build_wacz("data.warc.gz", &compressed)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(wacz))?;
    assert_eq!(
        archive.by_name("archive/data.warc.gz")?.compression(),
        CompressionMethod::Stored
    );

    Ok(())
}

#[test]
fn create_refuses_an_existing_output() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("test.wacz");
    std::fs::write(&path, b"existing")?;

    assert!(WaczWriter::create(&path).is_err());

    Ok(())
}

#[test]
fn write_and_open_from_paths() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let warc_path = directory.path().join("data.warc");
    std::fs::write(&warc_path, warc_bytes()?)?;

    let wacz_path = directory.path().join("test.wacz");
    let mut writer = WaczWriter::create(&wacz_path)?;
    writer.add_warc_from_path(&warc_path)?;
    writer.add_pages(&PageListHeader::default(), [])?;
    writer.finish(PackageMetadata::default())?;

    let mut reader = WaczReader::open(&wacz_path)?;

    assert!(reader.verify()?.is_success());
    assert_eq!(
        reader.warc_paths().collect::<Vec<_>>(),
        vec!["archive/data.warc"]
    );

    Ok(())
}

#[test]
fn verify_reports_missing_and_mismatched_members() -> Result<(), Box<dyn std::error::Error>> {
    // A hand-rolled container whose manifest lists a member that is absent and misstates the
    // hash of one that is present.
    let manifest = concat!(
        "{\"profile\": \"data-package\", \"wacz_version\": \"1.1.1\", \"resources\": [",
        "{\"name\": \"pages.jsonl\", \"path\": \"pages/pages.jsonl\", ",
        "\"hash\": \"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\", ",
        "\"bytes\": 0}, ",
        "{\"name\": \"missing.warc\", \"path\": \"archive/missing.warc\", ",
        "\"hash\": \"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\", ",
        "\"bytes\": 0}]}",
    );

    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("datapackage.json", options)?;
    zip.write_all(manifest.as_bytes())?;
    zip.start_file("pages/pages.jsonl", options)?;
    zip.write_all(b"{\"format\": \"json-pages-1.0\", \"id\": \"pages\", \"title\": \"t\"}\n")?;
    let bytes = zip.finish()?.into_inner();

    let mut reader = WaczReader::new(Cursor::new(bytes))?;
    let verification = reader.verify()?;

    assert!(!verification.is_success());
    assert_eq!(verification.mismatched, vec!["pages/pages.jsonl"]);
    assert_eq!(verification.missing, vec!["archive/missing.warc"]);

    Ok(())
}
