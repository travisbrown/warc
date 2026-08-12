//! The `datapackage.json` manifest and `datapackage-digest.json` formats.
//!
//! A WACZ manifest is a [Frictionless Data Package](https://specs.frictionlessdata.io/data-package/)
//! descriptor that enumerates every other member of the archive together with its size and
//! SHA-256 digest. The digest file in turn records the digest of the serialized manifest itself
//! (and optionally a cryptographic signature over it), so that a single hash comparison verifies
//! the integrity of the entire collection.
//!
//! Parsing is lenient: properties beyond those modeled here are preserved in [`DataPackage::extra`]
//! so that manifests written by other tools survive a read-modify-write cycle.

use std::borrow::Cow;

use chrono::{DateTime, Utc};

use crate::attributes;
use crate::digest::Sha256Digest;

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

/// The `signedData` signature envelope of a digest file, as defined by the
/// [WACZ signing specification](https://specs.webrecorder.net/wacz-auth/latest/).
///
/// The specification defines two formats, distinguished by their fields: an anonymous
/// signature carrying only a public key, and a domain-ownership identity signature carrying a
/// certificate chain and a signed timestamp.
///
/// The specification requires that the envelope contain no properties beyond those of its
/// format; parsing is correspondingly strict, and an envelope with unlisted properties (or
/// with fields from both formats) is rejected.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum SignatureData<'a> {
    /// A signature validated by a TLS certificate for a domain, countersigned by an RFC 3161
    /// timestamp server.
    DomainIdentity(#[serde(borrow)] DomainIdentitySignature<'a>),
    /// A signature validated by a bare public key, distributed out-of-band.
    Anonymous(#[serde(borrow)] AnonymousSignature<'a>),
}

impl SignatureData<'_> {
    /// The SHA-256 digest of `datapackage.json` that was signed.
    #[must_use]
    pub const fn hash(&self) -> &Sha256Digest {
        match self {
            Self::DomainIdentity(signature) => &signature.hash,
            Self::Anonymous(signature) => &signature.hash,
        }
    }

    /// When the signature was created.
    #[must_use]
    pub const fn created(&self) -> &DateTime<Utc> {
        match self {
            Self::DomainIdentity(signature) => &signature.created,
            Self::Anonymous(signature) => &signature.created,
        }
    }

    /// The software that created the signature.
    #[must_use]
    pub fn software(&self) -> &str {
        match self {
            Self::DomainIdentity(signature) => &signature.software,
            Self::Anonymous(signature) => &signature.software,
        }
    }

    /// The version of the software that created the signature.
    #[must_use]
    pub fn version(&self) -> &str {
        match self {
            Self::DomainIdentity(signature) => &signature.version,
            Self::Anonymous(signature) => &signature.version,
        }
    }

    /// The base64-encoded signature of [`hash`](Self::hash).
    #[must_use]
    pub fn signature(&self) -> &str {
        match self {
            Self::DomainIdentity(signature) => &signature.signature,
            Self::Anonymous(signature) => &signature.signature,
        }
    }

    /// Convert into a signature envelope that owns all of its data.
    #[must_use]
    pub fn into_owned(self) -> SignatureData<'static> {
        match self {
            Self::DomainIdentity(signature) => {
                SignatureData::DomainIdentity(signature.into_owned())
            }
            Self::Anonymous(signature) => SignatureData::Anonymous(signature.into_owned()),
        }
    }
}

/// An anonymous signature: validated by a bare public key, with authorship established by
/// distributing the key out-of-band.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnonymousSignature<'a> {
    /// The SHA-256 digest of `datapackage.json` that was signed.
    pub hash: Sha256Digest,
    /// When the signature was created.
    pub created: DateTime<Utc>,
    /// The software that created the signature.
    #[serde(borrow)]
    pub software: Cow<'a, str>,
    /// The version of the software that created the signature.
    #[serde(borrow)]
    pub version: Cow<'a, str>,
    /// The base64-encoded signature of [`hash`](Self::hash).
    #[serde(borrow)]
    pub signature: Cow<'a, str>,
    /// The base64-encoded ECDSA public key validating [`signature`](Self::signature).
    #[serde(borrow)]
    pub public_key: Cow<'a, str>,
}

