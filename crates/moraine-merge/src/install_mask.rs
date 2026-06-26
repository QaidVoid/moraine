//! `INSTALL_MASK` path filtering.
//!
//! `INSTALL_MASK` and `PKG_INSTALL_MASK` remove staged paths before they enter
//! CONTENTS, mirroring `lib/portage/util/install_mask.py`. A pattern anchored
//! with a leading slash matches the path (or the path as a directory prefix via
//! `pattern/*`); an unanchored pattern matches the basename. A `-pattern`
//! re-includes a path a prior pattern masked, with the last matching pattern in
//! list order deciding the outcome.

/// One `INSTALL_MASK` pattern: whether it masks (inclusive) or re-includes, the
/// glob text, and whether it is anchored to the install root.
#[derive(Debug, Clone)]
struct MaskPattern {
    /// True for a masking pattern, false for a `-pattern` re-include.
    inclusive: bool,
    /// The glob text (with any leading `-` and the anchoring slash retained on
    /// `pattern` only via `anchored`).
    pattern: String,
    /// Whether the pattern began with a leading slash.
    anchored: bool,
}

/// A compiled `INSTALL_MASK`/`PKG_INSTALL_MASK` filter.
#[derive(Debug, Clone, Default)]
pub struct InstallMask {
    patterns: Vec<MaskPattern>,
}

impl InstallMask {
    /// Compile a whitespace-separated `INSTALL_MASK` specification.
    pub fn new(spec: &str) -> Self {
        let patterns = spec
            .split_whitespace()
            .map(|raw| {
                let inclusive = !raw.starts_with('-');
                let pattern = if inclusive { raw } else { &raw[1..] };
                MaskPattern {
                    inclusive,
                    anchored: pattern.starts_with('/'),
                    pattern: pattern.to_owned(),
                }
            })
            .collect();
        InstallMask { patterns }
    }

    /// Whether the filter has no patterns.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether `path` (an install path with a leading slash, as recorded in
    /// CONTENTS) is masked out. The last matching pattern decides, so a later
    /// `-pattern` can re-include a path an earlier pattern masked.
    pub fn is_masked(&self, path: &str) -> bool {
        let rel = path.trim_start_matches('/');
        let base = rel.rsplit('/').next().unwrap_or(rel);
        let mut masked = false;
        for p in &self.patterns {
            let hit = if p.anchored {
                let pat = p.pattern.trim_start_matches('/');
                crate::fnmatch(rel, pat)
                    || crate::fnmatch(rel, &format!("{}/*", pat.trim_end_matches('/')))
            } else {
                crate::fnmatch(base, &p.pattern)
            };
            if hit {
                masked = p.inclusive;
            }
        }
        masked
    }
}

/// Build the combined `INSTALL_MASK` specification from the configured mask
/// values and the `nodoc`/`noman`/`noinfo` FEATURES, which expand to
/// `${eprefix}/usr/share/{doc,man,info}` exactly as `preinst_mask` does.
pub fn combined_spec(
    install_mask: &str,
    pkg_install_mask: &str,
    eprefix: &str,
    features: &[&str],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !install_mask.trim().is_empty() {
        parts.push(install_mask.trim().to_owned());
    }
    if !pkg_install_mask.trim().is_empty() {
        parts.push(pkg_install_mask.trim().to_owned());
    }
    let eprefix = eprefix.trim_end_matches('/');
    for (feature, subdir) in [("nodoc", "doc"), ("noman", "man"), ("noinfo", "info")] {
        if features.contains(&feature) {
            parts.push(format!("{eprefix}/usr/share/{subdir}"));
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchored_directory_masks_children() {
        let mask = InstallMask::new("/usr/share/doc");
        assert!(mask.is_masked("/usr/share/doc"));
        assert!(mask.is_masked("/usr/share/doc/foo/readme"));
        assert!(!mask.is_masked("/usr/share/man/man1/x.1"));
    }

    #[test]
    fn unanchored_matches_basename() {
        let mask = InstallMask::new("*.la");
        assert!(mask.is_masked("/usr/lib/libfoo.la"));
        assert!(!mask.is_masked("/usr/lib/libfoo.so"));
    }

    #[test]
    fn reinclude_pattern_unmasks() {
        // Mask all of /usr/share/doc but keep the licenses subtree.
        let mask = InstallMask::new("/usr/share/doc -/usr/share/doc/licenses");
        assert!(mask.is_masked("/usr/share/doc/foo/readme"));
        assert!(!mask.is_masked("/usr/share/doc/licenses/GPL"));
    }

    #[test]
    fn combined_spec_expands_no_doc_man_info() {
        let spec = combined_spec("/etc/keep", "", "", &["nodoc", "noinfo"]);
        assert!(spec.contains("/etc/keep"));
        assert!(spec.contains("/usr/share/doc"));
        assert!(spec.contains("/usr/share/info"));
        assert!(!spec.contains("/usr/share/man"));
    }
}
