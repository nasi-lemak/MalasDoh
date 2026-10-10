//! Line diffs for "review before saving" and for conflict screens.

use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub new_start: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Default)]
pub struct Diff {
    pub hunks: Vec<Hunk>,
    pub added: usize,
    pub removed: usize,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }
}

/// Compare two texts line by line, keeping `context` unchanged lines around
/// each change.
pub fn diff(old: &str, new: &str, context: usize) -> Diff {
    let td = TextDiff::from_lines(old, new);
    let mut out = Diff::default();
    for group in td.grouped_ops(context) {
        let mut hunk = Hunk { old_start: 0, new_start: 0, lines: Vec::new() };
        for op in &group {
            for change in td.iter_changes(op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => LineKind::Context,
                    ChangeTag::Insert => {
                        out.added += 1;
                        LineKind::Added
                    }
                    ChangeTag::Delete => {
                        out.removed += 1;
                        LineKind::Removed
                    }
                };
                let mut text = change.value().to_string();
                if text.ends_with('\n') {
                    text.pop();
                }
                hunk.lines.push(DiffLine {
                    kind,
                    old_no: change.old_index().map(|i| i + 1),
                    new_no: change.new_index().map(|i| i + 1),
                    text,
                });
            }
        }
        hunk.old_start = hunk.lines.iter().find_map(|l| l.old_no).unwrap_or(0);
        hunk.new_start = hunk.lines.iter().find_map(|l| l.new_no).unwrap_or(0);
        out.hunks.push(hunk);
    }
    out
}

/// One side's edit to the common ancestor: replace `base[start..end]` with `lines`.
#[derive(Debug, PartialEq)]
struct Change<'a> {
    start: usize,
    end: usize,
    lines: Vec<&'a str>,
}

fn changes<'a>(base: &[&'a str], other: &[&'a str]) -> Vec<Change<'a>> {
    let mut out: Vec<Change<'a>> = Vec::new();
    for op in similar::capture_diff_slices(similar::Algorithm::Myers, base, other) {
        let (old, new) = (op.old_range(), op.new_range());
        if op.tag() == similar::DiffTag::Equal {
            continue;
        }
        // Consecutive delete+insert from the differ are one replacement.
        match out.last_mut() {
            Some(last) if last.end == old.start => {
                last.end = old.end;
                last.lines.extend_from_slice(&other[new]);
            }
            _ => out.push(Change { start: old.start, end: old.end, lines: other[new].to_vec() }),
        }
    }
    out
}

/// Three-way merge of line-based edits: `mine` and `theirs` both started from
/// `base`. Returns the text with both sets of edits applied, or `None` if they
/// touch the same or neighbouring lines. Deliberately conservative: when in
/// doubt it refuses, and the user decides.
pub fn merge3(base: &str, mine: &str, theirs: &str) -> Option<String> {
    let b: Vec<&str> = base.split_inclusive('\n').collect();
    let m: Vec<&str> = mine.split_inclusive('\n').collect();
    let t: Vec<&str> = theirs.split_inclusive('\n').collect();
    let (ours, other) = (changes(&b, &m), changes(&b, &t));
    let mut all: Vec<&Change> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < ours.len() || j < other.len() {
        match (ours.get(i), other.get(j)) {
            (Some(a), Some(c)) if a == c => {
                all.push(a);
                i += 1;
                j += 1;
            }
            (Some(a), Some(c)) => {
                // Overlapping or touching ranges (including two insertions at
                // the same spot) are a real conflict.
                if a.start <= c.end && c.start <= a.end {
                    return None;
                }
                if a.start < c.start {
                    all.push(a);
                    i += 1;
                } else {
                    all.push(c);
                    j += 1;
                }
            }
            (Some(a), None) => {
                all.push(a);
                i += 1;
            }
            (None, Some(c)) => {
                all.push(c);
                j += 1;
            }
            (None, None) => unreachable!(),
        }
    }
    let mut out = String::with_capacity(theirs.len().max(mine.len()));
    let mut pos = 0;
    for c in all {
        out.extend(b[pos..c.start].iter().copied());
        out.extend(c.lines.iter().copied());
        pos = c.end;
    }
    out.extend(b[pos..].iter().copied());
    Some(out)
}

/// How a stretch of lines differs from the original, for the editor's gutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    Added,
    Changed,
    /// Lines were removed just before `start`.
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineMark {
    pub kind: MarkKind,
    /// First line (0-based) in the new text.
    pub start: usize,
    /// Lines covered in the new text (0 for a deletion).
    pub len: usize,
    /// What was there before (empty for an addition).
    pub old: String,
}

/// Which lines of `new` differ from `old`. Gives up on an exact answer
/// after a short time, so huge, very different files stay responsive.
pub fn line_marks(old: &str, new: &str) -> Vec<LineMark> {
    use similar::DiffOp;
    let td = TextDiff::configure().timeout(std::time::Duration::from_millis(150)).diff_lines(old, new);
    let old_lines = td.old_slices();
    let old_text = |at: usize, len: usize| old_lines[at..at + len].concat();
    let mut out: Vec<LineMark> = Vec::new();
    for op in td.ops() {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Insert { new_index, new_len, .. } => {
                // A deletion right before turns this into a change.
                match out.last_mut() {
                    Some(m) if m.kind == MarkKind::Deleted && m.start == new_index => {
                        m.kind = MarkKind::Changed;
                        m.len = new_len;
                    }
                    _ => {
                        out.push(LineMark { kind: MarkKind::Added, start: new_index, len: new_len, old: String::new() })
                    }
                }
            }
            DiffOp::Delete { old_index, old_len, new_index } => out.push(LineMark {
                kind: MarkKind::Deleted,
                start: new_index,
                len: 0,
                old: old_text(old_index, old_len),
            }),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => out.push(LineMark {
                kind: MarkKind::Changed,
                start: new_index,
                len: new_len,
                old: old_text(old_index, old_len),
            }),
        }
    }
    out
}

