//! App themes: one choice colours the window, the terminal and the editor
//! together. Built-in themes pair a terminal palette (`themes.rs`) with
//! window colours; your own themes are small TOML files in the settings
//! folder's `themes` directory.

use std::path::{Path, PathBuf};

use egui::{Color32, Id, Stroke, Visuals};
use serde::{Deserialize, Serialize};

use crate::themes::{THEMES, Theme};

/// The "theme" that switches with the operating system's dark or light mode.
pub const FOLLOW_SYSTEM: &str = "Follow system";
/// Terminal colours that come from the app theme.
pub const SAME_AS_APP: &str = "Same as the app";

/// The window's own colours. Everything else (buttons, hover, selection) is
/// mixed from these.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiColors {
    /// Side panels and the main area.
    pub panel: Color32,
    /// Windows, menus and pop-ups.
    pub window: Color32,
    /// Text boxes and lists.
    pub field: Color32,
    pub text: Color32,
    /// Selection, the active button, links.
    pub accent: Color32,
    /// Lines between panels, window edges.
    pub border: Color32,
}

#[derive(Clone, Debug)]
pub struct AppTheme {
    pub name: String,
    pub dark: bool,
    /// `None`: egui's own dark or light look.
    pub ui: Option<UiColors>,
    pub term: &'static Theme,
    /// Where a theme of your own came from.
    pub file: Option<PathBuf>,
}

const fn c(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

fn term(name: &str) -> &'static Theme {
    THEMES.iter().find(|t| t.name == name).unwrap_or(&THEMES[0])
}

/// The themes that come with MalasDoh. "Dark" first: it is the default.
pub fn builtin() -> Vec<AppTheme> {
    let t = |name: &str, dark: bool, ui: Option<UiColors>, term_name: &str| AppTheme {
        name: name.into(),
        dark,
        ui,
        term: term(term_name),
        file: None,
    };
    let ui = |panel, window, field, text, accent, border| {
        Some(UiColors {
            panel: c(panel),
            window: c(window),
            field: c(field),
            text: c(text),
            accent: c(accent),
            border: c(border),
        })
    };
    vec![
        t("Dark", true, None, "Tomorrow Night"),
        t("Light", false, None, "Light"),
        t("Tomorrow Night", true, ui(0x1d1f21, 0x25282b, 0x141517, 0xc5c8c6, 0x81a2be, 0x373b41), "Tomorrow Night"),
        t("Dracula", true, ui(0x282a36, 0x2f3241, 0x1e1f29, 0xf8f8f2, 0xbd93f9, 0x44475a), "Dracula"),
        t("Gruvbox Dark", true, ui(0x282828, 0x32302f, 0x1d2021, 0xebdbb2, 0xfabd2f, 0x504945), "Gruvbox Dark"),
        t("Solarized Dark", true, ui(0x002b36, 0x073642, 0x00212b, 0x93a1a1, 0x268bd2, 0x0e4b5a), "Solarized Dark"),
        t("Solarized Light", false, ui(0xfdf6e3, 0xeee8d5, 0xfffcf2, 0x586e75, 0x268bd2, 0xd6cdb4), "Solarized Light"),
    ]
}

/// `a` moved towards `b` by `t` (0..=1), in sRGB.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// How bright a colour looks (0..=1), for picking readable text on it.
fn luma(c: Color32) -> f32 {
    (0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32) / 255.0
}

