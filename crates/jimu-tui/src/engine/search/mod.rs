//! [`TextMatcher`] compiles a substring or regex query (smart-case) once and answers match queries; it owns no corpus and no UI state.
//! [`next_index_after`] and [`prev_index_before`] step through a sorted slice of match positions for `n`/`N` traversal, wrapping at the ends.

pub mod matcher;

pub use matcher::{QueryKind, TextMatcher};

/// Position in ascending `sorted` of the first index after `current`, wrapping to the front.
/// Returns `None` when `sorted` is empty.
pub fn next_index_after(sorted: &[usize], current: usize) -> Option<usize> {
    if sorted.is_empty() {
        return None;
    }
    let pos = sorted.partition_point(|&i| i <= current);
    Some(if pos < sorted.len() { pos } else { 0 })
}

/// Position in ascending `sorted` of the last index before `current`, wrapping to the back.
/// Returns `None` when `sorted` is empty.
pub fn prev_index_before(sorted: &[usize], current: usize) -> Option<usize> {
    if sorted.is_empty() {
        return None;
    }
    let pos = sorted.partition_point(|&i| i < current);
    Some(if pos > 0 { pos - 1 } else { sorted.len() - 1 })
}
