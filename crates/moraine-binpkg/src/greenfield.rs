//! The greenfield binary package container.
//!
//! Layout, in order:
//!
//! ```text
//! MORABPK1                 8-byte magic
//! header_len               4-byte big-endian length of the header blob
//! header                   rmp-serde [`Header`]: version, compression, section
//!                          spans, and the integrity manifest
//! metadata                 rmp-serde [`MetadataMap`], optionally compressed
//! image                    the installed image tar, compressed
//! [signature trailer]      optional: signature bytes, 4-byte big-endian length,
//!                          then the 8-byte magic MORASIG1
//! ```
//!
//! Sections are separately addressable: the header records the byte span of the
//! metadata and image sections, so a reader pulls metadata or verifies the
//! manifest without decompressing the image. The signature is detached and
//! lives in an appended trailer, so an unsigned artifact is signed later by
//! appending the trailer without rewriting the metadata or image sections.

use serde::{Deserialize, Serialize};

use moraine_common::hash::{blake2b, sha512};

use crate::compress::Compression;
use crate::error::ContainerError;
use crate::metadata::MetadataMap;
use crate::signature::SignatureConfig;

/// The 8-byte magic at the start of every greenfield container.
pub const MAGIC: &[u8; 8] = b"MORABPK1";
/// The 8-byte magic at the end of a signature trailer.
pub const SIGNATURE_MAGIC: &[u8; 8] = b"MORASIG1";
/// The container format version this crate writes and reads.
pub const FORMAT_VERSION: u32 = 1;

/// Per-section integrity checksums recorded in the header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionDigest {
    /// The lowercase hex BLAKE2b-512 digest of the section bytes.
    pub blake2b: String,
    /// The lowercase hex SHA-512 digest of the section bytes.
    pub sha512: String,
}

impl SectionDigest {
    fn of(bytes: &[u8]) -> Self {
        Self {
            blake2b: blake2b(bytes),
            sha512: sha512(bytes),
        }
    }

    fn verify(&self, section: &str, bytes: &[u8]) -> Result<(), ContainerError> {
        let actual_b2 = blake2b(bytes);
        if actual_b2 != self.blake2b {
            return Err(ContainerError::IntegrityMismatch {
                section: format!("{section}.blake2b"),
                expected: self.blake2b.clone(),
                actual: actual_b2,
            });
        }
        let actual_sha = sha512(bytes);
        if actual_sha != self.sha512 {
            return Err(ContainerError::IntegrityMismatch {
                section: format!("{section}.sha512"),
                expected: self.sha512.clone(),
                actual: actual_sha,
            });
        }
        Ok(())
    }
}

/// The integrity manifest: a digest per addressable section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Digest of the (uncompressed) serialized metadata section as stored.
    pub metadata: SectionDigest,
    /// Digest of the image section as stored (compressed bytes).
    pub image: SectionDigest,
}

/// The container header, serialized after the magic and length prefix.
///
/// The header records only section lengths, not absolute offsets. Offsets are
/// derived at read time as `magic + 4 + header_len`, then the metadata section,
/// then the image section. Recording lengths instead of offsets keeps the
/// header self-consistent: an offset cannot reference the header's own size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    /// The container format version.
    pub version: u32,
    /// The codec applied to the image section.
    pub image_compression: Compression,
    /// Whether the serialized metadata section is compressed, and how.
    pub metadata_compression: Compression,
    /// The length in bytes of the stored metadata section.
    pub metadata_len: u64,
    /// The length in bytes of the stored image section.
    pub image_len: u64,
    /// The integrity manifest.
    pub manifest: Manifest,
}

// `Compression` is a plain enum; derive serde for it via a shim mapping.
impl Serialize for Compression {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let tag: u8 = match self {
            Compression::None => 0,
            Compression::Bzip2 => 1,
            Compression::Gzip => 2,
            Compression::Zstd => 3,
            // The greenfield format only stores the in-process codecs; the
            // read-only system-tool codecs never reach a greenfield write.
            Compression::Xz | Compression::Lz4 | Compression::Lzip | Compression::Lzop => 3,
        };
        ser.serialize_u8(tag)
    }
}

