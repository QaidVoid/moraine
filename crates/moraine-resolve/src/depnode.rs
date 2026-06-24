//! A resolver-local dependency AST with string identifiers.
//!
//! `moraine-atom`'s `DepSpec` carries `Atom`s whose category, package, slot, and
//! USE-flag tokens are interned against the source repository's interner. To
//! keep the encoder free of any foreign interner, the source normalizes each
//! dependency string into this string-based tree once, when building package
//! metadata.

use moraine_version::Version;

/// A version operator on a normalized atom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `=`
    Equal,
    /// `>=`
    GreaterEqual,
    /// `<=`
    LessEqual,
    /// `>`
    Greater,
    /// `<`
    Less,
    /// `~` (any revision of the version).
    Tilde,
    /// `=...*` (version-prefix glob).
    EqualGlob,
}

/// The blocker strength of an atom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockerKind {
    /// Not a blocker.
    None,
    /// `!` weak blocker.
    Weak,
    /// `!!` strong blocker.
    Strong,
}

/// A slot-operator on an atom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotOpKind {
    /// `:=` binds the provider's slot and sub-slot.
    Equal,
    /// `:*` matches any slot with no binding.
    Star,
}

/// A USE-dependency requirement on an atom, in normalized string form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseReq {
    /// The flag name.
    pub flag: String,
    /// The kind of USE requirement.
    pub kind: UseReqKind,
    /// The default to assume when the flag is absent from the candidate's IUSE:
    /// `Some(true)` for `(+)`, `Some(false)` for `(-)`, `None` for no default.
    pub default: Option<bool>,
}

/// The kind of a USE-dependency requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseReqKind {
    /// `flag`: must be enabled.
    Enabled,
    /// `-flag` / `!flag`: must be disabled.
    Disabled,
    /// `flag?`: if enabled on the parent, must be enabled.
    EnabledIfParent,
    /// `!flag?`: if disabled on the parent, must be disabled.
    DisabledIfParent,
    /// `flag=`: must match the parent's state.
    EqualToParent,
    /// `!flag=`: must be the opposite of the parent's state.
    OppositeToParent,
}

/// A normalized atom: all identifiers are owned strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormAtom {
    /// The blocker strength.
    pub blocker: BlockerKind,
    /// The `category/package`.
    pub cp: String,
    /// The version operator and version, if the atom is versioned.
    pub version: Option<(Op, Version)>,
    /// The named slot, if any.
    pub slot: Option<String>,
    /// The named sub-slot, if any.
    pub subslot: Option<String>,
    /// The slot operator, if any.
    pub slot_op: Option<SlotOpKind>,
    /// The USE-dependency requirements.
    pub use_deps: Vec<UseReq>,
}

/// A node in the normalized dependency AST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepNode {
    /// An all-of group: every child must hold.
    AllOf(Vec<DepNode>),
    /// An any-of (`||`) group: at least one child must hold.
    AnyOf(Vec<DepNode>),
    /// A USE-conditional group.
    Conditional {
        /// The controlling flag.
        flag: String,
        /// `true` for `flag?`, `false` for `!flag?`.
        sense: bool,
        /// The conditional body.
        body: Vec<DepNode>,
    },
    /// A single atom.
    Leaf(NormAtom),
}
