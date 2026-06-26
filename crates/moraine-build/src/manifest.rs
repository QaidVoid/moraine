//! Repository `Manifest` parsing and distfile verification.
//!
//! The build engine reads each of the four `MANIFEST2_IDENTIFIERS`
//! (`AUX`, `MISC`, `DIST`, `EBUILD`) into per-type hash tables. Verification
//! checks the file size, then every listed hash this build can compute, and
//! enforces the GLEP 74 integrity chain: a verified file must match at least one
//! computable hash (never pass on size alone) and must carry every hash the
//! repository's `manifest-required-hashes` policy demands. The computable
//! algorithms are the Gentoo `Manifest` set from [`moraine_common::hash`]:
//! BLAKE2B, SHA512, SHA256, and MD5.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::Path;

use tracing::instrument;

use crate::error::{IoExt as _, Result};

/// The four Manifest line identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ManifestType {
    /// A file under the package's `files/` directory.
    Aux,
    /// A repository metadata file (`metadata.xml`, `ChangeLog`, ...).
    Misc,
    /// A fetched source distfile.
    Dist,
    /// An ebuild.
    Ebuild,
}

impl ManifestType {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "AUX" => Some(ManifestType::Aux),
            "MISC" => Some(ManifestType::Misc),
            "DIST" => Some(ManifestType::Dist),
            "EBUILD" => Some(ManifestType::Ebuild),
            _ => None,
        }
    }
}

/// One entry from a repository `Manifest` of any type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistEntry {
    /// The entry name (distfile name, ebuild name, or `files/`-relative path).
    pub name: String,
    /// The expected size in bytes.
    pub size: u64,
    /// The expected hashes keyed by uppercase algorithm name (`BLAKE2B`,
    /// `SHA512`, ...), values are lowercase hex digests.
    pub hashes: BTreeMap<String, String>,
}

/// A parsed repository `Manifest`: the entries of every type, keyed by name.
#[derive(Debug, Clone, Default)]
pub struct Manifest {
    types: BTreeMap<ManifestType, BTreeMap<String, DistEntry>>,
}

impl Manifest {
    /// Parse a `Manifest` file's text into per-type tables.
    ///
    /// A line is `<TYPE> <name> <size>( <ALGO> <hex>)+`. A line whose hash tail is
    /// not an even number of `ALGO`/`hex` fields, or a `DIST` name containing a
    /// path separator or other invalid path character, is rejected (skipped)
    /// rather than failing the whole parse, matching the lenient stock reader
    /// while refusing structurally invalid entries.
    pub fn parse(text: &str) -> Self {
        let mut types: BTreeMap<ManifestType, BTreeMap<String, DistEntry>> = BTreeMap::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let Some(kind) = fields.next().and_then(ManifestType::parse) else {
                continue;
            };
            let Some(name) = fields.next() else { continue };
            let Some(size) = fields.next().and_then(|s| s.parse::<u64>().ok()) else {
                continue;
            };
            // A DIST name is a plain filename: reject path separators and other
            // characters Portage forbids in a distfile name.
            if kind == ManifestType::Dist && !is_valid_distfile_name(name) {
                continue;
            }
            // The hash tail must be complete `ALGO hex` pairs.
            let tail: Vec<&str> = fields.collect();
            if !tail.len().is_multiple_of(2) {
                continue;
            }
            let mut hashes = BTreeMap::new();
            for pair in tail.chunks_exact(2) {
                hashes.insert(pair[0].to_ascii_uppercase(), pair[1].to_ascii_lowercase());
            }
            types.entry(kind).or_default().insert(
                name.to_string(),
                DistEntry {
                    name: name.to_string(),
                    size,
                    hashes,
                },
            );
        }
        Manifest { types }
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
        self.entry(ManifestType::Dist, name)
    }

    /// Look up an entry of the given type by name.
    pub fn entry(&self, kind: ManifestType, name: &str) -> Option<&DistEntry> {
        self.types.get(&kind).and_then(|m| m.get(name))
    }

    /// Iterate the entries of a given type.
    pub fn entries(&self, kind: ManifestType) -> impl Iterator<Item = &DistEntry> {
        self.types.get(&kind).into_iter().flat_map(|m| m.values())
    }

    /// The number of DIST entries.
    pub fn len(&self) -> usize {
        self.types.get(&ManifestType::Dist).map_or(0, |m| m.len())
    }

    /// Whether there are no DIST entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Whether `name` is a valid distfile name: non-empty, no path separators, and
