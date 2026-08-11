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
}

impl WaczWriter<BufWriter<File>> {
    /// Create a WACZ file at the given path, refusing to overwrite an existing file.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        Ok(Self::new(BufWriter::new(File::create_new(path)?)))
    }
}

impl<W: Write + Seek> WaczWriter<W> {
    /// Create a new writer.
    #[must_use]
    pub fn new(writer: W) -> Self {
        Self {
            zip: ZipWriter::new(writer),
            resources: Vec::new(),
        }
    }

    /// Add WARC data under `archive/` with the given file name.
    ///
    /// Names ending in `.gz` must hold gzip data, which is stored in the ZIP without
    /// recompression as the specification requires; other members are DEFLATE-compressed.
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
    pub fn add_pages<'a, I: IntoIterator<Item = &'a Page<'a>>>(
        &mut self,
        header: &PageListHeader<'_>,
        pages: I,
    ) -> Result<(), Error> {
        self.add_page_list("pages.jsonl", header, pages)
    }

    /// Write a page list member under `pages/` with the given file name (for example
    /// `extraPages.jsonl`).
    pub fn add_page_list<'a, I: IntoIterator<Item = &'a Page<'a>>>(
        &mut self,
        name: &str,
        header: &PageListHeader<'_>,
        pages: I,
    ) -> Result<(), Error> {
        self.add_member(&format!("{PAGES_PREFIX}{name}"), deflated(), |writer| {
            Ok(pages::write_page_list(writer, header, pages)?)
        })
    }

    /// Add an arbitrary member at the given path, recording it in the manifest.
    ///
    /// Paths ending in `.gz` must hold gzip data, which is stored in the ZIP without
    /// recompression; other members are DEFLATE-compressed. The specification permits custom
    /// members anywhere outside of the `archive/`, `indexes/`, and `pages/` directories, which
    /// are reserved for the dedicated methods.
    pub fn add_resource<R: Read>(&mut self, path: &str, mut reader: R) -> Result<(), Error> {
        self.add_member(path, options_for(path), |writer| {
            std::io::copy(&mut reader, writer)?;

            Ok(())
        })
    }

    /// Write the manifest and digest members and finish the ZIP, returning the underlying
    /// writer.
    pub fn finish(self, metadata: PackageMetadata) -> Result<W, Error> {
        let Self { mut zip, resources } = self;

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

/// The ZIP entry options for a member, storing gzip data without recompression.
fn options_for(path: &str) -> SimpleFileOptions {
    if path.ends_with(GZIP_EXTENSION) {
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
