//! A terminal widget: a vt100 screen drawn with egui, plus keyboard and mouse
//! handling. It knows nothing about SSH; it produces bytes to send and the
//! size the remote PTY should be.

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, Event, FontId, Key, Modifiers, Pos2, Rect, Sense, Stroke, Vec2};

const SCROLLBACK: usize = 10_000;

use crate::themes::{MouseButton, MouseKind, Theme, encode_mouse, url_at};

/// How the terminal behaves; from the settings, passed each frame.
pub struct TermOptions {
    pub theme: &'static Theme,
    pub copy_on_select: bool,
    /// Ask before pasting several lines into a shell, where each would run.
    pub confirm_paste: bool,
    /// A touch screen: a swipe scrolls, a long press selects, and the
    /// on-screen keyboard is asked for while the terminal has focus.
    pub touch: bool,
    /// Typing on the on-screen keyboard: a row of the keys it lacks (Esc,
    /// Tab, Ctrl, arrows).
    pub on_screen_keys: bool,
    /// A phone's screen: the keys in one row that scrolls sideways, and Find
    /// and Log in the session's menu instead of a row of their own.
    pub compact: bool,
}

/// What the widget wants the session to do after a frame.
/// The terminal's inner margin, in points.
const PAD_X: f32 = 6.0;
const PAD_Y: f32 = 4.0;

#[derive(Default)]
pub struct TerminalOutput {
    pub send: Vec<u8>,
    pub resize: Option<(u16, u16)>,
    /// The user clicked "Log to file" (the session picks the path).
    pub start_log: bool,
}

/// Search through the scrollback. Lines are numbered from the oldest kept.
#[derive(Default)]
struct Search {
    query: String,
    /// (line, column, length in columns)
    matches: Vec<(usize, usize, usize)>,
    current: usize,
    searched: String,
}

pub struct Terminal {
    /// The terminal's own widget id: the same whatever is shown above it
    /// (the key row comes and goes with the on-screen keyboard), so it keeps
    /// the focus.
    widget_id: egui::Id,
    parser: vt100::Parser,
    font_size: f32,
    /// Selection in screen cells (row, col), anchor and current end.
    selection: Option<((u16, u16), (u16, u16))>,
    /// Wheel movement not yet turned into whole lines.
    scroll_accum: f32,
    pub exited: Option<String>,
    search: Option<Search>,
    log: Option<(std::path::PathBuf, std::io::BufWriter<std::fs::File>)>,
    strip: AnsiStrip,
    osc7: Osc7,
    /// The shell's folder, as last reported with OSC 7.
    pub cwd: Option<String>,
    /// How many reports there have been; the first is where the shell started.
    pub cwd_reports: u64,
    theme: &'static Theme,
    /// Pasted text with several lines, waiting for "really paste?".
    pending_paste: Option<String>,
    /// The button held down while the program receives the mouse.
    mouse_down: Option<(MouseButton, (u16, u16))>,
    /// Take the keyboard next frame (a new terminal, or one switched to).
    pub take_focus: bool,
    /// The on-screen Ctrl key was tapped: the next key typed is sent with Ctrl.
    ctrl_latched: bool,
    /// Had the keyboard last frame (on a phone, the rest makes room).
    pub had_focus: bool,
    /// Had the focus when the app went to the background (a phone): takes
    /// it back on return, where the keyboard Android brings back types.
    pub refocus: bool,
    /// Pinch (or Ctrl+scroll) zoom, on top of the font size in Settings.
    zoom: f32,
    /// Start logging, asked for from outside the terminal (the phone menu).
    log_requested: bool,
    /// On a touch screen, a long press started a selection: dragging now
    /// extends it instead of scrolling.
    touch_selecting: bool,
}

