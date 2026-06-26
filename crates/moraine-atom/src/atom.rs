//! Package atoms: parsing, matching, the USE-dependency model, and rendering.
//!
//! Tokens (category, package, slot, sub-slot, USE flag, repository) are interned
//! through [`moraine_common::Interner`] so equal tokens share storage and match
//! by id. The parsed version carries its own comparison key, so matching never
//! reparses.

use std::collections::HashSet;

use moraine_common::{Interner, Symbol};
use moraine_eapi::EapiFeatures;
use moraine_version::Version;

use crate::error::AtomError;

/// A version-range operator on an atom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    /// `=` exact version.
    Equal,
    /// `>=`.
    GreaterEqual,
    /// `<=`.
    LessEqual,
    /// `>`.
    Greater,
    /// `<`.
    Less,
    /// `~` any revision of the version.
    Tilde,
    /// `=...*` prefix glob.
    EqualGlob,
}

/// Blocker strength on an atom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocker {
    /// Not a blocker.
    None,
    /// Weak blocker (`!`).
    Weak,
    /// Strong blocker (`!!`).
    Strong,
}

/// A slot operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotOp {
    /// `:=` rebuild-on-subslot-change binding.
    Equal,
    /// `:*` match any slot.
    Star,
}

/// The kind of a single USE dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseDepKind {
    /// `flag`: require enabled.
    Enabled,
    /// `-flag` or `!flag`: require disabled.
    Disabled,
    /// `flag?`: if enabled on the parent, require enabled.
    EnabledIfParent,
    /// `!flag?`: if disabled on the parent, require disabled.
    DisabledIfParent,
    /// `flag=`: require the same state as the parent.
    EqualToParent,
    /// `!flag=`: require the opposite state to the parent.
    OppositeToParent,
}

/// A single USE dependency such as `ssl`, `-bindist`, or `python_targets?`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UseDep {
    /// The interned USE flag.
    pub flag: Symbol,
    /// The kind of requirement.
    pub kind: UseDepKind,
    /// The missing-flag default: `Some(true)` for `(+)`, `Some(false)` for `(-)`.
    pub default: Option<bool>,
}

/// A concrete USE requirement produced by evaluating conditional USE deps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UseRequirement {
    /// The interned USE flag.
    pub flag: Symbol,
    /// Whether the dependency must have the flag enabled.
    pub enabled: bool,
}

/// A reference to a candidate package, used for matching without reparsing.
#[derive(Debug, Clone, Copy)]
pub struct PackageRef<'a> {
    /// The interned category.
    pub category: Symbol,
    /// The interned package name.
    pub package: Symbol,
    /// The candidate version.
    pub version: &'a Version,
    /// The interned slot, if known.
    pub slot: Option<Symbol>,
    /// The interned sub-slot, if known.
    pub subslot: Option<Symbol>,
    /// The interned origin repository, if known.
    pub repo: Option<Symbol>,
}

/// A parsed package atom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Atom {
    blocker: Blocker,
    category: Symbol,
    package: Symbol,
    version: Option<(Operator, Version)>,
    slot: Option<Symbol>,
    subslot: Option<Symbol>,
    slot_op: Option<SlotOp>,
    use_deps: Box<[UseDep]>,
    repo: Option<Symbol>,
}

impl Atom {
    /// The blocker strength.
    pub fn blocker(&self) -> Blocker {
        self.blocker
    }

    /// The interned category.
    pub fn category(&self) -> Symbol {
        self.category
    }

    /// The interned package name.
    pub fn package(&self) -> Symbol {
        self.package
    }

    /// The version operator and version, if the atom is versioned.
    pub fn version(&self) -> Option<(Operator, &Version)> {
        self.version.as_ref().map(|(op, v)| (*op, v))
    }

    /// The interned slot, if specified.
    pub fn slot(&self) -> Option<Symbol> {
        self.slot
    }

    /// The interned sub-slot, if specified.
    pub fn subslot(&self) -> Option<Symbol> {
        self.subslot
    }

    /// The slot operator, if specified.
    pub fn slot_op(&self) -> Option<SlotOp> {
        self.slot_op
    }

    /// The interned repository, if specified.
    pub fn repo(&self) -> Option<Symbol> {
        self.repo
    }

