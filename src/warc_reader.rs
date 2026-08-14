use crate::parser;
use crate::{BufferedBody, Error, MB, RawRecordHeader, Record, StreamingBody};

use std::convert::TryInto;
use std::fs;
use std::io;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[cfg(feature = "gzip")]
use libflate::gzip::MultiDecoder as GzipReader;

/// A reader which iteratively parses WARC records from a stream.
pub struct WarcReader<R> {
    reader: R,
}

impl<R: BufRead> WarcReader<R> {
    /// Create a new reader.
    pub fn new(r: R) -> Self {
        WarcReader { reader: r }
    }

    /// Create an iterator over all of the raw records read.
    ///
    /// This only does well-formedness checks on the headers. See `RawRecordHeader` for more
    /// information.
    pub fn iter_raw_records(self) -> RawRecordIter<R> {
        RawRecordIter::new(self.reader)
    }

    /// Create an iterator over all of the records read.
    ///
    /// This will fully build each record and check it for semantic correctness. See the `Record`
    /// type for more information.
    pub fn iter_records(self) -> RecordIter<R> {
        RecordIter::new(self.reader)
    }

    /// Create a streaming iterator over all of the records read.
    ///
    /// This will build each record header, and allow the caller to decide whether to read
    /// the body or not.
    pub fn stream_records(&mut self) -> StreamingIter<'_, R> {
        StreamingIter::new(&mut self.reader)
    }
}

impl WarcReader<BufReader<fs::File>> {
    /// Create a new reader which reads from file.
    pub fn from_path<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = fs::File::open(&path)?;

        let reader = BufReader::with_capacity(MB, file);

        Ok(WarcReader::new(reader))
    }
}

#[cfg(feature = "gzip")]
impl WarcReader<BufReader<GzipReader<BufReader<fs::File>>>> {
    /// Create a new reader which reads from a compressed file.
    ///
    /// Only GZIP compression is currently supported.
    pub fn from_path_gzip<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = fs::File::open(&path)?;

        let gzip_stream = GzipReader::new(BufReader::with_capacity(MB, file))?;
        Ok(WarcReader::new(BufReader::new(gzip_stream)))
    }
}

/// Read lines up to and including the blank line that terminates a header block.
///
/// The header block is left in `header_buffer`, which is cleared first so callers can reuse
/// one buffer across records.
///
/// Returns `None` on a clean end-of-stream at a record boundary.
fn read_header_block<R: BufRead>(
    reader: &mut R,
    header_buffer: &mut Vec<u8>,
) -> Option<Result<(), Error>> {
    header_buffer.clear();
    loop {
        let bytes_read = match reader.read_until(b'\n', header_buffer) {
            Err(io) => return Some(Err(Error::ReadData(io))),
            Ok(len) => len,
        };

        if bytes_read == 0 {
            return None;
        }

        if bytes_read == 2 && header_buffer.ends_with(b"\r\n") {
            return Some(Ok(()));
        }
    }
}

/// Parse a raw header block into its headers and the expected body length.
fn parse_header_block(buffer: &[u8]) -> Result<(RawRecordHeader, usize), Error> {
    let (_, (version, headers, expected_body_len)) = parser::headers(buffer)
        .map_err(|e| Error::ParseHeaders(e.map(|inner| (inner.input.to_owned(), inner.code))))?;

    let headers = RawRecordHeader {
        version: version.to_owned(),
        headers: headers
            .into_iter()
            .map(|(token, value)| (token.into(), value.to_owned()))
            .collect(),
    };

    Ok((headers, expected_body_len))
}

