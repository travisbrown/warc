use std::error;
use std::fmt;

use crate::header::WarcHeader;

/// An error type returned by WARC header parsing.
#[derive(Debug)]
pub enum Error {
    /// An error occured identifing or parsing headers.
    ParseHeaders(nom::Err<nom::error::Error<Vec<u8>>>),
    /// A header required by the standard is missing from the record. The record was well-formed,
    /// but invalid.
    MissingHeader(WarcHeader),
    /// A required header is not well-formed according to the standard.
    MalformedHeader(WarcHeader, String),
    /// A named field other than `WARC-Concurrent-To` appeared more than once in a record. The
    /// record was well-formed, but invalid.
    DuplicateHeader(WarcHeader),
    /// The underlying read from the data source failed.
    ReadData(std::io::Error),
    /// More data was read than expected by the header metadata. The record was well-formed, but
    /// invalid.
    ReadOverflow,
    /// The end of the record's body was found unexpectedly.
    UnexpectedEOB,
    /// The `\r\n\r\n` terminator after the record's body was missing or malformed. The record
    /// was read completely, but is invalid.
    MalformedRecordTerminator,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::ParseHeaders(_) => write!(f, "Error parsing headers."),
            Error::MissingHeader(h) => write!(f, "Missing required header: {}", h),
            Error::MalformedHeader(h, r) => {
                write!(f, "Malformed header: {}: {}", h, r)
            }
            Error::DuplicateHeader(h) => write!(f, "Duplicate header: {}", h),
            Error::ReadData(_) => write!(f, "Error reading data source."),
            Error::ReadOverflow => write!(f, "Read further than expected."),
            Error::UnexpectedEOB => write!(f, "Unexpected end of body."),
            Error::MalformedRecordTerminator => write!(f, "Malformed record terminator."),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::ParseHeaders(e) => Some(e),
            Error::ReadData(e) => Some(e),
            _ => None,
        }
    }
}
