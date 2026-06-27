//! Parsing of `make.conf` and `make.defaults` shell-style assignment files.
//!
//! Supports `KEY=value`, single and double quoting, backslash line
//! continuation, comments, and `$VAR` / `${VAR}` expansion against the
//! accumulating variable set. A directory path is read as its sorted member
//! files applied in filename order.

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::ConfigError;

/// A set of shell-style variable assignments, in the order they were defined.
#[derive(Debug, Default, Clone)]
pub struct VarMap {
    vars: BTreeMap<String, String>,
}

impl VarMap {
    /// Create an empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// The value of `key`, if set.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// All variables.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.vars.iter()
    }

    /// Insert or replace a variable directly.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(key.into(), value.into());
    }

    /// Remove a variable, returning its previous value if it was set.
    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.vars.remove(key)
    }

    /// Merge a single assignment, stacking incremental variables onto the current
    /// value and replacing non-incremental ones, as parsing a line would.
    pub fn merge_var(&mut self, key: &str, value: &str) {
        if self.is_incremental(key, false) {
            let merged = stack_incremental(self.vars.get(key), value);
            self.vars.insert(key.to_owned(), merged);
        } else {
            self.vars.insert(key.to_owned(), value.to_owned());
        }
    }

    /// Parse the contents of one assignment file, merging into this map so that
    /// later assignments override and expansion sees earlier values.
    pub fn merge_str(&mut self, content: &str, path: &Path) -> Result<(), ConfigError> {
        self.merge_str_layered(content, path, false)
    }

    /// Parse one assignment file's contents. When `user_layer` is set, a
    /// USE_EXPAND value variable is treated as non-incremental so a `make.conf`
    /// assignment replaces the profile-accumulated value instead of stacking
    /// onto it.
    fn merge_str_layered(
        &mut self,
        content: &str,
        path: &Path,
        user_layer: bool,
    ) -> Result<(), ConfigError> {
        let joined = join_continuations(content);
        for raw_line in joined.lines() {
            let line = raw_line.trim_start();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, rest)) = line.split_once('=') else {
                return Err(ConfigError::MakeConf {
                    path: path.to_path_buf(),
                    reason: "expected KEY=value assignment",
                });
            };
            let key = key.trim();
            if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(ConfigError::MakeConf {
                    path: path.to_path_buf(),
                    reason: "invalid variable name",
                });
            }
            let value = parse_value(rest, &self.vars);
            if self.is_incremental(key, user_layer) {
                // Incremental variables (USE, ACCEPT_KEYWORDS, USE_EXPAND, the
                // USE_EXPAND value vars, ...) accumulate across the profile
                // cascade and make.conf: each token adds, `-token` removes, and
                // `-*` clears, rather than the whole value being replaced.
                let merged = stack_incremental(self.vars.get(key), &value);
                self.vars.insert(key.to_owned(), merged);
            } else {
                self.vars.insert(key.to_owned(), value);
            }
        }
        Ok(())
    }

    /// Whether `key` stacks incrementally in the given layer.
    ///
    /// The fixed core set is always incremental. A USE_EXPAND value variable (any
    /// variable named in the current `USE_EXPAND` list, such as `PYTHON_TARGETS`)
    /// accumulates across `make.globals` and the profile `make.defaults` cascade,
    /// but is non-incremental in the user `make.conf` layer so a `make.conf`
    /// assignment replaces the profile-accumulated value, mirroring Portage
    /// omitting these variables from `INCREMENTALS`.
    fn is_incremental(&self, key: &str, user_layer: bool) -> bool {
        const CORE: &[&str] = &[
            "USE",
            "USE_EXPAND",
            "USE_EXPAND_HIDDEN",
            "USE_EXPAND_IMPLICIT",
            "USE_EXPAND_UNPREFIXED",
            "IUSE_IMPLICIT",
            "CONFIG_PROTECT",
            "CONFIG_PROTECT_MASK",
            "FEATURES",
            "ACCEPT_KEYWORDS",
            "ACCEPT_LICENSE",
            "ACCEPT_PROPERTIES",
            "ACCEPT_RESTRICT",
            "PROFILE_ONLY_VARIABLES",
            "ENV_UNSET",
        ];
        if CORE.contains(&key) {
            return true;
        }
        if user_layer {
            return false;
        }
        self.vars
            .get("USE_EXPAND")
            .map(|ue| ue.split_whitespace().any(|v| v == key))
            .unwrap_or(false)
    }

    /// Parse a file or directory path into this map.
    pub fn merge_path(&mut self, path: &Path) -> Result<(), ConfigError> {
        self.merge_path_layered(path, false)
    }

    /// Parse a `make.conf`-layer file or directory into this map: like
    /// [`merge_path`](Self::merge_path), but a USE_EXPAND value variable
    /// assignment replaces the profile-accumulated value instead of stacking
    /// onto it, mirroring Portage omitting these variables from `INCREMENTALS`
    /// and clearing the profile's prefixed flags before applying `make.conf`.
    pub fn merge_conf(&mut self, path: &Path) -> Result<(), ConfigError> {
        self.merge_path_layered(path, true)
    }

    fn merge_path_layered(&mut self, path: &Path, user_layer: bool) -> Result<(), ConfigError> {
        if path.is_dir() {
            // Skip files whose name starts with `.` (CONFIG_PROTECT merge
            // artifacts and other hidden files), matching Portage.
            let mut entries: Vec<_> = std::fs::read_dir(path)
                .map_err(|_| ConfigError::Io {
                    path: path.to_path_buf(),
                })?
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| !n.starts_with('.'))
                            .unwrap_or(false)
                })
                .collect();
            entries.sort();
            for entry in entries {
                self.merge_file_layered(&entry, user_layer)?;
            }
            Ok(())
        } else {
            self.merge_file_layered(path, user_layer)
        }
    }

    fn merge_file_layered(&mut self, path: &Path, user_layer: bool) -> Result<(), ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|_| ConfigError::Io {
            path: path.to_path_buf(),
        })?;
        self.merge_str_layered(&content, path, user_layer)
    }
}

