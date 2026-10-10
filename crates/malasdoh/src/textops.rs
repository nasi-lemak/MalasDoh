//! Text editing operations for the editor, kept free of UI so they can be
//! tested: indentation, find and replace, line positions.
//!
//! Positions are char indices (what egui's cursor uses) unless a name says
//! `byte`.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indent {
    Tab,
    Spaces(usize),
}

impl Indent {
    pub fn unit(self) -> String {
        match self {
            Indent::Tab => "\t".into(),
            Indent::Spaces(n) => " ".repeat(n),
        }
    }

    pub fn label(self) -> String {
        match self {
            Indent::Tab => "Tabs".into(),
            Indent::Spaces(n) => format!("{n} spaces"),
        }
    }
}

/// Guess the file's indentation from its indented lines: tabs if most of them
/// start with a tab, otherwise the most common step between indent widths.
pub fn detect_indent(text: &str) -> Indent {
    let (mut tabs, mut spaces) = (0, 0);
    let mut steps = [0usize; 9];
    let mut prev = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            tabs += 1;
            continue;
        }
        let n = line.len() - line.trim_start_matches(' ').len();
        if n > 0 {
            spaces += 1;
        }
        let d = n.abs_diff(prev);
        if (1..=8).contains(&d) {
            steps[d] += 1;
        }
        prev = n;
    }
    if tabs > spaces {
        return Indent::Tab;
    }
    // Prefer 2/4 over noise from odd alignments.
    let best = (1..=8).max_by_key(|&d| (steps[d], d == 4, d == 2)).unwrap_or(4);
    if steps[best] == 0 { Indent::Spaces(4) } else { Indent::Spaces(best) }
}

pub fn char_to_byte(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len())
}

pub fn byte_to_char(s: &str, bi: usize) -> usize {
    s[..bi.min(s.len())].chars().count()
}

/// Byte range of the line containing byte `bi` (without its newline).
fn line_at(s: &str, bi: usize) -> Range<usize> {
    let start = s[..bi].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = s[bi..].find('\n').map(|i| bi + i).unwrap_or(s.len());
    start..end
}

/// Enter with indentation kept: replaces the selection `sel` (char range)
/// with a newline plus the current line's leading whitespace, one level more
/// after an opening `{`, `[`, `(` or a YAML-style `key:`. Returns the new
/// cursor (char index).
pub fn newline_with_indent(text: &mut String, sel: Range<usize>, indent: Indent) -> usize {
    let (a, b) = (char_to_byte(text, sel.start), char_to_byte(text, sel.end));
    let line = line_at(text, a);
    let before = &text[line.start..a];
    let lead: String = before.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let opener = before.trim_end();
    let mut ins = format!("\n{lead}");
    if opener.ends_with(['{', '[', '(']) || (opener.ends_with(':') && !opener.trim_start().starts_with('#')) {
        ins.push_str(&indent.unit());
    }
    text.replace_range(a..b, &ins);
    byte_to_char(text, a + ins.len())
}

/// Tab / Shift+Tab. With a selection spanning lines (or `dedent`), every
/// touched line is shifted; otherwise one indent unit is inserted at the
/// cursor. Returns the new selection.
pub fn indent(text: &mut String, sel: Range<usize>, unit: Indent, dedent: bool) -> Range<usize> {
    let (a, b) = (char_to_byte(text, sel.start), char_to_byte(text, sel.end));
    let multi = text[a..b].contains('\n');
    if !multi && !dedent {
        let u = unit.unit();
        text.replace_range(a..b, &u);
        let c = byte_to_char(text, a + u.len());
        return c..c;
    }
    let first = line_at(text, a).start;
    // A selection ending at the start of a line does not include that line.
    let last_end = if b > a && text[..b].ends_with('\n') { b - 1 } else { b };
    let last = line_at(text, last_end).end;
    let block = text[first..last].to_string();
    let u = unit.unit();
    let width = match unit {
        Indent::Tab => 4,
        Indent::Spaces(n) => n,
    };
    let mut out = String::with_capacity(block.len() + 16);
    let (mut delta_first, mut delta_total) = (0isize, 0isize);
    for (i, line) in block.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let changed = if dedent {
            // One tab, or up to one indent's worth of spaces.
            let remove =
                if line.starts_with('\t') { 1 } else { line.chars().take_while(|c| *c == ' ').count().min(width) };
            out.push_str(&line[remove..]);
            -(remove as isize)
        } else if line.is_empty() {
            0
        } else {
            out.push_str(&u);
            out.push_str(line);
            u.len() as isize
        };
        if i == 0 {
            delta_first = changed;
        }
        delta_total += changed;
    }
    text.replace_range(first..last, &out);
    let new_a = (a as isize + delta_first).max(first as isize) as usize;
    let new_b = (b as isize + delta_total).max(new_a as isize) as usize;
    byte_to_char(text, new_a)..byte_to_char(text, new_b)
}

