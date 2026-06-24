//! Terms and their relation to an accumulated constraint.
//!
//! A term pairs a package's allowed version range with a polarity. A positive
//! term means "the selected version is in the range"; a negative term means "the
//! selected version is not in the range", handled by complementing the range.

use crate::range::Range;

/// How a term relates to an accumulated constraint on the same package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// The constraint guarantees the term holds.
    Satisfied,
    /// The constraint makes the term impossible.
    Contradicted,
    /// Neither: the term is still open.
    Inconclusive,
}

/// A term: a version range with a polarity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term<V> {
    range: Range<V>,
    positive: bool,
}

impl<V: Ord + Clone> Term<V> {
    /// A positive term: the version is in `range`.
    pub fn positive(range: Range<V>) -> Self {
        Term {
            range,
            positive: true,
        }
    }

    /// A negative term: the version is not in `range`.
    pub fn negative(range: Range<V>) -> Self {
        Term {
            range,
            positive: false,
        }
    }

    /// The set of versions this term allows.
    pub fn set(&self) -> Range<V> {
        if self.positive {
            self.range.clone()
        } else {
            self.range.complement()
        }
    }

    /// Whether this term has positive polarity (requires the package to be in
    /// the range, rather than excluded from it).
    pub fn is_positive(&self) -> bool {
        self.positive
    }

    /// The negation of this term.
    pub fn negate(&self) -> Term<V> {
        Term {
            range: self.range.clone(),
            positive: !self.positive,
        }
    }

    /// The universe term: any version, or absent. This is the identity for
    /// [`Term::intersection`] and the accumulated constraint of an unassigned
    /// package.
    pub fn any() -> Self {
        Term::negative(Range::empty())
    }

    /// Whether this term admits the package being absent (its polarity is
    /// negative).
    fn includes_absent(&self) -> bool {
        !self.positive
    }

    /// Whether this term's allowed states are a subset of `other`'s, accounting
    /// for package absence.
    pub fn is_subset_of(&self, other: &Term<V>) -> bool {
        let range_subset = self
            .set()
            .intersection(&other.set().complement())
            .is_empty();
        range_subset && (!self.includes_absent() || other.includes_absent())
    }

    /// Whether this term admits no states at all. A negative term always admits
    /// the package being absent, so only an empty positive term is truly empty.
    pub fn is_empty(&self) -> bool {
        self.set().is_empty() && !self.includes_absent()
    }

    /// The conjunction of two terms: a term whose set is the intersection. The
    /// result is positive unless both inputs are negative.
    pub fn intersection(&self, other: &Term<V>) -> Term<V> {
        let set = self.set().intersection(&other.set());
        if self.positive || other.positive {
            Term::positive(set)
        } else {
            Term::negative(set.complement())
        }
    }

    /// The part of this term not covered by `other`, as a positive term, or
    /// `None` if empty. Equivalent to `self ∩ ¬other`.
    pub fn difference(&self, other: &Term<V>) -> Option<Term<V>> {
        let set = self.set().intersection(&other.set().complement());
        if set.is_empty() {
            None
        } else {
            Some(Term::positive(set))
        }
    }

    /// Classify this term against the accumulated constraint term for the
    /// package (the conjunction of its assignments, or [`Term::any`] if none).
    pub fn relation(&self, accumulated: &Term<V>) -> Relation {
        if accumulated.intersection(self).is_empty() {
            Relation::Contradicted
        } else if accumulated.is_subset_of(self) {
            Relation::Satisfied
        } else {
            Relation::Inconclusive
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Bound;

    fn ge(n: i32) -> Range<i32> {
        Range::at_least(n)
    }

    #[test]
    fn relation_is_classified() {
        let term = Term::positive(ge(3));
        // Accumulated >= 5 guarantees >= 3.
        assert_eq!(term.relation(&Term::positive(ge(5))), Relation::Satisfied);
        // The universe (any version or absent) is inconclusive.
        assert_eq!(term.relation(&Term::any()), Relation::Inconclusive);
    }

    #[test]
    fn disjoint_constraint_contradicts() {
        let term = Term::positive(Range::at_least(5));
        let accumulated = Term::positive(Range::interval(Bound::Included(1), Bound::Excluded(3)));
        assert_eq!(term.relation(&accumulated), Relation::Contradicted);
    }

    #[test]
    fn negative_polarity_uses_complement() {
        // "not >= 5" is "< 5".
        let term = Term::negative(Range::at_least(5));
        assert!(term.set().contains(&4));
        assert!(!term.set().contains(&5));
    }
}
