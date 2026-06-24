//! Gentoo version parsing and comparison.
//!
//! [`Version`] parses a Gentoo version string into a value carrying a
//! precomputed comparison key, so ordering matches stock Portage `vercmp`
//! (`lib/portage/versions.py`) without reparsing or allocating at comparison
//! time. Syntax follows PMS: numeric dot-separated release components, an
//! optional single trailing letter, a chain of `_alpha`/`_beta`/`_pre`/`_rc`/
//! `_p` suffixes with optional numbers, and an optional `-rN` revision.

use std::cmp::Ordering;
use std::fmt;

/// An error produced while parsing a version string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid version `{input}`: {reason}")]
pub struct VersionError {
    /// The input that failed to parse.
    pub input: String,
    /// A short description of why it failed.
    pub reason: &'static str,
}

/// Suffix kinds with their stock `suffix_value` ranking.
fn suffix_value(name: &str) -> Option<i8> {
    match name {
        "alpha" => Some(-4),
        "beta" => Some(-3),
        "pre" => Some(-2),
        "rc" => Some(-1),
        "p" => Some(0),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct Component {
    /// The raw digit string, kept for rendering (preserves leading zeros).
    raw: Box<str>,
    /// Integer value ignoring leading zeros, used in integer-comparison mode.
    int_val: i128,
    /// Whether the first digit is `0`, which triggers fractional comparison.
    lz: bool,
    /// Length of the trailing-zero-stripped prefix of `raw`, used as the
    /// fractional comparison key.
    frac_len: usize,
}

impl Component {
    fn frac(&self) -> &[u8] {
        &self.raw.as_bytes()[..self.frac_len]
    }
}

/// A parsed Gentoo version with a precomputed comparison key.
#[derive(Debug, Clone)]
pub struct Version {
    raw: Box<str>,
    first: i128,
    dotted: Box<[Component]>,
    letter: Option<u8>,
    /// `(suffix_value, number)` pairs in order.
    suffixes: Box<[(i8, i64)]>,
    revision: u32,
}

impl Version {
    /// Parse a Gentoo version string.
    pub fn parse(input: &str) -> Result<Version, VersionError> {
        let err = |reason: &'static str| VersionError {
            input: input.to_owned(),
            reason,
        };
        let bytes = input.as_bytes();
        let mut i = 0usize;

        // First numeric component.
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return Err(err("expected a leading numeric component"));
        }
        let first: i128 = input[start..i]
            .parse()
            .map_err(|_| err("numeric component out of range"))?;

        // Dotted components.
        let mut dotted: Vec<Component> = Vec::new();
        while i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            let cstart = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i == cstart {
                return Err(err("expected digits after `.`"));
            }
            dotted.push(make_component(&input[cstart..i], &err)?);
        }

        // Optional single trailing letter.
        let mut letter = None;
        if i < bytes.len() && bytes[i].is_ascii_lowercase() {
            letter = Some(bytes[i]);
            i += 1;
        }

        // Suffix chain.
        let mut suffixes: Vec<(i8, i64)> = Vec::new();
        while i < bytes.len() && bytes[i] == b'_' {
            i += 1;
            let nstart = i;
            while i < bytes.len() && bytes[i].is_ascii_lowercase() {
                i += 1;
            }
            let name = &input[nstart..i];
            let value = suffix_value(name).ok_or_else(|| err("unknown version suffix"))?;
            let dstart = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let number: i64 = if dstart == i {
                0
            } else {
                input[dstart..i]
                    .parse()
                    .map_err(|_| err("suffix number out of range"))?
            };
            suffixes.push((value, number));
        }

        // Optional revision.
        let mut revision = 0u32;
        if i < bytes.len() && bytes[i] == b'-' {
            if input[i..].starts_with("-r") {
                i += 2;
                let rstart = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if rstart == i {
                    return Err(err("expected digits after `-r`"));
                }
                revision = input[rstart..i]
                    .parse()
                    .map_err(|_| err("revision out of range"))?;
            } else {
                return Err(err("unexpected `-` in version"));
            }
        }

        if i != bytes.len() {
            return Err(err("trailing characters after version"));
        }

        Ok(Version {
            raw: input.into(),
            first,
            dotted: dotted.into_boxed_slice(),
            letter,
            suffixes: suffixes.into_boxed_slice(),
            revision,
        })
    }

    /// The original version string.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The revision number (`-rN`), defaulting to zero.
    pub fn revision(&self) -> u32 {
        self.revision
    }

    /// Compare two versions ignoring the revision, used by the `~` atom
    /// operator which matches any revision of a version.
    pub fn cmp_ignoring_revision(&self, other: &Version) -> Ordering {
        self.cmp_release(other)
            .then_with(|| cmp_suffixes(&self.suffixes, &other.suffixes))
    }

    /// Whether this version equals `other` ignoring the revision.
    pub fn matches_any_revision(&self, other: &Version) -> bool {
        self.cmp_ignoring_revision(other) == Ordering::Equal
    }

    fn cmp_release(&self, other: &Version) -> Ordering {
        self.first
            .cmp(&other.first)
            .then_with(|| cmp_dotted(&self.dotted, &other.dotted))
            .then_with(|| cmp_letter(self.letter, other.letter))
    }
}

