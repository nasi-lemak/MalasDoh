//! The engine behind MalasDoh: saved connections, SSH sessions, SFTP and safe
//! remote editing. Nothing in here knows about the GUI; the app drives it through
//! async calls and answers its questions (host keys, passwords) via [`Prompter`].

pub mod commands;
pub mod diff;
pub mod drafts;
pub mod edit;
pub mod export;
pub mod forward;
pub mod import;
pub mod journal;
pub mod keys;
pub mod known_hosts;
pub mod patch;
pub mod paths;
pub mod pgconf;
pub mod profile;
pub mod proxy;
pub mod replace;
pub mod secrets;
pub mod sftp;
pub mod shellfs;
pub mod ssh;
pub mod ssh_config;
pub mod sync;
pub mod teleport;
pub mod teleport_api;
pub mod text;
pub mod transfer;
pub mod validate;
pub mod wireguard;

pub use paths::AppPaths;
pub use profile::{AuthMethod, Profile, ProfileId, ProfileStore};
pub use ssh::{Connection, Prompter};
