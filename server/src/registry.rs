//! In-memory signaling state: hosts, PIN index, active sessions.
//!
//! Guarded by `std::sync::Mutex` rather than `tokio::sync::Mutex`: every
//! critical section below is a short, synchronous map mutation with no
//! `.await` held across the lock, so the async-aware mutex would only add
//! overhead without any benefit (see the tokio docs on when to prefer a std
//! mutex in async code).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use proto::signal::SignalMessage;
use rand::RngExt;
use tokio::sync::mpsc::UnboundedSender;

/// Channel used to push outgoing `SignalMessage`s to one connection's writer task.
pub type Tx = UnboundedSender<SignalMessage>;

const ID_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const ID_LEN: usize = 16;
const PIN_LEN: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinError {
    UnknownPin,
    HostBusy,
    /// `join_by_host_id` (slice 3.1): no host is currently registered under
    /// that `host_id`.
    HostOffline,
}

impl JoinError {
    pub fn message(self) -> &'static str {
        match self {
            JoinError::UnknownPin => "unknown pin",
            JoinError::HostBusy => "host busy",
            JoinError::HostOffline => "device offline",
        }
    }
}

struct HostEntry {
    name: String,
    pin: String,
    tx: Tx,
    session_id: Option<String>,
}

struct SessionEntry {
    host_id: String,
    host_tx: Tx,
    client_tx: Tx,
}

#[derive(Default)]
struct Inner {
    hosts: HashMap<String, HostEntry>,
    pin_to_host: HashMap<String, String>,
    sessions: HashMap<String, SessionEntry>,
}

#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<Mutex<Inner>>,
}

