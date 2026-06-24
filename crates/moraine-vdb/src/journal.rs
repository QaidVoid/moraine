//! The append-only delta journal.
//!
//! A single-package add or remove is appended here instead of rewriting the
//! whole primary file. Each delta is a self-contained length-prefixed
//! `rmp-serde` frame: a `u32` little-endian length followed by that many encoded
//! bytes. The length prefix lets the loader detect and discard a partial
//! trailing record left by a crash mid-write.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use crate::error::{IoResultExt as _, VdbError};
use crate::wire::WireDelta;

/// Append a delta frame to the journal at `path`, creating it if needed.
pub(crate) fn append(path: &Path, delta: &WireDelta) -> Result<(), VdbError> {
    let body = rmp_serde::to_vec(delta).map_err(|source| VdbError::EncodeStore { source })?;
    let len = u32::try_from(body.len()).map_err(|_| VdbError::EncodeStore {
        source: rmp_serde::encode::Error::Syntax("delta frame too large".to_string()),
    })?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_path(path)?;
    file.write_all(&len.to_le_bytes()).with_path(path)?;
    file.write_all(&body).with_path(path)?;
    file.sync_all().with_path(path)?;
    Ok(())
}

/// Read every complete delta frame from `bytes`, discarding a partial trailing
/// frame. A short length prefix or a body shorter than its prefix claims marks
/// the end of the intact region.
pub(crate) fn read_all(bytes: &[u8]) -> Result<Vec<WireDelta>, VdbError> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= bytes.len() {
        let len = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        let body_start = pos + 4;
        let body_end = match body_start.checked_add(len) {
            Some(end) if end <= bytes.len() => end,
            // Partial trailing frame: stop here and keep what came before.
            _ => break,
        };
        match rmp_serde::from_slice::<WireDelta>(&bytes[body_start..body_end]) {
            Ok(delta) => out.push(delta),
            // A frame that decodes wrong is treated as a corrupt trailing tail.
            Err(_) => break,
        }
        pos = body_end;
    }
    Ok(out)
}
