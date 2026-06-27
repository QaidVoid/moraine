//! Post-`src_install` ELF stripping.
//!
//! After the phases run, the staged image `D` is stripped of debug symbols by
//! default, mirroring `bin/estrip`. Stripping is suppressed by `FEATURES=nostrip`
//! or `RESTRICT=strip`, and per-object by `STRIP_MASK`. The strip flags are
//! chosen by the object's real type: executables and shared objects, relocatable
//! objects, and `ar` archives each follow `estrip`'s `process_elf`/`process_ar`
//! dispatch. `FEATURES=splitdebug` writes the debug information under
//! `/usr/lib/debug` before stripping (only for final-linked objects and kernel
//! modules), and `FEATURES=installsources` records sources via `debugedit` and
//! copies the referenced source files into `/usr/src/debug`. The host `strip`,
//! `objcopy`, `ranlib`, and `debugedit` tools are invoked through the injectable
//! [`CommandRunner`]; a missing tool degrades to a warning rather than failing the
//! build.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use moraine_common::glob::fnmatch;

use crate::env::ConfigEnv;
use crate::runner::{CommandRunner, CommandSpec};

/// The safe strip flags applied to every stripped ELF object, matching
/// `estrip`'s `SAFE_STRIP_FLAGS`.
const SAFE_FLAGS: &[&str] = &["--strip-unneeded", "-N", "__gentoo_check_ldflags__"];
/// The extra flags applied on top of [`SAFE_FLAGS`] for executables and shared
/// objects, matching `estrip`'s `DEF_STRIP_FLAGS`.
const FULL_EXTRA: &[&str] = &[
    "-R",
    ".comment",
    "-R",
    ".GCC.command.line",
    "-R",
    ".note.gnu.gold-version",
];

/// The kind of object found in the image, dispatching how it is stripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectKind {
    /// An ELF executable (`ET_EXEC`) or shared object (`ET_DYN`).
    Executable,
    /// An ELF relocatable object (`ET_REL`), for example `.o` or `.ko`.
    Relocatable,
    /// An `ar` archive (`!<arch>\n` magic), for example a static `.a` library.
    Archive,
}

