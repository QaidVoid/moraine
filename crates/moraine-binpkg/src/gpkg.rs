//! Reader for modern GPKG containers.
//!
//! A GPKG file is a plain (uncompressed) outer tar whose members share a
//! `<basename>` prefix:
//!
//! - `<basename>/gpkg-1`: a version marker.
//! - `<basename>/metadata.tar.<comp>`: the metadata key files, compressed.
//! - `<basename>/image.tar.<comp>`: the installed image, compressed.
//! - `<basename>/Manifest`: per-member BLAKE2B and SHA512 checksums.
//! - optional `<basename>/*.sig`: detached GnuPG signatures.
//!
//! The importer recognizes the marker, verifies the listed checksums before
//! trusting member content, decodes the metadata inner tar into the canonical
//! metadata map, and exposes the image inner tar bytes. It is read-only.

use std::collections::HashMap;
use std::io::Read as _;

use moraine_common::hash::{blake2b, sha512};

use crate::compress::Compression;
use crate::error::ContainerError;
use crate::metadata::MetadataMap;
use crate::signature::SignatureConfig;

const MARKER_PREFIX: &str = "gpkg-1";

/// An imported GPKG container.
pub struct GpkgPackage {
    metadata: MetadataMap,
    image: Vec<u8>,
    image_compression: Compression,
}

impl GpkgPackage {
    /// The recovered canonical metadata map.
    pub fn metadata(&self) -> &MetadataMap {
        &self.metadata
    }

    /// The decompressed image inner tar bytes.
    pub fn image(&self) -> &[u8] {
        &self.image
    }

    /// The codec the source used for the image inner tar.
    pub fn image_compression(&self) -> Compression {
        self.image_compression
    }
}

/// A parsed `Manifest` line: a member name and its two digests.
#[derive(Debug, Clone)]
struct ManifestEntry {
    blake2b: Option<String>,
    sha512: Option<String>,
}

/// One outer-tar member, its basename-relative name and bytes.
struct Member {
    /// The member name with the `<basename>/` prefix stripped.
    rel: String,
    bytes: Vec<u8>,
}

/// Whether `bytes` is a GPKG container.
///
/// Detection reads the outer tar and looks for a `*/gpkg-1` marker member.
pub fn is_gpkg(bytes: &[u8]) -> bool {
    let Ok(members) = read_outer(bytes) else {
        return false;
    };
    members.iter().any(|m| m.rel == MARKER_PREFIX)
}

/// Import a GPKG file.
///
/// When `signature` is `Some`, a present detached `.sig` member is verified
/// against the configured key, and a verification failure rejects the package.
pub fn read(
    bytes: &[u8],
    signature: Option<&SignatureConfig>,
) -> Result<GpkgPackage, ContainerError> {
    let span = tracing::info_span!("binpkg.gpkg.read", size = bytes.len());
    let _enter = span.enter();

    let members = read_outer(bytes)?;
    let lookup: HashMap<&str, &Member> = members.iter().map(|m| (m.rel.as_str(), m)).collect();

    if !lookup.contains_key(MARKER_PREFIX) {
        return Err(ContainerError::MalformedGpkg(
            "missing gpkg-1 marker".into(),
        ));
    }

    let manifest = lookup
        .get("Manifest")
        .map(|m| parse_manifest(&m.bytes))
        .transpose()?
        .unwrap_or_default();

    // Verify any signed members and detached signatures.
    if let Some(config) = signature {
        for member in &members {
            if let Some(stripped) = member.rel.strip_suffix(".sig") {
                let signed = lookup.get(stripped).ok_or_else(|| {
                    ContainerError::MalformedGpkg(format!(
                        "detached signature for absent member `{stripped}`"
                    ))
                })?;
                config.verify_detached(&signed.bytes, &member.bytes)?;
            }
        }
    }

    let metadata_member = find_inner(&members, "metadata.tar")?;
    verify_member(&manifest, &metadata_member.rel, &metadata_member.bytes)?;
    let metadata = decode_metadata_tar(metadata_member)?;

    let image_member = find_inner(&members, "image.tar")?;
    verify_member(&manifest, &image_member.rel, &image_member.bytes)?;
    let image_comp = inner_compression(&image_member.rel)?;
    let image = image_comp.decompress(&image_member.bytes)?;

    tracing::info!(entries = metadata.len(), "gpkg imported");
    Ok(GpkgPackage {
        metadata,
        image,
        image_compression: image_comp,
    })
}

