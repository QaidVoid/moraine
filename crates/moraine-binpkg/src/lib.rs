//! Binary package containers, the binhost index, and binary candidate support.
//!
//! This crate is the binary-package layer of the Moraine package manager. It
//! owns:
//!
//! - [`greenfield`]: the binary package format Moraine produces, with
//!   separately addressable metadata, image, and manifest sections, an
//!   integrity manifest of BLAKE2b and SHA-512 per-section checksums, and an
//!   optional detached signature.
//! - [`xpak`] and [`gpkg`]: read-only importers for the two stock container
//!   formats, recovering their embedded metadata into the canonical
//!   [`metadata::MetadataMap`] without rewriting the source.
//! - [`detect`]: format detection that dispatches between the three readers.
//! - [`index`]: the binhost `Packages` index model with name translation and
//!   use-evaluated dependency keys, plus a local built-package index.
//! - [`resolution`]: binary packages as solver candidates, the usepkg-style
//!   selection policy, and the USE/CHOST/soname compatibility checks.
//! - [`fetch`]: network fetch of a binary package by shelling out to a
//!   configurable command, with manifest and signature verification.
//!
//! Network access and cryptographic verification are confined to this crate.
//! The crate exposes typed errors and never prints.

pub mod compress;
pub mod detect;
pub mod error;
pub mod fetch;
pub mod format;
pub mod gpkg;
pub mod greenfield;
pub mod index;
pub mod metadata;
pub mod moves;
pub mod resolution;
pub mod signature;
pub mod xpak;

pub use compress::Compression;
pub use detect::{Format, detect};
pub use error::{ContainerError, FetchError, IndexError};
pub use format::BinpkgFormat;
pub use index::{PackageEntry, PackagesIndex, build_local_index};
pub use metadata::MetadataMap;
pub use resolution::{
    BinaryCandidate, Eligibility, Rejection, TargetConfig, UsepkgMode, UsepkgPolicy, Verdict,
    check_compatibility,
};
pub use signature::{SignatureConfig, SignaturePolicy};

/// A binary package read into memory: its metadata and decompressed image.
///
/// This is the format-agnostic result of [`read_package`]; callers that do not
/// care which container format a file uses consume this directly.
pub struct Package {
    /// The container format the source used.
    pub format: Format,
    /// The recovered canonical metadata map.
    pub metadata: MetadataMap,
    /// The decompressed image tar bytes.
    pub image: Vec<u8>,
}

/// Read any supported binary package into a format-agnostic [`Package`].
///
/// Detects the container format, dispatches to the matching reader, verifies
/// integrity where the format carries a manifest, and returns the recovered
/// metadata and decompressed image. When `signature` is provided, a present
/// detached signature is verified for the greenfield and GPKG formats.
pub fn read_package(
    bytes: &[u8],
    signature: Option<&signature::SignatureConfig>,
) -> Result<Package, ContainerError> {
    read_package_with_policy(bytes, signature, SignaturePolicy::default())
}

/// Read any supported binary package under an explicit GPKG signature `policy`.
///
/// Identical to [`read_package`] except the `policy` is applied to a GPKG
/// Manifest: `RequestSignature` makes an unsigned Manifest fatal and
/// `IgnoreSignature` skips verification. The policy is inert for the greenfield
/// and xpak formats, which carry no inline Manifest signature.
pub fn read_package_with_policy(
    bytes: &[u8],
    signature: Option<&signature::SignatureConfig>,
    policy: SignaturePolicy,
) -> Result<Package, ContainerError> {
    let span = tracing::info_span!("binpkg.read_package");
    let _enter = span.enter();

    match detect(bytes)? {
        Format::Gpkg => {
            let pkg = gpkg::read_with_policy(bytes, signature, policy)?;
            Ok(Package {
                format: Format::Gpkg,
                metadata: pkg.metadata().clone(),
                image: pkg.image().to_vec(),
            })
        }
        Format::Greenfield => {
            let reader = greenfield::Reader::open(bytes)?;
            reader.verify_manifest()?;
            if let Some(config) = signature {
                reader.verify_signature(config)?;
            }
            Ok(Package {
                format: Format::Greenfield,
                metadata: reader.metadata()?,
                image: reader.image()?,
            })
        }
        Format::Xpak => {
            // A legacy xpak/tbz2 carries no Manifest signature, so it cannot
            // satisfy `binpkg-request-signature`. Reject it rather than install it
            // unsigned, mirroring Portage's `gpkg_only = True` gate.
            if policy == SignaturePolicy::RequestSignature {
                return Err(ContainerError::Signature(
                    "binpkg-request-signature: legacy xpak/tbz2 carries no signature".into(),
                ));
            }
            let pkg = xpak::read(bytes)?;
            Ok(Package {
                format: Format::Xpak,
                metadata: pkg.metadata().clone(),
                image: pkg.image().to_vec(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::greenfield::{WriteOptions, write_bytes};
    use crate::metadata::{KEY_CHOST, KEY_USE};

    fn meta() -> MetadataMap {
        let mut m = MetadataMap::new();
        m.set_str(KEY_CHOST, "x86_64-pc-linux-gnu");
        m.set_str(KEY_USE, "ssl zlib");
        m
    }

    #[test]
    fn read_package_greenfield() {
        let bytes = write_bytes(&meta(), b"image", &WriteOptions::default()).unwrap();
        let pkg = read_package(&bytes, None).unwrap();
        assert_eq!(pkg.format, Format::Greenfield);
        assert_eq!(pkg.metadata, meta());
        assert_eq!(pkg.image, b"image");
    }

    #[test]
    fn read_package_xpak() {
        let file = xpak::build_tbz2(b"img", &meta());
        let pkg = read_package(&file, None).unwrap();
        assert_eq!(pkg.format, Format::Xpak);
        assert_eq!(pkg.metadata, meta());
    }

    #[test]
    fn request_signature_rejects_xpak() {
        // An xpak/tbz2 carries no Manifest signature, so under
        // `binpkg-request-signature` it must be rejected rather than installed
        // unsigned.
        let file = xpak::build_tbz2(b"img", &meta());
        let res = read_package_with_policy(&file, None, SignaturePolicy::RequestSignature);
        assert!(matches!(res, Err(ContainerError::Signature(_))));
        // The default policy still reads the xpak.
        assert!(read_package(&file, None).is_ok());
    }
}
