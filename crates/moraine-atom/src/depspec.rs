//! The dependency-string AST.
//!
//! Parses a DEPEND-style string into a typed tree equivalent to stock
//! `use_reduce(..., opconvert=True, token_class=Atom)`. The tree preserves USE
//! conditional structure so the same AST can be evaluated under different USE
//! sets without reparsing or mutation.

use std::collections::HashSet;

use moraine_common::{Interner, Symbol};
use moraine_eapi::EapiFeatures;

use crate::atom::Atom;
use crate::error::DepError;

/// A node in a dependency-string AST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepSpec {
    /// An all-of group: every child must be satisfied.
    AllOf(Vec<DepSpec>),
    /// An any-of (`||`) group: at least one child must be satisfied.
    AnyOf(Vec<DepSpec>),
    /// A USE conditional group (`flag? ( ... )` or `!flag? ( ... )`).
    Conditional {
        /// The controlling USE flag.
        flag: Symbol,
        /// `true` for `flag?`, `false` for `!flag?`.
        sense: bool,
        /// The conditional body (an implicit all-of group).
        body: Vec<DepSpec>,
    },
    /// A single atom.
    Leaf(Atom),
}

impl DepSpec {
    /// Parse a dependency string into an all-of top-level group.
    pub fn parse(
        input: &str,
        features: EapiFeatures,
        interner: &Interner,
    ) -> Result<DepSpec, DepError> {
        let tokens: Vec<&str> = input.split_whitespace().collect();
        let mut parser = Parser {
            tokens: &tokens,
            pos: 0,
            features,
            interner,
        };
        let items = parser.parse_seq(false)?;
        Ok(DepSpec::AllOf(items))
    }

    /// Evaluate the AST against a USE set, resolving conditional groups. A
    /// satisfied conditional contributes its body; an unsatisfied one
    /// contributes nothing. The source AST is not mutated.
    pub fn evaluate(&self, use_set: &HashSet<Symbol>) -> DepSpec {
        match self {
            DepSpec::Leaf(atom) => DepSpec::Leaf(atom.clone()),
            DepSpec::AllOf(items) => DepSpec::AllOf(eval_items(items, use_set)),
            DepSpec::AnyOf(items) => DepSpec::AnyOf(eval_items(items, use_set)),
            DepSpec::Conditional { flag, sense, body } => {
                let active = use_set.contains(flag) == *sense;
                if active {
                    DepSpec::AllOf(eval_items(body, use_set))
                } else {
                    DepSpec::AllOf(Vec::new())
                }
            }
        }
    }

    /// Render the spec back to a dependency string, preserving grouping.
    ///
    /// All-of groups are flattened (redundant parentheses are dropped) while
    /// any-of (`||`) groups are emitted as `|| ( ... )` and an all-of group that
    /// is a direct alternative of an any-of keeps its `( ... )` parentheses,
    /// matching stock `use_reduce` followed by `paren_enclose`. A USE conditional
    /// that survived (an unevaluated spec) is rendered as `flag? ( ... )`.
    pub fn render(&self, interner: &Interner) -> String {
        let mut parts = Vec::new();
        self.render_into(&mut parts, interner, false);
        parts.join(" ")
    }

    fn render_into(&self, parts: &mut Vec<String>, interner: &Interner, in_any_of: bool) {
        match self {
            DepSpec::Leaf(atom) => parts.push(atom.render(interner)),
            DepSpec::AllOf(items) => {
                if in_any_of {
                    // A meaningful sub-alternative under `||`: keep its parens.
                    let mut inner = Vec::new();
                    for item in items {
                        item.render_into(&mut inner, interner, false);
                    }
                    if !inner.is_empty() {
                        parts.push(format!("( {} )", inner.join(" ")));
                    }
                } else {
                    // A redundant all-of group: flatten into the parent sequence.
                    for item in items {
                        item.render_into(parts, interner, false);
                    }
                }
            }
            DepSpec::AnyOf(items) => {
                let mut inner = Vec::new();
                for item in items {
                    item.render_into(&mut inner, interner, true);
                }
                parts.push(format!("|| ( {} )", inner.join(" ")));
            }
            DepSpec::Conditional { flag, sense, body } => {
                let mut inner = Vec::new();
                for item in body {
                    item.render_into(&mut inner, interner, false);
                }
                let name = interner.resolve(*flag).unwrap_or_default();
                let prefix = if *sense { "" } else { "!" };
                parts.push(format!("{prefix}{name}? ( {} )", inner.join(" ")));
            }
        }
    }

