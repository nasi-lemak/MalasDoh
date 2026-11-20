//! The system's file dialogs. On the desktop they are rfd's; elsewhere
//! (HarmonyOS, for now) this stand-in answers as if the user cancelled,
//! until the platform's own picker is wired in.

#[cfg(feature = "desktop")]
pub use rfd::FileDialog;

#[cfg(not(feature = "desktop"))]
pub use stand_in::FileDialog;

#[cfg(not(feature = "desktop"))]
mod stand_in {
    use std::path::PathBuf;

    /// The same calls as `crate::dialogs::FileDialog`; every pick comes back empty.
    #[derive(Default)]
    pub struct FileDialog;

    impl FileDialog {
        pub fn new() -> Self {
            Self
        }
        pub fn add_filter(self, _name: impl Into<String>, _extensions: &[impl ToString]) -> Self {
            self
        }
        pub fn set_title(self, _title: impl Into<String>) -> Self {
            self
        }
        pub fn set_file_name(self, _name: impl Into<String>) -> Self {
            self
        }
        pub fn pick_file(self) -> Option<PathBuf> {
            None
        }
        pub fn pick_files(self) -> Option<Vec<PathBuf>> {
            None
        }
        pub fn pick_folder(self) -> Option<PathBuf> {
            None
        }
        pub fn save_file(self) -> Option<PathBuf> {
            None
        }
    }
}
