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
}

impl JoinError {
    pub fn message(self) -> &'static str {
        match self {
            JoinError::UnknownPin => "unknown pin",
            JoinError::HostBusy => "host busy",
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

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new host, generating a unique `host_id` and PIN.
    pub fn register_host(&self, name: String, tx: Tx) -> (String, String) {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");

        let host_id = loop {
            let candidate = random_id();
            if !inner.hosts.contains_key(&candidate) {
                break candidate;
            }
        };
        let pin = loop {
            let candidate = random_pin();
            if !inner.pin_to_host.contains_key(&candidate) {
                break candidate;
            }
        };

        inner.pin_to_host.insert(pin.clone(), host_id.clone());
        inner.hosts.insert(
            host_id.clone(),
            HostEntry {
                name,
                pin: pin.clone(),
                tx,
                session_id: None,
            },
        );

        (host_id, pin)
    }

    /// Removes a host on disconnect. If it had an active session, returns
    /// that session's id and the client's tx so the caller can notify the
    /// client with `Bye`.
    pub fn unregister_host(&self, host_id: &str) -> Option<(String, Tx)> {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");
        let host = inner.hosts.remove(host_id)?;
        inner.pin_to_host.remove(&host.pin);

        if let Some(session_id) = host.session_id {
            let session = inner.sessions.remove(&session_id);
            return session.map(|s| (session_id, s.client_tx));
        }
        None
    }

    /// A client joins a host by PIN. Returns the new session id, the host's
    /// display name, and the host's tx (so the caller can notify it with
    /// `PeerJoined`).
    pub fn join(&self, pin: &str, client_tx: Tx) -> Result<(String, String, Tx), JoinError> {
        let mut inner = self.inner.lock().expect("registry mutex poisoned");

        let host_id = inner
            .pin_to_host
            .get(pin)
            .cloned()
            .ok_or(JoinError::UnknownPin)?;
        let host = inner
            .hosts
            .get(&host_id)
            .expect("pin_to_host points at a live host");

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
                host_id: host_id.clone(),
                host_tx: host_tx.clone(),
                client_tx,
            },
        );
        inner
            .hosts
            .get_mut(&host_id)
            .expect("host still present")
            .session_id = Some(session_id.clone());

        Ok((session_id, host_name, host_tx))
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
    /// it, frees the host, and returns `(session_id, host_tx)` to notify.
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

fn random_id() -> String {
    let mut rng = rand::rng();
    (0..ID_LEN)
        .map(|_| ID_CHARS[rng.random_range(0..ID_CHARS.len())] as char)
        .collect()
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
    fn register_host_generates_six_digit_pin_and_sixteen_char_id() {
        let registry = Registry::new();
        let (tx, _rx) = channel();
        let (host_id, pin) = registry.register_host("host".to_string(), tx);

        assert_eq!(host_id.len(), ID_LEN);
        assert!(host_id.bytes().all(|b| ID_CHARS.contains(&b)));
        assert_eq!(pin.len(), PIN_LEN);
        assert!(pin.chars().all(|c| c.is_ascii_digit()));
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
        let (_host_id, pin) = registry.register_host("host".to_string(), host_tx);

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
        let (host_id, pin) = registry.register_host("host".to_string(), host_tx);

        let (client_tx, _rx) = channel();
        let (session_id, _name, _host_tx) =
            registry.join(&pin, client_tx.clone()).expect("join ok");

        assert!(registry
            .close_session_by_client(&session_id, &client_tx)
            .is_some());

        let (client_tx_2, _rx2) = channel();
        let (_session_id_2, _name, _host_tx) =
            registry.join(&pin, client_tx_2).expect("second join ok");

        // host is still registered and its pin/id unchanged
        assert!(registry.peer_tx_for_host(&host_id, "nonexistent").is_none());
    }
}
