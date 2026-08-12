//! The `pages/pages.jsonl` page list format.
//!
//! A page list is a JSON Lines file whose first line is a [`PageListHeader`] identifying the
//! format and naming the list, followed by one [`Page`] entry per line. The `pages/pages.jsonl`
//! member is required in every WACZ file; additional lists (for example `extraPages.jsonl`) may
//! sit alongside it in the `pages/` directory using the same format.

use std::borrow::Cow;
use std::io::{BufRead, Write};

use bounded_static::{IntoBoundedStatic, ToBoundedStatic};
use chrono::{DateTime, SecondsFormat, Utc};
use sha2::Digest as _;

/// The format identifier required in the header line of a page list.
pub const FORMAT: &str = "json-pages-1.0";

/// An error type for page list reading and writing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying stream could not be read or written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The page list ended before a header line was read.
    #[error("missing page list header")]
    MissingHeader,
    /// The header line could not be parsed.
    #[error("invalid page list header")]
    InvalidHeader(#[source] serde_json::Error),
    /// The header line declares a format other than [`FORMAT`].
    #[error("unsupported page list format: {0}")]
    UnsupportedFormat(String),
    /// A page entry line could not be parsed.
    #[error("invalid page list entry on line {line_number}")]
    InvalidEntry {
        /// The underlying deserialization error.
        #[source]
        error: serde_json::Error,
        /// The one-based line number of the invalid entry.
        line_number: usize,
    },
    /// A page entry could not be serialized.
    #[error("invalid page list entry")]
    Serialization(#[source] serde_json::Error),
}

/// The header line of a page list.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PageListHeader<'a> {
    /// The format identifier (always [`FORMAT`]).
    #[serde(borrow)]
    pub format: Cow<'a, str>,
    /// An identifier for the list (`pages` for the required list).
    #[serde(borrow)]
    pub id: Cow<'a, str>,
    /// A display name for the list.
    #[serde(borrow)]
    pub title: Cow<'a, str>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// Implemented by hand because the `extra` map's type has no `bounded_static` support.
impl ToBoundedStatic for PageListHeader<'_> {
    type Static = PageListHeader<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_static()
    }
}

impl IntoBoundedStatic for PageListHeader<'_> {
    type Static = PageListHeader<'static>;

    fn into_static(self) -> Self::Static {
        PageListHeader {
            format: self.format.into_static(),
            id: self.id.into_static(),
            title: self.title.into_static(),
            extra: self.extra,
        }
    }
}

impl Default for PageListHeader<'static> {
    /// The conventional header of the required `pages/pages.jsonl` list.
    fn default() -> Self {
        Self {
            format: Cow::Borrowed(FORMAT),
            id: Cow::Borrowed("pages"),
            title: Cow::Borrowed("All Pages"),
            extra: serde_json::Map::new(),
        }
    }
}

/// A single page entry in a page list.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Page<'a> {
    /// The URL of the archived page.
    #[serde(borrow)]
    pub url: Cow<'a, str>,
    /// When the page was captured.
    pub ts: DateTime<Utc>,
    /// An arbitrary identifier for the page.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<Cow<'a, str>>,
    /// A title describing the page.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<Cow<'a, str>>,
    /// Text content extracted from the page, used for search.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub text: Option<Cow<'a, str>>,
    /// The size in bytes of the page and all of its subresources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// Implemented by hand because the `extra` map's type has no `bounded_static` support.
impl ToBoundedStatic for Page<'_> {
    type Static = Page<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_static()
    }
}

impl IntoBoundedStatic for Page<'_> {
    type Static = Page<'static>;

    fn into_static(self) -> Self::Static {
        Page {
            url: self.url.into_static(),
            ts: self.ts,
            id: self.id.into_static(),
            title: self.title.into_static(),
            text: self.text.into_static(),
            size: self.size,
            extra: self.extra,
        }
    }
}

/// A reader which iteratively parses page entries from a page list stream.
pub struct PageListReader<R> {
    underlying: R,
    header: PageListHeader<'static>,
    /// Scratch buffer reused across lines. Parsed pages are converted to owned values, so nothing
    /// borrows from it once [`Iterator::next`] returns.
    line: String,
    line_number: usize,
}

impl<R: BufRead> PageListReader<R> {
    /// Create a new reader, reading and validating the header line.
    ///
    /// # Errors
    ///
    /// Fails if the stream is empty, if the header line is not valid JSON, or if the header
    /// declares a format other than [`FORMAT`].
    pub fn new(mut reader: R) -> Result<Self, Error> {
        let mut line = String::new();

        if reader.read_line(&mut line)? == 0 {
            return Err(Error::MissingHeader);
        }

        let header =
            serde_json::from_str::<PageListHeader<'_>>(line.trim_end_matches(['\r', '\n']))
                .map_err(Error::InvalidHeader)?;

        if header.format != FORMAT {
            return Err(Error::UnsupportedFormat(header.format.into_owned()));
        }

        let header = header.into_static();
        line.clear();

        Ok(Self {
            underlying: reader,
            header,
            line,
            line_number: 1,
        })
    }

    /// The parsed header line.
    #[must_use]
    pub const fn header(&self) -> &PageListHeader<'static> {
        &self.header
    }
}

impl<R: BufRead> Iterator for PageListReader<R> {
    type Item = Result<Page<'static>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            self.line.clear();

