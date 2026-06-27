//! A minimal `fnmatch`-style glob matcher.
//!
//! Implements the subset of shell globbing that Portage relies on through
//! Python's `fnmatch`: `*` (any run, separators included), `?` (any single
//! character), and `[seq]`/`[!seq]` character classes including `a-z` ranges.
//! Used for `COLLISION_IGNORE`, `UNINSTALL_IGNORE`, and `STRIP_MASK` matching.

/// Whether `text` is matched by the `fnmatch`-style pattern `pat`.
///
/// Supports `*` (any run, separators included), `?` (any single character), and
/// `[seq]`/`[!seq]` character classes including `a-z` ranges, matching the
/// patterns Python's `fnmatch.translate` matches. An unterminated `[` is treated
/// as a literal `[`, as Python does.
pub fn fnmatch(text: &str, pat: &str) -> bool {
    let (t, p) = (text.as_bytes(), pat.as_bytes());
    // Iterative backtracking match.
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star_p, mut star_t): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
            continue;
        }
        // The pattern position past a single matched unit, or `None` on mismatch.
        let next = if pi < p.len() {
            match p[pi] {
                b'?' => Some(pi + 1),
                b'[' => match match_class(p, pi, t[ti]) {
                    Some((true, end)) => Some(end),
                    Some((false, _)) => None,
                    // An unterminated `[` is a literal `[`.
                    None => (p[pi] == t[ti]).then_some(pi + 1),
                },
                c => (c == t[ti]).then_some(pi + 1),
            }
        } else {
            None
        };
        match next {
            Some(np) => {
                pi = np;
                ti += 1;
            }
            None => match star_p {
                Some(sp) => {
                    pi = sp + 1;
                    star_t += 1;
                    ti = star_t;
                }
                None => return false,
            },
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Match a `[...]` character class in `pat` at `pi` (the opening `[`) against
/// `ch`. Returns whether `ch` matches and the index just past the closing `]`, or
/// `None` when the class is unterminated so the caller treats `[` as a literal. A
/// leading `!` or `^` negates; a `]` immediately after the opening (or negation)
/// is a literal member; `a-z` denotes an inclusive range.
fn match_class(pat: &[u8], pi: usize, ch: u8) -> Option<(bool, usize)> {
    let mut j = pi + 1;
    let negate = matches!(pat.get(j), Some(b'!') | Some(b'^'));
    if negate {
        j += 1;
    }
    let start = j;
    let mut matched = false;
    loop {
        match pat.get(j) {
            // Unterminated class.
            None => return None,
            // A closing `]` after at least one member ends the class; a `]` in the
            // first position is a literal member instead.
            Some(b']') if j > start => break,
            Some(&c) => {
                if pat.get(j + 1) == Some(&b'-') && pat.get(j + 2).is_some_and(|&e| e != b']') {
                    let end = pat[j + 2];
                    if c <= ch && ch <= end {
                        matched = true;
                    }
                    j += 3;
                } else {
                    if c == ch {
                        matched = true;
                    }
                    j += 1;
                }
            }
        }
    }
    Some((matched ^ negate, j + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnmatch_character_classes() {
        assert!(fnmatch("a", "[abc]"));
        assert!(!fnmatch("d", "[abc]"));
        assert!(fnmatch("d", "[!abc]"));
        assert!(!fnmatch("a", "[!abc]"));
        assert!(fnmatch("d", "[^abc]"));
        assert!(fnmatch("5", "[0-9]"));
        assert!(!fnmatch("x", "[0-9]"));
        assert!(fnmatch("]", "[]a]"));
        assert!(fnmatch("/usr/lib/libfoo.so.1", "/usr/lib/*.so.[0-9]*"));
        assert!(!fnmatch("/usr/lib/libfoo.so.x", "/usr/lib/*.so.[0-9]*"));
        // An unterminated `[` is a literal.
        assert!(fnmatch("a[b", "a[b"));
        assert!(!fnmatch("ab", "a[b"));
    }
}
