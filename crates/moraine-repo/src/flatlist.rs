//! The PMS `metadata/cache` (flat_list) cache reader.
//!
//! A flat_list cache file is one entry per ebuild under `metadata/cache/<cat>/`,
//! in either the positional form (the 22 `auxdbkey_order` lines, one value per
//! line) or the newer hashed `KEY=VALUE` form. This module parses both into the
//! `KEY -> VALUE` field map the importer consumes, mirroring
//! `lib/portage/cache/metadata.py`.

use std::collections::HashMap;

/// The selected metadata cache format for a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheFormat {
    /// `metadata/md5-cache`, the modern `KEY=VALUE` md5-dict format.
    Md5Dict,
    /// `metadata/cache`, the PMS positional (or hashed) flat_list format.
    Pms,
}

/// The positional `auxdbkey_order` of the PMS flat_list cache: each line in the
/// file is the value for the key at the same index. Trailing unused slots are
/// padding so a file always has the same line count.
const AUXDBKEY_ORDER: [&str; 22] = [
    "DEPEND",
    "RDEPEND",
    "SLOT",
    "SRC_URI",
    "RESTRICT",
    "HOMEPAGE",
    "LICENSE",
    "DESCRIPTION",
    "KEYWORDS",
    "INHERITED",
    "IUSE",
    "REQUIRED_USE",
    "PDEPEND",
    "BDEPEND",
    "EAPI",
    "PROPERTIES",
    "DEFINED_PHASES",
    "HDEPEND",
    "IDEPEND",
    "UNUSED_03",
    "UNUSED_02",
    "UNUSED_01",
];

/// Parse a flat_list cache file's text into its `KEY -> VALUE` field map.
///
/// A file whose first non-empty line contains `=` is the hashed `KEY=VALUE` form;
/// otherwise it is the positional form keyed by [`AUXDBKEY_ORDER`].
pub fn parse(text: &str) -> HashMap<String, String> {
    if is_hashed(text) {
        parse_hashed(text)
    } else {
        parse_positional(text)
    }
}

/// Whether the text is the hashed `KEY=VALUE` form.
fn is_hashed(text: &str) -> bool {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.contains('='))
        .unwrap_or(false)
}

fn parse_hashed(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            out.insert(key.trim().to_string(), value.to_string());
        }
    }
    out
}

fn parse_positional(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (idx, line) in text.lines().enumerate() {
        let Some(key) = AUXDBKEY_ORDER.get(idx) else {
            break;
        };
        if key.starts_with("UNUSED") {
            continue;
        }
        out.insert((*key).to_string(), line.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_maps_by_auxdbkey_order() {
        // DEPEND, RDEPEND, SLOT, SRC_URI, RESTRICT, HOMEPAGE, LICENSE, DESCRIPTION,
        // KEYWORDS, ..., EAPI at index 14.
        let lines = [
            "dev-lang/perl", // DEPEND
            "dev-libs/zlib", // RDEPEND
            "0/3",           // SLOT
            "",              // SRC_URI
            "",              // RESTRICT
            "https://x",     // HOMEPAGE
            "GPL-2",         // LICENSE
            "desc",          // DESCRIPTION
            "amd64 ~arm64",  // KEYWORDS
            "",              // INHERITED
            "ssl",           // IUSE
            "",              // REQUIRED_USE
            "",              // PDEPEND
            "",              // BDEPEND
            "8",             // EAPI
        ];
        let fields = parse(&lines.join("\n"));
        assert_eq!(
            fields.get("DEPEND").map(String::as_str),
            Some("dev-lang/perl")
        );
        assert_eq!(fields.get("SLOT").map(String::as_str), Some("0/3"));
        assert_eq!(
            fields.get("KEYWORDS").map(String::as_str),
            Some("amd64 ~arm64")
        );
        assert_eq!(fields.get("EAPI").map(String::as_str), Some("8"));
    }

    #[test]
    fn hashed_form_parsed() {
        let fields = parse("EAPI=8\nSLOT=0\nRDEPEND=dev-libs/zlib\n");
        assert_eq!(fields.get("EAPI").map(String::as_str), Some("8"));
        assert_eq!(
            fields.get("RDEPEND").map(String::as_str),
            Some("dev-libs/zlib")
        );
    }
}
