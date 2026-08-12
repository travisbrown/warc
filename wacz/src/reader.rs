//! Reading the members of an existing WACZ file.

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use libflate::gzip::MultiDecoder;
use warc::WarcReader;
use zip::ZipArchive;
use zip::result::ZipError;

use crate::cdxj::IndexReader;
use crate::digest::Sha256Digest;
use crate::frictionless::{DataPackage, DataPackageDigest};
use crate::pages::PageListReader;
use crate::{
    ARCHIVE_PREFIX, DATA_PACKAGE_DIGEST_PATH, DATA_PACKAGE_PATH, GZIP_EXTENSION, INDEXES_PREFIX,
    PAGES_PATH,
};

/// An error type for WACZ reading.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying stream could not be read.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The ZIP container is invalid.
    #[error(transparent)]
    Zip(#[from] ZipError),
    /// A requested member is not present in the archive.
    #[error("missing member: {0}")]
    MissingMember(String),
    /// The `datapackage.json` manifest could not be parsed.
    #[error("invalid data package manifest")]
    InvalidDataPackage(#[source] serde_json::Error),
    /// The `datapackage-digest.json` file could not be parsed.
    #[error("invalid data package digest")]
    InvalidDataPackageDigest(#[source] serde_json::Error),
    /// A page list member could not be read.
    #[error(transparent)]
    Pages(#[from] crate::pages::Error),
}

/// A buffered stream over a single member of a WACZ file, decompressed if it was gzip data.
pub type MemberReader<'a> = BufReader<Box<dyn Read + 'a>>;

/// The outcome of verifying the members of a WACZ file against its manifest.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Verification {
    /// The paths of members whose digests and sizes matched the manifest. Includes the manifest
    /// itself when a digest file is present and matches.
    pub verified: Vec<String>,
    /// The paths of members whose contents did not match the manifest.
    pub mismatched: Vec<String>,
    /// The paths of members listed in the manifest but absent from the archive.
    pub missing: Vec<String>,
}

impl Verification {
    /// Whether every check passed.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.mismatched.is_empty() && self.missing.is_empty()
    }
}

/// A reader over the members of a WACZ file.
///
/// The underlying ZIP archive yields one decompressed stream at a time, so the member accessors
/// borrow the reader mutably and only one member can be read at once.
pub struct WaczReader<R> {
    archive: ZipArchive<R>,
}

impl WaczReader<BufReader<File>> {
    /// Open a WACZ file for reading.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        Self::new(BufReader::new(File::open(path)?))
    }
}

impl<R: Read + Seek> WaczReader<R> {
    /// Create a new reader, parsing the ZIP central directory.
    pub fn new(reader: R) -> Result<Self, Error> {
        Ok(Self {
            archive: ZipArchive::new(reader)?,
        })
    }

    /// Read and parse the `datapackage.json` manifest.
    pub fn data_package(&mut self) -> Result<DataPackage<'static>, Error> {
        let bytes = self.member_bytes(DATA_PACKAGE_PATH)?;

