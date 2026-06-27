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
use crate::signature::{SignatureConfig, SignaturePolicy};

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

/// A parsed `Manifest` line: a member name, its recorded size, and its digests.
#[derive(Debug, Clone)]
struct ManifestEntry {
    size: Option<u64>,
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
    read_with_policy(bytes, signature, SignaturePolicy::default())
}

/// Import a GPKG file under an explicit signature `policy`.
///
/// When the `Manifest` carries an inline cleartext PGP signature, it is verified
/// with the configured `signature` and the `DATA` checksums are parsed only from
/// the verified body. `RequestSignature` rejects an unsigned Manifest;
/// `IgnoreSignature` skips verification.
pub fn read_with_policy(
    bytes: &[u8],
    signature: Option<&SignatureConfig>,
    policy: SignaturePolicy,
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

    // Resolve the Manifest, verifying an inline cleartext signature and parsing
    // DATA only from the verified body.
    let manifest = match lookup.get("Manifest") {
        Some(m) => {
            let signed = is_inline_signed(&m.bytes);
            if policy == SignaturePolicy::RequestSignature && !signed {
                return Err(ContainerError::Signature(
                    "binpkg-request-signature: Manifest is not signed".into(),
                ));
            }
            let body = if signed && policy != SignaturePolicy::IgnoreSignature {
                match signature {
                    Some(config) => config.verify_inline(&m.bytes)?,
                    // No key to verify with: parse the cleartext body unverified.
                    None => extract_cleartext_body(&m.bytes),
                }
            } else if signed {
                extract_cleartext_body(&m.bytes)
            } else {
                m.bytes.clone()
            };
            parse_manifest(&body)?
        }
        None => {
            return Err(ContainerError::MalformedGpkg(
                "missing Manifest member".into(),
            ));
        }
    };

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

    let mut verified: std::collections::HashSet<String> = std::collections::HashSet::new();

    let metadata_member = find_inner(&members, "metadata.tar")?;
    verify_member(&manifest, &metadata_member.rel, &metadata_member.bytes)?;
    verified.insert(metadata_member.rel.clone());
    let metadata = decode_metadata_tar(metadata_member)?;

    let image_member = find_inner(&members, "image.tar")?;
    verify_member(&manifest, &image_member.rel, &image_member.bytes)?;
    verified.insert(image_member.rel.clone());
    let image_comp = inner_compression(&image_member.rel)?;
    let image = image_comp.decompress(&image_member.bytes)?;

    // Verify the gpkg-1 marker against its Manifest record when the Manifest
    // lists it, mirroring Portage verifying the version file like any other
    // member (`lib/portage/gpkg.py:1722-1808`). The marker bytes are empty, so
    // the recorded size is 0 and the digests are over empty content. A Manifest
    // that omits the marker still reads, keeping older Moraine output importable.
    if manifest.contains_key(MARKER_PREFIX) {
        let marker = lookup
            .get(MARKER_PREFIX)
            .expect("marker member presence checked above");
        verify_member(&manifest, MARKER_PREFIX, &marker.bytes)?;
        verified.insert(MARKER_PREFIX.to_string());
    }

    // Cross-check completeness in both directions: every container member except
    // the Manifest and any detached signature must have been verified, and every
    // Manifest record must map to a verified member, matching Portage's
    // `_verify_binpkg` (`lib/portage/gpkg.py:1811`). The marker is verified above
    // when the Manifest lists it; a Manifest that omits the marker still reads.
    for member in &members {
        let rel = member.rel.as_str();
        // The marker is skipped here so older Moraine output, whose Manifest
        // carries no marker record, still passes the forward check.
        if rel == MARKER_PREFIX || rel == "Manifest" || rel.ends_with(".sig") {
            continue;
        }
        if !verified.contains(rel) {
            return Err(ContainerError::MalformedGpkg(format!(
                "container member `{rel}` is not listed in the Manifest"
            )));
        }
    }
    for name in manifest.keys() {
        if !verified.contains(name) {
            return Err(ContainerError::MalformedGpkg(format!(
                "Manifest record `{name}` has no corresponding container member"
            )));
        }
    }

    tracing::info!(entries = metadata.len(), "gpkg imported");
    Ok(GpkgPackage {
        metadata,
        image,
        image_compression: image_comp,
    })
}

