//! A terminal widget: a vt100 screen drawn with egui, plus keyboard and mouse
//! handling. It knows nothing about SSH; it produces bytes to send and the
//! size the remote PTY should be.

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, Event, FontId, Key, Modifiers, Pos2, Rect, Sense, Stroke, Vec2};

const SCROLLBACK: usize = 10_000;

/// What the widget wants the session to do after a frame.
#[derive(Default)]
pub struct TerminalOutput {
    pub send: Vec<u8>,
    pub resize: Option<(u16, u16)>,
}

pub struct Terminal {
    parser: vt100::Parser,
    font_size: f32,
    /// Selection in screen cells (row, col), anchor and current end.
    selection: Option<((u16, u16), (u16, u16))>,
    /// Wheel movement not yet turned into whole lines.
    scroll_accum: f32,
    pub exited: Option<String>,
}

impl Terminal {
    pub fn new(font_size: f32) -> Self {
        Self {
            parser: vt100::Parser::new(24, 80, SCROLLBACK),
            font_size,
            selection: None,
            scroll_accum: 0.0,
            exited: None,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        // New output snaps the view back to the bottom, like most terminals.
        if self.parser.screen().scrollback() > 0 {
            self.parser.screen_mut().set_scrollback(0);
        }
        self.parser.process(bytes);
    }

    pub fn set_font_size(&mut self, size: f32) {
        self.font_size = size;
    }

    fn selected_text(&self) -> Option<String> {
        let ((r1, c1), (r2, c2)) = self.selection?;
        let (start, end) = if (r1, c1) <= (r2, c2) { ((r1, c1), (r2, c2)) } else { ((r2, c2), (r1, c1)) };
        if start == end {
            return None;
        }
        let text = self.parser.screen().contents_between(start.0, start.1, end.0, end.1 + 1);
        Some(text.lines().map(str::trim_end).collect::<Vec<_>>().join("\n"))
    }

    fn in_selection(&self, row: u16, col: u16) -> bool {
        let Some(((r1, c1), (r2, c2))) = self.selection else { return false };
        let (start, end) = if (r1, c1) <= (r2, c2) { ((r1, c1), (r2, c2)) } else { ((r2, c2), (r1, c1)) };
        start != end && (row, col) >= start && (row, col) <= end
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> TerminalOutput {
        let mut out = TerminalOutput::default();
        let font = FontId::monospace(self.font_size);
        let (cell_w, cell_h) = ui.ctx().fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let avail = ui.available_size();
        let cols = ((avail.x / cell_w).floor() as u16).clamp(20, 500);
        let rows = ((avail.y / cell_h).floor() as u16).clamp(5, 300);
        if (rows, cols) != self.parser.screen().size() {
            self.parser.screen_mut().set_size(rows, cols);
            out.resize = Some((cols, rows));
        }

        let (rect, response) = ui.allocate_exact_size(avail, Sense::click_and_drag());
        let id = response.id;
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        let focused = response.has_focus();
        if focused {
            // Tab, arrows and Escape belong to the remote program, not egui.
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
        }

        let to_cell = |p: Pos2| -> (u16, u16) {
            let col = ((p.x - rect.left()) / cell_w).floor().clamp(0.0, (cols - 1) as f32) as u16;
            let row = ((p.y - rect.top()) / cell_h).floor().clamp(0.0, (rows - 1) as f32) as u16;
            (row, col)
        };
        if response.drag_started()
            && let Some(p) = response.interact_pointer_pos()
        {
            let c = to_cell(p);
            self.selection = Some((c, c));
        }
        if response.dragged()
            && let (Some(p), Some((anchor, _))) = (response.interact_pointer_pos(), self.selection)
        {
            self.selection = Some((anchor, to_cell(p)));
        }
        if response.clicked() {
            self.selection = None;
        }

        // Right click copies the selection.
        if response.secondary_clicked()
            && let Some(t) = self.selected_text()
        {
            ui.ctx().copy_text(t);
        }

        if response.hovered() {
            self.scroll_accum += ui.input(|i| i.smooth_scroll_delta.y);
            let lines = (self.scroll_accum / cell_h).trunc() as i64;
            if lines != 0 {
                self.scroll_accum -= lines as f32 * cell_h;
                let screen = self.parser.screen_mut();
                if screen.alternate_screen() {
                    // Full-screen programs (less, vim) scroll with arrow keys.
                    let key = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
                    for _ in 0..lines.unsigned_abs().min(10) {
                        out.send.extend_from_slice(key);
                    }
                } else {
                    let cur = screen.scrollback() as i64;
                    screen.set_scrollback((cur + lines).max(0) as usize);
                }
            }
        }

        if focused {
            self.handle_input(ui, &mut out);
        }

        self.paint(ui, rect, &font, cell_w, cell_h, focused);
        if self.parser.screen().scrollback() > 0 {
            let label = format!("⬆ {} lines", self.parser.screen().scrollback());
            ui.painter().text(
                rect.right_top() + Vec2::new(-8.0, 4.0),
                egui::Align2::RIGHT_TOP,
                label,
                FontId::proportional(12.0),
                Color32::YELLOW,
            );
        }
        if let Some(msg) = &self.exited {
            ui.painter().text(
                rect.center_bottom() - Vec2::new(0.0, 12.0),
                egui::Align2::CENTER_BOTTOM,
                msg,
                FontId::proportional(14.0),
                Color32::from_rgb(255, 180, 80),
            );
        }
        out
    }

    fn handle_input(&mut self, ui: &egui::Ui, out: &mut TerminalOutput) {
        let events = ui.input(|i| i.events.clone());
        let screen = self.parser.screen();
        let app_cursor = screen.application_cursor();
        let bracketed = screen.bracketed_paste();
        for ev in events {
            match ev {
                Event::Text(t) => out.send.extend_from_slice(t.as_bytes()),
                Event::Paste(t) => {
                    if bracketed {
                        out.send.extend_from_slice(b"\x1b[200~");
                    }
                    // Terminals send CR for newlines in pasted text.
                    out.send.extend_from_slice(t.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
                    if bracketed {
                        out.send.extend_from_slice(b"\x1b[201~");
                    }
                }
                // egui turns Ctrl+C / Ctrl+X into clipboard events; in a terminal
                // Ctrl+C copies only when something is selected, otherwise it is
                // an interrupt. Ctrl+X is nano's exit key.
                Event::Copy => match self.selected_text() {
                    Some(t) => {
                        ui.ctx().copy_text(t);
                        self.selection = None;
                    }
                    None => out.send.push(0x03),
                },
                Event::Cut => out.send.push(0x18),
                Event::Key { key, pressed: true, modifiers, .. } => {
                    if modifiers.ctrl && modifiers.shift && key == Key::C {
                        if let Some(t) = self.selected_text() {
                            ui.ctx().copy_text(t);
                        }
                        continue;
                    }
                    if let Some(bytes) = key_bytes(key, modifiers, app_cursor) {
                        out.send.extend_from_slice(&bytes);
                    }
                }
                _ => {}
            }
        }
        if !out.send.is_empty() {
            self.selection = None;
        }
    }

    fn paint(&self, ui: &egui::Ui, rect: Rect, font: &FontId, cell_w: f32, cell_h: f32, focused: bool) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, BG);
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        for row in 0..rows {
            let y = rect.top() + row as f32 * cell_h;
            let mut job = LayoutJob::default();
            let mut col = 0;
            while col < cols {
                let Some(cell) = screen.cell(row, col) else { break };
                if cell.is_wide_continuation() {
                    col += 1;
                    continue;
                }
                let (mut fg, mut bg) = (color(cell.fgcolor(), FG, cell.bold()), color(cell.bgcolor(), BG, false));
                if cell.inverse() {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cell.dim() {
                    fg = fg.gamma_multiply(0.6);
                }
                if self.in_selection(row, col) {
                    bg = SELECTION;
                }
                let text = if cell.has_contents() { cell.contents() } else { " " };
                let format = TextFormat {
                    font_id: font.clone(),
                    color: fg,
                    background: if bg == BG { Color32::TRANSPARENT } else { bg },
                    italics: cell.italic(),
                    underline: if cell.underline() { Stroke::new(1.0, fg) } else { Stroke::NONE },
                    ..Default::default()
                };
                job.append(text, 0.0, format);
                col += if cell.is_wide() { 2 } else { 1 };
            }
            let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
            painter.galley(Pos2::new(rect.left(), y), galley, FG);
        }
        if !screen.hide_cursor() && screen.scrollback() == 0 {
            let (r, c) = screen.cursor_position();
            let cursor = Rect::from_min_size(
                Pos2::new(rect.left() + c as f32 * cell_w, rect.top() + r as f32 * cell_h),
                Vec2::new(cell_w, cell_h),
            );
            if focused {
                painter.rect_filled(cursor, 0.0, CURSOR.gamma_multiply(0.7));
            } else {
                painter.rect_stroke(cursor, 0.0, Stroke::new(1.0, CURSOR), egui::StrokeKind::Inside);
            }
        }
    }
}

const BG: Color32 = Color32::from_rgb(0x1d, 0x1f, 0x21);
const FG: Color32 = Color32::from_rgb(0xd0, 0xd0, 0xd0);
const CURSOR: Color32 = Color32::from_rgb(0xf0, 0xc6, 0x74);
const SELECTION: Color32 = Color32::from_rgb(0x37, 0x4b, 0x6e);

/// xterm's 16 colours, a little softened.
const ANSI: [Color32; 16] = [
    Color32::from_rgb(0x1d, 0x1f, 0x21),
    Color32::from_rgb(0xcc, 0x66, 0x66),
    Color32::from_rgb(0xb5, 0xbd, 0x68),
    Color32::from_rgb(0xf0, 0xc6, 0x74),
    Color32::from_rgb(0x81, 0xa2, 0xbe),
    Color32::from_rgb(0xb2, 0x94, 0xbb),
    Color32::from_rgb(0x8a, 0xbe, 0xb7),
    Color32::from_rgb(0xc5, 0xc8, 0xc6),
    Color32::from_rgb(0x66, 0x66, 0x66),
    Color32::from_rgb(0xd5, 0x4e, 0x53),
    Color32::from_rgb(0xb9, 0xca, 0x4a),
    Color32::from_rgb(0xe7, 0xc5, 0x47),
    Color32::from_rgb(0x7a, 0xa6, 0xda),
    Color32::from_rgb(0xc3, 0x97, 0xd8),
    Color32::from_rgb(0x70, 0xc0, 0xb1),
    Color32::from_rgb(0xea, 0xea, 0xea),
];

fn color(c: vt100::Color, default: Color32, bold: bool) -> Color32 {
    match c {
        vt100::Color::Default => default,
        // Bold brightens the first 8 colours, as xterm does.
        vt100::Color::Idx(i) if i < 8 && bold => ANSI[i as usize + 8],
        vt100::Color::Idx(i) if i < 16 => ANSI[i as usize],
        vt100::Color::Idx(i) if i < 232 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        vt100::Color::Idx(i) => {
            let v = 8 + (i - 232) * 10;
            Color32::from_rgb(v, v, v)
        }
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

/// Bytes xterm sends for a key press that does not produce text.
pub fn key_bytes(key: Key, m: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let csi_mod = |code: &str, final_: char| -> Vec<u8> {
        // Modifier parameter: 1 + shift + 2*alt + 4*ctrl.
        let p = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8;
        if p == 1 {
            format!("\x1b[{code}{final_}")
        } else {
            format!("\x1b[{};{p}{final_}", if code.is_empty() { "1" } else { code })
        }
        .into_bytes()
    };
    let arrow = |c: char| -> Vec<u8> {
        if m.any() && !m.command_only() {
            csi_mod("", c)
        } else if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        let p = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8;
        if p == 1 { format!("\x1b[{n}~") } else { format!("\x1b[{n};{p}~") }.into_bytes()
    };
    let bytes = match key {
        Key::Enter => b"\r".to_vec(),
        Key::Backspace if m.ctrl => vec![0x08],
        Key::Backspace => vec![0x7f],
        Key::Tab if m.shift => b"\x1b[Z".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::ArrowUp => arrow('A'),
        Key::ArrowDown => arrow('B'),
        Key::ArrowRight => arrow('C'),
        Key::ArrowLeft => arrow('D'),
        Key::Home => arrow('H'),
        Key::End => arrow('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => b"\x1bOP".to_vec(),
        Key::F2 => b"\x1bOQ".to_vec(),
        Key::F3 => b"\x1bOR".to_vec(),
        Key::F4 => b"\x1bOS".to_vec(),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        _ if m.ctrl && !m.shift => {
            let name = key.name();
            let c = name.chars().next()?;
            if name.len() == 1 && c.is_ascii_alphabetic() {
                let code = (c.to_ascii_lowercase() as u8) & 0x1f;
                if m.alt { vec![0x1b, code] } else { vec![code] }
            } else {
                match key {
                    Key::Space => vec![0],
                    Key::OpenBracket => vec![0x1b],
                    Key::Backslash => vec![0x1c],
                    Key::CloseBracket => vec![0x1d],
                    Key::Slash => vec![0x1f],
                    _ => return None,
                }
            }
        }
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;
    const CTRL: Modifiers = Modifiers::CTRL;

    #[test]
    fn control_keys() {
        assert_eq!(key_bytes(Key::C, CTRL, false), Some(vec![3]));
        assert_eq!(key_bytes(Key::D, CTRL, false), Some(vec![4]));
        assert_eq!(key_bytes(Key::OpenBracket, CTRL, false), Some(vec![0x1b]));
        // Plain letters come through as text events, not keys.
        assert_eq!(key_bytes(Key::C, NONE, false), None);
    }

    #[test]
    fn cursor_keys_follow_application_mode() {
        assert_eq!(key_bytes(Key::ArrowUp, NONE, false), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes(Key::ArrowUp, NONE, true), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_bytes(Key::ArrowRight, CTRL, false), Some(b"\x1b[1;5C".to_vec()));
    }

    #[test]
    fn editing_keys() {
        assert_eq!(key_bytes(Key::Enter, NONE, false), Some(b"\r".to_vec()));
        assert_eq!(key_bytes(Key::Backspace, NONE, false), Some(vec![0x7f]));
        assert_eq!(key_bytes(Key::Delete, NONE, false), Some(b"\x1b[3~".to_vec()));
        assert_eq!(key_bytes(Key::F5, NONE, false), Some(b"\x1b[15~".to_vec()));
        assert_eq!(key_bytes(Key::Tab, Modifiers::SHIFT, false), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn colours() {
        assert_eq!(color(vt100::Color::Idx(1), FG, false), ANSI[1]);
        assert_eq!(color(vt100::Color::Idx(1), FG, true), ANSI[9]);
        assert_eq!(color(vt100::Color::Idx(16), FG, false), Color32::from_rgb(0, 0, 0));
        assert_eq!(color(vt100::Color::Idx(231), FG, false), Color32::from_rgb(255, 255, 255));
        assert_eq!(color(vt100::Color::Rgb(1, 2, 3), FG, false), Color32::from_rgb(1, 2, 3));
    }

    #[test]
    fn selection_text() {
        let mut t = Terminal::new(14.0);
        t.feed(b"hello world\r\nsecond line");
        t.selection = Some(((0, 6), (1, 5)));
        assert_eq!(t.selected_text().as_deref(), Some("world\nsecond"));
    }
}
