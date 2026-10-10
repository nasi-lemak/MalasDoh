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

/// A `diff -u` style patch, for copying into a ticket or change log.
pub fn unified(old: &str, new: &str, path: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{}", path.trim_start_matches('/')), &format!("b/{}", path.trim_start_matches('/')))
        .to_string()
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
    fn identical_is_empty() {
        assert!(diff("x\n", "x\n", 3).is_empty());
    }

    #[test]
    fn unified_has_headers() {
        let u = unified("a\n", "b\n", "/etc/hosts");
        assert!(u.starts_with("--- a/etc/hosts\n+++ b/etc/hosts\n"), "{u}");
        assert!(u.contains("-a\n+b\n"));
    }
}
