//! The binhost `Packages` index.
//!
//! A `Packages` file is a header block plus one stanza per package, each a run
//! of `KEY: VALUE` lines, with blank lines separating stanzas. The header
//! carries the producing-host configuration; each stanza carries the
//! installed-store metadata keys extended with BASE_URI, BUILD_ID, BUILD_TIME,
//! PKGINDEX_URI, SIZE, PROVIDES, and REQUIRES.
//!
//! Three keys are renamed at the serialization boundary: DESCRIPTION to DESC,
//! `_mtime_` to MTIME, and `repository` to REPO. The in-memory model uses the
//! canonical names. The use-evaluated dependency keys are reduced against the
//! recorded USE when a stanza is written so the stored strings carry no USE
//! conditionals.

use std::collections::BTreeMap;

use moraine_atom::DepSpec;
use moraine_common::Interner;
use moraine_eapi::PERMISSIVE;

use crate::error::IndexError;
use crate::metadata::{KEY_DESCRIPTION, KEY_MTIME, KEY_REPOSITORY, MetadataMap};

/// The newest `Packages` index version this crate understands.
pub const SUPPORTED_VERSION: u32 = 1;

/// The use-evaluated dependency keys, reduced against recorded USE at write
/// time.
pub const USE_EVALUATED_KEYS: &[&str] = &[
    "BDEPEND",
    "DEPEND",
    "IDEPEND",
    "LICENSE",
    "RDEPEND",
    "PDEPEND",
    "PROPERTIES",
    "RESTRICT",
];

/// The per-package keys that extend the installed-store metadata set.
pub const PER_PACKAGE_EXTRA_KEYS: &[&str] = &[
    "BASE_URI",
    "BUILD_ID",
    "BUILD_TIME",
    "PKGINDEX_URI",
    "SIZE",
    "PROVIDES",
    "REQUIRES",
];

/// The recognized header keys.
pub const HEADER_KEYS: &[&str] = &[
    "ACCEPT_KEYWORDS",
    "ACCEPT_LICENSE",
    "ACCEPT_PROPERTIES",
    "ACCEPT_RESTRICT",
    "CBUILD",
    "CONFIG_PROTECT",
    "FEATURES",
    "GENTOO_MIRRORS",
    "INSTALL_MASK",
    "IUSE_IMPLICIT",
    "USE",
    "USE_EXPAND",
    "USE_EXPAND_HIDDEN",
    "USE_EXPAND_IMPLICIT",
    "USE_EXPAND_UNPREFIXED",
    "VERSION",
];

/// A single package stanza: the cpv plus its metadata map (canonical keys).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageEntry {
    /// The `category/package-version` this stanza describes.
    pub cpv: String,
    /// The canonical metadata for this package.
    pub metadata: MetadataMap,
}

impl PackageEntry {
    /// Resolve this package's download location relative to the header.
    ///
    /// Uses the stanza's PKGINDEX_URI or BASE_URI when present, otherwise the
    /// header's URI, joined with the relative path under `SIZE`/cpv. The result
    /// is `<base>/<cpv>.gpkg.tar` style; the precise suffix is left to the
    /// caller. Returns the resolved base URI joined with the stanza `PATH` key
    /// when present.
    pub fn download_base(&self, header: &BTreeMap<String, String>) -> Option<String> {
        if let Some(uri) = self.metadata.get_str("PKGINDEX_URI") {
            return Some(uri);
        }
        if let Some(uri) = self.metadata.get_str("BASE_URI") {
            return Some(uri);
        }
        header
            .get("URI")
            .or_else(|| header.get("PKGINDEX_URI"))
            .cloned()
    }
}

/// A parsed `Packages` index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackagesIndex {
    /// The header key-value block.
    pub header: BTreeMap<String, String>,
    /// The package stanzas, in file order.
    pub packages: Vec<PackageEntry>,
}

impl PackagesIndex {
    /// Create an empty index with the supported VERSION in its header.
    pub fn new() -> Self {
        let mut header = BTreeMap::new();
        header.insert("VERSION".to_string(), SUPPORTED_VERSION.to_string());
        Self {
            header,
            packages: Vec::new(),
        }
    }

