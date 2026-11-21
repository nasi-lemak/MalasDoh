//! MalasDoh's whole app, as a library: the desktop program (`main.rs`, with
//! eframe) and the Android, iOS and HarmonyOS glue (`crates/malasdoh-*`)
//! each give it a window. Start it with [`app::App::with_context`].

pub mod app;
mod bridge;
mod browser;
pub mod clipboard;
mod compare;
pub mod dialogs;
mod dlg;
mod editor;
mod find_ui;
mod forwards;
mod highlight;
mod journal_ui;
mod keys_ui;
mod listsel;
mod local;
pub mod lock;
mod logview;
mod look;
mod multiedit;
mod multirun;
mod patch_ui;
mod queue;
mod rdp_ui;
mod replace_ui;
mod restore;
mod session;
mod settings;
pub mod share;
mod shortcut;
mod snippets;
mod sudo;
mod sync_ui;
mod teleport_ui;
mod terminal;
mod textops;
mod themes;
mod wireguard_ui;