    /// The USE dependencies.
    pub fn use_deps(&self) -> &[UseDep] {
        &self.use_deps
    }

    /// Return a copy of this atom with its slot operator bound to a concrete
    /// `slot` and optional `subslot`, leaving the operator in place. Used to bake
    /// a resolved `:=` binding into a recorded dependency (`:slot/subslot=`).
    pub fn with_bound_slot(&self, slot: Symbol, subslot: Option<Symbol>) -> Atom {
        let mut atom = self.clone();
        atom.slot = Some(slot);
        atom.subslot = subslot;
        atom
    }

    /// Parse an atom under the given EAPI features, interning tokens.
    pub fn parse(
        input: &str,
        features: EapiFeatures,
        interner: &Interner,
    ) -> Result<Atom, AtomError> {
        let err = |reason: &'static str| AtomError {
            input: input.to_owned(),
            reason,
        };

        // Blocker prefix.
        let (blocker, rest) = if let Some(r) = input.strip_prefix("!!") {
            if !features.strong_blocks {
                return Err(err("strong blocker requires EAPI 2 or later"));
            }
            (Blocker::Strong, r)
        } else if let Some(r) = input.strip_prefix('!') {
            (Blocker::Weak, r)
        } else {
            (Blocker::None, input)
        };

        // Version operator prefix.
        let (operator, rest) = parse_operator(rest);

        // Split off the slot / USE / repo suffix from the cpv body.
        let cpv_end = rest.find([':', '[']).unwrap_or(rest.len());
        let (cpv_raw, mut suffix) = rest.split_at(cpv_end);

        // Resolve the cp and optional version.
        let (cp, version) = match operator {
            None => (cpv_raw, None),
            Some(Operator::EqualGlob) => unreachable!("glob is detected below"),
            Some(op) => {
                let mut op = op;
                let mut body = cpv_raw;
                if op == Operator::Equal && body.ends_with('*') {
                    op = Operator::EqualGlob;
                    body = &body[..body.len() - 1];
                }
                let (cp, ver_str) =
                    split_cp_version(body).ok_or_else(|| err("expected a version"))?;
                let version = Version::parse(ver_str).map_err(|_| err("invalid version"))?;
                // PMS: the `~` operator ignores the revision, so specifying one is
                // invalid.
                if op == Operator::Tilde && version.revision() != 0 {
                    return Err(err("the ~ operator may not specify a revision"));
                }
                (cp, Some((op, version)))
            }
        };

        // Validate and intern the category/package.
        let (category, package) = parse_cp(cp, interner, &err)?;

        // Parse the suffix: slot, then USE deps, then repository.
        let mut slot = None;
        let mut subslot = None;
        let mut slot_op = None;
        let mut use_deps: Vec<UseDep> = Vec::new();
        let mut repo = None;

        if let Some(after) = suffix.strip_prefix("::") {
            // Repository directly after cpv (no slot, no use).
            repo = Some(parse_repo(after, features, interner, &err)?);
            suffix = "";
        } else if let Some(after) = suffix.strip_prefix(':') {
            let slot_end = after.find('[').unwrap_or(after.len());
            let (slot_part, rem) = after.split_at(slot_end);
            let (slot_part, rem) = match slot_part.find("::") {
                Some(p) => (&slot_part[..p], &after[p..]),
                None => (slot_part, rem),
            };
            parse_slot(
                slot_part,
                features,
                interner,
                &err,
                &mut slot,
                &mut subslot,
                &mut slot_op,
            )?;
            suffix = rem;
        }

        if let Some(after) = suffix.strip_prefix('[') {
            let close = after
                .find(']')
                .ok_or_else(|| err("unterminated USE dependency"))?;
            let list = &after[..close];
            parse_use_deps(list, features, interner, &err, &mut use_deps)?;
            suffix = &after[close + 1..];
        }

        if let Some(after) = suffix.strip_prefix("::") {
            repo = Some(parse_repo(after, features, interner, &err)?);
            suffix = "";
        }

        if !suffix.is_empty() {
            return Err(err("trailing characters after atom"));
        }

