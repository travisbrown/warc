//! Reading and writing web archive collections in the
//! [WACZ format](https://specs.webrecorder.net/wacz/1.1.1/).
//!
//! A WACZ file is a ZIP archive that bundles WARC data together with the metadata needed to
//! replay it: a [Frictionless Data Package](https://specs.frictionlessdata.io/data-package/)
//! manifest, a page list, and CDXJ lookup indexes.
//!
//! # Modules
//!
//! - [`cdxj`]: CDXJ index lines mapping searchable URL keys to WARC records
//! - [`datapackage`]: The `datapackage.json` manifest and `datapackage-digest.json` formats
//! - [`digest`]: SHA-256 digests in the `sha256:<hex>` encoding used by WACZ manifests
//! - [`pages`]: The `pages/pages.jsonl` page list format
//! - [`reader`]: Reading the members of an existing WACZ file
//! - [`writer`]: Assembling a new WACZ file
#![warn(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    rust_2018_idioms
)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

mod attributes;
pub mod cdxj;
pub mod datapackage;
pub mod digest;
pub mod pages;
pub mod reader;
pub mod writer;

/// The path of the data package manifest within a WACZ file.
pub const DATA_PACKAGE_PATH: &str = "datapackage.json";

/// The path of the data package digest within a WACZ file.
pub const DATA_PACKAGE_DIGEST_PATH: &str = "datapackage-digest.json";

/// The path of the required page list within a WACZ file.
pub const PAGES_PATH: &str = "pages/pages.jsonl";

/// The directory prefix under which WARC members are stored.
pub const ARCHIVE_PREFIX: &str = "archive/";

/// The directory prefix under which index members are stored.
pub const INDEXES_PREFIX: &str = "indexes/";

/// The directory prefix under which page lists are stored.
pub const PAGES_PREFIX: &str = "pages/";

/// Members with this extension hold gzip data and are stored in the ZIP without recompression.
const GZIP_EXTENSION: &str = ".gz";
