//! Terminal colour themes, and the small pure helpers the terminal needs for
//! links and mouse reporting.

use egui::Color32;

#[derive(Debug)]
pub struct Theme {
    pub name: &'static str,
    pub bg: Color32,
    pub fg: Color32,
    pub cursor: Color32,
    pub selection: Color32,
    /// The 16 ANSI colours: black, red, green, yellow, blue, magenta, cyan,
    /// white, then their bright versions.
    pub ansi: [Color32; 16],
}

const fn c(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const THEMES: &[Theme] = &[
    Theme {
        name: "Tomorrow Night",
        bg: c(0x1d1f21),
        fg: c(0xd0d0d0),
        cursor: c(0xf0c674),
        selection: c(0x374b6e),
        ansi: [
            c(0x1d1f21),
            c(0xcc6666),
            c(0xb5bd68),
            c(0xf0c674),
            c(0x81a2be),
            c(0xb294bb),
            c(0x8abeb7),
            c(0xc5c8c6),
            c(0x666666),
            c(0xd54e53),
            c(0xb9ca4a),
            c(0xe7c547),
            c(0x7aa6da),
            c(0xc397d8),
            c(0x70c0b1),
            c(0xeaeaea),
        ],
    },
    Theme {
        name: "Solarized Dark",
        bg: c(0x002b36),
        fg: c(0x93a1a1),
        cursor: c(0x93a1a1),
        selection: c(0x274642),
        ansi: [
            c(0x073642),
            c(0xdc322f),
            c(0x859900),
            c(0xb58900),
            c(0x268bd2),
            c(0xd33682),
            c(0x2aa198),
            c(0xeee8d5),
            c(0x586e75),
            c(0xcb4b16),
            c(0x93a1a1),
            c(0x839496),
            c(0x6c71c4),
            c(0xd33682),
            c(0x93a1a1),
            c(0xfdf6e3),
        ],
    },
    Theme {
        name: "Solarized Light",
        bg: c(0xfdf6e3),
        fg: c(0x586e75),
        cursor: c(0x586e75),
        selection: c(0xeee8d5),
        ansi: [
            c(0x073642),
            c(0xdc322f),
            c(0x859900),
            c(0xb58900),
            c(0x268bd2),
            c(0xd33682),
            c(0x2aa198),
            c(0xeee8d5),
            c(0x002b36),
            c(0xcb4b16),
            c(0x586e75),
            c(0x657b83),
            c(0x6c71c4),
            c(0xd33682),
            c(0x93a1a1),
            c(0xfdf6e3),
        ],
    },
    Theme {
        name: "Dracula",
        bg: c(0x282a36),
        fg: c(0xf8f8f2),
        cursor: c(0xf8f8f2),
        selection: c(0x44475a),
        ansi: [
            c(0x21222c),
            c(0xff5555),
            c(0x50fa7b),
            c(0xf1fa8c),
            c(0xbd93f9),
            c(0xff79c6),
            c(0x8be9fd),
            c(0xf8f8f2),
            c(0x6272a4),
            c(0xff6e6e),
            c(0x69ff94),
            c(0xffffa5),
            c(0xd6acff),
            c(0xff92df),
            c(0xa4ffff),
            c(0xffffff),
        ],
    },
    Theme {
        name: "Gruvbox Dark",
        bg: c(0x282828),
        fg: c(0xebdbb2),
        cursor: c(0xebdbb2),
        selection: c(0x504945),
        ansi: [
            c(0x282828),
            c(0xcc241d),
            c(0x98971a),
            c(0xd79921),
            c(0x458588),
            c(0xb16286),
            c(0x689d6a),
            c(0xa89984),
            c(0x928374),
            c(0xfb4934),
            c(0xb8bb26),
            c(0xfabd2f),
            c(0x83a598),
            c(0xd3869b),
            c(0x8ec07c),
            c(0xebdbb2),
        ],
    },
    Theme {
        name: "Light",
        bg: c(0xffffff),
        fg: c(0x24292f),
        cursor: c(0x0969da),
        selection: c(0xc8dcf4),
        ansi: [
            c(0x24292f),
            c(0xcf222e),
            c(0x116329),
            c(0x9a6700),
            c(0x0969da),
            c(0x8250df),
            c(0x1b7c83),
            c(0x6e7781),
            c(0x57606a),
            c(0xa40e26),
            c(0x1a7f37),
            c(0x7d4e00),
            c(0x218bff),
            c(0xa475f9),
            c(0x3192aa),
            c(0x8c959f),
        ],
    },
];

/// The URL in `line` covering character column `col`, if any, with its
/// column span. Trailing punctuation that usually ends a sentence is not
/// part of it.
pub fn url_at(line: &str, col: usize) -> Option<(usize, usize, String)> {
    let chars: Vec<char> = line.chars().collect();
    let text: String = chars.iter().collect();
    for scheme in ["https://", "http://"] {
        let mut from = 0;
        while let Some(i) = text[from..].find(scheme) {
            let start_b = from + i;
            let start = text[..start_b].chars().count();
            let mut end = start;
            while end < chars.len() && !chars[end].is_whitespace() && !"<>\"'`".contains(chars[end]) {
                end += 1;
            }
            while end > start && ".,;:!?)]}".contains(chars[end - 1]) {
                // A closing bracket stays when the URL opened one for it
                // (wiki_(page)), and goes when it closes text around the URL.
                let ch = chars[end - 1];
                let open = match ch {
                    ')' => '(',
                    ']' => '[',
                    '}' => '{',
                    _ => '\0',
                };
                if open != '\0' {
                    let part = &chars[start..end];
                    let opens = part.iter().filter(|&&c| c == open).count();
                    let closes = part.iter().filter(|&&c| c == ch).count();
                    if closes <= opens {
                        break;
                    }
                }
                end -= 1;
            }
            if end - start > scheme.len() && (start..end).contains(&col) {
                return Some((start, end, chars[start..end].iter().collect()));
            }
            from = start_b + scheme.len();
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Release,
    /// Movement with a button held.
    Drag,
}

/// Encode a mouse event the way xterm reports it to programs (htop, mc,
/// vim): SGR (`ESC [ < b ; x ; y M`) when the program asked for it, else the
/// classic `ESC [ M` with byte offsets. Rows and columns are 0-based here.
pub fn encode_mouse(
    button: MouseButton,
    kind: MouseKind,
    row: u16,
    col: u16,
    sgr: bool,
    shift: bool,
    ctrl: bool,
) -> Vec<u8> {
    let mut b: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    if kind == MouseKind::Drag {
        b += 32;
    }
    if shift {
        b += 4;
    }
    if ctrl {
        b += 16;
    }
    let (x, y) = (col as u32 + 1, row as u32 + 1);
    if sgr {
        let end = if kind == MouseKind::Release { 'm' } else { 'M' };
        return format!("\x1b[<{b};{x};{y}{end}").into_bytes();
    }
    // Classic encoding has no button for release; it reports "3". It cannot
    // express positions past 223.
    if kind == MouseKind::Release {
        b = (b & !3) | 3;
    }
    let clamp = |v: u32| (v + 32).min(255) as u8;
    vec![0x1b, b'[', b'M', (b + 32) as u8, clamp(x), clamp(y)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_urls_under_the_cursor() {
        let l = "see https://example.com/a?b=1, or (http://x.io/wiki_(page)).";
        let url = |l: &str, c: usize| url_at(l, c).map(|u| u.2);
        assert_eq!(url(l, 6).as_deref(), Some("https://example.com/a?b=1"));
        assert_eq!(url_at(l, 6).map(|u| (u.0, u.1)), Some((4, 29)));
        assert_eq!(url(l, 3), None);
        let at = l.find("x.io").unwrap();
        assert_eq!(url(l, at).as_deref(), Some("http://x.io/wiki_(page)"));
        assert_eq!(url("https://", 2), None);
        assert_eq!(url("ünï https://ü.example/x", 6).as_deref(), Some("https://ü.example/x"));
    }

    #[test]
    fn mouse_encodings() {
        use MouseButton::*;
        use MouseKind::*;
        assert_eq!(encode_mouse(Left, Press, 0, 0, true, false, false), b"\x1b[<0;1;1M");
        assert_eq!(encode_mouse(Left, Release, 4, 9, true, false, false), b"\x1b[<0;10;5m");
        assert_eq!(encode_mouse(WheelDown, Press, 1, 1, true, false, false), b"\x1b[<65;2;2M");
        assert_eq!(encode_mouse(Left, Drag, 1, 1, true, false, true), b"\x1b[<48;2;2M");
        assert_eq!(encode_mouse(Left, Press, 0, 0, false, false, false), vec![0x1b, b'[', b'M', 32, 33, 33]);
        assert_eq!(encode_mouse(Right, Release, 0, 0, false, false, false), vec![0x1b, b'[', b'M', 35, 33, 33]);
    }
}
