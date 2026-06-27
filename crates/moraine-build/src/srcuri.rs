//! `SRC_URI` parsing, USE reduction, and distfile mapping.
//!
//! `moraine-atom` parses dependency strings but not `SRC_URI`, whose grammar
//! differs (bare URIs, `->` destination arrows, and EAPI 8 `fetch+`/`mirror+`
//! grant-override prefixes). This module implements a small dedicated parser and
//! reducer with its own AST, gating the arrow and grant-override syntax on the
//! active EAPI's feature table from [`moraine_eapi`].
//!
//! The reduced result is a `{distfile -> DistFile}` table from which `A` (the
//! USE-filtered set) and `AA` (the full set) are derived.

use std::collections::BTreeMap;
use std::collections::HashSet;

use moraine_eapi::EapiFeatures;
use tracing::instrument;

use crate::error::{BuildError, Result};

/// A node in the parsed `SRC_URI` tree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    /// A USE-conditional group. `sense` is true for `flag?`, false for `!flag?`.
    Conditional {
        flag: String,
        sense: bool,
        body: Vec<Node>,
    },
    /// A single source URI with an optional explicit destination name and
    /// per-file fetch and mirror grant overrides.
    Uri {
        uri: String,
        dest: Option<String>,
        fetch_override: bool,
        mirror_override: bool,
    },
}

/// One distfile and the URIs from which it may be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistFile {
    /// The destination filename in the distdir.
    pub name: String,
    /// The candidate source URIs, in priority order.
    pub uris: Vec<String>,
    /// Whether this file carries a fetch grant override, set by an EAPI 8
    /// `fetch+` (or `mirror+`) prefix on any of its URIs. The override permits
    /// fetching the URI even under `RESTRICT=fetch`.
    pub fetch_override: bool,
    /// Whether this file carries a mirror grant override, set by an EAPI 8
    /// `mirror+` prefix on any of its URIs. The override keeps the file
    /// mirrorable even under `RESTRICT=mirror`.
    pub mirror_override: bool,
}

/// The result of reducing `SRC_URI` against a USE set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrcUriMap {
    files: BTreeMap<String, DistFile>,
    all_files: BTreeMap<String, DistFile>,
}

impl SrcUriMap {
    /// The USE-filtered distfiles, the `A` set, sorted by name.
    pub fn a(&self) -> Vec<&DistFile> {
        self.files.values().collect()
    }

    /// The unfiltered distfiles, the `AA` set, sorted by name.
    pub fn aa(&self) -> Vec<&DistFile> {
        self.all_files.values().collect()
    }

    /// The space-separated `A` variable value.
    pub fn a_string(&self) -> String {
        self.files.keys().cloned().collect::<Vec<_>>().join(" ")
    }

    /// The space-separated `AA` variable value.
    pub fn aa_string(&self) -> String {
        self.all_files.keys().cloned().collect::<Vec<_>>().join(" ")
    }

    /// Look up a distfile by destination name in the `A` set.
    pub fn get(&self, name: &str) -> Option<&DistFile> {
        self.files.get(name)
    }
}

/// Parse and reduce a `SRC_URI` value against a USE set, producing the distfile
/// map. Arrow and selective-restriction syntax are rejected for EAPIs that do
/// not define them.
#[instrument(name = "src_uri_map", skip_all)]
pub fn parse_and_reduce(
    src_uri: &str,
    use_set: &HashSet<String>,
    features: EapiFeatures,
) -> Result<SrcUriMap> {
    let nodes = parse(src_uri, features)?;
    let mut files = BTreeMap::new();
    let mut all = BTreeMap::new();
    collect(&nodes, use_set, &mut files);
    // AA ignores USE conditionals: every flag is treated as both on and off, so
    // the full set is collected by descending into every conditional branch.
    collect_all(&nodes, &mut all);
    Ok(SrcUriMap {
        files,
        all_files: all,
    })
}

/// Tokenize and parse a `SRC_URI` string into the node tree.
fn parse(src_uri: &str, features: EapiFeatures) -> Result<Vec<Node>> {
    let tokens: Vec<&str> = src_uri.split_whitespace().collect();
    let mut pos = 0;
    let nodes = parse_seq(&tokens, &mut pos, features, false)?;
    if pos != tokens.len() {
        return Err(BuildError::src_uri("unbalanced ')' in SRC_URI"));
    }
    Ok(nodes)
}

