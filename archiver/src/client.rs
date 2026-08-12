//! The archiving client.

use std::borrow::Cow;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use libflate::gzip;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, LOCATION, USER_AGENT};
use reqwest::redirect::Policy;
use url::Url;
use warc::{BufferedBody, Record, RecordBuilder, RecordType, WarcHeader, WarcWriter};
use warc_wacz::cdxj;
use warc_wacz::digest::Sha256Digest;
use warc_wacz::pages::{Page, PageListHeader};
use warc_wacz::writer::{PackageMetadata, WaczWriter, WriterConfig};

use crate::config::{Config, DEFAULT_USER_AGENT};
use crate::http;

/// The file name of the WARC member written into the WACZ file when it is not compressed.
const WARC_NAME: &str = "data.warc";
/// The file name of the WARC member written into the WACZ file when it is gzip-compressed.
const GZIP_WARC_NAME: &str = "data.warc.gz";
/// The file name of the CDXJ index member.
const INDEX_NAME: &str = "index.cdx";
/// The WARC `Content-Type` of response records.
const RESPONSE_CONTENT_TYPE: &str = "application/http;msgtype=response";
/// The WARC `Content-Type` of request records.
const REQUEST_CONTENT_TYPE: &str = "application/http;msgtype=request";

/// An error type for archiving.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The archive could not be written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A request could not be completed.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// A URL to be archived could not be parsed.
    #[error(transparent)]
    InvalidUrl(#[from] url::ParseError),
    /// A URL to be archived does not have a host.
    #[error("URL has no host: {0}")]
    MissingHost(String),
    /// A CDXJ search key could not be derived for a URL.
    #[error(transparent)]
    Index(#[from] cdxj::Error),
    /// A WARC record could not be assembled.
    #[error(transparent)]
    Warc(#[from] warc::Error),
    /// The WACZ file could not be written.
    #[error(transparent)]
    Wacz(#[from] warc_wacz::writer::Error),
}

/// The outcome of an archiving run.
///
/// Individual URLs that could not be downloaded are reported here rather than treated as errors,
/// so that one unreachable URL does not lose the rest of the collection.
#[derive(Debug, Default)]
pub struct ArchiveSummary {
    /// The URLs archived successfully, in request order.
    pub captures: Vec<CaptureSummary>,
    /// The URLs which could not be captured, with the reason for each.
    pub failures: Vec<Failure>,
}

impl ArchiveSummary {
    /// Whether every URL was captured.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failures.is_empty()
    }
}

/// The outcome of capturing a single URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureSummary {
    /// The URL as requested.
    pub url: String,
    /// When the final response was received.
    pub date: DateTime<Utc>,
    /// The status code of the final response.
    pub status: u16,
    /// The body length in bytes of the final response.
    pub size: u64,
    /// The number of redirects followed (each hop is recorded in the archive).
    pub redirects: usize,
}

/// A URL which could not be captured.
#[derive(Debug)]
pub struct Failure {
    /// The URL as requested.
    pub url: String,
    /// The reason the capture failed.
    pub error: Error,
}

/// A single request-response pair, captured and rendered but not yet written.
struct Exchange {
    url: Url,
    key: String,
    date: DateTime<Utc>,
    status: u16,
    mime: Option<String>,
    ip: Option<String>,
    request: Vec<u8>,
    response: Vec<u8>,
    payload_digest: Sha256Digest,
    payload_length: u64,
    location: Option<Url>,
}

/// An HTTP client which downloads lists of URLs into WACZ web archive collections.
///
/// Each URL is fetched with a `GET` request, following redirects up to the configured limit;
/// every hop is recorded in the WARC member as a response record and a request record holding
/// the full HTTP messages, indexed by an entry in the CDXJ index member. Each URL also receives
/// a page list entry describing its final response.
#[derive(Clone, Debug)]
pub struct Archiver {
    client: Client,
    config: Config,
}

impl Archiver {
    /// Create a new archiving client.
    ///
    /// Requests are made over HTTP/1.1 only, so that the recorded messages match the wire
    /// format, and redirects are followed (and captured) by the archiver itself.
    pub fn new(config: Config) -> Result<Self, Error> {
        let client = Client::builder()
            .http1_only()
            .timeout(config.timeout)
            .redirect(Policy::none())
            .build()?;

        Ok(Self { client, config })
    }

    /// Download each URL and write a WACZ file at the given path, refusing to overwrite an
    /// existing file.
    pub fn archive_to_path<P: AsRef<Path>, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        path: P,
    ) -> Result<ArchiveSummary, Error> {
        self.archive_into(
            urls,
            WaczWriter::create_with_config(path, self.writer_config())?,
        )
    }

    /// Download each URL and write a WACZ file to the given writer.
    pub fn archive<W: Write + Seek, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        writer: W,
    ) -> Result<ArchiveSummary, Error> {
        self.archive_into(urls, WaczWriter::with_config(writer, self.writer_config()))
    }

    /// The WACZ writer configuration derived from this client's configuration.
    fn writer_config(&self) -> WriterConfig {
        WriterConfig {
            index_format: self.config.index_format,
            ..WriterConfig::default()
        }
    }

    /// Capture each URL into a spooled WARC file and assemble the WACZ members around it.
    fn archive_into<W: Write + Seek, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        mut wacz: WaczWriter<W>,
    ) -> Result<ArchiveSummary, Error> {
        let gzip = self.config.gzip_warc;
        let warc_name = if gzip { GZIP_WARC_NAME } else { WARC_NAME };

        let mut summary = ArchiveSummary::default();
        let mut items = Vec::new();
        let mut page_list = Vec::new();

        // The WARC member is spooled to an unlinked temporary file so that response bodies never
        // need to be held in memory all at once.
        let mut spool = BufWriter::new(tempfile::tempfile()?);

        let warcinfo = warcinfo_record(warc_name)?;
        let warcinfo_id = warcinfo.warc_id().to_owned();
        let (mut offset, _) = write_record(&mut spool, &warcinfo, gzip)?;

        for url in urls {
            let url = url.as_ref();

            // Capture failures are network-level problems with one URL; they are recorded in the
            // summary, while errors writing the archive itself abort the run below.
            match self.capture(url) {
                Ok(exchanges) => {
                    let redirects = exchanges.len() - 1;
                    let mut last = None;

                    for exchange in exchanges {
                        last = Some((exchange.date, exchange.status, exchange.payload_length));

                        let (item, written) = write_exchange(
                            &mut spool,
                            exchange,
                            &warcinfo_id,
                            offset,
                            warc_name,
                            gzip,
                        )?;
                        items.push(item);
                        offset += written;
                    }

                    let (date, status, size) = last.expect("capture returns at least one exchange");

                    page_list.push(Page {
                        url: Cow::Owned(url.to_owned()),
                        ts: date,
                        id: None,
                        title: None,
                        text: None,
                        size: Some(size),
                        extra: serde_json::Map::new(),
                    });

                    summary.captures.push(CaptureSummary {
                        url: url.to_owned(),
                        date,
                        status,
                        size,
                        redirects,
                    });
                }
                Err(error) => summary.failures.push(Failure {
                    url: url.to_owned(),
                    error,
                }),
            }
        }

        let mut file = spool
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)?;
        file.rewind()?;

        wacz.add_warc(warc_name, file)?;
        wacz.add_index(INDEX_NAME, &items)?;
        wacz.add_pages(&PageListHeader::default(), &page_list)?;

        let metadata = PackageMetadata {
            software: Some(DEFAULT_USER_AGENT.to_owned()),
            main_page_url: summary.captures.first().map(|capture| capture.url.clone()),
            main_page_date: summary.captures.first().map(|capture| capture.date),
            ..PackageMetadata::default()
        };

        wacz.finish(metadata)?.flush()?;

        Ok(summary)
    }

    /// Fetch a URL and every hop of its redirect chain, in order.
    ///
    /// The returned list is never empty. A response which still redirects after the configured
    /// limit (or whose target is unusable) is recorded as the final hop rather than followed.
    fn capture(&self, url: &str) -> Result<Vec<Exchange>, Error> {
        let mut exchanges = Vec::new();
        let mut current = Url::parse(url)?;

        loop {
            let exchange = self.fetch(&current)?;
            let location = exchange.location.clone();
            exchanges.push(exchange);

            match location {
                Some(next) if exchanges.len() <= self.config.max_redirects => current = next,
                _ => return Ok(exchanges),
            }
        }
    }

    /// Perform one `GET` request and render the exchange for recording.
    fn fetch(&self, url: &Url) -> Result<Exchange, Error> {
        let host = url
            .host_str()
            .ok_or_else(|| Error::MissingHost(url.to_string()))?;
        let key = cdxj::search_key(url.as_str())?;
        let request = http::render_request(url, host, &self.config.user_agent);

        let date = Utc::now();
        let response = self
            .client
            .get(url.clone())
            .header(USER_AGENT, &self.config.user_agent)
            .header(ACCEPT, "*/*")
            .send()?;

        let status = response.status();
        let version = response.version();
        let ip = response
            .remote_addr()
            .map(|address| address.ip().to_string());
        let headers = response.headers().clone();
        let location = next_location(url, status, &headers);
        let mime = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split_once(';')
                    .map_or(value, |(mime, _)| mime)
                    .trim()
                    .to_owned()
            });

        let body = response.bytes()?;

        Ok(Exchange {
            url: url.clone(),
            key,
            date,
            status: status.as_u16(),
            mime,
            ip,
            request,
            response: http::render_response(version, status, &headers, &body),
            payload_digest: Sha256Digest::compute(&body),
            payload_length: body.len() as u64,
            location,
        })
    }
}