/// Read a record body of the given length, plus the `\r\n\r\n` record terminator.
fn read_body<R: BufRead>(reader: &mut R, expected_body_len: usize) -> Result<Vec<u8>, Error> {
    // Size the buffer to the record, but cap the speculative allocation at `MB` so a bogus
    // `Content-Length` cannot force a huge up-front allocation.
    let mut body_buffer: Vec<u8> = Vec::with_capacity(std::cmp::min(expected_body_len + 4, MB));
    let mut body_bytes_read = 0;
    let maximum_read_range = expected_body_len + 4;
    loop {
        let bytes_read = match reader.read_until(b'\n', &mut body_buffer) {
            Err(io) => return Err(Error::ReadData(io)),
            Ok(len) => len,
        };

        body_bytes_read += bytes_read;

        // we expect 4 characters (`\r\n\r\n`) after the body
        if bytes_read == 2 && body_bytes_read == maximum_read_range {
            if &body_buffer[expected_body_len..] != b"\r\n\r\n" {
                let synthetic_err: nom::Err<(Vec<u8>, nom::error::ErrorKind)> =
                    nom::Err::Failure((vec![0x0d, 0x0a, 0x0d, 0x0a], nom::error::ErrorKind::Tag));
                return Err(Error::ParseHeaders(synthetic_err));
            }
            body_buffer.truncate(expected_body_len);
            return Ok(body_buffer);
        }

        if bytes_read == 0 {
            return Err(Error::UnexpectedEOB);
        }

        if body_bytes_read > maximum_read_range {
            return Err(Error::ReadOverflow);
        }
    }
}

/// An iterator of raw records streamed from a reader. See `RawRecord` for more information.
pub struct RawRecordIter<R> {
    reader: R,
    header_buffer: Vec<u8>,
}

impl<R: BufRead> RawRecordIter<R> {
    pub(crate) fn new(reader: R) -> RawRecordIter<R> {
        RawRecordIter {
            reader,
            header_buffer: Vec::new(),
        }
    }
}

impl<R: BufRead> Iterator for RawRecordIter<R> {
    type Item = Result<(RawRecordHeader, Vec<u8>), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match read_header_block(&mut self.reader, &mut self.header_buffer)? {
            Ok(()) => {}
            Err(e) => return Some(Err(e)),
        }

        let (headers, expected_body_len) = match parse_header_block(&self.header_buffer) {
            Ok(parsed) => parsed,
            Err(e) => return Some(Err(e)),
        };

        let body = match read_body(&mut self.reader, expected_body_len) {
            Ok(body) => body,
            Err(e) => return Some(Err(e)),
        };

        Some(Ok((headers, body)))
    }
}

/// An iterator which returns the records read by a reader.
pub struct RecordIter<R> {
    raw_iter: RawRecordIter<R>,
}

impl<R: BufRead> RecordIter<R> {
    pub(crate) fn new(reader: R) -> RecordIter<R> {
        RecordIter {
            raw_iter: RawRecordIter::new(reader),
        }
    }
}

impl<R: BufRead> Iterator for RecordIter<R> {
    type Item = Result<Record<BufferedBody>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let (headers, body) = match self.raw_iter.next()? {
            Ok(parts) => parts,
            Err(e) => return Some(Err(e)),
        };

        match headers.try_into() {
            Ok(b) => {
                let buffered: Record<_> = b;
                Some(Ok(buffered.add_body(body)))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

/// An iterator-like type to "stream" records from a reader.
///
/// This API returns records which use the `StreamingBody` type. This allows reading record headers
/// and metadata without reading the bodies. Bodies can be read or skipped as desired.
///
/// This is streaming iterator is particularly useful for streams of records which are indefinite
/// or contain and records of unknown size.
pub struct StreamingIter<'r, R> {
    reader: &'r mut R,
    current_item_size: u64,
    first_record: bool,
    header_buffer: Vec<u8>,
}

impl<R: BufRead> StreamingIter<'_, R> {
    pub(crate) fn new(reader: &mut R) -> StreamingIter<'_, R> {
        StreamingIter {
            reader,
            current_item_size: 0,
            first_record: true,
            header_buffer: Vec::new(),
        }
    }

