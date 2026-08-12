use crate::{BufferedBody, MB, RawRecordHeader, Record, WarcHeader};

use std::fs;
use std::io;
use std::io::{BufWriter, Write};
use std::path::Path;

#[cfg(feature = "gzip")]
use libflate::gzip::Encoder as GzipWriter;

/// A writer which writes records to an output stream.
pub struct WarcWriter<W> {
    writer: W,
}

impl<W: Write> WarcWriter<W> {
    /// Create a new writer.
    pub fn new(w: W) -> Self {
        WarcWriter { writer: w }
    }

    /// Write a single record.
    ///
    /// The number of bytes written is returned upon success.
    pub fn write(&mut self, record: &Record<BufferedBody>) -> io::Result<usize> {
        self.write_raw(record.to_raw_header(), &record.body())
    }

    /// Write a single raw record.
    ///
    /// The number of bytes written is returned upon success.
    pub fn write_raw<B>(&mut self, headers: RawRecordHeader, body: &B) -> io::Result<usize>
    where
        B: AsRef<[u8]>,
    {
        let writer = &mut self.writer;
        let mut bytes_written = 0;
        let mut emit = |data: &[u8]| -> io::Result<()> {
            writer.write_all(data)?;
            bytes_written += data.len();
            Ok(())
        };

        emit(b"WARC/")?;
        emit(headers.version.as_bytes())?;
        emit(b"\r\n")?;

        for (token, value) in headers.as_ref().iter() {
            emit(token.to_string().as_bytes())?;
            emit(b": ")?;
            emit(value)?;
            emit(b"\r\n")?;
        }
        // `WARC-Concurrent-To` may repeat: each value becomes its own header line.
        for value in &headers.concurrent_to {
            emit(WarcHeader::ConcurrentTo.to_string().as_bytes())?;
            emit(b": ")?;
            emit(value)?;
            emit(b"\r\n")?;
        }
        emit(b"\r\n")?;

        emit(body.as_ref())?;
        emit(b"\r\n")?;
        emit(b"\r\n")?;

        Ok(bytes_written)
    }
}

impl<W: Write> WarcWriter<BufWriter<W>> {
    /// Consume this writer and return the inner writer.
    ///
    /// # Flushing Compressed Data Streams
    ///
    /// This method is necessary to be called at the end of a GZIP-compressed stream. An extra call
    /// is needed to flush the buffer of data, and write a trailer to the output stream.
    ///
    /// ```ignore
    /// let gzip_stream = writer.into_inner()?;
    /// gzip_writer.finish().into_result()?;
    /// ```
    ///
    pub fn into_inner(self) -> Result<W, std::io::IntoInnerError<BufWriter<W>>> {
        self.writer.into_inner()
    }
}

impl WarcWriter<BufWriter<fs::File>> {
    /// Create a new writer which writes to a file.
    ///
    /// The file is created if it does not exist and truncated if it does, following
    /// [`std::fs::File::create`] semantics: this writer always produces a fresh archive.
    /// To add records to an existing WARC file instead, open the file with
    /// [`std::fs::OpenOptions`] in append mode and pass it to [`WarcWriter::new`].
    pub fn from_path<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = fs::File::create(&path)?;
        let writer = BufWriter::with_capacity(MB, file);

        Ok(WarcWriter::new(writer))
    }
}

#[cfg(test)]
mod from_path_tests {
    use super::WarcWriter;
    use crate::{RawRecordHeader, WarcHeader};

    fn record_with_body(body: &[u8]) -> RawRecordHeader {
        RawRecordHeader {
            version: "1.0".to_owned(),
            headers: vec![
                (WarcHeader::WarcType, b"dunno".to_vec()),
                (
                    WarcHeader::ContentLength,
                    body.len().to_string().into_bytes(),
                ),
            ]
            .into_iter()
            .collect(),
            concurrent_to: Vec::new(),
        }
    }

    #[test]
    fn overwriting_a_longer_file_truncates_it() {
        let long_body = &b"a-long-body-that-outlasts-the-second-record"[..];
        let short_body = &b"short"[..];

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncate.warc");

        let mut writer = WarcWriter::from_path(&path).unwrap();
        writer
            .write_raw(record_with_body(long_body), &long_body)
            .unwrap();
        writer.into_inner().unwrap();

        let mut writer = WarcWriter::from_path(&path).unwrap();
        writer
            .write_raw(record_with_body(short_body), &short_body)
            .unwrap();
        writer.into_inner().unwrap();

        let mut expected_writer = WarcWriter::new(Vec::new());
        expected_writer
            .write_raw(record_with_body(short_body), &short_body)
            .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), expected_writer.writer);
    }
}

#[cfg(test)]
mod write_raw_tests {
    use super::WarcWriter;
    use crate::{RawRecordHeader, WarcHeader};
    use std::io::{self, Write};

    /// A writer that accepts at most one byte per `write` call.
    struct TrickleWriter(Vec<u8>);

