//! Lightweight syntax highlighting for the files people edit on servers.
//!
//! Not a parser: a line tokenizer per format that colours comments, strings,
//! keys/directives, sections, variables and numbers. That is what makes a
//! config readable, and it stays fast on big files.

use std::ops::Range;

use egui::Color32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Plain,
    /// `directive value;` formats: nginx, Apache, sshd_config, HAProxy, …
    Directive,
    /// `key = value` with `[sections]`: INI, systemd units, TOML, php.ini, my.cnf.
    Ini,
    Yaml,
    Json,
    Shell,
    Env,
    Xml,
    Dockerfile,
}

impl Lang {
    pub fn label(self) -> &'static str {
        match self {
            Lang::Plain => "Plain text",
            Lang::Directive => "Config (directives)",
            Lang::Ini => "INI / TOML",
            Lang::Yaml => "YAML",
            Lang::Json => "JSON",
            Lang::Shell => "Shell",
            Lang::Env => ".env",
            Lang::Xml => "XML / HTML",
            Lang::Dockerfile => "Dockerfile",
        }
    }

    pub const ALL: [Lang; 9] = [
        Lang::Plain,
        Lang::Directive,
        Lang::Ini,
        Lang::Yaml,
        Lang::Json,
        Lang::Shell,
        Lang::Env,
        Lang::Xml,
        Lang::Dockerfile,
    ];
}

/// Guess the format from the path and the first line.
pub fn detect(path: &str, text: &str) -> Lang {
    let name = path.rsplit('/').next().unwrap_or(path).to_lowercase();
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let first = text.lines().next().unwrap_or("");
    if first.starts_with("#!") {
        return Lang::Shell;
    }
    match ext {
        "json" | "jsonc" => return Lang::Json,
        "yml" | "yaml" => return Lang::Yaml,
        "toml" | "ini" | "cnf" | "service" | "socket" | "timer" | "mount" | "target" | "path" | "network"
        | "netdev" | "desktop" => return Lang::Ini,
        "sh" | "bash" | "zsh" | "ksh" => return Lang::Shell,
        "xml" | "html" | "htm" | "svg" | "xhtml" | "plist" => return Lang::Xml,
        "env" => return Lang::Env,
        _ => {}
    }
    if name == ".env" || name.starts_with(".env.") {
        return Lang::Env;
    }
    if name == "dockerfile" || name.starts_with("dockerfile.") || name.ends_with(".dockerfile") {
        return Lang::Dockerfile;
    }
    if [".bashrc", ".bash_profile", ".profile", ".zshrc", ".bash_aliases", "profile", "bashrc"].contains(&name.as_str())
        || path.contains("/profile.d/")
        || path.contains("/cron.")
    {
        return Lang::Shell;
    }
    if path.contains("/nginx/")
        || path.contains("/apache2/")
        || path.contains("/httpd/")
        || path.contains("/haproxy/")
        || path.contains("/ssh/")
        || name == "nginx.conf"
        || name == "caddyfile"
        || name.ends_with("_config")
    {
        return Lang::Directive;
    }
    if path.contains("/systemd/") || path.contains("/php") || path.contains("/mysql") {
        return Lang::Ini;
    }
    // Content sniffing for extension-less files.
    let sample: Vec<&str> =
        text.lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).take(20).collect();
    if sample.first().is_some_and(|l| l.trim_start().starts_with(['{', '[']))
        && ext.is_empty()
        && text.trim_end().ends_with(['}', ']'])
    {
        return Lang::Json;
    }
    if sample.iter().any(|l| l.trim_start().starts_with('[') && l.trim_end().ends_with(']')) {
        return Lang::Ini;
    }
    if !sample.is_empty() && sample.iter().filter(|l| l.contains('=')).count() * 2 >= sample.len() {
        return Lang::Ini;
    }
    if ext == "conf" || ext == "cfg" {
        return Lang::Directive;
    }
    Lang::Plain
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tok {
    Text,
    Comment,
    String,
    Number,
    Key,
    Section,
    Variable,
    Keyword,
    Literal,
}

pub struct Palette {
    pub text: Color32,
    pub comment: Color32,
    pub string: Color32,
    pub number: Color32,
    pub key: Color32,
    pub section: Color32,
    pub variable: Color32,
    pub keyword: Color32,
    pub literal: Color32,
    pub find: Color32,
    pub find_current: Color32,
}

