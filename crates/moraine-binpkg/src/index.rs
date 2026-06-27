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
use crate::metadata::{KEY_CHOST, KEY_DESCRIPTION, KEY_MTIME, KEY_REPOSITORY, MetadataMap};

/// The newest `Packages` index version this crate understands.
///
/// Pinned to `0` to match Portage's `_pkgindex_version`
/// (`lib/portage/dbapi/bintree.py:561`), so a written index declares
/// `VERSION: 0` and a stock Portage client accepts it through
/// `_pkgindex_version_supported` rather than discarding it.
pub const SUPPORTED_VERSION: u32 = 0;

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
    "TIMESTAMP",
    "PACKAGES",
    "ARCH",
    "PROFILE",
    "URI",
    "TTL",
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
    /// Resolve the base download URI this package fetches against: the stanza
    /// `PKGINDEX_URI` or `BASE_URI`, else the header `URI`/`PKGINDEX_URI`, with any
    /// trailing slash trimmed. This is the `pkgindex.header.get("URI", base_url)`
    /// base Portage injects per stanza as `BASE_URI`. Returns `None` when no base
    /// URI is configured.
    pub fn download_base_uri(&self, header: &BTreeMap<String, String>) -> Option<String> {
        let base = self
            .metadata
            .get_str("PKGINDEX_URI")
            .or_else(|| self.metadata.get_str("BASE_URI"))
            .or_else(|| header.get("URI").cloned())
            .or_else(|| header.get("PKGINDEX_URI").cloned())?;
        Some(base.trim_end_matches('/').to_string())
    }

    /// Resolve this package's full download URL: the [`download_base_uri`] with
    /// the stanza `PATH` appended as `<base>/<PATH>`
    /// (`BASE_URI.rstrip("/") + "/" + PATH.lstrip("/")`), matching Portage's
    /// `BinpkgFetcher`. Returns `None` when no base URI is configured.
    ///
    /// [`download_base_uri`]: PackageEntry::download_base_uri
    pub fn download_base(&self, header: &BTreeMap<String, String>) -> Option<String> {
        let base = self.download_base_uri(header)?;
        match self.metadata.get_str("PATH") {
            Some(path) if !path.is_empty() => {
                Some(format!("{base}/{}", path.trim_start_matches('/')))
            }
            _ => Some(base),
        }
    }

    /// Record the container integrity keys a binhost client validates against:
    /// `MD5` and `SHA1` over the container bytes, `SIZE`, and the pkgdir-relative
    /// `PATH`, matching Portage's `_pkgindex_entry`.
    pub fn record_container(&mut self, container: &[u8], rel_path: &str) {
        self.metadata
            .set_str("MD5", moraine_common::hash::md5(container));
        self.metadata
            .set_str("SHA1", moraine_common::hash::sha1(container));
        self.metadata.set_str("SIZE", container.len().to_string());
        self.metadata.set_str("PATH", rel_path);
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
            inherit_header_keys(&mut metadata, &index.header);
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

    /// Populate the binhost header keys a client validates against: `TIMESTAMP`
    /// (index generation time), `PACKAGES` (the stanza count), `ARCH`, `PROFILE`,
    /// `URI` (the base download URI), and `TTL` (staleness window in seconds),
    /// matching `_update_pkgindex_header`. Empty optional values are skipped.
    pub fn populate_header(
        &mut self,
        timestamp: i64,
        arch: &str,
        profile: &str,
        uri: &str,
        ttl: u64,
    ) {
        let count = self.packages.len();
        self.header
            .insert("TIMESTAMP".to_string(), timestamp.to_string());
        self.header
            .insert("PACKAGES".to_string(), count.to_string());
        self.header.insert("TTL".to_string(), ttl.to_string());
        for (key, value) in [("ARCH", arch), ("PROFILE", profile), ("URI", uri)] {
            if !value.is_empty() {
                self.header.insert(key.to_string(), value.to_string());
            }
        }
    }

    /// Add or replace the stanza matching `entry`'s `(cpv, BUILD_ID)`.
    ///
    /// Only the stanza whose cpv and `BUILD_ID` both equal the new entry's is
    /// replaced; otherwise the entry is appended. This keys mutation by
    /// `(cpv, BUILD_ID)` like Portage's `bindbapi`, so several builds of one cpv
    /// coexist rather than the first stanza sharing the cpv being overwritten.
    pub fn upsert(&mut self, entry: PackageEntry) {
        let build_id = entry.metadata.get_str("BUILD_ID");
        if let Some(slot) = self
            .packages
            .iter_mut()
            .find(|p| p.cpv == entry.cpv && p.metadata.get_str("BUILD_ID") == build_id)
        {
            *slot = entry;
        } else {
            self.packages.push(entry);
        }
    }

    /// Remove every stanza whose cpv equals `cpv`, returning whether one was
    /// removed.
    pub fn remove(&mut self, cpv: &str) -> bool {
        let before = self.packages.len();
        self.packages.retain(|p| p.cpv != cpv);
        self.packages.len() != before
    }

    /// Remove only the stanza matching `(cpv, build_id)`, returning whether one
    /// was removed, so dropping one build of a multi-instance cpv leaves the
    /// other builds in the index.
    pub fn remove_build(&mut self, cpv: &str, build_id: &str) -> bool {
        let before = self.packages.len();
        self.packages.retain(|p| {
            !(p.cpv == cpv && p.metadata.get_str("BUILD_ID").as_deref() == Some(build_id))
        });
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
    reduced.render(interner)
}

/// Re-inherit the binhost header keys `CHOST` and `repository` into a stanza
/// that omits them, mirroring Portage's `_pkgindex_inherited_keys` applied in
/// `IndexStanzas` reading. A real binhost records `CHOST` once in the header
/// rather than in every stanza, so without this the parsed stanza carries no
/// `CHOST` and the foreign-CHOST compatibility check cannot fire. A stanza that
/// already records a key keeps its own value. The header may spell the
/// repository key as `repository` or its serialized `REPO`.
fn inherit_header_keys(metadata: &mut MetadataMap, header: &BTreeMap<String, String>) {
    if metadata.get_str(KEY_CHOST).is_none()
        && let Some(chost) = header.get(KEY_CHOST).filter(|v| !v.is_empty())
    {
        metadata.set_str(KEY_CHOST, chost);
    }
    if metadata.get_str(KEY_REPOSITORY).is_none()
        && let Some(repo) = header
            .get(KEY_REPOSITORY)
            .or_else(|| header.get("REPO"))
            .filter(|v| !v.is_empty())
    {
        metadata.set_str(KEY_REPOSITORY, repo);
    }
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
        // Skip a line with no `key: value` separator rather than failing the
        // whole index: a truncated or partially-written cache (a clipped final
        // line) must not discard every package, matching Portage's tolerance.
        let Some((key, value)) = line.split_once(": ").or_else(|| line.split_once(':')) else {
            tracing::debug!(line, "skipping malformed index line");
            continue;
        };
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
    fn container_digests_header_and_download_path() {
        let mut index = sample_index();
        let container = b"the binary package bytes";
        index.packages[0].record_container(container, "dev-libs/foo-1.2.3.gpkg.tar");
        index.populate_header(
            1_700_000_500,
            "amd64",
            "default/linux",
            "https://binhost/p",
            3600,
        );

        let interner = Interner::new();
        let text = index.emit(&interner);
        assert!(text.contains(&format!("MD5: {}", moraine_common::hash::md5(container))));
        assert!(text.contains(&format!("SHA1: {}", moraine_common::hash::sha1(container))));
        assert!(text.contains(&format!("SIZE: {}", container.len())));
        assert!(text.contains("PATH: dev-libs/foo-1.2.3.gpkg.tar"));
        // Header keys a client validates against.
        assert!(text.contains("TIMESTAMP: 1700000500"));
        assert!(text.contains("PACKAGES: 1"));
        assert!(text.contains("TTL: 3600"));
        assert!(text.contains("PROFILE: default/linux"));

        // download_base appends PATH to the base URI.
        let url = index.packages[0].download_base(&index.header).unwrap();
        assert_eq!(url, "https://binhost/p/dev-libs/foo-1.2.3.gpkg.tar");
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
    fn index_version_is_zero() {
        // Portage's `_pkgindex_version` is 0, so the header must declare
        // VERSION: 0 for a stock client to accept the index.
        assert_eq!(SUPPORTED_VERSION, 0);
        let interner = Interner::new();
        let index = sample_index();
        assert_eq!(index.header.get("VERSION").map(String::as_str), Some("0"));
        let text = index.emit(&interner);
        assert!(text.contains("VERSION: 0"), "emitted header: {text}");
        // An index declaring VERSION 0 parses and retains its stanzas.
        let parsed = PackagesIndex::parse(&text).unwrap();
        assert_eq!(parsed.header.get("VERSION").map(String::as_str), Some("0"));
        assert_eq!(parsed.packages.len(), 1);
    }

    #[test]
    fn unsupported_version_rejected() {
        // SUPPORTED_VERSION is now 0, so this rejects the off-by-one VERSION: 1.
        let text = format!("VERSION: {}\n\n", SUPPORTED_VERSION + 1);
        assert!(matches!(
            PackagesIndex::parse(&text),
            Err(IndexError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn any_of_group_preserved_on_emit() {
        let interner = Interner::new();
        let mut index = PackagesIndex::new();
        let mut meta = MetadataMap::new();
        meta.set_str("SLOT", "0");
        meta.set_str("RDEPEND", "|| ( dev-libs/a dev-libs/b )");
        meta.set_str("LICENSE", "|| ( GPL-2 BSD )");
        index.packages.push(PackageEntry {
            cpv: "dev-libs/foo-1".into(),
            metadata: meta,
        });
        let text = index.emit(&interner);
        let rdepend = text.lines().find(|l| l.starts_with("RDEPEND:")).unwrap();
        assert!(
            rdepend.contains("|| ( dev-libs/a dev-libs/b )"),
            "any-of preserved: {rdepend}"
        );
        let license = text.lines().find(|l| l.starts_with("LICENSE:")).unwrap();
        assert!(
            license.contains("|| ( GPL-2 BSD )"),
            "license any-of preserved: {license}"
        );
    }

    #[test]
    fn multi_instance_upsert_and_targeted_remove() {
        let entry = |build_id: &str, desc: &str| {
            let mut meta = MetadataMap::new();
            meta.set_str("BUILD_ID", build_id);
            meta.set_str(KEY_DESCRIPTION, desc);
            PackageEntry {
                cpv: "dev-libs/foo-1".into(),
                metadata: meta,
            }
        };
        let mut index = PackagesIndex::new();
        index.upsert(entry("1", "first"));
        index.upsert(entry("2", "second"));
        assert_eq!(index.packages.len(), 2);

        // Upsert build 1 again: only that stanza is replaced.
        index.upsert(entry("1", "first-rebuilt"));
        assert_eq!(index.packages.len(), 2);
        let b1 = index
            .packages
            .iter()
            .find(|p| p.metadata.get_str("BUILD_ID").as_deref() == Some("1"))
            .unwrap();
        assert_eq!(
            b1.metadata.get_str(KEY_DESCRIPTION).as_deref(),
            Some("first-rebuilt")
        );

        // Targeted removal drops only build 1.
        assert!(index.remove_build("dev-libs/foo-1", "1"));
        assert_eq!(index.packages.len(), 1);
        assert_eq!(
            index.packages[0].metadata.get_str("BUILD_ID").as_deref(),
            Some("2")
        );
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
    fn stanza_inherits_header_chost_and_repository() {
        use crate::metadata::KEY_CHOST;

        let text = "VERSION: 0\nCHOST: x86_64-pc-linux-gnu\nrepository: gentoo\n\n\
             CPV: dev-libs/foo-1\nSLOT: 0\n\n\
             CPV: dev-libs/bar-1\nSLOT: 0\nCHOST: i686-pc-linux-gnu\n";
        let parsed = PackagesIndex::parse(text).unwrap();

        // The stanza that omits CHOST/repository inherits the header values.
        let foo = parsed
            .packages
            .iter()
            .find(|p| p.cpv == "dev-libs/foo-1")
            .unwrap();
        assert_eq!(
            foo.metadata.get_str(KEY_CHOST).as_deref(),
            Some("x86_64-pc-linux-gnu")
        );
        assert_eq!(
            foo.metadata.get_str(KEY_REPOSITORY).as_deref(),
            Some("gentoo")
        );

        // The stanza with its own CHOST keeps it rather than the header value.
        let bar = parsed
            .packages
            .iter()
            .find(|p| p.cpv == "dev-libs/bar-1")
            .unwrap();
        assert_eq!(
            bar.metadata.get_str(KEY_CHOST).as_deref(),
            Some("i686-pc-linux-gnu")
        );
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
