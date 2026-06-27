//! Importer from a stock `/var/db/pkg` tree.
//!
//! The importer walks each `<category>/<P-V>` directory, maps one-line aux key
//! files to record fields, parses `CONTENTS`, `NEEDED.ELF.2`, and `COUNTER`, and
//! carries `environment.bz2` as the saved build-environment reference. Fields
//! absent as their own file are recovered from the saved environment. A required
//! field present nowhere surfaces as a typed diagnostic. The walk runs in
//! parallel with `rayon` and never modifies the stock tree.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use moraine_common::Interner;
use rayon::prelude::*;

use crate::contents::{Contents, Entry, EntryKind};
use crate::error::{IoResultExt as _, VdbError};
use crate::record::{Depend, DependKind, EnvironmentRef, PackageRecord, Slot};
use crate::soname::{Provides, Requires, SonameEntry};

/// Import every package under a stock `/var/db/pkg` tree into records.
///
/// `interner` receives every interned token so the resulting records share a
/// single table. The stock tree is read only.
pub fn import_vdb(
    vdb_root: impl AsRef<Path>,
    interner: &Interner,
) -> Result<Vec<PackageRecord>, VdbError> {
    let vdb_root = vdb_root.as_ref();
    let span = tracing::info_span!("vdb.import", root = %vdb_root.display());
    let _enter = span.enter();

    let pkg_dirs = collect_package_dirs(vdb_root)?;
    tracing::info!(count = pkg_dirs.len(), "discovered package directories");

    let records: Vec<PackageRecord> = pkg_dirs
        .par_iter()
        .map(|dir| import_package_dir(dir, interner))
        .collect::<Result<_, _>>()?;

    tracing::info!(count = records.len(), "import complete");
    Ok(records)
}

/// List every `<category>/<P-V>` directory under `vdb_root`, skipping dotfiles.
fn collect_package_dirs(vdb_root: &Path) -> Result<Vec<PathBuf>, VdbError> {
    let mut dirs = Vec::new();
    let categories = std::fs::read_dir(vdb_root).with_path(vdb_root)?;
    for cat in categories {
        let cat = cat.with_path(vdb_root)?;
        if !cat.file_type().with_path(cat.path())?.is_dir() {
            continue;
        }
        let cat_path = cat.path();
        let name = cat.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        for pkg in std::fs::read_dir(&cat_path).with_path(&cat_path)? {
            let pkg = pkg.with_path(&cat_path)?;
            if !pkg.file_type().with_path(pkg.path())?.is_dir() {
                continue;
            }
            if pkg.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            dirs.push(pkg.path());
        }
    }
    Ok(dirs)
}

