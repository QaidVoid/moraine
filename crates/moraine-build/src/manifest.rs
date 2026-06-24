//! Repository `Manifest` parsing and distfile verification.
//!
//! `moraine-repo` does not parse `Manifest` files, so the build engine reads the
//! DIST digests itself. Verification checks the file size first and then every
//! listed hash, treating a zero-byte file or any mismatch as a failure. The
//! supported hash algorithms are the Gentoo `Manifest` defaults from
//! [`moraine_common::hash`]: BLAKE2B and SHA512 (with MD5 and SHA256 tolerated
//! where present in older Manifests).

use std::collections::BTreeMap;
use std::path::Path;

use tracing::instrument;

use crate::error::{IoExt as _, Result};

/// One `DIST` entry from a repository `Manifest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistEntry {
    /// The distfile name.
    pub name: String,
    /// The expected size in bytes.
    pub size: u64,
    /// The expected hashes keyed by uppercase algorithm name (`BLAKE2B`,
    /// `SHA512`, ...), values are lowercase hex digests.
    pub hashes: BTreeMap<String, String>,
}

/// The DIST entries of a repository `Manifest`, keyed by distfile name.
#[derive(Debug, Clone, Default)]
pub struct Manifest {
    entries: BTreeMap<String, DistEntry>,
}

impl Manifest {
    /// Parse a `Manifest` file's text, keeping only the `DIST` lines.
    ///
    /// A `DIST` line is `DIST <name> <size> [<ALGO> <hex> ...]`. Malformed lines
    /// are skipped rather than failing the whole parse, matching the lenient
    /// stock reader.
    pub fn parse(text: &str) -> Self {
        let mut entries = BTreeMap::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            if fields.next() != Some("DIST") {
                continue;
            }
            let Some(name) = fields.next() else { continue };
            let Some(size) = fields.next().and_then(|s| s.parse::<u64>().ok()) else {
                continue;
            };
            let mut hashes = BTreeMap::new();
            while let Some(algo) = fields.next() {
                let Some(hex) = fields.next() else { break };
                hashes.insert(algo.to_ascii_uppercase(), hex.to_ascii_lowercase());
            }
            entries.insert(
                name.to_string(),
                DistEntry {
                    name: name.to_string(),
                    size,
                    hashes,
                },
            );
        }
        Manifest { entries }
    }

    /// Read and parse a `Manifest` file from disk. A missing file yields an empty
    /// manifest.
    #[instrument(name = "manifest_read", skip_all, fields(path = %path.as_ref().display()))]
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
            Err(e) => Err(e).at(path),
        }
    }

    /// Look up a DIST entry by distfile name.
    pub fn dist(&self, name: &str) -> Option<&DistEntry> {
        self.entries.get(name)
    }

    /// The number of DIST entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no DIST entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The outcome of verifying a file against a [`DistEntry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// Size and all checked hashes matched.
    Ok,
    /// The file size did not match the expected size.
    SizeMismatch {
        /// The expected size.
        expected: u64,
        /// The actual size.
        actual: u64,
    },
    /// A hash did not match.
    HashMismatch {
        /// The algorithm whose digest mismatched.
        algo: String,
    },
    /// The file is zero bytes, which is always invalid.
    ZeroByte,
}

impl VerifyOutcome {
    /// Whether the file verified successfully.
    pub fn is_ok(&self) -> bool {
        matches!(self, VerifyOutcome::Ok)
    }

    /// A short human description of a failure, or `"ok"`.
    pub fn reason(&self) -> String {
        match self {
            VerifyOutcome::Ok => "ok".to_string(),
            VerifyOutcome::SizeMismatch { expected, actual } => {
                format!("size mismatch: expected {expected}, got {actual}")
            }
            VerifyOutcome::HashMismatch { algo } => format!("{algo} digest mismatch"),
            VerifyOutcome::ZeroByte => "zero-byte file".to_string(),
        }
    }
}