    fn skip_body(&mut self) -> Result<(), Error> {
        let mut read_buffer = [0u8; MB];
        let maximum_read_range = self.current_item_size;
        let mut body_bytes_left = maximum_read_range;
        while body_bytes_left > 0 {
            let read_size = std::cmp::min(body_bytes_left, read_buffer.len() as u64) as usize;
            let bytes_read = match self.reader.read(&mut read_buffer[..read_size]) {
                Err(io) => return Err(Error::ReadData(io)),
                Ok(len) => len as u64,
            };
            if bytes_read == 0 {
                return Err(Error::UnexpectedEOB);
            }
            body_bytes_left -= bytes_read;
        }

        let mut crlfs = [0; 4];

        match self.reader.read_exact(&mut crlfs) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(Error::UnexpectedEOB);
            }
            Err(io) => return Err(Error::ReadData(io)),
        }

        if &crlfs == b"\x0d\x0a\x0d\x0a" {
            Ok(())
        } else {
            let synthetic_err: nom::Err<(Vec<u8>, nom::error::ErrorKind)> =
                nom::Err::Failure((vec![0x0d, 0x0a, 0x0d, 0x0a], nom::error::ErrorKind::Tag));
            Err(Error::ParseHeaders(synthetic_err))
        }
    }

    /// Advance the stream to the next item.
    ///
    /// Returns one of the following:
    /// * `Some(Ok(r))` is the next record read from the stream.
    /// * `Some(Err)` indicates there was a read error.
    /// * `None` indicates no more records are returned.
    pub fn next_item(&mut self) -> Option<Result<Record<StreamingBody<'_, R>>, Error>> {
        if self.first_record {
            self.first_record = false;
        } else if let Err(e) = self.skip_body() {
            return Some(Err(e));
        }

        match read_header_block(self.reader, &mut self.header_buffer)? {
            Ok(()) => {}
            Err(e) => return Some(Err(e)),
        }

        let (headers, expected_body_len) = match parse_header_block(&self.header_buffer) {
            Ok(parsed) => parsed,
            Err(e) => return Some(Err(e)),
        };
        self.current_item_size = expected_body_len as u64;

        match headers.try_into() {
            Ok(b) => {
                let record: Record<_> = b;
                let fixed_stream_result = record
                    .add_fixed_stream(self.reader, &mut self.current_item_size)
                    .map_err(Error::ReadData);
                Some(fixed_stream_result)
            }
            Err(e) => Some(Err(e)),
        }
    }
}

#[cfg(test)]
mod from_path_tests {
    use crate::WarcReader;

    #[test]
    fn reads_existing_file() {
        let raw: &[u8] = b"\
            WARC/1.0\r\n\
            Warc-Type: dunno\r\n\
            Content-Length: 5\r\n\
            WARC-Record-Id: <urn:test:from-path:record-0>\r\n\
            WARC-Date: 2020-07-08T02:52:55Z\r\n\
            \r\n\
            12345\r\n\
            \r\n\
        ";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reads_existing_file.warc");
        std::fs::write(&path, raw).unwrap();

        let reader = WarcReader::from_path(&path).unwrap();
        let record = reader.iter_records().next().unwrap().unwrap();
        assert_eq!(record.warc_id(), "<urn:test:from-path:record-0>");
        assert_eq!(record.body(), b"12345");
    }

    #[test]
    fn missing_file_is_not_found_and_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing_file.warc");
        let err = match WarcReader::from_path(&path) {
            Ok(_) => panic!("expected opening a missing file to fail"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(!path.exists());
    }
}

#[cfg(test)]
mod iter_raw_tests {
    use indexmap::IndexMap;
    use std::io::{BufReader, Cursor};
    use std::iter::FromIterator;

    use crate::{Error, WarcHeader, WarcReader};
    macro_rules! create_reader {
        ($raw:expr) => {{ BufReader::new(Cursor::new($raw.get(..).unwrap())) }};
    }

    #[test]
    fn invalid_record_terminator() {
        // After the 4-byte body, the record ends with `c\nd\n` instead of `\r\n\r\n`; the byte
        // counts line up, but the terminator bytes are wrong.
        let raw = b"\
            WARC/1.0\r\n\
            Warc-Type: dunno\r\n\
            Content-Length: 4\r\n\
            \r\n\
            a\nb\nc\nd\n\
        ";

        let mut reader = WarcReader::new(create_reader!(raw)).iter_raw_records();
        match reader.next().unwrap() {
            Err(Error::ParseHeaders(_)) => {}
            other => panic!(
                "expected a parse error for an invalid record terminator, got {:?}",
                other.map(|(headers, body)| (headers, String::from_utf8_lossy(&body).to_string()))
            ),
        }
    }

