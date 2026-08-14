use crate::header::WarcHeader;

/// An error type returned by WARC header parsing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An error occured identifing or parsing headers.
    #[error("Error parsing headers.")]
    ParseHeaders(#[source] nom::Err<nom::error::Error<Vec<u8>>>),
    /// A header required by the standard is missing from the record. The record was well-formed,
    /// but invalid.
    #[error("Missing required header: {0}")]
    MissingHeader(WarcHeader),
    /// A required header is not well-formed according to the standard.
    #[error("Malformed header: {0}: {1}")]
    MalformedHeader(WarcHeader, String),
    /// The underlying read from the data source failed.
    #[error("Error reading data source.")]
    ReadData(#[source] std::io::Error),
    /// More data was read than expected by the header metadata. The record was well-formed, but
    /// invalid.
    #[error("Read further than expected.")]
    ReadOverflow,
    /// The end of the record's body was found unexpectedly.
    #[error("Unexpected end of body.")]
    UnexpectedEOB,
    /// The `\r\n\r\n` terminator after the record's body was missing or malformed. The record
    /// was read completely, but is invalid.
    #[error("Malformed record terminator.")]
    MalformedRecordTerminator,
}

#[cfg(test)]
mod tests {
    use super::Error;
    use crate::header::WarcHeader;
    use std::error::Error as _;

    /// The derived messages and sources match the previous hand-written implementation.
    #[test]
    fn display_and_source_are_unchanged() {
        let io = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        let parse = nom::Err::Error(nom::error::Error::new(
            b"x".to_vec(),
            nom::error::ErrorKind::Verify,
        ));

        let expectations = [
            (Error::ParseHeaders(parse), "Error parsing headers.", true),
            (
                Error::MissingHeader(WarcHeader::Date),
                "Missing required header: warc-date",
                false,
            ),
            (
                Error::MalformedHeader(WarcHeader::Date, "not a W3C-DTF timestamp".to_string()),
                "Malformed header: warc-date: not a W3C-DTF timestamp",
                false,
            ),
            (Error::ReadData(io), "Error reading data source.", true),
            (Error::ReadOverflow, "Read further than expected.", false),
            (Error::UnexpectedEOB, "Unexpected end of body.", false),
            (
                Error::MalformedRecordTerminator,
                "Malformed record terminator.",
                false,
            ),
        ];

        for (error, message, has_source) in expectations {
            assert_eq!(error.to_string(), message);
            assert_eq!(error.source().is_some(), has_source, "{message}");
        }
    }
}