impl<'de> Deserialize<'de> for Compression {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let tag = u8::deserialize(de)?;
        match tag {
            0 => Ok(Compression::None),
            1 => Ok(Compression::Bzip2),
            2 => Ok(Compression::Gzip),
            3 => Ok(Compression::Zstd),
            other => Err(serde::de::Error::custom(format!(
                "unknown compression tag {other}"
            ))),
        }
    }
}

/// Options controlling how a greenfield container is written.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Codec applied to the image section.
    pub image_compression: Compression,
    /// Codec applied to the serialized metadata section.
    pub metadata_compression: Compression,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            image_compression: Compression::Zstd,
            metadata_compression: Compression::Zstd,
        }
    }
}

/// Serialize a greenfield container into a byte buffer.
///
/// `image` is the raw (uncompressed) installed-image tar bytes; this function
/// applies `options.image_compression`. The returned bytes carry a computed
/// manifest but no signature.
pub fn write_bytes(
    metadata: &MetadataMap,
    image: &[u8],
    options: &WriteOptions,
) -> Result<Vec<u8>, ContainerError> {
    let span = tracing::info_span!("binpkg.greenfield.write");
    let _enter = span.enter();

    let metadata_plain =
        rmp_serde::to_vec(metadata).map_err(|e| ContainerError::Encode(e.to_string()))?;
    let metadata_stored = options.metadata_compression.compress(&metadata_plain)?;
    let image_stored = options.image_compression.compress(image)?;

    let manifest = Manifest {
        metadata: SectionDigest::of(&metadata_stored),
        image: SectionDigest::of(&image_stored),
    };

    let header = Header {
        version: FORMAT_VERSION,
        image_compression: options.image_compression,
        metadata_compression: options.metadata_compression,
        metadata_len: metadata_stored.len() as u64,
        image_len: image_stored.len() as u64,
        manifest,
    };
    let header_bytes =
        rmp_serde::to_vec(&header).map_err(|e| ContainerError::Encode(e.to_string()))?;

    let body_start = MAGIC.len() + 4 + header_bytes.len();
    let mut out = Vec::with_capacity(body_start + metadata_stored.len() + image_stored.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(&metadata_stored);
    out.extend_from_slice(&image_stored);
    Ok(out)
}

/// Write a greenfield container atomically to `path`.
pub fn write_file(
    path: impl AsRef<std::path::Path>,
    metadata: &MetadataMap,
    image: &[u8],
    options: &WriteOptions,
) -> Result<(), ContainerError> {
    let bytes = write_bytes(metadata, image, options)?;
    moraine_common::fs::atomic_write(path, &bytes)?;
    Ok(())
}

/// Whether `bytes` begins with the greenfield magic.
pub fn is_greenfield(bytes: &[u8]) -> bool {
    bytes.len() >= MAGIC.len() && &bytes[..MAGIC.len()] == MAGIC
}

/// A parsed greenfield container backed by its source bytes.
///
/// Holds the decoded header and a borrow of the whole container so callers pull
/// the metadata or image section on demand.
pub struct Reader<'a> {
    bytes: &'a [u8],
    header: Header,
    /// Offset of the metadata section from the start of the file.
    metadata_offset: usize,
    /// Offset of the image section from the start of the file.
    image_offset: usize,
    signature: Option<&'a [u8]>,
}