    #[test]
    fn basic_record() {
        let raw = b"\
            WARC/1.0\r\n\
            Warc-Type: dunno\r\n\
            Content-Length: 5\r\n\
            WARC-Record-Id: <urn:test:basic-record:record-0>\r\n\
            WARC-Date: 2020-07-08T02:52:55Z\r\n\
            \r\n\
            12345\r\n\
            \r\n\
        ";

        let expected_version = "1.0";
        let expected_headers: IndexMap<WarcHeader, Vec<u8>> = IndexMap::from_iter(vec![
            (WarcHeader::WarcType, b"dunno".to_vec()),
            (WarcHeader::ContentLength, b"5".to_vec()),
            (
                WarcHeader::RecordID,
                b"<urn:test:basic-record:record-0>".to_vec(),
            ),
            (WarcHeader::Date, b"2020-07-08T02:52:55Z".to_vec()),
        ]);
        let expected_body: &[u8] = b"12345";

        let mut reader = WarcReader::new(create_reader!(raw)).iter_raw_records();
        let (headers, body) = reader.next().unwrap().unwrap();
        assert_eq!(headers.version, expected_version);
        assert_eq!(headers.as_ref(), &expected_headers);
        assert_eq!(body, expected_body);
    }

    #[test]
    fn two_records() {
        let raw = b"\
            WARC/1.0\r\n\
            Warc-Type: dunno\r\n\
            Content-Length: 5\r\n\
            WARC-Record-Id: <urn:test:two-records:record-0>\r\n\
            WARC-Date: 2020-07-08T02:52:55Z\r\n\
            \r\n\
            12345\r\n\
            \r\n\
            WARC/1.0\r\n\
            Warc-Type: another\r\n\
            WARC-Record-Id: <urn:test:two-records:record-1>\r\n\
            WARC-Date: 2020-07-08T02:52:56Z\r\n\
            Content-Length: 6\r\n\
            \r\n\
            123456\r\n\
            \r\n\
        ";

        let mut reader = WarcReader::new(create_reader!(raw)).iter_raw_records();
        {
            let expected_version = "1.0";
            let expected_headers: IndexMap<WarcHeader, Vec<u8>> = IndexMap::from_iter(vec![
                (WarcHeader::WarcType, b"dunno".to_vec()),
                (WarcHeader::ContentLength, b"5".to_vec()),
                (
                    WarcHeader::RecordID,
                    b"<urn:test:two-records:record-0>".to_vec(),
                ),
                (WarcHeader::Date, b"2020-07-08T02:52:55Z".to_vec()),
            ]);
            let expected_body: &[u8] = b"12345";

            let (headers, body) = reader.next().unwrap().unwrap();
            assert_eq!(headers.version, expected_version);
            assert_eq!(headers.as_ref(), &expected_headers);
            assert_eq!(body, expected_body);
        }

        {
            let expected_version = "1.0";
            let expected_headers: IndexMap<WarcHeader, Vec<u8>> = IndexMap::from_iter(vec![
                (WarcHeader::WarcType, b"another".to_vec()),
                (WarcHeader::ContentLength, b"6".to_vec()),
                (
                    WarcHeader::RecordID,
                    b"<urn:test:two-records:record-1>".to_vec(),
                ),
                (WarcHeader::Date, b"2020-07-08T02:52:56Z".to_vec()),
            ]);
            let expected_body: &[u8] = b"123456";

            let (headers, body) = reader.next().unwrap().unwrap();
            assert_eq!(headers.version, expected_version);
            assert_eq!(headers.as_ref(), &expected_headers);
            assert_eq!(body, expected_body);
        }
    }
}

#[cfg(test)]
mod next_item_tests {
    use std::io::{BufReader, Cursor};