/// Import one `<category>/<P-V>` directory into a record, interning into
/// `interner`. Used both by the bulk import and by the per-package cache
/// revalidation that re-imports a single dbdir whose mtime changed.
pub fn import_package_dir(dir: &Path, interner: &Interner) -> Result<PackageRecord, VdbError> {
    let category = dir
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let pv = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let (pkg_name, version_str) = split_pv(&pv).ok_or_else(|| VdbError::BadPackageDir {
        name: format!("{category}/{pv}"),
    })?;
    let cpv = format!("{category}/{pv}");

    let version =
        moraine_version::Version::parse(&version_str).map_err(|_| VdbError::VersionParse {
            version: version_str.clone(),
            package: cpv.clone(),
        })?;

    // Read environment.bz2 once. It is the fallback source for any aux key not
    // present as its own one-line file.
    let env_path = dir.join("environment.bz2");
    let env_blob = if env_path.exists() {
        Some(std::fs::read(&env_path).with_path(&env_path)?)
    } else {
        None
    };
    let env_vars: HashMap<String, String> = match &env_blob {
        Some(blob) => parse_environment(blob, &cpv)?,
        None => HashMap::new(),
    };

    // Read a one-line file, falling back to the saved environment.
    let read_aux = |key: &str| -> Result<Option<String>, VdbError> {
        if let Some(v) = read_line_file(dir, key)? {
            return Ok(Some(v));
        }
        Ok(env_vars.get(key).cloned())
    };

    // Required fields: must exist as a file or in the environment.
    let eapi = read_aux("EAPI")?.unwrap_or_else(|| "0".to_string());
    let slot_raw = read_aux("SLOT")?.ok_or_else(|| VdbError::MissingField {
        field: "SLOT",
        package: cpv.clone(),
    })?;

    let slot = parse_slot(&slot_raw, interner);
    let features = moraine_eapi::features_for(&eapi);

    let use_flags = read_aux("USE")?
        .map(|s| split_ws_intern(&s, interner))
        .unwrap_or_default();

    let mut depends = crate::record::DependSet::default();
    for kind in DependKind::ALL {
        let raw = read_aux(kind.name())?;
        if let Some(raw) = raw {
            let raw = raw.trim().to_string();
            if raw.is_empty() {
                continue;
            }
            let ast = moraine_atom::DepSpec::parse(&raw, features, interner).map_err(|e| {
                VdbError::DepParse {
                    field: kind.name(),
                    package: cpv.clone(),
                    reason: e.to_string(),
                }
            })?;
            *depends.slot_mut(kind) = Some(Depend { raw, ast });
        }
    }

    let (provides, requires) = read_soname_linkage(dir, interner)?;
    let contents = parse_contents(dir, &cpv)?;

    let environment = env_blob.map(|blob| EnvironmentRef {
        digest: moraine_common::hash::blake3(&blob),
        blob,
    });

    Ok(PackageRecord {
        category: interner.intern(&category),
        package: interner.intern(&pkg_name),
        version,
        eapi,
        slot,
        use_flags,
        iuse: read_aux("IUSE")?.map(|s| split_ws(&s)).unwrap_or_default(),
        depends,
        keywords: read_aux("KEYWORDS")?
            .map(|s| split_ws(&s))
            .unwrap_or_default(),
        license: read_aux("LICENSE")?.unwrap_or_default(),
        description: read_aux("DESCRIPTION")?.unwrap_or_default(),
        homepage: read_aux("HOMEPAGE")?.unwrap_or_default(),
        properties: read_aux("PROPERTIES")?.unwrap_or_default(),
        restrict: read_aux("RESTRICT")?.unwrap_or_default(),
        repository: read_line_file(dir, "repository")?
            .or(read_line_file(dir, "REPOSITORY")?)
            .map(|s| interner.intern(s.trim())),
        defined_phases: read_aux("DEFINED_PHASES")?
            .map(|s| split_ws(&s))
            .unwrap_or_default(),
        build_time: read_aux("BUILD_TIME")?.and_then(|s| s.trim().parse().ok()),
        build_id: read_line_file(dir, "BUILD_ID")?.and_then(|s| s.trim().parse().ok()),
        counter: read_line_file(dir, "COUNTER")?
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0),
        chost: read_aux("CHOST")?.unwrap_or_default(),
        provides,
        requires,
        contents,
        environment,
        inherited: read_aux("INHERITED")?
            .map(|s| split_ws(&s))
            .unwrap_or_default(),
        features: read_aux("FEATURES")?
            .map(|s| split_ws(&s))
            .unwrap_or_default(),
        size: read_line_file(dir, "SIZE")?.and_then(|s| s.trim().parse().ok()),
        needed: read_needed_lines(dir)?,
        toolchain: crate::record::Toolchain {
            cbuild: read_aux("CBUILD")?.unwrap_or_default(),
            cc: read_aux("CC")?.unwrap_or_default(),
            cflags: read_aux("CFLAGS")?.unwrap_or_default(),
            cxx: read_aux("CXX")?.unwrap_or_default(),
            cxxflags: read_aux("CXXFLAGS")?.unwrap_or_default(),
            ctarget: read_aux("CTARGET")?.unwrap_or_default(),
            asflags: read_aux("ASFLAGS")?.unwrap_or_default(),
            ldflags: read_aux("LDFLAGS")?.unwrap_or_default(),
        },
        dbdir_mtime: crate::vardb::dbdir_mtime(dir),
    })
}

