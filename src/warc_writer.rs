use crate::{BufferedBody, RawRecordHeader, Record};

use std::fs;
use std::io;
use std::io::{BufWriter, Write};
use std::path::Path;

#[cfg(feature = "gzip")]
use libflate::gzip::Encoder as GzipWriter;

const MB: usize = 1_048_576;

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
    pub fn from_path<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
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
    pub fn from_path_gzip<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let gzip_stream = GzipWriter::new(file)?;
        let writer = BufWriter::with_capacity(MB, gzip_stream);

        Ok(WarcWriter::new(writer))
    }
}