            match self.underlying.read_line(&mut self.line) {
                Ok(0) => return None,
                Ok(_) => {
                    self.line_number += 1;
                    let content = self.line.trim_end_matches(['\r', '\n']);

                    // Blank lines (such as a trailing newline at the end of the file) are skipped
                    // rather than treated as invalid entries.
                    if content.is_empty() {
                        continue;
                    }

                    let line_number = self.line_number;

                    return Some(
                        serde_json::from_str::<Page<'_>>(content)
                            .map(IntoBoundedStatic::into_static)
                            .map_err(|error| Error::InvalidEntry { error, line_number }),
                    );
                }
                Err(error) => return Some(Err(Error::Io(error))),
            }
        }
    }
}

/// The synthetic identifier for a page: a truncated SHA-256 hash of its timestamp and URL.
///
/// The hash input is the concatenation of the timestamp in RFC 3339 format (UTC, `Z` suffix,
/// exactly as it is serialized in the page entry) and the URL; the identifier is the first
/// `length` characters of the lowercase hexadecimal digest. Lengths above 64 yield the full
/// digest.
#[must_use]
pub fn synthetic_id(ts: &DateTime<Utc>, url: &str, length: usize) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(ts.to_rfc3339_opts(SecondsFormat::AutoSi, true));
    hasher.update(url);

    let mut id = data_encoding::HEXLOWER.encode(&hasher.finalize());
    id.truncate(length);
    id
}

/// Write a page list: a header line followed by one JSON line per page.
///
/// # Errors
///
/// Fails if the underlying stream cannot be written or if an entry cannot be serialized.
pub fn write_page_list<'p, W: Write, I: IntoIterator<Item = &'p Page<'p>>>(
    mut writer: W,
    header: &PageListHeader<'_>,
    pages: I,
) -> Result<(), Error> {
    write_line(&mut writer, header)?;

    for page in pages {
        write_line(&mut writer, page)?;
    }

    Ok(())
}

/// Write one value as a JSON line, distinguishing stream failures from serialization failures.
fn write_line<W: Write, T: serde::ser::Serialize>(writer: &mut W, value: &T) -> Result<(), Error> {
    serde_json::to_writer(&mut *writer, value).map_err(|error| {
        if error.is_io() {
            Error::Io(error.into())
        } else {
            Error::Serialization(error)
        }
    })?;

    writer.write_all(b"\n")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = concat!(
        "{\"format\": \"json-pages-1.0\", \"id\": \"pages\", \"title\": \"All Pages\"}\n",
        "{\"id\": \"1db0ef709a\", \"url\": \"https://www.example.com/page\", \"size\": 1256, ",
        "\"ts\": \"2020-10-07T21:22:36Z\", \"title\": \"Example Domain\", \"custom\": true}\n",
        "{\"url\": \"https://www.example.com/another\", \"ts\": \"2020-10-07T21:23:36Z\"}\n",
    );

    #[test]
    fn read_example_page_list() -> Result<(), Box<dyn std::error::Error>> {
        let reader = PageListReader::new(EXAMPLE.as_bytes())?;

        assert_eq!(reader.header().id, "pages");

        let pages = reader.collect::<Result<Vec<_>, _>>()?;

        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].url, "https://www.example.com/page");
        assert_eq!(pages[0].size, Some(1256));
        assert_eq!(pages[0].extra["custom"], serde_json::Value::Bool(true));
        assert_eq!(pages[1].id, None);

        Ok(())
    }

    #[test]
    fn read_rejects_unsupported_format() {
        let result =
            PageListReader::new(&b"{\"format\": \"other\", \"id\": \"x\", \"title\": \"y\"}\n"[..]);

        assert!(matches!(result, Err(Error::UnsupportedFormat(format)) if format == "other"));
    }

    #[test]
    fn read_reports_entry_line_numbers() -> Result<(), Box<dyn std::error::Error>> {
        let mut reader = PageListReader::new(
            &b"{\"format\": \"json-pages-1.0\", \"id\": \"pages\", \"title\": \"t\"}\nnot json\n"[..],
        )?;

        assert!(matches!(
            reader.next(),
            Some(Err(Error::InvalidEntry { line_number: 2, .. }))
        ));

        Ok(())
    }

    #[test]
    fn synthetic_id_matches_known_value() {
        // Externally computed: sha256("2020-10-07T21:22:36Zhttps://www.example.com/page").
        const DIGEST: &str = "f5ca709e5e9363c834323853295995cc0df353276b4811df37034f2bab360bbd";

        let ts = DateTime::parse_from_rfc3339("2020-10-07T21:22:36Z")
            .expect("valid timestamp")
            .to_utc();
        let url = "https://www.example.com/page";

        assert_eq!(synthetic_id(&ts, url, 10), DIGEST[..10]);
        assert_eq!(synthetic_id(&ts, url, 64), DIGEST);
        // Lengths above the digest length yield the full digest.
        assert_eq!(synthetic_id(&ts, url, 100), DIGEST);
    }

    #[test]
    fn write_read_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let original = PageListReader::new(EXAMPLE.as_bytes())?;
        let header = original.header().clone();
        let pages = original.collect::<Result<Vec<_>, _>>()?;

        let mut buffer = Vec::new();
        write_page_list(&mut buffer, &header, &pages)?;

        let reader = PageListReader::new(buffer.as_slice())?;

        assert_eq!(reader.header(), &header);
        assert_eq!(reader.collect::<Result<Vec<_>, _>>()?, pages);

        Ok(())
    }
}
