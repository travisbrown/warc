use crate::{BufferedBody, MB, RawRecordHeader, Record};

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
        let (headers, body) = record.clone().into_raw_parts();
        self.write_raw(headers, &body)
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
    #[cfg_attr(
        feature = "gzip",
        doc = r#"

```
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let dir = tempfile::tempdir()?;
let writer = warc::WarcWriter::from_path_gzip(dir.path().join("example.warc.gz"))?;
// ... write records ...
let gzip_stream = writer
    .into_inner()
    .map_err(std::io::IntoInnerError::into_error)?;
gzip_stream.finish().into_result()?;
# Ok(())
# }
```"#
    )]
    pub fn into_inner(self) -> Result<W, std::io::IntoInnerError<BufWriter<W>>> {
        self.writer.into_inner()
    }
}

impl WarcWriter<BufWriter<fs::File>> {
    /// Create a new writer which writes to a file.
    ///
    /// The file is created if it does not exist and appended to if it does: existing
    /// records are never overwritten, and the result is a valid archive because WARC files
    /// are defined to be concatenable. To overwrite an existing file with a fresh archive
    /// instead, create the file with [`std::fs::File::create`] and pass it to
    /// [`WarcWriter::new`].
    pub fn from_path<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .create(true)
            .truncate(false)
            .append(true)
            .open(&path)?;
        let writer = BufWriter::with_capacity(MB, file);

        Ok(WarcWriter::new(writer))
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
    fn short_writes_do_not_truncate() {
        let headers = RawRecordHeader {
            version: "1.0".to_owned(),
            headers: vec![(WarcHeader::WarcType, b"dunno".to_vec())]
                .into_iter()
                .collect(),
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
    /// The file is created if it does not exist and appended to if it does: existing
    /// records are never overwritten, and the appended records form a new gzip member,
    /// which is valid in a compressed WARC file and is what `WarcReader::from_path_gzip`
    /// reads. To overwrite an existing file with a fresh archive instead, create the file
    /// with [`std::fs::File::create`], wrap it in a gzip encoder, and pass it to
    /// [`WarcWriter::new`].
    pub fn from_path_gzip<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .create(true)
            .truncate(false)
            .append(true)
            .open(&path)?;
        let gzip_stream = GzipWriter::new(file)?;
        let writer = BufWriter::with_capacity(MB, gzip_stream);

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
        }
    }

    #[test]
    fn reopening_an_existing_file_appends_to_it() {
        let first_body = &b"the-first-record-written"[..];
        let second_body = &b"appended-later"[..];

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("append.warc");

        let mut writer = WarcWriter::from_path(&path).unwrap();
        writer
            .write_raw(record_with_body(first_body), &first_body)
            .unwrap();
        writer.into_inner().unwrap();

        let mut writer = WarcWriter::from_path(&path).unwrap();
        writer
            .write_raw(record_with_body(second_body), &second_body)
            .unwrap();
        writer.into_inner().unwrap();

        let mut expected_writer = WarcWriter::new(Vec::new());
        expected_writer
            .write_raw(record_with_body(first_body), &first_body)
            .unwrap();
        expected_writer
            .write_raw(record_with_body(second_body), &second_body)
            .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), expected_writer.writer);
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn reopening_an_existing_gzip_file_appends_a_new_member() {
        let first_body = b"the-first-record-written".to_vec();
        let second_body = b"appended-later".to_vec();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("append.warc.gz");

        for body in [&first_body, &second_body] {
            let mut writer = WarcWriter::from_path_gzip(&path).unwrap();
            writer.write_raw(record_with_body(body), body).unwrap();
            // The compression stream must be finish()ed, or the member will be truncated.
            let gzip_stream = writer
                .into_inner()
                .map_err(std::io::IntoInnerError::into_error)
                .unwrap();
            gzip_stream.finish().into_result().unwrap();
        }

        let reader = crate::WarcReader::from_path_gzip(&path).unwrap();
        let bodies: Vec<Vec<u8>> = reader
            .iter_raw_records()
            .map(|record| record.unwrap().1)
            .collect();
        assert_eq!(bodies, vec![first_body, second_body]);
    }
}