/// The redirect target of a response, when present and followable over HTTP.
fn next_location(current: &Url, status: StatusCode, headers: &HeaderMap) -> Option<Url> {
    if !status.is_redirection() {
        return None;
    }

    let location = headers.get(LOCATION)?.to_str().ok()?;
    let next = current.join(location).ok()?;

    matches!(next.scheme(), "http" | "https").then_some(next)
}

/// Write one record to the spooled WARC member, returning the number of bytes written and
/// their SHA-256 digest.
///
/// When `gzip` is set, the record is compressed as an independent gzip member, following the
/// WARC convention, so that the returned length (and therefore the index offsets derived from
/// it) frames a complete member that can be decompressed on its own. The digest is of the
/// stored bytes — the compressed member when compressing — so that it covers exactly the
/// framed range.
fn write_record<W: Write>(
    writer: &mut W,
    record: &Record<BufferedBody>,
    gzip: bool,
) -> Result<(u64, Sha256Digest), Error> {
    let stored = if gzip {
        let mut encoder = gzip::Encoder::new(Vec::new())?;
        WarcWriter::new(&mut encoder).write(record)?;
        encoder.finish().into_result()?
    } else {
        let mut bytes = Vec::new();
        WarcWriter::new(&mut bytes).write(record)?;
        bytes
    };

    writer.write_all(&stored)?;

    Ok((stored.len() as u64, Sha256Digest::compute(&stored)))
}

