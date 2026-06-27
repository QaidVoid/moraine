//! Conversion between in-memory [`PackageRecord`]s and the on-disk wire form.
//!
//! Encoding interns the repeated tokens into a shared string table and rewrites
//! records to index it. Decoding builds fresh [`Symbol`]s in a target
//! [`Interner`] and parses the `*DEPEND` strings into ASTs. Symbols and ASTs are
//! never serialized.

use std::collections::HashMap;

use moraine_atom::DepSpec;
use moraine_common::{Interner, Symbol};

use crate::contents::{Contents, EntryKind};
use crate::error::VdbError;
use crate::record::{Depend, DependKind, EnvironmentRef, PackageRecord, Slot, Toolchain};
use crate::soname::{Provides, Requires, SonameEntry};
use crate::wire::{WireEntry, WireEntryKind, WireEnv, WireRecord};

/// Accumulates a string token table while encoding records.
#[derive(Default)]
pub(crate) struct TokenBuilder {
    lookup: HashMap<String, u32>,
    tokens: Vec<String>,
}

impl TokenBuilder {
    /// Intern a symbol resolved through `interner`, returning its table index.
    fn add_symbol(&mut self, sym: Symbol, interner: &Interner) -> u32 {
        let s = interner
            .resolve(sym)
            .map(|a| a.to_string())
            .unwrap_or_default();
        self.add_str(&s)
    }

    /// Intern a string, returning its table index.
    fn add_str(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.lookup.get(s) {
            return i;
        }
        let i = self.tokens.len() as u32;
        self.tokens.push(s.to_string());
        self.lookup.insert(s.to_string(), i);
        i
    }

    /// Consume the builder, yielding the finished token table.
    pub(crate) fn into_tokens(self) -> Vec<String> {
        self.tokens
    }
}

/// Encode an in-memory record into wire form, interning its tokens into `tb`.
pub(crate) fn encode_record(
    rec: &PackageRecord,
    interner: &Interner,
    tb: &mut TokenBuilder,
) -> WireRecord {
    let depends = DependKind::ALL.map(|kind| rec.depends.get(kind).map(|d| d.raw.clone()));

    let provides = rec
        .provides
        .entries
        .iter()
        .map(|e| {
            (
                tb.add_symbol(e.bucket, interner),
                tb.add_symbol(e.soname, interner),
            )
        })
        .collect();
    let requires = rec
        .requires
        .entries
        .iter()
        .map(|e| {
            (
                tb.add_symbol(e.bucket, interner),
                tb.add_symbol(e.soname, interner),
            )
        })
        .collect();

    let contents = rec
        .contents
        .iter()
        .map(|e| WireEntry {
            path: e.path,
            kind: match e.kind {
                EntryKind::Obj { md5, mtime } => WireEntryKind::Obj { md5, mtime },
                EntryKind::Sym { target, mtime } => WireEntryKind::Sym { target, mtime },
                EntryKind::Dir => WireEntryKind::Dir,
                EntryKind::Fif => WireEntryKind::Fif,
                EntryKind::Dev => WireEntryKind::Dev,
            },
        })
        .collect();

    WireRecord {
        category: tb.add_symbol(rec.category, interner),
        package: tb.add_symbol(rec.package, interner),
        version: rec.version.as_str().to_string(),
        eapi: rec.eapi.clone(),
        slot: tb.add_symbol(rec.slot.slot, interner),
        subslot: rec.slot.subslot.map(|s| tb.add_symbol(s, interner)),
        use_flags: rec
            .use_flags
            .iter()
            .map(|&s| tb.add_symbol(s, interner))
            .collect(),
        iuse: rec.iuse.clone(),
        depends,
        keywords: rec.keywords.clone(),
        license: rec.license.clone(),
        properties: rec.properties.clone(),
        restrict: rec.restrict.clone(),
        repository: rec.repository.map(|s| tb.add_symbol(s, interner)),
        defined_phases: rec.defined_phases.clone(),
        build_time: rec.build_time,
        build_id: rec.build_id,
        counter: rec.counter,
        chost: rec.chost.clone(),
        provides,
        requires,
        contents,
        environment: rec.environment.as_ref().map(|e| WireEnv {
            digest: e.digest.clone(),
            blob: e.blob.clone(),
        }),
        inherited: rec.inherited.clone(),
        features: rec.features.clone(),
        size: rec.size,
        needed: rec.needed.clone(),
        description: rec.description.clone(),
        homepage: rec.homepage.clone(),
        toolchain: [
            rec.toolchain.cbuild.clone(),
            rec.toolchain.cc.clone(),
            rec.toolchain.cflags.clone(),
            rec.toolchain.cxx.clone(),
            rec.toolchain.cxxflags.clone(),
            rec.toolchain.ctarget.clone(),
            rec.toolchain.asflags.clone(),
            rec.toolchain.ldflags.clone(),
        ],
        dbdir_mtime: rec.dbdir_mtime,
    }
}

