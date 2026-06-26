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
pub fn strip_image<R: CommandRunner>(
    image: &Path,
    config: &ConfigEnv,
    restrict: &[String],
    runner: &R,
) {
    // RESTRICT=binchecks/strip and FEATURES=nostrip suppress the default strip.
    if config.has_feature("nostrip") || restrict.iter().any(|r| r == "strip") {
        return;
    }
    let objects = elf_objects(image);
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
        strip_image(tmp.path(), &cfg, &[], &runner);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn restrict_strip_skips() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.bin"), elf_bytes()).unwrap();
        let runner = FakeRunner::always_ok();
        let cfg = ConfigEnv::rooted([]);
        strip_image(tmp.path(), &cfg, &["strip".to_string()], &runner);
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
        strip_image(tmp.path(), &cfg, &[], &runner);
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
        strip_image(tmp.path(), &cfg, &[], &runner);
        let calls = runner.calls();
        let programs: Vec<&str> = calls.iter().map(|c| c.program.as_str()).collect();
        assert_eq!(programs, vec!["objcopy", "strip", "objcopy"]);
    }
}
