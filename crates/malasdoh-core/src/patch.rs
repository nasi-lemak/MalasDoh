//! Apply `diff -u` / `git diff` patches to text.
//!
//! Like `patch`, a hunk may land at a different line than its header says
//! (the file moved on), but its context must match exactly: there is no fuzz,
//! because a config change applied in the wrong place is worse than one that
//! does not apply. A patch that is already in the file is recognised, so
//! applying twice does nothing.

use anyhow::{Result, bail};

/// One file's changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    /// The path as written in the patch (after `a/` / `b/`).
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub hunks: Vec<Hunk>,
}

impl FilePatch {
    /// The path to change: the new name, or the old one for a deletion.
    pub fn path(&self) -> &str {
        self.new_path.as_deref().or(self.old_path.as_deref()).unwrap_or("")
    }

    pub fn is_new_file(&self) -> bool {
        self.old_path.is_none()
    }

    pub fn is_deletion(&self) -> bool {
        self.new_path.is_none()
    }

    /// Lines added and removed.
    pub fn counts(&self) -> (usize, usize) {
        let mut a = 0;
        let mut r = 0;
        for h in &self.hunks {
            for l in &h.lines {
                match l {
                    Line::Add(_) => a += 1,
                    Line::Remove(_) => r += 1,
                    Line::Context(_) => {}
                }
            }
        }
        (a, r)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based line in the original where the hunk starts.
    pub old_start: usize,
    pub lines: Vec<Line>,
}

/// A line with its ending (`"listen 80;\n"`, or without `\n` at the very end
/// of a file that has no final newline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Context(String),
    Add(String),
    Remove(String),
}

impl Hunk {
    fn old_lines(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                Line::Context(s) | Line::Remove(s) => Some(s.as_str()),
                Line::Add(_) => None,
            })
            .collect()
    }

    fn new_lines(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                Line::Context(s) | Line::Add(s) => Some(s.as_str()),
                Line::Remove(_) => None,
            })
            .collect()
    }
}

/// The path from a `---`/`+++` line: without a timestamp, without `a/`/`b/`
/// (git's prefixes), `None` for `/dev/null`.
fn header_path(rest: &str) -> Option<String> {
    let p = rest.split('\t').next().unwrap_or("").trim_end();
    let p = p.trim_matches('"');
    if p == "/dev/null" || p.is_empty() {
        return None;
    }
    let p = p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p);
    Some(p.to_string())
}

/// Parse a patch with one or more files. Text around the diffs (a mail, a
/// commit message, `diff --git` and `index` lines) is ignored.
pub fn parse(text: &str) -> Result<Vec<FilePatch>> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut files = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if !(l.starts_with("--- ") && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ "))) {
            i += 1;
            continue;
        }
        let old_path = header_path(l[4..].trim_end_matches(['\n', '\r']));
        let new_path = header_path(lines[i + 1][4..].trim_end_matches(['\n', '\r']));
        if old_path.is_none() && new_path.is_none() {
            bail!("a file in the patch has no name");
        }
        i += 2;
        let mut hunks = Vec::new();
        while i < lines.len() && lines[i].starts_with("@@") {
            let (old_start, old_len, new_len) = parse_range(lines[i])?;
            i += 1;
            let (mut seen_old, mut seen_new) = (0, 0);
            let mut hl: Vec<Line> = Vec::new();
            while i < lines.len() && (seen_old < old_len || seen_new < new_len) {
                let raw = lines[i];
                // Patches made from CRLF files: the file is compared without \r.
                let body = |s: &str| {
                    let s = s.strip_suffix('\n').unwrap_or(s);
                    format!("{}\n", s.strip_suffix('\r').unwrap_or(s))
                };
                match raw.as_bytes().first() {
                    Some(b' ') => {
                        hl.push(Line::Context(body(&raw[1..])));
                        seen_old += 1;
                        seen_new += 1;
                    }
                    Some(b'-') => {
                        hl.push(Line::Remove(body(&raw[1..])));
                        seen_old += 1;
                    }
                    Some(b'+') => {
                        hl.push(Line::Add(body(&raw[1..])));
                        seen_new += 1;
                    }
                    // Some tools drop the space on empty context lines.
                    Some(b'\n') | Some(b'\r') => {
                        hl.push(Line::Context("\n".into()));
                        seen_old += 1;
                        seen_new += 1;
                    }
                    Some(b'\\') => {}
                    _ => bail!("line {} of the patch does not belong to its hunk: {}", i + 1, raw.trim_end()),
                }
                i += 1;
                no_newline(&lines, &mut i, &mut hl);
            }
            if seen_old != old_len || seen_new != new_len {
                bail!("the patch is cut short (a hunk ends early)");
            }
            hunks.push(Hunk { old_start, lines: hl });
        }
        files.push(FilePatch { old_path, new_path, hunks });
    }
    if files.is_empty() {
        bail!("no file changes found (expected a unified diff: ---, +++ and @@ lines)");
    }
    Ok(files)
}

