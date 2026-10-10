//! Compare two files: the same config on two servers, a server file and one
//! on this computer, or a server file and one of its backups.

use std::path::PathBuf;

use egui::{Color32, RichText};
use malasdoh_core::diff::{self, Diff, DiffLine, LineKind};
use malasdoh_core::edit::{self, MAX_EDIT_SIZE, RemoteIo};
use malasdoh_core::{sftp, text};

use crate::bridge::{EventKind, SessionId};
use crate::session::{ConnState, Session, Shared};

/// One side of a comparison.
#[derive(Clone, Debug, PartialEq)]
pub enum Side {
    Remote { session: SessionId, server: String, path: String },
    Local(PathBuf),
}

impl Side {
    pub fn label(&self) -> String {
        match self {
            Side::Remote { server, path, .. } => format!("{server}:{path}"),
            Side::Local(p) => p.display().to_string(),
        }
    }

    /// For headings: a local path is cut to its last two parts.
    pub fn short_label(&self) -> String {
        match self {
            Side::Remote { .. } => self.label(),
            Side::Local(p) => {
                let parts: Vec<_> = p.components().collect();
                if parts.len() <= 3 {
                    return p.display().to_string();
                }
                let tail: PathBuf = parts[parts.len() - 2..].iter().collect();
                format!("this computer: …{}{}", std::path::MAIN_SEPARATOR, tail.display())
            }
        }
    }

    pub fn remote(s: &Session, path: &str) -> Self {
        Side::Remote { session: s.id, server: s.profile.display_name(), path: path.to_string() }
    }
}

/// What a session asks the app to compare.
pub enum CompareRequest {
    /// Compare this remote file with something the user picks.
    Pick {
        path: String,
    },
    Pair(Side, Side),
}

pub enum CompareMsg {
    Loaded { id: u64, result: Result<(Loaded, Loaded), String> },
}

pub struct Loaded {
    text: String,
    bytes: Vec<u8>,
}

/// Where to read a side from, resolved on the UI thread.
enum Source {
    Remote { sftp: RemoteIo, sudo: Option<RemoteIo>, path: String },
    Local(PathBuf),
}

impl Source {
    fn resolve(side: &Side, sessions: &[Session]) -> Result<Self, String> {
        match side {
            Side::Local(p) => Ok(Source::Local(p.clone())),
            Side::Remote { session, server, path } => {
                let s = sessions
                    .iter()
                    .find(|s| s.id == *session && s.state == ConnState::Connected)
                    .ok_or_else(|| format!("{server} is not connected"))?;
                let (sftp, sudo) = s.readers().ok_or_else(|| format!("{server} is not connected"))?;
                Ok(Source::Remote { sftp, sudo, path: path.clone() })
            }
        }
    }

