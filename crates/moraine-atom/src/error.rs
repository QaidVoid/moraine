//! Error types for atom and dependency-string parsing.

/// An error produced while parsing a package atom.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid atom `{input}`: {reason}")]
pub struct AtomError {
    /// The atom string that failed to parse.
    pub input: String,
    /// A short description of why it failed.
    pub reason: &'static str,
}

/// An error produced while parsing a dependency string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DepError {
    /// An atom within the dependency string failed to parse.
    #[error(transparent)]
    Atom(#[from] AtomError),

    /// The dependency string was structurally malformed.
    #[error("malformed dependency string: {reason}")]
    Structure {
        /// A short description of the structural problem.
        reason: &'static str,
    },
}
