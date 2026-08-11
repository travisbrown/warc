#![deny(missing_docs)]
//! A WARC (Web ARChive) library

const MB: usize = 1_048_576;

mod error;
pub use error::Error;

mod warc_reader;
pub use warc_reader::*;
mod warc_writer;
pub use warc_writer::*;

mod header;
pub use header::WarcHeader;

/// Core functions for parsing. Not recommended for direct use.
pub mod parser;

mod record;
pub use record::{BufferedBody, EmptyBody, RawRecordHeader, Record, RecordBuilder, StreamingBody};

mod record_type;
pub use record_type::RecordType;

mod truncated_type;
pub use truncated_type::TruncatedType;