/// Strip the objects staged under the image directory `image` (`D`), honoring the
/// `FEATURES`, `RESTRICT`, and `STRIP_MASK` gates. Never returns an error: a
/// missing host tool or a failed strip is logged and skipped so the build still
/// completes, matching `estrip`'s tolerance.
///
/// `dostrip` and `dostrip_skip` are the `PORTAGE_DOSTRIP` and
/// `PORTAGE_DOSTRIP_SKIP` absolute path lists the `dostrip` helper recorded
/// during `src_install`. An object is stripped only when its image-relative path
/// lies under a `dostrip` path and under no `dostrip_skip` path. An empty
/// `dostrip` list falls back to stripping the whole image.
///
/// `strip_mask` is the whitespace-split `STRIP_MASK` glob list: an object whose
/// image-relative path matches any glob keeps its symbols. `workdir`,
/// `category`, and `pf` are threaded through for `FEATURES=installsources`, which
/// records the referenced sources with `debugedit` and copies them into
/// `${D}/${EPREFIX}/usr/src/debug/${CATEGORY}/${PF}`.
#[allow(clippy::too_many_arguments)]
pub fn strip_image<R: CommandRunner>(
    image: &Path,
    config: &ConfigEnv,
    restrict: &[String],
    dostrip: &[String],
    dostrip_skip: &[String],
    strip_mask: &str,
    workdir: &Path,
    category: &str,
    pf: &str,
    runner: &R,
) {
    // RESTRICT=strip and FEATURES=nostrip suppress the default strip.
    if config.has_feature("nostrip") || restrict.iter().any(|r| r == "strip") {
        return;
    }
    let include = relative_paths(dostrip);
    let exclude = relative_paths(dostrip_skip);
    let masks: Vec<&str> = strip_mask.split_whitespace().collect();
    let objects: Vec<(PathBuf, ObjectKind)> = collect_objects(image)
        .into_iter()
        .filter(|(object, _)| should_strip(object, image, &config.eprefix, &include, &exclude))
        .collect();
    if objects.is_empty() {
        return;
    }

    let splitdebug = config.has_feature("splitdebug");
    let installsources = config.has_feature("installsources");
    let debug_root = debug_root(image, &config.eprefix);
    let sources_dest = prepstrip_sources_dir(&config.eprefix, category, pf);
    // Host tools whose launch failed, warned about only once each.
    let mut missing: BTreeSet<&'static str> = BTreeSet::new();
    // The scratch directory holding `debugedit -l` source lists, and the union of
    // the referenced sources read back from them.
    let scratch = installsources.then(|| sources_scratch(workdir));
    let mut referenced: BTreeSet<String> = BTreeSet::new();

    for (index, (object, kind)) in objects.iter().enumerate() {
        if installsources && let Some(scratch) = &scratch {
            let listfile = scratch.join(format!("sources.{index}"));
            run_tool(
                runner,
                "debugedit",
                &debugedit_args(object, workdir, &sources_dest, &listfile),
                image,
                &mut missing,
            );
            read_source_list(&listfile, &mut referenced);
        }

        // STRIP_MASK skips the strip step but not the source recording above.
        let rel = ed_relative(object, image, &config.eprefix);
        if masks.iter().any(|glob| fnmatch(&rel, glob)) {
            continue;
        }

        match kind {
            ObjectKind::Archive => {
                // Archives run `strip -g` then `ranlib`, never split.
                let path = object.to_string_lossy().into_owned();
                run_tool(
                    runner,
                    "strip",
                    &["-g".to_owned(), path.clone()],
                    image,
                    &mut missing,
                );
                run_tool(runner, "ranlib", &[path], image, &mut missing);
            }
            ObjectKind::Executable => {
                let args = strip_args(ObjectKind::Executable, object);
                if splitdebug {
                    split_and_strip(runner, image, object, &debug_root, &args, &mut missing);
                } else {
                    run_tool(runner, "strip", &args, image, &mut missing);
                }
            }
            ObjectKind::Relocatable => {
                let args = strip_args(ObjectKind::Relocatable, object);
                // Splitdebug for intermediate relocatables is useless; only kernel
                // modules keep it.
                let is_ko = object.extension().and_then(|e| e.to_str()) == Some("ko");
                if splitdebug && is_ko {
                    split_and_strip(runner, image, object, &debug_root, &args, &mut missing);
                } else {
                    run_tool(runner, "strip", &args, image, &mut missing);
                }
            }
        }
    }

    if installsources && !referenced.is_empty() {
        copy_referenced_sources(&referenced, workdir, image, &config.eprefix, category, pf);
    }
    if let Some(scratch) = scratch {
        let _ = std::fs::remove_dir_all(&scratch);
    }
}

/// Write the object's debug info to a sibling under `/usr/lib/debug`, strip the
/// object with `strip_args`, then link the two with a `.gnu_debuglink`.
fn split_and_strip<R: CommandRunner>(
    runner: &R,
    image: &Path,
    object: &Path,
    debug_root: &Path,
    strip_args: &[String],
    missing: &mut BTreeSet<&'static str>,
) {
    let Ok(rel) = object.strip_prefix(image) else {
        return;
    };
    let debug_file = debug_root
        .join(rel)
        .with_extension(extension_with_debug(object));
    if let Some(parent) = debug_file.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(error = %e, "could not create splitdebug directory");
        return;
    }
    let obj = object.to_string_lossy().into_owned();
    let dbg = debug_file.to_string_lossy().into_owned();
    run_tool(
        runner,
        "objcopy",
        &["--only-keep-debug".to_owned(), obj.clone(), dbg.clone()],
        image,
        missing,
    );
    run_tool(runner, "strip", strip_args, image, missing);
    run_tool(
        runner,
        "objcopy",
        &[format!("--add-gnu-debuglink={dbg}"), obj],
        image,
        missing,
    );
}

/// The `strip` arguments for an ELF object, selected by its type: executables and
/// shared objects use the safe flags plus the comment/section cleanups,
/// relocatables use the safe flags alone. Mirrors `estrip`'s flag dispatch.
fn strip_args(kind: ObjectKind, object: &Path) -> Vec<String> {
    let mut args: Vec<String> = SAFE_FLAGS.iter().map(|s| s.to_string()).collect();
    if kind == ObjectKind::Executable {
        args.extend(FULL_EXTRA.iter().map(|s| s.to_string()));
    }
    args.push(object.to_string_lossy().into_owned());
    args
}