/// `\ No newline at end of file` after a line: that line has no `\n`.
fn no_newline(lines: &[&str], i: &mut usize, hl: &mut [Line]) {
    if lines.get(*i).is_some_and(|l| l.starts_with("\\ ")) {
        if let Some(Line::Context(s) | Line::Add(s) | Line::Remove(s)) = hl.last_mut() {
            s.pop();
        }
        *i += 1;
    }
}

fn parse_range(line: &str) -> Result<(usize, usize, usize)> {
    // @@ -12,7 +12,8 @@ optional text
    let bad = || anyhow::anyhow!("not a hunk header: {}", line.trim_end());
    let mut parts = line.split_whitespace();
    parts.next();
    let old = parts.next().and_then(|p| p.strip_prefix('-')).ok_or_else(bad)?;
    let new = parts.next().and_then(|p| p.strip_prefix('+')).ok_or_else(bad)?;
    let range = |r: &str| -> Option<(usize, usize)> {
        match r.split_once(',') {
            Some((s, n)) => Some((s.parse().ok()?, n.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (os, ol) = range(old).ok_or_else(bad)?;
    let (_, nl) = range(new).ok_or_else(bad)?;
    Ok((os, ol, nl))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The new text.
    Changed(String),
    /// Every hunk is already in the file.
    AlreadyApplied,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("change {hunk} of {total} does not match the file (near line {line})")]
pub struct HunkFailed {
    pub hunk: usize,
    pub total: usize,
    pub line: usize,
}

/// Where `want` occurs in `have`, nearest to `near` first.
fn find_block(have: &[&str], want: &[&str], near: usize) -> Option<usize> {
    if want.is_empty() {
        return Some(near.min(have.len()));
    }
    if want.len() > have.len() {
        return None;
    }
    let last = have.len() - want.len();
    let matches = |at: usize| have[at..at + want.len()] == *want;
    let near = near.min(last);
    for d in 0..=last {
        if near >= d && matches(near - d) {
            return Some(near - d);
        }
        if near + d <= last && d > 0 && matches(near + d) {
            return Some(near + d);
        }
        if near < d && near + d > last {
            break;
        }
    }
    None
}

/// Apply one file's hunks to `text`.
pub fn apply(text: &str, fp: &FilePatch) -> Result<Applied, HunkFailed> {
    apply_with(text, fp, false)
}

/// Like [`apply`], but each hunk's lines (context and removed) must occur
/// exactly once in `text`, wherever that is. For patches made with little
/// or no context, where "nearest to the old line number" is not safe enough.
pub fn apply_unique(text: &str, fp: &FilePatch) -> Result<Applied, HunkFailed> {
    apply_with(text, fp, true)
}

fn apply_with(text: &str, fp: &FilePatch, unique: bool) -> Result<Applied, HunkFailed> {
    let total = fp.hunks.len();
    match try_apply(text, &fp.hunks, false, unique) {
        Ok(t) => Ok(Applied::Changed(t)),
        Err(e) => {
            // Already there? Then the reverse applies cleanly.
            if try_apply(text, &fp.hunks, true, unique).is_ok() {
                Ok(Applied::AlreadyApplied)
            } else {
                Err(HunkFailed { total, ..e })
            }
        }
    }
}

/// Where `want` occurs in `have`, if it occurs exactly once.
fn find_unique(have: &[&str], want: &[&str]) -> Option<usize> {
    if want.is_empty() || want.len() > have.len() {
        return None;
    }
    let mut hits = (0..=have.len() - want.len()).filter(|&at| have[at..at + want.len()] == *want);
    let first = hits.next()?;
    hits.next().is_none().then_some(first)
}

fn try_apply(text: &str, hunks: &[Hunk], reverse: bool, unique: bool) -> Result<String, HunkFailed> {
    let have: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(have.len() + 16);
    let mut pos = 0; // next line of `have` not yet copied
    let mut shift: isize = 0; // how far hunks have moved so far
    for (n, h) in hunks.iter().enumerate() {
        let (old, new) = if reverse { (h.new_lines(), h.old_lines()) } else { (h.old_lines(), h.new_lines()) };
        let expected = ((h.old_start.max(1) - 1) as isize + shift).max(pos as isize) as usize;
        let fail = HunkFailed { hunk: n + 1, total: hunks.len(), line: h.old_start };
        // Only search forward of what earlier hunks used.
        let at = if unique {
            find_unique(&have, &old).filter(|&a| a >= pos).ok_or(fail)?
        } else {
            find_block(&have[pos..], &old, expected.saturating_sub(pos)).map(|a| a + pos).ok_or(fail)?
        };
        out.extend(have[pos..at].iter().map(|s| s.to_string()));
        out.extend(new.iter().map(|s| s.to_string()));
        shift = at as isize - (h.old_start.max(1) - 1) as isize;
        pos = at + old.len();
    }
    out.extend(have[pos..].iter().map(|s| s.to_string()));
    Ok(out.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = r#"From: someone
Subject: tune nginx

diff --git a/etc/nginx/nginx.conf b/etc/nginx/nginx.conf
index 1111..2222 100644
--- a/etc/nginx/nginx.conf
+++ b/etc/nginx/nginx.conf
@@ -1,4 +1,4 @@
 user www-data;
-worker_processes auto;
+worker_processes 8;
 pid /run/nginx.pid;
 
@@ -8,3 +8,4 @@ events {
 http {
     sendfile on;
+    server_tokens off;
 }
--- /dev/null
+++ b/etc/motd
@@ -0,0 +1,1 @@
+Managed by ops
"#;

    const NGINX: &str = "user www-data;\nworker_processes auto;\npid /run/nginx.pid;\n\nevents {\n    worker_connections 768;\n}\nhttp {\n    sendfile on;\n}\n";

    #[test]
    fn parses_files_and_hunks() {
        let files = parse(PATCH).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path(), "etc/nginx/nginx.conf");
        assert_eq!((files[0].hunks.len(), files[0].counts()), (2, (2, 1)));
        assert!(files[1].is_new_file() && files[1].path() == "etc/motd");
        assert!(parse("just text\n").is_err());
        assert!(parse("--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n a\n").is_err(), "cut short");
    }

    #[test]
    fn applies_moves_and_recognises_applied() {
        let f = &parse(PATCH).unwrap()[0];
        let Applied::Changed(new) = apply(NGINX, f).unwrap() else { panic!() };
        assert!(new.contains("worker_processes 8;\n") && new.contains("    server_tokens off;\n}\n"), "{new}");
        assert_eq!(apply(&new, f).unwrap(), Applied::AlreadyApplied);

        // Two lines added at the top: the hunks still land on their context.
        let moved = format!("# managed\n# by hand\n{NGINX}");
        let Applied::Changed(m) = apply(&moved, f).unwrap() else { panic!() };
        assert!(m.starts_with("# managed\n# by hand\nuser www-data;\nworker_processes 8;\n"), "{m}");

        // Context that is not there: refused, nothing guessed.
        let other = NGINX.replace("pid /run/nginx.pid;", "pid /var/run/nginx.pid;");
        let err = apply(&other, f).unwrap_err();
        assert_eq!((err.hunk, err.total), (1, 2));
    }

    #[test]
    fn final_newline_and_crlf_patches() {
        let p = "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n";
        let f = &parse(p).unwrap()[0];
        assert_eq!(apply("a\nb", f).unwrap(), Applied::Changed("a\nb\n".into()));
        let crlf = "--- a/x\r\n+++ b/x\r\n@@ -1 +1 @@\r\n-old\r\n+new\r\n";
        let f = &parse(crlf).unwrap()[0];
        assert_eq!(apply("old\n", f).unwrap(), Applied::Changed("new\n".into()));
        // New file: applies to empty text.
        let f = &parse(PATCH).unwrap()[1];
        assert_eq!(apply("", f).unwrap(), Applied::Changed("Managed by ops\n".into()));
    }

    #[test]
    fn unique_needs_exactly_one_match() {
        let fp = parse("--- a/x\n+++ b/x\n@@ -2,1 +2,1 @@\n-timeout 30\n+timeout 60\n").unwrap().remove(0);
        assert_eq!(apply_unique("a\ntimeout 30\nb\n", &fp), Ok(Applied::Changed("a\ntimeout 60\nb\n".into())));
        assert!(apply_unique("timeout 30\nb\ntimeout 30\n", &fp).is_err());
        assert_eq!(apply_unique("a\ntimeout 60\n", &fp), Ok(Applied::AlreadyApplied));
    }
}
