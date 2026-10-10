//! Getting sudo going on a connection: without a password if it needs none,
//! else with the saved sudo password, else (if the connection says so) with
//! the saved login password, else by asking. A saved password that sudo
//! rejects is forgotten and the user is asked, rather than tried again:
//! failed sudo attempts are logged and can lock the account.

use std::sync::Arc;

use malasdoh_core::edit::{RemoteIo, SudoError};
use malasdoh_core::profile::Profile;
use malasdoh_core::secrets::{SecretKey, SecretStore};
use malasdoh_core::ssh::Connection;
use tokio::sync::oneshot;

use crate::bridge::{EventKind, Events, Question, SessionId};

/// The password to use (`None`: sudo needs none), and a problem worth
/// telling the user about (the keychain refused to save it).
pub struct Obtained {
    pub password: Option<String>,
    pub warning: Option<String>,
}

/// Does sudo accept `password`? `Err` for anything other than a wrong or
/// missing password.
async fn accepts(conn: &Connection, password: Option<&str>) -> Result<bool, String> {
    match RemoteIo::check_sudo(conn, password).await {
        Ok(()) => Ok(true),
        Err(e) if matches!(e.downcast_ref::<SudoError>(), Some(SudoError::PasswordRequired)) => Ok(false),
        Err(e) => Err(format!("{e:#}")),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

pub async fn obtain(
    conn: &Connection,
    profile: &Profile,
    secrets: &Arc<dyn SecretStore>,
    events: &Events,
    session: SessionId,
) -> Result<Obtained, String> {
    if accepts(conn, None).await? {
        return Ok(Obtained { password: None, warning: None });
    }
    let sudo_key = SecretKey::SudoPassword(profile.id);
    let mut note = None;
    let store = secrets.clone();
    let key = sudo_key.clone();
    if let Some(saved) = blocking(move || store.get(&key)).await.flatten() {
        if accepts(conn, Some(&saved)).await? {
            return Ok(Obtained { password: Some(saved), warning: None });
        }
        let (store, key) = (secrets.clone(), sudo_key.clone());
        blocking(move || store.delete(&key)).await;
        note = Some("The saved sudo password was not accepted, so it has been forgotten.".to_string());
    } else if profile.sudo_uses_login_password {
        let (store, key) = (secrets.clone(), SecretKey::Password(profile.id));
        if let Some(login) = blocking(move || store.get(&key)).await.flatten() {
            if accepts(conn, Some(&login)).await? {
                return Ok(Obtained { password: Some(login), warning: None });
            }
            note = Some("Your saved login password does not work for sudo here.".to_string());
        }
    }
    let host = format!("{}@{} ({})", profile.user, profile.host, profile.display_name());
    let mut retry = false;
    loop {
        let (reply, rx) = oneshot::channel();
        events
            .send(session, EventKind::Question(Question::Sudo { host: host.clone(), retry, note: note.take(), reply }));
        let Some(answer) = rx.await.ok().flatten() else { return Err("cancelled".into()) };
        if !accepts(conn, Some(&answer.password)).await? {
            retry = true;
            continue;
        }
        let mut warning = None;
        if answer.remember {
            let (store, key, value) = (secrets.clone(), sudo_key.clone(), answer.password.clone());
            if let Some(Err(e)) = blocking(move || store.set(&key, &value)).await {
                warning = Some(format!("The sudo password was not saved: {e}"));
            }
        }
        return Ok(Obtained { password: Some(answer.password), warning });
    }
}
