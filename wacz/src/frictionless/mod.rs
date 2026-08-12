//! The `datapackage.json` manifest and `datapackage-digest.json` formats.
//!
//! A WACZ manifest is a [Frictionless Data Package](https://specs.frictionlessdata.io/data-package/)
//! descriptor that enumerates every other member of the archive together with its size and
//! SHA-256 digest. The digest file in turn records the digest of the serialized manifest itself
//! (and optionally a cryptographic signature over it), so that a single hash comparison verifies
//! the integrity of the entire collection.
//!
//! Parsing is lenient: properties beyond those modeled here are preserved in [`DataPackage::extra`]
//! so that manifests written by other tools survive a read-modify-write cycle. The
//! [`signature`] submodule models the digest file's signature envelope, whose parsing is
//! strict as its specification requires.

use std::borrow::Cow;

use chrono::{DateTime, Utc};

use crate::attributes;
use crate::digest::Sha256Digest;

pub mod signature;

use signature::SignatureData;

/// The Frictionless Data Package profile identifier required by the WACZ specification.
pub const PROFILE: &str = "data-package";

/// The WACZ specification version targeted by this crate.
pub const WACZ_VERSION: &str = "1.1.1";

/// A WACZ `datapackage.json` manifest.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct DataPackage<'a> {
    /// The data package profile identifier (always [`PROFILE`] for WACZ files).
    #[serde(borrow)]
    pub profile: Cow<'a, str>,
    /// The version of the WACZ specification the file conforms to.
    #[serde(borrow)]
    pub wacz_version: Cow<'a, str>,
    /// The members of the archive, excluding the manifest and digest files themselves.
    pub resources: Vec<Resource<'a>>,
    /// A short description of the collection.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<Cow<'a, str>>,
    /// A longer, possibly Markdown-formatted, description of the collection.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<Cow<'a, str>>,
    /// When the WACZ file was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>,
    /// When the WACZ file was last modified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<DateTime<Utc>>,
    /// A description of the software that created the WACZ file.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub software: Option<Cow<'a, str>>,
    /// The URL of the primary entry page for replay.
    #[serde(
        rename = "mainPageUrl",
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub main_page_url: Option<Cow<'a, str>>,
    /// The capture date to use when replaying the primary entry page.
    #[serde(
        rename = "mainPageDate",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub main_page_date: Option<DateTime<Utc>>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl DataPackage<'_> {
    /// Convert into a manifest that owns all of its data.
    #[must_use]
    pub fn into_owned(self) -> DataPackage<'static> {
        DataPackage {
            profile: attributes::into_owned(self.profile),
            wacz_version: attributes::into_owned(self.wacz_version),
            resources: self
                .resources
                .into_iter()
                .map(Resource::into_owned)
                .collect(),
            title: attributes::into_owned_option(self.title),
            description: attributes::into_owned_option(self.description),
            created: self.created,
            modified: self.modified,
            software: attributes::into_owned_option(self.software),
            main_page_url: attributes::into_owned_option(self.main_page_url),
            main_page_date: self.main_page_date,
            extra: self.extra,
        }
    }
}

/// A single member of the archive as listed in the manifest.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Resource<'a> {
    /// The file name of the member (its path's final segment).
    #[serde(borrow)]
    pub name: Cow<'a, str>,
    /// The path of the member relative to the root of the archive.
    #[serde(borrow)]
    pub path: Cow<'a, str>,
    /// The SHA-256 digest of the member's contents.
    pub hash: Sha256Digest,
    /// The size of the member's contents in bytes.
    pub bytes: u64,
}

impl Resource<'_> {
    /// Convert into a resource that owns all of its data.
    #[must_use]
    pub fn into_owned(self) -> Resource<'static> {
        Resource {
            name: attributes::into_owned(self.name),
            path: attributes::into_owned(self.path),
            hash: self.hash,
            bytes: self.bytes,
        }
    }
}

/// A WACZ `datapackage-digest.json` file.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct DataPackageDigest<'a> {
    /// The path of the manifest the digest covers (always `datapackage.json`).
    #[serde(borrow)]
    pub path: Cow<'a, str>,
    /// The SHA-256 digest of the serialized manifest bytes.
    pub hash: Sha256Digest,
    /// A signature over the manifest digest.
    #[serde(
        rename = "signedData",
        borrow,
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub signed_data: Option<SignatureData<'a>>,
}

impl DataPackageDigest<'_> {
    /// Convert into a digest that owns all of its data.
    #[must_use]
    pub fn into_owned(self) -> DataPackageDigest<'static> {
        DataPackageDigest {
            path: attributes::into_owned(self.path),
            hash: self.hash,
            signed_data: self.signed_data.map(SignatureData::into_owned),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example manifest from the WACZ 1.1.1 specification, with contextual properties added.
    const EXAMPLE: &str = r#"{
        "profile": "data-package",
        "wacz_version": "1.1.1",
        "title": "Example collection",
        "created": "2020-10-07T21:22:36Z",
        "mainPageUrl": "https://www.example.com/page",
        "custom": {"key": "value"},
        "resources": [
            {
                "name": "pages.jsonl",
                "path": "pages/pages.jsonl",
                "hash": "sha256:8a7fc0d302700bed02294404a627ddbbf0e35487565b1c6181c729dff8d2fff6",
                "bytes": 75
            },
            {
                "name": "data.warc",
                "path": "archive/data.warc",
                "hash": "sha256:0e7101316ba5d4b66f86a371ee615fbd20f9d3f32d32563ed2c829db062f7714",
                "bytes": 11469796
            }
        ]
    }"#;

    #[test]
    fn deserialize_example_manifest() -> Result<(), Box<dyn std::error::Error>> {
        let package = serde_json::from_str::<DataPackage<'_>>(EXAMPLE)?;

        assert_eq!(package.profile, PROFILE);
        assert_eq!(package.wacz_version, WACZ_VERSION);
        assert_eq!(package.title.as_deref(), Some("Example collection"));
        assert_eq!(
            package.main_page_url.as_deref(),
            Some("https://www.example.com/page")
        );
        assert_eq!(package.resources.len(), 2);
        assert_eq!(package.resources[1].name, "data.warc");
        assert_eq!(package.resources[1].bytes, 11_469_796);
        assert!(package.extra.contains_key("custom"));

        Ok(())
    }

    #[test]
    fn round_trip_preserves_extra_properties() -> Result<(), Box<dyn std::error::Error>> {
        let package = serde_json::from_str::<DataPackage<'_>>(EXAMPLE)?.into_owned();
        let encoded = serde_json::to_string(&package)?;

        assert_eq!(serde_json::from_str::<DataPackage<'_>>(&encoded)?, package);

        Ok(())
    }

    #[test]
    fn deserialize_digest() -> Result<(), Box<dyn std::error::Error>> {
        let digest = serde_json::from_str::<DataPackageDigest<'_>>(
            r#"{
                "path": "datapackage.json",
                "hash": "sha256:ec1f44ab13e2c94b0ddf66e9673d585ba4a77e6f8c9cc30d8665da434557e885"
            }"#,
        )?;

        assert_eq!(digest.path, crate::DATA_PACKAGE_PATH);
        assert!(digest.signed_data.is_none());

        Ok(())
    }
}