/// Fold physical lines into logical assignment lines: a trailing backslash is a
/// continuation, and a newline inside an open quote becomes a space so a quoted
/// value may span several lines (as `make.globals` and `make.conf` both do).
///
/// Comments (an unquoted `#` at the start of a token through end of line) are
/// skipped without tracking quotes, so an apostrophe in a comment such as
/// `# the user's session` does not open a spurious string that swallows the
/// following assignment lines.
fn join_continuations(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut in_comment = false;
    // A `#` only begins a comment at the start of a token (line start or after
    // whitespace), matching `parse_value`'s comment rule.
    let mut prev_ws = true;
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if in_comment {
            if c == '\n' {
                in_comment = false;
                prev_ws = true;
                out.push('\n');
            }
            continue;
        }
        match c {
            '\\' if !in_single && chars.peek() == Some(&'\n') => {
                // Backslash line continuation: drop both characters.
                chars.next();
                prev_ws = true;
            }
            '\\' if !in_single => {
                // A backslash escape; keep it for `parse_value` to interpret.
                out.push('\\');
                if let Some(next) = chars.next() {
                    out.push(next);
                }
                prev_ws = false;
            }
            '#' if !in_single && !in_double && prev_ws => {
                in_comment = true;
            }
            '\'' if !in_double => {
                in_single = !in_single;
                out.push(c);
                prev_ws = false;
            }
            '"' if !in_single => {
                in_double = !in_double;
                out.push(c);
                prev_ws = false;
            }
            '\n' if in_single || in_double => {
                out.push(' ');
                prev_ws = true;
            }
            c => {
                out.push(c);
                prev_ws = c.is_whitespace();
            }
        }
    }
    out
}

/// Apply an incremental assignment's tokens onto the accumulated value,
/// preserving the sign of the most recent occurrence of each token. A plain
/// token enables, `-token` disables, and `-*` clears. The sign is kept (rather
/// than resolved here) because an incremental variable is consumed in signed
/// form: a `-flag` for a flag not yet present must survive so a later reader can
/// see it was disabled, not silently drop it.
fn stack_incremental(current: Option<&String>, value: &str) -> String {
    let mut acc: Vec<String> = current
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    for token in value.split_whitespace() {
        if token == "-*" {
            acc.clear();
            acc.push(token.to_owned());
            continue;
        }
        let name = token.strip_prefix('-').unwrap_or(token);
        acc.retain(|t| t.strip_prefix('-').unwrap_or(t) != name);
        acc.push(token.to_owned());
    }
    acc.join(" ")
}

fn parse_value(rest: &str, vars: &BTreeMap<String, String>) -> String {
    let chars: Vec<char> = rest.trim_start().chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut prev_ws = true;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    out.push(chars[i]);
                    i += 1;
                }
                i += 1;
                prev_ws = false;
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(chars[i + 1]);
                        i += 2;
                    } else if chars[i] == '$' {
                        i += 1;
                        out.push_str(&expand(&chars, &mut i, vars));
                    } else {
                        out.push(chars[i]);
                        i += 1;
                    }
                }
                i += 1;
                prev_ws = false;
            }
            '$' => {
                i += 1;
                out.push_str(&expand(&chars, &mut i, vars));
                prev_ws = false;
            }
            '\\' if i + 1 < chars.len() => {
                out.push(chars[i + 1]);
                i += 2;
                prev_ws = false;
            }
            '#' if prev_ws => break,
            c if c.is_whitespace() => {
                out.push(c);
                i += 1;
                prev_ws = true;
            }
            c => {
                out.push(c);
                i += 1;
                prev_ws = false;
            }
        }
    }
    out.trim().to_owned()
}