/// Verify file `bytes` against a DIST entry. Size is checked first, then every
/// hash the entry lists for an algorithm this build understands.
pub fn verify_bytes(entry: &DistEntry, bytes: &[u8]) -> VerifyOutcome {
    if bytes.is_empty() {
        return VerifyOutcome::ZeroByte;
    }
    if bytes.len() as u64 != entry.size {
        return VerifyOutcome::SizeMismatch {
            expected: entry.size,
            actual: bytes.len() as u64,
        };
    }
    for (algo, expected) in &entry.hashes {
        if let Some(actual) = digest_for(algo, bytes)
            && &actual != expected
        {
            return VerifyOutcome::HashMismatch { algo: algo.clone() };
        }
    }
    VerifyOutcome::Ok
}

/// Verify a file on disk against a DIST entry.
#[instrument(name = "verify_distfile", skip(entry, path), fields(file = %path.as_ref().display()))]
pub fn verify_file(entry: &DistEntry, path: impl AsRef<Path>) -> Result<VerifyOutcome> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).at(path)?;
    Ok(verify_bytes(entry, &bytes))
}

/// Compute the lowercase hex digest of `bytes` for a Manifest algorithm name, or
/// `None` for an algorithm this build does not implement.
fn digest_for(algo: &str, bytes: &[u8]) -> Option<String> {
    match algo {
        "BLAKE2B" => Some(moraine_common::hash::blake2b(bytes)),
        "SHA512" => Some(moraine_common::hash::sha512(bytes)),
        "MD5" => Some(moraine_common::hash::md5(bytes)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dist_lines_only() {
        let text = "\
EBUILD foo-1.ebuild 100 BLAKE2B abc SHA512 def
DIST foo-1.tar.gz 12 BLAKE2B aa SHA512 bb
MISC metadata.xml 50 BLAKE2B cc
";
        let m = Manifest::parse(text);
        assert_eq!(m.len(), 1);
        let d = m.dist("foo-1.tar.gz").unwrap();
        assert_eq!(d.size, 12);
        assert_eq!(d.hashes.get("BLAKE2B").unwrap(), "aa");
        assert_eq!(d.hashes.get("SHA512").unwrap(), "bb");
    }

    #[test]
    fn verify_matches_real_digests() {
        let data = b"abc";
        let mut hashes = BTreeMap::new();
        hashes.insert("BLAKE2B".to_string(), moraine_common::hash::blake2b(data));
        hashes.insert("SHA512".to_string(), moraine_common::hash::sha512(data));
        let entry = DistEntry {
            name: "x".into(),
            size: 3,
            hashes,
        };
        assert!(verify_bytes(&entry, data).is_ok());
    }

    #[test]
    fn zero_byte_is_invalid() {
        let entry = DistEntry {
            name: "x".into(),
            size: 0,
            hashes: BTreeMap::new(),
        };
        assert_eq!(verify_bytes(&entry, b""), VerifyOutcome::ZeroByte);
    }

    #[test]
    fn size_mismatch_detected() {
        let entry = DistEntry {
            name: "x".into(),
            size: 99,
            hashes: BTreeMap::new(),
        };
        assert!(matches!(
            verify_bytes(&entry, b"abc"),
            VerifyOutcome::SizeMismatch { .. }
        ));
    }

    #[test]
    fn hash_mismatch_detected() {
        let mut hashes = BTreeMap::new();
        hashes.insert("SHA512".to_string(), "deadbeef".to_string());
        let entry = DistEntry {
            name: "x".into(),
            size: 3,
            hashes,
        };
        assert!(matches!(
            verify_bytes(&entry, b"abc"),
            VerifyOutcome::HashMismatch { .. }
        ));
    }

    #[test]
    fn unknown_algo_is_skipped() {
        let mut hashes = BTreeMap::new();
        hashes.insert("WHIRLPOOL".to_string(), "ignored".to_string());
        hashes.insert("SHA512".to_string(), moraine_common::hash::sha512(b"abc"));
        let entry = DistEntry {
            name: "x".into(),
            size: 3,
            hashes,
        };
        assert!(verify_bytes(&entry, b"abc").is_ok());
    }
}