    /// Collect every atom leaf in the tree, depth-first.
    pub fn atoms(&self) -> Vec<&Atom> {
        let mut out = Vec::new();
        self.collect_atoms(&mut out);
        out
    }

    fn collect_atoms<'a>(&'a self, out: &mut Vec<&'a Atom>) {
        match self {
            DepSpec::Leaf(atom) => out.push(atom),
            DepSpec::AllOf(items) | DepSpec::AnyOf(items) => {
                for item in items {
                    item.collect_atoms(out);
                }
            }
            DepSpec::Conditional { body, .. } => {
                for item in body {
                    item.collect_atoms(out);
                }
            }
        }
    }
}

fn eval_items(items: &[DepSpec], use_set: &HashSet<Symbol>) -> Vec<DepSpec> {
    items
        .iter()
        .filter_map(|node| match node {
            DepSpec::Conditional { flag, sense, body } => {
                if use_set.contains(flag) == *sense {
                    Some(DepSpec::AllOf(eval_items(body, use_set)))
                } else {
                    None
                }
            }
            other => Some(other.evaluate(use_set)),
        })
        .collect()
}

struct Parser<'a> {
    tokens: &'a [&'a str],
    pos: usize,
    features: EapiFeatures,
    interner: &'a Interner,
}

impl Parser<'_> {
    fn parse_seq(&mut self, expect_close: bool) -> Result<Vec<DepSpec>, DepError> {
        let mut items = Vec::new();
        loop {
            let Some(&tok) = self.tokens.get(self.pos) else {
                if expect_close {
                    return Err(DepError::Structure {
                        reason: "unbalanced parentheses",
                    });
                }
                return Ok(items);
            };

            if tok == ")" {
                if !expect_close {
                    return Err(DepError::Structure {
                        reason: "unexpected closing parenthesis",
                    });
                }
                self.pos += 1;
                return Ok(items);
            } else if tok == "(" {
                self.pos += 1;
                let inner = self.parse_seq(true)?;
                items.push(DepSpec::AllOf(inner));
            } else if tok == "||" {
                self.pos += 1;
                let inner = self.parse_grouped()?;
                items.push(DepSpec::AnyOf(inner));
            } else if tok == "^^" || tok == "??" {
                // Exactly-one-of and at-most-one-of are REQUIRED_USE-only; they
                // are not valid in a dependency-spec string.
                return Err(DepError::Structure {
                    reason: "^^/?? groups are only valid in REQUIRED_USE",
                });
            } else if let Some(cond) = tok.strip_suffix('?') {
                self.pos += 1;
                let (flag, sense) = if let Some(flag) = cond.strip_prefix('!') {
                    (flag, false)
                } else {
                    (cond, true)
                };
                if flag.is_empty() {
                    return Err(DepError::Structure {
                        reason: "empty conditional flag",
                    });
                }
                let body = self.parse_grouped()?;
                items.push(DepSpec::Conditional {
                    flag: self.interner.intern(flag),
                    sense,
                    body,
                });
            } else {
                let atom = Atom::parse(tok, self.features, self.interner)?;
                items.push(DepSpec::Leaf(atom));
                self.pos += 1;
            }
        }
    }

    /// Parse a parenthesized group that must immediately follow `||` or a
    /// conditional token.
    fn parse_grouped(&mut self) -> Result<Vec<DepSpec>, DepError> {
        if self.tokens.get(self.pos) != Some(&"(") {
            return Err(DepError::Structure {
                reason: "operator must be followed by a parenthesized group",
            });
        }
        self.pos += 1;
        self.parse_seq(true)
    }
}
