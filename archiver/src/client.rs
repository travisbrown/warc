//! The archiving client.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;
use std::sync::{Mutex, mpsc};
use std::thread;

use chrono::{DateTime, Utc};
use libflate::gzip;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, LOCATION, USER_AGENT};
use reqwest::redirect::Policy;
use sha2::Digest as _;
use url::Url;
use warc::{BufferedBody, Record, RecordBuilder, RecordType, WarcHeader, WarcWriter};
use warc_wacz::ExtraProperties;
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
    /// A URL to be archived contains credentials.
    ///
    /// Credentials are rejected rather than archived: the HTTP layer would send them as an
    /// `Authorization` header, so capturing the exchange would either leak the secret into the
    /// archive or misrepresent what was sent. The URL carried here has its credentials removed
    /// so that the error is safe to log.
    #[error("URL contains credentials: {0}")]
    CredentialedUrl(String),
    /// A URL to be archived does not have a host.
    #[error("URL has no host: {0}")]
    MissingHost(String),
    /// The configured `User-Agent` is not a valid HTTP header value.
    #[error("invalid User-Agent header value: {0:?}")]
    InvalidUserAgent(String),
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
    /// When the capture of the final response began (the `WARC-Date` of its records).
    pub date: DateTime<Utc>,
    /// The status code of the final response.
    pub status: u16,
    /// The body length in bytes of the final response.
    pub size: u64,
    /// The number of redirects followed (each hop is recorded in the archive).
    pub redirects: usize,
}

/// A URL which could not be captured.
///
/// Hops of a redirect chain captured before the failure are still recorded in the archive and
/// its index; only the page entry, which describes a final response that was never received, is
/// omitted.
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
}

/// A completed capture travelling from a worker thread to the writer: the input position and
/// URL, the exchanges captured, and the error that ended the chain early, if any.
type CaptureOutcome = (usize, String, Vec<Exchange>, Option<Error>);

/// The collection members accumulated as captures are recorded, around the spooled WARC member.
struct Collection {
    spool: BufWriter<File>,
    /// The offset in the WARC member at which the next record will be written.
    offset: u64,
    warcinfo_id: String,
    warc_name: &'static str,
    gzip: bool,
    summary: ArchiveSummary,
    items: Vec<cdxj::Item<'static>>,
    page_list: Vec<Page<'static>>,
}

impl Collection {
    /// Record the outcome of capturing one URL: write and index every captured hop, then add a
    /// page entry and capture summary on success, or a failure entry otherwise.
    fn record(
        &mut self,
        url: String,
        exchanges: Vec<Exchange>,
        error: Option<Error>,
    ) -> Result<(), Error> {
        let redirects = exchanges.len().saturating_sub(1);
        let mut last = None;

        for exchange in exchanges {
            last = Some((exchange.date, exchange.status, exchange.payload_length));

            let (item, written) = write_exchange(
                &mut self.spool,
                exchange,
                &self.warcinfo_id,
                self.offset,
                self.warc_name,
                self.gzip,
            )?;
            self.items.push(item);
            self.offset += written;
        }

        if let Some(error) = error {
            self.summary.failures.push(Failure { url, error });
        } else {
            let (date, status, size) =
                last.expect("a capture without an error has at least one exchange");

            self.page_list.push(Page {
                url: Cow::Owned(url.clone()),
                ts: date,
                id: None,
                title: None,
                text: None,
                size: Some(size),
                extra: ExtraProperties::default(),
            });

            self.summary.captures.push(CaptureSummary {
                url,
                date,
                status,
                size,
                redirects,
            });
        }

        Ok(())
    }
}

