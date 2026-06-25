//! USE-conditional reduction of a `LICENSE` dependency string.
//!
//! `LICENSE` uses the dependency grammar (`token`, `flag? ( ... )`, `|| ( ... )`,
//! nested `( ... )`) but its leaves are license tokens rather than atoms, so it
//! needs its own small reducer. Conditionals are resolved against the package's
//! enabled USE here, producing a [`LicenseReq`] tree of only all-of and any-of
//! groups over token leaves, which the license manager then evaluates.

use std::collections::BTreeSet;

use moraine_config::LicenseReq;

/// Reduce a raw `LICENSE` string against `use_set` into a [`LicenseReq`] tree
/// with all USE-conditional groups resolved.
pub fn reduce_license(input: &str, use_set: &BTreeSet<String>) -> LicenseReq {
    let tokens = tokenize(input);
    let mut pos = 0;
    LicenseReq::AllOf(parse_seq(&tokens, &mut pos, use_set))
}

fn tokenize(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in input.split_whitespace() {
        let mut cur = String::new();
        for ch in raw.chars() {
            if ch == '(' || ch == ')' {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(ch.to_string());
            } else {
                cur.push(ch);
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

fn parse_seq(tokens: &[String], pos: &mut usize, use_set: &BTreeSet<String>) -> Vec<LicenseReq> {
    let mut nodes = Vec::new();
    while *pos < tokens.len() {
        let tok = &tokens[*pos];
        if tok == ")" {
            break;
        }
        if tok == "||" {
            *pos += 1;
            consume_open(tokens, pos);
            let body = parse_seq(tokens, pos, use_set);
            consume_close(tokens, pos);
            nodes.push(LicenseReq::AnyOf(body));
            continue;
        }
        if let Some(cond) = tok.strip_suffix('?') {
            let (flag, sense) = match cond.strip_prefix('!') {
                Some(f) => (f, false),
                None => (cond, true),
            };
            *pos += 1;
            consume_open(tokens, pos);
            let body = parse_seq(tokens, pos, use_set);
            consume_close(tokens, pos);
            // Resolve the conditional now: an active group contributes its body,
            // an inactive one contributes nothing.
            if use_set.contains(flag) == sense {
                nodes.extend(body);
            }
            continue;
        }
        if tok == "(" {
            *pos += 1;
            let body = parse_seq(tokens, pos, use_set);
            consume_close(tokens, pos);
            nodes.push(LicenseReq::AllOf(body));
            continue;
        }
        nodes.push(LicenseReq::Token(tok.clone()));
        *pos += 1;
    }
    nodes
}

fn consume_open(tokens: &[String], pos: &mut usize) {
    if tokens.get(*pos).map(String::as_str) == Some("(") {
        *pos += 1;
    }
}

fn consume_close(tokens: &[String], pos: &mut usize) {
    if tokens.get(*pos).map(String::as_str) == Some(")") {
        *pos += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uses(flags: &[&str]) -> BTreeSet<String> {
        flags.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_tokens_are_all_of() {
        let r = reduce_license("GPL-2 BSD", &uses(&[]));
        assert_eq!(
            r,
            LicenseReq::AllOf(vec![
                LicenseReq::Token("GPL-2".to_owned()),
                LicenseReq::Token("BSD".to_owned()),
            ])
        );
    }

    #[test]
    fn conditional_included_only_when_active() {
        let active = reduce_license("base flag? ( extra )", &uses(&["flag"]));
        assert_eq!(
            active,
            LicenseReq::AllOf(vec![
                LicenseReq::Token("base".to_owned()),
                LicenseReq::Token("extra".to_owned()),
            ])
        );
        let inactive = reduce_license("base flag? ( extra )", &uses(&[]));
        assert_eq!(
            inactive,
            LicenseReq::AllOf(vec![LicenseReq::Token("base".to_owned())])
        );
    }

    #[test]
    fn any_of_preserved() {
        let r = reduce_license("|| ( GPL-2 BSD )", &uses(&[]));
        assert_eq!(
            r,
            LicenseReq::AllOf(vec![LicenseReq::AnyOf(vec![
                LicenseReq::Token("GPL-2".to_owned()),
                LicenseReq::Token("BSD".to_owned()),
            ])])
        );
    }
}
