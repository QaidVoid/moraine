//! Mapping solver and serialization failures to located diagnostics.
//!
//! `moraine-resolve` returns structured failures: an unsatisfiable request with
//! a derivation chain, a parse failure naming the bad atom, or an unbreakable
//! merge cycle. This module turns each into a [`ResolutionDiagnostic`] that
//! renders through the binary's miette reporter and drives a non-zero exit.

use miette::Diagnostic;
use moraine_resolve::{MergeOrderError, ResolveError};
use thiserror::Error;

/// A resolution or merge-order failure ready to render through the reporter.
#[derive(Debug, Error, Diagnostic)]
pub enum ResolutionDiagnostic {
    /// A requested atom could not be parsed.
    #[error("could not parse request `{atom}`")]
    #[diagnostic(code(moraine::resolve::bad_request))]
    BadRequest {
        /// The atom that failed to parse.
        atom: String,
        /// Why it failed.
        #[help]
        reason: String,
    },

    /// A requested atom matched no package.
    #[error("no package matches `{atom}`")]
    #[diagnostic(
        code(moraine::resolve::no_match),
        help("no visible version satisfies this atom")
    )]
    NoMatch {
        /// The atom that matched nothing.
        atom: String,
    },

    /// A dependency string was invalid for its EAPI.
    #[error("invalid dependency in {package}")]
    #[diagnostic(code(moraine::resolve::invalid_dependency))]
    InvalidDependency {
        /// The package whose dependency was invalid.
        package: String,
        /// Why the dependency was rejected.
        #[help]
        reason: String,
    },

    /// The request could not be satisfied. The conflict body is the solver's
    /// derivation chain, which names the competing constraints, the rejected
    /// candidates, and any blocking atoms.
    #[error("the request could not be satisfied")]
    #[diagnostic(
        code(moraine::resolve::unsatisfiable),
        help("the conflict below names the constraints that cannot hold together")
    )]
    Unsatisfiable {
        /// The solver's structured explanation, rendered for display.
        explanation: String,
    },

    /// The merge order contains an unbreakable dependency cycle.
    #[error("the merge order contains an unbreakable dependency cycle")]
    #[diagnostic(code(moraine::resolve::cycle))]
    Cycle {
        /// The cycle, rendered with its packages and edges.
        detail: String,
    },

    /// A scheduling safety rule refused an operation.
    #[error("the resolver refused an unsafe operation")]
    #[diagnostic(code(moraine::resolve::unsafe_operation))]
    UnsafeOperation {
        /// The refusal reason.
        reason: String,
    },
}

impl ResolutionDiagnostic {
    /// The detail block appended below the headline, if any.
    ///
    /// Used so the binary can print the solver's full derivation chain or cycle
    /// detail under the rendered diagnostic.
    pub fn detail(&self) -> Option<&str> {
        match self {
            ResolutionDiagnostic::Unsatisfiable { explanation } => Some(explanation),
            ResolutionDiagnostic::Cycle { detail } => Some(detail),
            _ => None,
        }
    }
}

impl From<ResolveError> for ResolutionDiagnostic {
    fn from(error: ResolveError) -> Self {
        match error {
            ResolveError::BadRequest { atom, reason } => {
                ResolutionDiagnostic::BadRequest { atom, reason }
            }
            ResolveError::NoMatch { atom } => ResolutionDiagnostic::NoMatch { atom },
            ResolveError::InvalidDependency { package, reason } => {
                ResolutionDiagnostic::InvalidDependency { package, reason }
            }
            ResolveError::Unsatisfiable { explanation } => {
                ResolutionDiagnostic::Unsatisfiable { explanation }
            }
            ResolveError::UnresolvableBlocker {
                blocker,
                victim,
                reason,
            } => ResolutionDiagnostic::Unsatisfiable {
                explanation: format!("blocker {blocker} blocks {victim}, but {reason}"),
            },
        }
    }
}

impl From<MergeOrderError> for ResolutionDiagnostic {
    fn from(error: MergeOrderError) -> Self {
        match error {
            MergeOrderError::UnresolvableCycle(cycle) => ResolutionDiagnostic::Cycle {
                detail: cycle.to_string(),
            },
            MergeOrderError::UnsafeOperation(reason) => {
                ResolutionDiagnostic::UnsafeOperation { reason }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_request_maps_to_diagnostic() {
        let diag: ResolutionDiagnostic = ResolveError::BadRequest {
            atom: "cat/".to_owned(),
            reason: "missing package name".to_owned(),
        }
        .into();
        assert!(matches!(diag, ResolutionDiagnostic::BadRequest { .. }));
        assert!(diag.detail().is_none());
    }

    #[test]
    fn unsatisfiable_carries_explanation() {
        let diag: ResolutionDiagnostic = ResolveError::Unsatisfiable {
            explanation: "cat/a needs cat/b but cat/b is masked".to_owned(),
        }
        .into();
        assert_eq!(diag.detail(), Some("cat/a needs cat/b but cat/b is masked"));
    }

    #[test]
    fn no_match_maps_to_diagnostic() {
        let diag: ResolutionDiagnostic = ResolveError::NoMatch {
            atom: "cat/missing".to_owned(),
        }
        .into();
        assert!(matches!(diag, ResolutionDiagnostic::NoMatch { .. }));
    }
}