fn find_inner<'m>(members: &'m [Member], stem: &str) -> Result<&'m Member, ContainerError> {
    members
        .iter()
        .find(|m| m.rel == stem || m.rel.starts_with(&format!("{stem}.")))
        .ok_or_else(|| ContainerError::MissingMember(stem.to_string()))
}

fn inner_compression(rel: &str) -> Result<Compression, ContainerError> {
    match rel.rsplit_once("tar.") {
        Some((_, suffix)) => Compression::from_suffix(suffix),
        None => Ok(Compression::None),
    }
}

fn verify_member(
    manifest: &HashMap<String, ManifestEntry>,
    rel: &str,
    bytes: &[u8],
) -> Result<(), ContainerError> {
    let Some(entry) = manifest.get(rel) else {
        return Ok(());
    };
    if let Some(expected) = &entry.blake2b {
        let actual = blake2b(bytes);
        if &actual != expected {
            return Err(ContainerError::IntegrityMismatch {
                section: format!("{rel}.blake2b"),
                expected: expected.clone(),
                actual,
            });
        }
    }
    if let Some(expected) = &entry.sha512 {
        let actual = sha512(bytes);
        if &actual != expected {
            return Err(ContainerError::IntegrityMismatch {
                section: format!("{rel}.sha512"),
                expected: expected.clone(),
                actual,
            });
        }
    }
    Ok(())
}

fn decode_metadata_tar(member: &Member) -> Result<MetadataMap, ContainerError> {
    let comp = inner_compression(&member.rel)?;
    let plain = comp.decompress(&member.bytes)?;
    let mut metadata = MetadataMap::new();
    let mut archive = tar::Archive::new(plain.as_slice());
    let entries = archive
        .entries()
        .map_err(|e| ContainerError::MalformedGpkg(format!("metadata tar: {e}")))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|e| ContainerError::MalformedGpkg(format!("metadata entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| ContainerError::MalformedGpkg(format!("metadata path: {e}")))?;
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| ContainerError::MalformedGpkg(format!("metadata read: {e}")))?;
        metadata.insert(name, buf);
    }
    Ok(metadata)
}

/// Read the outer tar into its members, stripping the shared basename prefix.
fn read_outer(bytes: &[u8]) -> Result<Vec<Member>, ContainerError> {
    let mut archive = tar::Archive::new(bytes);
    let mut out = Vec::new();
    let entries = archive
        .entries()
        .map_err(|e| ContainerError::MalformedGpkg(format!("outer tar: {e}")))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|e| ContainerError::MalformedGpkg(format!("outer entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| ContainerError::MalformedGpkg(format!("outer path: {e}")))?;
        let full = path.to_string_lossy().into_owned();
        let rel = match full.split_once('/') {
            Some((_, rest)) => rest.to_string(),
            None => full,
        };
        if rel.is_empty() {
            continue;
        }
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| ContainerError::MalformedGpkg(format!("outer read: {e}")))?;
        out.push(Member { rel, bytes: buf });
    }
    Ok(out)
}