    async fn read(self, label: String) -> anyhow::Result<Loaded> {
        let bytes = match self {
            Source::Local(p) => {
                let meta = tokio::fs::metadata(&p).await?;
                if meta.len() > MAX_EDIT_SIZE {
                    anyhow::bail!("{label} is larger than {}", sftp::human_size(MAX_EDIT_SIZE));
                }
                tokio::fs::read(&p).await?
            }
            Source::Remote { sftp, sudo, path } => {
                let first = sftp.read_bytes(&path).await;
                let got = match (first, sudo) {
                    // Readable only by root: use sudo if this session has it.
                    (Err(e), Some(sudo)) if format!("{e:#}").to_lowercase().contains("permission denied") => {
                        sudo.read_bytes(&path).await?
                    }
                    (r, _) => r?,
                };
                got.ok_or_else(|| anyhow::anyhow!("{label} does not exist"))?
            }
        };
        let text = match text::decode(&bytes) {
            Ok((t, _)) => t,
            Err(_) => anyhow::bail!("{label} is not a text file"),
        };
        Ok(Loaded { text, bytes })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Context {
    Three,
    Ten,
    Whole,
}

impl Context {
    fn lines(self, a: &str, b: &str) -> usize {
        match self {
            Context::Three => 3,
            Context::Ten => 10,
            Context::Whole => a.lines().count().max(b.lines().count()) + 1,
        }
    }
}

enum Row {
    Gap,
    Line(DiffLine),
}

enum State {
    Loading,
    Failed(String),
    Ready { left: Loaded, right: Loaded, diff: Diff, rows: Vec<Row> },
}

struct Window {
    id: u64,
    left: Side,
    right: Side,
    state: State,
    context: Context,
    open: bool,
}

impl Window {
    fn rebuild(&mut self) {
        let context = self.context;
        if let State::Ready { left, right, diff, rows } = &mut self.state {
            *diff = diff::diff(&left.text, &right.text, context.lines(&left.text, &right.text));
            *rows = Vec::new();
            for (i, h) in diff.hunks.iter().enumerate() {
                if i > 0 {
                    rows.push(Row::Gap);
                }
                rows.extend(h.lines.iter().cloned().map(Row::Line));
            }
        }
    }
}

/// "Compare with…": the user picks the other side.
struct Pick {
    left: Side,
    session: SessionId,
    path: String,
    mode: Mode,
    other: Option<SessionId>,
    other_path: String,
    local: String,
    backups: Vec<PathBuf>,
    backup: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Server,
    Local,
    Backup,
}

#[derive(Default)]
pub struct Compares {
    windows: Vec<Window>,
    next: u64,
    pick: Option<Pick>,
}

impl Compares {
    pub fn request(&mut self, req: CompareRequest, from: SessionId, sessions: &[Session], shared: &Shared) {
        match req {
            CompareRequest::Pair(a, b) => self.open(a, b, sessions, shared),
            CompareRequest::Pick { path } => {
                let Some(s) = sessions.iter().find(|s| s.id == from) else { return };
                let root = edit::host_backup_dir(&shared.paths.backups_dir(), &s.profile.host, s.profile.port);
                let backups = edit::list_local_backups(&root, &path);
                let other = sessions.iter().find(|o| o.id != from && o.state == ConnState::Connected).map(|o| o.id);
                // Prefer what is selected in the local pane, else the same
                // name in its folder.
                let local = s
                    .local
                    .as_ref()
                    .map(|l| match l.selection() {
                        [one] if one.is_file() => one.clone(),
                        _ => l.path.join(sftp::file_name(&path)),
                    })
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let mode = if other.is_some() {
                    Mode::Server
                } else if !local.is_empty() && PathBuf::from(&local).is_file() {
                    Mode::Local
                } else if !backups.is_empty() {
                    Mode::Backup
                } else {
                    Mode::Local
                };
                self.pick = Some(Pick {
                    left: Side::remote(s, &path),
                    session: from,
                    other_path: path.clone(),
                    path,
                    mode,
                    other,
                    local,
                    backups,
                    backup: 0,
                });
            }
        }
    }

    pub fn open(&mut self, left: Side, right: Side, sessions: &[Session], shared: &Shared) {
        self.next += 1;
        let id = self.next;
        let mut w = Window { id, left, right, state: State::Loading, context: Context::Three, open: true };
        Self::load(&mut w, sessions, shared);
        self.windows.push(w);
    }

    fn load(w: &mut Window, sessions: &[Session], shared: &Shared) {
        w.state = State::Loading;
        let sources = Source::resolve(&w.left, sessions).and_then(|a| Ok((a, Source::resolve(&w.right, sessions)?)));
        let (a, b) = match sources {
            Ok(s) => s,
            Err(e) => {
                w.state = State::Failed(e);
                return;
            }
        };
        let (la, lb) = (w.left.label(), w.right.label());
        let (events, id) = (shared.events.clone(), w.id);
        shared.rt.spawn(async move {
            let result =
                async { anyhow::Ok((a.read(la).await?, b.read(lb).await?)) }.await.map_err(|e| format!("{e:#}"));
            events.send(0, EventKind::Compare(CompareMsg::Loaded { id, result }));
        });
    }

    pub fn handle(&mut self, msg: CompareMsg) {
        match msg {
            CompareMsg::Loaded { id, result } => {
                let Some(w) = self.windows.iter_mut().find(|w| w.id == id) else { return };
                match result {
                    Ok((left, right)) => {
                        w.state = State::Ready { left, right, diff: Diff::default(), rows: Vec::new() };
                        w.rebuild();
                    }
                    Err(e) => w.state = State::Failed(e),
                }
            }
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, sessions: &[Session], shared: &Shared) {
        self.pick_ui(ctx, sessions, shared);
        let mut reload = None;
        for w in &mut self.windows {
            let mut open = w.open;
            // Named after the server's file when there is one.
            let named = if matches!(w.left, Side::Remote { .. }) || matches!(w.right, Side::Local(_)) {
                &w.left
            } else {
                &w.right
            };
            egui::Window::new(format!("Compare: {}", sftp::file_name(&side_path(named))))
                .id(egui::Id::new(("compare", w.id)))
                .open(&mut open)
                .default_size([900.0, 560.0])
                .resizable(true)
                .show(ctx, |ui| {
                    if window_ui(ui, w) {
                        reload = Some(w.id);
                    }
                });
            w.open = open;
        }
        if let Some(id) = reload
            && let Some(w) = self.windows.iter_mut().find(|w| w.id == id)
        {
            Self::load(w, sessions, shared);
        }
        self.windows.retain(|w| w.open);
    }

    fn pick_ui(&mut self, ctx: &egui::Context, sessions: &[Session], shared: &Shared) {
        let Some(p) = &mut self.pick else { return };
        let servers: Vec<(SessionId, String)> = sessions
            .iter()
            .filter(|s| s.state == ConnState::Connected)
            .map(|s| (s.id, s.profile.display_name()))
            .collect();
        let mut result = None;
        egui::Modal::new("compare-pick".into()).show(ctx, |ui| {
            ui.set_min_width(520.0);
            ui.heading("Compare with…");
            ui.monospace(p.left.label());
            ui.add_space(6.0);

            ui.radio_value(&mut p.mode, Mode::Server, "A file on a server");
            ui.add_enabled_ui(p.mode == Mode::Server, |ui| {
                ui.indent("server", |ui| {
                    let name = p
                        .other
                        .and_then(|id| servers.iter().find(|s| s.0 == id))
                        .map(|s| s.1.clone())
                        .unwrap_or_else(|| "Choose a connection".into());
                    egui::ComboBox::from_id_salt("compare-server").selected_text(name).show_ui(ui, |ui| {
                        for (id, name) in &servers {
                            let label = if *id == p.session { format!("{name} (this one)") } else { name.clone() };
                            ui.selectable_value(&mut p.other, Some(*id), label);
                        }
                    });
                    ui.add(egui::TextEdit::singleline(&mut p.other_path).desired_width(f32::INFINITY));
                    if servers.len() < 2 {
                        ui.label(
                            RichText::new("Connect to the other server in another tab to compare across servers.")
                                .weak()
                                .small(),
                        );
                    }
                });
            });

            ui.radio_value(&mut p.mode, Mode::Local, "A file on this computer");
            ui.add_enabled_ui(p.mode == Mode::Local, |ui| {
                ui.indent("local", |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut p.local).desired_width(400.0));
                        if ui.button("Browse…").clicked()
                            && let Some(f) = rfd::FileDialog::new().set_title("Compare with").pick_file()
                        {
                            p.local = f.display().to_string();
                        }
                    });
                });
            });

            ui.radio_value(&mut p.mode, Mode::Backup, "A local backup of it");
            ui.add_enabled_ui(p.mode == Mode::Backup, |ui| {
                ui.indent("backup", |ui| {
                    if p.backups.is_empty() {
                        ui.label(RichText::new("None yet: a backup is made each time you save this file here.").weak());
                    }
                    egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                        for (i, b) in p.backups.iter().enumerate() {
                            let name = b.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            ui.radio_value(&mut p.backup, i, RichText::new(name).monospace());
                        }
                    });
                });
            });

            ui.add_space(6.0);
            let right = match p.mode {
                Mode::Server => p.other.and_then(|id| {
                    let path = p.other_path.trim();
                    let name = servers.iter().find(|s| s.0 == id)?.1.clone();
                    (!path.is_empty() && !(id == p.session && path == p.path)).then(|| Side::Remote {
                        session: id,
                        server: name,
                        path: path.to_string(),
                    })
                }),
                Mode::Local => {
                    let l = malasdoh_core::paths::expand_tilde(p.local.trim());
                    (!p.local.trim().is_empty()).then_some(Side::Local(l))
                }
                Mode::Backup => p.backups.get(p.backup).cloned().map(Side::Local),
            };
            ui.horizontal(|ui| {
                if ui.add_enabled(right.is_some(), egui::Button::new("Compare")).clicked() {
                    result = Some(right.clone());
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    result = Some(None);
                }
            });
        });
        match result {
            Some(Some(right)) => {
                let p = self.pick.take().unwrap();
                // Older on the left: a backup or the other copy is "before".
                let (a, b) = if p.mode == Mode::Backup { (right, p.left) } else { (p.left, right) };
                self.open(a, b, sessions, shared);
            }
            Some(None) => self.pick = None,
            None => {}
        }
    }
}

