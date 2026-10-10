//! The engine behind MalasDoh: saved connections, SSH sessions, SFTP and safe
//! remote editing. Nothing in here knows about the GUI; the app drives it through
//! async calls and answers its questions (host keys, passwords) via [`Prompter`].

pub mod diff;
pub mod edit;
pub mod forward;
pub mod known_hosts;
pub mod paths;
pub mod profile;
pub mod secrets;
pub mod sftp;
pub mod ssh;
pub mod ssh_config;
pub mod text;
pub mod validate;

pub use paths::AppPaths;
pub use profile::{AuthMethod, Profile, ProfileId, ProfileStore};
pub use ssh::{Connection, Prompter};