/// The colours `theme` gives egui, with `accent` in place of its own.
pub fn visuals(theme: &AppTheme, accent: Option<Color32>) -> Visuals {
    let mut v = if theme.dark { Visuals::dark() } else { Visuals::light() };
    if let Some(u) = theme.ui {
        let shade = |t: f32| mix(u.panel, u.text, t);
        v.panel_fill = u.panel;
        v.window_fill = u.window;
        v.extreme_bg_color = u.field;
        v.text_edit_bg_color = Some(u.field);
        v.faint_bg_color = shade(0.04);
        v.code_bg_color = shade(0.08);
        v.window_stroke = Stroke::new(1.0, u.border);
        let strong = if theme.dark { mix(u.text, Color32::WHITE, 0.5) } else { mix(u.text, Color32::BLACK, 0.5) };
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = u.panel;
        w.noninteractive.weak_bg_fill = u.panel;
        w.noninteractive.bg_stroke = Stroke::new(1.0, u.border);
        w.noninteractive.fg_stroke = Stroke::new(1.0, u.text);
        w.inactive.bg_fill = shade(0.14);
        w.inactive.weak_bg_fill = shade(0.09);
        w.inactive.fg_stroke = Stroke::new(1.0, u.text);
        w.hovered.bg_fill = shade(0.2);
        w.hovered.weak_bg_fill = shade(0.16);
        w.hovered.bg_stroke = Stroke::new(1.0, shade(0.4));
        w.hovered.fg_stroke = Stroke::new(1.5, strong);
        w.active.bg_fill = mix(u.panel, u.accent, 0.5);
        w.active.weak_bg_fill = mix(u.panel, u.accent, 0.4);
        w.active.bg_stroke = Stroke::new(1.0, u.accent);
        w.active.fg_stroke = Stroke::new(2.0, strong);
        w.open.bg_fill = shade(0.12);
        w.open.weak_bg_fill = shade(0.12);
        w.open.bg_stroke = Stroke::new(1.0, u.border);
        w.open.fg_stroke = Stroke::new(1.0, u.text);
        set_accent(&mut v, accent.unwrap_or(u.accent));
    } else if let Some(a) = accent {
        set_accent(&mut v, a);
    }
    let s = Semantic::new(theme, &v);
    v.warn_fg_color = s.warn;
    v.error_fg_color = s.bad;
    // Softer corners than egui's defaults.
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = egui::CornerRadius::same(4);
    }
    v.window_corner_radius = egui::CornerRadius::same(8);
    v.menu_corner_radius = egui::CornerRadius::same(6);
    v
}

/// Room inside and between widgets: a little more than egui's defaults,
/// which feel cramped at the sizes MalasDoh uses. On a touch screen, room
/// enough for a finger.
pub fn spacing(style: &mut egui::Style, touch: bool) {
    style.spacing.button_padding = egui::vec2(6.0, 3.0);
    style.spacing.item_spacing = egui::vec2(8.0, 4.0);
    style.spacing.menu_margin = egui::Margin::same(6);
    style.spacing.window_margin = egui::Margin::same(10);
    if touch {
        style.spacing.button_padding = egui::vec2(10.0, 7.0);
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.interact_size.y = 36.0;
        style.spacing.text_edit_width = 320.0;
        style.spacing.icon_width = 20.0;
        style.spacing.icon_width_inner = 12.0;
        style.spacing.scroll = egui::style::ScrollStyle::solid();
        style.spacing.scroll.bar_width = 10.0;
    }
}

fn touch_id() -> egui::Id {
    egui::Id::new("malasdoh-touch")
}

/// Use the touch-screen sizes: larger buttons, rows and scroll bars, and
/// on-screen keys over the terminal.
pub fn set_touch(ctx: &egui::Context, touch: bool) {
    ctx.data_mut(|d| d.insert_temp(touch_id(), touch));
    ctx.all_styles_mut(|s| spacing(s, touch));
}

/// Whether [`set_touch`] asked for touch-screen sizes.
pub fn is_touch(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp(touch_id())).unwrap_or(false)
}

/// The main button of a dialog (Save, Connect…): in the accent colour.
pub fn primary(ui: &egui::Ui, text: impl Into<String>) -> egui::Button<'static> {
    let s = sem(ui.ctx());
    egui::Button::new(egui::RichText::new(text.into()).color(s.on_accent)).fill(s.accent)
}

/// A button that destroys something (Delete…): in the theme's red.
pub fn danger(ui: &egui::Ui, text: impl Into<String>) -> egui::Button<'static> {
    let s = sem(ui.ctx());
    egui::Button::new(egui::RichText::new(text.into()).color(s.on_danger)).fill(s.danger)
}

/// Selection, links and the focus ring in the accent colour, with selected
/// text that stays readable on it.
fn set_accent(v: &mut Visuals, accent: Color32) {
    let bg = mix(v.panel_fill, accent, if v.dark_mode { 0.55 } else { 0.4 });
    v.selection.bg_fill = bg;
    let on = if luma(bg) > 0.55 { mix(accent, Color32::BLACK, 0.75) } else { mix(accent, Color32::WHITE, 0.8) };
    v.selection.stroke = Stroke::new(1.0, on);
    v.hyperlink_color = if v.dark_mode { mix(accent, Color32::WHITE, 0.2) } else { mix(accent, Color32::BLACK, 0.2) };
}

