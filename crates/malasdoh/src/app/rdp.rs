//! Remote Desktop connections: finding a way to the desktop (a Teleport
//! server in the list that reaches it, a chosen SSH connection, the
//! connection's proxy, or directly), then handing it to the desktops window.

use std::sync::Arc;
use std::time::Duration;

use malasdoh_core::forward::ForwardSpec;
use malasdoh_core::profile::{Profile, ProfileId};
use malasdoh_core::ssh::{self, ConnectContext};
use malasdoh_core::teleport::LoginNeeded;

use super::App;
use crate::bridge::{EventKind, MULTI_RUN, UiPrompter};
use crate::rdp_ui::{Carrier, RdpMsg};
use crate::session::Shared;

/// How long a server gets to show it reaches the desktop.
const PROBE: Duration = Duration::from_secs(8);

fn connect_ctx(shared: &Shared) -> ConnectContext<UiPrompter> {
    let prompter = Arc::new(UiPrompter { events: shared.events.clone(), session: MULTI_RUN });
    let mut ctx = ConnectContext::new(shared.known_hosts.clone(), shared.secrets.clone(), prompter);
    ctx.wireguard = shared.wireguard.clone();
    ctx
}

impl App {
    /// Connect a Remote Desktop connection: find a route, then open the
    /// Remote Desktop program on it. An open one is just shown again.
    pub(super) fn open_desktop(&mut self, id: ProfileId) {
        let Some(p) = self.profiles.get(id).cloned() else { return };
        if self.desktops.reopen(id, &crate::rdp_ui::Env::new(&self.shared)) {
            return;
        }
        let via = p.rdp.as_ref().and_then(|r| r.via);
        // The chosen (or last working) server first, then, for a Teleport
        // desktop, the cluster's other servers in the list.
        let mut routes: Vec<(ProfileId, Vec<Profile>, String)> = Vec::new();
        if let Some(v) = via
            && let Ok(chain) = self.profiles.chain(v)
        {
            routes.push((v, chain, self.profiles.get(v).map(|q| q.display_name()).unwrap_or_default()));
        }
        if let Some(cluster) = p.teleport {
            let mut others: Vec<&Profile> = self
                .profiles
                .profiles
                .iter()
                .filter(|q| q.teleport == Some(cluster) && q.rdp.is_none() && q.jump.is_some() && Some(q.id) != via)
                .collect();
            others.sort_by_key(|q| q.display_name().to_lowercase());
            for q in others {
                if let Ok(chain) = self.profiles.chain(q.id) {
                    routes.push((q.id, chain, q.display_name()));
                }
            }
        }
        let did = self.desktops.start(id, p.display_name(), p.user.clone());
        let (events, rt) = (self.shared.events.clone(), self.shared.rt.clone());
        let send = move |m: RdpMsg| events.send(MULTI_RUN, EventKind::Rdp(m));
        let (host, port) = (p.host.clone(), p.port);

        if routes.is_empty() && p.teleport.is_some() {
            return self.desktop_msg(RdpMsg::Failed {
                id: did,
                error: "Remote Desktop goes through one of the cluster's Linux servers, and none is in your \
                        connection list. Add one that can reach this desktop (Connection > Teleport…), or choose \
                        one under Through in Edit…."
                    .into(),
            });
        }
        if routes.is_empty() && p.proxy.is_none() {
            // Nothing to carry: the program connects to the desktop itself.
            return self.desktop_msg(RdpMsg::Ready {
                id: did,
                addr: format!("{host}:{port}"),
                via: None,
                via_name: String::new(),
                carrier: Carrier::Direct,
            });
        }
        if routes.is_empty() {
            // Through the connection's proxy (WireGuard, SOCKS, HTTP…).
            let ctx = Arc::new(connect_ctx(&self.shared));
            let label = p.proxy.label();
            rt.spawn(async move {
                send(RdpMsg::Status { id: did, text: format!("Reaching {host}:{port} through {label}…") });
                let first = match ssh::dial(&p, &ctx).await {
                    Ok(s) => {
                        malasdoh_core::rdp::probe_stream(s, &format!("{host}:{port}"), &format!("through {label}"))
                            .await
                    }
                    Err(e) => Err(e),
                };
                if let Err(e) = first {
                    return send(RdpMsg::Failed { id: did, error: format!("{e:#}") });
                }
                let dial = move || {
                    let (p, ctx) = (p.clone(), ctx.clone());
                    async move { ssh::dial(&p, &ctx).await }
                };
                match malasdoh_core::rdp::listen(dial).await {
                    Ok((local, task)) => send(RdpMsg::Ready {
                        id: did,
                        addr: format!("127.0.0.1:{local}"),
                        via: None,
                        via_name: label,
                        carrier: Carrier::Proxy(task),
                    }),
                    Err(e) => send(RdpMsg::Failed { id: did, error: format!("{e:#}") }),
                }
            });
            return;
        }

        let ctx = connect_ctx(&self.shared);
        rt.spawn(async move {
            let mut tried = Vec::new();
            for (via, chain, name) in routes {
                send(RdpMsg::Status { id: did, text: format!("Connecting to {name}…") });
                let conn = match ssh::connect(chain, &ctx).await {
                    Ok(c) => Arc::new(c),
                    Err(e) if e.downcast_ref::<LoginNeeded>().is_some() => {
                        return send(RdpMsg::Failed {
                            id: did,
                            error: format!("{e:#}. Log in (Connection > Teleport…), then connect again."),
                        });
                    }
                    Err(e) => {
                        tried.push(format!("{name}: {e:#}"));
                        continue;
                    }
                };
                send(RdpMsg::Status { id: did, text: format!("Checking that {name} reaches {host}:{port}…") });
                if let Err(e) = malasdoh_core::rdp::probe(&conn, &host, port, PROBE).await {
                    tried.push(format!("{name}: {e:#}"));
                    conn.disconnect().await;
                    continue;
                }
                let spec = ForwardSpec { target_host: host.clone(), target_port: port, ..Default::default() };
                match conn.start_forward(&spec).await {
                    Ok(forward) => {
                        let addr = format!("127.0.0.1:{}", forward.port);
                        return send(RdpMsg::Ready {
                            id: did,
                            addr,
                            via: Some(via),
                            via_name: name,
                            carrier: Carrier::Ssh { conn, forward },
                        });
                    }
                    Err(e) => {
                        tried.push(format!("{name}: {e:#}"));
                        conn.disconnect().await;
                    }
                }
            }
            send(RdpMsg::Failed {
                id: did,
                error: format!(
                    "No route to {host}:{port}. Tried:\n{}\nA server must be on the desktop's network and allowed \
                     to forward ports (Teleport: your role's port forwarding).",
                    tried.join("\n")
                ),
            });
        });
    }

    pub(super) fn desktop_msg(&mut self, m: RdpMsg) {
        // A route that worked is kept, to be tried first next time.
        if let Some((id, Some(via))) = self.desktops.handle(m, &crate::rdp_ui::Env::new(&self.shared))
            && let Some(mut p) = self.profiles.get(id).cloned()
            && let Some(rd) = p.rdp.as_mut()
            && rd.via != Some(via)
        {
            rd.via = Some(via);
            if self.profiles.upsert(p).is_ok() {
                self.save_profiles();
            }
        }
    }
}
