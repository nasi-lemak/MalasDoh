//! Connection → Export / Import: saved connections (and optionally snippets
//! and trusted host keys) in one passphrase-encrypted file.

use egui::RichText;
use malasdoh_core::export::{self, Bundle};
use malasdoh_core::profile::ProfileStore;

use super::App;
use crate::snippets::SnippetStore;

pub struct ExportDialog {
    pass: String,
    pass2: String,
    snippets: bool,
    known_hosts: bool,
    error: Option<String>,
}

pub struct ImportDialog {
    path: std::path::PathBuf,
    pass: String,
    error: Option<String>,
}

impl Default for ExportDialog {
    fn default() -> Self {
        Self { pass: String::new(), pass2: String::new(), snippets: true, known_hosts: true, error: None }
    }
}

impl App {
    pub(super) fn start_export(&mut self) {
        self.export_dlg = Some(ExportDialog::default());
    }

    pub(super) fn start_import(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Import a MalasDoh export")
            .add_filter("MalasDoh export", &["malasdoh"])
            .pick_file()
        {
            self.import_dlg = Some(ImportDialog { path, pass: String::new(), error: None });
        }
    }

    pub(super) fn backup_ui(&mut self, ctx: &egui::Context) {
        self.export_ui(ctx);
        self.import_ui(ctx);
    }

    fn export_ui(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.export_dlg else { return };
        let mut go = false;
        let mut close = false;
        egui::Modal::new("export-dialog".into()).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading("Export connections");
            ui.label(format!("{} saved connection(s), encrypted with a passphrase.", self.profiles.profiles.len()));
            ui.checkbox(&mut d.snippets, "Include snippets");
            ui.checkbox(&mut d.known_hosts, "Include the host keys you accepted");
            egui::Grid::new("export-pass").num_columns(2).show(ui, |ui| {
                ui.label("Passphrase");
                let r = ui.add(egui::TextEdit::singleline(&mut d.pass).password(true));
                if d.pass.is_empty() && d.pass2.is_empty() {
                    r.request_focus();
                }
                ui.end_row();
                ui.label("Again");
                ui.add(egui::TextEdit::singleline(&mut d.pass2).password(true));
                ui.end_row();
            });
            ui.label(RichText::new("Saved passwords stay in this computer's keychain and are not exported.").weak());
            if let Some(e) = &d.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.horizontal(|ui| {
                let ok = d.pass.chars().count() >= 8 && d.pass == d.pass2;
                if ui.add_enabled(ok, egui::Button::new("Export…")).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                if d.pass.chars().count() < 8 {
                    ui.label(RichText::new("at least 8 characters").weak());
                } else if d.pass != d.pass2 {
                    ui.label(RichText::new("the two do not match").weak());
                }
            });
        });
        if go {
            let d = self.export_dlg.as_ref().unwrap();
            let bundle = Bundle {
                profiles: toml::to_string_pretty(&self.profiles).unwrap_or_default(),
                snippets: d.snippets.then(|| toml::to_string_pretty(&self.snippets).unwrap_or_default()),
                known_hosts: d
                    .known_hosts
                    .then(|| std::fs::read_to_string(self.shared.paths.known_hosts_file()).unwrap_or_default()),
            };
            let result = export::seal(&bundle, &d.pass).and_then(|text| {
                let Some(path) = rfd::FileDialog::new()
                    .set_title("Save the export")
                    .set_file_name("connections.malasdoh")
                    .save_file()
                else {
                    return Ok(None);
                };
                malasdoh_core::paths::write_atomic(&path, text.as_bytes())?;
                Ok(Some(path))
            });
            match result {
                Ok(Some(path)) => {
                    self.export_dlg = None;
                    self.notify(format!("Exported to {}", path.display()), false);
                }
                Ok(None) => {}
                Err(e) => self.export_dlg.as_mut().unwrap().error = Some(format!("{e:#}")),
            }
        } else if close {
            self.export_dlg = None;
        }
    }

    fn import_ui(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.import_dlg else { return };
        let mut go = false;
        let mut close = false;
        egui::Modal::new("import-dialog".into()).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading("Import connections");
            ui.monospace(d.path.display().to_string());
            ui.horizontal(|ui| {
                ui.label("Passphrase");
                let r = ui.add(egui::TextEdit::singleline(&mut d.pass).password(true));
                r.request_focus();
                go |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            });
            ui.label(
                RichText::new("Connections and snippets you already have are kept; only new ones are added.").weak(),
            );
            if let Some(e) = &d.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.horizontal(|ui| {
                go |= ui.add_enabled(!d.pass.is_empty(), egui::Button::new("Import")).clicked();
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        });
        if go {
            let d = self.import_dlg.as_ref().unwrap();
            let result = std::fs::read_to_string(&d.path)
                .map_err(anyhow::Error::from)
                .and_then(|t| export::open(&t, &d.pass))
                .and_then(|b| self.merge(b));
            match result {
                Ok(msg) => {
                    self.import_dlg = None;
                    self.notify(msg, false);
                }
                Err(e) => self.import_dlg.as_mut().unwrap().error = Some(format!("{e:#}")),
            }
        } else if close {
            self.import_dlg = None;
        }
    }

    /// Add what is new; never replace or remove anything here.
    fn merge(&mut self, b: Bundle) -> anyhow::Result<String> {
        let incoming: ProfileStore = toml::from_str(&b.profiles)?;
        let added = self.profiles.merge(incoming);
        self.save_profiles();
        let mut msg = format!("Imported {added} connection(s)");
        if let Some(s) = b.snippets
            && let Ok(store) = toml::from_str::<SnippetStore>(&s)
        {
            let before = self.snippets.snippets.len();
            for sn in store.snippets {
                if !self.snippets.snippets.contains(&sn) {
                    self.snippets.snippets.push(sn);
                }
            }
            let n = self.snippets.snippets.len() - before;
            if n > 0 {
                self.snippets.save(&self.shared.paths.config_dir.join("snippets.toml"))?;
            }
            msg.push_str(&format!(", {n} snippet(s)"));
        }
        if let Some(kh) = b.known_hosts {
            let path = self.shared.paths.known_hosts_file();
            let mut current = std::fs::read_to_string(&path).unwrap_or_default();
            let have: std::collections::HashSet<String> = current.lines().map(|l| l.trim().to_string()).collect();
            let mut n = 0;
            for l in kh.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                if !have.contains(l) {
                    if !current.is_empty() && !current.ends_with('\n') {
                        current.push('\n');
                    }
                    current.push_str(l);
                    current.push('\n');
                    n += 1;
                }
            }
            if n > 0 {
                malasdoh_core::paths::write_atomic(&path, current.as_bytes())?;
            }
            msg.push_str(&format!(", {n} host key(s)"));
        }
        msg.push('.');
        Ok(msg)
    }
}