/// Byte ranges of every match of `query`.
pub fn find_all(text: &str, query: &str, case_sensitive: bool) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    if case_sensitive {
        return text.match_indices(query).map(|(i, m)| i..i + m.len()).collect();
    }
    // Lowercasing can change byte lengths (e.g. 'İ'); map back through chars.
    let hay: Vec<(usize, char)> = text.char_indices().collect();
    let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < hay.len() {
        let mut j = 0;
        let mut k = i;
        while j < needle.len() && k < hay.len() {
            let lc: Vec<char> = hay[k].1.to_lowercase().collect();
            if needle[j..].starts_with(&lc) {
                j += lc.len();
                k += 1;
            } else {
                break;
            }
        }
        if j == needle.len() {
            let end = hay.get(k).map(|c| c.0).unwrap_or(text.len());
            out.push(hay[i].0..end);
            i = k.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// Replace every match; returns how many were replaced.
pub fn replace_all(text: &mut String, query: &str, with: &str, case_sensitive: bool) -> usize {
    let hits = find_all(text, query, case_sensitive);
    for r in hits.iter().rev() {
        text.replace_range(r.clone(), with);
    }
    hits.len()
}

/// Char index where 1-based `line` starts (clamped to the last line).
pub fn line_start(text: &str, line: usize) -> usize {
    let mut ci = 0;
    let mut current = 1;
    for c in text.chars() {
        if current >= line {
            break;
        }
        ci += 1;
        if c == '\n' {
            current += 1;
        }
    }
    if current < line {
        // Past the end: the start of the last line.
        let last = text.trim_end_matches('\n').rfind('\n').map(|i| i + 1).unwrap_or(0);
        return byte_to_char(text, last);
    }
    ci
}

/// 0-based line containing byte `bi`.
pub fn line_of(text: &str, bi: usize) -> usize {
    text[..bi.min(text.len())].matches('\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_indentation() {
        assert_eq!(detect_indent("a {\n    b;\n    c {\n        d;\n    }\n}\n"), Indent::Spaces(4));
        assert_eq!(detect_indent("a:\n  b: 1\n  c:\n    d: 2\n"), Indent::Spaces(2));
        assert_eq!(detect_indent("a {\n\tb;\n\tc;\n}\n"), Indent::Tab);
        assert_eq!(detect_indent("no indent\n"), Indent::Spaces(4));
    }

    #[test]
    fn enter_keeps_indentation() {
        let mut t = String::from("http {\n    listen 80;\n}");
        let at = byte_to_char(&t, "http {\n    listen 80;".len());
        let c = newline_with_indent(&mut t, at..at, Indent::Spaces(4));
        assert_eq!(t, "http {\n    listen 80;\n    \n}");
        assert_eq!(c, byte_to_char(&t, "http {\n    listen 80;\n    ".len()));

        // One level deeper after an opening brace.
        let mut t = String::from("    server {");
        let end = t.chars().count();
        newline_with_indent(&mut t, end..end, Indent::Spaces(4));
        assert_eq!(t, "    server {\n        ");

        // YAML key, tabs, and a selection being replaced.
        let mut t = String::from("services:");
        newline_with_indent(&mut t, 9..9, Indent::Spaces(2));
        assert_eq!(t, "services:\n  ");
        let mut t = String::from("\ta = 1 XYZ");
        newline_with_indent(&mut t, 7..10, Indent::Tab);
        assert_eq!(t, "\ta = 1 \n\t");
    }

    #[test]
    fn tab_and_shift_tab() {
        let mut t = String::from("ab");
        let s = indent(&mut t, 1..1, Indent::Spaces(4), false);
        assert_eq!((t.as_str(), s), ("a    b", 5..5));

        let mut t = String::from("a\nb\n\nc\n");
        let s = indent(&mut t, 0..6, Indent::Spaces(2), false);
        assert_eq!(t, "  a\n  b\n\n  c\n", "blank lines stay blank");
        let s = indent(&mut t, s, Indent::Spaces(2), true);
        assert_eq!(t, "a\nb\n\nc\n");
        assert_eq!(s.start, 0);

        let mut t = String::from("\tx\n\t\ty");
        let all = t.chars().count();
        indent(&mut t, 0..all, Indent::Tab, true);
        assert_eq!(t, "x\n\ty");

        // Dedent never removes more than the indent there is.
        let mut t = String::from(" x");
        indent(&mut t, 0..0, Indent::Spaces(4), true);
        assert_eq!(t, "x");
    }

    #[test]
    fn find_and_replace() {
        let t = "Listen 80\nlisten 443\n";
        assert_eq!(find_all(t, "listen", true), vec![10..16]);
        assert_eq!(find_all(t, "LISTEN", false), vec![0..6, 10..16]);
        let mut t = String::from(t);
        assert_eq!(replace_all(&mut t, "listen", "listen", false), 2);
        assert_eq!(t, "listen 80\nlisten 443\n");
        assert_eq!(find_all("ÄÄ ää", "ää", false).len(), 2);
        assert!(find_all("x", "", false).is_empty());
    }

    #[test]
    fn line_positions() {
        let t = "one\ntwo\nthree\n";
        assert_eq!(line_start(t, 1), 0);
        assert_eq!(line_start(t, 2), 4);
        assert_eq!(line_start(t, 3), 8);
        assert_eq!(line_start(t, 99), 8);
        assert_eq!(line_of(t, 9), 2);
    }
}