impl AnonymousSignature<'_> {
    /// Convert into a signature that owns all of its data.
    #[must_use]
    pub fn into_owned(self) -> AnonymousSignature<'static> {
        AnonymousSignature {
            hash: self.hash,
            created: self.created,
            software: attributes::into_owned(self.software),
            version: attributes::into_owned(self.version),
            signature: attributes::into_owned(self.signature),
            public_key: attributes::into_owned(self.public_key),
        }
    }
}

/// A domain-ownership identity signature: validated by a TLS certificate for
/// [`domain`](Self::domain), with the signature itself countersigned by an RFC 3161 timestamp
/// server to attest to the creation time.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DomainIdentitySignature<'a> {
    /// The SHA-256 digest of `datapackage.json` that was signed.
    pub hash: Sha256Digest,
    /// When the signature was created.
    pub created: DateTime<Utc>,
    /// The software that created the signature.
    #[serde(borrow)]
    pub software: Cow<'a, str>,
    /// The version of the software that created the signature.
    #[serde(borrow)]
    pub version: Cow<'a, str>,
    /// The base64-encoded signature of [`hash`](Self::hash) by the key of
    /// [`domain_cert`](Self::domain_cert).
    #[serde(borrow)]
    pub signature: Cow<'a, str>,
    /// The hostname whose certificate created the signature.
    #[serde(borrow)]
    pub domain: Cow<'a, str>,
    /// The PEM certificate chain validating [`signature`](Self::signature).
    #[serde(borrow)]
    pub domain_cert: Cow<'a, str>,
    /// The base64-encoded RFC 3161 timestamp server signature of
    /// [`signature`](Self::signature).
    #[serde(borrow)]
    pub time_signature: Cow<'a, str>,
    /// The PEM certificate chain validating [`time_signature`](Self::time_signature).
    #[serde(borrow)]
    pub timestamp_cert: Cow<'a, str>,
    /// An optional cross-signed PEM certificate chain providing an alternative trust path for
    /// [`signature`](Self::signature), should the domain certificate be compromised.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub cross_signed_cert: Option<Cow<'a, str>>,
}

