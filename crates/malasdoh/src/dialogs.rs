//! The system's file dialogs. On the desktop they are rfd's, and answer at
//! once. On phones and tablets the platform's glue sets a [`Platform`]:
//! picking files opens the system's picker, whose answer comes a moment
//! later; folders and files to save go to the app's own documents folder.
//!
//! So that a later answer reaches the right place, each dialog has an id,
//! and the code that opens it also looks for [`ready`] answers:
//!
//! ```ignore
//! if (ui.button("Browse…").clicked() || dialogs::ready("key-file"))
//!     && let Some(f) = FileDialog::new().id("key-file").pick_file()
//! ```

use std::path::PathBuf;

/// How a phone or tablet picks files.
#[cfg_attr(feature = "desktop", allow(dead_code))]
pub struct Platform {
    /// Open the system's file picker for the dialog with this id; its answer
    /// comes back through [`deliver`]. `multiple`: more than one file.
    pub pick: Box<dyn Fn(&'static str, bool) + Send + Sync>,
    /// Where folders and saved files go: the app's documents folder.
    pub documents: PathBuf,
}

#[cfg(not(feature = "desktop"))]
mod answers {
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    pub static PLATFORM: OnceLock<super::Platform> = OnceLock::new();
    /// Answers not taken yet, by dialog id.
    static ANSWERS: Mutex<Vec<(String, Vec<PathBuf>)>> = Mutex::new(Vec::new());

    pub fn deliver(id: &str, paths: Vec<PathBuf>) {
        let mut all = ANSWERS.lock().unwrap_or_else(|e| e.into_inner());
        all.retain(|(i, _)| i != id);
        all.push((id.to_string(), paths));
    }

    pub fn ready(id: &str) -> bool {
        ANSWERS.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(i, _)| i == id)
    }

    pub fn take(id: &str) -> Option<Vec<PathBuf>> {
        let mut all = ANSWERS.lock().unwrap_or_else(|e| e.into_inner());
        let at = all.iter().position(|(i, _)| i == id)?;
        Some(all.remove(at).1)
    }
}

/// Say how this platform picks files. Set once, at start.
#[cfg(not(feature = "desktop"))]
pub fn set_platform(platform: Platform) {
    let _ = answers::PLATFORM.set(platform);
}

/// The system's picker answered the dialog with this id: these files
/// (none: cancelled).
#[cfg(not(feature = "desktop"))]
pub fn deliver(id: &str, paths: Vec<PathBuf>) {
    answers::deliver(id, paths);
}

/// On phones and tablets, the app's documents folder (made if missing):
/// where "This device" starts.
pub fn documents() -> Option<PathBuf> {
    #[cfg(feature = "desktop")]
    {
        None
    }
    #[cfg(not(feature = "desktop"))]
    {
        let dir = answers::PLATFORM.get()?.documents.clone();
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }
}

/// An answer for the dialog with this id is waiting: open it again to have it.
pub fn ready(id: &str) -> bool {
    #[cfg(feature = "desktop")]
    {
        let _ = id;
        false
    }
    #[cfg(not(feature = "desktop"))]
    answers::ready(id)
}

/// A file dialog: the same calls as rfd's, plus [`FileDialog::id`].
#[derive(Default)]
pub struct FileDialog {
    #[cfg(feature = "desktop")]
    inner: rfd::FileDialog,
    #[cfg(not(feature = "desktop"))]
    id: &'static str,
    #[cfg(not(feature = "desktop"))]
    name: Option<String>,
}

#[cfg(feature = "desktop")]
impl FileDialog {
    pub fn new() -> Self {
        Self { inner: rfd::FileDialog::new() }
    }
    /// Which dialog this is, for an answer that comes later (phones).
    pub fn id(self, _id: &'static str) -> Self {
        self
    }
    pub fn add_filter(self, name: impl Into<String>, extensions: &[impl ToString]) -> Self {
        Self { inner: self.inner.add_filter(name, extensions) }
    }
    pub fn set_title(self, title: impl Into<String>) -> Self {
        Self { inner: self.inner.set_title(title) }
    }
    pub fn set_file_name(self, name: impl Into<String>) -> Self {
        Self { inner: self.inner.set_file_name(name) }
    }
    pub fn pick_file(self) -> Option<PathBuf> {
        self.inner.pick_file()
    }
    pub fn pick_files(self) -> Option<Vec<PathBuf>> {
        self.inner.pick_files()
    }
    pub fn pick_folder(self) -> Option<PathBuf> {
        self.inner.pick_folder()
    }
    pub fn save_file(self) -> Option<PathBuf> {
        self.inner.save_file()
    }
}

#[cfg(not(feature = "desktop"))]
impl FileDialog {
    pub fn new() -> Self {
        Self::default()
    }
    /// Which dialog this is, for the answer that comes later.
    pub fn id(mut self, id: &'static str) -> Self {
        self.id = id;
        self
    }
    /// The system picker shows every file: filters are left to the code
    /// that reads it.
    pub fn add_filter(self, _name: impl Into<String>, _extensions: &[impl ToString]) -> Self {
        self
    }
    pub fn set_title(self, _title: impl Into<String>) -> Self {
        self
    }
    pub fn set_file_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
    /// The picked file, if the picker has answered; otherwise opens it and
    /// gives nothing for now.
    pub fn pick_file(self) -> Option<PathBuf> {
        self.pick(false).and_then(|v| v.into_iter().next())
    }
    pub fn pick_files(self) -> Option<Vec<PathBuf>> {
        self.pick(true)
    }
    fn pick(self, multiple: bool) -> Option<Vec<PathBuf>> {
        if let Some(paths) = answers::take(self.id) {
            return (!paths.is_empty()).then_some(paths);
        }
        if let Some(p) = answers::PLATFORM.get() {
            (p.pick)(self.id, multiple);
        }
        None
    }
    /// The app's documents folder.
    pub fn pick_folder(self) -> Option<PathBuf> {
        self.documents()
    }
    /// A file of that name in the app's documents folder.
    pub fn save_file(self) -> Option<PathBuf> {
        let dir = self.documents()?;
        Some(dir.join(self.name.as_deref().unwrap_or("saved")))
    }
    fn documents(&self) -> Option<PathBuf> {
        documents()
    }
}
