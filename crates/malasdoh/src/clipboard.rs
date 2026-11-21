//! The system clipboard, for the platforms where egui has no way to read it
//! (Android, iOS): each one's glue says how, and the app offers Paste
//! buttons where a touch screen has no Ctrl+V.

use std::sync::OnceLock;

type Reader = Box<dyn Fn() -> Option<String> + Send + Sync>;

static READER: OnceLock<Reader> = OnceLock::new();

/// How to read the system clipboard. Set once, at start.
pub fn set_reader(read: impl Fn() -> Option<String> + Send + Sync + 'static) {
    let _ = READER.set(Box::new(read));
}

/// Whether [`read`] can read the system clipboard here.
pub fn available() -> bool {
    READER.get().is_some()
}

/// The text on the system clipboard, if there is any and it can be read.
pub fn read() -> Option<String> {
    READER.get().and_then(|r| r()).filter(|t| !t.is_empty())
}
