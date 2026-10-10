//! The built-in editor for remote files, with save review and conflict screens.

use egui::{Color32, RichText};
use malasdoh_core::diff::{self, Diff, LineKind};
use malasdoh_core::edit::{Access, EditSession};
use malasdoh_core::validate::{CheckWhen, ValidationRule};

use crate::highlight;
use crate::textops::{self, Indent, detect_indent};

pub enum State {
    Ready,
    Busy(String),
    /// Showing what will change before writing.
    Review(Diff),
    /// The server copy changed under us; the diff is theirs against ours.
    /// `merged` is both sets of changes combined, when they do not overlap.
    Conflict {
        diff: Diff,
        theirs: Vec<u8>,
        merged: Option<String>,
    },
    /// Saving failed with "permission denied"; offer sudo.
    NeedsSudo(String),
    /// The config check failed. `rolled_back`: the original is back on the
    /// server; otherwise nothing was written.
    CheckFailed {
        rule: String,
        output: String,
        rolled_back: bool,
    },
}

pub enum EditorAction {
    /// Write now. `force` overwrites a newer server copy.
    Save {
        force: bool,
        /// Run the file's config check (if it has one and it is switched on).
        check: bool,
    },
    Reload,
    RetryWithSudo,
}

pub struct Editor {
    pub id: u64,
    pub session: EditSession,
    pub text: String,
    pub access: Access,
    pub state: State,
    pub message: Option<(String, bool)>,
    /// Edits made in an external editor arrive here; true once uploaded.
    pub external: Option<std::path::PathBuf>,
    /// The config check that applies to this file, and whether to run it.
    pub check: Option<ValidationRule>,
    pub check_enabled: bool,
    pub lang: highlight::Lang,
    hl_cache: Option<(u64, egui::text::LayoutJob)>,
    find: Find,
    /// The "go to line" box, when open, and whether it still needs focus.
    goto: Option<(String, bool)>,
    pub indent: Indent,
    /// Cursor/selection (char range) to put into the text box next frame.
    pending_select: Option<std::ops::Range<usize>>,
    scroll_to_line: Option<usize>,
    focus_text: bool,
    /// Input held back to the next frame so it is applied after an Enter/Tab
    /// we handled ourselves, in the order it was typed.
    deferred: Vec<egui::Event>,
}

#[derive(Default)]
struct Find {
    open: bool,
    replace_open: bool,
    query: String,
    with: String,
    case_sensitive: bool,
    /// Byte ranges of matches, and what they were computed for.
    matches: Vec<std::ops::Range<usize>>,
    key: u64,
    current: usize,
    focus: bool,
}

impl Editor {
    pub fn new(id: u64, session: EditSession, access: Access, check: Option<ValidationRule>) -> Self {
        let indent = detect_indent(&session.base_text);
        Self {
            lang: highlight::detect(&session.path, &session.base_text),
            hl_cache: None,
            check_enabled: check.is_some(),
            check,
            id,
            text: session.base_text.clone(),
            session,
            access,
            state: State::Ready,
            message: None,
            external: None,
            find: Find::default(),
            goto: None,
            indent,
            pending_select: None,
            scroll_to_line: None,
            focus_text: false,
            deferred: Vec::new(),
        }
    }

    pub fn title(&self) -> String {
        let name = malasdoh_core::sftp::file_name(&self.session.path).to_string();
        let root = if self.access.is_sudo() { " (root)" } else { "" };
        let dirty = if self.is_modified() { " •" } else { "" };
        format!("{name}{root}{dirty}")
    }

    pub fn is_modified(&self) -> bool {
        self.session.is_modified(&self.text)
    }