fn make_component(
    raw: &str,
    err: &impl Fn(&'static str) -> VersionError,
) -> Result<Component, VersionError> {
    let int_val: i128 = raw
        .parse()
        .map_err(|_| err("numeric component out of range"))?;
    let lz = raw.as_bytes().first() == Some(&b'0');
    let frac_len = raw.trim_end_matches('0').len();
    Ok(Component {
        raw: raw.into(),
        int_val,
        lz,
        frac_len,
    })
}

fn cmp_dotted(a: &[Component], b: &[Component]) -> Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        let ord = match (a.get(i), b.get(i)) {
            (Some(x), Some(y)) => {
                if x.lz || y.lz {
                    x.frac().cmp(y.frac())
                } else {
                    x.int_val.cmp(&y.int_val)
                }
            }
            // A missing component is the implicit `.0` valued at -1.
            (Some(x), None) => x.int_val.cmp(&-1),
            (None, Some(y)) => (-1i128).cmp(&y.int_val),
            (None, None) => Ordering::Equal,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn cmp_letter(a: Option<u8>, b: Option<u8>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

fn cmp_suffixes(a: &[(i8, i64)], b: &[(i8, i64)]) -> Ordering {
    let n = a.len().max(b.len());
    for i in 0..n {
        // A missing suffix is the implicit `_p` with number -1.
        let (ak, an) = a.get(i).copied().unwrap_or((0, -1));
        let (bk, bn) = b.get(i).copied().unwrap_or((0, -1));
        let ord = ak.cmp(&bk).then(an.cmp(&bn));
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

impl Ord for Version {
    fn cmp(&self, other: &Version) -> Ordering {
        self.cmp_release(other)
            .then_with(|| cmp_suffixes(&self.suffixes, &other.suffixes))
            .then_with(|| self.revision.cmp(&other.revision))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Version) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.first)?;
        for c in &self.dotted {
            write!(f, ".{}", c.raw)?;
        }
        if let Some(letter) = self.letter {
            write!(f, "{}", letter as char)?;
        }
        for (value, number) in &self.suffixes {
            let name = match value {
                -4 => "alpha",
                -3 => "beta",
                -2 => "pre",
                -1 => "rc",
                _ => "p",
            };
            write!(f, "_{name}")?;
            if *number != 0 {
                write!(f, "{number}")?;
            }
        }
        if self.revision != 0 {
            write!(f, "-r{}", self.revision)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parses_full_version() {
        let parsed = v("1.2.3b_alpha4-r2");
        assert_eq!(parsed.revision(), 2);
        assert_eq!(parsed.to_string(), "1.2.3b_alpha4-r2");
    }

    #[test]
    fn missing_revision_equals_r0() {
        assert_eq!(v("1.0"), v("1.0-r0"));
    }

    #[test]
    fn suffix_ranking() {
        assert!(v("1.0_rc1") < v("1.0"));
        assert!(v("1.0_p1") > v("1.0"));
        assert!(v("1.0_alpha") < v("1.0_beta"));
        assert!(v("1.0_beta") < v("1.0_pre"));
        assert!(v("1.0_pre") < v("1.0_rc"));
        assert!(v("1.0_rc") < v("1.0"));
    }

    #[test]
    fn leading_zero_component_is_fractional() {
        // 1.01 < 1.1 because the leading zero triggers fractional comparison.
        assert!(v("1.01") < v("1.1"));
        // 1.1 < 1.02? No: 0.1 > 0.02.
        assert!(v("1.1") > v("1.02"));
    }

    #[test]
    fn revision_is_least_significant() {
        assert!(v("1.2-r3") > v("1.2-r1"));
        assert!(v("1.3") > v("1.2-r3"));
    }

    #[test]
    fn letter_orders_above_plain() {
        assert!(v("1.2b") > v("1.2"));
        assert!(v("1.2b") < v("1.2c"));
        // Behavior change: 12.2.5 > 12.2b.
        assert!(v("12.2.5") > v("12.2b"));
    }

    #[test]
    fn implicit_trailing_component() {
        assert!(v("1.0.0") > v("1.0"));
        assert!(v("1") < v("1.0"));
    }

    #[test]
    fn golden_sorted_order_matches_vercmp() {
        let ordered = [
            "1",
            "1.0_alpha",
            "1.0_alpha1",
            "1.0_beta",
            "1.0_pre",
            "1.0_rc",
            "1.0_rc1",
            "1.0",
            "1.0_p",
            "1.0_p1",
            "1.0.0",
            "1.01",
            "1.1",
            "1.2_alpha",
            "1.2",
            "1.2-r1",
            "1.2.3",
            "2",
        ];
        for win in ordered.windows(2) {
            let a = v(win[0]);
            let b = v(win[1]);
            assert!(a < b, "expected {} < {}", win[0], win[1]);
        }
    }
}