impl<'a> Reader<'a> {
    /// Open a greenfield container over `bytes`, decoding and validating the
    /// header. Does not decompress any section.
    pub fn open(bytes: &'a [u8]) -> Result<Self, ContainerError> {
        let span = tracing::info_span!("binpkg.greenfield.open");
        let _enter = span.enter();

        if !is_greenfield(bytes) {
            return Err(ContainerError::UnknownFormat);
        }
        let mut cursor = MAGIC.len();
        let header_len = read_u32_be(bytes, cursor)
            .ok_or_else(|| ContainerError::MalformedGreenfield("truncated header length".into()))?
            as usize;
        cursor += 4;
        let header_end = cursor
            .checked_add(header_len)
            .filter(|&end| end <= bytes.len())
            .ok_or_else(|| ContainerError::MalformedGreenfield("truncated header".into()))?;
        let header: Header = rmp_serde::from_slice(&bytes[cursor..header_end])
            .map_err(|e| ContainerError::Decode(e.to_string()))?;

        if header.version > FORMAT_VERSION {
            return Err(ContainerError::MalformedGreenfield(format!(
                "unsupported container version {}",
                header.version
            )));
        }

        let metadata_offset = header_end;
        let image_offset = metadata_offset
            .checked_add(header.metadata_len as usize)
            .ok_or_else(|| {
                ContainerError::MalformedGreenfield("metadata length overflow".into())
            })?;
        let body_end = image_offset
            .checked_add(header.image_len as usize)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| ContainerError::MalformedGreenfield("sections exceed file".into()))?;

        let signature = parse_signature_trailer(bytes, body_end as u64);

        Ok(Self {
            bytes,
            header,
            metadata_offset,
            image_offset,
            signature,
        })
    }

    /// The decoded header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The detached signature bytes, if a trailer is present.
    pub fn signature(&self) -> Option<&[u8]> {
        self.signature
    }

    /// The byte offset of the metadata section from the start of the file.
    pub fn metadata_offset(&self) -> usize {
        self.metadata_offset
    }

    /// The byte offset of the image section from the start of the file.
    pub fn image_offset(&self) -> usize {
        self.image_offset
    }

    fn metadata_section(&self) -> &'a [u8] {
        &self.bytes[self.metadata_offset..self.metadata_offset + self.header.metadata_len as usize]
    }

    fn image_section(&self) -> &'a [u8] {
        &self.bytes[self.image_offset..self.image_offset + self.header.image_len as usize]
    }

    fn body_end(&self) -> usize {
        self.image_offset + self.header.image_len as usize
    }

    /// Read and decode the metadata section without touching the image.
    ///
    /// Verifies the metadata section checksum against the manifest first.
    pub fn metadata(&self) -> Result<MetadataMap, ContainerError> {
        let section = self.metadata_section();
        self.header.manifest.metadata.verify("metadata", section)?;
        let plain = self.header.metadata_compression.decompress(section)?;
        rmp_serde::from_slice(&plain).map_err(|e| ContainerError::Decode(e.to_string()))
    }

    /// Verify both section checksums against the manifest.
    pub fn verify_manifest(&self) -> Result<(), ContainerError> {
        self.header
            .manifest
            .metadata
            .verify("metadata", self.metadata_section())?;
        self.header
            .manifest
            .image
            .verify("image", self.image_section())?;
        Ok(())
    }

    /// The raw (compressed) image section bytes.
    pub fn image_raw(&self) -> &'a [u8] {
        self.image_section()
    }

    /// Decompress and return the image tar bytes.
    ///
    /// Verifies the image section checksum against the manifest first.
    pub fn image(&self) -> Result<Vec<u8>, ContainerError> {
        let section = self.image_section();
        self.header.manifest.image.verify("image", section)?;
        self.header.image_compression.decompress(section)
    }

    /// Verify the detached signature against `config`.
    ///
    /// The signed region is the container body up to and excluding the signature
    /// trailer. Returns an error when no signature trailer is present.
    pub fn verify_signature(&self, config: &SignatureConfig) -> Result<(), ContainerError> {
        let signature = self
            .signature
            .ok_or_else(|| ContainerError::Signature("no signature present".into()))?;
        config.verify_detached(&self.bytes[..self.body_end()], signature)
    }
}