        Ok(Atom {
            blocker,
            category,
            package,
            version,
            slot,
            subslot,
            slot_op,
            use_deps: use_deps.into_boxed_slice(),
            repo,
        })
    }

    /// Test whether a candidate package matches this atom's category/package,
    /// version operator, slot, and repository constraints. USE dependencies are
    /// evaluated separately via [`Atom::evaluate_use`].
    pub fn matches(&self, pkg: &PackageRef<'_>) -> bool {
        if self.category != pkg.category || self.package != pkg.package {
            return false;
        }
        if let Some((op, ver)) = &self.version {
            let ok = match op {
                Operator::Equal => pkg.version == ver,
                Operator::GreaterEqual => pkg.version >= ver,
                Operator::LessEqual => pkg.version <= ver,
                Operator::Greater => pkg.version > ver,
                Operator::Less => pkg.version < ver,
                Operator::Tilde => pkg.version.matches_any_revision(ver),
                Operator::EqualGlob => version_glob_matches(pkg.version.as_str(), ver.as_str()),
            };
            if !ok {
                return false;
            }
        }
        if let Some(slot) = self.slot
            && pkg.slot != Some(slot)
        {
            return false;
        }
        if let Some(subslot) = self.subslot
            && pkg.subslot != Some(subslot)
        {
            return false;
        }
        if let Some(repo) = self.repo
            && pkg.repo != Some(repo)
        {
            return false;
        }
        true
    }

    /// Evaluate conditional USE dependencies against the parent package's USE
    /// set, producing the concrete USE requirements the candidate must satisfy.
    pub fn evaluate_use(&self, parent_use: &HashSet<Symbol>) -> Vec<UseRequirement> {
        let mut out = Vec::with_capacity(self.use_deps.len());
        for dep in self.use_deps.iter() {
            let req = |enabled| UseRequirement {
                flag: dep.flag,
                enabled,
            };
            match dep.kind {
                UseDepKind::Enabled => out.push(req(true)),
                UseDepKind::Disabled => out.push(req(false)),
                UseDepKind::EnabledIfParent => {
                    if parent_use.contains(&dep.flag) {
                        out.push(req(true));
                    }
                }
                UseDepKind::DisabledIfParent => {
                    if !parent_use.contains(&dep.flag) {
                        out.push(req(false));
                    }
                }
                UseDepKind::EqualToParent => out.push(req(parent_use.contains(&dep.flag))),
                UseDepKind::OppositeToParent => out.push(req(!parent_use.contains(&dep.flag))),
            }
        }
        out
    }

    /// Render the atom back to its canonical string form.
    pub fn render(&self, interner: &Interner) -> String {
        let resolve = |s: Symbol| {
            interner
                .resolve(s)
                .map(|a| a.to_string())
                .unwrap_or_default()
        };
        let mut out = String::new();
        match self.blocker {
            Blocker::None => {}
            Blocker::Weak => out.push('!'),
            Blocker::Strong => out.push_str("!!"),
        }
        if let Some((op, ver)) = &self.version {
            out.push_str(match op {
                Operator::Equal | Operator::EqualGlob => "=",
                Operator::GreaterEqual => ">=",
                Operator::LessEqual => "<=",
                Operator::Greater => ">",
                Operator::Less => "<",
                Operator::Tilde => "~",
            });
            out.push_str(&resolve(self.category));
            out.push('/');
            out.push_str(&resolve(self.package));
            out.push('-');
            out.push_str(ver.as_str());
            if *op == Operator::EqualGlob {
                out.push('*');
            }
        } else {
            out.push_str(&resolve(self.category));
            out.push('/');
            out.push_str(&resolve(self.package));
        }
        if let Some(slot) = self.slot {
            out.push(':');
            out.push_str(&resolve(slot));
            if let Some(sub) = self.subslot {
                out.push('/');
                out.push_str(&resolve(sub));
            }
            if self.slot_op == Some(SlotOp::Equal) {
                out.push('=');
            }
        } else {
            match self.slot_op {
                Some(SlotOp::Equal) => out.push_str(":="),
                Some(SlotOp::Star) => out.push_str(":*"),
                None => {}
            }
        }
        if !self.use_deps.is_empty() {
            out.push('[');
            for (i, dep) in self.use_deps.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                render_use_dep(&mut out, dep, &resolve);
            }
            out.push(']');
        }
        if let Some(repo) = self.repo {
            out.push_str("::");
            out.push_str(&resolve(repo));
        }
        out
    }
}