        serde_json::from_slice::<DataPackage<'_>>(&bytes)
            .map(DataPackage::into_owned)
            .map_err(Error::InvalidDataPackage)
    }

    /// Read and parse the `datapackage-digest.json` file.
    ///
    /// Returns `None` when the file is absent, since the specification only recommends it.
    pub fn data_package_digest(&mut self) -> Result<Option<DataPackageDigest<'static>>, Error> {
        match self.member_bytes(DATA_PACKAGE_DIGEST_PATH) {
            Ok(bytes) => serde_json::from_slice::<DataPackageDigest<'_>>(&bytes)
                .map(|digest| Some(digest.into_owned()))
                .map_err(Error::InvalidDataPackageDigest),
            Err(Error::MissingMember(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Read the required `pages/pages.jsonl` page list.
    pub fn pages(&mut self) -> Result<PageListReader<MemberReader<'_>>, Error> {
        self.page_list(PAGES_PATH)
    }

    /// Read a page list member by path (for example `pages/extraPages.jsonl`).
    pub fn page_list(&mut self, path: &str) -> Result<PageListReader<MemberReader<'_>>, Error> {
        let member = self.member_stream(path)?;

        Ok(PageListReader::new(member)?)
    }

    /// The paths of the WARC members of the archive, in unspecified order.
    pub fn warc_paths(&self) -> impl Iterator<Item = &str> {
        self.archive
            .file_names()
            .filter(|name| name.starts_with(ARCHIVE_PREFIX))
    }

    /// The paths of the index members of the archive, in unspecified order.
    pub fn index_paths(&self) -> impl Iterator<Item = &str> {
        self.archive
            .file_names()
            .filter(|name| name.starts_with(INDEXES_PREFIX))
    }

    /// Read a CDXJ index member by path, decoding gzip members by extension.
    pub fn index(&mut self, path: &str) -> Result<IndexReader<MemberReader<'_>>, Error> {
        Ok(IndexReader::new(self.member_stream(path)?))
    }

    /// Read a WARC member by path, decoding gzip members by extension.
    pub fn warc(&mut self, path: &str) -> Result<WarcReader<MemberReader<'_>>, Error> {
        Ok(WarcReader::new(self.member_stream(path)?))
    }

    /// Verify the members of the archive against the manifest, and the manifest against the
    /// digest file if one is present.
    ///
    /// Missing or corrupt members are reported in the result rather than treated as errors.
    pub fn verify(&mut self) -> Result<Verification, Error> {
        let manifest_bytes = self.member_bytes(DATA_PACKAGE_PATH)?;
        let package = serde_json::from_slice::<DataPackage<'_>>(&manifest_bytes)
            .map(DataPackage::into_owned)
            .map_err(Error::InvalidDataPackage)?;

        let mut verification = Verification::default();

        if let Some(digest) = self.data_package_digest()? {
            if digest.hash == Sha256Digest::compute(&manifest_bytes) {
                verification.verified.push(DATA_PACKAGE_PATH.to_owned());
            } else {
                verification.mismatched.push(DATA_PACKAGE_PATH.to_owned());
            }
        }

        for resource in &package.resources {
            match self.member(&resource.path) {
                Ok(member) => {
                    let (hash, bytes) = Sha256Digest::from_reader(member)?;

                    if hash == resource.hash && bytes == resource.bytes {
                        verification
                            .verified
                            .push(resource.path.clone().into_owned());
                    } else {
                        verification
                            .mismatched
                            .push(resource.path.clone().into_owned());
                    }
                }
                Err(Error::MissingMember(path)) => verification.missing.push(path),
                Err(error) => return Err(error),
            }
        }

        Ok(verification)
    }

    /// Open a member by path, mapping the ZIP crate's not-found error to a dedicated variant.
    fn member(&mut self, path: &str) -> Result<zip::read::ZipFile<'_, R>, Error> {
        match self.archive.by_name(path) {
            Err(ZipError::FileNotFound) => Err(Error::MissingMember(path.to_owned())),
            result => Ok(result?),
        }
    }

    /// Open a member by path as a buffered stream, decoding gzip members by extension.
    fn member_stream(&mut self, path: &str) -> Result<MemberReader<'_>, Error> {
        let is_gzip = path.ends_with(GZIP_EXTENSION);
        let member = self.member(path)?;

        let stream: Box<dyn Read + '_> = if is_gzip {
            Box::new(MultiDecoder::new(BufReader::new(member))?)
        } else {
            Box::new(member)
        };

        Ok(BufReader::new(stream))
    }

    /// Read the full contents of a member by path.
    fn member_bytes(&mut self, path: &str) -> Result<Vec<u8>, Error> {
        let mut member = self.member(path)?;
        let mut bytes = Vec::with_capacity(usize::try_from(member.size()).unwrap_or(0));
        member.read_to_end(&mut bytes)?;

        Ok(bytes)
    }
}
