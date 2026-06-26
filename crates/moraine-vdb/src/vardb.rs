//! Portage-format VDB export.
//!
//! Each installed package is materialized as a `<vdb>/<category>/<PF>/` directory
//! with one file per aux key, the layout Portage's `vardbapi._aux_get` reads. The
//! directory tree is authoritative and interoperable with `emerge`, `equery`,
//! `qlist`, and `portageq`; moraine's own `installed.mvdb` is a derived cache.
//!
//! Export is crash-safe: the directory is written to a sibling temp directory and
//! renamed into place so a partial export is never observed.

use std::path::{Path, PathBuf};

use moraine_common::Interner;

use crate::contents::{Contents, EntryKind};
use crate::error::VdbError;
use crate::record::{DependKind, PackageRecord};

/// Materialize the Portage-format dbdir for `record` under `vdb_root`, replacing
/// any existing dbdir atomically. `ebuild` is the ebuild source to copy in, when
/// available.
pub fn export_record(
    vdb_root: &Path,
    record: &PackageRecord,
    interner: &Interner,
    ebuild: Option<&[u8]>,
) -> Result<(), VdbError> {
    let category = resolve(interner, record.category);
    let package = resolve(interner, record.package);
    let pf = format!("{package}-{}", record.version.as_str());

    let cat_dir = vdb_root.join(&category);
    let final_dir = cat_dir.join(&pf);
    let tmp_dir = cat_dir.join(format!(".{pf}.tmp"));

    io(&tmp_dir, std::fs::create_dir_all(&tmp_dir))?;
    // Clear a stale temp from a prior interrupted export.
    let _ = std::fs::remove_dir_all(&tmp_dir);
    io(&tmp_dir, std::fs::create_dir_all(&tmp_dir))?;

    write_aux_files(&tmp_dir, record, interner, &category, &pf, ebuild)?;

    // Swap the new dbdir in: remove any existing one, then rename.
    let _ = std::fs::remove_dir_all(&final_dir);
    io(&final_dir, std::fs::rename(&tmp_dir, &final_dir))?;
    Ok(())
}

/// Remove the Portage-format dbdir for `category/package-version` under
/// `vdb_root`. A missing dbdir is not an error.
pub fn remove_record(
    vdb_root: &Path,
    category: &str,
    package: &str,
    version: &str,
) -> Result<(), VdbError> {
    let dir = vdb_root.join(category).join(format!("{package}-{version}"));
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(VdbError::Io { path: dir, source }),
    }
}

fn write_aux_files(
    dir: &Path,
    record: &PackageRecord,
    interner: &Interner,
    category: &str,
    pf: &str,
    ebuild: Option<&[u8]>,
) -> Result<(), VdbError> {
    let slot = match record.slot.subslot {
        Some(sub) => format!(
            "{}/{}",
            resolve(interner, record.slot.slot),
            resolve(interner, sub)
        ),
        None => resolve(interner, record.slot.slot),
    };
    let use_flags = record
        .use_flags
        .iter()
        .map(|&s| resolve(interner, s))
        .collect::<Vec<_>>()
        .join(" ");

    let mut files: Vec<(&str, String)> = vec![
        ("CATEGORY", category.to_string()),
        ("PF", pf.to_string()),
        ("EAPI", record.eapi.clone()),
        ("SLOT", slot),
        ("USE", use_flags),
        ("IUSE", record.iuse.join(" ")),
        ("KEYWORDS", record.keywords.join(" ")),
        ("LICENSE", record.license.clone()),
        ("PROPERTIES", record.properties.clone()),
        ("RESTRICT", record.restrict.clone()),
        ("DEFINED_PHASES", record.defined_phases.join(" ")),
        ("INHERITED", record.inherited.join(" ")),
        ("FEATURES", record.features.join(" ")),
        ("CHOST", record.chost.clone()),
        ("COUNTER", record.counter.to_string()),
        ("CONTENTS", render_contents(&record.contents)),
        ("NEEDED.ELF.2", render_needed(record, interner)),
        (
            "PROVIDES",
            render_sonames(record.provides.entries.iter(), interner),
        ),
        (
            "REQUIRES",
            render_sonames(record.requires.entries.iter(), interner),
        ),
    ];

    for kind in DependKind::ALL {
        let raw = record
            .depends
            .get(kind)
            .map(|d| d.raw.clone())
            .unwrap_or_default();
        files.push((kind.name(), raw));
    }
    if let Some(repo) = record.repository {
        files.push(("repository", resolve(interner, repo)));
    }
    if let Some(bt) = record.build_time {
        files.push(("BUILD_TIME", bt.to_string()));
    }
    if let Some(id) = record.build_id {
        files.push(("BUILD_ID", id.to_string()));
    }
    if let Some(size) = record.size {
        files.push(("SIZE", size.to_string()));
    }

    for (name, content) in files {
        write_line_file(dir, name, &content)?;
    }

    // The saved build environment, written as the real compressed blob.
    if let Some(env) = &record.environment {
        let path = dir.join("environment.bz2");
        io(&path, std::fs::write(&path, &env.blob))?;
    }
    if let Some(bytes) = ebuild {
        let path = dir.join(format!("{pf}.ebuild"));
        io(&path, std::fs::write(&path, bytes))?;
    }
    Ok(())
}

