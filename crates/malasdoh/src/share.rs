//! Handing a file to another app (the system's share sheet), on the
//! platforms that have one (Android, iOS): each one's glue says how.

use std::path::Path;
use std::sync::OnceLock;

type Sharer = Box<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

static SHARER: OnceLock<Sharer> = OnceLock::new();

/// How to share a file here. Set once, at start.
pub fn set_sharer(share: impl Fn(&Path) -> Result<(), String> + Send + Sync + 'static) {
    let _ = SHARER.set(Box::new(share));
}

/// Whether files can be shared here.
pub fn available() -> bool {
    SHARER.get().is_some()
}

/// Opens the system's share sheet for `path`.
pub fn share(path: &Path) -> Result<(), String> {
    match SHARER.get() {
        Some(s) => s(path),
        None => Err("sharing is not available here".into()),
    }
}