/// An HTTP client which downloads lists of URLs into WACZ web archive collections.
///
/// Each URL is fetched with a `GET` request, following redirects up to the configured limit;
/// every hop is recorded in the WARC member as a response record and a request record holding
/// the full HTTP messages, indexed by an entry in the CDXJ index member. Each URL also receives
/// a page list entry describing its final response. When a download fails partway through a
/// redirect chain, the hops already captured are still recorded and the URL is reported as a
/// failure.
///
/// The client is blocking: creating or using it from within an async runtime panics.
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
    ///
    /// # Panics
    ///
    /// The underlying blocking HTTP client cannot be created or used from within an async
    /// runtime and panics if that is attempted; construct and use the archiver from a
    /// synchronous context (for example a dedicated thread).
    pub fn new(config: Config) -> Result<Self, Error> {
        // Building the header value up front validates the configured `User-Agent`: a value
        // with embedded line breaks would otherwise forge header lines in the rendered request
        // and break every request sent.
        let user_agent = HeaderValue::from_str(&config.user_agent)
            .map_err(|_| Error::InvalidUserAgent(config.user_agent.clone()))?;

        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, user_agent);
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));

        let client = Client::builder()
            .http1_only()
            .timeout(config.timeout)
            .redirect(Policy::none())
            .default_headers(headers)
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

        // The WARC member is spooled to an unlinked temporary file so that response bodies
        // never need to be held in memory all at once.
        let mut spool = BufWriter::new(tempfile::tempfile()?);

        let warcinfo = warcinfo_record(warc_name)?;
        let warcinfo_id = warcinfo.warc_id().to_owned();
        let (offset, _) = write_record(&mut spool, &warcinfo, gzip)?;

        let mut collection = Collection {
            spool,
            offset,
            warcinfo_id,
            warc_name,
            gzip,
            summary: ArchiveSummary::default(),
            items: Vec::new(),
            page_list: Vec::new(),
        };

        let concurrency = self.config.concurrency.max(1);

        if concurrency == 1 {
            for url in urls {
                let url = url.as_ref();
                let (exchanges, error) = self.capture(url);

                collection.record(url.to_owned(), exchanges, error)?;
            }
        } else {
            self.capture_concurrently(urls, concurrency, &mut collection)?;
        }

        let Collection {
            spool,
            summary,
            items,
            page_list,
            ..
        } = collection;

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

    /// Capture URLs with a pool of worker threads, recording the outcomes in input order.
    ///
    /// At most `concurrency` downloads are in flight at a time, and completed captures are
    /// buffered only until their turn comes to be written, so memory use is proportional to
    /// the concurrency rather than to the number of URLs.
    fn capture_concurrently<I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        concurrency: usize,
        collection: &mut Collection,
    ) -> Result<(), Error> {
        let mut urls = urls.into_iter();

        // The channels live outside the thread scope so that the workers may borrow them; the
        // task sender is moved into the scope body and dropped there, closing the task channel
        // so that idle workers exit before the scope joins them.
        let (task_sender, task_receiver) = mpsc::channel::<(usize, String)>();
        let task_receiver = Mutex::new(task_receiver);
        let (outcome_sender, outcome_receiver) = mpsc::sync_channel::<CaptureOutcome>(concurrency);

        thread::scope(|scope| {
            for _ in 0..concurrency {
                let task_receiver = &task_receiver;
                let outcome_sender = outcome_sender.clone();

                scope.spawn(move || {
                    loop {
                        // The lock is held only to take the next task, never while downloading.
                        let task = task_receiver
                            .lock()
                            .ok()
                            .and_then(|receiver| receiver.recv().ok());

                        let Some((index, url)) = task else { return };
                        let (exchanges, error) = self.capture(&url);

                        // The writer hanging up means the run ended early; stop working.
                        if outcome_sender.send((index, url, exchanges, error)).is_err() {
                            return;
                        }
                    }
                });
            }

            drop(outcome_sender);

            let mut dispatched = 0;

            for (index, url) in urls.by_ref().take(concurrency).enumerate() {
                let _ = task_sender.send((index, url.as_ref().to_owned()));
                dispatched += 1;
            }

            let mut result = Ok(());
            let mut completed = 0;
            let mut next_to_record = 0;
            let mut pending = BTreeMap::new();

            // Every dispatched task is drained even after a write error, so that no worker is
            // left blocked on the outcome channel when the scope joins the pool.
            while completed < dispatched {
                let (index, url, exchanges, error) = outcome_receiver
                    .recv()
                    .expect("workers always report an outcome before exiting");
                completed += 1;

                if result.is_ok() {
                    // Refill the pool so that `concurrency` downloads stay in flight.
                    if let Some(url) = urls.next() {
                        let _ = task_sender.send((dispatched, url.as_ref().to_owned()));
                        dispatched += 1;
                    }

                    // Outcomes are recorded strictly in input order, so completions that
                    // arrive early wait in the reorder buffer for their turn.
                    pending.insert(index, (url, exchanges, error));

                    while let Some((url, exchanges, error)) = pending.remove(&next_to_record) {
                        if let Err(error) = collection.record(url, exchanges, error) {
                            result = Err(error);
                            break;
                        }

                        next_to_record += 1;
                    }
                }
            }

            drop(task_sender);

            result
        })
    }

    /// Fetch a URL and every hop of its redirect chain, in order.
    ///
    /// The returned list holds every hop captured. When the error is set, the request for the
    /// next hop (or, if the list is empty, the first request) failed; otherwise the list ends
    /// with the final response, and a response which still redirects after the configured
    /// limit (or whose target is unusable) is recorded as final rather than followed.
    fn capture(&self, url: &str) -> (Vec<Exchange>, Option<Error>) {
        let mut exchanges = Vec::new();

        let mut current = match Url::parse(url) {
            Ok(url) => url,
            Err(error) => return (exchanges, Some(error.into())),
        };

        loop {
            match self.fetch(&current) {
                Ok((exchange, location)) => {
                    exchanges.push(exchange);

                    match location {
                        Some(next) if exchanges.len() <= self.config.max_redirects => {
                            current = next;
                        }
                        _ => return (exchanges, None),
                    }
                }
                Err(error) => return (exchanges, Some(error)),
            }
        }
    }

    /// Perform one `GET` request, rendering the exchange for recording and returning its
    /// redirect target, if any.
    fn fetch(&self, url: &Url) -> Result<(Exchange, Option<Url>), Error> {
        // A URL with credentials cannot be archived faithfully: the HTTP layer would turn them
        // into an `Authorization` header, so recording the exchange would either leak the
        // secret into the archive or misrepresent what was sent.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::CredentialedUrl(redact_credentials(url)));
        }

        let host = url
            .host_str()
            .ok_or_else(|| Error::MissingHost(url.to_string()))?;
        let key = cdxj::search_key(url.as_str())?;
        let request = http::render_request(url, host, &self.config.user_agent);

        // WARC-Date is the instant at which data capture began, so it is taken before the
        // request is sent.
        let date = Utc::now();
        let response = self.client.get(url.clone()).send()?;

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

        Ok((
            Exchange {
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
            },
            location,
        ))
    }
}