/// Read the verbatim `NEEDED.ELF.2` lines, returning an empty list when the file
/// is absent.
fn read_needed_lines(dir: &Path) -> Result<Vec<String>, VdbError> {
    let path = dir.join("NEEDED.ELF.2");
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(s
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(VdbError::Io { path, source }),
    }
}

/// Read a one-line aux file, returning its trimmed contents if it exists and is
/// non-empty.
fn read_line_file(dir: &Path, key: &str) -> Result<Option<String>, VdbError> {
    let path = dir.join(key);
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(VdbError::Io { path, source: e }),
    }
}

/// Parse `SLOT` (`slot` or `slot/subslot`) into a [`Slot`].
fn parse_slot(raw: &str, interner: &Interner) -> Slot {
    let raw = raw.trim();
    match raw.split_once('/') {
        Some((slot, subslot)) => Slot {
            slot: interner.intern(slot),
            subslot: Some(interner.intern(subslot)),
        },
        None => Slot {
            slot: interner.intern(raw),
            subslot: None,
        },
    }
}

/// Split `P-V` into `(package, version)` by locating the version component,
/// which is the last `-`-separated token that parses as a version.
pub(crate) fn split_pv(pv: &str) -> Option<(String, String)> {
    let mut search_end = pv.len();
    while let Some(idx) = pv[..search_end].rfind('-') {
        let candidate = &pv[idx + 1..];
        if moraine_version::Version::parse(candidate).is_ok() {
            let name = &pv[..idx];
            if !name.is_empty() {
                return Some((name.to_string(), candidate.to_string()));
            }
        }
        search_end = idx;
    }
    None
}

/// Split whitespace-separated tokens into owned strings.
fn split_ws(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

/// Split whitespace-separated tokens and intern each.
fn split_ws_intern(s: &str, interner: &Interner) -> Vec<moraine_common::Symbol> {
    s.split_whitespace().map(|t| interner.intern(t)).collect()
}

/// Parse a package's `CONTENTS` file into a [`Contents`] model.
fn parse_contents(dir: &Path, cpv: &str) -> Result<Contents, VdbError> {
    let path = dir.join("CONTENTS");
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Contents::default()),
        Err(e) => return Err(VdbError::Io { path, source: e }),
    };

    let mut entries = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let entry = parse_contents_line(line).ok_or_else(|| VdbError::BadContents {
            package: cpv.to_string(),
            line: line.to_string(),
        })?;
        entries.push(entry);
    }
    Ok(Contents::from_entries(entries))
}

/// Parse a single `CONTENTS` line.
///
/// Recognized forms:
/// - `dir <path>`
/// - `obj <path> <md5> <mtime>`
/// - `sym <path> -> <target> <mtime>`
/// - the legacy old-symlink form `sym <path> -> <target> <mtime> <something>`
/// - `fif <path>` (named pipe)
/// - `dev <path>` (device node)
fn parse_contents_line(line: &str) -> Option<Entry> {
    let (kind, rest) = line.split_once(' ')?;
    match kind {
        "dir" => Some(Entry {
            path: rest.trim().to_string(),
            kind: EntryKind::Dir,
        }),
        "fif" => Some(Entry {
            path: rest.trim().to_string(),
            kind: EntryKind::Fif,
        }),
        "dev" => Some(Entry {
            path: rest.trim().to_string(),
            kind: EntryKind::Dev,
        }),
        "obj" => {
            // `<path> <md5> <mtime>`: split the trailing md5 and mtime from the
            // right so paths with spaces survive.
            let tokens: Vec<&str> = rest.rsplitn(3, ' ').collect();
            if tokens.len() < 3 {
                return None;
            }
            let mtime = tokens[0].trim().parse().ok()?;
            let md5 = tokens[1].to_string();
            let path = tokens[2].to_string();
            Some(Entry {
                path,
                kind: EntryKind::Obj { md5, mtime },
            })
        }
        "sym" => {
            // `<path> -> <target> <mtime>` with an optional trailing legacy field.
            let arrow = rest.find(" -> ")?;
            let path = rest[..arrow].to_string();
            let after = &rest[arrow + 4..];
            let tokens: Vec<&str> = after.rsplitn(2, ' ').collect();
            if tokens.len() < 2 {
                return None;
            }
            // The last token is the mtime; if it is not numeric this is a legacy
            // old-symlink line and the real mtime sits one field earlier.
            let (target, mtime) = if let Ok(m) = tokens[0].trim().parse::<i64>() {
                (tokens[1].to_string(), m)
            } else {
                let inner: Vec<&str> = tokens[1].rsplitn(2, ' ').collect();
                if inner.len() < 2 {
                    return None;
                }
                let m = inner[0].trim().parse().ok()?;
                (inner[1].to_string(), m)
            };
            Some(Entry {
                path,
                kind: EntryKind::Sym { target, mtime },
            })
        }
        _ => None,
    }
}