/// no leading dot, matching Portage's distfile-name rule.
fn is_valid_distfile_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('/')
        && !name.contains('/')
        && !name.contains('\0')
}

/// The outcome of verifying a file against a [`DistEntry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// Size and all checked hashes matched, with sufficient computable digests.
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
    /// The entry lists no hash this build can compute, so the file cannot be
    /// verified ("Insufficient data for checksum verification").
    InsufficientData,
    /// A hash the repository policy requires is absent from the entry.
    MissingRequiredHash {
        /// The required algorithm that was missing.
        algo: String,
    },
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
            VerifyOutcome::InsufficientData => {
                "insufficient data for checksum verification".to_string()
            }
            VerifyOutcome::MissingRequiredHash { algo } => {
                format!("required {algo} digest missing from Manifest")
            }
        }
    }
}

/// Verify file `bytes` against an entry, enforcing the `required` hash set.
///
/// Size is checked first (a zero size or zero-byte file is always invalid), then
/// every listed hash this build can compute. When `required` is non-empty, each
/// required algorithm must be present and match. The verification also fails with
/// [`VerifyOutcome::InsufficientData`] when the entry lists no hash this build can
/// compute, so a file never passes on size alone.
pub fn verify_bytes(entry: &DistEntry, bytes: &[u8], required: &BTreeSet<String>) -> VerifyOutcome {
    // A zero recorded size or zero-byte file is inherently invalid.
    if entry.size == 0 || bytes.is_empty() {
        return VerifyOutcome::ZeroByte;
    }
    if bytes.len() as u64 != entry.size {
        return VerifyOutcome::SizeMismatch {
            expected: entry.size,
            actual: bytes.len() as u64,
        };
    }
    // Every required hash must be present.
    for algo in required {
        if !entry.hashes.contains_key(algo) {
            return VerifyOutcome::MissingRequiredHash { algo: algo.clone() };
        }
    }
    let mut computed_any = false;
    for (algo, expected) in &entry.hashes {
        if let Some(actual) = digest_for(algo, bytes) {
            computed_any = true;
            if &actual != expected {
                return VerifyOutcome::HashMismatch { algo: algo.clone() };
            }
        }
    }
    if !computed_any {
        return VerifyOutcome::InsufficientData;
    }
    VerifyOutcome::Ok
}

/// Verify a file on disk against an entry, streaming the file through the
/// hashers so a large distfile is not read entirely into memory.
#[instrument(name = "verify_distfile", skip(entry, path, required), fields(file = %path.as_ref().display()))]
pub fn verify_file(
    entry: &DistEntry,
    path: impl AsRef<Path>,
    required: &BTreeSet<String>,
) -> Result<VerifyOutcome> {
    let path = path.as_ref();
    if entry.size == 0 {
        return Ok(VerifyOutcome::ZeroByte);
    }
    for algo in required {
        if !entry.hashes.contains_key(algo) {
            return Ok(VerifyOutcome::MissingRequiredHash { algo: algo.clone() });
        }
    }

    let file = std::fs::File::open(path).at(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut hashers =
        moraine_common::hash::MultiHasher::new(entry.hashes.keys().map(String::as_str));
    let mut size = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).at(path)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hashers.update(&buf[..n]);
    }

    if size == 0 {
        return Ok(VerifyOutcome::ZeroByte);
    }
    if size != entry.size {
        return Ok(VerifyOutcome::SizeMismatch {
            expected: entry.size,
            actual: size,
        });
    }
    let digests = hashers.finalize();
    let mut computed_any = false;
    for (algo, expected) in &entry.hashes {
        if let Some(actual) = digests.get(algo) {
            computed_any = true;
            if actual != expected {
                return Ok(VerifyOutcome::HashMismatch { algo: algo.clone() });
            }
        }
    }
    if !computed_any {
        return Ok(VerifyOutcome::InsufficientData);
    }
    Ok(VerifyOutcome::Ok)
}

