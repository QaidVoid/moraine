//! Shared in-memory test fixture: a `ResolveSource` built directly from dep
//! strings, with no on-disk repository, vdb, or config.

#![allow(dead_code)]

use std::collections::BTreeSet;

use moraine_atom::DepSpec;
use moraine_common::Interner;
use moraine_eapi::PERMISSIVE;
use moraine_resolve::depnode::DepNode;
use moraine_resolve::normalize::normalize_depspec;
use moraine_resolve::source::{InstalledMeta, PackageMeta, ResolveSource};
use moraine_version::Version;

/// A package to register in the fixture.
pub struct PkgSpec {
    pub cp: &'static str,
    pub version: &'static str,
    pub eapi: &'static str,
    pub slot: &'static str,
    pub subslot: Option<&'static str>,
    pub depend: &'static str,
    pub bdepend: &'static str,
    pub rdepend: &'static str,
    pub pdepend: &'static str,
    pub idepend: &'static str,
    pub required_use: &'static str,
    pub iuse: &'static [&'static str],
    /// USE flags enabled on this package (the resolved USE).
    pub use_enabled: &'static [&'static str],
    /// Whether the package is visible (passes mask/keywords).
    pub visible: bool,
}

impl Default for PkgSpec {
    fn default() -> Self {
        PkgSpec {
            cp: "",
            version: "1",
            eapi: "8",
            slot: "0",
            subslot: None,
            depend: "",
            bdepend: "",
            rdepend: "",
            pdepend: "",
            idepend: "",
            required_use: "",
            iuse: &[],
            use_enabled: &[],
            visible: true,
        }
    }
}

struct Entry {
    meta: PackageMeta,
    use_enabled: BTreeSet<String>,
    visible: bool,
    provided: bool,
}

/// An in-memory test source.
#[derive(Default)]
pub struct Fixture {
    interner: Interner,
    entries: Vec<Entry>,
    installed: Vec<InstalledMeta>,
    provided: Vec<(String, String)>,
}

fn parse_node(interner: &Interner, s: &str) -> DepNode {
    let spec = DepSpec::parse(s, PERMISSIVE, interner).expect("dep string parses");
    normalize_depspec(&spec, interner)
}

impl Fixture {
    pub fn new() -> Self {
        Fixture::default()
    }

    pub fn add(&mut self, spec: PkgSpec) -> &mut Self {
        let meta = PackageMeta {
            cp: spec.cp.to_owned(),
            version: Version::parse(spec.version).expect("version parses"),
            eapi: spec.eapi.to_owned(),
            slot: spec.slot.to_owned(),
            subslot: spec.subslot.map(|s| s.to_owned()),
            depend: parse_node(&self.interner, spec.depend),
            bdepend: parse_node(&self.interner, spec.bdepend),
            rdepend: parse_node(&self.interner, spec.rdepend),
            pdepend: parse_node(&self.interner, spec.pdepend),
            idepend: parse_node(&self.interner, spec.idepend),
            required_use: moraine_resolve::required_use::parse_required_use(spec.required_use),
            license: String::new(),
            iuse: spec.iuse.iter().map(|s| (*s).to_owned()).collect(),
        };
        self.entries.push(Entry {
            meta,
            use_enabled: spec.use_enabled.iter().map(|s| (*s).to_owned()).collect(),
            visible: spec.visible,
            provided: false,
        });
        self
    }

    pub fn add_installed(&mut self, inst: InstalledMeta) -> &mut Self {
        self.installed.push(inst);
        self
    }

    pub fn add_provided(&mut self, cp: &str, version: &str) -> &mut Self {
        self.provided.push((cp.to_owned(), version.to_owned()));
        self
    }
}

impl ResolveSource for Fixture {
    fn versions_of(&self, cp: &str) -> Vec<PackageMeta> {
        let mut out: Vec<PackageMeta> = self
            .entries
            .iter()
            .filter(|e| e.meta.cp == cp)
            .map(|e| e.meta.clone())
            .collect();
        out.sort_by(|a, b| a.version.cmp(&b.version));
        out
    }

    fn is_visible(&self, meta: &PackageMeta) -> bool {
        self.entries
            .iter()
            .find(|e| e.meta.cp == meta.cp && e.meta.version == meta.version)
            .map(|e| e.visible)
            .unwrap_or(false)
    }

    fn resolved_use(&self, meta: &PackageMeta) -> BTreeSet<String> {
        self.entries
            .iter()
            .find(|e| e.meta.cp == meta.cp && e.meta.version == meta.version)
            .map(|e| e.use_enabled.clone())
            .unwrap_or_default()
    }

    fn is_provided(&self, cp: &str, version: &Version) -> bool {
        self.provided
            .iter()
            .any(|(c, v)| c == cp && Version::parse(v).map(|pv| &pv == version).unwrap_or(false))
    }

    fn installed(&self, cp: &str) -> Vec<InstalledMeta> {
        self.installed
            .iter()
            .filter(|i| i.cp == cp)
            .cloned()
            .collect()
    }
}

/// Build an `InstalledMeta` tersely.
pub fn installed(
    cp: &str,
    version: &str,
    slot: &str,
    subslot: Option<&str>,
    bindings: &[(&str, &str, Option<&str>)],
) -> InstalledMeta {
    InstalledMeta {
        cp: cp.to_owned(),
        version: Version::parse(version).expect("version parses"),
        slot: slot.to_owned(),
        subslot: subslot.map(|s| s.to_owned()),
        use_enabled: BTreeSet::new(),
        slot_bindings: bindings
            .iter()
            .map(|(c, s, ss)| ((*c).to_owned(), (*s).to_owned(), ss.map(|x| x.to_owned())))
            .collect(),
    }
}