    /// Start a save, through review if the user wants one.
    pub fn request_save(&mut self, review: bool) -> Option<EditorAction> {
        if !self.is_modified() {
            self.message = Some(("No changes to save.".into(), false));
            return None;
        }
        if review {
            self.state = State::Review(diff::diff(&self.session.base_text, &self.text, 3));
            None
        } else {
            Some(EditorAction::Save { force: false, check: true })
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, font_size: f32, review: bool) -> Option<EditorAction> {
        let mut action = None;

        ui.horizontal(|ui| {
            ui.monospace(&self.session.path);
            ui.label(
                RichText::new(format!(
                    "{} · {}{}",
                    self.session.format.line_ending.label(),
                    if self.session.format.bom { "UTF-8 BOM" } else { "UTF-8" },
                    if self.access.is_sudo() { " · via sudo" } else { "" }
                ))
                .weak(),
            );
            egui::ComboBox::from_id_salt(("lang", self.id)).selected_text(self.lang.label()).width(130.0).show_ui(
                ui,
                |ui| {
                    for l in highlight::Lang::ALL {
                        ui.selectable_value(&mut self.lang, l, l.label());
                    }
                },
            );
            egui::ComboBox::from_id_salt(("indent", self.id)).selected_text(self.indent.label()).width(80.0).show_ui(
                ui,
                |ui| {
                    for i in [Indent::Spaces(2), Indent::Spaces(4), Indent::Spaces(8), Indent::Tab] {
                        ui.selectable_value(&mut self.indent, i, i.label());
                    }
                },
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let busy = matches!(self.state, State::Busy(_));
                if ui.add_enabled(!busy, egui::Button::new("💾 Save")).on_hover_text("Ctrl+S").clicked() {
                    action = self.request_save(review);
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("⟳ Reload"))
                    .on_hover_text("Discard changes and load the server copy")
                    .clicked()
                {
                    action = Some(EditorAction::Reload);
                }
                if ui.button("🔍").on_hover_text("Find (Ctrl+F), replace (Ctrl+H), go to line (Ctrl+G)").clicked() {
                    self.find.open = !self.find.open;
                    self.find.focus = self.find.open;
                }
                if let Some(rule) = &self.check {
                    let when = match rule.when {
                        CheckWhen::BeforeWrite => "on a copy before writing; nothing is written if it fails",
                        CheckWhen::AfterWrite => "after writing; the original is put back if it fails",
                    };
                    let mut tip = format!("Runs `{}` {when}.", rule.check);
                    if !rule.reload.trim().is_empty() {
                        tip.push_str(&format!("\nThen: `{}`", rule.reload));
                    }
                    ui.checkbox(&mut self.check_enabled, format!("Check: {}", rule.name)).on_hover_text(tip);
                }
            });
        });

        if let Some((msg, is_err)) = &self.message {
            let color = if *is_err { ui.visuals().error_fg_color } else { ui.visuals().weak_text_color() };
            ui.colored_label(color, msg);
        }
        if let State::Busy(what) = &self.state {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(what);
            });
        }

        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S))
            && !matches!(self.state, State::Busy(_))
        {
            action = self.request_save(review);
        }
        self.shortcuts(ui);
        self.update_matches();
        self.find_ui(ui);

        if let Some((l, c)) = self.cursor_pos(ui.ctx()) {
            ui.label(RichText::new(format!("Ln {l}, Col {c} · {} lines", self.text.lines().count())).weak().small());
        }
        ui.separator();
        let te_id = egui::Id::new(("editor-text", self.id));
        let editing = ui.memory(|m| m.has_focus(te_id));
        if editing {
            self.edit_keys(ui, te_id);
        } else if !self.deferred.is_empty() {
            let mut held = std::mem::take(&mut self.deferred);
            ui.input_mut(|i| {
                held.append(&mut i.events);
                i.events = held;
            });
        }
        if let Some(sel) = self.pending_select.take() {
            let mut st = egui::text_edit::TextEditState::load(ui.ctx(), te_id).unwrap_or_default();
            st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(sel.start),
                egui::text::CCursor::new(sel.end),
            )));
            st.store(ui.ctx(), te_id);
        }
        if std::mem::take(&mut self.focus_text) {
            ui.memory_mut(|m| m.request_focus(te_id));
        }

        let font = egui::FontId::monospace(font_size);
        let row_h = ui.ctx().fonts_mut(|f| f.row_height(&font));
        let lang = self.lang;
        let (matches, current) =
            if self.find.open { (self.find.matches.clone(), Some(self.find.current)) } else { (Vec::new(), None) };
        let mut cache = std::mem::take(&mut self.hl_cache);
        let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
            // Re-tokenize only when something that affects colours changed.
            let key = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                (buf.as_str(), &matches, current, lang as u8, font.size.to_bits(), ui.visuals().dark_mode).hash(&mut h);
                h.finish()
            };
            let job = match &cache {
                Some((k, job)) if *k == key => job.clone(),
                _ => {
                    let pal = highlight::Palette::new(ui.visuals().dark_mode, ui.visuals().text_color());
                    let job = highlight::layout(buf.as_str(), lang, &matches, current, &font, &pal);
                    cache = Some((key, job.clone()));
                    job
                }
            };
            let mut job = job;
            job.wrap.max_width = wrap_width;
            ui.ctx().fonts_mut(|f| f.layout_job(job))
        };
        let mut area = egui::ScrollArea::both().auto_shrink([false, false]).id_salt(("editor", self.id));
        if let Some(line) = self.scroll_to_line.take() {
            // Put the line about a third of the way down the view.
            let y = (line as f32 * row_h - ui.available_height() / 3.0).max(0.0);
            area = area.vertical_scroll_offset(y);
        }
        area.show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let lines = self.text.lines().count().max(1) + usize::from(self.text.ends_with('\n'));
                let numbers: String = (1..=lines).map(|n| format!("{n:>4}\n")).collect();
                ui.label(RichText::new(numbers).font(egui::FontId::monospace(font_size)).weak());
                ui.add(
                    egui::TextEdit::multiline(&mut self.text)
                        .id(te_id)
                        .code_editor()
                        .lock_focus(true)
                        .desired_width(f32::INFINITY)
                        .desired_rows(30)
                        .layouter(&mut layouter),
                );
            });
        });

        self.hl_cache = cache;

        if let Some(a) = self.modal_ui(ui.ctx()) {
            action = Some(a);
        }
        action
    }

    fn shortcuts(&mut self, ui: &mut egui::Ui) {
        use egui::{Key, Modifiers};
        let (f, h, g, f3, sf3) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::COMMAND, Key::F),
                i.consume_key(Modifiers::COMMAND, Key::H),
                i.consume_key(Modifiers::COMMAND, Key::G),
                i.consume_key(Modifiers::NONE, Key::F3),
                i.consume_key(Modifiers::SHIFT, Key::F3),
            )
        });
        if f || h {
            self.find.open = true;
            self.find.replace_open |= h;
            self.find.focus = true;
        }
        if g {
            self.goto = Some((String::new(), true));
        }
        if f3 {
            self.step_match(1);
        }
        if sf3 {
            self.step_match(-1);
        }
    }

    fn update_matches(&mut self) {
        if !self.find.open {
            return;
        }
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (&self.text, &self.find.query, self.find.case_sensitive).hash(&mut h);
            h.finish()
        };
        if key != self.find.key {
            self.find.key = key;
            self.find.matches = textops::find_all(&self.text, &self.find.query, self.find.case_sensitive);
            self.find.current = self.find.current.min(self.find.matches.len().saturating_sub(1));
        }
    }

    /// Move to the next (+1) or previous (-1) match, selecting and revealing it.
    fn step_match(&mut self, dir: isize) {
        self.find.open = true;
        self.update_matches();
        let n = self.find.matches.len();
        if n == 0 {
            return;
        }
        self.find.current = ((self.find.current as isize + dir).rem_euclid(n as isize)) as usize;
        self.reveal_match();
    }

    fn reveal_match(&mut self) {
        let Some(r) = self.find.matches.get(self.find.current).cloned() else { return };
        let (a, b) = (textops::byte_to_char(&self.text, r.start), textops::byte_to_char(&self.text, r.end));
        self.pending_select = Some(a..b);
        self.scroll_to_line = Some(textops::line_of(&self.text, r.start));
    }

    fn find_ui(&mut self, ui: &mut egui::Ui) {
        if let Some((line, focus)) = &mut self.goto {
            let mut close = false;
            let mut go = None;
            ui.horizontal(|ui| {
                ui.label("Go to line:");
                let r = ui.add(egui::TextEdit::singleline(line).desired_width(80.0));
                if std::mem::take(focus) {
                    r.request_focus();
                }
                // Consume the Enter, or the editor (focused next) would get it too.
                if (r.lost_focus() || r.has_focus())
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
                {
                    go = line.trim().parse::<usize>().ok().filter(|n| *n > 0);
                    close = true;
                }
                if ui.small_button("✖").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
            if let Some(n) = go {
                let c = textops::line_start(&self.text, n);
                self.pending_select = Some(c..c);
                self.scroll_to_line = Some(n - 1);
                self.focus_text = true;
            }
            if close {
                self.goto = None;
            }
        }
        if !self.find.open {
            return;
        }
        let n = self.find.matches.len();
        let mut step = 0;
        let mut replace_one = false;
        let mut replace_all = false;
        ui.horizontal(|ui| {
            let arrow = if self.find.replace_open { "⏷" } else { "⏵" };
            if ui.small_button(arrow).on_hover_text("Replace (Ctrl+H)").clicked() {
                self.find.replace_open = !self.find.replace_open;
            }
            ui.label("Find:");
            let r = ui.add(egui::TextEdit::singleline(&mut self.find.query).desired_width(220.0));
            if std::mem::take(&mut self.find.focus) {
                r.request_focus();
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                step = if ui.input(|i| i.modifiers.shift) { -1 } else { 1 };
                r.request_focus();
            }
            ui.toggle_value(&mut self.find.case_sensitive, "Aa").on_hover_text("Match case");
            if self.find.query.is_empty() {
                ui.label(RichText::new("").weak());
            } else if n == 0 {
                ui.colored_label(ui.visuals().warn_fg_color, "no matches");
            } else {
                ui.label(format!("{} of {n}", self.find.current + 1));
            }
            if ui.small_button("⬆").on_hover_text("Previous (Shift+F3)").clicked() {
                step = -1;
            }
            if ui.small_button("⬇").on_hover_text("Next (F3, Enter)").clicked() {
                step = 1;
            }
            if ui.small_button("✖").clicked() || (r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape))) {
                self.find.open = false;
            }
        });
        if self.find.replace_open {
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                ui.label("Replace:");
                ui.add(egui::TextEdit::singleline(&mut self.find.with).desired_width(220.0));
                replace_one = ui.add_enabled(n > 0, egui::Button::new("Replace")).clicked();
                replace_all = ui.add_enabled(n > 0, egui::Button::new(format!("Replace all ({n})"))).clicked();
            });
        }
        if replace_one && let Some(r) = self.find.matches.get(self.find.current).cloned() {
            self.text.replace_range(r, &self.find.with);
            self.update_matches();
            if !self.find.matches.is_empty() {
                // The next match now sits at the same index.
                self.find.current %= self.find.matches.len();
                self.reveal_match();
            }
        }
        if replace_all {
            let count =
                textops::replace_all(&mut self.text, &self.find.query, &self.find.with, self.find.case_sensitive);
            self.message =
                Some((format!("Replaced {count} occurrence(s). Review shows them all before saving."), false));
            self.update_matches();
        }
        if step != 0 {
            self.step_match(step);
        }
    }

    /// Enter keeps indentation; Tab and Shift+Tab indent with the file's own
    /// style instead of inserting a literal tab.
    ///
    /// The text box applies keys in order, but we must apply Enter/Tab
    /// ourselves, at the cursor as it is after every key typed before it. So
    /// each frame handles at most one of them, first, and events after the
    /// next one wait for the following frame.
    fn edit_keys(&mut self, ui: &mut egui::Ui, te_id: egui::Id) {
        use egui::{Event, Key};
        let special = |e: &Event| {
            matches!(e, Event::Key { key: Key::Enter | Key::Tab, pressed: true, modifiers, .. }
                if !modifiers.ctrl && !modifiers.alt && !modifiers.command && (!modifiers.shift || matches!(e, Event::Key { key: Key::Tab, .. })))
        };
        let mut handle = None;
        ui.input_mut(|i| {
            let mut events = std::mem::take(&mut self.deferred);
            events.append(&mut i.events);
            if events.first().is_some_and(special) {
                handle = Some(events.remove(0));
            }
            if let Some(k) = events.iter().position(special) {
                self.deferred = events.split_off(k);
            }
            i.events = events;
        });
        if !self.deferred.is_empty() {
            ui.ctx().request_repaint();
        }
        let Some(Event::Key { key, modifiers, .. }) = handle else { return };
        let Some(range) = egui::text_edit::TextEditState::load(ui.ctx(), te_id).and_then(|s| s.cursor.char_range())
        else {
            return;
        };
        let r = range.as_sorted_char_range();
        let sel = r.start.0..r.end.0;
        let new_sel = if key == Key::Enter {
            let c = textops::newline_with_indent(&mut self.text, sel, self.indent);
            c..c
        } else {
            textops::indent(&mut self.text, sel, self.indent, modifiers.shift)
        };
        self.pending_select = Some(new_sel);
    }

    /// "Ln 12, Col 5" for the status line.
    fn cursor_pos(&self, ctx: &egui::Context) -> Option<(usize, usize)> {
        let te_id = egui::Id::new(("editor-text", self.id));
        let c = egui::text_edit::TextEditState::load(ctx, te_id)?.cursor.char_range()?.primary.index.0;
        let b = textops::char_to_byte(&self.text, c);
        let line = textops::line_of(&self.text, b);
        let line_start = self.text[..b].rfind('\n').map(|i| i + 1).unwrap_or(0);
        Some((line + 1, self.text[line_start..b].chars().count() + 1))
    }

    fn modal_ui(&mut self, ctx: &egui::Context) -> Option<EditorAction> {
        let mut action = None;
        let mut close = false;
        match &self.state {
            State::Review(d) => {
                egui::Modal::new(egui::Id::new(("review", self.id))).show(ctx, |ui| {
                    ui.set_max_width(900.0);
                    ui.heading(format!("Save {}?", self.session.path));
                    ui.label(format!("+{} −{} lines", d.added, d.removed));
                    diff_ui(ui, d, egui::Id::new(("review-diff", self.id)));
                    ui.horizontal(|ui| {
                        if ui.button("💾 Save").clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command)
                        {
                            action = Some(EditorAction::Save { force: false, check: true });
                        }
                        if ui.button("Copy as patch").clicked() {
                            ui.ctx().copy_text(diff::unified(&self.session.base_text, &self.text, &self.session.path));
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            close = true;
                        }
                    });
                    ui.label(RichText::new("The original is backed up on this computer before writing.").weak());
                    if let Some(rule) = self.check.as_ref().filter(|_| self.check_enabled) {
                        let when = match rule.when {
                            CheckWhen::BeforeWrite => "before writing",
                            CheckWhen::AfterWrite => "after writing (rolled back if it fails)",
                        };
                        ui.label(RichText::new(format!("{} check `{}` runs {when}.", rule.name, rule.check)).weak());
                    }
                });
            }
            State::Conflict { diff: d, theirs, merged } => {
                let mut take_merge = false;
                egui::Modal::new(egui::Id::new(("conflict", self.id))).show(ctx, |ui| {
                    ui.set_max_width(900.0);
                    ui.heading("⚠ The file changed on the server");
                    ui.label("Someone else saved this file after you opened it. Below: their version (−) against yours (+).");
                    diff_ui(ui, d, egui::Id::new(("conflict-diff", self.id)));
                    if merged.is_some() {
                        ui.colored_label(ui.visuals().text_color(), "Your edits and theirs touch different lines, so both can be kept.");
                    } else {
                        ui.colored_label(ui.visuals().warn_fg_color, "Your edits and theirs overlap; choose which version wins, or copy yours and redo the edit.");
                    }
                    ui.horizontal(|ui| {
                        if merged.is_some() && ui.button("Keep both changes").on_hover_text("Apply your edits on top of their version, then review").clicked() {
                            take_merge = true;
                        }
                        if ui.button("Overwrite with mine").on_hover_text("Their version is backed up first").clicked() {
                            action = Some(EditorAction::Save { force: true, check: true });
                        }
                        if ui.button("Copy my version").clicked() {
                            ui.ctx().copy_text(self.text.clone());
                        }
                        if ui.button("Discard mine, load theirs").clicked() {
                            action = Some(EditorAction::Reload);
                        }
                        if ui.button("Keep editing").clicked() {
                            close = true;
                        }
                    });
                });
                if take_merge {
                    let merged = merged.clone().unwrap();
                    match self.session.rebase(theirs) {
                        Ok(()) => {
                            self.text = merged;
                            self.state = State::Review(diff::diff(&self.session.base_text, &self.text, 3));
                            self.message = Some(("Merged onto the server's version; review and save.".into(), false));
                        }
                        Err(e) => {
                            self.state = State::Ready;
                            self.message = Some((format!("Cannot merge: {e:#}"), true));
                        }
                    }
                    return action;
                }
            }
            State::NeedsSudo(err) => {
                let err = err.clone();
                egui::Modal::new(egui::Id::new(("sudo", self.id))).show(ctx, |ui| {
                    ui.heading("Permission denied");
                    ui.label(err);
                    ui.label("Save through sudo instead? Your edits are kept either way.");
                    ui.horizontal(|ui| {
                        if ui.button("Save as root (sudo)").clicked() {
                            action = Some(EditorAction::RetryWithSudo);
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }
            State::CheckFailed { rule, output, rolled_back } => {
                egui::Modal::new(egui::Id::new(("check-failed", self.id))).show(ctx, |ui| {
                    ui.set_max_width(900.0);
                    ui.heading(format!("⚠ {rule} check failed"));
                    ui.label(if *rolled_back {
                        "Your version was written, failed the check, and the original has been put back. The server is as it was."
                    } else {
                        "Your version failed the check, so nothing was written. The server is as it was."
                    });
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(output).monospace()).selectable(true));
                    });
                    ui.label(RichText::new("Your edits are still in the editor.").weak());
                    ui.horizontal(|ui| {
                        if ui.button("Keep editing").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            close = true;
                        }
                        if ui.button("Copy output").clicked() {
                            ui.ctx().copy_text(output.clone());
                        }
                        if ui
                            .button("Save without checking")
                            .on_hover_text("Write it anyway. The original is still backed up first.")
                            .clicked()
                        {
                            action = Some(EditorAction::Save { force: false, check: false });
                        }
                    });
                });
            }
            State::Ready | State::Busy(_) => {}
        }
        if close {
            self.state = State::Ready;
        }
        action
    }
}

pub fn diff_ui(ui: &mut egui::Ui, d: &Diff, id: egui::Id) {
    if d.is_empty() {
        ui.label("No differences.");
        return;
    }
    let add_bg = Color32::from_rgba_unmultiplied(60, 160, 60, 60);
    let del_bg = Color32::from_rgba_unmultiplied(200, 60, 60, 60);
    egui::ScrollArea::vertical().id_salt(id).max_height(500.0).auto_shrink([false, true]).show(ui, |ui| {
        for (i, h) in d.hunks.iter().enumerate() {
            if i > 0 {
                ui.separator();
            }
            ui.label(RichText::new(format!("@@ line {} @@", h.new_start)).weak().monospace());
            for l in &h.lines {
                let (sign, bg) = match l.kind {
                    LineKind::Context => (' ', Color32::TRANSPARENT),
                    LineKind::Added => ('+', add_bg),
                    LineKind::Removed => ('-', del_bg),
                };
                let num = l.new_no.or(l.old_no).map(|n| format!("{n:>5}")).unwrap_or_default();
                let text = RichText::new(format!("{num} {sign} {}", l.text)).monospace().background_color(bg);
                ui.label(text);
            }
        }
    });
}