/// The `debugedit` arguments recording the source base directory (`WORKDIR`), the
/// per-package destination, and the referenced-source list file.
fn debugedit_args(
    object: &Path,
    workdir: &Path,
    sources_dest: &str,
    listfile: &Path,
) -> Vec<String> {
    vec![
        "-b".to_owned(),
        workdir.to_string_lossy().into_owned(),
        "-d".to_owned(),
        sources_dest.to_owned(),
        "-l".to_owned(),
        listfile.to_string_lossy().into_owned(),
        object.to_string_lossy().into_owned(),
    ]
}

/// Run one host tool through the runner, warning once if its launch fails.
fn run_tool<R: CommandRunner>(
    runner: &R,
    program: &'static str,
    args: &[String],
    cwd: &Path,
    missing: &mut BTreeSet<&'static str>,
) {
    let spec = CommandSpec::new(program, cwd).args(args.iter().cloned());
    match runner.run(&spec) {
        Ok(out) if out.success() => {}
        Ok(out) => tracing::warn!(program, status = out.status, "strip tool reported failure"),
        Err(_) => {
            if missing.insert(program) {
                tracing::warn!("`{program}` is unavailable; skipping that part of stripping");
            }
        }
    }
}

/// The `/usr/lib/debug` root under the image, composing the prefix offset.
fn debug_root(image: &Path, eprefix: &str) -> PathBuf {
    let mut root = image.to_path_buf();
    let offset = eprefix.trim_start_matches('/');
    if !offset.is_empty() {
        root = root.join(offset);
    }
    root.join("usr/lib/debug")
}

/// The per-package source destination, `${EPREFIX}/usr/src/debug/${CATEGORY}/${PF}`.
fn prepstrip_sources_dir(eprefix: &str, category: &str, pf: &str) -> String {
    let eprefix = eprefix.trim_end_matches('/');
    format!("{eprefix}/usr/src/debug/{category}/{pf}")
}

/// The scratch directory under the build tree holding `debugedit` source lists.
fn sources_scratch(workdir: &Path) -> PathBuf {
    let base = workdir.parent().unwrap_or(workdir);
    let dir = base.join(".moraine-strip-sources");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Read a `debugedit -l` source list, adding each non-empty entry to `out`. The
/// list is newline or NUL separated; missing files are tolerated.
fn read_source_list(listfile: &Path, out: &mut BTreeSet<String>) {
    let Ok(text) = std::fs::read_to_string(listfile) else {
        return;
    };
    for entry in text.split(['\0', '\n']) {
        let entry = entry.trim();
        if !entry.is_empty() {
            out.insert(entry.to_owned());
        }
    }
}

/// Copy the unique referenced sources from `WORKDIR` into
/// `${D}/${EPREFIX}/usr/src/debug/${CATEGORY}/${PF}`, skipping system-header
/// (`/<...>`) and bare-directory (`/`) entries, per `estrip:669-690`.
fn copy_referenced_sources(
    referenced: &BTreeSet<String>,
    workdir: &Path,
    image: &Path,
    eprefix: &str,
    category: &str,
    pf: &str,
) {
    let offset = eprefix.trim_start_matches('/').trim_end_matches('/');
    let mut dest_root = image.to_path_buf();
    if !offset.is_empty() {
        dest_root = dest_root.join(offset);
    }
    let dest_root = dest_root.join("usr/src/debug").join(category).join(pf);
    for entry in referenced {
        // Skip complete directories and system headers.
        if entry.ends_with('/') || is_system_header(entry) {
            continue;
        }
        let rel = entry.trim_start_matches('/');
        if rel.is_empty() {
            continue;
        }
        let src = workdir.join(rel);
        if !src.is_file() {
            continue;
        }
        let dest = dest_root.join(rel);
        if let Some(parent) = dest.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(error = %e, "could not create installsources directory");
            continue;
        }
        if let Err(e) = std::fs::copy(&src, &dest) {
            tracing::warn!(error = %e, "could not copy installsources file");
        }
    }
}

/// Whether the entry is a system-header reference (`/<...>`), matching Portage's
/// `/<[^/>]*>$`: a `/<`, a body with no inner `/` or `>`, then a trailing `>`.
fn is_system_header(entry: &str) -> bool {
    let Some(idx) = entry.rfind("/<") else {
        return false;
    };
    let after = &entry[idx + 2..];
    let Some(body) = after.strip_suffix('>') else {
        return false;
    };
    !body.contains('/') && !body.contains('>')
}