    use crate::WarcReader;

    macro_rules! create_reader {
        ($raw:expr) => {{ BufReader::new(Cursor::new($raw.get(..).unwrap())) }};
    }

    #[test]
    fn first_item() {
        let raw = b"\
            WARC/1.0\r\n\
            Warc-Type: dunno\r\n\
            Content-Length: 5\r\n\
            WARC-Record-Id: <urn:test:basic-record:record-0>\r\n\
            WARC-Date: 2020-07-08T02:52:55Z\r\n\
            \r\n\
            12345\r\n\
            \r\n\
        ";

        let mut reader = WarcReader::new(create_reader!(raw));
        let mut stream_iter = reader.stream_records();
        let record = stream_iter
            .next_item()
            .unwrap()
            .unwrap()
            .into_buffered()
            .unwrap();
        assert_eq!(record.warc_version(), "1.0");
        assert_eq!(record.content_length(), 5);
        assert_eq!(record.warc_id(), "<urn:test:basic-record:record-0>");
        assert_eq!(record.body(), b"12345");
    }

    #[test]
    fn both_items() {
        let raw = b"\
            WARC/1.0\r\n\
            Warc-Type: dunno\r\n\
            Content-Length: 5\r\n\
            WARC-Record-Id: <urn:test:two-records:record-0>\r\n\
            WARC-Date: 2020-07-08T02:52:55Z\r\n\
            \r\n\
            12345\r\n\
            \r\n\
            WARC/1.0\r\n\
            Warc-Type: another\r\n\
            WARC-Record-Id: <urn:test:two-records:record-1>\r\n\
            WARC-Date: 2020-07-08T02:52:56Z\r\n\
            Content-Length: 6\r\n\
            \r\n\
            123456\r\n\
            \r\n\
        ";

        let mut reader = WarcReader::new(create_reader!(raw));
        let mut stream_iter = reader.stream_records();

        {
            let record = stream_iter
                .next_item()
                .unwrap()
                .unwrap()
                .into_buffered()
                .unwrap();
            assert_eq!(record.warc_version(), "1.0");
            assert_eq!(record.content_length(), 5);
            assert_eq!(record.warc_id(), "<urn:test:two-records:record-0>");
            assert_eq!(record.body(), b"12345");
        }

        {
            let record = stream_iter
                .next_item()
                .unwrap()
                .unwrap()
                .into_buffered()
                .unwrap();
            assert_eq!(record.warc_version(), "1.0");
            assert_eq!(record.content_length(), 6);
            assert_eq!(record.warc_id(), "<urn:test:two-records:record-1>");
            assert_eq!(record.body(), b"123456");
        }
    }

    #[test]
    fn only_second_item() {
        let raw = b"\
            WARC/1.0\r\n\
            Warc-Type: dunno\r\n\
            Content-Length: 5\r\n\
            WARC-Record-Id: <urn:test:two-records:record-0>\r\n\
            WARC-Date: 2020-07-08T02:52:55Z\r\n\
            \r\n\
            12345\r\n\
            \r\n\
            WARC/1.0\r\n\
            Warc-Type: another\r\n\
            WARC-Record-Id: <urn:test:two-records:record-1>\r\n\
            WARC-Date: 2020-07-08T02:52:56Z\r\n\
            Content-Length: 6\r\n\
            \r\n\
            123456\r\n\
            \r\n\
        ";

        let mut reader = WarcReader::new(create_reader!(raw));
        let mut stream_iter = reader.stream_records();

        let _skipped = stream_iter.next_item().unwrap().unwrap();

        {
            let record = stream_iter
                .next_item()
                .unwrap()
                .unwrap()
                .into_buffered()
                .unwrap();
            assert_eq!(record.warc_version(), "1.0");
            assert_eq!(record.content_length(), 6);
            assert_eq!(record.warc_id(), "<urn:test:two-records:record-1>");
            assert_eq!(record.body(), b"123456");
        }
    }

