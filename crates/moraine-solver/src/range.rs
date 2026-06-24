//! Version ranges as canonical sorted sets of disjoint intervals.
//!
//! A [`Range`] is generic over an ordered version type. Set operations return
//! canonical results so that equal sets have equal representations. Union is
//! derived from intersection and complement via De Morgan, keeping the
//! primitive operations small.

use std::ops::Bound;

/// A position on the extended version line, used to compare interval endpoints
/// with correct inclusive/exclusive semantics.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Edge<V> {
    NegInf,
    At(V, Side),
    PosInf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Side {
    Before,
    After,
}

impl<V: Ord> Edge<V> {
    fn cmp(&self, other: &Edge<V>) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        match (self, other) {
            (Edge::NegInf, Edge::NegInf) | (Edge::PosInf, Edge::PosInf) => Equal,
            (Edge::NegInf, _) | (_, Edge::PosInf) => Less,
            (Edge::PosInf, _) | (_, Edge::NegInf) => Greater,
            (Edge::At(a, sa), Edge::At(b, sb)) => a.cmp(b).then(sa.cmp(sb)),
        }
    }
}

fn lower_edge<V: Clone>(b: &Bound<V>) -> Edge<V> {
    match b {
        Bound::Unbounded => Edge::NegInf,
        Bound::Included(v) => Edge::At(v.clone(), Side::Before),
        Bound::Excluded(v) => Edge::At(v.clone(), Side::After),
    }
}

fn upper_edge<V: Clone>(b: &Bound<V>) -> Edge<V> {
    match b {
        Bound::Unbounded => Edge::PosInf,
        Bound::Included(v) => Edge::At(v.clone(), Side::After),
        Bound::Excluded(v) => Edge::At(v.clone(), Side::Before),
    }
}

fn edge_to_lower<V>(e: Edge<V>) -> Bound<V> {
    match e {
        Edge::NegInf => Bound::Unbounded,
        Edge::At(v, Side::Before) => Bound::Included(v),
        Edge::At(v, Side::After) => Bound::Excluded(v),
        Edge::PosInf => Bound::Excluded(unreachable_pos()),
    }
}

fn unreachable_pos<V>() -> V {
    unreachable!("a positive-infinity edge is never a lower bound")
}

/// A canonical set of versions, stored as sorted disjoint half-open intervals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range<V> {
    /// Sorted, disjoint, non-adjacent `(lower, upper)` segments.
    segments: Vec<(Bound<V>, Bound<V>)>,
}

impl<V: Ord + Clone> Range<V> {
    /// The empty range (no versions).
    pub fn empty() -> Self {
        Range { segments: vec![] }
    }

    /// The full range (all versions).
    pub fn full() -> Self {
        Range {
            segments: vec![(Bound::Unbounded, Bound::Unbounded)],
        }
    }

    /// A single interval `[lower, upper]` using the given bounds.
    pub fn interval(lower: Bound<V>, upper: Bound<V>) -> Self {
        if lower_edge(&lower).cmp(&upper_edge(&upper)) == std::cmp::Ordering::Less {
            Range {
                segments: vec![(lower, upper)],
            }
        } else {
            Range::empty()
        }
    }

    /// The single-version range `{v}`.
    pub fn singleton(v: V) -> Self {
        Range::interval(Bound::Included(v.clone()), Bound::Included(v))
    }

    /// `>= v`.
    pub fn at_least(v: V) -> Self {
        Range::interval(Bound::Included(v), Bound::Unbounded)
    }

    /// `> v`.
    pub fn greater(v: V) -> Self {
        Range::interval(Bound::Excluded(v), Bound::Unbounded)
    }

    /// `<= v`.
    pub fn at_most(v: V) -> Self {
        Range::interval(Bound::Unbounded, Bound::Included(v))
    }

    /// `< v`.
    pub fn less(v: V) -> Self {
        Range::interval(Bound::Unbounded, Bound::Excluded(v))
    }

    /// Whether the range is empty.
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Whether `v` is contained in the range.
    pub fn contains(&self, v: &V) -> bool {
        let point_before = Edge::At(v.clone(), Side::Before);
        let point_after = Edge::At(v.clone(), Side::After);
        self.segments.iter().any(|(lo, up)| {
            lower_edge(lo).cmp(&point_after) == std::cmp::Ordering::Less
                && point_before.cmp(&upper_edge(up)) == std::cmp::Ordering::Less
        })
    }

    /// The intersection of two ranges.
    pub fn intersection(&self, other: &Range<V>) -> Range<V> {
        let mut out: Vec<(Bound<V>, Bound<V>)> = Vec::new();
        for (l1, u1) in &self.segments {
            for (l2, u2) in &other.segments {
                let lo = max_lower(l1, l2);
                let up = min_upper(u1, u2);
                if lower_edge(&lo).cmp(&upper_edge(&up)) == std::cmp::Ordering::Less {
                    out.push((lo, up));
                }
            }
        }
        Range { segments: out }.normalized()
    }