fn render_use_dep(out: &mut String, dep: &UseDep, resolve: &impl Fn(Symbol) -> String) {
    match dep.kind {
        UseDepKind::Disabled => out.push('-'),
        UseDepKind::DisabledIfParent | UseDepKind::OppositeToParent => out.push('!'),
        _ => {}
    }
    out.push_str(&resolve(dep.flag));
    match dep.default {
        Some(true) => out.push_str("(+)"),
        Some(false) => out.push_str("(-)"),
        None => {}
    }
    match dep.kind {
        UseDepKind::EnabledIfParent | UseDepKind::DisabledIfParent => out.push('?'),
        UseDepKind::EqualToParent | UseDepKind::OppositeToParent => out.push('='),
        _ => {}
    }
}

/// Whether `candidate` matches an `=...*` glob whose version string is `prefix`.
///
/// PMS treats the asterisk as wildcarding any further version components, so the
/// match must end at a component boundary: `=1.2*` matches `1.2`, `1.2.3`, and
/// `1.2_alpha`, but not `1.20` (the next character continues the same numeric
/// component).
fn version_glob_matches(candidate: &str, prefix: &str) -> bool {
    candidate.starts_with(prefix)
        && candidate
            .as_bytes()
            .get(prefix.len())
            .map(|b| !b.is_ascii_digit())
            .unwrap_or(true)
}

fn parse_operator(s: &str) -> (Option<Operator>, &str) {
    if let Some(r) = s.strip_prefix(">=") {
        (Some(Operator::GreaterEqual), r)
    } else if let Some(r) = s.strip_prefix("<=") {
        (Some(Operator::LessEqual), r)
    } else if let Some(r) = s.strip_prefix('>') {
        (Some(Operator::Greater), r)
    } else if let Some(r) = s.strip_prefix('<') {
        (Some(Operator::Less), r)
    } else if let Some(r) = s.strip_prefix('~') {
        (Some(Operator::Tilde), r)
    } else if let Some(r) = s.strip_prefix('=') {
        (Some(Operator::Equal), r)
    } else {
        (None, s)
    }
}

fn split_cp_version(cpv: &str) -> Option<(&str, &str)> {
    let bytes = cpv.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'-' && i > 0 && i + 1 < bytes.len() && Version::parse(&cpv[i + 1..]).is_ok() {
            return Some((&cpv[..i], &cpv[i + 1..]));
        }
    }
    None
}

fn parse_cp(
    cp: &str,
    interner: &Interner,
    err: &impl Fn(&'static str) -> AtomError,
) -> Result<(Symbol, Symbol), AtomError> {
    let (category, package) = cp.split_once('/').ok_or_else(|| err("missing category"))?;
    if !is_valid_name(category) {
        return Err(err("invalid category name"));
    }
    if package.is_empty() || package.contains('/') || !is_valid_name(package) {
        return Err(err("invalid package name"));
    }
    Ok((interner.intern(category), interner.intern(package)))
}

fn is_valid_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let first = s.as_bytes()[0];
    if !(first.is_ascii_alphanumeric() || first == b'_') {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'_' | b'-' | b'.'))
}

#[allow(clippy::too_many_arguments)]
fn parse_slot(
    s: &str,
    features: EapiFeatures,
    interner: &Interner,
    err: &impl Fn(&'static str) -> AtomError,
    slot: &mut Option<Symbol>,
    subslot: &mut Option<Symbol>,
    slot_op: &mut Option<SlotOp>,
) -> Result<(), AtomError> {
    if !features.slot_deps {
        return Err(err("slot dependencies require EAPI 1 or later"));
    }
    if s == "*" {
        require_slot_op(features, err)?;
        *slot_op = Some(SlotOp::Star);
        return Ok(());
    }
    if s == "=" {
        require_slot_op(features, err)?;
        *slot_op = Some(SlotOp::Equal);
        return Ok(());
    }
    let body = if let Some(stripped) = s.strip_suffix('=') {
        require_slot_op(features, err)?;
        *slot_op = Some(SlotOp::Equal);
        stripped
    } else {
        s
    };
    let (slot_name, sub) = match body.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (body, None),
    };
    if !is_valid_name(slot_name) {
        return Err(err("invalid slot name"));
    }
    *slot = Some(interner.intern(slot_name));
    if let Some(sub) = sub {
        if !is_valid_name(sub) {
            return Err(err("invalid sub-slot name"));
        }
        *subslot = Some(interner.intern(sub));
    }
    Ok(())
}

