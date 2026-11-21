//! Locking the app on phones: with the setting on, MalasDoh asks for the
//! fingerprint, face or the device's PIN when it starts and when it comes
//! back after a while away. Each platform's glue says how to ask; the
//! answer comes back through [`answer`].

use std::sync::{Mutex, OnceLock};

type Asker = Box<dyn Fn() + Send + Sync>;

static ASKER: OnceLock<Asker> = OnceLock::new();
static ANSWER: Mutex<Option<bool>> = Mutex::new(None);

/// How to ask for the fingerprint, face or PIN here. Set once, at start.
pub fn set_asker(ask: impl Fn() + Send + Sync + 'static) {
    let _ = ASKER.set(Box::new(ask));
}

/// Whether this platform can lock the app.
pub fn available() -> bool {
    ASKER.get().is_some()
}

/// Show the system's prompt; its answer comes through [`answer`].
pub(crate) fn ask() {
    if let Some(a) = ASKER.get() {
        a();
    }
}

/// The system's prompt answered: `true` unlocks. (A device with no screen
/// lock at all answers `true`: there is nothing to check against.)
pub fn answer(unlocked: bool) {
    *ANSWER.lock().unwrap_or_else(|e| e.into_inner()) = Some(unlocked);
}

pub(crate) fn take_answer() -> Option<bool> {
    ANSWER.lock().unwrap_or_else(|e| e.into_inner()).take()
}