/// Colours that mean something (added, removed, failed, root…), from the
/// theme's terminal palette so they suit it, and syntax colours for the
/// editor.
#[derive(Clone, Debug, PartialEq)]
pub struct Semantic {
    pub dark: bool,
    pub good: Color32,
    pub warn: Color32,
    pub bad: Color32,
    /// Changed lines, information.
    pub info: Color32,
    /// Behind added and removed lines of a diff.
    pub add_bg: Color32,
    pub del_bg: Color32,
    /// Behind "danger" labels (browsing as root, typing into several
    /// terminals), with `on_danger` text.
    pub danger: Color32,
    pub on_danger: Color32,
    /// The main button of a dialog, with `on_accent` text.
    pub accent: Color32,
    pub on_accent: Color32,
    pub syntax: Syntax,
    /// Changes whenever any colour above does (for caches).
    pub stamp: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Syntax {
    pub comment: Color32,
    pub string: Color32,
    pub number: Color32,
    pub key: Color32,
    pub section: Color32,
    pub variable: Color32,
    pub keyword: Color32,
    pub literal: Color32,
    /// Behind search matches, and behind the current one.
    pub find: Color32,
    pub find_current: Color32,
}

impl Semantic {
    pub fn new(theme: &AppTheme, v: &Visuals) -> Self {
        let a = &theme.term.ansi;
        let fg = theme.term.fg;
        // The terminal's own colours can be hard to read on the window's
        // background: pull them towards the text colour where needed.
        let bg = v.panel_fill;
        let readable = |c: Color32| {
            let gap = (luma(c) - luma(bg)).abs();
            if gap >= 0.3 { c } else { mix(c, v.text_color(), (0.3 - gap) * 2.0) }
        };
        let good = readable(a[2]);
        let bad = readable(a[1]);
        let info = readable(a[4]);
        let warn = readable(a[3]);
        let alpha = |c: Color32, al: u8| Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), al);
        let (add_al, del_al) = if theme.dark { (60, 60) } else { (55, 45) };
        // Recognisably green and red whatever the theme's own shades are.
        let green = mix(a[2], c(0x2ea043), 0.5);
        let red = mix(a[1], c(0xf85149), 0.5);
        // A light theme's selection colour is too pale for a button: its
        // link colour is the strong version of the same accent.
        let accent = if theme.dark { v.selection.bg_fill } else { v.hyperlink_color };
        let on_accent = if luma(accent) > 0.55 { Color32::BLACK } else { Color32::WHITE };
        let danger = mix(a[1], Color32::BLACK, if theme.dark { 0.45 } else { 0.25 });
        let syntax = Syntax {
            comment: readable(mix(a[8], fg, 0.3)),
            string: readable(a[2]),
            number: readable(mix(a[1], a[3], 0.5)),
            key: readable(a[4]),
            section: readable(a[3]),
            variable: readable(a[5]),
            keyword: readable(a[1]),
            literal: readable(a[6]),
            find: mix(bg, a[3], if theme.dark { 0.3 } else { 0.35 }),
            find_current: mix(bg, a[3], if theme.dark { 0.6 } else { 0.7 }),
        };
        let mut s = Self {
            dark: theme.dark,
            good,
            warn,
            bad,
            info,
            add_bg: alpha(green, add_al),
            del_bg: alpha(red, del_al),
            danger,
            on_danger: Color32::WHITE,
            accent,
            on_accent,
            syntax,
            stamp: 0,
        };
        s.stamp = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            format!("{s:?}{:?}{:?}", v.panel_fill, v.text_color()).hash(&mut h);
            h.finish()
        };
        s
    }
}

fn sem_id(dark: bool) -> Id {
    Id::new(("malasdoh-semantic", dark))
}

fn term_id(dark: bool) -> Id {
    Id::new(("malasdoh-terminal-theme", dark))
}

/// The meaningful colours of the theme shown now.
pub fn sem(ctx: &egui::Context) -> Semantic {
    let dark = ctx.global_style().visuals.dark_mode;
    ctx.data(|d| d.get_temp::<Semantic>(sem_id(dark))).unwrap_or_else(|| {
        let t = &builtin()[if dark { 0 } else { 1 }];
        Semantic::new(t, &visuals(t, None))
    })
}

/// The terminal colours to use now.
pub fn terminal_theme(ctx: &egui::Context) -> &'static Theme {
    let dark = ctx.global_style().visuals.dark_mode;
    ctx.data(|d| d.get_temp::<&'static Theme>(term_id(dark))).unwrap_or(&THEMES[0])
}