/// The old connection of the same device, displaced by a new registration
/// under the same `host_id` (slice 3.1: `host_id` is now the device's
/// persistent id, so a reconnecting host -- e.g. after 2.6a's backoff,
/// before the server notices the old socket is dead -- registers again
/// under an id that's still "live"). The caller (`ws.rs`) uses `host_tx` to
/// tell the old connection it's been replaced. `session`, if the old
/// connection was in one, is returned so the caller can log it -- the P2P
/// session itself is left alone (slice 3.5a: no `Bye` on signaling loss).
pub struct Displaced {
    pub host_tx: Tx,
    pub session: Option<(String, Tx)>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a host under the given `host_id` (slice 3.1: the caller --
    /// `devices::authenticate` via `ws.rs` -- decides this, it's no longer
    /// generated here). If `host_id` is already registered to a live
    /// connection, that connection is displaced: its session (if any) is
    /// closed and its `tx`/session info is returned in `Displaced` so the
    /// caller can notify it. A fresh PIN is generated either way.
    pub fn register_host(
        &self,
        host_id: String,
        name: String,
        tx: Tx,
    ) -> (String, Option<Displaced>) {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");

        let displaced = if let Some(old) = inner.hosts.remove(&host_id) {
            inner.pin_to_host.remove(&old.pin);
            let session = old.session_id.and_then(|session_id| {
                inner
                    .sessions
                    .remove(&session_id)
                    .map(|s| (session_id, s.client_tx))
            });
            Some(Displaced {
                host_tx: old.tx,
                session,
            })
        } else {
            None
        };

        let pin = loop {
            let candidate = random_pin();
            if !inner.pin_to_host.contains_key(&candidate) {
                break candidate;
            }
        };

        inner.pin_to_host.insert(pin.clone(), host_id.clone());
        inner.hosts.insert(
            host_id,
            HostEntry {
                name,
                pin: pin.clone(),
                tx,
                session_id: None,
            },
        );

        (pin, displaced)
    }

    /// Removes a host on disconnect, but only if `tx` is still the
    /// connection that owns `host_id` (`same_channel`) -- a displaced
    /// connection's own disconnect (it was already removed from the
    /// registry by `register_host`) must not tear down whatever new
    /// connection has since taken over `host_id`. If the host had an active
    /// session, returns that session's id and the client's tx for the
    /// caller to log (slice 3.5a: the P2P session itself is left running,
    /// no `Bye` is sent for a signaling-only disconnect).
    pub fn unregister_host(&self, host_id: &str, tx: &Tx) -> Option<(String, Tx)> {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");
        if !inner
            .hosts
            .get(host_id)
            .is_some_and(|host| host.tx.same_channel(tx))
        {
            return None;
        }
        let host = inner.hosts.remove(host_id)?;
        inner.pin_to_host.remove(&host.pin);

        if let Some(session_id) = host.session_id {
            let session = inner.sessions.remove(&session_id);
            return session.map(|s| (session_id, s.client_tx));
        }
        None
    }

    /// A client joins a host by PIN. Returns the new session id, the host's
    /// `host_id` (slice 3.1: so the caller can link the device to a known
    /// owner), the host's display name, and the host's tx (so the caller can
    /// notify it with `PeerJoined`). Resolves `pin` to a `host_id` in its own
    /// short critical section, then delegates the rest to `join_by_host_id`
    /// -- the two can't share one critical section since both take the same
    /// `Mutex`.
    pub fn join(
        &self,
        pin: &str,
        client_tx: Tx,
    ) -> Result<(String, String, String, Tx), JoinError> {
        let host_id = {
            let inner = self.inner.lock().expect("registry mutex poisoned");
            inner
                .pin_to_host
                .get(pin)
                .cloned()
                .ok_or(JoinError::UnknownPin)?
        };

        let (session_id, host_name, host_tx) =
            self.join_by_host_id(&host_id, client_tx)
                .map_err(|err| match err {
                    // `host_id` was just resolved from a live `pin_to_host`
                    // entry; if the host has since disconnected in the tiny
                    // window before we re-took the lock, that's still "this pin
                    // doesn't lead anywhere" from the PIN-joining caller's point
                    // of view, not a caller-visible `HostOffline` (a PIN joiner
                    // doesn't know about `host_id`s).
                    JoinError::HostOffline => JoinError::UnknownPin,
                    other => other,
                })?;

        Ok((session_id, host_id, host_name, host_tx))
    }

    /// A client joins a specific device by `host_id`, without a PIN (slice
    /// 3.1: `ConnectDevice`, once the client is authenticated and the device
    /// is confirmed linked to it). Returns the same triple as `join` minus
    /// `host_id` (the caller already has it). `HostOffline` if no host is
    /// currently registered under that id, `HostBusy` if it already has an
    /// active session.
    pub fn join_by_host_id(
        &self,
        host_id: &str,
        client_tx: Tx,
    ) -> Result<(String, String, Tx), JoinError> {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");

        let host = inner.hosts.get(host_id).ok_or(JoinError::HostOffline)?;
        if host.session_id.is_some() {
            return Err(JoinError::HostBusy);
        }

        let session_id = loop {
            let candidate = random_id();
            if !inner.sessions.contains_key(&candidate) {
                break candidate;
            }
        };

        let host_tx = host.tx.clone();
        let host_name = host.name.clone();

        inner.sessions.insert(
            session_id.clone(),
            SessionEntry {
                host_id: host_id.to_string(),
                host_tx: host_tx.clone(),
                client_tx,
            },
        );
        inner
            .hosts
            .get_mut(host_id)
            .expect("host still present")
            .session_id = Some(session_id.clone());

        Ok((session_id, host_name, host_tx))
    }

    /// Presence of a registered device (slice 3.1, used to fill
    /// `DeviceEntry.online`/`.busy`): `None` if it isn't currently
    /// connected, `Some(busy)` if it is, where `busy` is whether it has an
    /// active session.
    pub fn host_presence(&self, host_id: &str) -> Option<bool> {
        let inner = self.inner.lock().expect("registry mutex poisoned");
        inner
            .hosts
            .get(host_id)
            .map(|host| host.session_id.is_some())
    }

    /// Looks up the client tx for a session, validating that `host_id` is
    /// currently the host of `session_id`.
    pub fn peer_tx_for_host(&self, host_id: &str, session_id: &str) -> Option<Tx> {
        let inner = self.inner.lock().expect("registry mutex poisoned");
        let host = inner.hosts.get(host_id)?;
        if host.session_id.as_deref() != Some(session_id) {
            return None;
        }
        inner.sessions.get(session_id).map(|s| s.client_tx.clone())
    }

    /// Looks up the host tx for a session, validating that `client_tx` is the
    /// registered client of `session_id` (via channel identity).
    pub fn peer_tx_for_client(&self, session_id: &str, client_tx: &Tx) -> Option<Tx> {
        let inner = self.inner.lock().expect("registry mutex poisoned");
        let session = inner.sessions.get(session_id)?;
        if !session.client_tx.same_channel(client_tx) {
            return None;
        }
        Some(session.host_tx.clone())
    }

    /// Ends a session initiated by the host side (explicit `Bye`), freeing
    /// the host for a new session. Returns the client's tx to notify.
    pub fn close_session_by_host(&self, host_id: &str, session_id: &str) -> Option<Tx> {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");
        let host = inner.hosts.get(host_id)?;
        if host.session_id.as_deref() != Some(session_id) {
            return None;
        }
        let session = inner.sessions.remove(session_id)?;
        inner
            .hosts
            .get_mut(host_id)
            .expect("host still present")
            .session_id = None;
        Some(session.client_tx)
    }

    /// Ends a session initiated by the client side (explicit `Bye`, or the
    /// client disconnecting), freeing the host for a new session. Returns
    /// the host's tx to notify.
    pub fn close_session_by_client(&self, session_id: &str, client_tx: &Tx) -> Option<Tx> {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");
        let session = inner.sessions.get(session_id)?;
        if !session.client_tx.same_channel(client_tx) {
            return None;
        }
        let session = inner.sessions.remove(session_id)?;
        if let Some(host) = inner.hosts.get_mut(&session.host_id) {
            host.session_id = None;
        }
        Some(session.host_tx)
    }

    /// Called when a client connection is dropped without an explicit `Bye`.
    /// Scans active sessions for one whose client tx is `client_tx`, removes
    /// it, frees the host, and returns `(session_id, host_tx)` for the
    /// caller to log (slice 3.5a: no `Bye` is sent for a signaling-only
    /// disconnect -- the P2P session is left running).
    pub fn disconnect_client(&self, client_tx: &Tx) -> Option<(String, Tx)> {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");
        let session_id = inner
            .sessions
            .iter()
            .find(|(_, s)| s.client_tx.same_channel(client_tx))
            .map(|(id, _)| id.clone())?;
        let session = inner.sessions.remove(&session_id)?;
        if let Some(host) = inner.hosts.get_mut(&session.host_id) {
            host.session_id = None;
        }
        Some((session_id, session.host_tx))
    }
}

/// A random string of `len` characters drawn from `ID_CHARS`
/// (`a-z0-9`), used both for this module's own ids (`random_id`) and, via
/// `server::devices`, for device ids/secrets (slice 3.1) -- one generator so
/// the alphabet isn't duplicated.
pub fn random_token(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| ID_CHARS[rng.random_range(0..ID_CHARS.len())] as char)
        .collect()
}