impl Palette {
    /// The editor's colours: `text` for plain text, the rest from the theme.
    pub fn new(text: Color32, s: &crate::look::Syntax) -> Self {
        Self {
            text,
            comment: s.comment,
            string: s.string,
            number: s.number,
            key: s.key,
            section: s.section,
            variable: s.variable,
            keyword: s.keyword,
            literal: s.literal,
            find: s.find,
            find_current: s.find_current,
        }
    }

    pub fn color(&self, t: Tok) -> Color32 {
        match t {
            Tok::Text => self.text,
            Tok::Comment => self.comment,
            Tok::String => self.string,
            Tok::Number => self.number,
            Tok::Key => self.key,
            Tok::Section => self.section,
            Tok::Variable => self.variable,
            Tok::Keyword => self.keyword,
            Tok::Literal => self.literal,
        }
    }
}

const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "in", "function",
    "return", "export", "local", "readonly", "set", "unset", "exit", "source", "trap", "shift", "break", "continue",
];
const DOCKER_KEYWORDS: &[&str] = &[
    "FROM",
    "RUN",
    "CMD",
    "LABEL",
    "EXPOSE",
    "ENV",
    "ADD",
    "COPY",
    "ENTRYPOINT",
    "VOLUME",
    "USER",
    "WORKDIR",
    "ARG",
    "ONBUILD",
    "STOPSIGNAL",
    "HEALTHCHECK",
    "SHELL",
    "AS",
];
const LITERALS: &[&str] = &["on", "off", "yes", "no", "true", "false", "null", "none", "~"];

/// State carried between lines (only XML comments span lines here).
#[derive(Default, Clone, Copy)]
pub struct LineState {
    in_xml_comment: bool,
}