/// Read a package's soname linkage, preferring the authoritative `PROVIDES` and
/// `REQUIRES` files Portage writes over a recompute from `NEEDED.ELF.2`.
///
/// When a `PROVIDES` or `REQUIRES` file is present it is recorded verbatim, since
/// it already carries the multilib-category buckets and the `PROVIDES_EXCLUDE`/
/// `REQUIRES_EXCLUDE` filtering moraine cannot reconstruct from `NEEDED.ELF.2`
/// (`doebuild.py:3270-3363`). When a file is absent the side is recomputed from
/// `NEEDED.ELF.2`; the recomputed `REQUIRES` drops any soname the package itself
/// provides in the same bucket, mirroring `SonameDepsProcessor._intersect`.
fn read_soname_linkage(dir: &Path, interner: &Interner) -> Result<(Provides, Requires), VdbError> {
    // The NEEDED-derived provides are the fallback for an absent `PROVIDES` file
    // and the self-provided set the recomputed `REQUIRES` intersects against.
    let needed_provides = parse_needed_provides(dir, interner)?;

    let provides = match read_soname_file(dir, "PROVIDES", interner)? {
        Some(entries) => Provides { entries },
        None => needed_provides.clone(),
    };
    let requires = match read_soname_file(dir, "REQUIRES", interner)? {
        Some(entries) => Requires { entries },
        None => {
            let recomputed = parse_needed_requires(dir, interner)?;
            Requires {
                entries: recomputed
                    .entries
                    .into_iter()
                    .filter(|e| !needed_provides.provides_in(e.bucket, e.soname))
                    .collect(),
            }
        }
    };
    Ok((provides, requires))
}

/// Read an authoritative `PROVIDES`/`REQUIRES` file into `(bucket, soname)`
/// entries, returning `None` when the file is absent. The format is
/// `bucket: soname soname` groups, the inverse of `render_sonames`, matching the
/// shape `moraine_binpkg::resolution::parse_sonames` reads.
fn read_soname_file(
    dir: &Path,
    name: &str,
    interner: &Interner,
) -> Result<Option<Vec<SonameEntry>>, VdbError> {
    let path = dir.join(name);
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(VdbError::Io { path, source }),
    };
    let mut entries = Vec::new();
    let mut bucket = "";
    for token in text.split_whitespace() {
        if let Some(label) = token.strip_suffix(':') {
            bucket = label;
        } else {
            entries.push(SonameEntry {
                bucket: interner.intern(bucket),
                soname: interner.intern(token),
            });
        }
    }
    Ok(Some(entries))
}

/// Parse `NEEDED.ELF.2` into the provided sonames, bucketed by multilib category.
fn parse_needed_provides(dir: &Path, interner: &Interner) -> Result<Provides, VdbError> {
    let mut entries = Vec::new();
    for fields in read_needed(dir)? {
        if let Some(soname) = fields.soname {
            entries.push(SonameEntry {
                bucket: interner.intern(&fields.bucket),
                soname: interner.intern(&soname),
            });
        }
    }
    Ok(Provides { entries })
}