/// What the settings ask for, ready to apply.
pub struct Choice<'a> {
    /// A theme name, or [`FOLLOW_SYSTEM`].
    pub theme: &'a str,
    pub system_dark: &'a str,
    pub system_light: &'a str,
    /// "#rrggbb", or empty for the theme's own.
    pub accent: &'a str,
    /// A terminal theme name, or [`SAME_AS_APP`].
    pub terminal: &'a str,
}

/// Colour the app as `choice` says, from `themes` (built-in and your own).
pub fn apply(ctx: &egui::Context, themes: &[AppTheme], choice: &Choice) {
    let find = |name: &str, dark: bool| {
        themes
            .iter()
            .find(|t| t.name == name)
            .or_else(|| themes.iter().find(|t| t.dark == dark))
            .cloned()
            .unwrap_or_else(|| builtin().remove(if dark { 0 } else { 1 }))
    };
    let accent = parse_color(choice.accent);
    let (dark, light, pref) = if choice.theme == FOLLOW_SYSTEM {
        (find(choice.system_dark, true), find(choice.system_light, false), egui::ThemePreference::System)
    } else {
        let t = find(choice.theme, true);
        let pref = if t.dark { egui::ThemePreference::Dark } else { egui::ThemePreference::Light };
        (t.clone(), t, pref)
    };
    for (mode, t) in [(egui::Theme::Dark, &dark), (egui::Theme::Light, &light)] {
        let mut v = visuals(t, accent);
        // egui keeps one look per mode: a light theme chosen for "dark"
        // still has to say it is light, and the other way round.
        v.dark_mode = t.dark;
        let s = Semantic::new(t, &v);
        let term = if choice.terminal == SAME_AS_APP {
            t.term
        } else {
            terminal_themes(themes).into_iter().find(|x| x.name == choice.terminal).unwrap_or(t.term)
        };
        ctx.set_visuals_of(mode, v);
        let key = mode == egui::Theme::Dark;
        ctx.data_mut(|d| {
            d.insert_temp(sem_id(key), s.clone());
            d.insert_temp(term_id(key), term);
        });
        // A light theme shown in the "dark" slot reports dark_mode false,
        // so look its colours up under that too.
        if t.dark != key {
            ctx.data_mut(|d| {
                d.insert_temp(sem_id(t.dark), s);
                d.insert_temp(term_id(t.dark), term);
            });
        }
    }
    ctx.set_theme(pref);
    let touch = is_touch(ctx);
    ctx.all_styles_mut(|s| spacing(s, touch));
}

/// Every terminal palette to choose from: the built-in ones, then those of
/// your own themes.
pub fn terminal_themes(themes: &[AppTheme]) -> Vec<&'static Theme> {
    let mut v: Vec<&'static Theme> = THEMES.iter().collect();
    for t in themes.iter().filter(|t| t.file.is_some()) {
        if !v.iter().any(|x| x.name == t.term.name) {
            v.push(t.term);
        }
    }
    v
}

/// "#rrggbb" (or "rrggbb") as a colour.
pub fn parse_color(s: &str) -> Option<Color32> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(c(v))
}