/// Write a single aux file with a trailing newline (Portage's convention for
/// one-line aux files; multi-line files already end in a newline).
fn write_line_file(dir: &Path, name: &str, content: &str) -> Result<(), VdbError> {
    let path = dir.join(name);
    let mut body = content.to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    io(&path, std::fs::write(&path, body.as_bytes()))
}

/// Render the CONTENTS manifest in Portage's text format.
fn render_contents(contents: &Contents) -> String {
    let mut out = String::new();
    for entry in contents.iter() {
        match &entry.kind {
            EntryKind::Obj { md5, mtime } => {
                out.push_str(&format!("obj {} {md5} {mtime}\n", entry.path));
            }
            EntryKind::Sym { target, mtime } => {
                out.push_str(&format!("sym {} -> {target} {mtime}\n", entry.path));
            }
            EntryKind::Dir => out.push_str(&format!("dir {}\n", entry.path)),
            EntryKind::Fif => out.push_str(&format!("fif {}\n", entry.path)),
            EntryKind::Dev => out.push_str(&format!("dev {}\n", entry.path)),
        }
    }
    out
}

/// Render the `NEEDED.ELF.2` file. The verbatim recorded lines are written when
/// present, preserving per-object paths and rpaths; otherwise the lines are
/// reconstructed from the recorded provides and requires so the soname sets still
/// round-trip through the importer.
fn render_needed(record: &PackageRecord, interner: &Interner) -> String {
    if !record.needed.is_empty() {
        let mut out = record.needed.join("\n");
        out.push('\n');
        return out;
    }
    let mut out = String::new();
    for e in &record.provides.entries {
        let arch = resolve(interner, e.bucket);
        let soname = resolve(interner, e.soname);
        out.push_str(&format!("{arch};/{soname};{soname};;\n"));
    }
    // Group requires by arch into a single consumer line each.
    let mut by_arch: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for e in &record.requires.entries {
        by_arch
            .entry(resolve(interner, e.bucket))
            .or_default()
            .push(resolve(interner, e.soname));
    }
    for (arch, sonames) in by_arch {
        out.push_str(&format!(
            "{arch};/{}.consumer;;;{}\n",
            arch,
            sonames.join(",")
        ));
    }
    out
}

/// Render the PROVIDES/REQUIRES file as `<arch>: soname soname` lines.
fn render_sonames<'a>(
    entries: impl Iterator<Item = &'a crate::soname::SonameEntry>,
    interner: &Interner,
) -> String {
    let mut by_arch: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for e in entries {
        by_arch
            .entry(resolve(interner, e.bucket))
            .or_default()
            .push(resolve(interner, e.soname));
    }
    let mut out = String::new();
    for (arch, sonames) in by_arch {
        out.push_str(&format!("{arch}: {}\n", sonames.join(" ")));
    }
    out
}

/// Resolve an interned symbol to an owned string, empty when unknown.
fn resolve(interner: &Interner, sym: moraine_common::Symbol) -> String {
    interner
        .resolve(sym)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Map an `io::Result` to a [`VdbError::Io`] tagged with `path`.
fn io<T>(path: &Path, r: std::io::Result<T>) -> Result<T, VdbError> {
    r.map_err(|source| VdbError::Io {
        path: PathBuf::from(path),
        source,
    })
}
