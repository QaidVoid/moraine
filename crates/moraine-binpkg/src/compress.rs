//! Compression codecs for inner tar streams.
//!
//! The greenfield format and the GPKG importer share this small set of codecs.
//! GPKG names the codec in a member suffix (`metadata.tar.<comp>`); the
//! greenfield format records it in its header. Each codec compresses or
//! decompresses a whole in-memory buffer, which suits the metadata section and
//! the test-sized image sections this crate handles.

use std::io::{Read as _, Write as _};

use crate::error::ContainerError;

/// A supported inner-stream compression codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// No compression; the bytes are a plain tar stream.
    None,
    /// bzip2, the codec stock xpak/tbz2 uses for its payload.
    Bzip2,
    /// gzip.
    Gzip,
    /// zstd.
    Zstd,
}

impl Compression {
    /// The GPKG member suffix for this codec (the part after `tar.`).
    ///
    /// Returns an empty string for [`Compression::None`].
    pub fn suffix(self) -> &'static str {
        match self {
            Compression::None => "",
            Compression::Bzip2 => "bz2",
            Compression::Gzip => "gz",
            Compression::Zstd => "zst",
        }
    }

    /// Parse a GPKG member suffix into a codec.
    ///
    /// Accepts the suffix after `tar.` (for example `bz2`) or an empty string
    /// for an uncompressed inner tar. Returns an error for `xz`, which is noted
    /// as a future codec, and for any other unknown suffix.
    pub fn from_suffix(suffix: &str) -> Result<Self, ContainerError> {
        match suffix {
            "" => Ok(Compression::None),
            "bz2" => Ok(Compression::Bzip2),
            "gz" => Ok(Compression::Gzip),
            "zst" | "zstd" => Ok(Compression::Zstd),
            other => Err(ContainerError::UnsupportedCompression(other.to_string())),
        }
    }

    /// Compress `data` with this codec.
    pub fn compress(self, data: &[u8]) -> Result<Vec<u8>, ContainerError> {
        match self {
            Compression::None => Ok(data.to_vec()),
            Compression::Bzip2 => {
                let mut enc =
                    bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
                enc.write_all(data).map_err(ContainerError::IoBare)?;
                enc.finish().map_err(ContainerError::IoBare)
            }
            Compression::Gzip => {
                let mut enc =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                enc.write_all(data).map_err(ContainerError::IoBare)?;
                enc.finish().map_err(ContainerError::IoBare)
            }
            Compression::Zstd => zstd::stream::encode_all(data, 0).map_err(ContainerError::IoBare),
        }
    }

    /// Decompress `data` with this codec.
    pub fn decompress(self, data: &[u8]) -> Result<Vec<u8>, ContainerError> {
        match self {
            Compression::None => Ok(data.to_vec()),
            Compression::Bzip2 => {
                let mut dec = bzip2::read::BzDecoder::new(data);
                let mut out = Vec::new();
                dec.read_to_end(&mut out).map_err(ContainerError::IoBare)?;
                Ok(out)
            }
            Compression::Gzip => {
                let mut dec = flate2::read::GzDecoder::new(data);
                let mut out = Vec::new();
                dec.read_to_end(&mut out).map_err(ContainerError::IoBare)?;
                Ok(out)
            }
            Compression::Zstd => zstd::stream::decode_all(data).map_err(ContainerError::IoBare),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_each_codec() {
        let payload = b"the quick brown fox jumps over the lazy dog".repeat(10);
        for c in [
            Compression::None,
            Compression::Bzip2,
            Compression::Gzip,
            Compression::Zstd,
        ] {
            let comp = c.compress(&payload).unwrap();
            let back = c.decompress(&comp).unwrap();
            assert_eq!(back, payload, "codec {c:?} failed round-trip");
        }
    }

    #[test]
    fn suffix_round_trip() {
        for c in [Compression::Bzip2, Compression::Gzip, Compression::Zstd] {
            assert_eq!(Compression::from_suffix(c.suffix()).unwrap(), c);
        }
        assert_eq!(Compression::from_suffix("").unwrap(), Compression::None);
    }

    #[test]
    fn xz_reported_unsupported() {
        assert!(matches!(
            Compression::from_suffix("xz"),
            Err(ContainerError::UnsupportedCompression(_))
        ));
    }
}