impl Terminal {
    pub fn new(font_size: f32) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self {
            widget_id: egui::Id::new(("malasdoh-terminal", n)),
            parser: vt100::Parser::new(24, 80, SCROLLBACK),
            font_size,
            selection: None,
            scroll_accum: 0.0,
            exited: None,
            search: None,
            log: None,
            strip: AnsiStrip::default(),
            osc7: Osc7::default(),
            cwd: None,
            cwd_reports: 0,
            theme: &crate::themes::THEMES[0],
            pending_paste: None,
            mouse_down: None,
            take_focus: false,
            ctrl_latched: false,
            had_focus: false,
            refocus: false,
            zoom: 1.0,
            log_requested: false,
            touch_selecting: false,
        }
    }

    /// Process output from the server. Returns bytes to send back: answers to
    /// cursor-position, status and device-attribute queries, which vt100 does
    /// not answer itself and some programs wait for.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        // New output snaps the view back to the bottom, like most terminals,
        // unless the user is reading search results.
        if self.parser.screen().scrollback() > 0 && self.search.is_none() {
            self.parser.screen_mut().set_scrollback(0);
        }
        if let Some((_, w)) = &mut self.log {
            use std::io::Write;
            let text = self.strip.strip(bytes);
            if w.write_all(&text).and_then(|_| w.flush()).is_err() {
                self.log = None;
            }
        }
        let (dir, n) = self.osc7.scan(bytes);
        if let Some(dir) = dir {
            self.cwd = Some(dir);
            self.cwd_reports += n;
        }
        let mut reply = Vec::new();
        let mut rest = bytes;
        // Process up to each query, so the cursor position is the one at the
        // time the program asked.
        while let Some((i, len, q)) = find_query(rest) {
            self.parser.process(&rest[..i + len]);
            match q {
                Query::CursorPosition => {
                    let (r, c) = self.parser.screen().cursor_position();
                    reply.extend_from_slice(format!("\x1b[{};{}R", r + 1, c + 1).as_bytes());
                }
                Query::Status => reply.extend_from_slice(b"\x1b[0n"),
                Query::DeviceAttributes => reply.extend_from_slice(b"\x1b[?1;2c"),
            }
            rest = &rest[i + len..];
        }
        self.parser.process(rest);
        reply
    }

    pub fn start_log(&mut self, path: std::path::PathBuf) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            malasdoh_core::paths::create_private_dir(dir)?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let f = opts.open(&path)?;
        self.log = Some((path, std::io::BufWriter::new(f)));
        Ok(())
    }

    pub fn log_path(&self) -> Option<&std::path::Path> {
        self.log.as_ref().map(|(p, _)| p.as_path())
    }

    /// All lines: scrollback (oldest first) then the screen. Also returns how
    /// many of them are scrollback.
    fn all_lines(&mut self) -> (Vec<String>, usize) {
        let keep = self.parser.screen().scrollback();
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let total_sb = screen.scrollback();
        let (rows, cols) = screen.size();
        let rows = rows as usize;
        let mut lines = vec![String::new(); total_sb + rows];
        let mut off = total_sb;
        loop {
            screen.set_scrollback(off);
            for (r, text) in screen.rows(0, cols).enumerate() {
                lines[total_sb - off + r] = text;
            }
            if off == 0 {
                break;
            }
            off = off.saturating_sub(rows);
        }
        screen.set_scrollback(keep);
        (lines, total_sb)
    }

    fn run_search(&mut self) {
        let Some(q) = self.search.as_ref().map(|s| s.query.to_lowercase()) else { return };
        let mut matches = Vec::new();
        if !q.is_empty() {
            let (lines, _) = self.all_lines();
            for (i, l) in lines.iter().enumerate() {
                let lower = l.to_lowercase();
                for (byte, _) in lower.match_indices(&q) {
                    let col = lower[..byte].chars().count();
                    matches.push((i, col, q.chars().count()));
                }
            }
        }
        let s = self.search.as_mut().unwrap();
        s.searched = s.query.clone();
        s.current = matches.len().saturating_sub(1);
        s.matches = matches;
        self.reveal_current();
    }

    /// Scroll so the current match is on screen.
    fn reveal_current(&mut self) {
        let Some(&(line, _, _)) = self.search.as_ref().and_then(|s| s.matches.get(s.current)) else { return };
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let total_sb = screen.scrollback();
        let rows = screen.size().0 as usize;
        // Visible row r shows line total_sb - off + r; put the match mid-screen.
        let off = (total_sb + rows / 2).saturating_sub(line).min(total_sb);
        screen.set_scrollback(off);
    }

    /// Absolute line number of the top visible row.
    fn top_line(&mut self) -> usize {
        let screen = self.parser.screen_mut();
        let off = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let total_sb = screen.scrollback();
        screen.set_scrollback(off);
        total_sb - off
    }

    /// Open the search over the scrollback.
    pub fn open_search(&mut self) {
        if self.search.is_none() {
            self.search = Some(Search::default());
        }
    }

    /// Start logging to a file (the session picks the file).
    pub fn request_log(&mut self) {
        self.log_requested = true;
    }

    pub fn stop_log(&mut self) {
        self.log = None;
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

    fn toolbar_ui(&mut self, ui: &mut egui::Ui, out: &mut TerminalOutput) {
        let open_search = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::F));
        if open_search && self.search.is_none() {
            self.search = Some(Search::default());
        }
        ui.horizontal(|ui| {
            if let Some(s) = &mut self.search {
                ui.label("Find:");
                let r =
                    ui.add(egui::TextEdit::singleline(&mut s.query).desired_width(220.0).hint_text("in scrollback"));
                if open_search {
                    r.request_focus();
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                let (n, cur) = (s.matches.len(), s.current);
                let stale = s.query != s.searched;
                if !stale {
                    ui.label(if n == 0 { "no matches".to_string() } else { format!("{} of {n}", cur + 1) });
                }
                let prev = ui.small_button("⬆").on_hover_text("Previous (older)").clicked();
                let next = ui.small_button("⬇").on_hover_text("Next (newer)").clicked();
                let close = ui.small_button("✖").clicked() || ui.input(|i| i.key_pressed(Key::Escape) && r.has_focus());
                if close {
                    self.search = None;
                    self.parser.screen_mut().set_scrollback(0);
                } else if enter && stale {
                    self.run_search();
                    r.request_focus();
                } else if (prev || enter) && n > 0 {
                    s.current = (cur + n - 1) % n;
                    self.reveal_current();
                    if enter {
                        r.request_focus();
                    }
                } else if next && n > 0 {
                    s.current = (cur + 1) % n;
                    self.reveal_current();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match self.log_path() {
                    Some(p) => {
                        let p = p.display().to_string();
                        if ui.small_button("Stop logging").on_hover_text(&p).clicked() {
                            self.log = None;
                        }
                        ui.label(egui::RichText::new("• logging").color(crate::look::sem(ui.ctx()).bad))
                            .on_hover_text(p);
                    }
                    None => {
                        if ui
                            .small_button("Log to file")
                            .on_hover_text("Save this terminal's output (as plain text) from now on")
                            .clicked()
                        {
                            out.start_log = true;
                        }
                    }
                }
                if self.search.is_none()
                    && ui.small_button("🔍 Find").on_hover_text("Search the scrollback (Ctrl+Shift+F)").clicked()
                {
                    self.search = Some(Search::default());
                }
            });
        });
    }

    /// Keys phone and tablet keyboards do not have, over the terminal.
    fn key_row_ui(&mut self, ui: &mut egui::Ui, out: &mut TerminalOutput, compact: bool) {
        let app_cursor = self.parser.screen().application_cursor();
        let special = |k: Key| key_bytes(k, Modifiers::NONE, app_cursor).unwrap_or_default();
        let keys: [(&str, Vec<u8>); 15] = [
            ("Esc", b"\x1b".to_vec()),
            ("Tab", b"\t".to_vec()),
            ("⏴", special(Key::ArrowLeft)),
            ("⏷", special(Key::ArrowDown)),
            ("⏶", special(Key::ArrowUp)),
            ("⏵", special(Key::ArrowRight)),
            ("|", b"|".to_vec()),
            ("~", b"~".to_vec()),
            ("/", b"/".to_vec()),
            ("-", b"-".to_vec()),
            (":", b":".to_vec()),
            ("Home", special(Key::Home)),
            ("End", special(Key::End)),
            ("PgUp", special(Key::PageUp)),
            ("PgDn", special(Key::PageDown)),
        ];
        let mut tapped = false;
        let selected = self.selected_text();
        let mut copied = false;
        let mut row = |ui: &mut egui::Ui| {
            // Text selected with a long press: copy it.
            if let Some(t) = &selected
                && ui.button("Copy").clicked()
            {
                ui.ctx().copy_text(t.clone());
                copied = true;
                tapped = true;
            }
            for (n, (label, bytes)) in keys.iter().enumerate() {
                if n == 2
                    && ui
                        .selectable_label(self.ctrl_latched, "Ctrl")
                        .on_hover_text("The next key is sent with Ctrl")
                        .clicked()
                {
                    self.ctrl_latched = !self.ctrl_latched;
                    tapped = true;
                }
                if ui.button(*label).clicked() {
                    out.send.extend_from_slice(bytes);
                    tapped = true;
                }
            }
            // No Ctrl+V on a phone: paste from the system clipboard.
            if crate::clipboard::available()
                && ui.button("Paste").clicked()
                && let Some(text) = crate::clipboard::read()
            {
                // Handled below like a pasted Ctrl+V (asking first for
                // several lines into a shell).
                ui.ctx().input_mut(|i| i.events.push(Event::Paste(text)));
                tapped = true;
            }
        };
        if compact {
            // One row, swiped sideways: the terminal keeps the height.
            egui::ScrollArea::horizontal()
                .id_salt("term-keys")
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| ui.horizontal(|ui| row(ui)));
        } else {
            ui.horizontal_wrapped(|ui| row(ui));
        }
        if copied {
            self.selection = None;
        }
        if tapped {
            // Keep the keyboard up: the terminal takes the focus back.
            self.take_focus = true;
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, opts: &TermOptions) -> TerminalOutput {
        let mut out = TerminalOutput::default();
        self.theme = opts.theme;
        // Pinch (or Ctrl+scroll) over the terminal: bigger or smaller text.
        let area = ui.available_rect_before_wrap();
        let (zoom, over) = ui.input(|i| (i.zoom_delta(), i.pointer.hover_pos().is_some_and(|p| area.contains(p))));
        if zoom != 1.0 && over {
            self.zoom = (self.zoom * zoom).clamp(7.0 / self.font_size, 40.0 / self.font_size);
        }
        if opts.on_screen_keys {
            self.key_row_ui(ui, &mut out, opts.compact);
        }
        // On a phone, Find and Log are in the session's menu; the search row
        // shows only once opened.
        if !opts.compact || self.search.is_some() {
            self.toolbar_ui(ui, &mut out);
        }
        if std::mem::take(&mut self.log_requested) {
            out.start_log = true;
        }
        let font = FontId::monospace(self.font_size * self.zoom);
        let (cell_w, cell_h) = ui.ctx().fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let avail = ui.available_size();
        // A margin inside the terminal's background, so text does not touch
        // the pane's edges.
        let pad = Vec2::new(PAD_X, PAD_Y);
        let cols = (((avail.x - 2.0 * pad.x) / cell_w).floor() as u16).clamp(20, 500);
        let rows = (((avail.y - 2.0 * pad.y) / cell_h).floor() as u16).clamp(5, 300);
        if (rows, cols) != self.parser.screen().size() {
            self.parser.screen_mut().set_size(rows, cols);
            out.resize = Some((cols, rows));
        }

        let (outer, _) = ui.allocate_exact_size(avail, Sense::hover());
        let response = ui.interact(outer, self.widget_id, Sense::click_and_drag());
        ui.painter().rect_filled(outer, 0.0, self.theme.bg);
        let rect = Rect::from_min_size(outer.min + pad, outer.size() - 2.0 * pad);
        let id = response.id;
        if response.clicked()
            || response.drag_started()
            || response.secondary_clicked()
            || response.middle_clicked()
            || std::mem::take(&mut self.take_focus)
        {
            response.request_focus();
        }
        let focused = response.has_focus();
        self.had_focus = focused;
        if focused {
            // Tab, arrows and Escape belong to the remote program, not egui.
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
            if opts.touch {
                // Ask for the on-screen keyboard, as a text field does (with
                // a hardware keyboard, the system leaves it hidden).
                let (row, col) = self.parser.screen().cursor_position();
                let at = rect.min + Vec2::new(f32::from(col) * cell_w, f32::from(row) * cell_h);
                ui.ctx().output_mut(|o| {
                    o.ime = Some(egui::output::IMEOutput {
                        purpose: egui::IMEPurpose::Terminal,
                        rect: outer,
                        cursor_rect: Rect::from_min_size(at, Vec2::new(1.0, cell_h)),
                        should_interrupt_composition: false,
                    })
                });
            }
        }

        let to_cell = |p: Pos2| -> (u16, u16) {
            let col = ((p.x - rect.left()) / cell_w).floor().clamp(0.0, (cols - 1) as f32) as u16;
            let row = ((p.y - rect.top()) / cell_h).floor().clamp(0.0, (rows - 1) as f32) as u16;
            (row, col)
        };
        // Programs like htop, mc and vim can ask for the mouse; Shift keeps it
        // for selecting text, as in xterm.
        let (mode, encoding) = {
            let s = self.parser.screen();
            (s.mouse_protocol_mode(), s.mouse_protocol_encoding())
        };
        let mods = ui.input(|i| i.modifiers);
        let reporting = mode != vt100::MouseProtocolMode::None && !mods.shift;
        let sgr = encoding == vt100::MouseProtocolEncoding::Sgr;

        // Ctrl+click opens a link.
        let mut link = None;
        if mods.command
            && let Some(p) = response.hover_pos()
        {
            let (row, col) = to_cell(p);
            let line = self.parser.screen().rows(0, cols).nth(row as usize).unwrap_or_default();
            if let Some((a, b, url)) = url_at(&line, col as usize) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                link = Some((row, a, b, url));
            }
        }
        let mut handled_click = false;
        if let Some((_, _, _, url)) = &link
            && response.clicked()
        {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            handled_click = true;
        }

        // On a touch screen a swipe scrolls (the scrollback, or the program
        // with the mouse wheel), and a long press starts selecting text.
        let touch = opts.touch;
        let swiping = touch && !self.touch_selecting && (response.dragged() || response.drag_stopped());
        let mut scroll = 0.0;
        if response.hovered() || response.dragged() {
            scroll += ui.input(|i| i.smooth_scroll_delta.y);
        }
        if swiping {
            scroll += response.drag_delta().y;
        }
        if touch && response.secondary_clicked() && !handled_click {
            // A long press selects the word under the finger; dragging on
            // extends the selection.
            if let Some(p) = ui.input(|i| i.pointer.press_origin()).or(response.interact_pointer_pos()) {
                let (row, col) = to_cell(p);
                let line: Vec<char> =
                    self.parser.screen().rows(0, cols).nth(row as usize).unwrap_or_default().chars().collect();
                let (start, end) = word_at(&line, col as usize);
                self.selection = Some(((row, start as u16), (row, end as u16)));
                self.touch_selecting = true;
            }
        } else if touch && self.touch_selecting {
            if response.dragged()
                && let (Some(p), Some((anchor, _))) = (response.interact_pointer_pos(), self.selection)
            {
                self.selection = Some((anchor, to_cell(p)));
            }
            if response.drag_stopped() || ui.input(|i| i.pointer.any_released()) {
                self.touch_selecting = false;
            }
        } else if touch && !reporting {
            if response.clicked() {
                self.selection = None;
            }
        } else if reporting && focused {
            if !swiping {
                self.report_mouse(ui, rect, &to_cell, mode, sgr, &mut out);
            }
        } else if !handled_click {
            // From where the button went down, not where the drag was noticed.
            if response.drag_started()
                && let Some(p) = ui.input(|i| i.pointer.press_origin()).or(response.interact_pointer_pos())
            {
                let c = to_cell(p);
                self.selection = Some((c, c));
            }
            if response.dragged()
                && let (Some(p), Some((anchor, _))) = (response.interact_pointer_pos(), self.selection)
            {
                self.selection = Some((anchor, to_cell(p)));
            }
            if response.drag_stopped()
                && opts.copy_on_select
                && let Some(t) = self.selected_text()
            {
                ui.ctx().copy_text(t);
            }
            if response.clicked() {
                self.selection = None;
            }
            // Right click copies a selection, or pastes when there is none
            // (PuTTY style); middle click pastes.
            if response.secondary_clicked() {
                match self.selected_text() {
                    Some(t) => {
                        ui.ctx().copy_text(t);
                        self.selection = None;
                    }
                    None => ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste),
                }
            }
            if response.middle_clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            }
        }

        if scroll != 0.0 && !reporting {
            self.scroll_accum += scroll;
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

        if scroll != 0.0 && reporting {
            // One wheel report per line scrolled, like a real mouse wheel.
            self.scroll_accum += scroll;
            let lines = (self.scroll_accum / cell_h).trunc() as i64;
            if lines != 0
                && let Some(p) = response.hover_pos().or(response.interact_pointer_pos())
            {
                self.scroll_accum -= lines as f32 * cell_h;
                let (r, c) = to_cell(p);
                let b = if lines > 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
                for _ in 0..lines.unsigned_abs().min(5) {
                    out.send.extend(encode_mouse(b, MouseKind::Press, r, c, sgr, false, mods.ctrl));
                }
            }
        }

        // Keys typed into a dialog or menu must not also reach the shell.
        let blocked = ui.ctx().memory(|m| m.top_modal_layer().is_some()) || egui::Popup::is_any_open(ui.ctx());
        if focused && !blocked {
            self.handle_input(ui, opts.confirm_paste, &mut out);
        }
        self.paste_confirm_ui(ui.ctx(), id, &mut out);

        let top = self.top_line();
        self.paint(ui, rect, &font, cell_w, cell_h, focused, top);
        if let Some((row, a, b, _)) = link {
            let y = rect.top() + (row as f32 + 1.0) * cell_h - 1.0;
            let x = |c: usize| rect.left() + c as f32 * cell_w;
            ui.painter().hline(x(a)..=x(b), y, Stroke::new(1.0, self.theme.fg));
        }
        if self.parser.screen().scrollback() > 0 {
            let label = format!("⬆ {} lines", self.parser.screen().scrollback());
            ui.painter().text(
                rect.right_top() + Vec2::new(-8.0, 4.0),
                egui::Align2::RIGHT_TOP,
                label,
                FontId::proportional(12.0),
                self.theme.ansi[3],
            );
        }
        if let Some(msg) = &self.exited {
            ui.painter().text(
                rect.center_bottom() - Vec2::new(0.0, 12.0),
                egui::Align2::CENTER_BOTTOM,
                msg,
                FontId::proportional(14.0),
                self.theme.ansi[3],
            );
        }
        out
    }

    /// Send mouse presses, releases, drags to the program (wheel is handled
    /// by the caller).
    fn report_mouse(
        &mut self,
        ui: &egui::Ui,
        rect: Rect,
        to_cell: &dyn Fn(Pos2) -> (u16, u16),
        mode: vt100::MouseProtocolMode,
        sgr: bool,
        out: &mut TerminalOutput,
    ) {
        use vt100::MouseProtocolMode as M;
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            match ev {
                Event::PointerButton { pos, button, pressed, modifiers } if rect.contains(pos) || !pressed => {
                    let b = match button {
                        egui::PointerButton::Primary => MouseButton::Left,
                        egui::PointerButton::Middle => MouseButton::Middle,
                        egui::PointerButton::Secondary => MouseButton::Right,
                        _ => continue,
                    };
                    let (r, c) = to_cell(pos);
                    if pressed {
                        self.mouse_down = Some((b, (r, c)));
                        out.send.extend(encode_mouse(b, MouseKind::Press, r, c, sgr, false, modifiers.ctrl));
                    } else if self.mouse_down.take().is_some() && mode != M::Press {
                        out.send.extend(encode_mouse(b, MouseKind::Release, r, c, sgr, false, modifiers.ctrl));
                    }
                }
                Event::PointerMoved(pos) if matches!(mode, M::ButtonMotion | M::AnyMotion) => {
                    if let Some((b, last)) = self.mouse_down {
                        let cell = to_cell(pos);
                        if cell != last {
                            self.mouse_down = Some((b, cell));
                            out.send.extend(encode_mouse(b, MouseKind::Drag, cell.0, cell.1, sgr, false, false));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn paste_confirm_ui(&mut self, ctx: &egui::Context, id: egui::Id, out: &mut TerminalOutput) {
        let Some(text) = &self.pending_paste else { return };
        let lines: Vec<&str> = text.lines().collect();
        let mut result = None;
        egui::Modal::new(id.with("paste")).show(ctx, |ui| {
            crate::dlg::fit_modal(ui);
            ui.set_max_width(560.0);
            ui.heading(format!("Paste {} lines?", lines.len()));
            ui.label("In a shell each line runs as a command as soon as it arrives.");
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                for l in lines.iter().take(200) {
                    ui.monospace(*l);
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Paste").clicked() {
                    result = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    result = Some(false);
                }
            });
        });
        match result {
            Some(true) => {
                let t = self.pending_paste.take().unwrap_or_default();
                out.send.extend_from_slice(t.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
            }
            Some(false) => self.pending_paste = None,
            None => {}
        }
    }

    fn handle_input(&mut self, ui: &egui::Ui, confirm_paste: bool, out: &mut TerminalOutput) {
        let events = ui.input(|i| i.events.clone());
        let screen = self.parser.screen();
        let app_cursor = screen.application_cursor();
        let bracketed = screen.bracketed_paste();
        // The on-screen Ctrl applied to a key press: the same key's text,
        // which follows it, is not typed as well.
        let mut ctrl_used = false;
        for ev in events {
            match ev {
                Event::Text(_) if std::mem::take(&mut ctrl_used) => {}
                Event::Text(t) if std::mem::take(&mut self.ctrl_latched) => {
                    let mut chars = t.chars();
                    if let Some(c) = chars.next() {
                        out.send.push(ctrl_byte(c).unwrap_or(c as u8));
                    }
                    out.send.extend_from_slice(chars.as_str().as_bytes());
                }
                Event::Key { key, pressed: true, modifiers, .. } if self.ctrl_latched => {
                    if let Some(bytes) = key_bytes(key, modifiers | Modifiers::CTRL, app_cursor) {
                        out.send.extend_from_slice(&bytes);
                        self.ctrl_latched = false;
                        ctrl_used = true;
                    }
                }
                Event::Text(t) | Event::Ime(egui::ImeEvent::Commit(t)) => out.send.extend_from_slice(t.as_bytes()),
                // Several lines into a plain shell: ask first. Programs that
                // asked for bracketed paste (editors, modern shells) get it
                // as text and do not run it.
                Event::Paste(t) if confirm_paste && !bracketed && t.trim_end().contains('\n') => {
                    self.pending_paste = Some(t);
                }
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

    #[allow(clippy::too_many_arguments)]
    fn paint(&self, ui: &egui::Ui, rect: Rect, font: &FontId, cell_w: f32, cell_h: f32, focused: bool, top: usize) {
        let hits: Vec<(usize, usize, usize, bool)> = match &self.search {
            Some(s) => s.matches.iter().enumerate().map(|(i, &(l, c, n))| (l, c, n, i == s.current)).collect(),
            None => Vec::new(),
        };
        let painter = ui.painter_at(rect);
        let th = self.theme;
        painter.rect_filled(rect, 0.0, th.bg);
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
                let (mut fg, mut bg) =
                    (color(cell.fgcolor(), th, th.fg, cell.bold()), color(cell.bgcolor(), th, th.bg, false));
                if cell.inverse() {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cell.dim() {
                    fg = fg.gamma_multiply(0.6);
                }
                if self.in_selection(row, col) {
                    bg = th.selection;
                }
                let line = top + row as usize;
                if let Some(&(.., current)) =
                    hits.iter().find(|&&(l, c, n, _)| l == line && (col as usize) >= c && (col as usize) < c + n)
                {
                    // The terminal theme's yellow: strong for the current
                    // match, faint for the others, text readable on each.
                    let yellow = th.ansi[3];
                    if current {
                        bg = yellow;
                        fg = th.bg;
                    } else {
                        bg = crate::look::mix(th.bg, yellow, 0.4);
                        fg = th.fg;
                    }
                }
                let text = if cell.has_contents() { cell.contents() } else { " " };
                let format = TextFormat {
                    font_id: font.clone(),
                    color: fg,
                    background: if bg == th.bg { Color32::TRANSPARENT } else { bg },
                    italics: cell.italic(),
                    underline: if cell.underline() { Stroke::new(1.0, fg) } else { Stroke::NONE },
                    ..Default::default()
                };
                job.append(text, 0.0, format);
                col += if cell.is_wide() { 2 } else { 1 };
            }
            let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
            painter.galley(Pos2::new(rect.left(), y), galley, th.fg);
        }
        if !screen.hide_cursor() && screen.scrollback() == 0 {
            let (r, c) = screen.cursor_position();
            let cursor = Rect::from_min_size(
                Pos2::new(rect.left() + c as f32 * cell_w, rect.top() + r as f32 * cell_h),
                Vec2::new(cell_w, cell_h),
            );
            if focused {
                painter.rect_filled(cursor, 0.0, th.cursor.gamma_multiply(0.7));
            } else {
                painter.rect_stroke(cursor, 0.0, Stroke::new(1.0, th.cursor), egui::StrokeKind::Inside);
            }
        }
    }
}

fn color(c: vt100::Color, theme: &Theme, default: Color32, bold: bool) -> Color32 {
    match c {
        vt100::Color::Default => default,
        // Bold brightens the first 8 colours, as xterm does.
        vt100::Color::Idx(i) if i < 8 && bold => theme.ansi[i as usize + 8],
        vt100::Color::Idx(i) if i < 16 => theme.ansi[i as usize],
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

#[derive(Debug, PartialEq, Eq)]
enum Query {
    CursorPosition,
    Status,
    DeviceAttributes,
}

/// The first terminal query in `b`: (offset, length, kind).
fn find_query(b: &[u8]) -> Option<(usize, usize, Query)> {
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == 0x1b && b[i + 1] == b'[' {
            let rest = &b[i + 2..];
            if rest.starts_with(b"6n") {
                return Some((i, 4, Query::CursorPosition));
            }
            if rest.starts_with(b"5n") {
                return Some((i, 4, Query::Status));
            }
            if rest.starts_with(b"c") {
                return Some((i, 3, Query::DeviceAttributes));
            }
            if rest.starts_with(b"0c") {
                return Some((i, 4, Query::DeviceAttributes));
            }
        }
        i += 1;
    }
    None
}

/// Turns terminal output into plain text for logs: drops escape sequences
/// and carriage returns. Keeps state so a sequence split across reads is
/// still removed.
#[derive(Default)]
struct AnsiStrip {
    state: StripState,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum StripState {
    #[default]
    Text,
    Esc,
    Csi,
    /// OSC/DCS string, until BEL or ESC \.
    Str,
    StrEsc,
}

impl AnsiStrip {
    fn strip(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        for &c in input {
            self.state = match (self.state, c) {
                (StripState::Text, 0x1b) => StripState::Esc,
                (StripState::Text, b'\r') => StripState::Text,
                (StripState::Text, c) if c == b'\n' || c == b'\t' || c >= 0x20 || c >= 0x80 => {
                    out.push(c);
                    StripState::Text
                }
                (StripState::Text, _) => StripState::Text,
                (StripState::Esc, b'[') => StripState::Csi,
                (StripState::Esc, b']' | b'P' | b'_' | b'^') => StripState::Str,
                (StripState::Esc, _) => StripState::Text,
                (StripState::Csi, 0x40..=0x7e) => StripState::Text,
                (StripState::Csi, _) => StripState::Csi,
                (StripState::Str, 0x07) => StripState::Text,
                (StripState::Str, 0x1b) => StripState::StrEsc,
                (StripState::Str, _) => StripState::Str,
                (StripState::StrEsc, b'\\') => StripState::Text,
                (StripState::StrEsc, _) => StripState::Str,
            };
        }
        out
    }
}

/// Bytes xterm sends for a key press that does not produce text.
/// The word (a run of non-blank characters) around column `col`: its first
/// and last columns. On a blank, just that column.
fn word_at(line: &[char], col: usize) -> (usize, usize) {
    let blank = |i: usize| line.get(i).is_none_or(|c| c.is_whitespace());
    if blank(col) {
        return (col, col);
    }
    let mut start = col;
    while start > 0 && !blank(start - 1) {
        start -= 1;
    }
    let mut end = col;
    while !blank(end + 1) {
        end += 1;
    }
    (start, end)
}

/// The byte Ctrl and `c` make in a terminal (Ctrl+C is 3), for the
/// on-screen Ctrl key.
fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' | 'A'..='Z' => Some(c.to_ascii_uppercase() as u8 & 0x1f),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

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

/// Finds `ESC ] 7 ; file://host/path` (ended by BEL or ESC \\), the way
/// shells report their folder, across chunk boundaries.
#[derive(Default)]
struct Osc7 {
    /// An unfinished sequence from the previous chunk.
    pending: Vec<u8>,
}

impl Osc7 {
    const START: &'static [u8] = b"\x1b]7;";
    const MAX: usize = 4096;

    /// The last folder reported in `bytes`, if any, and how many reports
    /// there were.
    fn scan(&mut self, bytes: &[u8]) -> (Option<String>, u64) {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);
        let mut found = None;
        let mut count = 0;
        let mut pos = 0;
        while let Some(i) = find(&buf[pos..], Self::START) {
            let start = pos + i + Self::START.len();
            let end = buf[start..].iter().position(|&b| b == 0x07 || b == 0x1b);
            match end {
                Some(e) if buf[start + e] == 0x07 || buf.get(start + e + 1) == Some(&b'\\') => {
                    if let Some(dir) = osc7_path(&buf[start..start + e]) {
                        found = Some(dir);
                        count += 1;
                    }
                    pos = start + e + 1;
                }
                // ESC at the very end: the backslash may be in the next chunk.
                Some(e) if start + e + 1 == buf.len() => {
                    self.keep(&buf[pos + i..]);
                    return (found, count);
                }
                Some(e) => pos = start + e,
                None => {
                    self.keep(&buf[pos + i..]);
                    return (found, count);
                }
            }
        }
        // A chunk can end part-way through the introducer itself.
        for n in (1..Self::START.len()).rev() {
            if buf.ends_with(&Self::START[..n]) {
                self.keep(&buf[buf.len() - n..]);
                break;
            }
        }
        (found, count)
    }

    fn keep(&mut self, b: &[u8]) {
        if b.len() <= Self::MAX {
            self.pending = b.to_vec();
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `file://host/some%20dir` → `/some dir`.
fn osc7_path(url: &[u8]) -> Option<String> {
    let url = std::str::from_utf8(url).ok()?;
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let mut out = Vec::with_capacity(path.len());
    let b = path.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = b.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc7_folder_reports() {
        let mut o = Osc7::default();
        let mut last = |b: &[u8]| o.scan(b).0;
        assert_eq!(last(b"ls\r\n\x1b]7;file://web-1/var/www\x07$ "), Some("/var/www".into()));
        assert_eq!(last(b"\x1b]7;file://h/a%20b\x1b\\"), Some("/a b".into()), "ST terminator, percent-encoding");
        // Split across chunks, including inside the introducer and the ST.
        assert_eq!(last(b"out\x1b]"), None);
        assert_eq!(last(b"7;file://h/et"), None);
        assert_eq!(last(b"c\x1b"), None);
        assert_eq!(last(b"\\more"), Some("/etc".into()));
        assert_eq!(last(b"\x1b]7;http://x/y\x07"), None);
        // The last of several wins and all are counted; other OSCs are ignored.
        assert_eq!(o.scan(b"\x1b]0;title\x07\x1b]7;file://h/a\x07\x1b]7;file://h/b\x07"), (Some("/b".into()), 2));
    }

    const NONE: Modifiers = Modifiers::NONE;
    const CTRL: Modifiers = Modifiers::CTRL;

    #[test]
    fn long_press_selects_a_word() {
        let line: Vec<char> = "ls -la /var/log  done".chars().collect();
        assert_eq!(super::word_at(&line, 9), (7, 14));
        assert_eq!(super::word_at(&line, 0), (0, 1));
        assert_eq!(super::word_at(&line, 15), (15, 15));
        assert_eq!(super::word_at(&line, 30), (30, 30));
    }

    #[test]
    fn on_screen_ctrl() {
        assert_eq!(super::ctrl_byte('c'), Some(3));
        assert_eq!(super::ctrl_byte('D'), Some(4));
        assert_eq!(super::ctrl_byte('['), Some(0x1b));
        assert_eq!(super::ctrl_byte('é'), None);
    }

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
        let t = &crate::themes::THEMES[0];
        assert_eq!(color(vt100::Color::Idx(1), t, t.fg, false), t.ansi[1]);
        assert_eq!(color(vt100::Color::Idx(1), t, t.fg, true), t.ansi[9]);
        assert_eq!(color(vt100::Color::Idx(16), t, t.fg, false), Color32::from_rgb(0, 0, 0));
        assert_eq!(color(vt100::Color::Idx(231), t, t.fg, false), Color32::from_rgb(255, 255, 255));
        assert_eq!(color(vt100::Color::Rgb(1, 2, 3), t, t.fg, false), Color32::from_rgb(1, 2, 3));
    }

    #[test]
    fn answers_cursor_position_queries() {
        let mut t = Terminal::new(14.0);
        assert_eq!(t.feed(b"abc\x1b[6n"), b"\x1b[1;4R");
        assert_eq!(t.feed(b"\r\nx\x1b[6ny\x1b[c"), b"\x1b[2;2R\x1b[?1;2c");
        assert!(t.feed(b"plain").is_empty());
    }

    #[test]
    fn strips_escapes_for_logs() {
        let mut s = AnsiStrip::default();
        assert_eq!(s.strip(b"\x1b[1;32mgreen\x1b[0m ok\r\n"), b"green ok\n");
        assert_eq!(s.strip(b"\x1b]0;title\x07after"), b"after");
        // A sequence split across two reads.
        assert_eq!(s.strip(b"a\x1b[3"), b"a");
        assert_eq!(s.strip(b"1mb"), b"b");
    }

    #[test]
    fn search_finds_lines_in_scrollback() {
        let mut t = Terminal::new(14.0);
        for i in 0..100 {
            t.feed(format!("line {i}\r\n").as_bytes());
        }
        t.search = Some(Search { query: "line 7".into(), ..Default::default() });
        t.run_search();
        let s = t.search.as_ref().unwrap();
        // "line 7" and "line 70".."line 79".
        assert_eq!(s.matches.len(), 11);
        assert_eq!(s.matches[0], (7, 0, 6));
        // The newest match is selected and scrolled into view.
        let line = s.matches[s.current].0;
        let top = t.top_line();
        assert!(line >= top && line < top + 24, "line {line} not in view from {top}");
    }

    #[test]
    fn selection_text() {
        let mut t = Terminal::new(14.0);
        t.feed(b"hello world\r\nsecond line");
        t.selection = Some(((0, 6), (1, 5)));
        assert_eq!(t.selected_text().as_deref(), Some("world\nsecond"));
    }
}