    /// The complement of the range (all versions not in it).
    pub fn complement(&self) -> Range<V> {
        let mut out: Vec<(Bound<V>, Bound<V>)> = Vec::new();
        let mut cursor = Edge::NegInf;
        for (lo, up) in &self.segments {
            let seg_lo = lower_edge(lo);
            if cursor.cmp(&seg_lo) == std::cmp::Ordering::Less {
                out.push((edge_to_lower(cursor.clone()), gap_upper(&seg_lo)));
            }
            cursor = upper_edge(up);
        }
        if cursor.cmp(&Edge::PosInf) == std::cmp::Ordering::Less {
            out.push((gap_lower(&cursor), Bound::Unbounded));
        }
        Range { segments: out }.normalized()
    }

    /// The union of two ranges.
    pub fn union(&self, other: &Range<V>) -> Range<V> {
        self.complement()
            .intersection(&other.complement())
            .complement()
    }

    fn normalized(mut self) -> Range<V> {
        self.segments
            .sort_by(|a, b| lower_edge(&a.0).cmp(&lower_edge(&b.0)));
        let mut merged: Vec<(Bound<V>, Bound<V>)> = Vec::new();
        for (lo, up) in self.segments {
            if let Some(last) = merged.last_mut() {
                // Merge if this segment starts at or before the end of the last,
                // including adjacency (touching edges).
                if lower_edge(&lo).cmp(&upper_edge(&last.1)) != std::cmp::Ordering::Greater
                    || adjacent(&last.1, &lo)
                {
                    if upper_edge(&up).cmp(&upper_edge(&last.1)) == std::cmp::Ordering::Greater {
                        last.1 = up;
                    }
                    continue;
                }
            }
            merged.push((lo, up));
        }
        Range { segments: merged }
    }
}

fn max_lower<V: Ord + Clone>(a: &Bound<V>, b: &Bound<V>) -> Bound<V> {
    if lower_edge(a).cmp(&lower_edge(b)) == std::cmp::Ordering::Greater {
        a.clone()
    } else {
        b.clone()
    }
}

fn min_upper<V: Ord + Clone>(a: &Bound<V>, b: &Bound<V>) -> Bound<V> {
    if upper_edge(a).cmp(&upper_edge(b)) == std::cmp::Ordering::Less {
        a.clone()
    } else {
        b.clone()
    }
}

/// The upper bound of a complement gap that ends just before `seg_lo`.
fn gap_upper<V: Clone>(seg_lo: &Edge<V>) -> Bound<V> {
    match seg_lo {
        Edge::At(v, Side::Before) => Bound::Excluded(v.clone()),
        Edge::At(v, Side::After) => Bound::Included(v.clone()),
        _ => Bound::Unbounded,
    }
}

/// The lower bound of a complement gap that starts just after `cursor`.
fn gap_lower<V: Clone>(cursor: &Edge<V>) -> Bound<V> {
    match cursor {
        Edge::At(v, Side::After) => Bound::Excluded(v.clone()),
        Edge::At(v, Side::Before) => Bound::Included(v.clone()),
        _ => Bound::Unbounded,
    }
}

/// Whether two consecutive segment edges touch (end of one equals start of the
/// next as the same point), so the segments should merge.
fn adjacent<V: Ord + Clone>(last_upper: &Bound<V>, next_lower: &Bound<V>) -> bool {
    matches!(
        (last_upper, next_lower),
        (Bound::Included(a), Bound::Excluded(b))
            | (Bound::Excluded(a), Bound::Included(b))
            | (Bound::Included(a), Bound::Included(b))
        if a == b
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(lo: i32, hi: i32) -> Range<i32> {
        Range::interval(Bound::Included(lo), Bound::Excluded(hi))
    }

    #[test]
    fn contains_basic() {
        let range = r(1, 5);
        assert!(range.contains(&1));
        assert!(range.contains(&4));
        assert!(!range.contains(&5));
        assert!(!range.contains(&0));
    }

    #[test]
    fn empty_and_full() {
        assert!(Range::<i32>::empty().is_empty());
        assert!(!Range::<i32>::full().is_empty());
        assert!(Range::<i32>::full().contains(&12345));
    }

    #[test]
    fn intersection_and_union() {
        let a = r(1, 5);
        let b = r(3, 8);
        assert_eq!(a.intersection(&b), r(3, 5));
        assert_eq!(a.union(&b), r(1, 8));
    }

    #[test]
    fn complement_involution() {
        let a = r(1, 5).union(&r(10, 20));
        assert_eq!(a.complement().complement(), a);
    }

    #[test]
    fn equal_sets_equal_representation() {
        let a = r(1, 5).union(&r(5, 9)); // adjacent, merges to [1,9)
        let b = r(1, 9);
        assert_eq!(a, b);
    }

    #[test]
    fn de_morgan() {
        let a = r(1, 5);
        let b = r(3, 9);
        let lhs = a.intersection(&b).complement();
        let rhs = a.complement().union(&b.complement());
        assert_eq!(lhs, rhs);
    }
}