/// Parse a `Manifest` blob into per-member digest entries.
///
/// Recognizes lines of the form `TYPE name size [ALG hex]...`, picking the
/// BLAKE2B and SHA512 digests. Unknown algorithms and line types are ignored.
fn parse_manifest(bytes: &[u8]) -> Result<HashMap<String, ManifestEntry>, ContainerError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ContainerError::MalformedGpkg("non-UTF-8 Manifest".into()))?;
    let mut out: HashMap<String, ManifestEntry> = HashMap::new();
    for line in text.lines() {
        let mut tokens = line.split_whitespace();
        let Some(_kind) = tokens.next() else { continue };
        let Some(name) = tokens.next() else { continue };
        let _size = tokens.next();
        let mut entry = ManifestEntry {
            blake2b: None,
            sha512: None,
        };
        while let (Some(alg), Some(hex)) = (tokens.next(), tokens.next()) {
            match alg.to_ascii_uppercase().as_str() {
                "BLAKE2B" => entry.blake2b = Some(hex.to_ascii_lowercase()),
                "SHA512" => entry.sha512 = Some(hex.to_ascii_lowercase()),
                _ => {}
            }
        }
        // Manifest names may carry the basename prefix; index by basename too.
        let rel = match name.split_once('/') {
            Some((_, rest)) => rest.to_string(),
            None => name.to_string(),
        };
        out.insert(rel, entry);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{KEY_CHOST, KEY_USE};

    fn build_inner_metadata_tar(meta: &MetadataMap) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, value) in meta.iter() {
            let mut header = tar::Header::new_gnu();
            header.set_size(value.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, value.as_slice())
                .unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn build_inner_image_tar() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let content = b"hello from image";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "usr/bin/foo", content.as_slice())
            .unwrap();
        builder.into_inner().unwrap()
    }

    fn append_member(builder: &mut tar::Builder<Vec<u8>>, name: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, bytes).unwrap();
    }

    fn build_gpkg(meta: &MetadataMap, comp: Compression, sign: bool) -> Vec<u8> {
        let basename = "cat_pkg-1-2";
        let meta_tar = comp.compress(&build_inner_metadata_tar(meta)).unwrap();
        let image_tar = comp.compress(&build_inner_image_tar()).unwrap();
        let meta_name = format!("metadata.tar.{}", comp.suffix());
        let image_name = format!("image.tar.{}", comp.suffix());

        let manifest = format!(
            "DATA {meta_name} {} BLAKE2B {} SHA512 {}\nDATA {image_name} {} BLAKE2B {} SHA512 {}\n",
            meta_tar.len(),
            blake2b(&meta_tar),
            sha512(&meta_tar),
            image_tar.len(),
            blake2b(&image_tar),
            sha512(&image_tar),
        );

        let mut builder = tar::Builder::new(Vec::new());
        append_member(&mut builder, &format!("{basename}/gpkg-1"), b"");
        append_member(&mut builder, &format!("{basename}/{meta_name}"), &meta_tar);
        append_member(
            &mut builder,
            &format!("{basename}/{image_name}"),
            &image_tar,
        );
        if sign {
            append_member(
                &mut builder,
                &format!("{basename}/{meta_name}.sig"),
                b"SIGNATURE-BYTES",
            );
        }
        append_member(
            &mut builder,
            &format!("{basename}/Manifest"),
            manifest.as_bytes(),
        );
        builder.into_inner().unwrap()
    }

    fn sample_metadata() -> MetadataMap {
        let mut m = MetadataMap::new();
        m.set_str(KEY_CHOST, "x86_64-pc-linux-gnu");
        m.set_str(KEY_USE, "ssl zlib");
        m.set_str("EAPI", "8");
        m
    }

    #[test]
    fn import_each_codec() {
        for comp in [Compression::Bzip2, Compression::Gzip, Compression::Zstd] {
            let meta = sample_metadata();
            let file = build_gpkg(&meta, comp, false);
            assert!(is_gpkg(&file), "detection failed for {comp:?}");
            let pkg = read(&file, None).unwrap();
            assert_eq!(pkg.metadata(), &meta, "metadata mismatch for {comp:?}");
            assert!(!pkg.image().is_empty());
        }
    }

    #[test]
    fn corrupt_member_rejected_by_manifest() {
        let meta = sample_metadata();
        let mut file = build_gpkg(&meta, Compression::Gzip, false);
        // Flip a byte somewhere in the body (past the first member header).
        let mid = file.len() / 2;
        file[mid] ^= 0xff;
        let res = read(&file, None);
        assert!(res.is_err());
    }

    #[test]
    fn signature_failure_rejects() {
        let meta = sample_metadata();
        let file = build_gpkg(&meta, Compression::Gzip, true);
        // A gpg that always fails: use `false` as the command.
        let config = SignatureConfig {
            gpg_command: "false".to_string(),
            keyring: None,
            extra_args: Vec::new(),
        };
        assert!(read(&file, Some(&config)).is_err());
    }
}