    /// Parse a `Packages` index from its text.
    ///
    /// Applies the DESC/MTIME/REPO translations into the canonical names and
    /// rejects an index whose declared VERSION exceeds [`SUPPORTED_VERSION`].
    pub fn parse(text: &str) -> Result<Self, IndexError> {
        let span = tracing::info_span!("binpkg.index.parse");
        let _enter = span.enter();

        let mut blocks = split_blocks(text);
        let mut index = PackagesIndex {
            header: BTreeMap::new(),
            packages: Vec::new(),
        };
        if let Some(first) = blocks.first()
            && block_is_header(first)
        {
            index.header = parse_kv_block(first)?;
            blocks.remove(0);
        }

        if let Some(version) = index.header.get("VERSION") {
            let declared: u32 = version.trim().parse().unwrap_or(0);
            if declared > SUPPORTED_VERSION {
                return Err(IndexError::UnsupportedVersion {
                    found: declared,
                    supported: SUPPORTED_VERSION,
                });
            }
        }

        for block in blocks {
            let kv = parse_kv_block(&block)?;
            let mut metadata = MetadataMap::new();
            let mut cpv = String::new();
            for (key, value) in kv {
                if key == "CPV" {
                    cpv = value;
                    continue;
                }
                let canonical = from_index_key(&key);
                metadata.set_str(canonical, value);
            }
            index.packages.push(PackageEntry { cpv, metadata });
        }
        tracing::info!(packages = index.packages.len(), "index parsed");
        Ok(index)
    }