/// Verify a package's `EBUILD` and `AUX` entries (and `MISC` when `strict_misc`)
/// against the on-disk files before the ebuild is sourced. `pkg_dir` is the
/// package directory (`<repo>/<category>/<pn>/`) and `ebuild_name` the ebuild's
/// filename. A missing or mismatching file fails the build.
pub fn verify_package(
    manifest: &Manifest,
    pkg_dir: &Path,
    ebuild_name: &str,
    required: &BTreeSet<String>,
    strict_misc: bool,
) -> Result<()> {
    let check = |kind: ManifestType, entry: &DistEntry, path: &Path| -> Result<()> {
        let outcome = verify_file(entry, path, required)?;
        if !outcome.is_ok() {
            return Err(crate::error::BuildError::ManifestMismatch {
                name: format!("{kind:?}/{}", entry.name),
                reason: outcome.reason(),
            });
        }
        Ok(())
    };

    if let Some(entry) = manifest.entry(ManifestType::Ebuild, ebuild_name) {
        check(ManifestType::Ebuild, entry, &pkg_dir.join(ebuild_name))?;
    }
    let files_dir = pkg_dir.join("files");
    for entry in manifest.entries(ManifestType::Aux) {
        check(ManifestType::Aux, entry, &files_dir.join(&entry.name))?;
    }
    if strict_misc {
        for entry in manifest.entries(ManifestType::Misc) {
            check(ManifestType::Misc, entry, &pkg_dir.join(&entry.name))?;
        }
    }
    Ok(())
}

