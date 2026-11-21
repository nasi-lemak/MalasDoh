//! Settings → Appearance: the theme, accent colour, terminal colours and
//! fonts, and themes of your own.

use egui::RichText;

use super::{App, theme_combo};
use crate::look::{self, AppTheme, FOLLOW_SYSTEM, SAME_AS_APP};
use crate::settings::Settings;

/// What the Appearance tab asks the app to do with theme files.
pub enum ThemeAction {
    /// Write the theme shown now to a new file, to change it from there.
    Copy,
    OpenFolder,
    Reload,
}

pub(super) fn appearance_ui(
    ui: &mut egui::Ui,
    s: &mut Settings,
    themes: &[AppTheme],
    errors: &[String],
    action: &mut Option<ThemeAction>,
) {
    egui::Grid::new("appearance").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
        ui.label("Theme");
        theme_combo(ui, "ui-theme", &mut s.ui_theme, themes, None, true);
        ui.end_row();
        if s.ui_theme == FOLLOW_SYSTEM {
            ui.label("When the system is dark");
            theme_combo(ui, "ui-theme-dark", &mut s.system_dark_theme, themes, Some(true), false);
            ui.end_row();
            ui.label("When it is light");
            theme_combo(ui, "ui-theme-light", &mut s.system_light_theme, themes, Some(false), false);
            ui.end_row();
        }

        ui.label("Accent colour");
        ui.horizontal(|ui| {
            let mut own = !s.accent.is_empty();
            if ui.checkbox(&mut own, "Your own").on_hover_text("Selection, the pressed button and links").changed() {
                s.accent = if own { "#e5a50a".into() } else { String::new() };
            }
            if let Some(c) = look::parse_color(&s.accent) {
                let mut rgb = [c.r(), c.g(), c.b()];
                if ui.color_edit_button_srgb(&mut rgb).changed() {
                    s.accent = look::hex(egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
                }
            } else {
                ui.label(RichText::new("the theme's").weak());
            }
        });
        ui.end_row();

        ui.label("Terminal colours");
        egui::ComboBox::from_id_salt("term-theme").selected_text(&s.terminal_theme).width(200.0).show_ui(ui, |ui| {
            ui.selectable_value(&mut s.terminal_theme, SAME_AS_APP.to_string(), SAME_AS_APP)
                .on_hover_text("The colours of the app theme's own terminal");
            ui.separator();
            for t in look::terminal_themes(themes) {
                ui.selectable_value(&mut s.terminal_theme, t.name.to_string(), t.name);
            }
        });
        ui.end_row();

        ui.label("Terminal font size");
        ui.add(egui::Slider::new(&mut s.terminal_font_size, 9.0..=28.0));
        ui.end_row();
        ui.label("Editor font size");
        ui.add(egui::Slider::new(&mut s.editor_font_size, 9.0..=28.0));
        ui.end_row();
        ui.label("Monospace font");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut s.mono_font_file)
                    .hint_text("built-in (Hack)")
                    .desired_width(crate::dlg::field_width(ui, 260.0)),
            )
            .on_hover_text("A monospace .ttf or .otf, used by terminals and the editor");
            if (ui.button("Browse…").clicked() || crate::dialogs::ready("mono-font"))
                && let Some(f) = crate::dialogs::FileDialog::new()
                    .id("mono-font")
                    .add_filter("Fonts", &["ttf", "otf", "ttc"])
                    .pick_file()
            {
                s.mono_font_file = f.display().to_string();
            }
            if !s.mono_font_file.is_empty() && ui.small_button("Reset").clicked() {
                s.mono_font_file.clear();
            }
        });
        ui.end_row();
    });

    ui.add_space(12.0);
    ui.label(RichText::new("Themes of your own").strong());
    ui.label(
        RichText::new(
            "A theme is a small text file of colours. Make a copy of the theme you see now, change its colours in \
             any text editor, then press Reload: it appears in the list above.",
        )
        .weak(),
    );
    ui.horizontal(|ui| {
        if ui.button("Copy this theme to a new file").clicked() {
            *action = Some(ThemeAction::Copy);
        }
        if ui.button("Open the themes folder").clicked() {
            *action = Some(ThemeAction::OpenFolder);
        }
        if ui.button("⟳ Reload").on_hover_text("Read the theme files again").clicked() {
            *action = Some(ThemeAction::Reload);
        }
    });
    let mine: Vec<&AppTheme> = themes.iter().filter(|t| t.file.is_some()).collect();
    if !mine.is_empty() {
        let names: Vec<&str> = mine.iter().map(|t| t.name.as_str()).collect();
        ui.label(RichText::new(format!("Yours: {}", names.join(", "))).small());
    }
    for e in errors {
        ui.colored_label(ui.visuals().error_fg_color, format!("Not loaded: {e}"));
    }
}

impl App {
    /// The theme on screen now (with "Follow system", the one for the
    /// system's current mode).
    fn shown_theme(&self, ctx: &egui::Context) -> AppTheme {
        let s = &self.settings;
        let name = if s.ui_theme == FOLLOW_SYSTEM {
            if ctx.global_style().visuals.dark_mode { &s.system_dark_theme } else { &s.system_light_theme }
        } else {
            &s.ui_theme
        };
        self.themes.iter().find(|t| &t.name == name).cloned().unwrap_or_else(|| look::builtin().remove(0))
    }

    pub(super) fn theme_action(&mut self, ctx: &egui::Context, action: ThemeAction) {
        let dir = self.shared.paths.themes_dir();
        match action {
            ThemeAction::Copy => {
                let base = self.shown_theme(ctx);
                let mut name = format!("My {}", base.name.trim_start_matches("My "));
                let mut n = 2;
                while self.themes.iter().any(|t| t.name == name) {
                    name = format!("My {} {n}", base.name.trim_start_matches("My "));
                    n += 1;
                }
                let path = look::new_file_path(&dir, &name);
                let written = std::fs::create_dir_all(&dir)
                    .map_err(anyhow::Error::from)
                    .and_then(|_| malasdoh_core::paths::write_atomic(&path, look::to_file(&base, &name).as_bytes()));
                match written {
                    Ok(()) => {
                        self.reload_themes();
                        self.settings.ui_theme = name.clone();
                        self.notify(
                            format!("Made \"{name}\" in {}. Change its colours there, then Reload.", path.display()),
                            false,
                        );
                        if let Err(e) = look::open_folder(&dir) {
                            tracing::warn!("opening {}: {e}", dir.display());
                        }
                    }
                    Err(e) => self.notify(format!("Could not write {}: {e:#}", path.display()), true),
                }
            }
            ThemeAction::OpenFolder => {
                if let Err(e) = look::open_folder(&dir) {
                    self.notify(format!("Could not open {}: {e}", dir.display()), true);
                }
            }
            ThemeAction::Reload => {
                self.reload_themes();
                let n = self.themes.iter().filter(|t| t.file.is_some()).count();
                let msg = match (n, self.theme_errors.len()) {
                    (n, 0) => format!("{n} theme(s) of your own."),
                    (n, e) => format!("{n} theme(s) of your own; {e} file(s) could not be read (see Settings)."),
                };
                self.notify(msg, !self.theme_errors.is_empty());
            }
        }
        look::apply(ctx, &self.themes, &super::look_choice(&self.settings));
    }

    fn reload_themes(&mut self) {
        (self.themes, self.theme_errors) = look::load_all(&self.shared.paths.themes_dir());
    }
}
