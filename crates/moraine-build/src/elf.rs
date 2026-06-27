//! A minimal ELF dynamic-section reader for `NEEDED.ELF.2` generation.
//!
//! After a build, each ELF object in the staged image is read for its
//! `DT_SONAME` (what it provides) and `DT_NEEDED` entries (what it requires),
//! plus a multilib bucket derived from the ELF class and machine. This feeds the
//! recorded `PROVIDES`/`REQUIRES` and the `NEEDED.ELF.2` lines, matching what
//! Portage's `scanelf`-based pass records, without depending on a full ELF crate.
//!
//! The reader is deliberately conservative: any malformed or non-dynamic file is
//! skipped rather than erroring, so a build is never failed by an odd object.

use std::path::Path;

/// One `NEEDED.ELF.2` line: the multilib bucket, the install path, the soname it
/// provides (if any), and the sonames it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeededLine {
    /// The multilib bucket (for example `x86_64`).
    pub bucket: String,
    /// The install-root-relative path of the object.
    pub path: String,
    /// The `DT_SONAME`, if the object declares one.
    pub soname: Option<String>,
    /// The `DT_NEEDED` sonames.
    pub needed: Vec<String>,
}

/// The soname linkage scanned from a staged image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SonameScan {
    /// The provided `(bucket, soname)` pairs.
    pub provides: Vec<(String, String)>,
    /// The required `(bucket, soname)` pairs.
    pub requires: Vec<(String, String)>,
    /// The full `NEEDED.ELF.2` lines for round-trip recording.
    pub needed_lines: Vec<NeededLine>,
}

/// Scan every regular file under `image_dir`, reading ELF dynamic linkage, and
/// return the aggregated provides/requires and per-object NEEDED lines. A
/// directory that does not exist yields an empty scan.
pub fn scan_image_sonames(image_dir: &Path) -> SonameScan {
    let mut scan = SonameScan::default();
    let mut stack = vec![image_dir.to_path_buf()];
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
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Some(dynamic) = read_dynamic(&bytes) else {
                continue;
            };
            let install_path = path
                .strip_prefix(image_dir)
                .map(|rel| format!("/{}", rel.to_string_lossy()))
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
            if let Some(soname) = &dynamic.soname {
                scan.provides.push((dynamic.bucket.clone(), soname.clone()));
            }
            for need in &dynamic.needed {
                scan.requires.push((dynamic.bucket.clone(), need.clone()));
            }
            scan.needed_lines.push(NeededLine {
                bucket: dynamic.bucket,
                path: install_path,
                soname: dynamic.soname,
                needed: dynamic.needed,
            });
        }
    }
    scan.provides.sort();
    scan.provides.dedup();
    scan.requires.sort();
    scan.requires.dedup();
    intersect_self_provided(&scan.provides, &mut scan.requires);
    scan
}

/// Drop from `requires` any `(bucket, soname)` present in `provides` for the same
/// bucket, mirroring `SonameDepsProcessor._intersect`: a soname the image itself
/// provides is not an external requirement.
fn intersect_self_provided(provides: &[(String, String)], requires: &mut Vec<(String, String)>) {
    let provided: std::collections::HashSet<&(String, String)> = provides.iter().collect();
    requires.retain(|pair| !provided.contains(pair));
}

/// The dynamic linkage of one ELF object.
struct Dynamic {
    bucket: String,
    soname: Option<String>,
    needed: Vec<String>,
}

const DT_NEEDED: u64 = 1;
const DT_SONAME: u64 = 14;
const SHT_DYNAMIC: u32 = 6;

/// Parse the dynamic linkage of an ELF object, returning `None` for a non-ELF or
/// non-dynamic file or any malformed structure.
fn read_dynamic(bytes: &[u8]) -> Option<Dynamic> {
    if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" {
        return None;
    }
    let is_64 = match bytes[4] {
        1 => false,
        2 => true,
        _ => return None,
    };
    let le = match bytes[5] {
        1 => true,
        2 => false,
        _ => return None,
    };
    let r = Reader { bytes, le };

    let e_machine = r.u16(18)?;
    let bucket = multilib_bucket(is_64, e_machine);

    // Section header table.
    let (e_shoff, e_shentsize, e_shnum) = if is_64 {
        (r.u64(40)?, r.u16(58)?, r.u16(60)?)
    } else {
        (r.u32(32)? as u64, r.u16(46)?, r.u16(48)?)
    };
    if e_shoff == 0 || e_shnum == 0 {
        return None;
    }

    // Find the SHT_DYNAMIC section and its linked string table.
    let mut dyn_off = None;
    let mut dyn_size = 0u64;
    let mut dyn_link = 0u32;
    for i in 0..e_shnum as u64 {
        let base = e_shoff + i * e_shentsize as u64;
        let sh_type = r.u32(base as usize + 4)?;
        if sh_type == SHT_DYNAMIC {
            let (off, size, link) = if is_64 {
                (
                    r.u64(base as usize + 24)?,
                    r.u64(base as usize + 32)?,
                    r.u32(base as usize + 40)?,
                )
            } else {
                (
                    r.u32(base as usize + 16)? as u64,
                    r.u32(base as usize + 20)? as u64,
                    r.u32(base as usize + 24)?,
                )
            };
            dyn_off = Some(off);
            dyn_size = size;
            dyn_link = link;
            break;
        }
    }
    let dyn_off = dyn_off?;

    // The string table section pointed to by sh_link.
    let str_base = e_shoff + dyn_link as u64 * e_shentsize as u64;
    let (str_off, str_size) = if is_64 {
        (
            r.u64(str_base as usize + 24)?,
            r.u64(str_base as usize + 32)?,
        )
    } else {
        (
            r.u32(str_base as usize + 16)? as u64,
            r.u32(str_base as usize + 20)? as u64,
        )
    };

    // Walk the dynamic entries.
    let entry_size = if is_64 { 16 } else { 8 };
    let count = (dyn_size / entry_size) as usize;
    let mut soname = None;
    let mut needed = Vec::new();
    for i in 0..count {
        let base = dyn_off as usize + i * entry_size as usize;
        let (tag, val) = if is_64 {
            (r.u64(base)?, r.u64(base + 8)?)
        } else {
            (r.u32(base)? as u64, r.u32(base + 4)? as u64)
        };
        if tag == 0 {
            break;
        }
        if tag == DT_SONAME {
            soname = read_str(bytes, str_off, str_size, val);
        } else if tag == DT_NEEDED
            && let Some(s) = read_str(bytes, str_off, str_size, val)
        {
            needed.push(s);
        }
    }

    if soname.is_none() && needed.is_empty() {
        return None;
    }
    Some(Dynamic {
        bucket,
        soname,
        needed,
    })
}