/// Write a GPKG container from `metadata` and a root-relative `image_tar`,
/// compressing both inner tars with `comp`.
///
/// The outer tar shares a single `<basename>` prefix and carries `gpkg-1`, the
/// `metadata.tar.<comp>` (members under `metadata/`), the `image.tar.<comp>`
/// (members re-prefixed under `image/`), and a `Manifest` of per-member BLAKE2B
/// and SHA512 digests over the stored (compressed) bytes. Portage installs the
/// result unchanged.
pub fn write(
    metadata: &MetadataMap,
    image_tar: &[u8],
    comp: Compression,
) -> Result<Vec<u8>, ContainerError> {
    let basename = metadata
        .get_str("PF")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "gpkg".to_string());

    let meta_inner = build_metadata_tar(metadata)?;
    let image_inner = reprefix_image_tar(image_tar)?;
    let meta_stored = comp.compress(&meta_inner)?;
    let image_stored = comp.compress(&image_inner)?;
    let meta_name = format!("metadata.tar.{}", comp.suffix());
    let image_name = format!("image.tar.{}", comp.suffix());

    // Record the gpkg-1 marker first, over its empty bytes, then the metadata
    // and image members. Portage records the version file before the inner tars
    // (`lib/portage/gpkg.py:1024`) and requires a record for every member,
    // marker included, so omitting it fails with `gpkg-1 checksum not found`.
    let marker = b"";
    let mut manifest = format!(
        "DATA {MARKER_PREFIX} {} BLAKE2B {} SHA512 {}\n",
        marker.len(),
        blake2b(marker),
        sha512(marker),
    );
    for (name, bytes) in [(&meta_name, &meta_stored), (&image_name, &image_stored)] {
        manifest.push_str(&format!(
            "DATA {name} {} BLAKE2B {} SHA512 {}\n",
            bytes.len(),
            blake2b(bytes),
            sha512(bytes),
        ));
    }

    let mut builder = tar::Builder::new(Vec::new());
    append_outer(&mut builder, &format!("{basename}/gpkg-1"), b"")?;
    append_outer(
        &mut builder,
        &format!("{basename}/{meta_name}"),
        &meta_stored,
    )?;
    append_outer(
        &mut builder,
        &format!("{basename}/{image_name}"),
        &image_stored,
    )?;
    append_outer(
        &mut builder,
        &format!("{basename}/Manifest"),
        manifest.as_bytes(),
    )?;
    builder
        .into_inner()
        .map_err(|e| ContainerError::MalformedGpkg(format!("gpkg outer tar: {e}")))
}

/// Append one outer-tar member.
fn append_outer(
    builder: &mut tar::Builder<Vec<u8>>,
    name: &str,
    bytes: &[u8],
) -> Result<(), ContainerError> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, name, bytes)
        .map_err(|e| ContainerError::MalformedGpkg(format!("gpkg member `{name}`: {e}")))
}

/// Build the metadata inner tar, storing each key under `metadata/<key>`.
fn build_metadata_tar(metadata: &MetadataMap) -> Result<Vec<u8>, ContainerError> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, value) in metadata.iter() {
        let mut header = tar::Header::new_gnu();
        header.set_size(value.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("metadata/{name}"), value.as_slice())
            .map_err(|e| ContainerError::MalformedGpkg(format!("metadata member: {e}")))?;
    }
    builder
        .into_inner()
        .map_err(|e| ContainerError::MalformedGpkg(format!("metadata tar: {e}")))
}

