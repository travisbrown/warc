//! Assembling a new WACZ file.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sha2::Digest as _;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::cdxj;
use crate::datapackage::{DataPackage, DataPackageDigest, PROFILE, Resource, WACZ_VERSION};
use crate::digest::Sha256Digest;
use crate::pages::{self, Page, PageListHeader};
use crate::{
    ARCHIVE_PREFIX, DATA_PACKAGE_DIGEST_PATH, DATA_PACKAGE_PATH, GZIP_EXTENSION, INDEXES_PREFIX,
    PAGES_PREFIX,
};

/// The default `software` manifest property written by this crate.
const SOFTWARE: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// The default number of characters in synthetic page identifiers.
const DEFAULT_PAGE_ID_LENGTH: usize = 24;

/// Configuration for WACZ creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriterConfig {
    /// The number of characters in the synthetic identifiers given to pages written without one
    /// (see [`pages::synthetic_id`]).
    pub page_id_length: usize,
}

impl Default for WriterConfig {
    /// The default configuration: 24-character synthetic page identifiers.
    fn default() -> Self {
        Self {
            page_id_length: DEFAULT_PAGE_ID_LENGTH,
        }
    }
}

/// An error type for WACZ writing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying stream could not be written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The ZIP container could not be written.
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    /// A page list could not be written.
    #[error(transparent)]
    Pages(#[from] crate::pages::Error),
    /// The data package manifest could not be serialized.
    #[error("invalid data package manifest")]
    Manifest(#[source] serde_json::Error),
    /// A file to be added under `archive/` does not have a usable UTF-8 file name.
    #[error("invalid WARC file name: {}", .0.display())]
    InvalidFileName(PathBuf),
}

/// The contextual manifest properties written by [`WaczWriter::finish`].
///
/// All fields are optional; `created` defaults to the current time and `software` to this
/// crate's name and version.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PackageMetadata {
    /// A short description of the collection.
    pub title: Option<String>,
    /// A longer, possibly Markdown-formatted, description of the collection.
    pub description: Option<String>,
    /// When the WACZ file was created.
    pub created: Option<DateTime<Utc>>,
    /// When the WACZ file was last modified.
    pub modified: Option<DateTime<Utc>>,
    /// A description of the software that created the WACZ file.
    pub software: Option<String>,
    /// The URL of the primary entry page for replay.
    pub main_page_url: Option<String>,
    /// The capture date to use when replaying the primary entry page.
    pub main_page_date: Option<DateTime<Utc>>,
}

/// A writer which assembles a WACZ file, tracking the digest and size of every member so that the
/// manifest and digest files can be written by a final consuming [`finish`](Self::finish) call.
pub struct WaczWriter<W: Write + Seek> {
    zip: ZipWriter<W>,
    resources: Vec<Resource<'static>>,
    config: WriterConfig,
}

impl WaczWriter<BufWriter<File>> {
    /// Create a WACZ file at the given path, refusing to overwrite an existing file.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        Ok(Self::new(BufWriter::new(File::create_new(path)?)))
    }
}

impl<W: Write + Seek> WaczWriter<W> {
    /// Create a new writer with the default configuration.
    #[must_use]
    pub fn new(writer: W) -> Self {
        Self::with_config(writer, WriterConfig::default())
    }

    /// Create a new writer with the given configuration.
    #[must_use]
    pub fn with_config(writer: W, config: WriterConfig) -> Self {
        Self {
            zip: ZipWriter::new(writer),
            resources: Vec::new(),
            config,
        }
    }

    /// Add WARC data under `archive/` with the given file name.
    ///
    /// As the specification requires, `archive/` members are always stored in the ZIP without
    /// compression (`STORE`), so that readers can seek to CDX offsets within them. Names
    /// ending in `.gz` must hold gzip data.
    pub fn add_warc<R: Read>(&mut self, name: &str, reader: R) -> Result<(), Error> {
        self.add_resource(&format!("{ARCHIVE_PREFIX}{name}"), reader)
    }

