//! Error types for resolution and merge-order serialization.

use std::fmt;

/// An error raised during dependency resolution.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// A dependency string declared a feature its EAPI does not support.
    #[error("invalid dependency in {package}: {reason}")]
    InvalidDependency {
        /// The package whose dependency string was invalid.
        package: String,
        /// Why the dependency string was rejected.
        reason: String,
    },

    /// The request could not be satisfied. The rendered explanation is a
    /// derivation chain from the solver naming the conflicting constraints.
    #[error("unsatisfiable request:\n{explanation}")]
    Unsatisfiable {
        /// The human-readable derivation chain.
        explanation: String,
    },

    /// A requested atom matched no package at all.
    #[error("no package matches the request `{atom}`")]
    NoMatch {
        /// The atom that matched nothing.
        atom: String,
    },

    /// A requested atom string could not be parsed.
    #[error("could not parse request `{atom}`: {reason}")]
    BadRequest {
        /// The atom that failed to parse.
        atom: String,
        /// Why it failed.
        reason: String,
    },
}

/// An error raised while serializing the merge order.
#[derive(Debug, thiserror::Error)]
pub enum MergeOrderError {
    /// A dependency cycle could not be broken by any loosening stage.
    #[error("unresolvable dependency cycle:\n{0}")]
    UnresolvableCycle(ResidualCycle),

    /// A scheduling safety rule refused an operation.
    #[error("refused unsafe operation: {0}")]
    UnsafeOperation(String),
}

/// A structured diagnostic describing a residual dependency cycle that no
/// loosening stage could break.
#[derive(Debug, Clone)]
pub struct ResidualCycle {
    /// The packages forming the cycle, as `category/package` strings.
    pub packages: Vec<String>,
    /// The directed edges between cycle members, as `(from, to, class)`.
    pub edges: Vec<(String, String, String)>,
}

impl fmt::Display for ResidualCycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "  packages in cycle:")?;
        for p in &self.packages {
            writeln!(f, "    - {p}")?;
        }
        writeln!(f, "  edges:")?;
        for (from, to, class) in &self.edges {
            writeln!(f, "    - {from} --{class}--> {to}")?;
        }
        Ok(())
    }
}