    #[test]
    fn triple_items() {
        let raw = b"\
            WARC/1.0\r\n\
            Warc-Type: dunno\r\n\
            Content-Length: 5\r\n\
            WARC-Record-Id: <urn:test:three-records:record-0>\r\n\
            WARC-Date: 2020-07-08T02:52:55Z\r\n\
            \r\n\
            12345\r\n\
            \r\n\
            WARC/1.0\r\n\
            Warc-Type: another\r\n\
            WARC-Record-Id: <urn:test:three-records:record-1>\r\n\
            WARC-Date: 2020-07-08T02:52:56Z\r\n\
            Content-Length: 6\r\n\
            \r\n\
            123456\r\n\
            \r\n\
            WARC/1.0\r\n\
            Warc-Type: yet another\r\n\
            WARC-Record-Id: <urn:test:three-records:record-2>\r\n\
            WARC-Date: 2020-07-08T02:52:56Z\r\n\
            Content-Length: 8\r\n\
            \r\n\
            12345678\r\n\
            \r\n\
        ";

        let mut reader = WarcReader::new(create_reader!(raw));
        let mut stream_iter = reader.stream_records();

        {
            let record = stream_iter
                .next_item()
                .unwrap()
                .unwrap()
                .into_buffered()
                .unwrap();
            assert_eq!(record.warc_version(), "1.0");
            assert_eq!(record.content_length(), 5);
            assert_eq!(record.warc_id(), "<urn:test:three-records:record-0>");
            assert_eq!(record.body(), b"12345");
        }

        {
            let record = stream_iter
                .next_item()
                .unwrap()
                .unwrap()
                .into_buffered()
                .unwrap();
            assert_eq!(record.warc_version(), "1.0");
            assert_eq!(record.content_length(), 6);
            assert_eq!(record.warc_id(), "<urn:test:three-records:record-1>");
            assert_eq!(record.body(), b"123456");
        }

        {
            let record = stream_iter
                .next_item()
                .unwrap()
                .unwrap()
                .into_buffered()
                .unwrap();
            assert_eq!(record.warc_version(), "1.0");
            assert_eq!(record.content_length(), 8);
            assert_eq!(record.warc_id(), "<urn:test:three-records:record-2>");
            assert_eq!(record.body(), b"12345678");
        }
    }

    #[test]
    fn empty_content_length() {
        let raw = b"\
        WARC/1.0\r\n\
        Warc-Type: empty-record\r\n\
        Content-Length: 0\r\n\
        WARC-Record-Id: <urn:test:empty-content-length>\r\n\
        WARC-Date: 2020-07-08T02:52:57Z\r\n\
        \r\n\
        \r\n\
    ";

        let mut reader = WarcReader::new(create_reader!(raw));
        let mut stream_iter = reader.stream_records();

        let record = stream_iter
            .next_item()
            .unwrap()
            .unwrap()
            .into_buffered()
            .unwrap();
        assert_eq!(record.warc_version(), "1.0");
        assert_eq!(record.content_length(), 0);
        assert_eq!(record.warc_id(), "<urn:test:empty-content-length>");
        assert_eq!(record.body(), b"");
    }

    #[test]
    fn zero_and_nonzero_content_length() {
        let raw = b"\
        WARC/1.0\r\n\
        Warc-Type: empty-record\r\n\
        Content-Length: 0\r\n\
        WARC-Record-Id: <urn:test:zero-content-length>\r\n\
        WARC-Date: 2020-07-08T02:52:57Z\r\n\
        \r\n\
        \r\n\
        \r\n\
        WARC/1.0\r\n\
        Warc-Type: non-empty-record\r\n\
        Content-Length: 7\r\n\
        WARC-Record-Id: <urn:test:nonzero-content-length>\r\n\
        WARC-Date: 2020-07-08T02:52:58Z\r\n\
        \r\n\
        1234567\r\n\
        \r\n\
    ";

        let reader = WarcReader::new(create_reader!(raw));
        let mut iter = reader.iter_records();

        // Test the first record with Content-Length: 0
        {
            let record = iter.next().unwrap().unwrap();
            assert_eq!(record.warc_version(), "1.0");
            assert_eq!(record.content_length(), 0);
            assert_eq!(record.warc_id(), "<urn:test:zero-content-length>");
            assert_eq!(record.body(), b"");
        }

        // Test the second record with non-zero Content-Length
        {
            let record = iter.next().unwrap().unwrap();
            assert_eq!(record.warc_version(), "1.0");
            assert_eq!(record.content_length(), 7);
            assert_eq!(record.warc_id(), "<urn:test:nonzero-content-length>");
            assert_eq!(record.body(), b"1234567");
        }
    }
}

#[cfg(all(test, feature = "gzip"))]
mod gzip_tests {
    use std::io::Write;