/// Build and write the response and request records for an exchange, returning the CDXJ index
/// entry for the response and the total bytes written.
fn write_exchange<W: Write>(
    writer: &mut W,
    exchange: Exchange,
    warcinfo_id: &str,
    offset: u64,
    warc_name: &'static str,
    gzip: bool,
) -> Result<(cdxj::Item<'static>, u64), Error> {
    let mut response_builder = RecordBuilder::default()
        .warc_type(RecordType::Response)
        .date(exchange.date)
        .header(WarcHeader::TargetURI, exchange.url.as_str())
        .header(WarcHeader::ContentType, RESPONSE_CONTENT_TYPE)
        .header(
            WarcHeader::PayloadDigest,
            exchange.payload_digest.to_string(),
        )
        .header(WarcHeader::WarcInfoID, warcinfo_id);

    if let Some(ip) = &exchange.ip {
        response_builder = response_builder.header(WarcHeader::IPAddress, ip.as_str());
    }

    let response = response_builder.body(exchange.response).build()?;

    let request = RecordBuilder::default()
        .warc_type(RecordType::Request)
        .date(exchange.date)
        .header(WarcHeader::TargetURI, exchange.url.as_str())
        .header(WarcHeader::ContentType, REQUEST_CONTENT_TYPE)
        .header(WarcHeader::ConcurrentTo, response.warc_id())
        .header(WarcHeader::WarcInfoID, warcinfo_id)
        .body(exchange.request)
        .build()?;

    let (response_length, response_digest) = write_record(writer, &response, gzip)?;
    let (request_length, _) = write_record(writer, &request, gzip)?;

    let item = cdxj::Item {
        key: Cow::Owned(exchange.key),
        timestamp: exchange.date.into(),
        fields: cdxj::Fields {
            url: Cow::Owned(exchange.url.into()),
            digest: Some(Cow::Owned(exchange.payload_digest.to_string())),
            mime: exchange.mime.map(Cow::Owned),
            status: Some(exchange.status),
            offset: Some(offset),
            length: Some(response_length),
            filename: Some(Cow::Borrowed(warc_name)),
            record_digest: Some(response_digest),
            extra: serde_json::Map::new(),
        },
    };

    Ok((item, response_length + request_length))
}

/// The `warcinfo` record leading the WARC member.
fn warcinfo_record(warc_name: &str) -> Result<Record<BufferedBody>, Error> {
    let body = format!("software: {DEFAULT_USER_AGENT}\r\nformat: WARC file version 1.1\r\n");

    Ok(RecordBuilder::default()
        .warc_type(RecordType::WarcInfo)
        .header(WarcHeader::ContentType, "application/warc-fields")
        .header(WarcHeader::Filename, warc_name)
        .body(body.into_bytes())
        .build()?)
}
