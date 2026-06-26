//! Token-preserving rewrite of dependency strings for package moves.
//!
//! A `move` rewrites every dependency atom whose `category/package` equals the
//! old name, and a `slotmove` rewrites the slot constraint of matching atoms.
//! The rewrite operates on the verbatim recorded dependency string, replacing
//! only the affected substring of each atom token so surrounding structure
//! (operators, versions, USE deps, `||` groups, whitespace runs) survives, then
//! the caller re-parses the result so the AST stays in sync.

use moraine_common::Interner;
use moraine_eapi::EapiFeatures;

use crate::Atom;

/// Rewrite dependency atoms whose cp equals `old_cp` to `new_cp` in `raw`. A
/// blocker token whose rewrite would point at `self_cp` (the owning package) is
/// left untouched to avoid creating a self-blocker (bug #367215).
pub fn rewrite_dep_cp(
    raw: &str,
    old_cp: &str,
    new_cp: &str,
    self_cp: &str,
    features: EapiFeatures,
    interner: &Interner,
) -> String {
    rewrite_tokens(raw, |token| {
        let atom = parse_atom(token, features, interner)?;
        if cp_of(&atom, interner) != old_cp {
            return None;
        }
        if atom.blocker() != crate::Blocker::None && new_cp == self_cp {
            return None;
        }
        Some(token.replacen(old_cp, new_cp, 1))
    })
}

/// Rewrite the slot constraint from `old_slot` to `new_slot` on atoms whose cp
/// equals `atom_cp` and whose slot equals `old_slot`, in `raw`.
pub fn rewrite_dep_slot(
    raw: &str,
    atom_cp: &str,
    old_slot: &str,
    new_slot: &str,
    features: EapiFeatures,
    interner: &Interner,
) -> String {
    rewrite_tokens(raw, |token| {
        let atom = parse_atom(token, features, interner)?;
        if cp_of(&atom, interner) != atom_cp {
            return None;
        }
        let slot = atom.slot().and_then(|s| interner.resolve(s));
        if slot.as_deref() != Some(old_slot) {
            return None;
        }
        // Replace `:old_slot` where the slot ends at `/`, `=`, `[`, or token end.
        Some(replace_slot(token, old_slot, new_slot))
    })
}

/// Apply `f` to each whitespace-separated token of `raw`, substituting the token
/// when `f` returns `Some`, preserving the original whitespace runs.
fn rewrite_tokens(raw: &str, f: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while !rest.is_empty() {
        let ws_end = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        out.push_str(&rest[..ws_end]);
        rest = &rest[ws_end..];
        if rest.is_empty() {
            break;
        }
        let tok_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..tok_end];
        match f(token) {
            Some(replacement) => out.push_str(&replacement),
            None => out.push_str(token),
        }
        rest = &rest[tok_end..];
    }
    out
}

/// Replace a `:old_slot` constraint with `:new_slot`, keeping any sub-slot,
/// slot-operator, or USE-dep tail that follows the slot.
fn replace_slot(token: &str, old_slot: &str, new_slot: &str) -> String {
    let needle = format!(":{old_slot}");
    if let Some(pos) = token.find(&needle) {
        let after = &token[pos + needle.len()..];
        // Only a slot boundary: end, sub-slot, slot-op, or USE dep.
        if after.is_empty() || after.starts_with(['/', '=', '*', '[']) {
            let mut out = String::with_capacity(token.len());
            out.push_str(&token[..pos]);
            out.push(':');
            out.push_str(new_slot);
            out.push_str(after);
            return out;
        }
    }
    token.to_string()
}

/// Parse an atom token, ignoring non-atom structure tokens (`(`, `)`, `||`).
fn parse_atom(token: &str, features: EapiFeatures, interner: &Interner) -> Option<Atom> {
    if matches!(token, "(" | ")" | "||" | "^^" | "??") || token.ends_with('?') {
        return None;
    }
    Atom::parse(token, features, interner).ok()
}

/// The `category/package` of an atom.
fn cp_of(atom: &Atom, interner: &Interner) -> String {
    let cat = interner.resolve(atom.category()).unwrap_or_default();
    let pkg = interner.resolve(atom.package()).unwrap_or_default();
    format!("{cat}/{pkg}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features() -> EapiFeatures {
        moraine_eapi::features_for_level(8)
    }

    #[test]
    fn rewrites_cp_preserving_operator_and_slot() {
        let i = Interner::new();
        let out = rewrite_dep_cp(
            ">=dev-util/foo-1.2:3[ssl] dev-libs/other",
            "dev-util/foo",
            "dev-libs/foo",
            "cat/self",
            features(),
            &i,
        );
        assert_eq!(out, ">=dev-libs/foo-1.2:3[ssl] dev-libs/other");
    }

    #[test]
    fn skips_self_blocker_rewrite() {
        let i = Interner::new();
        // The owning package is dev-libs/foo; a blocker on the old name must not
        // be rewritten into a self-blocker.
        let out = rewrite_dep_cp(
            "!dev-util/foo",
            "dev-util/foo",
            "dev-libs/foo",
            "dev-libs/foo",
            features(),
            &i,
        );
        assert_eq!(out, "!dev-util/foo");
    }

    #[test]
    fn rewrites_slot_constraint_only_when_matching() {
        let i = Interner::new();
        let out = rewrite_dep_slot(
            "dev-libs/bar:0 dev-libs/bar:1",
            "dev-libs/bar",
            "0",
            "2",
            features(),
            &i,
        );
        assert_eq!(out, "dev-libs/bar:2 dev-libs/bar:1");
    }

    #[test]
    fn rewrites_slot_with_operator_tail() {
        let i = Interner::new();
        let out = rewrite_dep_slot("dev-libs/bar:0=", "dev-libs/bar", "0", "2", features(), &i);
        assert_eq!(out, "dev-libs/bar:2=");
    }
}