impl DomainIdentitySignature<'_> {
    /// Convert into a signature that owns all of its data.
    #[must_use]
    pub fn into_owned(self) -> DomainIdentitySignature<'static> {
        DomainIdentitySignature {
            hash: self.hash,
            created: self.created,
            software: attributes::into_owned(self.software),
            version: attributes::into_owned(self.version),
            signature: attributes::into_owned(self.signature),
            domain: attributes::into_owned(self.domain),
            domain_cert: attributes::into_owned(self.domain_cert),
            time_signature: attributes::into_owned(self.time_signature),
            timestamp_cert: attributes::into_owned(self.timestamp_cert),
            cross_signed_cert: attributes::into_owned_option(self.cross_signed_cert),
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

    /// A digest file with an anonymous signature envelope, shaped as in the signing
    /// specification.
    const ANONYMOUS_DIGEST: &str = r#"{
        "path": "datapackage.json",
        "hash": "sha256:ec1f44ab13e2c94b0ddf66e9673d585ba4a77e6f8c9cc30d8665da434557e885",
        "signedData": {
            "hash": "sha256:ec1f44ab13e2c94b0ddf66e9673d585ba4a77e6f8c9cc30d8665da434557e885",
            "created": "2020-10-07T21:22:36Z",
            "software": "example-signer",
            "version": "0.1.0",
            "signature": "base64-encoded-signature",
            "publicKey": "base64-encoded-public-key"
        }
    }"#;

    /// A digest file with a domain-identity signature envelope, shaped as in the signing
    /// specification.
    const DOMAIN_DIGEST: &str = r#"{
        "path": "datapackage.json",
        "hash": "sha256:ec1f44ab13e2c94b0ddf66e9673d585ba4a77e6f8c9cc30d8665da434557e885",
        "signedData": {
            "hash": "sha256:ec1f44ab13e2c94b0ddf66e9673d585ba4a77e6f8c9cc30d8665da434557e885",
            "created": "2020-10-07T21:22:36Z",
            "software": "example-signer",
            "version": "0.1.0",
            "signature": "base64-encoded-signature",
            "domain": "signing.example.com",
            "domainCert": "pem-certificate-chain",
            "timeSignature": "base64-encoded-timestamp-signature",
            "timestampCert": "pem-timestamp-certificate-chain"
        }
    }"#;

    #[test]
    fn deserialize_anonymous_signature() -> Result<(), Box<dyn std::error::Error>> {
        let digest = serde_json::from_str::<DataPackageDigest<'_>>(ANONYMOUS_DIGEST)?;
        let signed_data = digest.signed_data.expect("signature should be present");

        assert_eq!(signed_data.hash(), &digest.hash);
        assert_eq!(signed_data.software(), "example-signer");
        assert_eq!(signed_data.version(), "0.1.0");
        assert_eq!(signed_data.signature(), "base64-encoded-signature");

        match signed_data {
            SignatureData::Anonymous(signature) => {
                assert_eq!(signature.public_key, "base64-encoded-public-key");
            }
            SignatureData::DomainIdentity(_) => panic!("expected an anonymous signature"),
        }

        Ok(())
    }

    #[test]
    fn deserialize_domain_identity_signature() -> Result<(), Box<dyn std::error::Error>> {
        let digest = serde_json::from_str::<DataPackageDigest<'_>>(DOMAIN_DIGEST)?;
        let signed_data = digest.signed_data.expect("signature should be present");

        assert_eq!(signed_data.hash(), &digest.hash);

        match signed_data {
            SignatureData::DomainIdentity(signature) => {
                assert_eq!(signature.domain, "signing.example.com");
                assert_eq!(signature.domain_cert, "pem-certificate-chain");
                assert_eq!(
                    signature.time_signature,
                    "base64-encoded-timestamp-signature"
                );
                assert_eq!(signature.timestamp_cert, "pem-timestamp-certificate-chain");
                assert_eq!(signature.cross_signed_cert, None);
            }
            SignatureData::Anonymous(_) => panic!("expected a domain-identity signature"),
        }

        Ok(())
    }

    #[test]
    fn signature_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        // A domain-identity envelope with the optional cross-signed certificate present.
        let mut cross_signed = serde_json::from_str::<serde_json::Value>(DOMAIN_DIGEST)?;
        cross_signed["signedData"]["crossSignedCert"] =
            serde_json::Value::String("pem-cross-signed-chain".to_owned());

        for original in [
            ANONYMOUS_DIGEST.to_owned(),
            DOMAIN_DIGEST.to_owned(),
            serde_json::to_string(&cross_signed)?,
        ] {
            let digest = serde_json::from_str::<DataPackageDigest<'_>>(&original)?.into_owned();
            let encoded = serde_json::to_string(&digest)?;

            assert_eq!(
                serde_json::from_str::<DataPackageDigest<'_>>(&encoded)?,
                digest
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&encoded)?,
                serde_json::from_str::<serde_json::Value>(&original)?
            );
        }

        Ok(())
    }

    /// The specification forbids properties beyond those of each signature format.
    #[test]
    fn signature_rejects_unlisted_properties() -> Result<(), Box<dyn std::error::Error>> {
        for original in [ANONYMOUS_DIGEST, DOMAIN_DIGEST] {
            let mut value = serde_json::from_str::<serde_json::Value>(original)?;
            value["signedData"]["custom"] = serde_json::Value::Bool(true);
            let augmented = serde_json::to_string(&value)?;

            assert!(serde_json::from_str::<DataPackageDigest<'_>>(&augmented).is_err());
        }

        Ok(())
    }

    /// An envelope mixing the fields of both signature formats conforms to neither.
    #[test]
    fn signature_rejects_mixed_formats() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = serde_json::from_str::<serde_json::Value>(DOMAIN_DIGEST)?;
        value["signedData"]["publicKey"] =
            serde_json::Value::String("base64-encoded-public-key".to_owned());
        let augmented = serde_json::to_string(&value)?;

        assert!(serde_json::from_str::<DataPackageDigest<'_>>(&augmented).is_err());

        Ok(())
    }
}