/// Whether an object should be stripped given the include/exclude path lists.
///
/// The object is stripped when its install-relative path lies under an include
/// path and under no exclude path. An empty include list strips everything.
fn should_strip(
    object: &Path,
    image: &Path,
    eprefix: &str,
    include: &[PathBuf],
    exclude: &[PathBuf],
) -> bool {
    let Some(rel) = install_relative(object, image, eprefix) else {
        return false;
    };
    let included = include.is_empty() || include.iter().any(|base| path_under(&rel, base));
    let excluded = exclude.iter().any(|base| path_under(&rel, base));
    included && !excluded
}

/// The object's path relative to the install root, accounting for the prefix
/// offset so the `PORTAGE_DOSTRIP` paths (which are relative to `ED`) match.
fn install_relative(object: &Path, image: &Path, eprefix: &str) -> Option<PathBuf> {
    let rel = object.strip_prefix(image).ok()?;
    let offset = eprefix.trim_start_matches('/').trim_end_matches('/');
    if offset.is_empty() {
        return Some(rel.to_path_buf());
    }
    match rel.strip_prefix(offset) {
        Ok(stripped) => Some(stripped.to_path_buf()),
        Err(_) => Some(rel.to_path_buf()),
    }
}

/// The object's `ED`-relative path with a leading slash, the form `STRIP_MASK`
/// globs are matched against (`${x#"${ED%/}"}` in `estrip`).
fn ed_relative(object: &Path, image: &Path, eprefix: &str) -> String {
    match install_relative(object, image, eprefix) {
        Some(rel) => format!("/{}", rel.to_string_lossy()),
        None => String::new(),
    }
}

/// Whether `path` lies at or under `base`, comparing whole path components. An
/// empty `base` (the image root `/`) matches every path.
fn path_under(path: &Path, base: &Path) -> bool {
    base.as_os_str().is_empty() || path == base || path.starts_with(base)
}

/// Convert absolute `PORTAGE_DOSTRIP` paths into image-relative paths by
/// dropping the leading slash. The bare root `/` becomes an empty path that
/// matches every object.
fn relative_paths(absolute: &[String]) -> Vec<PathBuf> {
    absolute
        .iter()
        .map(|p| PathBuf::from(p.trim_start_matches('/')))
        .collect()
}

/// The object's file extension with `.debug` appended for the split debug file.
fn extension_with_debug(object: &Path) -> String {
    match object.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.debug"),
        None => "debug".to_owned(),
    }
}

/// Parse a bash `set -o posix` array dump such as `([0]="/" [1]="/usr/lib")`
/// into its element strings. A value that is not an array dump, or an empty
/// array `()`, yields an empty vector.
pub fn parse_bash_string_array(dump: &str) -> Vec<String> {
    let Some(inner) = dump
        .trim()
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
    else {
        return Vec::new();
    };
    let chars: Vec<char> = inner.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '"' {
            i += 1;
            continue;
        }
        i += 1;
        let mut value = String::new();
        while i < chars.len() {
            if chars[i] == '\\' && i + 1 < chars.len() {
                value.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if chars[i] == '"' {
                i += 1;
                break;
            }
            value.push(chars[i]);
            i += 1;
        }
        out.push(value);
    }
    out
}