/// Tokens for one line (without its newline). Ranges cover the whole line.
pub fn tokenize_line(lang: Lang, line: &str, st: &mut LineState) -> Vec<(Range<usize>, Tok)> {
    let mut out = Vec::new();
    match lang {
        Lang::Plain => out.push((0..line.len(), Tok::Text)),
        Lang::Xml => xml_line(line, st, &mut out),
        Lang::Json => generic(
            line,
            &mut out,
            Opts { comment: None, key_mode: KeyMode::QuotedBeforeColon, vars: false, keywords: &[] },
        ),
        Lang::Yaml => {
            let trimmed = line.trim_start();
            if trimmed == "---" || trimmed == "..." {
                out.push((0..line.len(), Tok::Section));
            } else {
                generic(
                    line,
                    &mut out,
                    Opts { comment: Some('#'), key_mode: KeyMode::BeforeColon, vars: false, keywords: &[] },
                )
            }
        }
        Lang::Ini | Lang::Env => {
            let t = line.trim();
            if lang == Lang::Ini && t.starts_with('[') && t.ends_with(']') {
                out.push((0..line.len(), Tok::Section));
            } else if t.starts_with(';') || t.starts_with('#') {
                out.push((0..line.len(), Tok::Comment));
            } else {
                generic(
                    line,
                    &mut out,
                    Opts {
                        comment: Some('#'),
                        key_mode: KeyMode::BeforeEquals,
                        vars: lang == Lang::Env,
                        keywords: &[],
                    },
                )
            }
        }
        Lang::Directive => {
            let t = line.trim_start();
            if t.starts_with('<') {
                // Apache sections: <VirtualHost *:80> … </VirtualHost>
                out.push((0..line.len(), Tok::Section));
            } else {
                generic(
                    line,
                    &mut out,
                    Opts { comment: Some('#'), key_mode: KeyMode::FirstWord, vars: true, keywords: &[] },
                )
            }
        }
        Lang::Shell => generic(
            line,
            &mut out,
            Opts { comment: Some('#'), key_mode: KeyMode::None, vars: true, keywords: SHELL_KEYWORDS },
        ),
        Lang::Dockerfile => generic(
            line,
            &mut out,
            Opts { comment: Some('#'), key_mode: KeyMode::None, vars: true, keywords: DOCKER_KEYWORDS },
        ),
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum KeyMode {
    None,
    /// The first word is the directive name.
    FirstWord,
    /// Text before the first `=`.
    BeforeEquals,
    /// Text before `: ` (YAML).
    BeforeColon,
    /// A quoted string followed by `:` (JSON).
    QuotedBeforeColon,
}

struct Opts {
    comment: Option<char>,
    key_mode: KeyMode,
    vars: bool,
    keywords: &'static [&'static str],
}

fn generic(line: &str, out: &mut Vec<(Range<usize>, Tok)>, o: Opts) {
    let b = line.as_bytes();
    let mut i = 0;
    let mut text_start = 0;
    let flush = |out: &mut Vec<(Range<usize>, Tok)>, from: usize, to: usize| {
        if to > from {
            out.push((from..to, Tok::Text));
        }
    };

    // Leading key.
    let indent = line.len() - line.trim_start().len();
    let key_end = match o.key_mode {
        KeyMode::FirstWord => {
            let rest = &line[indent..];
            if rest.starts_with(o.comment.unwrap_or('\0')) || rest.starts_with(['}', '{']) {
                None
            } else {
                rest.find(|c: char| c.is_whitespace() || c == ';' || c == '{').map(|e| indent + e).or(Some(line.len()))
            }
        }
        KeyMode::BeforeEquals => line.find('=').filter(|&e| !line[..e].contains(o.comment.unwrap_or('\0'))),
        KeyMode::BeforeColon => {
            let rest = &line[indent..];
            let rest_off = if rest.starts_with("- ") { 2 } else { 0 };
            line[indent + rest_off..]
                .find(": ")
                .or_else(|| line.trim_end().ends_with(':').then(|| line.trim_end().len() - 1 - indent - rest_off))
                .map(|e| indent + rest_off + e)
                .filter(|&e| {
                    !line[..e].contains(['#', '"', '\'']) || line[indent + rest_off..].starts_with(['"', '\''])
                })
        }
        _ => None,
    };
    if let Some(e) = key_end.filter(|&e| e > indent) {
        flush(out, 0, indent);
        let key_start =
            if o.key_mode == KeyMode::BeforeColon && line[indent..].starts_with("- ") { indent + 2 } else { indent };
        flush(out, indent, key_start);
        out.push((key_start..e, Tok::Key));
        i = e;
        text_start = e;
    }

    while i < b.len() {
        let c = b[i];
        if Some(c as char) == o.comment
            && (i == 0 || b[i - 1].is_ascii_whitespace() || o.key_mode == KeyMode::None && i == 0)
        {
            flush(out, text_start, i);
            out.push((i..b.len(), Tok::Comment));
            return;
        }
        if c == b'"' || c == b'\'' {
            flush(out, text_start, i);
            let end = string_end(b, i);
            let tok = if o.key_mode == KeyMode::QuotedBeforeColon && line[end..].trim_start().starts_with(':') {
                Tok::Key
            } else {
                Tok::String
            };
            out.push((i..end, tok));
            i = end;
            text_start = i;
            continue;
        }
        if o.vars && c == b'$' && i + 1 < b.len() {
            flush(out, text_start, i);
            let end = if b[i + 1] == b'{' {
                line[i..].find('}').map(|e| i + e + 1).unwrap_or(b.len())
            } else {
                let mut e = i + 1;
                while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'_') {
                    e += 1;
                }
                if e == i + 1 && e < b.len() && b"?#@*!$0123456789".contains(&b[e]) { e + 1 } else { e }
            };
            out.push((i..end.max(i + 1), Tok::Variable));
            i = end.max(i + 1);
            text_start = i;
            continue;
        }
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'-' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b"_.-:/".contains(&b[i])) {
                i += 1;
            }
            // Don't swallow a trailing ':' that ends a word.
            let word = &line[start..i];
            let at_boundary = start == 0 || !b[start - 1].is_ascii_alphanumeric();
            let tok = if at_boundary && is_number(word) {
                Some(Tok::Number)
            } else if at_boundary && o.keywords.contains(&word) {
                Some(Tok::Keyword)
            } else if at_boundary && LITERALS.contains(&word.to_lowercase().as_str()) {
                Some(Tok::Literal)
            } else {
                None
            };
            if let Some(t) = tok {
                flush(out, text_start, start);
                out.push((start..i, t));
                text_start = i;
            }
            continue;
        }
        i += 1;
    }
    flush(out, text_start, b.len());
}

