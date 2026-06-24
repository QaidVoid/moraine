//! Crash recovery: in-progress markers and the recovery procedure.
//!
//! Each operation writes a marker naming the package and whether it is a merge
//! or an unmerge before any mutation, and clears it at the commit point. On
//! invocation the engine scans for markers: an interrupted merge is rolled back
//! to the prior visible state (its record never became visible, so the safe
//! recovery removes the just-placed files), and an interrupted unmerge is re-run,
//! which is idempotent because it removes only paths still owned and matching.

use std::path::Path;

use crate::error::{IoResultExt as _, MergeError};

/// The phase recorded in an in-progress marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    /// A merge was in progress.
    Merge,
    /// An unmerge was in progress.
    Unmerge,
}

impl MarkerKind {
    fn as_str(self) -> &'static str {
        match self {
            MarkerKind::Merge => "merge",
            MarkerKind::Unmerge => "unmerge",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "merge" => Some(MarkerKind::Merge),
            "unmerge" => Some(MarkerKind::Unmerge),
            _ => None,
        }
    }
}

/// A discovered in-progress marker: the operation kind and the package label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// The operation kind that was in progress.
    pub kind: MarkerKind,
    /// The `category/package-version` the operation concerned.
    pub cpv: String,
}

/// Write an in-progress marker for `cpv` of `kind` into `marker_dir`.
pub(crate) fn write_marker(
    marker_dir: &Path,
    kind: MarkerKind,
    cpv: &str,
) -> Result<(), MergeError> {
    std::fs::create_dir_all(marker_dir).with_path(marker_dir)?;
    let path = marker_dir.join("current");
    let body = format!("{}\n{}\n", kind.as_str(), cpv);
    moraine_common::fs::atomic_write(&path, body.as_bytes())?;
    Ok(())
}

/// Clear the in-progress marker, the commit-point side effect.
pub(crate) fn clear_marker(marker_dir: &Path) -> Result<(), MergeError> {
    let path = marker_dir.join("current");
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(MergeError::Io { path, source }),
    }
}

/// Scan `marker_dir` for an in-progress marker, returning it when present.
pub fn scan(marker_dir: &Path) -> Result<Option<Marker>, MergeError> {
    let path = marker_dir.join("current");
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&path).with_path(&path)?;
    let mut lines = body.lines();
    let kind = lines.next().and_then(MarkerKind::parse);
    let cpv = lines.next().map(str::to_string);
    match (kind, cpv) {
        (Some(kind), Some(cpv)) => Ok(Some(Marker { kind, cpv })),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_write_scan_clear_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("in-progress");
        assert_eq!(scan(&md).unwrap(), None);

        write_marker(&md, MarkerKind::Merge, "cat/pkg-1").unwrap();
        assert_eq!(
            scan(&md).unwrap(),
            Some(Marker {
                kind: MarkerKind::Merge,
                cpv: "cat/pkg-1".to_string(),
            })
        );

        clear_marker(&md).unwrap();
        assert_eq!(scan(&md).unwrap(), None);
    }
}