fn require_slot_op(
    features: EapiFeatures,
    err: &impl Fn(&'static str) -> AtomError,
) -> Result<(), AtomError> {
    if features.slot_operator {
        Ok(())
    } else {
        Err(err("slot operators require EAPI 5 or later"))
    }
}

fn parse_use_deps(
    list: &str,
    features: EapiFeatures,
    interner: &Interner,
    err: &impl Fn(&'static str) -> AtomError,
    out: &mut Vec<UseDep>,
) -> Result<(), AtomError> {
    if !features.use_deps {
        return Err(err("USE dependencies require EAPI 2 or later"));
    }
    if list.is_empty() {
        return Err(err("empty USE dependency block"));
    }
    for raw in list.split(',') {
        let entry = raw.trim();
        if entry.is_empty() {
            return Err(err("empty USE dependency entry"));
        }
        out.push(parse_use_dep(entry, features, interner, err)?);
    }
    Ok(())
}

fn parse_use_dep(
    entry: &str,
    features: EapiFeatures,
    interner: &Interner,
    err: &impl Fn(&'static str) -> AtomError,
) -> Result<UseDep, AtomError> {
    let mut s = entry;
    let mut excl = false;
    let mut minus = false;
    if let Some(r) = s.strip_prefix('!') {
        excl = true;
        s = r;
    } else if let Some(r) = s.strip_prefix('-') {
        minus = true;
        s = r;
    }

    let mut suffix: Option<char> = None;
    if let Some(r) = s.strip_suffix('?') {
        suffix = Some('?');
        s = r;
    } else if let Some(r) = s.strip_suffix('=') {
        suffix = Some('=');
        s = r;
    }

    let mut default = None;
    if let Some(r) = s.strip_suffix("(+)") {
        default = Some(true);
        s = r;
    } else if let Some(r) = s.strip_suffix("(-)") {
        default = Some(false);
        s = r;
    }
    if default.is_some() && !features.use_dep_defaults {
        return Err(err("USE-dependency defaults require EAPI 4 or later"));
    }

    if s.is_empty() || !is_valid_flag(s) {
        return Err(err("invalid USE flag"));
    }

    let kind = match (excl, minus, suffix) {
        (false, false, None) => UseDepKind::Enabled,
        (false, true, None) => UseDepKind::Disabled,
        (true, false, None) => UseDepKind::Disabled,
        (false, false, Some('?')) => UseDepKind::EnabledIfParent,
        (true, false, Some('?')) => UseDepKind::DisabledIfParent,
        (false, false, Some('=')) => UseDepKind::EqualToParent,
        (true, false, Some('=')) => UseDepKind::OppositeToParent,
        _ => return Err(err("invalid USE dependency form")),
    };

    Ok(UseDep {
        flag: interner.intern(s),
        kind,
        default,
    })
}

fn is_valid_flag(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let first = s.as_bytes()[0];
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    // PMS USE-flag names: `[A-Za-z0-9][A-Za-z0-9+_@-]*` (no `.`).
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'_' | b'-' | b'@'))
}

/// PMS repository names: `[A-Za-z0-9_][A-Za-z0-9_-]*` (no `+`, no `.`).
fn is_valid_repo_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let first = s.as_bytes()[0];
    if !(first.is_ascii_alphanumeric() || first == b'_') {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn parse_repo(
    s: &str,
    features: EapiFeatures,
    interner: &Interner,
    err: &impl Fn(&'static str) -> AtomError,
) -> Result<Symbol, AtomError> {
    if !features.repo_deps {
        return Err(err("repository specifiers are not permitted for this EAPI"));
    }
    if !is_valid_repo_name(s) {
        return Err(err("invalid repository name"));
    }
    Ok(interner.intern(s))
}
