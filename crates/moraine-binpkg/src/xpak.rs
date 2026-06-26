//! Reader for legacy xpak/tbz2 containers.
//!
//! A tbz2 file is a `tar.bz2` image stream with an xpak metadata blob appended,
//! followed by a fixed trailer. The blob is:
//!
//! ```text
//! "XPAKPACK" index_len(4B BE) data_len(4B BE) index data "XPAKSTOP"
//! ```
//!
//! Each index entry is `pathname_len(4B BE) pathname data_offset(4B BE)
//! data_len(4B BE)`, and the offsets index into the data section. The whole file
//! ends with `"XPAKSTOP" xpak_offset(4B BE) "STOP"`, where `xpak_offset` is the
//! distance from the start of the blob back from the end-of-file region. The
//! importer is read-only and does not rewrite the source.

use crate::compress::Compression;
use crate::error::ContainerError;
use crate::metadata::MetadataMap;

const XPAK_PACK: &[u8; 8] = b"XPAKPACK";
const XPAK_STOP: &[u8; 8] = b"XPAKSTOP";
const STOP: &[u8; 4] = b"STOP";

/// An imported xpak/tbz2 container: its metadata and a borrow of its image.
pub struct XpakPackage<'a> {
    metadata: MetadataMap,
    image: &'a [u8],
}

impl<'a> XpakPackage<'a> {
    /// The recovered canonical metadata map.
    pub fn metadata(&self) -> &MetadataMap {
        &self.metadata
    }

    /// The leading `tar.bz2` image stream, borrowed from the source bytes.
    pub fn image(&self) -> &'a [u8] {
        self.image
    }
}

/// Whether `bytes` looks like a tbz2 file (ends with the `STOP` trailer).
pub fn is_xpak(bytes: &[u8]) -> bool {
    bytes.len() >= STOP.len() && &bytes[bytes.len() - STOP.len()..] == STOP
}

/// Import a tbz2 file, recovering its metadata and exposing its image stream.
pub fn read(bytes: &[u8]) -> Result<XpakPackage<'_>, ContainerError> {
    let span = tracing::info_span!("binpkg.xpak.read", size = bytes.len());
    let _enter = span.enter();

    // Trailer: ... blob(ending in "XPAKSTOP") offset(4B) "STOP", where the blob's
    // closing XPAKSTOP is the trailer marker and `offset` is the blob length.
    let trailer_len = XPAK_STOP.len() + 4 + STOP.len();
    if bytes.len() < trailer_len {
        return Err(ContainerError::MalformedXpak(
            "file shorter than trailer".into(),
        ));
    }
    let end = bytes.len();
    if &bytes[end - STOP.len()..] != STOP {
        return Err(ContainerError::MalformedXpak("missing STOP trailer".into()));
    }
    let off_start = end - STOP.len() - 4;
    let xpak_offset = read_u32_be(bytes, off_start)
        .ok_or_else(|| ContainerError::MalformedXpak("truncated trailer offset".into()))?
        as usize;

    // The blob ends just before the offset field and must close with XPAKSTOP.
    let blob_end = off_start;
    if blob_end < XPAK_STOP.len() || &bytes[blob_end - XPAK_STOP.len()..blob_end] != XPAK_STOP {
        return Err(ContainerError::MalformedXpak(
            "missing XPAKSTOP trailer".into(),
        ));
    }
    let blob_start = blob_end
        .checked_sub(xpak_offset)
        .ok_or_else(|| ContainerError::MalformedXpak("xpak offset underflows file".into()))?;
    let blob = &bytes[blob_start..blob_end];
    let metadata = parse_blob(blob)?;

    let image = &bytes[..blob_start];
    tracing::info!(
        entries = metadata.len(),
        image = image.len(),
        "xpak imported"
    );
    Ok(XpakPackage { metadata, image })
}

