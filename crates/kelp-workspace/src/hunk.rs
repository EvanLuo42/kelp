//! Line hunks of text files.
//!
//! Each hunk carries an anchor that hashes the base-side context around the
//! change: a few lines before it, the lines it deletes and a few lines after
//! it, but not the text it inserts. Editing inside a hunk therefore keeps its
//! anchor, and with it the hunk's changelist assignment.

use std::ops::Range;

use serde::{Deserialize, Serialize};
use similar::{DiffOp, TextDiff};

const CONTEXT: usize = 3;

/// An identifier of a hunk that is stable while the hunk is being edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HunkAnchor(pub [u8; 16]);

/// A contiguous change between a base text and a working text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub anchor: HunkAnchor,
    /// The lines of the base replaced by this hunk.
    pub base: Range<usize>,
    /// The replacement lines, including their line endings.
    pub new_lines: Vec<String>,
}

fn lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

/// Returns the hunks that turn `base` into `working`, in order.
///
/// # Examples
///
/// ```
/// use kelp_workspace::hunk::{apply_hunks, diff_hunks};
///
/// let base = "a\nb\nc\n";
/// let working = "a\nB\nc\n";
/// let hunks = diff_hunks(base, working);
/// assert_eq!(hunks.len(), 1);
/// assert_eq!(apply_hunks(base, &hunks), working);
/// ```
pub fn diff_hunks(base: &str, working: &str) -> Vec<Hunk> {
    let base_lines = lines(base);
    let work_lines = lines(working);
    let diff = TextDiff::from_slices(&base_lines, &work_lines);
    diff.ops()
        .iter()
        .filter(|op| !matches!(op, DiffOp::Equal { .. }))
        .map(|op| {
            let old = op.old_range();
            let mut h = blake3::Hasher::new();
            for l in &base_lines[old.start.saturating_sub(CONTEXT)..old.start] {
                h.update(l.as_bytes());
            }
            h.update(b"\0-");
            for l in &base_lines[old.clone()] {
                h.update(l.as_bytes());
            }
            h.update(b"\0+");
            for l in &base_lines[old.end..(old.end + CONTEXT).min(base_lines.len())] {
                h.update(l.as_bytes());
            }
            let mut anchor = [0u8; 16];
            anchor.copy_from_slice(&h.finalize().as_bytes()[..16]);
            Hunk {
                anchor: HunkAnchor(anchor),
                base: old,
                new_lines: work_lines[op.new_range()]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            }
        })
        .collect()
}

/// Applies a subset of the hunks returned by one [`diff_hunks`] call to `base`.
pub fn apply_hunks<'a>(base: &str, hunks: impl IntoIterator<Item = &'a Hunk>) -> String {
    let base_lines = lines(base);
    let mut hunks: Vec<&Hunk> = hunks.into_iter().collect();
    hunks.sort_by_key(|h| h.base.start);
    let mut out = String::with_capacity(base.len());
    let mut pos = 0;
    for h in hunks {
        out.extend(base_lines[pos..h.base.start].iter().copied());
        out.extend(h.new_lines.iter().map(String::as_str));
        pos = h.base.end;
    }
    out.extend(base_lines[pos..].iter().copied());
    out
}