/// Re-tar a root-relative image tar with every member under the `image/` arcname.
fn reprefix_image_tar(image_tar: &[u8]) -> Result<Vec<u8>, ContainerError> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut archive = tar::Archive::new(image_tar);
    let entries = archive
        .entries()
        .map_err(|e| ContainerError::MalformedGpkg(format!("image tar: {e}")))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|e| ContainerError::MalformedGpkg(format!("image entry: {e}")))?;
        let mut header = entry.header().clone();
        let path = entry
            .path()
            .map_err(|e| ContainerError::MalformedGpkg(format!("image path: {e}")))?;
        let rel = path.to_string_lossy();
        let rel = rel.trim_start_matches("./").trim_start_matches('/');
        if rel.is_empty() || rel == "." {
            continue;
        }
        let name = format!("image/{rel}");
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| ContainerError::MalformedGpkg(format!("image read: {e}")))?;
        header.set_size(buf.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, &name, buf.as_slice())
            .map_err(|e| ContainerError::MalformedGpkg(format!("image member `{name}`: {e}")))?;
    }
    builder
        .into_inner()
        .map_err(|e| ContainerError::MalformedGpkg(format!("image tar build: {e}")))
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
        return Err(ContainerError::MalformedGpkg(format!(
            "member `{rel}` has no Manifest record"
        )));
    };
    if let Some(expected) = entry.size
        && expected != bytes.len() as u64
    {
        return Err(ContainerError::IntegrityMismatch {
            section: format!("{rel}.size"),
            expected: expected.to_string(),
            actual: bytes.len().to_string(),
        });
    }
    let mut verified_hash_count = 0u32;
    if let Some(expected) = &entry.blake2b {
        let actual = blake2b(bytes);
        if &actual != expected {
            return Err(ContainerError::IntegrityMismatch {
                section: format!("{rel}.blake2b"),
                expected: expected.clone(),
                actual,
            });
        }
        verified_hash_count += 1;
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
        verified_hash_count += 1;
    }
    // Require at least one supported checksum to have been matched, so a Manifest
    // record that lists only a SIZE and no digest is rejected rather than trusted
    // on the size alone, mirroring Portage's `verified_hash_count < 1` rejection.
    if verified_hash_count < 1 {
        return Err(ContainerError::MalformedGpkg(format!(
            "member `{rel}` has no supported checksum in the Manifest"
        )));
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
///
/// Validates the container structure as Portage's `_verify_binpkg` does: no
/// member name begins with `/`, every member has exactly one path-separator
/// depth, all members share a single non-empty common prefix, and no member name
/// repeats (a duplicate-name attack).
fn read_outer(bytes: &[u8]) -> Result<Vec<Member>, ContainerError> {
    let mut archive = tar::Archive::new(bytes);
    let mut out = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut prefix: Option<String> = None;
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
        let full = full.strip_suffix('/').unwrap_or(&full).to_string();
        if full.is_empty() {
            continue;
        }
        if full.starts_with('/') || full.matches('/').count() != 1 {
            return Err(ContainerError::MalformedGpkg(format!(
                "gpkg structure mismatch: `{full}`"
            )));
        }
        if seen.contains(&full) {
            return Err(ContainerError::MalformedGpkg(format!(
                "duplicate member `{full}`"
            )));
        }
        let (dir, rest) = full.split_once('/').expect("one separator checked above");
        match &prefix {
            Some(p) if p != dir => {
                return Err(ContainerError::MalformedGpkg(
                    "gpkg members do not share a single common prefix".into(),
                ));
            }
            Some(_) => {}
            None => prefix = Some(dir.to_string()),
        }
        seen.push(full.clone());
        let rel = rest.to_string();
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

/// Whether a Manifest blob is an inline cleartext-signed PGP document.
fn is_inline_signed(bytes: &[u8]) -> bool {
    let needle = b"-----BEGIN PGP SIGNATURE-----";
    bytes.windows(needle.len()).any(|w| w == needle)
}

/// Extract the cleartext body of an inline cleartext-signed Manifest: the lines
/// between the `BEGIN PGP SIGNED MESSAGE` header block and the signature, with
/// PGP dash-escaping (`- `) removed. A non-signed blob is returned unchanged.
fn extract_cleartext_body(bytes: &[u8]) -> Vec<u8> {
    let text = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return bytes.to_vec(),
    };
    if !text.contains("-----BEGIN PGP SIGNED MESSAGE-----") {
        return bytes.to_vec();
    }
    let mut out = String::new();
    let mut in_headers = false;
    let mut in_body = false;
    for line in text.lines() {
        if line.starts_with("-----BEGIN PGP SIGNED MESSAGE-----") {
            in_headers = true;
            continue;
        }
        if in_headers && !in_body {
            // Armor headers (Hash: ...) end at the first blank line.
            if line.is_empty() {
                in_body = true;
            }
            continue;
        }
        if line.starts_with("-----BEGIN PGP SIGNATURE-----") {
            break;
        }
        if in_body {
            // Un-dash-escape a line that begins with "- ".
            let line = line.strip_prefix("- ").unwrap_or(line);
            out.push_str(line);
            out.push('\n');
        }
    }
    out.into_bytes()
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
        let size = tokens.next().and_then(|s| s.parse::<u64>().ok());
        let mut entry = ManifestEntry {
            size,
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

        // Portage layout: the Manifest lists a record for the gpkg-1 marker
        // over its empty bytes ahead of the inner-tar members.
        let marker: &[u8] = b"";
        let manifest = format!(
            "DATA {MARKER_PREFIX} 0 BLAKE2B {} SHA512 {}\nDATA {meta_name} {} BLAKE2B {} SHA512 {}\nDATA {image_name} {} BLAKE2B {} SHA512 {}\n",
            blake2b(marker),
            sha512(marker),
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

    fn root_relative_image_tar() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let content = b"hello from image";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "usr/bin/foo", content.as_slice())
            .unwrap();
        builder.into_inner().unwrap()
    }

    #[test]
    fn writer_round_trips_through_reader() {
        for comp in [Compression::Bzip2, Compression::Gzip, Compression::Zstd] {
            let meta = sample_metadata();
            let bytes = write(&meta, &root_relative_image_tar(), comp).unwrap();
            assert!(is_gpkg(&bytes), "produced container is a gpkg ({comp:?})");
            let pkg = read(&bytes, None).unwrap();
            assert_eq!(pkg.metadata(), &meta);
            // Image members are stored under the `image/` arcname.
            let mut archive = tar::Archive::new(pkg.image());
            let names: Vec<String> = archive
                .entries()
                .unwrap()
                .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
                .collect();
            assert!(
                names.iter().any(|n| n == "image/usr/bin/foo"),
                "image members carry the image/ prefix: {names:?}"
            );
        }
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
    fn portage_layout_manifest_with_marker_imports() {
        // A Portage gpkg lists a `DATA gpkg-1 0 ...` record for the version
        // marker. The reader must verify that record and import the package,
        // covering the `--getbinpkg` interop direction.
        let meta = sample_metadata();
        let file = build_gpkg(&meta, Compression::Zstd, false);
        let pkg = read(&file, None).unwrap();
        assert_eq!(pkg.metadata(), &meta);
    }

    #[test]
    fn manifest_without_marker_still_reads() {
        // Older Moraine output omits the marker record (build_gpkg_mutated's
        // Manifest lists only the inner tars). It must still import.
        let meta = sample_metadata();
        let file = build_gpkg_mutated(&meta, Compression::Gzip, |_| {});
        let pkg = read(&file, None).unwrap();
        assert_eq!(pkg.metadata(), &meta);
    }

    #[test]
    fn writer_manifest_lists_gpkg_marker() {
        // The produced-package interop direction: Portage requires a Manifest
        // record for the gpkg-1 marker, so write emits a `DATA gpkg-1 0` line
        // over the empty marker bytes.
        let meta = sample_metadata();
        let bytes = write(&meta, &root_relative_image_tar(), Compression::Zstd).unwrap();
        let members = read_outer(&bytes).unwrap();
        let manifest = members
            .iter()
            .find(|m| m.rel == "Manifest")
            .expect("Manifest member present");
        let text = std::str::from_utf8(&manifest.bytes).unwrap();
        let marker_line = format!(
            "DATA {MARKER_PREFIX} 0 BLAKE2B {} SHA512 {}",
            blake2b(b""),
            sha512(b""),
        );
        assert!(
            text.lines().any(|l| l == marker_line),
            "Manifest lists the gpkg-1 marker line: {text}"
        );
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

    /// Build a gpkg whose Manifest is wrapped in an inline cleartext PGP
    /// signature, so the real DATA lines sit in the signed body.
    fn build_gpkg_signed(meta: &MetadataMap, comp: Compression) -> Vec<u8> {
        let basename = "cat_pkg-1-2";
        let meta_tar = comp.compress(&build_inner_metadata_tar(meta)).unwrap();
        let image_tar = comp.compress(&build_inner_image_tar()).unwrap();
        let meta_name = format!("metadata.tar.{}", comp.suffix());
        let image_name = format!("image.tar.{}", comp.suffix());
        let data = format!(
            "DATA {meta_name} {} BLAKE2B {} SHA512 {}\nDATA {image_name} {} BLAKE2B {} SHA512 {}\n",
            meta_tar.len(),
            blake2b(&meta_tar),
            sha512(&meta_tar),
            image_tar.len(),
            blake2b(&image_tar),
            sha512(&image_tar),
        );
        let manifest = format!(
            "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA512\n\n{data}-----BEGIN PGP SIGNATURE-----\n\nABCDEF==\n-----END PGP SIGNATURE-----\n"
        );

        let mut builder = tar::Builder::new(Vec::new());
        append_member(&mut builder, &format!("{basename}/gpkg-1"), b"");
        append_member(&mut builder, &format!("{basename}/{meta_name}"), &meta_tar);
        append_member(
            &mut builder,
            &format!("{basename}/{image_name}"),
            &image_tar,
        );
        append_member(
            &mut builder,
            &format!("{basename}/Manifest"),
            manifest.as_bytes(),
        );
        builder.into_inner().unwrap()
    }

    /// Build a gpkg, then apply `mutate` to its outer-tar members before
    /// re-packing, so a test can strip the Manifest, add an extra member, repeat
    /// a name, or change a size.
    fn build_gpkg_mutated(
        meta: &MetadataMap,
        comp: Compression,
        mutate: impl Fn(&mut Vec<(String, Vec<u8>)>),
    ) -> Vec<u8> {
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
        let mut members: Vec<(String, Vec<u8>)> = vec![
            (format!("{basename}/gpkg-1"), b"".to_vec()),
            (format!("{basename}/{meta_name}"), meta_tar),
            (format!("{basename}/{image_name}"), image_tar),
            (format!("{basename}/Manifest"), manifest.into_bytes()),
        ];
        mutate(&mut members);
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes) in &members {
            append_member(&mut builder, name, bytes);
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn stripped_manifest_rejected() {
        let meta = sample_metadata();
        let file = build_gpkg_mutated(&meta, Compression::Gzip, |members| {
            members.retain(|(name, _)| !name.ends_with("/Manifest"));
        });
        assert!(read(&file, None).is_err());
    }

    #[test]
    fn extra_unlisted_member_rejected() {
        let meta = sample_metadata();
        let file = build_gpkg_mutated(&meta, Compression::Gzip, |members| {
            members.push(("cat_pkg-1-2/extra".to_string(), b"surprise".to_vec()));
        });
        assert!(read(&file, None).is_err());
    }

    #[test]
    fn duplicate_member_name_rejected() {
        let meta = sample_metadata();
        let file = build_gpkg_mutated(&meta, Compression::Gzip, |members| {
            members.push(("cat_pkg-1-2/gpkg-1".to_string(), b"".to_vec()));
        });
        assert!(read(&file, None).is_err());
    }

    #[test]
    fn member_size_mismatch_rejected() {
        let meta = sample_metadata();
        let file = build_gpkg_mutated(&meta, Compression::Gzip, |members| {
            for (name, bytes) in members.iter_mut() {
                if name.contains("/image.tar.") {
                    bytes.extend_from_slice(b"trailing bytes that break the size");
                }
            }
        });
        assert!(read(&file, None).is_err());
    }

    #[test]
    fn nested_member_path_rejected() {
        let meta = sample_metadata();
        let file = build_gpkg_mutated(&meta, Compression::Gzip, |members| {
            members.push(("cat_pkg-1-2/sub/deep".to_string(), b"x".to_vec()));
        });
        assert!(read(&file, None).is_err());
    }

    #[test]
    fn cleartext_body_extracts_data_lines() {
        let signed = b"-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA512\n\nDATA x 1 BLAKE2B aa\n-----BEGIN PGP SIGNATURE-----\nsig\n-----END PGP SIGNATURE-----\n";
        let body = extract_cleartext_body(signed);
        assert_eq!(body, b"DATA x 1 BLAKE2B aa\n");
    }

    #[test]
    fn ignore_signature_reads_signed_manifest() {
        let meta = sample_metadata();
        let file = build_gpkg_signed(&meta, Compression::Gzip);
        // Without verification the signed Manifest still yields the real DATA
        // lines (no PGP-framing junk), so the package reads and verifies.
        let pkg = read_with_policy(&file, None, SignaturePolicy::IgnoreSignature).unwrap();
        assert_eq!(pkg.metadata(), &meta);
    }

    #[test]
    fn signed_manifest_rejected_on_bad_signature() {
        let meta = sample_metadata();
        let file = build_gpkg_signed(&meta, Compression::Gzip);
        // A gpg that always fails rejects the package under verify-if-present.
        let config = SignatureConfig {
            gpg_command: "false".to_string(),
            keyring: None,
            extra_args: Vec::new(),
        };
        assert!(read_with_policy(&file, Some(&config), SignaturePolicy::VerifyIfPresent).is_err());
    }

    #[test]
    fn request_signature_rejects_unsigned_manifest() {
        let meta = sample_metadata();
        let file = build_gpkg(&meta, Compression::Gzip, false);
        assert!(read_with_policy(&file, None, SignaturePolicy::RequestSignature).is_err());
    }

    #[test]
    fn size_only_manifest_record_rejected() {
        // A Manifest record that lists a SIZE but no BLAKE2B/SHA512 digest must
        // be rejected: the member cannot be verified on the size alone.
        let meta = sample_metadata();
        let comp = Compression::Gzip;
        let basename = "cat_pkg-1-2";
        let meta_tar = comp.compress(&build_inner_metadata_tar(&meta)).unwrap();
        let image_tar = comp.compress(&build_inner_image_tar()).unwrap();
        let meta_name = format!("metadata.tar.{}", comp.suffix());
        let image_name = format!("image.tar.{}", comp.suffix());
        let marker: &[u8] = b"";
        let manifest = format!(
            "DATA {MARKER_PREFIX} 0 BLAKE2B {} SHA512 {}\n\
             DATA {meta_name} {} BLAKE2B {} SHA512 {}\n\
             DATA {image_name} {}\n",
            blake2b(marker),
            sha512(marker),
            meta_tar.len(),
            blake2b(&meta_tar),
            sha512(&meta_tar),
            image_tar.len(),
        );
        let mut builder = tar::Builder::new(Vec::new());
        append_member(&mut builder, &format!("{basename}/gpkg-1"), b"");
        append_member(&mut builder, &format!("{basename}/{meta_name}"), &meta_tar);
        append_member(
            &mut builder,
            &format!("{basename}/{image_name}"),
            &image_tar,
        );
        append_member(
            &mut builder,
            &format!("{basename}/Manifest"),
            manifest.as_bytes(),
        );
        let file = builder.into_inner().unwrap();
        assert!(read(&file, None).is_err());
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