fn parse_signature_trailer(bytes: &[u8], body_end: u64) -> Option<&[u8]> {
    let body_end = body_end as usize;
    if bytes.len() < body_end + 4 + SIGNATURE_MAGIC.len() {
        return None;
    }
    let magic_start = bytes.len() - SIGNATURE_MAGIC.len();
    if &bytes[magic_start..] != SIGNATURE_MAGIC {
        return None;
    }
    let len_start = magic_start - 4;
    let sig_len = read_u32_be(bytes, len_start)? as usize;
    let sig_start = len_start.checked_sub(sig_len)?;
    if sig_start < body_end {
        return None;
    }
    Some(&bytes[sig_start..len_start])
}

fn read_u32_be(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Append a detached signature trailer to an already-written container.
///
/// `container` is the bytes of an unsigned (or already-signed) greenfield
/// container; this returns the bytes with `signature` appended as a trailer. The
/// metadata and image sections are not rewritten. An existing trailer is
/// replaced.
pub fn attach_signature(container: &[u8], signature: &[u8]) -> Result<Vec<u8>, ContainerError> {
    let reader = Reader::open(container)?;
    let body_end = reader.body_end();
    let mut out = Vec::with_capacity(body_end + signature.len() + 12);
    out.extend_from_slice(&container[..body_end]);
    out.extend_from_slice(signature);
    out.extend_from_slice(&(signature.len() as u32).to_be_bytes());
    out.extend_from_slice(SIGNATURE_MAGIC);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{KEY_BUILD_ID, KEY_CHOST, KEY_USE};

    fn sample() -> (MetadataMap, Vec<u8>) {
        let mut m = MetadataMap::new();
        m.set_str(KEY_CHOST, "x86_64-pc-linux-gnu");
        m.set_str(KEY_USE, "ssl zlib");
        m.set_str(KEY_BUILD_ID, "3");
        let image = b"PRETEND-TAR-IMAGE-BYTES".repeat(50);
        (m, image)
    }

    #[test]
    fn write_then_read_round_trips() {
        let (meta, image) = sample();
        let bytes = write_bytes(&meta, &image, &WriteOptions::default()).unwrap();
        assert!(is_greenfield(&bytes));

        let reader = Reader::open(&bytes).unwrap();
        assert_eq!(reader.metadata().unwrap(), meta);
        assert_eq!(reader.image().unwrap(), image);
        reader.verify_manifest().unwrap();
        assert!(reader.signature().is_none());
    }

    #[test]
    fn metadata_read_does_not_need_image_section() {
        let (meta, image) = sample();
        let mut bytes = write_bytes(&meta, &image, &WriteOptions::default()).unwrap();
        let img_off = Reader::open(&bytes).unwrap().image_offset();
        // Corrupt the image section; metadata read must still succeed.
        bytes[img_off] ^= 0xff;
        let reader = Reader::open(&bytes).unwrap();
        assert_eq!(reader.metadata().unwrap(), meta);
        // But a full manifest verify catches the corruption.
        assert!(reader.verify_manifest().is_err());
    }

    #[test]
    fn corrupt_metadata_rejected() {
        let (meta, image) = sample();
        let mut bytes = write_bytes(&meta, &image, &WriteOptions::default()).unwrap();
        let off = Reader::open(&bytes).unwrap().metadata_offset();
        bytes[off] ^= 0xff;
        let reader = Reader::open(&bytes).unwrap();
        assert!(matches!(
            reader.metadata(),
            Err(ContainerError::IntegrityMismatch { .. })
        ));
    }

    #[test]
    fn attach_signature_keeps_sections() {
        let (meta, image) = sample();
        let bytes = write_bytes(&meta, &image, &WriteOptions::default()).unwrap();
        let signed = attach_signature(&bytes, b"FAKE-SIGNATURE").unwrap();
        let reader = Reader::open(&signed).unwrap();
        assert_eq!(reader.signature(), Some(&b"FAKE-SIGNATURE"[..]));
        // Sections still read correctly after attaching.
        assert_eq!(reader.metadata().unwrap(), meta);
        assert_eq!(reader.image().unwrap(), image);
    }
}