fn side_path(s: &Side) -> String {
    match s {
        Side::Remote { path, .. } => path.clone(),
        Side::Local(p) => p.display().to_string(),
    }
}

/// Returns true when the user asked to reload both sides.
fn window_ui(ui: &mut egui::Ui, w: &mut Window) -> bool {
    let mut reload = false;
    let remove = Color32::from_rgb(220, 90, 90);
    let add = Color32::from_rgb(90, 190, 90);
    egui::Grid::new(("compare-sides", w.id)).num_columns(2).show(ui, |ui| {
        for (sign, color, side) in [("-", remove, &w.left), ("+", add, &w.right)] {
            ui.label(RichText::new(sign).color(color).strong());
            ui.add(egui::Label::new(RichText::new(side.short_label()).monospace()).truncate())
                .on_hover_text(side.label());
            ui.end_row();
        }
    });
    let mut swap = false;
    let mut rebuild = false;
    ui.horizontal(|ui| {
        if ui.button("Swap sides").clicked() {
            swap = true;
        }
        if ui.button("⟳ Reload").on_hover_text("Read both files again").clicked() {
            reload = true;
        }
        ui.label("Context:");
        for (c, label) in [(Context::Three, "3 lines"), (Context::Ten, "10 lines"), (Context::Whole, "Whole file")] {
            if ui.selectable_label(w.context == c, label).clicked() && w.context != c {
                w.context = c;
                rebuild = true;
            }
        }
        if let State::Ready { left, right, diff, .. } = &w.state
            && !diff.is_empty()
        {
            ui.label(RichText::new(format!("+{}", diff.added)).color(add));
            ui.label(RichText::new(format!("-{}", diff.removed)).color(remove));
            if ui.button("Copy as patch").clicked() {
                ui.ctx().copy_text(diff::unified_labeled(&left.text, &right.text, &w.left.label(), &w.right.label()));
            }
        }
    });
    if swap {
        std::mem::swap(&mut w.left, &mut w.right);
        if let State::Ready { left, right, .. } = &mut w.state {
            std::mem::swap(left, right);
        }
        rebuild = true;
    }
    if rebuild {
        w.rebuild();
    }
    ui.separator();
    match &w.state {
        State::Loading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading both files…");
            });
        }
        State::Failed(e) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        State::Ready { left, right, diff, rows } => {
            if diff.is_empty() {
                if left.bytes == right.bytes {
                    ui.label("The files are identical.");
                } else {
                    ui.label("The text is the same; the files differ only in line endings, a byte-order mark or the final newline.");
                }
            } else {
                rows_ui(ui, rows, w.id);
            }
        }
    }
    reload
}