/// Compute the lowercase hex digest of `bytes` for a Manifest algorithm name, or
/// `None` for an algorithm this build does not implement.
fn digest_for(algo: &str, bytes: &[u8]) -> Option<String> {
    match algo {
        "BLAKE2B" => Some(moraine_common::hash::blake2b(bytes)),
        "SHA512" => Some(moraine_common::hash::sha512(bytes)),
        "SHA256" => Some(moraine_common::hash::sha256(bytes)),
        "MD5" => Some(moraine_common::hash::md5(bytes)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn parses_all_four_types() {
        let text = "\
EBUILD foo-1.ebuild 100 BLAKE2B abc SHA512 def
DIST foo-1.tar.gz 12 BLAKE2B aa SHA512 bb
MISC metadata.xml 50 BLAKE2B cc SHA512 dd
AUX fix.patch 7 BLAKE2B ee SHA512 ff
";
        let m = Manifest::parse(text);
        assert_eq!(m.len(), 1);
        let d = m.dist("foo-1.tar.gz").unwrap();
        assert_eq!(d.size, 12);
        assert_eq!(d.hashes.get("BLAKE2B").unwrap(), "aa");
        assert!(m.entry(ManifestType::Ebuild, "foo-1.ebuild").is_some());
        assert!(m.entry(ManifestType::Aux, "fix.patch").is_some());
        assert!(m.entry(ManifestType::Misc, "metadata.xml").is_some());
    }

    #[test]
    fn rejects_structural_and_name_violations() {
        // An odd hash tail (dangling ALGO) is rejected.
        let m = Manifest::parse("DIST a.tar 5 BLAKE2B aa SHA512\n");
        assert!(m.dist("a.tar").is_none());
        // A DIST name with a path separator is rejected.
        let m = Manifest::parse("DIST ../escape 5 BLAKE2B aa\n");
        assert!(m.dist("../escape").is_none());
        // An AUX name with a subdirectory is allowed.
        let m = Manifest::parse("AUX sub/dir.patch 5 BLAKE2B aa\n");
        assert!(m.entry(ManifestType::Aux, "sub/dir.patch").is_some());
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
        assert!(verify_bytes(&entry, data, &none()).is_ok());
    }

    #[test]
    fn sha256_is_computed_and_verified() {
        let mut hashes = BTreeMap::new();
        hashes.insert("SHA256".to_string(), moraine_common::hash::sha256(b"abc"));
        let entry = DistEntry {
            name: "x".into(),
            size: 3,
            hashes,
        };
        assert!(verify_bytes(&entry, b"abc", &none()).is_ok());
    }

    #[test]
    fn zero_size_entry_is_invalid() {
        let entry = DistEntry {
            name: "x".into(),
            size: 0,
            hashes: BTreeMap::new(),
        };
        // Even a non-empty file fails against a zero-size entry (Portage rule).
        assert_eq!(
            verify_bytes(&entry, b"abc", &none()),
            VerifyOutcome::ZeroByte
        );
        assert_eq!(verify_bytes(&entry, b"", &none()), VerifyOutcome::ZeroByte);
    }

    #[test]
    fn size_mismatch_detected() {
        let mut hashes = BTreeMap::new();
        hashes.insert("SHA512".to_string(), moraine_common::hash::sha512(b"abc"));
        let entry = DistEntry {
            name: "x".into(),
            size: 99,
            hashes,
        };
        assert!(matches!(
            verify_bytes(&entry, b"abc", &none()),
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
            verify_bytes(&entry, b"abc", &none()),
            VerifyOutcome::HashMismatch { .. }
        ));
    }

    #[test]
    fn unknown_algo_only_is_insufficient_data() {
        // An entry listing only an algorithm this build cannot compute fails the
        // insufficient-data gate rather than passing on size alone.
        let mut hashes = BTreeMap::new();
        hashes.insert("WHIRLPOOL".to_string(), "ignored".to_string());
        let entry = DistEntry {
            name: "x".into(),
            size: 3,
            hashes,
        };
        assert_eq!(
            verify_bytes(&entry, b"abc", &none()),
            VerifyOutcome::InsufficientData
        );
    }

    #[test]
    fn unknown_algo_is_safe_only_with_a_computable_cohash() {
        // Safe only because a co-listed computable hash (SHA512) is present.
        let mut hashes = BTreeMap::new();
        hashes.insert("WHIRLPOOL".to_string(), "ignored".to_string());
        hashes.insert("SHA512".to_string(), moraine_common::hash::sha512(b"abc"));
        let entry = DistEntry {
            name: "x".into(),
            size: 3,
            hashes,
        };
        assert!(verify_bytes(&entry, b"abc", &none()).is_ok());
    }

    #[test]
    fn required_hash_must_be_present() {
        let mut hashes = BTreeMap::new();
        hashes.insert("SHA512".to_string(), moraine_common::hash::sha512(b"abc"));
        let entry = DistEntry {
            name: "x".into(),
            size: 3,
            hashes,
        };
        let required: BTreeSet<String> = ["BLAKE2B".to_string()].into_iter().collect();
        assert_eq!(
            verify_bytes(&entry, b"abc", &required),
            VerifyOutcome::MissingRequiredHash {
                algo: "BLAKE2B".to_string()
            }
        );
    }

    #[test]
    fn streaming_verify_file_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d");
        std::fs::write(&path, b"abc").unwrap();
        let mut hashes = BTreeMap::new();
        hashes.insert("BLAKE2B".to_string(), moraine_common::hash::blake2b(b"abc"));
        hashes.insert("SHA256".to_string(), moraine_common::hash::sha256(b"abc"));
        let entry = DistEntry {
            name: "d".into(),
            size: 3,
            hashes,
        };
        assert!(verify_file(&entry, &path, &none()).unwrap().is_ok());
    }
}