pub fn hex(c: Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

// ---------------------------------------------------------------- files

/// A theme file. Every colour is optional: what is missing comes from
/// `based_on` (or from "Dark" or "Light").
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeFile {
    pub name: String,
    pub dark: Option<bool>,
    pub based_on: Option<String>,
    pub ui: UiFile,
    pub terminal: TerminalFile,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiFile {
    pub panel: Option<String>,
    pub window: Option<String>,
    pub field: Option<String>,
    pub text: Option<String>,
    pub accent: Option<String>,
    pub border: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalFile {
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub cursor: Option<String>,
    pub selection: Option<String>,
    /// The 16 ANSI colours: black, red, green, yellow, blue, magenta, cyan,
    /// white, then their bright versions.
    pub ansi: Option<Vec<String>>,
}

/// The window colours egui uses for its own dark or light look, so a theme
/// based on "Dark" or "Light" can be written out in full.
fn ui_of(theme: &AppTheme) -> UiColors {
    theme.ui.unwrap_or_else(|| {
        let v = if theme.dark { Visuals::dark() } else { Visuals::light() };
        UiColors {
            panel: v.panel_fill,
            window: v.window_fill,
            field: v.extreme_bg_color,
            text: v.widgets.noninteractive.fg_stroke.color,
            accent: v.selection.bg_fill,
            border: v.widgets.noninteractive.bg_stroke.color,
        }
    })
}

/// A theme read from a file, on top of `builtins`.
pub fn from_file(text: &str, builtins: &[AppTheme]) -> Result<AppTheme, String> {
    let f: ThemeFile = toml::from_str(text).map_err(|e| e.message().to_string())?;
    if f.name.trim().is_empty() {
        return Err("it has no name (name = \"…\")".into());
    }
    if builtins.iter().any(|t| t.name == f.name.trim()) || f.name.trim() == FOLLOW_SYSTEM {
        return Err(format!("\"{}\" is a built-in theme's name; give it another", f.name.trim()));
    }
    let base = match &f.based_on {
        Some(b) => builtins.iter().find(|t| &t.name == b).ok_or_else(|| format!("no built-in theme \"{b}\""))?,
        None => &builtins[if f.dark == Some(false) { 1 } else { 0 }],
    };
    let color = |field: &str, v: &Option<String>, dflt: Color32| -> Result<Color32, String> {
        match v {
            None => Ok(dflt),
            Some(s) => parse_color(s).ok_or_else(|| format!("{field} = \"{s}\" is not a colour like \"#1d1f21\"")),
        }
    };
    let b = ui_of(base);
    let ui = UiColors {
        panel: color("panel", &f.ui.panel, b.panel)?,
        window: color("window", &f.ui.window, b.window)?,
        field: color("field", &f.ui.field, b.field)?,
        text: color("text", &f.ui.text, b.text)?,
        accent: color("accent", &f.ui.accent, b.accent)?,
        border: color("border", &f.ui.border, b.border)?,
    };
    let bt = base.term;
    let mut ansi = bt.ansi;
    if let Some(list) = &f.terminal.ansi {
        if list.len() != 16 {
            return Err(format!("terminal ansi has {} colours; it needs 16", list.len()));
        }
        for (i, s) in list.iter().enumerate() {
            ansi[i] = color(&format!("ansi {}", i + 1), &Some(s.clone()), bt.ansi[i])?;
        }
    }
    let name = f.name.trim().to_string();
    let term = Theme {
        // Kept for the rest of the run, like the built-in palettes.
        name: Box::leak(name.clone().into_boxed_str()),
        bg: color("background", &f.terminal.background, bt.bg)?,
        fg: color("foreground", &f.terminal.foreground, bt.fg)?,
        cursor: color("cursor", &f.terminal.cursor, bt.cursor)?,
        selection: color("selection", &f.terminal.selection, bt.selection)?,
        ansi,
    };
    Ok(AppTheme { name, dark: f.dark.unwrap_or(base.dark), ui: Some(ui), term: Box::leak(Box::new(term)), file: None })
}

/// `theme` written out in full, as a starting point for your own.
pub fn to_file(theme: &AppTheme, name: &str) -> String {
    let u = ui_of(theme);
    let t = theme.term;
    let f = ThemeFile {
        name: name.to_string(),
        dark: Some(theme.dark),
        based_on: None,
        ui: UiFile {
            panel: Some(hex(u.panel)),
            window: Some(hex(u.window)),
            field: Some(hex(u.field)),
            text: Some(hex(u.text)),
            accent: Some(hex(u.accent)),
            border: Some(hex(u.border)),
        },
        terminal: TerminalFile {
            background: Some(hex(t.bg)),
            foreground: Some(hex(t.fg)),
            cursor: Some(hex(t.cursor)),
            selection: Some(hex(t.selection)),
            ansi: Some(t.ansi.iter().map(|c| hex(*c)).collect()),
        },
    };
    format!(
        "# A MalasDoh theme. Change the colours, save, then press Reload in\n\
         # Settings > Appearance. Colours you remove come from \"Dark\" (or\n\
         # \"Light\" with dark = false), or from based_on = \"<a built-in theme>\".\n\n{}",
        toml::to_string(&f).unwrap_or_default()
    )
}

/// The built-in themes, then every `*.toml` in `dir` that reads as a theme,
/// and what was wrong with those that did not.
pub fn load_all(dir: &Path) -> (Vec<AppTheme>, Vec<String>) {
    let mut themes = builtin();
    let mut errors = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return (themes, errors) };
    let mut files: Vec<PathBuf> =
        rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect();
    files.sort();
    let builtins = builtin();
    for p in files {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match std::fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|t| from_file(&t, &builtins)) {
            Ok(mut t) if !themes.iter().any(|x| x.name == t.name) => {
                t.file = Some(p);
                themes.push(t);
            }
            Ok(t) => errors.push(format!("{name}: another theme is already called \"{}\"", t.name)),
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    (themes, errors)
}

/// A file name in `dir` for a new theme called `name`, not yet taken.
pub fn new_file_path(dir: &Path, name: &str) -> PathBuf {
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = if slug.is_empty() { "theme".to_string() } else { slug };
    let mut p = dir.join(format!("{slug}.toml"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{slug}-{n}.toml"));
        n += 1;
    }
    p
}

/// Show `dir` in the system's file manager.
pub fn open_folder(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(windows)]
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(dir).spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_have_unique_names_and_readable_selection() {
        let all = builtin();
        for (i, t) in all.iter().enumerate() {
            assert!(!all[..i].iter().any(|x| x.name == t.name), "{}", t.name);
            let v = visuals(t, None);
            let gap = (luma(v.selection.bg_fill) - luma(v.selection.stroke.color)).abs();
            assert!(gap > 0.35, "{}: selected text {gap}", t.name);
            let text = (luma(v.panel_fill) - luma(v.text_color())).abs();
            assert!(text > 0.35, "{}: text on panel {text}", t.name);
        }
        // An accent of your own replaces the theme's.
        let v = visuals(&all[2], Some(c(0xff0000)));
        assert_eq!(v.hyperlink_color, mix(c(0xff0000), Color32::WHITE, 0.2));
    }

    #[test]
    fn meaningful_colours_stand_out() {
        for t in builtin() {
            let v = visuals(&t, None);
            let s = Semantic::new(&t, &v);
            for (what, c) in [("good", s.good), ("bad", s.bad), ("warn", s.warn), ("keyword", s.syntax.keyword)] {
                let gap = (luma(c) - luma(v.panel_fill)).abs();
                assert!(gap >= 0.25, "{} {what}: {gap}", t.name);
            }
        }
    }

    #[test]
    fn theme_files_round_trip_and_fill_gaps() {
        let builtins = builtin();
        let dracula = builtins.iter().find(|t| t.name == "Dracula").unwrap();
        let text = to_file(dracula, "My Dracula");
        let t = from_file(&text, &builtins).unwrap();
        assert_eq!((t.name.as_str(), t.dark), ("My Dracula", true));
        assert_eq!(t.ui, dracula.ui);
        assert_eq!(t.term.ansi, dracula.term.ansi);
        // Only a few colours: the rest from the theme it is based on.
        let t = from_file("name = \"Green\"\nbased_on = \"Solarized Light\"\n[ui]\naccent = \"#2aa198\"\n", &builtins)
            .unwrap();
        assert!(!t.dark);
        assert_eq!(t.ui.unwrap().accent, c(0x2aa198));
        assert_eq!(t.ui.unwrap().panel, c(0xfdf6e3));
        // Mistakes say what is wrong.
        let err = |s: &str| from_file(s, &builtins).unwrap_err();
        assert!(err("name = \"X\"\n[ui]\ntext = \"red\"").contains("text = \"red\""));
        assert!(err("name = \"Dark\"").contains("built-in"));
        assert!(err("[ui]\ntext = \"#ffffff\"").contains("no name"));
        assert!(err("name = \"X\"\ncolour = 1").contains("colour"));
        assert!(err("name = \"X\"\n[terminal]\nansi = [\"#000000\"]").contains("16"));
    }

    #[test]
    fn theme_folder_loads_good_files_and_reports_bad_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.toml"), "name = \"Mine\"\n[ui]\naccent = \"#ff8800\"\n").unwrap();
        std::fs::write(dir.path().join("b.toml"), "name = \"Mine\"\n").unwrap();
        std::fs::write(dir.path().join("c.toml"), "not toml [").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        let (themes, errors) = load_all(dir.path());
        assert_eq!(themes.len(), builtin().len() + 1);
        assert_eq!(themes.last().unwrap().name, "Mine");
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].starts_with("b.toml"));
        assert_eq!(new_file_path(dir.path(), "Mine!").file_name().unwrap(), "mine.toml");
        std::fs::write(dir.path().join("mine.toml"), "").unwrap();
        assert_eq!(new_file_path(dir.path(), "Mine!").file_name().unwrap(), "mine-2.toml");
        assert!(terminal_themes(&themes).iter().any(|t| t.name == "Mine"));
    }
}