/// Parse the xpak blob (`XPAKPACK ... XPAKSTOP`) into a metadata map.
fn parse_blob(blob: &[u8]) -> Result<MetadataMap, ContainerError> {
    if blob.len() < XPAK_PACK.len() + 8 + XPAK_STOP.len() {
        return Err(ContainerError::MalformedXpak("blob too short".into()));
    }
    if &blob[..XPAK_PACK.len()] != XPAK_PACK {
        return Err(ContainerError::MalformedXpak(
            "missing XPAKPACK header".into(),
        ));
    }
    let mut cursor = XPAK_PACK.len();
    let index_len = read_u32_be(blob, cursor)
        .ok_or_else(|| ContainerError::MalformedXpak("truncated index length".into()))?
        as usize;
    cursor += 4;
    let data_len = read_u32_be(blob, cursor)
        .ok_or_else(|| ContainerError::MalformedXpak("truncated data length".into()))?
        as usize;
    cursor += 4;

    let index_start = cursor;
    let index_end = index_start
        .checked_add(index_len)
        .filter(|&e| e <= blob.len())
        .ok_or_else(|| ContainerError::MalformedXpak("index exceeds blob".into()))?;
    let data_start = index_end;
    let data_end = data_start
        .checked_add(data_len)
        .filter(|&e| e <= blob.len())
        .ok_or_else(|| ContainerError::MalformedXpak("data exceeds blob".into()))?;
    let data = &blob[data_start..data_end];

    let mut metadata = MetadataMap::new();
    let index = &blob[index_start..index_end];
    let mut i = 0;
    while i < index.len() {
        let name_len = read_u32_be(index, i)
            .ok_or_else(|| ContainerError::MalformedXpak("truncated entry name length".into()))?
            as usize;
        i += 4;
        let name_end = i
            .checked_add(name_len)
            .filter(|&e| e <= index.len())
            .ok_or_else(|| ContainerError::MalformedXpak("entry name exceeds index".into()))?;
        let name = String::from_utf8_lossy(&index[i..name_end]).into_owned();
        i = name_end;
        let data_offset = read_u32_be(index, i)
            .ok_or_else(|| ContainerError::MalformedXpak("truncated entry data offset".into()))?
            as usize;
        i += 4;
        let entry_len = read_u32_be(index, i)
            .ok_or_else(|| ContainerError::MalformedXpak("truncated entry data length".into()))?
            as usize;
        i += 4;

        let entry_end = data_offset
            .checked_add(entry_len)
            .filter(|&e| e <= data.len())
            .ok_or_else(|| ContainerError::MalformedXpak("entry data out of range".into()))?;
        metadata.insert(name, data[data_offset..entry_end].to_vec());
    }
    Ok(metadata)
}

fn read_u32_be(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Build an xpak blob from a name-to-bytes metadata map.
///
/// Used by tests and as the inverse of [`parse_blob`]. Entries are emitted in
/// the map's key order.
pub fn build_blob(metadata: &MetadataMap) -> Vec<u8> {
    let mut index = Vec::new();
    let mut data = Vec::new();
    for (name, value) in metadata.iter() {
        let offset = data.len() as u32;
        index.extend_from_slice(&(name.len() as u32).to_be_bytes());
        index.extend_from_slice(name.as_bytes());
        index.extend_from_slice(&offset.to_be_bytes());
        index.extend_from_slice(&(value.len() as u32).to_be_bytes());
        data.extend_from_slice(value);
    }
    let mut blob = Vec::new();
    blob.extend_from_slice(XPAK_PACK);
    blob.extend_from_slice(&(index.len() as u32).to_be_bytes());
    blob.extend_from_slice(&(data.len() as u32).to_be_bytes());
    blob.extend_from_slice(&index);
    blob.extend_from_slice(&data);
    blob.extend_from_slice(XPAK_STOP);
    blob
}

/// Build a complete tbz2 file from an image stream and a metadata map.
///
/// Write a `BINPKG_FORMAT=xpak` `.tbz2` container from a root-relative image tar
/// and `metadata`: bzip2-compress the image and append the xpak blob. Portage
/// installs the result unchanged.
pub fn write_tbz2(metadata: &MetadataMap, image_tar: &[u8]) -> Result<Vec<u8>, ContainerError> {
    let compressed = Compression::Bzip2.compress(image_tar)?;
    Ok(build_tbz2(&compressed, metadata))
}

/// Used by tests. `image` is the leading `tar.bz2` stream emitted verbatim.
pub fn build_tbz2(image: &[u8], metadata: &MetadataMap) -> Vec<u8> {
    let blob = build_blob(metadata);
    let mut out = Vec::with_capacity(image.len() + blob.len() + 8);
    out.extend_from_slice(image);
    out.extend_from_slice(&blob);
    out.extend_from_slice(&(blob.len() as u32).to_be_bytes());
    out.extend_from_slice(STOP);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{KEY_CHOST, KEY_USE};

    fn sample_metadata() -> MetadataMap {
        let mut m = MetadataMap::new();
        m.set_str(KEY_CHOST, "x86_64-pc-linux-gnu");
        m.set_str(KEY_USE, "ssl zlib");
        m.set_str("EAPI", "8");
        m
    }

    #[test]
    fn round_trip_in_memory_tbz2() {
        let image = b"FAKE-TAR-BZ2-STREAM".repeat(20);
        let meta = sample_metadata();
        let file = build_tbz2(&image, &meta);

        assert!(is_xpak(&file));
        let pkg = read(&file).unwrap();
        assert_eq!(pkg.metadata(), &meta);
        assert_eq!(pkg.image(), &image[..]);
    }

    #[test]
    fn missing_stop_rejected() {
        let mut file = build_tbz2(b"img", &sample_metadata());
        let n = file.len();
        file[n - 1] ^= 0xff;
        assert!(read(&file).is_err());
    }

    #[test]
    fn entries_recovered_by_name() {
        let image = b"img";
        let meta = sample_metadata();
        let file = build_tbz2(image, &meta);
        let pkg = read(&file).unwrap();
        assert_eq!(pkg.metadata().get_str(KEY_USE).as_deref(), Some("ssl zlib"));
    }
}