    /// Emit the index as `Packages` text.
    ///
    /// Reduces the use-evaluated keys against each stanza's recorded USE and
    /// applies the DESC/MTIME/REPO name translations. `interner` is used to
    /// parse and re-render dependency strings during use-evaluation.
    pub fn emit(&self, interner: &Interner) -> String {
        let span = tracing::info_span!("binpkg.index.emit");
        let _enter = span.enter();

        let mut out = String::new();
        for (key, value) in &self.header {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(value);
            out.push('\n');
        }
        out.push('\n');

        for pkg in &self.packages {
            let use_flags: Vec<String> = pkg.metadata.use_flags();
            let enabled: std::collections::HashSet<_> =
                use_flags.iter().map(|f| interner.intern(f)).collect();

            out.push_str("CPV: ");
            out.push_str(&pkg.cpv);
            out.push('\n');
            for (key, value) in pkg.metadata.iter() {
                let Ok(text) = std::str::from_utf8(value) else {
                    continue;
                };
                let rendered = if USE_EVALUATED_KEYS.contains(&key.as_str()) {
                    reduce_use(text, &enabled, interner)
                } else {
                    text.to_string()
                };
                out.push_str(to_index_key(key));
                out.push_str(": ");
                out.push_str(rendered.trim());
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }

    /// Add or replace the stanza for `entry`'s cpv.
    pub fn upsert(&mut self, entry: PackageEntry) {
        if let Some(slot) = self.packages.iter_mut().find(|p| p.cpv == entry.cpv) {
            *slot = entry;
        } else {
            self.packages.push(entry);
        }
    }

    /// Remove the stanza whose cpv equals `cpv`, returning whether one was
    /// removed.
    pub fn remove(&mut self, cpv: &str) -> bool {
        let before = self.packages.len();
        self.packages.retain(|p| p.cpv != cpv);
        self.packages.len() != before
    }
}

/// Build a `Packages` index from a set of present packages.
///
/// Each `(cpv, metadata)` pair becomes a stanza. The header carries the
/// supported VERSION. This regenerates the local index from scratch so adding or
/// removing a package keeps the index consistent.
pub fn build_local_index(
    packages: impl IntoIterator<Item = (String, MetadataMap)>,
) -> PackagesIndex {
    let mut index = PackagesIndex::new();
    for (cpv, metadata) in packages {
        index.packages.push(PackageEntry { cpv, metadata });
    }
    index
}

/// Reduce a USE-conditional dependency string against `enabled`.
///
/// Parses the string, evaluates its USE conditionals against the enabled set,
/// and re-renders the flattened result. On a parse failure the original string
/// is returned unchanged so a malformed entry is not silently dropped.
fn reduce_use(
    raw: &str,
    enabled: &std::collections::HashSet<moraine_common::Symbol>,
    interner: &Interner,
) -> String {
    let Ok(spec) = DepSpec::parse(raw, PERMISSIVE, interner) else {
        return raw.to_string();
    };
    let reduced = spec.evaluate(enabled);
    render_depspec(&reduced, interner)
}

/// Render a flattened [`DepSpec`] back to a space-separated atom string.
fn render_depspec(spec: &DepSpec, interner: &Interner) -> String {
    let mut atoms = Vec::new();
    for atom in spec.atoms() {
        atoms.push(atom.render(interner));
    }
    atoms.join(" ")
}

/// Translate a canonical key name to its `Packages` index name.
fn to_index_key(key: &str) -> &str {
    match key {
        KEY_DESCRIPTION => "DESC",
        KEY_MTIME => "MTIME",
        KEY_REPOSITORY => "REPO",
        other => other,
    }
}

/// Translate a `Packages` index key name to its canonical name.
fn from_index_key(key: &str) -> &str {
    match key {
        "DESC" => KEY_DESCRIPTION,
        "MTIME" => KEY_MTIME,
        "REPO" => KEY_REPOSITORY,
        other => other,
    }
}

fn split_blocks(text: &str) -> Vec<String> {
    text.split("\n\n")
        .map(|b| b.trim_matches('\n').to_string())
        .filter(|b| !b.trim().is_empty())
        .collect()
}

fn block_is_header(block: &str) -> bool {
    !block.lines().any(|l| l.starts_with("CPV:"))
}

fn parse_kv_block(block: &str) -> Result<BTreeMap<String, String>, IndexError> {
    let mut map = BTreeMap::new();
    for line in block.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(": ")
            .or_else(|| line.split_once(':'))
            .ok_or_else(|| IndexError::MalformedLine(line.to_string()))?;
        map.insert(key.trim().to_string(), value.trim().to_string());
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::KEY_USE;

    fn sample_index() -> PackagesIndex {
        let mut index = PackagesIndex::new();
        index.header.insert("ARCH".into(), "amd64".into());
        let mut meta = MetadataMap::new();
        meta.set_str(KEY_DESCRIPTION, "An example package");
        meta.set_str(KEY_REPOSITORY, "gentoo");
        meta.set_str(KEY_MTIME, "1700000000");
        meta.set_str(KEY_USE, "ssl");
        meta.set_str("SLOT", "0");
        meta.set_str("BUILD_ID", "1");
        meta.set_str("RDEPEND", "ssl? ( dev-libs/openssl ) sys-libs/zlib");
        index.packages.push(PackageEntry {
            cpv: "dev-libs/foo-1.2.3".into(),
            metadata: meta,
        });
        index
    }

    #[test]
    fn translations_applied_on_emit() {
        let interner = Interner::new();
        let text = sample_index().emit(&interner);
        assert!(text.contains("DESC: An example package"));
        assert!(text.contains("REPO: gentoo"));
        assert!(text.contains("MTIME: 1700000000"));
        assert!(!text.contains("DESCRIPTION:"));
        assert!(!text.contains("repository:"));
    }

    #[test]
    fn use_evaluation_reduces_conditionals() {
        let interner = Interner::new();
        let text = sample_index().emit(&interner);
        // ssl is enabled, so openssl is kept; no `ssl? (` remains.
        let rdepend = text.lines().find(|l| l.starts_with("RDEPEND:")).unwrap();
        assert!(rdepend.contains("dev-libs/openssl"));
        assert!(rdepend.contains("sys-libs/zlib"));
        assert!(!rdepend.contains("ssl?"));
        assert!(!rdepend.contains('('));
    }

    #[test]
    fn parse_round_trips_canonical_names() {
        let interner = Interner::new();
        let text = sample_index().emit(&interner);
        let parsed = PackagesIndex::parse(&text).unwrap();
        let pkg = &parsed.packages[0];
        assert_eq!(pkg.cpv, "dev-libs/foo-1.2.3");
        assert_eq!(
            pkg.metadata.get_str(KEY_DESCRIPTION).as_deref(),
            Some("An example package")
        );
        assert_eq!(
            pkg.metadata.get_str(KEY_REPOSITORY).as_deref(),
            Some("gentoo")
        );
        assert_eq!(
            pkg.metadata.get_str(KEY_MTIME).as_deref(),
            Some("1700000000")
        );
    }

    #[test]
    fn unsupported_version_rejected() {
        let text = format!("VERSION: {}\n\n", SUPPORTED_VERSION + 1);
        assert!(matches!(
            PackagesIndex::parse(&text),
            Err(IndexError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn local_index_reflects_present_packages() {
        let mut m = MetadataMap::new();
        m.set_str("SLOT", "0");
        let mut index = build_local_index([("a/b-1".to_string(), m)]);
        assert_eq!(index.packages.len(), 1);
        assert!(index.remove("a/b-1"));
        assert_eq!(index.packages.len(), 0);
        assert!(!index.remove("a/b-1"));
    }

    #[test]
    fn download_base_resolves_from_stanza_or_header() {
        let mut header = BTreeMap::new();
        header.insert("URI".to_string(), "https://binhost/packages".to_string());
        let mut entry = PackageEntry {
            cpv: "a/b-1".into(),
            metadata: MetadataMap::new(),
        };
        assert_eq!(
            entry.download_base(&header).as_deref(),
            Some("https://binhost/packages")
        );
        entry.metadata.set_str("BASE_URI", "https://mirror/pkgs");
        assert_eq!(
            entry.download_base(&header).as_deref(),
            Some("https://mirror/pkgs")
        );
    }
}
