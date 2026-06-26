//! Parsing of Gentoo `Manifest` files for metamanifest verification.
//!
//! A repository's signed top-level `Manifest` lists, for every tracked file, a
//! size and one or more hashes, plus `MANIFEST` entries pointing at nested
//! Manifest files (typically the gzipped `Manifest.files.gz`) and a `TIMESTAMP`.
//! This module parses that line format into typed entries so the verifier can
//! recursively check sizes and hashes across the whole tree.

/// A single Manifest record: a typed file entry with a size and hash list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// The record type (`MANIFEST`, `DATA`, `DIST`, `EBUILD`, `MISC`, `AUX`).
    pub kind: String,
    /// The file path relative to the directory holding this Manifest.
    pub path: String,
    /// The recorded file size in bytes.
    pub size: u64,
    /// The recorded hashes as `(name, hex)` pairs in listed order.
    pub hashes: Vec<(String, String)>,
}

/// The parsed contents of one Manifest file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// The file entries, excluding `TIMESTAMP`/`IGNORE` control lines.
    pub entries: Vec<ManifestEntry>,
    /// The `TIMESTAMP` value (ISO 8601), when present.
    pub timestamp: Option<String>,
}

/// Parse Manifest text into typed entries. A PGP cleartext wrapper is stripped
/// first, so a signed top-level Manifest parses the same as a bare one.
pub fn parse(text: &str) -> Manifest {
    let body = strip_cleartext(text);
    let mut manifest = Manifest::default();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let Some(kind) = tokens.next() else {
            continue;
        };
        match kind {
            "TIMESTAMP" => manifest.timestamp = tokens.next().map(str::to_owned),
            // `IGNORE <dir>` marks an unverified directory; it carries no hashes.
            "IGNORE" => {}
            _ => {
                let Some(path) = tokens.next() else {
                    continue;
                };
                let Some(size) = tokens.next().and_then(|s| s.parse::<u64>().ok()) else {
                    continue;
                };
                let mut hashes = Vec::new();
                while let (Some(name), Some(hex)) = (tokens.next(), tokens.next()) {
                    hashes.push((name.to_owned(), hex.to_owned()));
                }
                manifest.entries.push(ManifestEntry {
                    kind: kind.to_owned(),
                    path: path.to_owned(),
                    size,
                    hashes,
                });
            }
        }
    }
    manifest
}

/// Strip an inline PGP cleartext signature wrapper, returning the signed body.
/// Text without a wrapper is returned unchanged.
fn strip_cleartext(text: &str) -> String {
    if !text.contains("-----BEGIN PGP SIGNED MESSAGE-----") {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut in_body = false;
    let mut past_headers = false;
    for line in text.lines() {
        if line.starts_with("-----BEGIN PGP SIGNED MESSAGE-----") {
            in_body = true;
            past_headers = false;
            continue;
        }
        if line.starts_with("-----BEGIN PGP SIGNATURE-----") {
            break;
        }
        if !in_body {
            continue;
        }
        if !past_headers {
            // The armor headers (e.g. `Hash: SHA512`) end at the first blank line.
            if line.trim().is_empty() {
                past_headers = true;
            }
            continue;
        }
        out.push_str(line.strip_prefix("- ").unwrap_or(line));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entries_and_timestamp() {
        let text = "MANIFEST Manifest.files.gz 30419 BLAKE2B aa SHA512 bb\n\
                    TIMESTAMP 2026-06-21T05:38:02Z\n";
        let m = parse(text);
        assert_eq!(m.timestamp.as_deref(), Some("2026-06-21T05:38:02Z"));
        assert_eq!(m.entries.len(), 1);
        let e = &m.entries[0];
        assert_eq!(e.kind, "MANIFEST");
        assert_eq!(e.path, "Manifest.files.gz");
        assert_eq!(e.size, 30419);
        assert_eq!(
            e.hashes,
            vec![
                ("BLAKE2B".to_owned(), "aa".to_owned()),
                ("SHA512".to_owned(), "bb".to_owned()),
            ]
        );
    }

    #[test]
    fn strips_cleartext_signature_and_dash_escape() {
        let text = "-----BEGIN PGP SIGNED MESSAGE-----\n\
                    Hash: SHA512\n\n\
                    DATA foo 3 SHA256 cc\n\
                    - DATA bar 4 SHA256 dd\n\
                    -----BEGIN PGP SIGNATURE-----\n\
                    junk\n\
                    -----END PGP SIGNATURE-----\n";
        let m = parse(text);
        assert_eq!(m.entries.len(), 2);
        assert_eq!(m.entries[0].path, "foo");
        assert_eq!(m.entries[1].path, "bar");
        assert_eq!(m.entries[1].kind, "DATA");
    }

    #[test]
    fn ignores_control_lines() {
        let m = parse("IGNORE local\nDATA a 1 MD5 ee\n");
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].path, "a");
    }
}