fn parse_seq(
    tokens: &[&str],
    pos: &mut usize,
    features: EapiFeatures,
    nested: bool,
) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    while *pos < tokens.len() {
        let tok = tokens[*pos];
        match tok {
            ")" => {
                if !nested {
                    return Err(BuildError::src_uri("unexpected ')' in SRC_URI"));
                }
                *pos += 1;
                return Ok(nodes);
            }
            "(" => {
                return Err(BuildError::src_uri("unexpected '(' without a conditional"));
            }
            "->" => {
                return Err(BuildError::src_uri("'->' without a preceding URI"));
            }
            _ if tok.ends_with('?') => {
                let (flag, sense) = parse_cond_flag(tok)?;
                *pos += 1;
                if tokens.get(*pos) != Some(&"(") {
                    return Err(BuildError::src_uri("conditional must be followed by '('"));
                }
                *pos += 1;
                let body = parse_seq(tokens, pos, features, true)?;
                nodes.push(Node::Conditional { flag, sense, body });
            }
            _ => {
                let node = parse_uri(tokens, pos, features)?;
                nodes.push(node);
            }
        }
    }
    if nested {
        return Err(BuildError::src_uri("unterminated conditional group"));
    }
    Ok(nodes)
}

fn parse_cond_flag(tok: &str) -> Result<(String, bool)> {
    let inner = tok
        .strip_suffix('?')
        .ok_or_else(|| BuildError::src_uri("malformed conditional token"))?;
    if let Some(flag) = inner.strip_prefix('!') {
        if flag.is_empty() {
            return Err(BuildError::src_uri("empty conditional flag"));
        }
        Ok((flag.to_string(), false))
    } else {
        if inner.is_empty() {
            return Err(BuildError::src_uri("empty conditional flag"));
        }
        Ok((inner.to_string(), true))
    }
}

fn parse_uri(tokens: &[&str], pos: &mut usize, features: EapiFeatures) -> Result<Node> {
    let raw = tokens[*pos];
    *pos += 1;

    let mut fetch_override = false;
    let mut mirror_override = false;
    let uri = if let Some(rest) = raw.strip_prefix("fetch+") {
        if !features.selective_src_uri_restriction {
            return Err(BuildError::src_uri(
                "fetch+ selective restriction requires EAPI 8 or later",
            ));
        }
        fetch_override = true;
        rest.to_string()
    } else if let Some(rest) = raw.strip_prefix("mirror+") {
        if !features.selective_src_uri_restriction {
            return Err(BuildError::src_uri(
                "mirror+ selective restriction requires EAPI 8 or later",
            ));
        }
        // mirror+ grants both the mirror override and the fetch override,
        // matching `override_fetch = override_mirror or startswith("fetch+")`.
        mirror_override = true;
        fetch_override = true;
        rest.to_string()
    } else {
        raw.to_string()
    };

    // Optional `-> dest` arrow.
    let dest = if tokens.get(*pos) == Some(&"->") {
        if !features.src_uri_arrows {
            return Err(BuildError::src_uri(
                "SRC_URI arrows require EAPI 2 or later",
            ));
        }
        *pos += 1;
        let name = tokens
            .get(*pos)
            .ok_or_else(|| BuildError::src_uri("'->' without a destination name"))?;
        if *name == ")" || *name == "(" || name.ends_with('?') {
            return Err(BuildError::src_uri("'->' destination is not a filename"));
        }
        // PMS 12.1.2: the arrow destination is a plain filename, never a path.
        if name.contains('/') {
            return Err(BuildError::src_uri(
                "'->' destination must not contain a path separator",
            ));
        }
        *pos += 1;
        Some((*name).to_string())
    } else {
        None
    };

    Ok(Node::Uri {
        uri,
        dest,
        fetch_override,
        mirror_override,
    })
}

fn collect(nodes: &[Node], use_set: &HashSet<String>, out: &mut BTreeMap<String, DistFile>) {
    for node in nodes {
        match node {
            Node::Conditional { flag, sense, body } => {
                if use_set.contains(flag) == *sense {
                    collect(body, use_set, out);
                }
            }
            Node::Uri { .. } => add_uri(node, out),
        }
    }
}

fn collect_all(nodes: &[Node], out: &mut BTreeMap<String, DistFile>) {
    for node in nodes {
        match node {
            Node::Conditional { body, .. } => collect_all(body, out),
            Node::Uri { .. } => add_uri(node, out),
        }
    }
}

fn add_uri(node: &Node, out: &mut BTreeMap<String, DistFile>) {
    let Node::Uri {
        uri,
        dest,
        fetch_override,
        mirror_override,
    } = node
    else {
        return;
    };
    let name = dest.clone().unwrap_or_else(|| basename(uri));
    let entry = out.entry(name.clone()).or_insert_with(|| DistFile {
        name,
        uris: Vec::new(),
        fetch_override: false,
        mirror_override: false,
    });
    if !entry.uris.contains(uri) {
        entry.uris.push(uri.clone());
    }
    entry.fetch_override |= fetch_override;
    entry.mirror_override |= mirror_override;
}

