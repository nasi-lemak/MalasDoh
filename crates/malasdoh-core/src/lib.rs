//! The engine behind MalasDoh: saved connections, SSH sessions, SFTP and safe
//! remote editing. Nothing in here knows about the GUI; the app drives it through
//! async calls and answers its questions (host keys, passwords) via [`Prompter`].

pub mod commands;
pub mod diff;
pub mod edit;
pub mod export;
pub mod forward;
pub mod import;
pub mod journal;
pub mod keys;
pub mod known_hosts;
pub mod paths;
pub mod profile;
pub mod proxy;
pub mod secrets;
pub mod sftp;
pub mod shellfs;
pub mod ssh;
pub mod ssh_config;
pub mod sync;
pub mod text;
pub mod validate;

pub use paths::AppPaths;
pub use profile::{AuthMethod, Profile, ProfileId, ProfileStore};
pub use ssh::{Connection, Prompter};