/// Parse `NEEDED.ELF.2` into the required sonames, bucketed by multilib category.
fn parse_needed_requires(dir: &Path, interner: &Interner) -> Result<Requires, VdbError> {
    let mut entries = Vec::new();
    for fields in read_needed(dir)? {
        let bucket = interner.intern(&fields.bucket);
        for needed in fields.needed {
            entries.push(SonameEntry {
                bucket,
                soname: interner.intern(&needed),
            });
        }
    }
    Ok(Requires { entries })
}

/// One parsed `NEEDED.ELF.2` line.
struct Needed {
    /// The multilib category bucket: the sixth field when present, else the
    /// first field (the ELF arch) as a fallback, mirroring
    /// `NeededEntry.multilib_category`.
    bucket: String,
    soname: Option<String>,
    needed: Vec<String>,
}

/// Read and parse every `NEEDED.ELF.2` line, returning an empty list when the
/// file is absent.
fn read_needed(dir: &Path) -> Result<Vec<Needed>, VdbError> {
    let path = dir.join("NEEDED.ELF.2");
    let cpv = dir
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(VdbError::Io { path, source: e }),
    };

    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // `arch;path;soname;rpath;needed-csv;multilib-category`. The sixth field
        // is the authoritative multilib-category bucket; field 0 (the ELF arch)
        // is the fallback only when it is absent or empty, so Portage's six-field
        // lines and moraine's legacy five-field lines both resolve to the bucket.
        let fields: Vec<&str> = line.split(';').collect();
        if fields.len() < 5 {
            return Err(VdbError::BadNeeded {
                package: cpv.clone(),
                line: line.to_string(),
            });
        }
        let bucket = fields
            .get(5)
            .filter(|f| !f.is_empty())
            .copied()
            .unwrap_or(fields[0])
            .to_string();
        let soname = if fields[2].is_empty() {
            None
        } else {
            Some(fields[2].to_string())
        };
        let needed = fields[4]
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        out.push(Needed {
            bucket,
            soname,
            needed,
        });
    }
    Ok(out)
}