fn string_end(b: &[u8], start: usize) -> usize {
    let q = b[start];
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == b'\\' && q == b'"' {
            i += 2;
            continue;
        }
        if b[i] == q {
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// 80, 1.5, 0755, 10m, 1g, 30s, 192.168.1.1, 0x1f.
fn is_number(w: &str) -> bool {
    let w = w.trim_end_matches(':');
    if w.is_empty() || !w.as_bytes()[0].is_ascii_digit() {
        return false;
    }
    if let Some(hex) = w.strip_prefix("0x") {
        return !hex.is_empty() && hex.bytes().all(|c| c.is_ascii_hexdigit());
    }
    let digits = w.trim_end_matches(['k', 'K', 'm', 'M', 'g', 'G', 's', 'h', 'd', 'w', 'y', '%']);
    !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit() || c == b'.' || c == b':')
}

fn xml_line(line: &str, st: &mut LineState, out: &mut Vec<(Range<usize>, Tok)>) {
    let mut i = 0;
    while i < line.len() {
        if st.in_xml_comment {
            match line[i..].find("-->") {
                Some(e) => {
                    out.push((i..i + e + 3, Tok::Comment));
                    i += e + 3;
                    st.in_xml_comment = false;
                }
                None => {
                    out.push((i..line.len(), Tok::Comment));
                    return;
                }
            }
            continue;
        }
        let rest = &line[i..];
        if rest.starts_with("<!--") {
            st.in_xml_comment = true;
            continue;
        }
        if rest.starts_with('<') {
            let end = rest.find('>').map(|e| i + e + 1).unwrap_or(line.len());
            let tag = &line[i..end];
            // Tag name as a section, attributes as keys, values as strings.
            let name_end = tag.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(tag.len());
            out.push((i..i + name_end, Tok::Section));
            let mut j = i + name_end;
            let b = line.as_bytes();
            while j < end {
                if b[j] == b'"' || b[j] == b'\'' {
                    let e = string_end(b, j).min(end);
                    out.push((j..e, Tok::String));
                    j = e;
                } else {
                    let s = j;
                    while j < end && b[j] != b'"' && b[j] != b'\'' {
                        j += 1;
                    }
                    out.push((s..j, Tok::Key));
                }
            }
            i = end;
            continue;
        }
        let end = rest.find('<').map(|e| i + e).unwrap_or(line.len());
        out.push((i..end, Tok::Text));
        i = end;
    }
}

/// A full-text layout job. `matches` (byte ranges, sorted) are highlighted,
/// `current` (an index into them) more strongly.
pub fn layout(
    text: &str,
    lang: Lang,
    matches: &[Range<usize>],
    current: Option<usize>,
    font: &egui::FontId,
    pal: &Palette,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let fmt = |c: Color32| egui::TextFormat { font_id: font.clone(), color: c, ..Default::default() };
    let (hit_bg, current_bg) = (pal.find, pal.find_current);
    let mut m = 0;
    let mut st = LineState::default();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        for (r, tok) in tokenize_line(lang, body, &mut st) {
            let (mut a, b) = (offset + r.start, offset + r.end);
            // Split the token where search matches start and end.
            while a < b {
                while m < matches.len() && matches[m].end <= a {
                    m += 1;
                }
                let (end, hit) = match matches.get(m) {
                    Some(h) if h.start <= a => (h.end.min(b), Some(m)),
                    Some(h) if h.start < b => (h.start, None),
                    _ => (b, None),
                };
                let mut f = fmt(pal.color(tok));
                if let Some(i) = hit {
                    f.background = if Some(i) == current { current_bg } else { hit_bg };
                }
                if tok == Tok::Comment {
                    f.italics = true;
                }
                job.append(&text[a..end], 0.0, f);
                a = end;
            }
        }
        if line.ends_with('\n') {
            job.append("\n", 0.0, fmt(pal.text));
        }
        offset += line.len();
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(lang: Lang, line: &str) -> Vec<(&str, Tok)> {
        let mut st = LineState::default();
        tokenize_line(lang, line, &mut st)
            .into_iter()
            .filter(|(_, t)| *t != Tok::Text)
            .map(|(r, t)| (&line[r], t))
            .collect()
    }

    #[test]
    fn detects_common_files() {
        assert_eq!(detect("/etc/nginx/nginx.conf", ""), Lang::Directive);
        assert_eq!(detect("/etc/ssh/sshd_config", ""), Lang::Directive);
        assert_eq!(detect("/etc/systemd/system/app.service", ""), Lang::Ini);
        assert_eq!(detect("/srv/app/.env", ""), Lang::Env);
        assert_eq!(detect("/srv/app/docker-compose.yml", ""), Lang::Yaml);
        assert_eq!(detect("/srv/app/package.json", ""), Lang::Json);
        assert_eq!(detect("/usr/local/bin/deploy", "#!/bin/bash\necho"), Lang::Shell);
        assert_eq!(detect("/root/.bashrc", ""), Lang::Shell);
        assert_eq!(detect("/srv/Dockerfile", ""), Lang::Dockerfile);
        assert_eq!(detect("/etc/hosts", "127.0.0.1 localhost\n"), Lang::Plain);
        assert_eq!(detect("/etc/myapp/settings", "[db]\nhost=x\n"), Lang::Ini);
    }

    #[test]
    fn directives() {
        assert_eq!(
            toks(Lang::Directive, "    listen 8080; # main"),
            [("listen", Tok::Key), ("8080", Tok::Number), ("# main", Tok::Comment)]
        );
        assert_eq!(
            toks(Lang::Directive, "  root $document_root;"),
            [("root", Tok::Key), ("$document_root", Tok::Variable)]
        );
        assert_eq!(toks(Lang::Directive, "PermitRootLogin no"), [("PermitRootLogin", Tok::Key), ("no", Tok::Literal)]);
    }

    #[test]
    fn hash_inside_strings_is_not_a_comment() {
        assert_eq!(
            toks(Lang::Shell, r#"echo "a # b" # real"#),
            [(r#""a # b""#, Tok::String), ("# real", Tok::Comment)]
        );
        assert_eq!(toks(Lang::Directive, "rewrite ^/a#b /c;"), [("rewrite", Tok::Key)]);
    }

    #[test]
    fn ini_yaml_json() {
        assert_eq!(toks(Lang::Ini, "[Service]"), [("[Service]", Tok::Section)]);
        assert_eq!(toks(Lang::Ini, "Restart=always"), [("Restart", Tok::Key)]);
        assert_eq!(toks(Lang::Yaml, "  - name: web"), [("name", Tok::Key)]);
        assert_eq!(toks(Lang::Yaml, "ports:"), [("ports", Tok::Key)]);
        assert_eq!(toks(Lang::Json, r#"  "port": 8080,"#), [(r#""port""#, Tok::Key), ("8080", Tok::Number)]);
        assert_eq!(toks(Lang::Env, "DB_URL=postgres://${HOST}/db"), [("DB_URL", Tok::Key), ("${HOST}", Tok::Variable)]);
    }

    #[test]
    fn shell_keywords_and_xml() {
        assert_eq!(toks(Lang::Shell, "if [ -f x ]; then"), [("if", Tok::Keyword), ("then", Tok::Keyword)]);
        let mut st = LineState::default();
        let t = tokenize_line(Lang::Xml, "<a href=\"x\"><!-- c", &mut st);
        assert!(t.iter().any(|(_, k)| *k == Tok::Comment));
        assert!(st.in_xml_comment);
    }

    #[test]
    fn layout_covers_all_text() {
        let t = &crate::look::builtin()[0];
        let pal = Palette::new(Color32::WHITE, &crate::look::Semantic::new(t, &crate::look::visuals(t, None)).syntax);
        let text = "server {\n  listen 80; # x\n}\n";
        let hits: Vec<_> = text.match_indices("listen").map(|(i, m)| i..i + m.len()).collect();
        let job = layout(text, Lang::Directive, &hits, Some(0), &egui::FontId::monospace(12.0), &pal);
        assert_eq!(job.text, text);
        assert!(job.sections.iter().any(|s| s.format.background != Color32::TRANSPARENT));
    }

    #[test]
    fn numbers() {
        for n in ["80", "1.5", "0755", "10m", "30s", "192.168.1.1", "0x1f"] {
            assert!(is_number(n), "{n}");
        }
        for n in ["v1", "abc", "1a2b"] {
            assert!(!is_number(n), "{n}");
        }
    }
}