/// Read a NUL-terminated string at `offset` within the string table located at
/// `str_off..str_off+str_size`.
fn read_str(bytes: &[u8], str_off: u64, str_size: u64, offset: u64) -> Option<String> {
    let start = str_off.checked_add(offset)? as usize;
    let end = str_off.checked_add(str_size)? as usize;
    if start >= end || end > bytes.len() {
        return None;
    }
    let slice = &bytes[start..end];
    let nul = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    std::str::from_utf8(&slice[..nul]).ok().map(str::to_string)
}

/// Map an ELF class and machine to a Portage-style multilib bucket. Common
/// arches are named; an unknown machine falls back to a stable `class;machine`
/// token so provides and requires still match within one ABI.
fn multilib_bucket(is_64: bool, e_machine: u16) -> String {
    match (e_machine, is_64) {
        (62, true) => "x86_64".to_string(),  // EM_X86_64
        (3, false) => "x86_32".to_string(),  // EM_386
        (183, true) => "arm_64".to_string(), // EM_AARCH64
        (40, false) => "arm_32".to_string(), // EM_ARM
        (21, true) => "ppc_64".to_string(),  // EM_PPC64
        (20, false) => "ppc_32".to_string(), // EM_PPC
        (243, true) => "riscv".to_string(),  // EM_RISCV
        _ => format!(
            "{};{e_machine}",
            if is_64 { "ELFCLASS64" } else { "ELFCLASS32" }
        ),
    }
}

/// A small endian- and width-aware byte reader over an ELF image.
struct Reader<'a> {
    bytes: &'a [u8],
    le: bool,
}

impl Reader<'_> {
    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.bytes.get(at..at + 2)?.try_into().ok()?;
        Some(if self.le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.bytes.get(at..at + 4)?.try_into().ok()?;
        Some(if self.le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn u64(&self, at: usize) -> Option<u64> {
        let b: [u8; 8] = self.bytes.get(at..at + 8)?.try_into().ok()?;
        Some(if self.le {
            u64::from_le_bytes(b)
        } else {
            u64::from_be_bytes(b)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_elf_is_skipped() {
        assert!(read_dynamic(b"not an elf file at all, padding padding padding").is_none());
        assert!(read_dynamic(&[0u8; 128]).is_none());
    }

    #[test]
    fn parses_a_real_dynamic_elf() {
        // The test binary itself is a dynamically linked ELF on this platform.
        let exe = std::env::current_exe().unwrap();
        let bytes = std::fs::read(&exe).unwrap();
        let Some(dynamic) = read_dynamic(&bytes) else {
            // A fully static build has no dynamic section; nothing to assert.
            return;
        };
        assert!(!dynamic.bucket.is_empty());
        assert!(
            dynamic.soname.is_some() || !dynamic.needed.is_empty(),
            "a dynamic ELF should declare a soname or needed libraries"
        );
    }

    #[test]
    fn intersect_drops_self_provided_per_bucket() {
        let provides = vec![("x86_64".to_string(), "libfoo.so.1".to_string())];
        let mut requires = vec![
            ("x86_64".to_string(), "libfoo.so.1".to_string()),
            ("x86_64".to_string(), "libc.so.6".to_string()),
            // Same soname in a different bucket is not self-provided here.
            ("x86_32".to_string(), "libfoo.so.1".to_string()),
        ];
        intersect_self_provided(&provides, &mut requires);
        assert!(!requires.contains(&("x86_64".to_string(), "libfoo.so.1".to_string())));
        assert!(requires.contains(&("x86_64".to_string(), "libc.so.6".to_string())));
        assert!(requires.contains(&("x86_32".to_string(), "libfoo.so.1".to_string())));
    }

    #[test]
    fn scan_collects_from_image_tree() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path();
        std::fs::create_dir_all(img.join("usr/lib")).unwrap();
        // A non-ELF file must not contribute.
        std::fs::write(img.join("usr/lib/note.txt"), b"hello").unwrap();
        // Copy a real ELF in so the scan finds at least one linkage record.
        let exe = std::env::current_exe().unwrap();
        std::fs::copy(&exe, img.join("usr/lib/probe")).unwrap();

        let scan = scan_image_sonames(img);
        if read_dynamic(&std::fs::read(&exe).unwrap()).is_some() {
            assert!(
                !scan.needed_lines.is_empty(),
                "an ELF in the image should yield a NEEDED line"
            );
        }
    }
}