/// Collect the regular (non-symlink) strippable objects under `image`, each with
/// its classified [`ObjectKind`].
fn collect_objects(image: &Path) -> Vec<(PathBuf, ObjectKind)> {
    let mut out = Vec::new();
    let mut stack = vec![image.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file()
                && let Some(kind) = classify(&path)
            {
                out.push((path, kind));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Classify a file by its leading magic: an `ar` archive (`!<arch>\n`), or an ELF
/// object dispatched on its `e_type` (offset 16, honoring the `EI_DATA`
/// endianness byte at offset 5). Other files, and ELF types that are neither
/// executable/shared, relocatable, return `None`.
fn classify(path: &Path) -> Option<ObjectKind> {
    use std::io::Read as _;
    let mut buf = [0u8; 18];
    let mut file = std::fs::File::open(path).ok()?;
    let mut read = 0;
    while read < buf.len() {
        match file.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => return None,
        }
    }
    if read >= 8 && &buf[..8] == b"!<arch>\n" {
        return Some(ObjectKind::Archive);
    }
    if read >= 18 && &buf[..4] == b"\x7fELF" {
        let little = buf[5] != 2;
        let e_type = if little {
            u16::from_le_bytes([buf[16], buf[17]])
        } else {
            u16::from_be_bytes([buf[16], buf[17]])
        };
        return match e_type {
            1 => Some(ObjectKind::Relocatable),
            2 | 3 => Some(ObjectKind::Executable),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::FakeRunner;

    /// ELF header bytes with the given `e_type` at offset 16 (little-endian).
    fn elf_bytes(e_type: u16) -> Vec<u8> {
        let mut v = vec![0u8; 64];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = 2; // EI_CLASS = ELFCLASS64
        v[5] = 1; // EI_DATA = little-endian
        v[16..18].copy_from_slice(&e_type.to_le_bytes());
        v
    }

    /// An `ET_EXEC` executable.
    fn exec_bytes() -> Vec<u8> {
        elf_bytes(2)
    }

    /// An `ET_DYN` shared object.
    fn shared_bytes() -> Vec<u8> {
        elf_bytes(3)
    }

    /// An `ET_REL` relocatable object.
    fn reloc_bytes() -> Vec<u8> {
        elf_bytes(1)
    }

    /// An `ar` archive.
    fn ar_bytes() -> Vec<u8> {
        let mut v = b"!<arch>\n".to_vec();
        v.extend(std::iter::repeat_n(0u8, 16));
        v
    }

    fn strip_image_default<R: CommandRunner>(image: &Path, config: &ConfigEnv, runner: &R) {
        strip_image(
            image,
            config,
            &[],
            &[],
            &[],
            "",
            &image.join("work"),
            "dev-libs",
            "foo-1",
            runner,
        );
    }

    #[test]
    fn nostrip_skips_all_tools() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.bin"), exec_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted(["nostrip".to_string()]);
        strip_image_default(tmp.path(), &cfg, &runner);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn restrict_strip_skips() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.bin"), exec_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted([]);
        strip_image(
            tmp.path(),
            &cfg,
            &["strip".to_string()],
            &[],
            &[],
            "",
            &tmp.path().join("work"),
            "dev-libs",
            "foo-1",
            &runner,
        );
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn executables_and_shared_use_full_flags() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("prog"), exec_bytes()).unwrap();
        std::fs::write(tmp.path().join("readme.txt"), b"not elf").unwrap();
        std::fs::write(tmp.path().join("lib.so"), shared_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted([]);
        strip_image_default(tmp.path(), &cfg, &runner);
        let calls = runner.calls();
        // Only the two ELF files are stripped; the text file is untouched.
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|c| c.program == "strip"));
        for c in &calls {
            // Both the executable and the shared object get the full flag set.
            assert!(c.args.iter().any(|a| a == "--strip-unneeded"));
            assert!(c.args.iter().any(|a| a == "__gentoo_check_ldflags__"));
            assert!(c.args.iter().any(|a| a == ".comment"));
            assert!(c.args.iter().any(|a| a == ".note.gnu.gold-version"));
        }
    }

    #[test]
    fn relocatable_uses_safe_flags_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("module.ko"), reloc_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted([]);
        strip_image_default(tmp.path(), &cfg, &runner);
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "strip");
        // The safe flags are present, the full-only section removals are not.
        assert!(calls[0].args.iter().any(|a| a == "--strip-unneeded"));
        assert!(
            calls[0]
                .args
                .iter()
                .any(|a| a == "__gentoo_check_ldflags__")
        );
        assert!(!calls[0].args.iter().any(|a| a == ".comment"));
    }

    #[test]
    fn archive_runs_strip_g_then_ranlib() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("libfoo.a"), ar_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted([]);
        strip_image_default(tmp.path(), &cfg, &runner);
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].program, "strip");
        assert!(calls[0].args.iter().any(|a| a == "-g"));
        assert!(!calls[0].args.iter().any(|a| a == "--strip-unneeded"));
        assert_eq!(calls[1].program, "ranlib");
    }

    #[test]
    fn archive_is_never_split() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("libfoo.a"), ar_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted(["splitdebug".to_string()]);
        strip_image_default(tmp.path(), &cfg, &runner);
        let calls = runner.calls();
        // No objcopy: archives keep their debug info, never split.
        assert!(calls.iter().all(|c| c.program != "objcopy"));
        assert_eq!(calls[0].program, "strip");
        assert_eq!(calls[1].program, "ranlib");
    }

    #[test]
    fn splitdebug_runs_objcopy_and_strip_for_executable() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("prog"), exec_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted(["splitdebug".to_string()]);
        strip_image_default(tmp.path(), &cfg, &runner);
        let calls = runner.calls();
        let programs: Vec<&str> = calls.iter().map(|c| c.program.as_str()).collect();
        assert_eq!(programs, vec!["objcopy", "strip", "objcopy"]);
    }

    #[test]
    fn relocatable_splits_only_for_ko() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("crt.o"), reloc_bytes()).unwrap();
        std::fs::write(tmp.path().join("module.ko"), reloc_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted(["splitdebug".to_string()]);
        strip_image_default(tmp.path(), &cfg, &runner);
        let calls = runner.calls();
        // The `.ko` splits (objcopy/strip/objcopy); the `.o` is stripped only.
        let ko_objcopy = calls
            .iter()
            .filter(|c| c.program == "objcopy" && c.args.iter().any(|a| a.contains("module.ko")))
            .count();
        assert!(ko_objcopy >= 1);
        let o_objcopy = calls
            .iter()
            .filter(|c| c.program == "objcopy" && c.args.iter().any(|a| a.contains("crt.o")))
            .count();
        assert_eq!(o_objcopy, 0);
    }

    #[test]
    fn strip_mask_excludes_object() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("usr/lib/keep");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("masked.so"), shared_bytes()).unwrap();
        std::fs::write(tmp.path().join("prog"), exec_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted([]);
        strip_image(
            tmp.path(),
            &cfg,
            &[],
            &[],
            &[],
            "/usr/lib/keep/*",
            &tmp.path().join("work"),
            "dev-libs",
            "foo-1",
            &runner,
        );
        let calls = runner.calls();
        // Only the unmasked program is stripped.
        assert_eq!(calls.len(), 1);
        assert!(calls[0].args.iter().any(|a| a.ends_with("prog")));
        assert!(
            !calls
                .iter()
                .any(|c| c.args.iter().any(|a| a.ends_with("masked.so")))
        );
    }

    #[test]
    fn dostrip_skip_excludes_object() {
        let tmp = tempfile::tempdir().unwrap();
        // One object under an excluded path, one under a stripped path.
        let keep = tmp.path().join("usr/lib/foo");
        let strip = tmp.path().join("usr/bin");
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::create_dir_all(&strip).unwrap();
        std::fs::write(keep.join("keepme.so"), shared_bytes()).unwrap();
        std::fs::write(strip.join("prog"), exec_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted([]);
        strip_image(
            tmp.path(),
            &cfg,
            &[],
            &["/".to_string()],
            &["/usr/lib/foo".to_string()],
            "",
            &tmp.path().join("work"),
            "dev-libs",
            "foo-1",
            &runner,
        );
        let calls = runner.calls();
        // Only the object outside the skip path is stripped.
        assert_eq!(calls.len(), 1);
        assert!(calls[0].args.iter().any(|a| a.ends_with("prog")));
        assert!(
            !calls
                .iter()
                .any(|c| c.args.iter().any(|a| a.ends_with("keepme.so")))
        );
    }

    #[test]
    fn classify_reads_magic_and_type() {
        let tmp = tempfile::tempdir().unwrap();
        let exec = tmp.path().join("e");
        let shared = tmp.path().join("s");
        let reloc = tmp.path().join("r");
        let ar = tmp.path().join("a");
        let txt = tmp.path().join("t");
        std::fs::write(&exec, exec_bytes()).unwrap();
        std::fs::write(&shared, shared_bytes()).unwrap();
        std::fs::write(&reloc, reloc_bytes()).unwrap();
        std::fs::write(&ar, ar_bytes()).unwrap();
        std::fs::write(&txt, b"not an object").unwrap();
        assert_eq!(classify(&exec), Some(ObjectKind::Executable));
        assert_eq!(classify(&shared), Some(ObjectKind::Executable));
        assert_eq!(classify(&reloc), Some(ObjectKind::Relocatable));
        assert_eq!(classify(&ar), Some(ObjectKind::Archive));
        assert_eq!(classify(&txt), None);
    }

    #[test]
    fn is_system_header_matches_angle_brackets() {
        assert!(is_system_header("/usr/src/<built-in>"));
        assert!(!is_system_header("/usr/src/foo.c"));
        assert!(!is_system_header("/usr/src/<a/b>"));
    }

    #[test]
    fn parse_bash_string_array_reads_elements() {
        assert_eq!(
            parse_bash_string_array("([0]=\"/\" [1]=\"/usr/lib/foo\")"),
            vec!["/".to_string(), "/usr/lib/foo".to_string()]
        );
        assert!(parse_bash_string_array("()").is_empty());
        assert!(parse_bash_string_array("not an array").is_empty());
    }
}