/// Resolve a token index against `tokens`, erroring if out of range.
fn token(tokens: &[String], index: u32) -> Result<&str, VdbError> {
    tokens
        .get(index as usize)
        .map(String::as_str)
        .ok_or(VdbError::TokenOutOfRange {
            index,
            len: tokens.len(),
        })
}

/// Decode a wire record into an in-memory record, interning into `interner` and
/// parsing the `*DEPEND` strings into ASTs.
pub(crate) fn decode_record(
    wire: &WireRecord,
    tokens: &[String],
    interner: &Interner,
) -> Result<PackageRecord, VdbError> {
    let category = interner.intern(token(tokens, wire.category)?);
    let package = interner.intern(token(tokens, wire.package)?);
    let cpv = format!(
        "{}/{}-{}",
        token(tokens, wire.category)?,
        token(tokens, wire.package)?,
        wire.version
    );

    let version =
        moraine_version::Version::parse(&wire.version).map_err(|_| VdbError::VersionParse {
            version: wire.version.clone(),
            package: cpv.clone(),
        })?;

    let features = moraine_eapi::features_for(&wire.eapi);

    let slot = Slot {
        slot: interner.intern(token(tokens, wire.slot)?),
        subslot: match wire.subslot {
            Some(i) => Some(interner.intern(token(tokens, i)?)),
            None => None,
        },
    };

    let mut use_flags = Vec::with_capacity(wire.use_flags.len());
    for &i in &wire.use_flags {
        use_flags.push(interner.intern(token(tokens, i)?));
    }

    let mut depends = crate::record::DependSet::default();
    for (kind, raw) in DependKind::ALL.into_iter().zip(wire.depends.iter()) {
        if let Some(raw) = raw {
            let ast = DepSpec::parse(raw, features, interner).map_err(|e| VdbError::DepParse {
                field: kind.name(),
                package: cpv.clone(),
                reason: e.to_string(),
            })?;
            *depends.slot_mut(kind) = Some(Depend {
                raw: raw.clone(),
                ast,
            });
        }
    }

    let repository = match wire.repository {
        Some(i) => Some(interner.intern(token(tokens, i)?)),
        None => None,
    };

    let decode_sonames = |pairs: &[(u32, u32)]| -> Result<Vec<SonameEntry>, VdbError> {
        pairs
            .iter()
            .map(|&(b, s)| {
                Ok(SonameEntry {
                    bucket: interner.intern(token(tokens, b)?),
                    soname: interner.intern(token(tokens, s)?),
                })
            })
            .collect()
    };
    let provides = Provides {
        entries: decode_sonames(&wire.provides)?,
    };
    let requires = Requires {
        entries: decode_sonames(&wire.requires)?,
    };

    let mut map = std::collections::BTreeMap::new();
    for e in &wire.contents {
        let kind = match &e.kind {
            WireEntryKind::Obj { md5, mtime } => EntryKind::Obj {
                md5: md5.clone(),
                mtime: *mtime,
            },
            WireEntryKind::Sym { target, mtime } => EntryKind::Sym {
                target: target.clone(),
                mtime: *mtime,
            },
            WireEntryKind::Dir => EntryKind::Dir,
            WireEntryKind::Fif => EntryKind::Fif,
            WireEntryKind::Dev => EntryKind::Dev,
        };
        map.insert(e.path.clone(), kind);
    }
    let contents = Contents::from_map(map);

    let environment = wire.environment.as_ref().map(|e| EnvironmentRef {
        digest: e.digest.clone(),
        blob: e.blob.clone(),
    });

    Ok(PackageRecord {
        category,
        package,
        version,
        eapi: wire.eapi.clone(),
        slot,
        use_flags,
        iuse: wire.iuse.clone(),
        depends,
        keywords: wire.keywords.clone(),
        license: wire.license.clone(),
        description: wire.description.clone(),
        homepage: wire.homepage.clone(),
        properties: wire.properties.clone(),
        restrict: wire.restrict.clone(),
        repository,
        defined_phases: wire.defined_phases.clone(),
        build_time: wire.build_time,
        build_id: wire.build_id,
        counter: wire.counter,
        chost: wire.chost.clone(),
        provides,
        requires,
        contents,
        environment,
        inherited: wire.inherited.clone(),
        features: wire.features.clone(),
        size: wire.size,
        needed: wire.needed.clone(),
        toolchain: Toolchain {
            cbuild: wire.toolchain[0].clone(),
            cc: wire.toolchain[1].clone(),
            cflags: wire.toolchain[2].clone(),
            cxx: wire.toolchain[3].clone(),
            cxxflags: wire.toolchain[4].clone(),
            ctarget: wire.toolchain[5].clone(),
            asflags: wire.toolchain[6].clone(),
            ldflags: wire.toolchain[7].clone(),
        },
        dbdir_mtime: wire.dbdir_mtime,
    })
}
