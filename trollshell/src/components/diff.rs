//! Generic keyed-diff planner for turning a `Vec<T>` signal into in-place
//! GTK widget updates instead of a teardown-and-rebuild on every emission.
//!
//! This is the same shape as `widgets/tray.rs`'s `plan_diff`/`DiffOp` (#198),
//! pulled out so `widgets/workspaces.rs` and `widgets/window_list.rs` (#229)
//! can share it instead of hand-rolling their own copies. `widgets/tray.rs`
//! itself still carries its original, non-generic copy — folding it onto
//! this shared helper is out of scope for #229 (tray.rs wasn't touched) and
//! left for a follow-up.

use std::collections::HashSet;
use std::hash::Hash;

/// Classification of an incoming key in a keyed-diff pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffOp {
    /// The key is new — build a fresh widget.
    Create,
    /// The key was already present — reuse the existing widget.
    Reuse,
}

/// Classify each key in `next` as [`DiffOp::Create`] or [`DiffOp::Reuse`].
///
/// Returns one op per entry in `next` (same order) and the list of keys
/// present in `prev` but absent from `next` (to be removed from the
/// container).
///
/// Pure function — no GTK state; unit-testable without a display.
pub(crate) fn plan_diff<K>(prev: &[K], next: &[K]) -> (Vec<DiffOp>, Vec<K>)
where
    K: Eq + Hash + Clone,
{
    let prev_set: HashSet<&K> = prev.iter().collect();
    let next_set: HashSet<&K> = next.iter().collect();

    let ops = next
        .iter()
        .map(|k| {
            if prev_set.contains(k) {
                DiffOp::Reuse
            } else {
                DiffOp::Create
            }
        })
        .collect();

    let removed = prev
        .iter()
        .filter(|k| !next_set.contains(k))
        .cloned()
        .collect();

    (ops, removed)
}

#[cfg(test)]
mod tests {
    use super::{DiffOp, plan_diff};

    #[test]
    fn diff_no_change() {
        let prev = vec![1u64, 2];
        let next = prev.clone();
        let (ops, removed) = plan_diff(&prev, &next);
        assert_eq!(ops, [DiffOp::Reuse, DiffOp::Reuse]);
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_insert() {
        let prev = vec![1u64, 2];
        let next = vec![1u64, 2, 3];
        let (ops, removed) = plan_diff(&prev, &next);
        assert_eq!(ops, [DiffOp::Reuse, DiffOp::Reuse, DiffOp::Create]);
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_remove() {
        let prev = vec![1u64, 2, 3];
        let next = vec![1u64, 3];
        let (ops, mut removed) = plan_diff(&prev, &next);
        assert_eq!(ops, [DiffOp::Reuse, DiffOp::Reuse]);
        removed.sort_unstable();
        assert_eq!(removed, [2]);
    }

    #[test]
    fn diff_reorder() {
        let prev = vec![1u64, 2, 3];
        let next = vec![3u64, 2, 1];
        let (ops, removed) = plan_diff(&prev, &next);
        assert_eq!(ops, [DiffOp::Reuse, DiffOp::Reuse, DiffOp::Reuse]);
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_full_replace() {
        let prev = vec![1u64, 2];
        let next = vec![3u64, 4];
        let (ops, mut removed) = plan_diff(&prev, &next);
        assert_eq!(ops, [DiffOp::Create, DiffOp::Create]);
        removed.sort_unstable();
        assert_eq!(removed, [1, 2]);
    }

    #[test]
    fn diff_empty_prev() {
        let prev: Vec<u64> = vec![];
        let next = vec![1u64];
        let (ops, removed) = plan_diff(&prev, &next);
        assert_eq!(ops, [DiffOp::Create]);
        assert!(removed.is_empty());
    }

    #[test]
    fn diff_empty_next() {
        let prev = vec![1u64, 2];
        let next: Vec<u64> = vec![];
        let (ops, mut removed) = plan_diff(&prev, &next);
        assert!(ops.is_empty());
        removed.sort_unstable();
        assert_eq!(removed, [1, 2]);
    }
}