    use crate::{
        BufferedBody, Record, RecordBuilder, RecordType, WarcHeader, WarcReader, WarcWriter,
    };

    fn record(body: &[u8], url: &str) -> Record<BufferedBody> {
        RecordBuilder::default()
            .warc_type(RecordType::Response)
            .header(WarcHeader::TargetURI, url)
            .body(body.to_vec())
            .build()
            .expect("record should build")
    }

    /// Records written through the gzip writer read back through the gzip reader.
    #[test]
    fn gzip_path_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("round-trip.warc.gz");

        let mut writer = WarcWriter::from_path_gzip(&path).unwrap();
        writer
            .write(&record(b"first body", "https://example.com/1"))
            .unwrap();
        writer
            .write(&record(b"second body", "https://example.com/2"))
            .unwrap();
        // The gzip stream must be finished, or the file is truncated.
        let gzip_stream = writer
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)
            .unwrap();
        gzip_stream.finish().into_result().unwrap();

        let reader = WarcReader::from_path_gzip(&path).unwrap();
        let records = reader
            .iter_records()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].body(), b"first body");
        assert_eq!(
            records[0].header(WarcHeader::TargetURI).as_deref(),
            Some("https://example.com/1")
        );
        assert_eq!(records[1].body(), b"second body");
        assert_eq!(
            records[1].header(WarcHeader::TargetURI).as_deref(),
            Some("https://example.com/2")
        );
    }

    /// A `.warc.gz` holding each record as its own gzip member — the conventional layout for
    /// compressed WARC files, and the case the multi-member decoder exists for — reads back
    /// as a single record stream.
    #[test]
    fn gzip_multi_member_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi-member.warc.gz");

        let mut file = std::fs::File::create(&path).unwrap();
        for (body, url) in [
            (&b"first body"[..], "https://example.com/1"),
            (&b"second body"[..], "https://example.com/2"),
        ] {
            let mut encoder = libflate::gzip::Encoder::new(Vec::new()).unwrap();
            WarcWriter::new(&mut encoder)
                .write(&record(body, url))
                .unwrap();
            let member = encoder.finish().into_result().unwrap();
            file.write_all(&member).unwrap();
        }
        drop(file);

        let reader = WarcReader::from_path_gzip(&path).unwrap();
        let records = reader
            .iter_records()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].body(), b"first body");
        assert_eq!(records[1].body(), b"second body");
    }

    /// The streaming iterator reads gzip input like any other, skipping and buffering across
    /// member boundaries.
    #[test]
    fn gzip_streaming_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("streaming.warc.gz");

        let mut writer = WarcWriter::from_path_gzip(&path).unwrap();
        writer
            .write(&record(b"first body", "https://example.com/1"))
            .unwrap();
        writer
            .write(&record(b"second body", "https://example.com/2"))
            .unwrap();
        let gzip_stream = writer
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)
            .unwrap();
        gzip_stream.finish().into_result().unwrap();

        let mut reader = WarcReader::from_path_gzip(&path).unwrap();
        let mut stream_iter = reader.stream_records();

        // Skip the first record's body entirely, then buffer the second.
        let _skipped = stream_iter.next_item().unwrap().unwrap();
        let second = stream_iter
            .next_item()
            .unwrap()
            .unwrap()
            .into_buffered()
            .unwrap();

        assert_eq!(second.body(), b"second body");
        assert!(stream_iter.next_item().is_none());
    }
}
