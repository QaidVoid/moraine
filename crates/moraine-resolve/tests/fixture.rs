//! Shared in-memory test fixture: a `ResolveSource` built directly from dep
//! strings, with no on-disk repository, vdb, or config.

#![allow(dead_code)]

use std::collections::BTreeSet;

use moraine_atom::DepSpec;
use moraine_common::Interner;
use moraine_eapi::PERMISSIVE;
use moraine_resolve::depnode::DepNode;
use moraine_resolve::normalize::normalize_depspec;
use moraine_resolve::source::{
    AcceptChange, Acceptability, InstalledMeta, PackageMeta, ResolveSource,
};
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
    /// A `~arch` keyword that autounmask would have to accept. When set, the
    /// package is not visible and its acceptability is a keyword change.
    pub accept_keyword: Option<&'static str>,
    /// Licenses that autounmask would have to accept. When non-empty, the package
    /// is not visible and its acceptability is a license change.
    pub accept_licenses: &'static [&'static str],
    /// Flags pinned by `use.mask`/`use.force`, which USE autounmask must not
    /// propose to toggle.
    pub locked_use: &'static [&'static str],
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
            accept_keyword: None,
            accept_licenses: &[],
            locked_use: &[],
        }
    }
}

struct Entry {
    meta: PackageMeta,
    use_enabled: BTreeSet<String>,
    visible: bool,
    provided: bool,
    /// The keyword/license change autounmask must accept, when the package is
    /// soft-masked rather than hard-masked.
    accept: Option<AcceptChange>,
    /// Flags pinned by `use.mask`/`use.force`.
    locked_use: BTreeSet<String>,
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
        // A soft-masked package (a keyword or license change) is not visible, so
        // the visible passes skip it and autounmask admits it instead.
        let accept = if spec.accept_keyword.is_some() || !spec.accept_licenses.is_empty() {
            Some(AcceptChange {
                keyword: spec.accept_keyword.map(|s| s.to_owned()),
                licenses: spec
                    .accept_licenses
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect(),
                ..Default::default()
            })
        } else {
            None
        };
        let visible = spec.visible && accept.is_none();
        self.entries.push(Entry {
            meta,
            use_enabled: spec.use_enabled.iter().map(|s| (*s).to_owned()).collect(),
            visible,
            provided: false,
            accept,
            locked_use: spec.locked_use.iter().map(|s| (*s).to_owned()).collect(),
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

    fn locked_use(&self, meta: &PackageMeta) -> BTreeSet<String> {
        self.entries
            .iter()
            .find(|e| e.meta.cp == meta.cp && e.meta.version == meta.version)
            .map(|e| e.locked_use.clone())
            .unwrap_or_default()
    }

    fn acceptability(&self, meta: &PackageMeta) -> Acceptability {
        let Some(entry) = self
            .entries
            .iter()
            .find(|e| e.meta.cp == meta.cp && e.meta.version == meta.version)
        else {
            return Acceptability::HardMasked;
        };
        if entry.visible {
            Acceptability::Visible
        } else {
            match &entry.accept {
                Some(change) => Acceptability::NeedsAccept(change.clone()),
                None => Acceptability::HardMasked,
            }
        }
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

    fn installed_all(&self) -> Vec<InstalledMeta> {
        self.installed.clone()
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
        iuse: BTreeSet::new(),
        slot_bindings: bindings
            .iter()
            .map(|(c, s, ss)| ((*c).to_owned(), (*s).to_owned(), ss.map(|x| x.to_owned())))
            .collect(),
        recorded_deps: std::collections::BTreeMap::new(),
    }
}