    /// Add a WARC file under `archive/`, using its file name.
    pub fn add_warc_from_path<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
        let path = path.as_ref();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error::InvalidFileName(path.to_path_buf()))?;
        let file = File::open(path)?;

        self.add_warc(name, BufReader::new(file))
    }

    /// Write a plain-text CDXJ index member under `indexes/` with the given file name (which
    /// should therefore not end in `.gz`; conventionally `index.cdx`).
    ///
    /// The items are sorted by key and timestamp as required for binary search.
    pub fn add_index<'a, I: IntoIterator<Item = &'a cdxj::Item<'a>>>(
        &mut self,
        name: &str,
        items: I,
    ) -> Result<(), Error> {
        let mut items = items.into_iter().collect::<Vec<_>>();
        items.sort_unstable_by(|a, b| {
            a.key
                .cmp(&b.key)
                .then_with(|| a.timestamp.cmp(&b.timestamp))
        });

        self.add_member(&format!("{INDEXES_PREFIX}{name}"), deflated(), |writer| {
            for item in items {
                writeln!(writer, "{item}")?;
            }

            Ok(())
        })
    }

    /// Write the required page list at `pages/pages.jsonl`.
    ///
    /// Pages without an identifier are given a synthetic one derived from their timestamp and
    /// URL (see [`pages::synthetic_id`]), truncated to the configured
    /// [`page_id_length`](WriterConfig::page_id_length).
    pub fn add_pages<'a, I: IntoIterator<Item = &'a Page<'a>>>(
        &mut self,
        header: &PageListHeader<'_>,
        pages: I,
    ) -> Result<(), Error> {
        self.add_page_list("pages.jsonl", header, pages)
    }

    /// Write a page list member under `pages/` with the given file name (for example
    /// `extraPages.jsonl`).
    ///
    /// Pages without an identifier are given a synthetic one derived from their timestamp and
    /// URL (see [`pages::synthetic_id`]), truncated to the configured
    /// [`page_id_length`](WriterConfig::page_id_length).
    pub fn add_page_list<'a, I: IntoIterator<Item = &'a Page<'a>>>(
        &mut self,
        name: &str,
        header: &PageListHeader<'_>,
        pages: I,
    ) -> Result<(), Error> {
        let id_length = self.config.page_id_length;
        let pages = pages
            .into_iter()
            .map(|page| {
                if page.id.is_some() {
                    Cow::Borrowed(page)
                } else {
                    let mut page = page.clone();
                    page.id = Some(Cow::Owned(pages::synthetic_id(
                        &page.ts, &page.url, id_length,
                    )));
                    Cow::Owned(page)
                }
            })
            .collect::<Vec<_>>();

        self.add_member(&format!("{PAGES_PREFIX}{name}"), deflated(), |writer| {
            Ok(pages::write_page_list(
                writer,
                header,
                pages.iter().map(Cow::as_ref),
            )?)
        })
    }

    /// Add an arbitrary member at the given path, recording it in the manifest.
    ///
    /// Members under `archive/` and paths ending in `.gz` (which must hold gzip data) are
    /// stored in the ZIP without compression (`STORE`), as the specification requires; other
    /// members are DEFLATE-compressed. The specification permits custom members anywhere
    /// outside of the `archive/`, `indexes/`, and `pages/` directories, which are reserved
    /// for the dedicated methods.
    pub fn add_resource<R: Read>(&mut self, path: &str, mut reader: R) -> Result<(), Error> {
        self.add_member(path, options_for(path), |writer| {
            std::io::copy(&mut reader, writer)?;

            Ok(())
        })
    }

    /// Write the manifest and digest members and finish the ZIP, returning the underlying
    /// writer.
    pub fn finish(self, metadata: PackageMetadata) -> Result<W, Error> {
        let Self {
            mut zip,
            resources,
            config: _,
        } = self;

        let package = DataPackage {
            profile: Cow::Borrowed(PROFILE),
            wacz_version: Cow::Borrowed(WACZ_VERSION),
            resources,
            title: metadata.title.map(Cow::Owned),
            description: metadata.description.map(Cow::Owned),
            created: Some(metadata.created.unwrap_or_else(Utc::now)),
            modified: metadata.modified,
            software: Some(
                metadata
                    .software
                    .map_or(Cow::Borrowed(SOFTWARE), Cow::Owned),
            ),
            main_page_url: metadata.main_page_url.map(Cow::Owned),
            main_page_date: metadata.main_page_date,
            extra: serde_json::Map::new(),
        };

        let manifest = serde_json::to_vec_pretty(&package).map_err(Error::Manifest)?;
        zip.start_file(DATA_PACKAGE_PATH, deflated())?;
        zip.write_all(&manifest)?;

        let digest = DataPackageDigest {
            path: Cow::Borrowed(DATA_PACKAGE_PATH),
            hash: Sha256Digest::compute(&manifest),
            signed_data: None,
        };

        let digest_bytes = serde_json::to_vec_pretty(&digest).map_err(Error::Manifest)?;
        zip.start_file(DATA_PACKAGE_DIGEST_PATH, deflated())?;
        zip.write_all(&digest_bytes)?;

        Ok(zip.finish()?)
    }

    /// Start a member, delegate its contents to a closure over a hashing writer, and record the
    /// resulting resource. All member writes share this path so that the manifest is always
    /// consistent with what was written.
    fn add_member<F>(
        &mut self,
        path: &str,
        options: SimpleFileOptions,
        write: F,
    ) -> Result<(), Error>
    where
        F: FnOnce(&mut HashingWriter<&mut ZipWriter<W>>) -> Result<(), Error>,
    {
        self.zip.start_file(path, options)?;

        let mut writer = HashingWriter::new(&mut self.zip);
        write(&mut writer)?;
        let (hash, bytes) = writer.finish();

        self.resources.push(Resource {
            name: Cow::Owned(file_name(path).to_owned()),
            path: Cow::Owned(path.to_owned()),
            hash,
            bytes,
        });

        Ok(())
    }
}

/// A writer which computes the digest and size of the bytes passing through it.
struct HashingWriter<W> {
    underlying: W,
    hasher: sha2::Sha256,
    bytes: u64,
}

impl<W> HashingWriter<W> {
    fn new(underlying: W) -> Self {
        Self {
            underlying,
            hasher: sha2::Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (Sha256Digest, u64) {
        (Sha256Digest(self.hasher.finalize().into()), self.bytes)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.underlying.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.bytes += written as u64;

        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.underlying.flush()
    }
}

/// The ZIP entry options for a member.
///
/// The specification requires the `STORE` method for all `archive/` members (readers seek to
/// CDX offsets within them, which ZIP-level compression would break) and for gzip members
/// anywhere (already-compressed data must not be compressed again); only plain-text members
/// may use `DEFLATE`.
fn options_for(path: &str) -> SimpleFileOptions {
    if path.starts_with(ARCHIVE_PREFIX) || path.ends_with(GZIP_EXTENSION) {
        // `large_file` permits members over the ZIP64 threshold, at a cost of a few bytes of
        // header overhead per member.
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(true)
    } else {
        deflated().large_file(true)
    }
}

/// The ZIP entry options for DEFLATE-compressed members.
fn deflated() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)
}

/// The final segment of a member path, used as the resource name in the manifest.
fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}
