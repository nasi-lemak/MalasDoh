//! What the Teleport window asks of the app: storing clusters, turning
//! their servers into connections, and connecting sessions that were
//! waiting for a login.

use malasdoh_core::profile::Profile;
use malasdoh_core::secrets::SecretKey;
use malasdoh_core::teleport::{self, ClusterId};

use super::App;
use crate::teleport_ui::TeleportAction;

impl App {
    pub(super) fn teleport_actions(&mut self, actions: Vec<TeleportAction>) {
        if actions.is_empty() {
            return;
        }
        let tdir = self.shared.paths.teleport_dir();
        for a in actions {
            match a {
                TeleportAction::Save(cluster) => {
                    let id = cluster.id;
                    match self.teleport.clusters.iter_mut().find(|c| c.id == id) {
                        Some(c) => *c = *cluster,
                        None => self.teleport.clusters.push(*cluster),
                    }
                    self.refresh_proxy(id);
                    self.reconnect_waiting(id);
                }
                TeleportAction::AddServers { cluster, nodes, login } => {
                    self.add_teleport_servers(cluster, nodes, login)
                }
                TeleportAction::LogOut(id) => {
                    if let Some(c) = self.teleport.get(id) {
                        teleport::log_out(c, &tdir);
                    }
                }
                TeleportAction::Remove(id) => {
                    if let Some(c) = self.teleport.get(id) {
                        teleport::log_out(c, &tdir);
                    }
                    self.teleport.clusters.retain(|c| c.id != id);
                    let secrets = self.shared.secrets.clone();
                    self.shared.rt.spawn_blocking(move || secrets.delete(&SecretKey::TeleportPassword(id)));
                }
            }
        }
        if let Err(e) = self.teleport.save() {
            self.notify(format!("Could not save the Teleport clusters: {e:#}"), true);
        }
        if let Err(e) = self.teleport.write_known_hosts(&tdir.join("known_hosts")) {
            self.notify(format!("Could not write the Teleport host CAs: {e:#}"), true);
        }
    }

    /// After a login: the proxy connection logs in as a login the new
    /// certificate allows, and the CA lines cover it.
    fn refresh_proxy(&mut self, id: ClusterId) {
        let tdir = self.shared.paths.teleport_dir();
        let Some(cluster) = self.teleport.get(id).cloned() else { return };
        let Some(pid) = cluster.proxy_profile else { return };
        let Some(state) = teleport::cert_state(&cluster.key_path(&tdir)) else { return };
        let existing = self.profiles.get(pid).cloned();
        let login = existing
            .as_ref()
            .map(|p| p.user.clone())
            .filter(|u| state.logins.contains(u))
            .or_else(|| state.logins.first().cloned())
            .unwrap_or_default();
        if let Ok(p) = teleport::proxy_profile(&cluster, &tdir, &login, existing) {
            let pattern = teleport::host_pattern(&p.host, p.port);
            let _ = self.profiles.upsert(p);
            if let Some(c) = self.teleport.clusters.iter_mut().find(|c| c.id == id)
                && !c.hosts.contains(&pattern)
            {
                c.hosts.insert(0, pattern);
            }
            self.save_profiles();
        }
    }

    fn add_teleport_servers(&mut self, id: ClusterId, nodes: Vec<teleport::Node>, login: String) {
        let tdir = self.shared.paths.teleport_dir();
        let Some(cluster) = self.teleport.get(id).cloned() else { return };
        let Some(state) = teleport::cert_state(&cluster.key_path(&tdir)) else {
            return self.notify("Log in to Teleport first.", true);
        };
        // The proxy hop logs in too; any login the certificate allows will do.
        let proxy_login = if state.logins.contains(&login) {
            login.clone()
        } else {
            state.logins.first().cloned().unwrap_or_default()
        };
        let existing = cluster.proxy_profile.and_then(|p| self.profiles.get(p).cloned());
        let proxy = match teleport::proxy_profile(&cluster, &tdir, &proxy_login, existing) {
            Ok(p) => p,
            Err(e) => return self.notify(format!("{e:#}"), true),
        };
        let (proxy_id, mut hosts) = (proxy.id, vec![teleport::host_pattern(&proxy.host, proxy.port)]);
        let _ = self.profiles.upsert(proxy);
        let mut added = 0;
        for n in &nodes {
            let existing: Option<Profile> =
                self.profiles.profiles.iter().find(|p| p.teleport == Some(id) && p.host == n.id).cloned();
            let login = if n.logins.contains(&login) {
                login.clone()
            } else {
                n.logins.first().cloned().unwrap_or(login.clone())
            };
            let p = teleport::node_profile(&cluster, &tdir, n, &login, proxy_id, existing);
            hosts.push(teleport::host_pattern(&p.host, p.port));
            if self.profiles.upsert(p).is_ok() {
                added += 1;
            }
        }
        if let Some(c) = self.teleport.clusters.iter_mut().find(|c| c.id == id) {
            c.proxy_profile = Some(proxy_id);
            for h in hosts {
                if !c.hosts.contains(&h) {
                    c.hosts.push(h);
                }
            }
        }
        self.save_profiles();
        self.notify(format!("{added} Teleport server(s) added to the connection list."), false);
    }

    /// Sessions that stopped for this login try again now that it is done.
    fn reconnect_waiting(&mut self, id: ClusterId) {
        let tdir = self.shared.paths.teleport_dir();
        let valid =
            self.teleport.get(id).and_then(|c| teleport::cert_state(&c.key_path(&tdir))).is_some_and(|s| s.is_valid());
        if !valid {
            return;
        }
        let waiting: Vec<u64> = self.teleport_win.waiting.iter().filter(|(c, _)| *c == id).map(|(_, s)| *s).collect();
        self.teleport_win.waiting.retain(|(c, _)| *c != id);
        for sid in waiting {
            if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                s.connect(&self.shared);
            }
        }
    }
}