/// Decompress and parse `environment.bz2` into a `KEY=value` map.
///
/// Only simple `KEY=value` and `KEY="value"` assignments at line starts are
/// extracted, which covers the aux keys this crate may need to recover. Shell
/// function bodies and multi-line values are ignored.
fn parse_environment(blob: &[u8], cpv: &str) -> Result<HashMap<String, String>, VdbError> {
    let mut decoder = bzip2::read::BzDecoder::new(blob);
    let mut text = String::new();
    decoder
        .read_to_string(&mut text)
        .map_err(|source| VdbError::Environment {
            package: cpv.to_string(),
            source,
        })?;

    let mut map = HashMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        if !value.is_empty() {
            map.insert(key.to_string(), value.to_string());
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_pv_with_hyphenated_name() {
        let (name, ver) = split_pv("gtk+-3.24.41-r1").unwrap();
        assert_eq!(name, "gtk+");
        assert_eq!(ver, "3.24.41-r1");
    }

    #[test]
    fn splits_pv_simple() {
        let (name, ver) = split_pv("foo-1.2.3").unwrap();
        assert_eq!(name, "foo");
        assert_eq!(ver, "1.2.3");
    }

    #[test]
    fn parses_obj_line() {
        let e = parse_contents_line("obj /usr/bin/foo d41d8cd98f00b204e9800998ecf8427e 1700000000")
            .unwrap();
        assert_eq!(e.path, "/usr/bin/foo");
        assert!(matches!(e.kind, EntryKind::Obj { .. }));
    }

    #[test]
    fn parses_sym_line() {
        let e = parse_contents_line("sym /usr/lib/libfoo.so -> libfoo.so.1 1700000000").unwrap();
        assert_eq!(e.path, "/usr/lib/libfoo.so");
        match e.kind {
            EntryKind::Sym { target, .. } => assert_eq!(target, "libfoo.so.1"),
            _ => panic!("expected sym"),
        }
    }

    #[test]
    fn parses_legacy_sym_line() {
        let e = parse_contents_line("sym /a/b -> target 1700000000 extra").unwrap();
        match e.kind {
            EntryKind::Sym { target, mtime } => {
                assert_eq!(target, "target");
                assert_eq!(mtime, 1700000000);
            }
            _ => panic!("expected sym"),
        }
    }

    #[test]
    fn parses_dir_line() {
        let e = parse_contents_line("dir /usr/bin").unwrap();
        assert!(matches!(e.kind, EntryKind::Dir));
    }

    #[test]
    fn buckets_by_multilib_category_field() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("app-misc").join("foo-1");
        std::fs::create_dir_all(&pkg).unwrap();
        // A Portage six-field line: ELF arch `X86_64`, multilib category `x86_64`.
        std::fs::write(
            pkg.join("NEEDED.ELF.2"),
            "X86_64;/usr/lib64/libfoo.so.1;libfoo.so.1;;libc.so.6;x86_64\n",
        )
        .unwrap();

        let interner = Interner::new();
        let provides = parse_needed_provides(&pkg, &interner).unwrap();
        let x86_64 = interner.intern("x86_64");
        let big_x86_64 = interner.intern("X86_64");
        let libfoo = interner.intern("libfoo.so.1");
        // The soname buckets under the multilib category, not the ELF arch.
        assert!(provides.provides_in(x86_64, libfoo));
        assert!(!provides.provides_in(big_x86_64, libfoo));
    }

    #[test]
    fn legacy_five_field_line_buckets_by_field_zero() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("app-misc").join("bar-1");
        std::fs::create_dir_all(&pkg).unwrap();
        // A moraine legacy five-field line carries the multilib token in field 0.
        std::fs::write(
            pkg.join("NEEDED.ELF.2"),
            "x86_64;/usr/lib64/libbar.so.1;libbar.so.1;;libc.so.6\n",
        )
        .unwrap();

        let interner = Interner::new();
        let provides = parse_needed_provides(&pkg, &interner).unwrap();
        let x86_64 = interner.intern("x86_64");
        let libbar = interner.intern("libbar.so.1");
        assert!(provides.provides_in(x86_64, libbar));
    }

    #[test]
    fn stored_requires_file_is_recorded_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("app-misc").join("baz-1");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("NEEDED.ELF.2"),
            "X86_64;/usr/lib64/libbaz.so.1;libbaz.so.1;;libc.so.6;x86_64\n",
        )
        .unwrap();
        // Portage wrote authoritative PROVIDES/REQUIRES; they win over a recompute.
        std::fs::write(pkg.join("PROVIDES"), "x86_64: libbaz.so.1\n").unwrap();
        std::fs::write(pkg.join("REQUIRES"), "x86_64: libc.so.6\n").unwrap();

        let interner = Interner::new();
        let (provides, requires) = read_soname_linkage(&pkg, &interner).unwrap();
        let x86_64 = interner.intern("x86_64");
        assert!(provides.provides_in(x86_64, interner.intern("libbaz.so.1")));
        assert!(requires.requires_in(x86_64, interner.intern("libc.so.6")));
    }

    #[test]
    fn recomputed_requires_excludes_self_provided() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("app-misc").join("qux-1");
        std::fs::create_dir_all(&pkg).unwrap();
        // No PROVIDES/REQUIRES files: recompute from NEEDED. One object provides
        // `libqux.so.1` and another needs it, so it must drop out of REQUIRES.
        std::fs::write(
            pkg.join("NEEDED.ELF.2"),
            "X86_64;/usr/lib64/libqux.so.1;libqux.so.1;;libc.so.6;x86_64\n\
             X86_64;/usr/bin/qux;;;libqux.so.1,libc.so.6;x86_64\n",
        )
        .unwrap();

        let interner = Interner::new();
        let (_provides, requires) = read_soname_linkage(&pkg, &interner).unwrap();
        let x86_64 = interner.intern("x86_64");
        // The self-provided soname is intersected out; the external one remains.
        assert!(!requires.requires_in(x86_64, interner.intern("libqux.so.1")));
        assert!(requires.requires_in(x86_64, interner.intern("libc.so.6")));
    }
}