    impl Write for TrickleWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            let n = data.len().min(1);
            self.0.extend_from_slice(&data[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_matches_into_raw_parts() {
        let mut record = crate::Record::<crate::BufferedBody>::default();
        record.replace_body(b"body-bytes".to_vec());

        let mut record_writer = WarcWriter::new(Vec::new());
        let record_len = record_writer.write(&record).unwrap();

        let (headers, body) = record.into_raw_parts();
        let mut raw_writer = WarcWriter::new(Vec::new());
        let raw_len = raw_writer.write_raw(headers, &body).unwrap();

        assert_eq!(record_writer.writer, raw_writer.writer);
        assert_eq!(record_len, raw_len);
    }

    /// A written record follows the WARC 1.1 grammar byte for byte: version line, named
    /// fields, and block all terminated by CRLF, with two CRLFs closing the record.
    #[test]
    fn written_record_follows_the_warc_1_1_grammar() {
        let record = crate::RecordBuilder::default()
            .warc_type(crate::RecordType::Response)
            .warc_id("<urn:test:grammar:record-0>")
            .header(WarcHeader::Date, "2020-07-08T02:52:55Z")
            .header(WarcHeader::TargetURI, "https://example.com/")
            .body(b"body".to_vec())
            .build()
            .unwrap();

        let mut writer = WarcWriter::new(Vec::new());
        let bytes_written = writer.write(&record).unwrap();

        let expected: &[u8] = b"WARC/1.1\r\n\
            warc-type: response\r\n\
            warc-record-id: <urn:test:grammar:record-0>\r\n\
            warc-date: 2020-07-08T02:52:55Z\r\n\
            warc-target-uri: https://example.com/\r\n\
            content-length: 4\r\n\
            \r\n\
            body\r\n\
            \r\n";
        assert_eq!(writer.writer.as_slice(), expected);
        assert_eq!(bytes_written, expected.len());
    }

    /// A record written by the writer reads back identically, including a sub-second
    /// `WARC-Date`, which WARC 1.1 permits at up to nanosecond precision.
    #[test]
    fn written_record_round_trips_through_the_reader() {
        let mut record = crate::Record::with_body("payload");
        record
            .set_header(WarcHeader::TargetURI, "https://example.com/a?b=c")
            .unwrap();

        let mut writer = WarcWriter::new(Vec::new());
        writer.write(&record).unwrap();

        let read_back = crate::WarcReader::new(writer.writer.as_slice())
            .iter_records()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(read_back, vec![record]);
    }

    /// Repeated `WARC-Concurrent-To` values are each written as their own header line and
    /// survive a round trip through the reader.
    #[test]
    fn repeated_concurrent_to_round_trips() {
        let mut record = crate::Record::with_body("payload");
        record.add_concurrent_to("<urn:test:concurrent:record-1>");
        record.add_concurrent_to("<urn:test:concurrent:record-2>");

        let mut writer = WarcWriter::new(Vec::new());
        writer.write(&record).unwrap();

        let written = String::from_utf8(writer.writer.clone()).unwrap();
        assert_eq!(
            written
                .matches("warc-concurrent-to: <urn:test:concurrent:record-")
                .count(),
            2
        );

        let read_back = crate::WarcReader::new(writer.writer.as_slice())
            .iter_records()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(read_back, vec![record]);
    }

    #[test]
    fn short_writes_do_not_truncate() {
        let headers = RawRecordHeader {
            version: "1.0".to_owned(),
            headers: vec![(WarcHeader::WarcType, b"dunno".to_vec())]
                .into_iter()
                .collect(),
            concurrent_to: Vec::new(),
        };

        let mut writer = WarcWriter::new(TrickleWriter(Vec::new()));
        let bytes_written = writer.write_raw(headers, b"12345").unwrap();

        let expected: &[u8] = b"WARC/1.0\r\nwarc-type: dunno\r\n\r\n12345\r\n\r\n";
        assert_eq!(writer.writer.0.as_slice(), expected);
        assert_eq!(bytes_written, expected.len());
    }
}

#[cfg(feature = "gzip")]
impl WarcWriter<BufWriter<GzipWriter<std::fs::File>>> {
    /// Create a new writer which writes to a GZIP-compressed file.
    ///
    /// The file is created if it does not exist and truncated if it does, following
    /// [`std::fs::File::create`] semantics: this writer always produces a fresh archive.
    /// To add records to an existing compressed WARC file instead, open the file with
    /// [`std::fs::OpenOptions`] in append mode and wrap it in a new gzip encoder passed to
    /// [`WarcWriter::new`]; the appended records form a new gzip member, which is valid in
    /// a multi-member WARC file.
    pub fn from_path_gzip<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = fs::File::create(&path)?;
        let gzip_stream = GzipWriter::new(file)?;
        let writer = BufWriter::with_capacity(MB, gzip_stream);

        Ok(WarcWriter::new(writer))
    }
}
