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

/// Leading spaces and tabs of `line`.
fn lead_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

fn opener_of(closer: char) -> Option<char> {
    match closer {
        '}' => Some('{'),
        ']' => Some('['),
        ')' => Some('('),
        _ => None,
    }
}

/// The indentation of the line holding the bracket that a `closer` typed
/// at byte `at` would close: the nearest unmatched opener of its kind
/// before it. Brackets inside quotes on the way are not counted.
fn opener_indent(text: &str, at: usize, closer: char) -> Option<String> {
    let open = opener_of(closer)?;
    let mut depth = 0usize;
    // Walk back line by line, so quotes can be paired within a line.
    let mut end = at;
    loop {
        let start = text[..end].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line = &text[start..end];
        let mut in_quote = None;
        let mut marks = Vec::new();
        for (i, c) in line.char_indices() {
            match (c, in_quote) {
                ('"' | '\'', None) => in_quote = Some(c),
                (q, Some(o)) if q == o => in_quote = None,
                (c, None) if c == open || c == closer => marks.push((i, c)),
                _ => {}
            }
        }
        for &(_, c) in marks.iter().rev() {
            if c == closer {
                depth += 1;
            } else if depth == 0 {
                return Some(lead_of(&text[start..]).to_string());
            } else {
                depth -= 1;
            }
        }
        if start == 0 {
            return None;
        }
        end = start - 1;
    }
}

/// One indent unit less than `lead` (for a closer with no opener found).
fn dedented(lead: &str, unit: Indent) -> String {
    match unit {
        _ if lead.ends_with('\t') => lead[..lead.len() - 1].to_string(),
        Indent::Spaces(n) => {
            let spaces = lead.len() - lead.trim_end_matches(' ').len();
            lead[..lead.len() - spaces.min(n)].to_string()
        }
        Indent::Tab => lead.to_string(),
    }
}

/// Enter with indentation kept: replaces the selection `sel` (char range)
/// with a newline plus the current line's leading whitespace, one level more
/// after an opening `{`, `[`, `(` or a YAML-style `key:`. When the rest of
/// the line starts with a closing bracket, that goes on the next line at its
/// opener's indentation (`{|}` becomes three lines, cursor in the middle).
/// Returns the new cursor (char index).
pub fn newline_with_indent(text: &mut String, sel: Range<usize>, indent: Indent) -> usize {
    let (a, b) = (char_to_byte(text, sel.start), char_to_byte(text, sel.end));
    let line = line_at(text, a);
    let before = &text[line.start..a];
    let lead = lead_of(before).to_string();
    let opener = before.trim_end();
    let opens = opener.ends_with(['{', '[', '(']) || (opener.ends_with(':') && !opener.trim_start().starts_with('#'));
    let rest = text[b..line.end.max(b)].trim_start_matches([' ', '\t']);
    let closer = rest.chars().next().filter(|c| opener_of(*c).is_some());
    let mut ins = format!("\n{lead}");
    if opens {
        ins.push_str(&indent.unit());
    }
    let cursor = a + ins.len();
    if let Some(c) = closer {
        // The closer moves down, lined up with its opener.
        let close_lead = opener_indent(text, a, c).unwrap_or_else(|| dedented(&lead, indent));
        if opens {
            ins.push_str(&format!("\n{close_lead}"));
        } else {
            ins = format!("\n{close_lead}");
        }
        // Spaces between the cursor and the closer go.
        let skip = text[b..].len() - text[b..].trim_start_matches([' ', '\t']).len();
        text.replace_range(a..b + skip, &ins);
        return byte_to_char(text, if opens { cursor } else { a + ins.len() });
    }
    text.replace_range(a..b, &ins);
    byte_to_char(text, cursor)
}

/// Typing a closing bracket `c` over the selection `sel`: on a line that is
/// only indentation so far, the line is first lined up with the bracket's
/// opener. Returns the new cursor (char index).
pub fn type_closer(text: &mut String, sel: Range<usize>, c: char, indent: Indent) -> usize {
    let (a, b) = (char_to_byte(text, sel.start), char_to_byte(text, sel.end));
    let line = line_at(text, a);
    let before = &text[line.start..a];
    if !before.is_empty() && before.trim().is_empty() {
        let lead = opener_indent(text, line.start, c).unwrap_or_else(|| dedented(before, indent));
        let ins = format!("{lead}{c}");
        text.replace_range(line.start..b, &ins);
        return byte_to_char(text, line.start + ins.len());
    }
    let mut buf = [0u8; 4];
    let ins = c.encode_utf8(&mut buf);
    text.replace_range(a..b, ins);
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
    fn a_closing_bracket_lines_up_with_its_opener() {
        // { Enter, a line, Enter, } : the brace goes back to the left.
        let mut t = String::from("{");
        let mut c = newline_with_indent(&mut t, 1..1, Indent::Spaces(4));
        assert_eq!(t, "{\n    ");
        t.insert_str(char_to_byte(&t, c), "a : \"\"");
        c += 6;
        c = newline_with_indent(&mut t, c..c, Indent::Spaces(4));
        assert_eq!(t, "{\n    a : \"\"\n    ");
        let c = type_closer(&mut t, c..c, '}', Indent::Spaces(4));
        assert_eq!(t, "{\n    a : \"\"\n}");
        assert_eq!(c, t.chars().count());

        // Nested, with its opener's own indentation; tabs too.
        let mut t = String::from("server {\n    location / {\n        root x;\n        ");
        let end = t.chars().count();
        type_closer(&mut t, end..end, '}', Indent::Spaces(4));
        assert!(t.ends_with("root x;\n    }"), "{t:?}");
        let mut t = String::from("[\n\t1,\n\t");
        let end = t.chars().count();
        type_closer(&mut t, end..end, ']', Indent::Tab);
        assert_eq!(t, "[\n\t1,\n]");

        // Brackets in strings and already closed pairs do not count.
        let mut t = String::from("  a {\n    b = \"}\";\n    c { d }\n    ");
        let end = t.chars().count();
        type_closer(&mut t, end..end, '}', Indent::Spaces(2));
        assert!(t.ends_with("c { d }\n  }"), "{t:?}");

        // After other text on the line, it is just typed.
        let mut t = String::from("    x = {");
        let end = t.chars().count();
        type_closer(&mut t, end..end, '}', Indent::Spaces(4));
        assert_eq!(t, "    x = {}");

        // No opener in sight: one level less.
        let mut t = String::from("        ");
        type_closer(&mut t, 8..8, ')', Indent::Spaces(4));
        assert_eq!(t, "    )");
    }

    #[test]
    fn enter_between_brackets_opens_them_up() {
        let mut t = String::from("  x = {}");
        let c = newline_with_indent(&mut t, 7..7, Indent::Spaces(2));
        assert_eq!(t, "  x = {\n    \n  }");
        assert_eq!(c, "  x = {\n    ".len());
        // Before a closer that is not right after an opener.
        let mut t = String::from("{\n    a: 1}");
        let at = t.chars().count() - 1;
        let c = newline_with_indent(&mut t, at..at, Indent::Spaces(4));
        assert_eq!(t, "{\n    a: 1\n}");
        assert_eq!(c, t.chars().count() - 1);
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