fn expand(chars: &[char], i: &mut usize, vars: &BTreeMap<String, String>) -> String {
    let mut name = String::new();
    if *i < chars.len() && chars[*i] == '{' {
        *i += 1;
        while *i < chars.len() && chars[*i] != '}' {
            name.push(chars[*i]);
            *i += 1;
        }
        *i += 1;
    } else {
        while *i < chars.len() && (chars[*i].is_ascii_alphanumeric() || chars[*i] == '_') {
            name.push(chars[*i]);
            *i += 1;
        }
    }
    vars.get(&name).cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> VarMap {
        let mut m = VarMap::new();
        m.merge_str(content, Path::new("make.conf")).unwrap();
        m
    }

    #[test]
    fn quoted_multi_token() {
        let m = parse("USE=\"a b c\"\n");
        assert_eq!(m.get("USE"), Some("a b c"));
    }

    #[test]
    fn variable_expansion() {
        let m = parse("A=\"x\"\nB=\"${A}y\"\nC=\"$A z\"\n");
        assert_eq!(m.get("B"), Some("xy"));
        assert_eq!(m.get("C"), Some("x z"));
    }

    #[test]
    fn line_continuation_and_comments() {
        let m = parse("# comment\nUSE=\"a \\\nb\"\n");
        assert_eq!(m.get("USE"), Some("a b"));
    }

    #[test]
    fn apostrophe_in_comment_does_not_swallow_assignments() {
        // A comment apostrophe must not open a string that consumes the
        // following assignment lines (the base/make.defaults bug).
        let m = parse(
            "# avoid the user's session\nUSE=\"acl unicode\"\n# another's note\nARCH=\"amd64\"\n",
        );
        assert_eq!(m.get("USE"), Some("acl unicode"));
        assert_eq!(m.get("ARCH"), Some("amd64"));
    }

    #[test]
    fn quotes_inside_a_comment_are_ignored() {
        // A `#` comment containing `USE="..."` must not affect the real USE.
        let m = parse("# stage1 breaks because of USE=\"-* foo\"\nUSE=\"acl\"\n");
        assert_eq!(m.get("USE"), Some("acl"));
    }

    #[test]
    fn incremental_use_preserves_negatives() {
        // A `-flag` for a flag not present in the accumulated value must survive
        // so a later signed reader (global USE resolution) sees it disabled,
        // rather than silently dropping it.
        let m = parse("USE=\"acl unicode\"\nUSE=\"-man bluetooth\"\n");
        assert_eq!(m.get("USE"), Some("acl unicode -man bluetooth"));
    }

    #[test]
    fn incremental_use_last_sign_wins() {
        // Re-enabling a previously disabled flag drops the negative, and vice
        // versa: only the most recent occurrence of a flag is kept.
        let m = parse("USE=\"-foo bar\"\nUSE=\"foo -bar\"\n");
        assert_eq!(m.get("USE"), Some("foo -bar"));
    }

    #[test]
    fn make_conf_replaces_use_expand_value_but_cascade_unions() {
        let dir = tempfile::tempdir().unwrap();
        // make.globals declares which variables hold USE_EXPAND values.
        let globals = dir.path().join("make.globals");
        std::fs::write(&globals, "USE_EXPAND=\"PYTHON_TARGETS VIDEO_CARDS\"\n").unwrap();
        // Two profile make.defaults layers contribute VIDEO_CARDS (which union)
        // and set the profile PYTHON_TARGETS.
        let defaults1 = dir.path().join("defaults1");
        std::fs::write(
            &defaults1,
            "VIDEO_CARDS=\"amdgpu\"\nPYTHON_TARGETS=\"python3_11 python3_12\"\n",
        )
        .unwrap();
        let defaults2 = dir.path().join("defaults2");
        std::fs::write(&defaults2, "VIDEO_CARDS=\"fbdev\"\n").unwrap();
        // make.conf replaces the profile-accumulated PYTHON_TARGETS.
        let conf = dir.path().join("make.conf");
        std::fs::write(&conf, "PYTHON_TARGETS=\"python3_13\"\n").unwrap();

        let mut env = VarMap::new();
        env.merge_path(&globals).unwrap();
        env.merge_path(&defaults1).unwrap();
        env.merge_path(&defaults2).unwrap();
        env.merge_conf(&conf).unwrap();

        // The make.conf value wins outright, not stacked onto the profile.
        assert_eq!(env.get("PYTHON_TARGETS"), Some("python3_13"));
        // The two profile-cascade layers of VIDEO_CARDS union.
        let vc: Vec<&str> = env.get("VIDEO_CARDS").unwrap().split_whitespace().collect();
        assert!(vc.contains(&"amdgpu") && vc.contains(&"fbdev"));
    }

    #[test]
    fn quoted_value_spans_multiple_lines() {
        let m = parse("FEATURES=\"a b\n          c d\"\n");
        assert_eq!(
            m.get("FEATURES")
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"]
        );
    }
}