/// The trailing path component of a URI, used as the default distfile name.
fn basename(uri: &str) -> String {
    uri.rsplit('/').next().unwrap_or(uri).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn use_set(flags: &[&str]) -> HashSet<String> {
        flags.iter().map(|s| s.to_string()).collect()
    }

    fn eapi(n: u8) -> EapiFeatures {
        moraine_eapi::features_for_level(n)
    }

    #[test]
    fn plain_uri_uses_basename() {
        let m =
            parse_and_reduce("https://example.com/foo-1.tar.gz", &use_set(&[]), eapi(8)).unwrap();
        let f = m.get("foo-1.tar.gz").unwrap();
        assert_eq!(f.uris, vec!["https://example.com/foo-1.tar.gz"]);
    }

    #[test]
    fn arrow_sets_destination_name() {
        let m = parse_and_reduce(
            "https://example.com/download?id=5 -> foo-1.tar.gz",
            &use_set(&[]),
            eapi(8),
        )
        .unwrap();
        assert!(m.get("foo-1.tar.gz").is_some());
    }

    #[test]
    fn arrow_rejected_before_eapi_two() {
        let err = parse_and_reduce("https://example.com/a -> b", &use_set(&[]), eapi(0));
        assert!(matches!(err, Err(BuildError::SrcUri { .. })));
    }

    #[test]
    fn arrow_destination_with_path_separator_rejected() {
        let err = parse_and_reduce(
            "https://example.com/a -> sub/dir/foo.tar.gz",
            &use_set(&[]),
            eapi(8),
        );
        assert!(matches!(err, Err(BuildError::SrcUri { .. })));
    }

    #[test]
    fn selective_restriction_gated_at_eight() {
        let ok = parse_and_reduce(
            "fetch+https://example.com/foo.tar.gz",
            &use_set(&[]),
            eapi(8),
        )
        .unwrap();
        assert!(ok.get("foo.tar.gz").unwrap().fetch_override);

        let err = parse_and_reduce(
            "fetch+https://example.com/foo.tar.gz",
            &use_set(&[]),
            eapi(7),
        );
        assert!(matches!(err, Err(BuildError::SrcUri { .. })));
    }

    #[test]
    fn mirror_plus_sets_overrides() {
        let m = parse_and_reduce(
            "mirror+https://example.com/foo.tar.gz",
            &use_set(&[]),
            eapi(8),
        )
        .unwrap();
        let f = m.get("foo.tar.gz").unwrap();
        // mirror+ grants the mirror override and the fetch override.
        assert!(f.mirror_override);
        assert!(f.fetch_override);
    }

    #[test]
    fn a_and_aa_reflect_use_filtering() {
        let src = "https://e.com/base.tar.gz flag? ( https://e.com/extra.tar.gz )";
        let with = parse_and_reduce(src, &use_set(&["flag"]), eapi(8)).unwrap();
        assert!(with.get("base.tar.gz").is_some());
        assert!(with.get("extra.tar.gz").is_some());

        let without = parse_and_reduce(src, &use_set(&[]), eapi(8)).unwrap();
        assert!(without.get("base.tar.gz").is_some());
        assert!(without.get("extra.tar.gz").is_none());
        // AA always contains both.
        assert!(without.aa_string().contains("extra.tar.gz"));
        assert!(without.aa_string().contains("base.tar.gz"));
        assert!(!without.a_string().contains("extra.tar.gz"));
    }

    #[test]
    fn negated_conditional() {
        let src = "!flag? ( https://e.com/only-without.tar.gz )";
        let on = parse_and_reduce(src, &use_set(&["flag"]), eapi(8)).unwrap();
        assert!(on.get("only-without.tar.gz").is_none());
        let off = parse_and_reduce(src, &use_set(&[]), eapi(8)).unwrap();
        assert!(off.get("only-without.tar.gz").is_some());
    }

    #[test]
    fn nested_conditionals() {
        let src = "a? ( b? ( https://e.com/ab.tar.gz ) )";
        let both = parse_and_reduce(src, &use_set(&["a", "b"]), eapi(8)).unwrap();
        assert!(both.get("ab.tar.gz").is_some());
        let one = parse_and_reduce(src, &use_set(&["a"]), eapi(8)).unwrap();
        assert!(one.get("ab.tar.gz").is_none());
    }

    #[test]
    fn unbalanced_parens_rejected() {
        assert!(parse_and_reduce("flag? ( https://e.com/a", &use_set(&[]), eapi(8)).is_err());
        assert!(parse_and_reduce("https://e.com/a )", &use_set(&[]), eapi(8)).is_err());
    }

    #[test]
    fn duplicate_uris_for_same_dest_merge() {
        let src = "https://a.com/x.tar.gz https://b.com/x.tar.gz";
        let m = parse_and_reduce(src, &use_set(&[]), eapi(8)).unwrap();
        let f = m.get("x.tar.gz").unwrap();
        assert_eq!(f.uris.len(), 2);
    }
}
