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
    /// xz, decompressed via the system `xz` tool.
    Xz,
    /// lz4, decompressed via the system `lz4` tool.
    Lz4,
    /// lzip (`.lz`), decompressed via the system `lzip` tool.
    Lzip,
    /// lzop (`.lzo`), decompressed via the system `lzop` tool.
    Lzop,
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
            Compression::Xz => "xz",
            Compression::Lz4 => "lz4",
            Compression::Lzip => "lz",
            Compression::Lzop => "lzo",
        }
    }

    /// Parse a GPKG member suffix into a codec.
    ///
    /// Accepts the suffix after `tar.` (for example `bz2`) or an empty string
    /// for an uncompressed inner tar, covering the full gpkg suffix set so any
    /// genuine Portage container is recognized. The `xz`/`lz4`/`lz`/`lzo` codecs
    /// decompress through the matching system tool.
    pub fn from_suffix(suffix: &str) -> Result<Self, ContainerError> {
        match suffix {
            "" | "tar" => Ok(Compression::None),
            "bz2" => Ok(Compression::Bzip2),
            "gz" => Ok(Compression::Gzip),
            "zst" | "zstd" => Ok(Compression::Zstd),
            "xz" => Ok(Compression::Xz),
            "lz4" => Ok(Compression::Lz4),
            "lz" => Ok(Compression::Lzip),
            "lzo" => Ok(Compression::Lzop),
            other => Err(ContainerError::UnsupportedCompression(other.to_string())),
        }
    }

    /// The system tool that decompresses this codec, when it is not handled
    /// in-process.
    fn system_tool(self) -> Option<&'static str> {
        match self {
            Compression::Xz => Some("xz"),
            Compression::Lz4 => Some("lz4"),
            Compression::Lzip => Some("lzip"),
            Compression::Lzop => Some("lzop"),
            _ => None,
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
            // The writer never emits these; the reader decompresses them.
            Compression::Xz | Compression::Lz4 | Compression::Lzip | Compression::Lzop => Err(
                ContainerError::UnsupportedCompression(format!("{}: write", self.suffix())),
            ),
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
            Compression::Xz | Compression::Lz4 | Compression::Lzip | Compression::Lzop => {
                decompress_with_tool(self.system_tool().unwrap(), data)
            }
        }
    }
}

/// Decompress `data` by piping it through `tool -dc` (stdin to stdout), the
/// fallback for codecs without an in-process crate, matching Portage which
/// shells out to the same tools.
fn decompress_with_tool(tool: &str, data: &[u8]) -> Result<Vec<u8>, ContainerError> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(tool)
        .arg("-dc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(ContainerError::IoBare)?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(data)
        .map_err(ContainerError::IoBare)?;
    let output = child.wait_with_output().map_err(ContainerError::IoBare)?;
    if !output.status.success() {
        return Err(ContainerError::UnsupportedCompression(format!(
            "{tool} decompression failed"
        )));
    }
    Ok(output.stdout)
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
    fn full_gpkg_suffix_set_recognized() {
        // The whole gpkg suffix set is recognized; the system-tool codecs map to
        // their variants, and a truly unknown suffix still errors.
        assert_eq!(Compression::from_suffix("xz").unwrap(), Compression::Xz);
        assert_eq!(Compression::from_suffix("lz4").unwrap(), Compression::Lz4);
        assert_eq!(Compression::from_suffix("lz").unwrap(), Compression::Lzip);
        assert_eq!(Compression::from_suffix("lzo").unwrap(), Compression::Lzop);
        assert_eq!(Compression::Xz.suffix(), "xz");
        assert!(matches!(
            Compression::from_suffix("bogus"),
            Err(ContainerError::UnsupportedCompression(_))
        ));
    }
}