pub fn random_id() -> String {
    random_token(ID_LEN)
}

fn random_pin() -> String {
    let mut rng = rand::rng();
    (0..PIN_LEN)
        .map(|_| char::from_digit(rng.random_range(0..10u32), 10).expect("0..10 is a valid digit"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn channel() -> (Tx, tokio::sync::mpsc::UnboundedReceiver<SignalMessage>) {
        mpsc::unbounded_channel()
    }

    #[test]
    fn register_host_with_given_id_generates_six_digit_pin_and_no_displaced() {
        let registry = Registry::new();
        let (tx, _rx) = channel();
        let (pin, displaced) = registry.register_host("host1".to_string(), "host".to_string(), tx);

        assert_eq!(pin.len(), PIN_LEN);
        assert!(pin.chars().all(|c| c.is_ascii_digit()));
        assert!(displaced.is_none());
    }

    #[test]
    fn join_unknown_pin_fails() {
        let registry = Registry::new();
        let (client_tx, _rx) = channel();
        let err = registry.join("000000", client_tx).unwrap_err();
        assert_eq!(err, JoinError::UnknownPin);
    }

    #[test]
    fn join_busy_host_fails() {
        let registry = Registry::new();
        let (host_tx, _host_rx) = channel();
        let (pin, _displaced) =
            registry.register_host("host1".to_string(), "host".to_string(), host_tx);

        let (client_tx_1, _rx1) = channel();
        registry.join(&pin, client_tx_1).expect("first join ok");

        let (client_tx_2, _rx2) = channel();
        let err = registry.join(&pin, client_tx_2).unwrap_err();
        assert_eq!(err, JoinError::HostBusy);
    }

    #[test]
    fn close_session_by_client_frees_host_for_same_pin() {
        let registry = Registry::new();
        let (host_tx, _host_rx) = channel();
        let (pin, _displaced) =
            registry.register_host("host1".to_string(), "host".to_string(), host_tx);

        let (client_tx, _rx) = channel();
        let (session_id, _host_id, _name, _host_tx) =
            registry.join(&pin, client_tx.clone()).expect("join ok");

        assert!(registry
            .close_session_by_client(&session_id, &client_tx)
            .is_some());

        let (client_tx_2, _rx2) = channel();
        let (_session_id_2, _host_id_2, _name, _host_tx) =
            registry.join(&pin, client_tx_2).expect("second join ok");

        // host is still registered and its pin/id unchanged
        assert!(registry.peer_tx_for_host("host1", "nonexistent").is_none());
    }

    #[test]
    fn re_registering_same_host_id_displaces_old_connection_and_invalidates_old_pin() {
        let registry = Registry::new();
        let (old_tx, _old_rx) = channel();
        let (old_pin, displaced) =
            registry.register_host("host1".to_string(), "host".to_string(), old_tx.clone());
        assert!(displaced.is_none());

        let (new_tx, _new_rx) = channel();
        let (new_pin, displaced) =
            registry.register_host("host1".to_string(), "host".to_string(), new_tx);

        let displaced = displaced.expect("second registration displaces the first");
        assert!(displaced.host_tx.same_channel(&old_tx));
        assert!(displaced.session.is_none());

        let (client_tx, _rx) = channel();
        assert_eq!(
            registry.join(&old_pin, client_tx).unwrap_err(),
            JoinError::UnknownPin
        );

        let (client_tx_2, _rx2) = channel();
        registry
            .join(&new_pin, client_tx_2)
            .expect("new pin still works");
    }

    #[test]
    fn re_registering_host_in_session_displaces_and_returns_session_info() {
        let registry = Registry::new();
        let (host_tx, _host_rx) = channel();
        let (pin, _displaced) =
            registry.register_host("host1".to_string(), "host".to_string(), host_tx);

        let (client_tx, _client_rx) = channel();
        let (session_id, _host_id, _name, _host_tx) =
            registry.join(&pin, client_tx.clone()).expect("join ok");

        let (new_tx, _new_rx) = channel();
        let (_new_pin, displaced) =
            registry.register_host("host1".to_string(), "host".to_string(), new_tx);

        let displaced = displaced.expect("re-registration while in session displaces");
        let (displaced_session_id, displaced_client_tx) =
            displaced.session.expect("displaced host was in a session");
        assert_eq!(displaced_session_id, session_id);
        assert!(displaced_client_tx.same_channel(&client_tx));

        // the session is gone: the client can no longer be found via it
        assert!(registry
            .peer_tx_for_client(&session_id, &client_tx)
            .is_none());
    }

    #[test]
    fn unregister_host_with_stale_tx_is_a_no_op() {
        let registry = Registry::new();
        let (old_tx, _old_rx) = channel();
        let (_old_pin, _displaced) =
            registry.register_host("host1".to_string(), "host".to_string(), old_tx.clone());

        let (new_tx, _new_rx) = channel();
        let (new_pin, _displaced) =
            registry.register_host("host1".to_string(), "host".to_string(), new_tx);

        // the stale (displaced) tx unregistering must not remove the new
        // registration
        assert!(registry.unregister_host("host1", &old_tx).is_none());

        let (client_tx, _rx) = channel();
        registry
            .join(&new_pin, client_tx)
            .expect("new registration still works");
    }

    #[test]
    fn unregister_host_with_current_tx_removes_registration() {
        let registry = Registry::new();
        let (tx, _rx) = channel();
        let (pin, _displaced) =
            registry.register_host("host1".to_string(), "host".to_string(), tx.clone());

        // No session was active, so a successful unregister also returns
        // `None` here -- verify success via the pin no longer working.
        assert!(registry.unregister_host("host1", &tx).is_none());

        let (client_tx, _rx2) = channel();
        assert_eq!(
            registry.join(&pin, client_tx).unwrap_err(),
            JoinError::UnknownPin
        );
    }

    #[test]
    fn join_by_host_id_for_unregistered_device_fails_with_host_offline() {
        let registry = Registry::new();
        let (client_tx, _rx) = channel();
        let err = registry
            .join_by_host_id("no-such-device", client_tx)
            .unwrap_err();
        assert_eq!(err, JoinError::HostOffline);
    }

    #[test]
    fn join_by_host_id_for_busy_device_fails_with_host_busy() {
        let registry = Registry::new();
        let (host_tx, _host_rx) = channel();
        registry.register_host("host1".to_string(), "host".to_string(), host_tx);

        let (client_tx_1, _rx1) = channel();
        registry
            .join_by_host_id("host1", client_tx_1)
            .expect("first join ok");

        let (client_tx_2, _rx2) = channel();
        let err = registry.join_by_host_id("host1", client_tx_2).unwrap_err();
        assert_eq!(err, JoinError::HostBusy);
    }

    #[test]
    fn join_by_host_id_success_is_visible_via_peer_tx_for_host() {
        let registry = Registry::new();
        let (host_tx, _host_rx) = channel();
        registry.register_host("host1".to_string(), "host".to_string(), host_tx);

        let (client_tx, _rx) = channel();
        let (session_id, host_name, _host_tx) = registry
            .join_by_host_id("host1", client_tx)
            .expect("join ok");

        assert_eq!(host_name, "host");
        assert!(registry.peer_tx_for_host("host1", &session_id).is_some());
    }

    #[test]
    fn join_by_pin_still_works_and_also_returns_the_hosts_id() {
        let registry = Registry::new();
        let (host_tx, _host_rx) = channel();
        let (pin, _displaced) =
            registry.register_host("host1".to_string(), "host".to_string(), host_tx);

        let (client_tx, _rx) = channel();
        let (_session_id, host_id, host_name, _host_tx) =
            registry.join(&pin, client_tx).expect("join ok");

        assert_eq!(host_id, "host1");
        assert_eq!(host_name, "host");
    }

    #[test]
    fn host_presence_reflects_disconnected_connected_and_busy_states() {
        let registry = Registry::new();
        assert_eq!(registry.host_presence("host1"), None);

        let (host_tx, _host_rx) = channel();
        registry.register_host("host1".to_string(), "host".to_string(), host_tx);
        assert_eq!(registry.host_presence("host1"), Some(false));

        let (client_tx, _rx) = channel();
        registry
            .join_by_host_id("host1", client_tx)
            .expect("join ok");
        assert_eq!(registry.host_presence("host1"), Some(true));
    }
}
