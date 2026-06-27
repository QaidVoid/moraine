//! Post-`src_install` ELF stripping.
//!
//! After the phases run, the staged image `D` is stripped of ELF debug symbols
//! by default, mirroring `bin/estrip`. Stripping is suppressed by
//! `FEATURES=nostrip` or `RESTRICT=strip`. `FEATURES=splitdebug` writes the debug
//! information under `/usr/lib/debug` before stripping, and
//! `FEATURES=installsources` records sources via `debugedit`. The host `strip`,
//! `objcopy`, and `debugedit` tools are invoked through the injectable
//! [`CommandRunner`]; a missing tool degrades to a warning rather than failing the
//! build.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::env::ConfigEnv;
use crate::runner::{CommandRunner, CommandSpec};

/// Strip the ELF objects staged under the image directory `image` (`D`),
/// honoring the `FEATURES` and `RESTRICT` gates. Never returns an error: a
/// missing host tool or a failed strip is logged and skipped so the build still
/// completes, matching `estrip`'s tolerance.
///
/// `dostrip` and `dostrip_skip` are the `PORTAGE_DOSTRIP` and
/// `PORTAGE_DOSTRIP_SKIP` absolute path lists the `dostrip` helper recorded
/// during `src_install`. An object is stripped only when its image-relative path
/// lies under a `dostrip` path and under no `dostrip_skip` path. An empty
/// `dostrip` list falls back to stripping the whole image.
pub fn strip_image<R: CommandRunner>(
    image: &Path,
    config: &ConfigEnv,
    restrict: &[String],
    dostrip: &[String],
    dostrip_skip: &[String],
    runner: &R,
) {
    // RESTRICT=binchecks/strip and FEATURES=nostrip suppress the default strip.
    if config.has_feature("nostrip") || restrict.iter().any(|r| r == "strip") {
        return;
    }
    let include = relative_paths(dostrip);
    let exclude = relative_paths(dostrip_skip);
    let objects: Vec<PathBuf> = elf_objects(image)
        .into_iter()
        .filter(|object| should_strip(object, image, &config.eprefix, &include, &exclude))
        .collect();
    if objects.is_empty() {
        return;
    }

    let splitdebug = config.has_feature("splitdebug");
    let installsources = config.has_feature("installsources");
    let debug_root = debug_root(image, &config.eprefix);
    // Host tools whose launch failed, warned about only once each.
    let mut missing: BTreeSet<&'static str> = BTreeSet::new();

    for object in &objects {
        if installsources {
            run_tool(
                runner,
                "debugedit",
                &debugedit_args(object),
                image,
                &mut missing,
            );
        }
        if splitdebug {
            split_and_strip(runner, image, object, &debug_root, &mut missing);
        } else {
            run_tool(runner, "strip", &strip_args(object), image, &mut missing);
        }
    }
}

/// Write the object's debug info to a sibling under `/usr/lib/debug`, strip the
/// object, then link the two with a `.gnu_debuglink`.
fn split_and_strip<R: CommandRunner>(
    runner: &R,
    image: &Path,
    object: &Path,
    debug_root: &Path,
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
    run_tool(runner, "strip", &strip_args(object), image, missing);
    run_tool(
        runner,
        "objcopy",
        &[format!("--add-gnu-debuglink={dbg}"), obj],
        image,
        missing,
    );
}

/// The `strip` arguments for an object: shared objects keep their dynamic symbol
/// table (`--strip-unneeded`), executables are stripped fully.
fn strip_args(object: &Path) -> Vec<String> {
    let name = object
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let shared = name.ends_with(".so") || name.contains(".so.");
    let path = object.to_string_lossy().into_owned();
    if shared {
        vec!["--strip-unneeded".to_owned(), path]
    } else {
        vec![path]
    }
}

/// The `debugedit` arguments recording the source base directory.
fn debugedit_args(object: &Path) -> Vec<String> {
    vec![
        "-b".to_owned(),
        "/".to_owned(),
        "-d".to_owned(),
        "/usr/src/debug".to_owned(),
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

/// The object's file extension with `.debug` appended for the split debug file.
fn extension_with_debug(object: &Path) -> String {
    match object.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.debug"),
        None => "debug".to_owned(),
    }
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

/// Collect the regular (non-symlink) ELF object files under `image`.
fn elf_objects(image: &Path) -> Vec<PathBuf> {
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
            } else if meta.is_file() && is_elf(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Whether the file begins with the ELF magic.
fn is_elf(path: &Path) -> bool {
    let mut buf = [0u8; 4];
    use std::io::Read as _;
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
        && &buf == b"\x7fELF"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::testing::FakeRunner;

    fn elf_bytes() -> Vec<u8> {
        let mut v = b"\x7fELF".to_vec();
        v.extend(std::iter::repeat_n(0u8, 60));
        v
    }

    #[test]
    fn nostrip_skips_all_tools() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.bin"), elf_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted(["nostrip".to_string()]);
        strip_image(tmp.path(), &cfg, &[], &[], &[], &runner);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn restrict_strip_skips() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.bin"), elf_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted([]);
        strip_image(tmp.path(), &cfg, &["strip".to_string()], &[], &[], &runner);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn default_strips_elf_objects_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("prog"), elf_bytes()).unwrap();
        std::fs::write(tmp.path().join("readme.txt"), b"not elf").unwrap();
        std::fs::write(tmp.path().join("lib.so"), elf_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted([]);
        strip_image(tmp.path(), &cfg, &[], &[], &[], &runner);
        let calls = runner.calls();
        // Only the two ELF files are stripped; the text file is untouched.
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|c| c.program == "strip"));
        // The shared object keeps its dynamic symbols via --strip-unneeded.
        let so = calls
            .iter()
            .find(|c| c.args.iter().any(|a| a.ends_with("lib.so")))
            .unwrap();
        assert!(so.args.iter().any(|a| a == "--strip-unneeded"));
    }

    #[test]
    fn splitdebug_runs_objcopy_and_strip() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("prog"), elf_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted(["splitdebug".to_string()]);
        strip_image(tmp.path(), &cfg, &[], &[], &[], &runner);
        let calls = runner.calls();
        let programs: Vec<&str> = calls.iter().map(|c| c.program.as_str()).collect();
        assert_eq!(programs, vec!["objcopy", "strip", "objcopy"]);
    }

    #[test]
    fn dostrip_skip_excludes_object() {
        let tmp = tempfile::tempdir().unwrap();
        // One object under an excluded path, one under a stripped path.
        let keep = tmp.path().join("usr/lib/foo");
        let strip = tmp.path().join("usr/bin");
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::create_dir_all(&strip).unwrap();
        std::fs::write(keep.join("keepme.so"), elf_bytes()).unwrap();
        std::fs::write(strip.join("prog"), elf_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted([]);
        strip_image(
            tmp.path(),
            &cfg,
            &[],
            &["/".to_string()],
            &["/usr/lib/foo".to_string()],
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
    fn parse_bash_string_array_reads_elements() {
        assert_eq!(
            parse_bash_string_array("([0]=\"/\" [1]=\"/usr/lib/foo\")"),
            vec!["/".to_string(), "/usr/lib/foo".to_string()]
        );
        assert!(parse_bash_string_array("()").is_empty());
        assert!(parse_bash_string_array("not an array").is_empty());
    }
}
