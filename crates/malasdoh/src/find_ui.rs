//! Find files on the server by name, and optionally by what they contain.

use egui::RichText;

pub struct FindWindow {
    pub open: bool,
    pub dir: String,
    name: String,
    contains: String,
    depth: u32,
    limit_depth: bool,
    pub sudo: bool,
    pub running: bool,
    pub results: Vec<String>,
    pub truncated: bool,
    pub error: Option<String>,
    searched: bool,
}

/// Results are cut at this many.
pub const LIMIT: usize = 500;

pub enum FindAction {
    Search {
        dir: String,
        name: String,
        contains: String,
        depth: Option<u32>,
        sudo: bool,
    },
    /// Show the folder holding this path, with it selected.
    Reveal(String),
    Open(String),
}

impl Default for FindWindow {
    fn default() -> Self {
        Self {
            open: false,
            dir: String::new(),
            name: String::new(),
            contains: String::new(),
            depth: 6,
            limit_depth: false,
            sudo: false,
            running: false,
            results: Vec::new(),
            truncated: false,
            error: None,
            searched: false,
        }
    }
}

impl FindWindow {
    pub fn show(&mut self, dir: &str) {
        self.open = true;
        if !self.running {
            self.dir = dir.to_string();
        }
    }

    pub fn done(&mut self, result: Result<Vec<String>, String>) {
        self.running = false;
        self.searched = true;
        match result {
            Ok(mut r) => {
                self.truncated = r.len() > LIMIT;
                r.truncate(LIMIT);
                self.results = r;
                self.error = None;
            }
            Err(e) => {
                self.results.clear();
                self.error = Some(e);
            }
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, session: u64) -> Option<FindAction> {
        if !self.open {
            return None;
        }
        let mut action = None;
        let mut open = self.open;
        egui::Window::new("Find files")
            .id(egui::Id::new(("find", session)))
            .open(&mut open)
            .default_size([620.0, 460.0])
            .show(ctx, |ui| {
                let mut go = false;
                egui::Grid::new("find-form").num_columns(2).show(ui, |ui| {
                    ui.label("In folder");
                    ui.add(egui::TextEdit::singleline(&mut self.dir).desired_width(400.0));
                    ui.end_row();
                    ui.label("Name");
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.name).hint_text("*.conf, nginx*, …").desired_width(250.0),
                    );
                    go |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.end_row();
                    ui.label("Containing");
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.contains)
                            .hint_text("text inside the file (optional)")
                            .desired_width(250.0),
                    );
                    go |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.end_row();
                    ui.label("");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.limit_depth, "At most");
                        ui.add_enabled(self.limit_depth, egui::DragValue::new(&mut self.depth).range(1..=50));
                        ui.label("folders deep");
                        ui.checkbox(&mut self.sudo, "As root (sudo)");
                    });
                    ui.end_row();
                });
                ui.label(
                    RichText::new(
                        "Names match without case. Stays on one filesystem, skips binary and files over 10 MB when \
                         searching contents, and stops after a minute.",
                    )
                    .weak()
                    .small(),
                );
                ui.horizontal(|ui| {
                    let ready = !self.running && (!self.name.trim().is_empty() || !self.contains.is_empty());
                    if ui.add_enabled(ready, egui::Button::new("🔍 Find")).clicked() || (go && ready) {
                        self.running = true;
                        self.error = None;
                        action = Some(FindAction::Search {
                            dir: self.dir.trim().to_string(),
                            name: self.name.trim().to_string(),
                            contains: self.contains.clone(),
                            depth: self.limit_depth.then_some(self.depth),
                            sudo: self.sudo,
                        });
                    }
                    if self.running {
                        ui.spinner();
                        ui.label("Searching…");
                    } else if self.searched && self.error.is_none() {
                        let mut s = format!("{} found", self.results.len());
                        if self.truncated {
                            s.push_str(&format!(" (only the first {LIMIT} shown)"));
                        }
                        ui.label(s);
                    }
                });
                if let Some(e) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for p in &self.results {
                        let r = ui
                            .add(egui::Label::new(RichText::new(p).monospace()).sense(egui::Sense::click()).truncate())
                            .on_hover_text("Click: show in the browser. Double-click: open in the editor.");
                        if r.double_clicked() {
                            action = Some(FindAction::Open(p.clone()));
                        } else if r.clicked() {
                            action = Some(FindAction::Reveal(p.clone()));
                        }
                    }
                });
            });
        self.open = open;
        action
    }
}