fn rows_ui(ui: &mut egui::Ui, rows: &[Row], id: u64) {
    let add_bg = Color32::from_rgba_unmultiplied(60, 160, 60, 60);
    let del_bg = Color32::from_rgba_unmultiplied(200, 60, 60, 60);
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 2.0;
    let weak = ui.visuals().weak_text_color();
    egui::ScrollArea::both().id_salt(("compare-rows", id)).auto_shrink([false, false]).show_rows(
        ui,
        row_h,
        rows.len(),
        |ui, range| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            ui.spacing_mut().item_spacing.y = 2.0;
            for row in &rows[range] {
                match row {
                    Row::Gap => {
                        ui.label(RichText::new("    ···").monospace().color(weak));
                    }
                    Row::Line(l) => {
                        let (sign, bg) = match l.kind {
                            LineKind::Context => (' ', Color32::TRANSPARENT),
                            LineKind::Added => ('+', add_bg),
                            LineKind::Removed => ('-', del_bg),
                        };
                        let num = |n: Option<usize>| n.map(|n| format!("{n:>5}")).unwrap_or_else(|| "     ".into());
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{} {}", num(l.old_no), num(l.new_no))).monospace().color(weak),
                            );
                            ui.label(RichText::new(format!("{sign} {}", l.text)).monospace().background_color(bg));
                        });
                    }
                }
            }
        },
    );
}