/// The redirect target of a response, when present and followable over HTTP.
fn next_location(current: &Url, status: StatusCode, headers: &HeaderMap) -> Option<Url> {
    // Only the redirect statuses that denote a fetchable alternate location are followed:
    // `300 Multiple Choices` and `304 Not Modified` are redirection-class but are final
    // responses in their own right.
    if !matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    ) {
        return None;
    }

    let location = headers.get(LOCATION)?.to_str().ok()?;
    let next = current.join(location).ok()?;

    // A target with credentials could not be archived faithfully (see `Error::CredentialedUrl`),
    // so it is treated as unusable and the redirecting response is recorded as the final hop.
    (matches!(next.scheme(), "http" | "https")
        && next.username().is_empty()
        && next.password().is_none())
    .then_some(next)
}

/// The URL rendered with its credentials removed, safe for error messages and logs.
fn redact_credentials(url: &Url) -> String {
    let mut redacted = url.clone();

    // Removing credentials only fails for URLs that cannot carry them, which cannot get here.
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);

    redacted.to_string()
}

/// A writer adapter that counts and hashes the bytes written through it.
struct DigestWriter<W> {
    inner: W,
    hasher: sha2::Sha256,
    written: u64,
}

impl<W: Write> DigestWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: sha2::Sha256::new(),
            written: 0,
        }
    }

    fn finish(self) -> (u64, Sha256Digest) {
        (self.written, Sha256Digest(self.hasher.finalize().into()))
    }
}

impl<W: Write> Write for DigestWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;

        self.hasher.update(&buf[..written]);
        self.written += written as u64;

        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Write one record to the spooled WARC member, returning the number of bytes written and
/// their SHA-256 digest.
///
/// The record is hashed and counted as it streams through to the spool, so it is never
/// serialized into memory. When `gzip` is set, the record is compressed as an independent gzip
/// member, following the WARC convention, so that the returned length (and therefore the index
/// offsets derived from it) frames a complete member that can be decompressed on its own. The
/// digest is of the stored bytes — the compressed member when compressing — so that it covers
/// exactly the framed range.
fn write_record<W: Write>(
    writer: &mut W,
    record: &Record<BufferedBody>,
    gzip: bool,
) -> Result<(u64, Sha256Digest), Error> {
    let mut digest_writer = DigestWriter::new(writer);

    if gzip {
        let mut encoder = gzip::Encoder::new(&mut digest_writer)?;
        WarcWriter::new(&mut encoder).write(record)?;
        encoder.finish().into_result()?;
    } else {
        WarcWriter::new(&mut digest_writer).write(record)?;
    }

    Ok(digest_writer.finish())
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
            extra: ExtraProperties::default(),
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