/// A `diff -u` style patch, for copying into a ticket or change log.
pub fn unified(old: &str, new: &str, path: &str) -> String {
    let p = path.trim_start_matches('/');
    unified_labeled(old, new, &format!("a/{p}"), &format!("b/{p}"))
}

/// Like [`unified`], with `context` lines around each change.
pub fn unified_context(old: &str, new: &str, path: &str, context: usize) -> String {
    let p = path.trim_start_matches('/');
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(context)
        .header(&format!("a/{p}"), &format!("b/{p}"))
        .to_string()
}

/// A `diff -u` style patch with the given `---`/`+++` names.
pub fn unified_labeled(old: &str, new: &str, old_name: &str, new_name: &str) -> String {
    TextDiff::from_lines(old, new).unified_diff().context_radius(3).header(old_name, new_name).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_numbers() {
        let d = diff("a\nb\nc\n", "a\nB\nc\nd\n", 1);
        assert_eq!((d.added, d.removed), (2, 1));
        assert_eq!(d.hunks.len(), 1);
        let removed = d.hunks[0].lines.iter().find(|l| l.kind == LineKind::Removed).unwrap();
        assert_eq!((removed.old_no, removed.text.as_str()), (Some(2), "b"));
    }

    #[test]
    fn marks_added_changed_and_deleted_lines() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nB\nc\nnew\nd\n";
        let m = line_marks(old, new);
        let kinds: Vec<(MarkKind, usize, usize)> = m.iter().map(|m| (m.kind, m.start, m.len)).collect();
        assert_eq!(kinds, vec![(MarkKind::Changed, 1, 1), (MarkKind::Added, 3, 1), (MarkKind::Deleted, 5, 0)]);
        assert_eq!(m[0].old, "b\n");
        assert_eq!(m[2].old, "e\n");
        assert!(line_marks(old, old).is_empty());
    }

    #[test]
    fn identical_is_empty() {
        assert!(diff("x\n", "x\n", 3).is_empty());
    }

    #[test]
    fn merges_edits_to_different_lines() {
        let base = "a\nb\nc\nd\ne\nf\n";
        let mine = "a\nB\nc\nd\ne\nf\n";
        let theirs = "a\nb\nc\nd\nE\nf\ng\n";
        assert_eq!(merge3(base, mine, theirs).as_deref(), Some("a\nB\nc\nd\nE\nf\ng\n"));
    }

    #[test]
    fn same_edit_on_both_sides_is_not_a_conflict() {
        assert_eq!(merge3("a\nb\n", "a\nX\n", "a\nX\n").as_deref(), Some("a\nX\n"));
    }

    #[test]
    fn overlapping_or_adjacent_edits_conflict() {
        let base = "a\nb\nc\n";
        assert_eq!(merge3(base, "a\nB\nc\n", "a\nb2\nc\n"), None);
        assert_eq!(merge3(base, "a\nB\nc\n", "a\nb\nC\n"), None, "neighbouring lines");
        assert_eq!(merge3(base, "a\nx\nb\nc\n", "a\ny\nb\nc\n"), None, "two inserts at one spot");
    }

    #[test]
    fn handles_missing_final_newline() {
        assert_eq!(merge3("a\nb\nc", "A\nb\nc", "a\nb\nC").as_deref(), Some("A\nb\nC"));
    }

    #[test]
    fn unified_has_headers() {
        let u = unified("a\n", "b\n", "/etc/hosts");
        assert!(u.starts_with("--- a/etc/hosts\n+++ b/etc/hosts\n"), "{u}");
        assert!(u.contains("-a\n+b\n"));
    }
}
