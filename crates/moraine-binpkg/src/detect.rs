//! Container format detection.
//!
//! Dispatches between the greenfield format, legacy xpak/tbz2, and modern GPKG
//! by inspecting structure and markers. The greenfield magic is checked first,
//! then GPKG (an outer tar with a `gpkg-1` marker), then xpak (a trailing `STOP`
//! trailer).

use crate::error::ContainerError;

/// A detected binary-package container format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// The greenfield format this crate produces.
    Greenfield,
    /// The modern GPKG container.
    Gpkg,
    /// The legacy xpak/tbz2 container.
    Xpak,
}

/// Detect the container format of `bytes`.
pub fn detect(bytes: &[u8]) -> Result<Format, ContainerError> {
    if crate::greenfield::is_greenfield(bytes) {
        return Ok(Format::Greenfield);
    }
    if crate::gpkg::is_gpkg(bytes) {
        return Ok(Format::Gpkg);
    }
    if crate::xpak::is_xpak(bytes) {
        return Ok(Format::Xpak);
    }
    Err(ContainerError::UnknownFormat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::greenfield::{WriteOptions, write_bytes};
    use crate::metadata::MetadataMap;

    #[test]
    fn detects_greenfield() {
        let bytes = write_bytes(&MetadataMap::new(), b"img", &WriteOptions::default()).unwrap();
        assert_eq!(detect(&bytes).unwrap(), Format::Greenfield);
    }

    #[test]
    fn detects_xpak() {
        let file = crate::xpak::build_tbz2(b"img", &MetadataMap::new());
        assert_eq!(detect(&file).unwrap(), Format::Xpak);
    }

    #[test]
    fn unknown_rejected() {
        assert!(matches!(
            detect(b"not a package"),
            Err(ContainerError::UnknownFormat)
        ));
    }
}
