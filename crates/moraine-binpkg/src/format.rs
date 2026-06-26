//! Output container format selection (`BINPKG_FORMAT`).

use crate::compress::Compression;
use crate::error::ContainerError;
use crate::metadata::MetadataMap;

/// The binary-package container format to produce, from `BINPKG_FORMAT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BinpkgFormat {
    /// The modern GPKG container (`.gpkg.tar`), Portage's default.
    #[default]
    Gpkg,
    /// The legacy xpak `.tbz2` container.
    Xpak,
}

impl BinpkgFormat {
    /// Parse a `BINPKG_FORMAT` value, defaulting to [`BinpkgFormat::Gpkg`].
    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "xpak" => BinpkgFormat::Xpak,
            _ => BinpkgFormat::Gpkg,
        }
    }

    /// The on-disk filename extension for this format.
    pub fn extension(self) -> &'static str {
        match self {
            BinpkgFormat::Gpkg => "gpkg.tar",
            BinpkgFormat::Xpak => "tbz2",
        }
    }

    /// Write a container of this format from `metadata` and a root-relative
    /// `image_tar`, compressing a gpkg's inner tars with `comp`.
    pub fn write(
        self,
        metadata: &MetadataMap,
        image_tar: &[u8],
        comp: Compression,
    ) -> Result<Vec<u8>, ContainerError> {
        match self {
            BinpkgFormat::Gpkg => crate::gpkg::write(metadata, image_tar, comp),
            BinpkgFormat::Xpak => crate::xpak::write_tbz2(metadata, image_tar),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defaults_to_gpkg() {
        assert_eq!(BinpkgFormat::parse("gpkg"), BinpkgFormat::Gpkg);
        assert_eq!(BinpkgFormat::parse("xpak"), BinpkgFormat::Xpak);
        assert_eq!(BinpkgFormat::parse(""), BinpkgFormat::Gpkg);
        assert_eq!(BinpkgFormat::parse("rpm"), BinpkgFormat::Gpkg);
        assert_eq!(BinpkgFormat::Gpkg.extension(), "gpkg.tar");
        assert_eq!(BinpkgFormat::Xpak.extension(), "tbz2");
    }
}
