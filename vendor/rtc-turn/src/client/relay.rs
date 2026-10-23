use log::{debug, warn};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::ops::Add;
use std::time::{Duration, Instant};

use stun::attributes::*;
use stun::error_code::*;
use stun::fingerprint::*;
use stun::integrity::*;
use stun::message::*;
use stun::textattrs::*;

use super::permission::*;
use super::transaction::*;
use crate::proto;

use crate::client::binding::BindingState;
use crate::client::{Client, Event, RelayedAddr};
use shared::error::{Error, Result};

const PERM_REFRESH_INTERVAL: Duration = Duration::from_secs(120);
// https://datatracker.ietf.org/doc/html/rfc8656#name-permissions-2
// The Permission Lifetime MUST be 300 seconds (= 5 minutes).
const PERM_LIFETIME: Duration = Duration::from_secs(300);
const MAX_RETRY_ATTEMPTS: u16 = 3;
/// rcdesk patch (D75): how soon a channel-binding refresh the server rejected is tried again.
const BIND_REFRESH_RETRY: Duration = Duration::from_secs(5);
/// Smallest step the allocation-refresh timer may advance by.
///
/// The timer advances by a fixed cadence from its own previous value, which keeps the refresh
/// schedule drift-free. That only works while the step is positive. `lifetime` is assigned
/// straight from the server's LIFETIME attribute, and a refresh carrying `LIFETIME=0` — how
/// RFC 5766 deallocates, and what [`Relay::close`] sends — makes `lifetime / 2` zero. The timer
/// then freezes on an already-expired instant and `poll_timeout` reports that same instant
/// forever. A caller that treats an expired deadline as "handle it and go round again" spins
/// on it without ever reaching its socket reads, which is
/// [webrtc#862](https://github.com/webrtc-rs/webrtc/issues/862).
const MIN_ALLOC_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

// RelayState is a set of params use by Relay
pub(crate) struct RelayState {
    pub(crate) relayed_addr: RelayedAddr,
    pub(crate) integrity: MessageIntegrity,
    pub(crate) nonce: Nonce,
    pub(crate) lifetime: Duration,
    perm_map: HashMap<SocketAddr, Permission>,
    refresh_alloc_timer: Instant,
    refresh_perms_timer: Instant,
}

impl RelayState {
    pub(super) fn new(
        relayed_addr: RelayedAddr,
        integrity: MessageIntegrity,
        nonce: Nonce,
        lifetime: Duration,
    ) -> Self {
        debug!("initial lifetime: {} seconds", lifetime.as_secs());

        Self {
            relayed_addr,
            integrity,
            nonce,
            lifetime,
            perm_map: HashMap::new(),
            refresh_alloc_timer: Instant::now().add(lifetime / 2),
            refresh_perms_timer: Instant::now().add(PERM_REFRESH_INTERVAL),
        }
    }

    /// Takes the NONCE a 438 (Stale Nonce) response carries. Returns whether it differs from the
    /// one we had -- the condition for retrying the rejected request (rcdesk patch, D75), which
    /// keeps a server repeating the same nonce from looping us.
    pub(super) fn set_nonce_from_msg(&mut self, msg: &Message) -> bool {
        // Update nonce
        match Nonce::get_from_as(msg, ATTR_NONCE) {
            Ok(nonce) => {
                let changed = nonce.text != self.nonce.text;
                self.nonce = nonce;
                debug!("refresh allocation: 438, got new nonce.");
                changed
            }
            Err(_) => {
                warn!("refresh allocation: 438 but no nonce.");
                false
            }
        }
    }
}

/// A borrowed handle to one live allocation on the TURN server.
///
/// Obtained from [`Client::relay`](crate::client::Client::relay). Sending to a peer requires
/// a permission for it first — see [`Self::create_permission`].
pub struct Relay<'a> {
    pub(crate) relayed_addr: RelayedAddr,
    pub(crate) client: &'a mut Client,
}

impl Relay<'_> {
    /// Asks the server to permit traffic to and from `peer_addr`.
    ///
    /// The relay silently drops data for peers with no permission, and permissions expire after
    /// five minutes unless refreshed. Returns the transaction id to match against the resulting
    /// [`Event`], or `None` if a permission is already in place.
    ///
    /// # Errors
    ///
    /// Fails if the allocation no longer exists or the request cannot be encoded.
    pub fn create_permission(&mut self, peer_addr: SocketAddr) -> Result<Option<TransactionId>> {
        if let Some(relay) = self.client.relays.get_mut(&self.relayed_addr) {
            relay
                .perm_map
                .entry(peer_addr)
                .or_insert_with(Permission::default);
            if let Some(perm) = relay.perm_map.get(&peer_addr)
                && perm.state() == PermState::Idle
            {
                return Ok(Some(
                    self.create_permissions(&[peer_addr], Some(peer_addr))?,
                ));
            }
            Ok(None)
        } else {
            Err(Error::ErrConnClosed)
        }
    }

    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        if let Some(relay) = self.client.relays.get(&self.relayed_addr) {
            if relay.refresh_alloc_timer < relay.refresh_perms_timer {
                Some(relay.refresh_alloc_timer)
            } else {
                Some(relay.refresh_perms_timer)
            }
        } else {
            None
        }
    }

    pub(crate) fn handle_timeout(&mut self, now: Instant) {
        let (refresh_alloc_timer, refresh_perms_timer) =
            if let Some(relay) = self.client.relays.get_mut(&self.relayed_addr) {
                let refresh_alloc_timer = if relay.refresh_alloc_timer <= now {
                    // Floored so the timer always moves. A zero step would leave it pinned to an
                    // instant already in the past — see `MIN_ALLOC_REFRESH_INTERVAL`.
                    let step = (relay.lifetime / 2).max(MIN_ALLOC_REFRESH_INTERVAL);
                    relay.refresh_alloc_timer = now.add(step);
                    Some(relay.lifetime)
                } else {
                    None
                };

                let refresh_perms_timer = if relay.refresh_perms_timer <= now {
                    relay.refresh_perms_timer = now.add(PERM_REFRESH_INTERVAL);
                    true
                } else {
                    false
                };

                (refresh_alloc_timer, refresh_perms_timer)
            } else {
                (None, false)
            };

        if let Some(lifetime) = refresh_alloc_timer {
            let _ = self.refresh_allocation(lifetime);
        }
        if refresh_perms_timer {
            let _ = self.refresh_permissions();
        }
    }

    /// Sends `p` to `peer_addr` through the relay.
    ///
    /// Uses ChannelData framing if a channel is bound for that peer, otherwise a Data indication.
    ///
    /// # Errors
    ///
    /// Fails if the allocation is gone, or if no permission exists for `peer_addr`.
    pub fn send_to(&mut self, p: &[u8], peer_addr: SocketAddr) -> Result<()> {
        // check if we have a permission for the destination IP addr
        let result = if let Some(relay) = self.client.relays.get_mut(&self.relayed_addr) {
            if let Some(perm) = relay.perm_map.get_mut(&peer_addr) {
                if perm.state() != PermState::Permitted {
                    Err(Error::ErrNoPermission)
                } else {
                    Ok((relay.integrity.clone(), relay.nonce.clone()))
                }
            } else {
                Err(Error::ErrNoPermission)
            }
        } else {
            Err(Error::ErrConnClosed)
        };

        let (integrity, nonce) = result?;

        self.send(p, peer_addr, integrity, nonce)
    }

    fn send(
        &mut self,
        p: &[u8],
        peer_addr: SocketAddr,
        integrity: MessageIntegrity,
        nonce: Nonce,
    ) -> Result<()> {
        let channel_number = {
            let (bind_st, bind_at, bind_number, bind_addr) = {
                let b = if let Some(b) = self.client.binding_mgr.find_by_addr(&peer_addr) {
                    b
                } else {
                    self.client
                        .binding_mgr
                        .create(peer_addr)
                        .ok_or_else(|| Error::Other("Addr not found".to_owned()))?
                };
                (b.state(), b.refreshed_at(), b.number, b.addr)
            };

            if bind_st == BindingState::Idle
                || bind_st == BindingState::Request
                || bind_st == BindingState::Failed
            {
                // block only callers with the same binding until
                // the binding transaction has been complete
                // binding state may have been changed while waiting. check again.
                if bind_st == BindingState::Idle {
                    if let Some(b) = self.client.binding_mgr.get_by_addr(&bind_addr) {
                        b.set_state(BindingState::Request);
                    }
                    self.channel_bind(self.relayed_addr, bind_addr, bind_number, nonce, integrity)?;
                }

                // send data using SendIndication
                let mut msg = Message::new();
                msg.build(&[
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(METHOD_SEND, CLASS_INDICATION)),
                    Box::new(proto::data::Data(p.to_vec())),
                    Box::new(proto::peeraddr::PeerAddress {
                        ip: peer_addr.ip(),
                        port: peer_addr.port(),
                    }),
                    Box::new(FINGERPRINT),
                ])?;

                // indication has no transaction (fire-and-forget)
                self.client
                    .write_to(&msg.raw, self.client.turn_server_addr()?);
                return Ok(());
            }

            // binding is ready
            // check if the binding needs a refresh
            if bind_st == BindingState::Ready
                && Instant::now()
                    .checked_duration_since(bind_at)
                    .unwrap_or_else(|| Duration::from_secs(0))
                    > PERM_LIFETIME
            {
                if let Some(b) = self.client.binding_mgr.get_by_addr(&bind_addr) {
                    b.set_state(BindingState::Refresh);
                }
                self.channel_bind(self.relayed_addr, bind_addr, bind_number, nonce, integrity)?;
            }

            bind_number
        };

        // send via ChannelData
        self.send_channel_data(p, channel_number)
    }

    // Close closes the connection.
    // Any blocked ReadFrom or write_to operations will be unblocked and return errors.
    /// Releases the allocation by refreshing it with a zero lifetime.
    ///
    /// # Errors
    ///
    /// Fails if the refresh request cannot be sent.
    pub fn close(&mut self) -> Result<()> {
        self.refresh_allocation(Duration::from_secs(0))
    }

    fn create_permissions(
        &mut self,
        peer_addrs: &[SocketAddr],
        peer_addr_opt: Option<SocketAddr>,
    ) -> Result<TransactionId> {
        let (username, realm) = (self.client.username(), self.client.realm());
        if let Some(relay) = self.client.relays.get_mut(&self.relayed_addr) {
            let msg = {
                let mut setters: Vec<Box<dyn Setter>> = vec![
                    Box::new(TransactionId::new()),
                    Box::new(MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)),
                ];

                for addr in peer_addrs {
                    setters.push(Box::new(proto::peeraddr::PeerAddress {
                        ip: addr.ip(),
                        port: addr.port(),
                    }));
                }

                setters.push(Box::new(username));
                setters.push(Box::new(realm));
                setters.push(Box::new(relay.nonce.clone()));
                setters.push(Box::new(relay.integrity.clone()));
                setters.push(Box::new(FINGERPRINT));

                let mut msg = Message::new();
                msg.build(&setters)?;
                msg
            };

            Ok(self.client.perform_transaction(
                &msg,
                self.client.turn_server_addr()?,
                TransactionType::CreatePermissionRequest(self.relayed_addr, peer_addr_opt),
            ))
        } else {
            Err(Error::ErrConnClosed)
        }
    }

    pub(super) fn handle_create_permission_response(
        &mut self,
        res: Message,
        peer_addr_opt: Option<SocketAddr>,
    ) -> Result<()> {
        let mut retry_refresh = false;
        let result = if let Some(relay) = self.client.relays.get_mut(&self.relayed_addr) {
            if res.typ.class == CLASS_ERROR_RESPONSE {
                let mut code = ErrorCodeAttribute::default();
                let result = code.get_from(&res);
                let err = if result.is_err() {
                    Error::Other(format!("{}", res.typ))
                } else if code.code == CODE_STALE_NONCE {
                    // rcdesk patch (D75): a periodic refresh (no `peer_addr_opt`) rejected for a
                    // stale nonce is otherwise dropped, and the permissions lapse unless the next
                    // tick lands in time.
                    retry_refresh = relay.set_nonce_from_msg(&res) && peer_addr_opt.is_none();
                    Error::ErrTryAgain
                } else {
                    Error::Other(format!("{} (error {})", res.typ, code))
                };
                if let Some(peer_addr) = peer_addr_opt {
                    self.client
                        .events
                        .push_back(Event::CreatePermissionError(res.transaction_id, err));
                    relay.perm_map.remove(&peer_addr);
                }
            } else if let Some(peer_addr) = peer_addr_opt
                && let Some(perm) = relay.perm_map.get_mut(&peer_addr)
            {
                perm.set_state(PermState::Permitted);
                self.client
                    .events
                    .push_back(Event::CreatePermissionResponse(
                        res.transaction_id,
                        peer_addr,
                    ));
            }

            Ok(())
        } else {
            Err(Error::ErrConnClosed)
        };

        if retry_refresh {
            self.refresh_permissions()?;
        }

        result
    }

    pub(super) fn refresh_allocation(&mut self, lifetime: Duration) -> Result<()> {
        let (username, realm) = (self.client.username(), self.client.realm());
        if let Some(relay) = self.client.relays.get_mut(&self.relayed_addr) {
            let mut msg = Message::new();
            msg.build(&[
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_REFRESH, CLASS_REQUEST)),
                Box::new(proto::lifetime::Lifetime(lifetime)),
                Box::new(username),
                Box::new(realm),
                Box::new(relay.nonce.clone()),
                Box::new(relay.integrity.clone()),
                Box::new(FINGERPRINT),
            ])?;

            let _ = self.client.perform_transaction(
                &msg,
                self.client.turn_server_addr()?,
                TransactionType::RefreshRequest(self.relayed_addr),
            );

            Ok(())
        } else {
            Err(Error::ErrConnClosed)
        }
    }

    pub(super) fn handle_refresh_allocation_response(&mut self, res: Message) -> Result<()> {
        let mut released = false;
        let mut retry_lifetime = None;
        let result = if let Some(relay) = self.client.relays.get_mut(&self.relayed_addr) {
            if res.typ.class == CLASS_ERROR_RESPONSE {
                let mut code = ErrorCodeAttribute::default();
                let result = code.get_from(&res);
                if result.is_err() {
                    Err(Error::Other(format!("{}", res.typ)))
                } else if code.code == CODE_STALE_NONCE {
                    // rcdesk patch (D75): retry at once with the new nonce. Upstream waits for the
                    // next tick, half a lifetime later -- exactly when the allocation expires, so
                    // the refresh races the server's expiry and eventually loses (437).
                    if relay.set_nonce_from_msg(&res) {
                        retry_lifetime = Some(relay.lifetime);
                    }
                    Ok(())
                } else {
                    Err(Error::Other(format!("{} (error {})", res.typ, code)))
                }
            } else {
                // Getting lifetime from response
                let mut updated_lifetime = proto::lifetime::Lifetime::default();
                updated_lifetime.get_from(&res)?;

                relay.lifetime = updated_lifetime.0;
                debug!("updated lifetime: {} seconds", relay.lifetime.as_secs());

                // A zero lifetime is the server confirming deallocation — the reply to the
                // refresh `close()` sends. The allocation no longer exists, so neither should
                // the relay: leaving it in the map means it keeps reporting refresh deadlines
                // for something that is gone, and keeps trying to refresh it.
                released = relay.lifetime.is_zero();

                Ok(())
            }
        } else {
            Err(Error::ErrConnClosed)
        };

        if released {
            self.client.relays.remove(&self.relayed_addr);
            debug!("allocation released; relay {} dropped", self.relayed_addr);
        }
        if let Some(lifetime) = retry_lifetime {
            self.refresh_allocation(lifetime)?;
        }

        result
    }

    pub(super) fn refresh_permissions(&mut self) -> Result<()> {
        if let Some(relay) = self.client.relays.get_mut(&self.relayed_addr) {
            #[allow(clippy::map_clone)]
            let addrs: Vec<SocketAddr> = relay.perm_map.keys().map(|addr| *addr).collect();
            if addrs.is_empty() {
                debug!("no permission to refresh");
                return Ok(());
            }
            let _ = self.create_permissions(&addrs, None)?;
            Ok(())
        } else {
            Err(Error::ErrConnClosed)
        }
    }

    fn channel_bind(
        &mut self,
        relayed_addr: RelayedAddr,
        bind_addr: SocketAddr,
        bind_number: u16,
        nonce: Nonce,
        integrity: MessageIntegrity,
    ) -> Result<()> {
        let (msg, turn_server_addr) = {
            let setters: Vec<Box<dyn Setter>> = vec![
                Box::new(TransactionId::new()),
                Box::new(MessageType::new(METHOD_CHANNEL_BIND, CLASS_REQUEST)),
                Box::new(proto::peeraddr::PeerAddress {
                    ip: bind_addr.ip(),
                    port: bind_addr.port(),
                }),
                Box::new(proto::channum::ChannelNumber(bind_number)),
                Box::new(self.client.username()),
                Box::new(self.client.realm()),
                Box::new(nonce),
                Box::new(integrity),
                Box::new(FINGERPRINT),
            ];

            let mut msg = Message::new();
            msg.build(&setters)?;

            (msg, self.client.turn_server_addr()?)
        };

        debug!("UDPConn.bind call PerformTransaction 1");
        let _ = self.client.perform_transaction(
            &msg,
            turn_server_addr,
            TransactionType::ChannelBindRequest(relayed_addr, bind_addr),
        );

        Ok(())
    }

    pub(super) fn handle_channel_bind_response(
        &mut self,
        res: Message,
        bind_addr: SocketAddr,
    ) -> Result<()> {
        if let Some(relay) = self.client.relays.get_mut(&self.relayed_addr) {
            let result = if res.typ.class == CLASS_ERROR_RESPONSE {
                let mut code = ErrorCodeAttribute::default();
                let result = code.get_from(&res);
                if result.is_err() {
                    Err(Error::Other(format!("{}", res.typ)))
                } else if code.code == CODE_STALE_NONCE {
                    // rcdesk patch (D75): re-send the bind with the new nonce and keep the binding.
                    // Upstream deletes it, but on a refresh the server still holds the channel and
                    // keeps relaying the peer's data as ChannelData on that number, which we then
                    // drop as "no binding found for channel" -- the session dies 600 s in.
                    if relay.set_nonce_from_msg(&res) {
                        let (nonce, integrity) = (relay.nonce.clone(), relay.integrity.clone());
                        if let Some(number) = self
                            .client
                            .binding_mgr
                            .find_by_addr(&bind_addr)
                            .map(|b| b.number)
                        {
                            debug!("channel bind: 438, retrying with new nonce: {}", bind_addr);
                            return self.channel_bind(
                                self.relayed_addr,
                                bind_addr,
                                number,
                                nonce,
                                integrity,
                            );
                        }
                    }
                    Err(Error::ErrTryAgain)
                } else {
                    Err(Error::Other(format!("{} (error {})", res.typ, code)))
                }
            } else if res.typ != MessageType::new(METHOD_CHANNEL_BIND, CLASS_SUCCESS_RESPONSE) {
                Err(Error::ErrUnexpectedResponse)
            } else {
                Ok(())
            };

            let refreshing = self
                .client
                .binding_mgr
                .find_by_addr(&bind_addr)
                .is_some_and(|b| b.state() == BindingState::Refresh);
            if let Err(err) = &result
                && refreshing
            {
                // rcdesk patch (D75): a failed *refresh* keeps the binding. The server still has
                // the channel (bound at most `PERM_LIFETIME` ago, it lives 600 s) and relays the
                // peer on that number; deleting it here is what turned one rejected refresh into
                // "no binding found for channel" and a dead session. Retry from the next send
                // after `BIND_REFRESH_RETRY`, not on every packet.
                if let Some(b) = self.client.binding_mgr.get_by_addr(&bind_addr) {
                    b.set_refreshed_at(Instant::now() - (PERM_LIFETIME - BIND_REFRESH_RETRY));
                    b.set_state(BindingState::Ready);
                }
                warn!("bind() refresh failed, binding kept: {}", err);
            } else if let Err(err) = result {
                if Error::ErrUnexpectedResponse != err {
                    self.client.binding_mgr.delete_by_addr(&bind_addr);
                } else if let Some(b) = self.client.binding_mgr.get_by_addr(&bind_addr) {
                    b.set_state(BindingState::Failed);
                }

                // keep going...
                warn!("bind() failed: {}", err);
            } else if let Some(b) = self.client.binding_mgr.get_by_addr(&bind_addr) {
                b.set_refreshed_at(Instant::now());
                b.set_state(BindingState::Ready);
                debug!("channel binding successful: {}", bind_addr);
            }
            Ok(())
        } else {
            Err(Error::ErrConnClosed)
        }
    }

    fn send_channel_data(&mut self, data: &[u8], channel_number: u16) -> Result<()> {
        let mut ch_data = proto::chandata::ChannelData {
            data: data.to_vec(),
            number: proto::channum::ChannelNumber(channel_number),
            ..Default::default()
        };
        ch_data.encode();

        self.client
            .write_to(&ch_data.raw, self.client.turn_server_addr()?);

        Ok(())
    }
}

/// rcdesk patch (D75): a 438 (Stale Nonce) is retried with the new nonce instead of being
/// dropped, and a channel binding survives it. See `vendor/rtc-turn/RCDESK-PATCH.md`.
#[cfg(test)]
mod rcdesk_patch_tests {
    use super::*;
    use crate::client::ClientConfig;
    use crate::proto::channum::ChannelNumber;
    use shared::TransportProtocol;

    const SERVER: &str = "127.0.0.1:3478";

    fn relayed() -> SocketAddr {
        "198.51.100.1:50000".parse().unwrap()
    }

    fn peer() -> SocketAddr {
        "203.0.113.7:40000".parse().unwrap()
    }

    fn server() -> SocketAddr {
        SERVER.parse().unwrap()
    }

    fn client_with_relay() -> Client {
        let mut client = Client::new(ClientConfig {
            stun_serv_addr: String::new(),
            turn_serv_addr: SERVER.to_owned(),
            local_addr: "127.0.0.1:40001".parse().unwrap(),
            transport_protocol: TransportProtocol::UDP,
            username: "user".to_owned(),
            password: "pass".to_owned(),
            realm: "rcdesk.app".to_owned(),
            software: String::new(),
            rto_in_ms: 0,
        })
        .unwrap();
        let mut relay = RelayState::new(
            relayed(),
            MessageIntegrity::new_short_term_integrity("pass".to_owned()),
            Nonce::new(ATTR_NONCE, "n0".to_owned()),
            Duration::from_secs(600),
        );
        let mut perm = Permission::default();
        perm.set_state(PermState::Permitted);
        relay.perm_map.insert(peer(), perm);
        client.relays.insert(relayed(), relay);
        client
    }

    /// Every STUN request the client has queued, decoded; non-STUN (ChannelData) is skipped.
    fn requests(client: &mut Client) -> Vec<Message> {
        let mut out = vec![];
        while let Some(t) = client.transmits.pop_front() {
            let mut m = Message::new();
            m.raw = t.message.to_vec();
            if m.decode().is_ok() && m.typ.class == CLASS_REQUEST {
                out.push(m);
            }
        }
        out
    }

    fn only(mut msgs: Vec<Message>, method: Method) -> Message {
        msgs.retain(|m| m.typ.method == method);
        assert_eq!(msgs.len(), 1, "expected one {method} request");
        msgs.pop().unwrap()
    }

    fn nonce_of(m: &Message) -> String {
        Nonce::get_from_as(m, ATTR_NONCE).unwrap().text
    }

    fn answer(
        client: &mut Client,
        req: &Message,
        class: MessageClass,
        extra: Vec<Box<dyn Setter>>,
    ) {
        let mut setters: Vec<Box<dyn Setter>> = vec![
            Box::new(req.transaction_id),
            Box::new(MessageType::new(req.typ.method, class)),
        ];
        setters.extend(extra);
        let mut res = Message::new();
        res.build(&setters).unwrap();
        // Ok or the method's own error: both mean "handled", which is all a test needs here.
        let _ = client.handle_inbound(&res.raw, server());
    }

    fn stale_nonce(client: &mut Client, req: &Message, nonce: &str) {
        answer(
            client,
            req,
            CLASS_ERROR_RESPONSE,
            vec![
                Box::new(ErrorCodeAttribute {
                    code: CODE_STALE_NONCE,
                    reason: b"Stale Nonce".to_vec(),
                }),
                Box::new(Nonce::new(ATTR_NONCE, nonce.to_owned())),
            ],
        );
    }

    fn send(client: &mut Client) {
        client
            .relay(relayed())
            .unwrap()
            .send_to(b"x", peer())
            .unwrap();
    }

    #[test]
    fn channel_binding_survives_stale_nonce_on_refresh() {
        let mut client = client_with_relay();

        send(&mut client);
        let bind = only(requests(&mut client), METHOD_CHANNEL_BIND);
        answer(&mut client, &bind, CLASS_SUCCESS_RESPONSE, vec![]);
        let number = client.binding_mgr.find_by_addr(&peer()).unwrap().number;

        // Past the refresh threshold: the next send re-binds, and the server says 438.
        client
            .binding_mgr
            .get_by_addr(&peer())
            .unwrap()
            .set_refreshed_at(Instant::now() - Duration::from_secs(301));
        send(&mut client);
        let refresh = only(requests(&mut client), METHOD_CHANNEL_BIND);
        assert_eq!(nonce_of(&refresh), "n0");
        stale_nonce(&mut client, &refresh, "n1");

        // The binding is kept (upstream deleted it, so the server's ChannelData on this number
        // hit "no binding found for channel") and re-sent at once with the new nonce.
        let kept = client
            .binding_mgr
            .find_by_addr(&peer())
            .expect("binding kept");
        assert_eq!(kept.number, number);
        let retry = only(requests(&mut client), METHOD_CHANNEL_BIND);
        assert_eq!(nonce_of(&retry), "n1");
        let mut ch = ChannelNumber::default();
        ch.get_from(&retry).unwrap();
        assert_eq!(ch.0, number);

        answer(&mut client, &retry, CLASS_SUCCESS_RESPONSE, vec![]);
        assert_eq!(
            client.binding_mgr.find_by_addr(&peer()).unwrap().state(),
            BindingState::Ready
        );

        // Inbound ChannelData on that number still reaches the caller.
        let mut data = proto::chandata::ChannelData {
            data: b"pong".to_vec(),
            number: ChannelNumber(number),
            ..Default::default()
        };
        data.encode();
        client.handle_inbound(&data.raw, server()).unwrap();
        assert!(matches!(
            client.events.pop_back(),
            Some(Event::DataIndicationOrChannelData(_, from, _)) if from == peer()
        ));
    }

    #[test]
    fn failed_channel_binding_refresh_keeps_binding_and_retries_later() {
        let mut client = client_with_relay();

        send(&mut client);
        let bind = only(requests(&mut client), METHOD_CHANNEL_BIND);
        answer(&mut client, &bind, CLASS_SUCCESS_RESPONSE, vec![]);
        let number = client.binding_mgr.find_by_addr(&peer()).unwrap().number;

        client
            .binding_mgr
            .get_by_addr(&peer())
            .unwrap()
            .set_refreshed_at(Instant::now() - Duration::from_secs(301));
        send(&mut client);
        let refresh = only(requests(&mut client), METHOD_CHANNEL_BIND);
        answer(
            &mut client,
            &refresh,
            CLASS_ERROR_RESPONSE,
            vec![Box::new(ErrorCodeAttribute {
                code: CODE_BAD_REQUEST,
                reason: b"Bad Request".to_vec(),
            })],
        );

        let kept = client
            .binding_mgr
            .find_by_addr(&peer())
            .expect("binding kept");
        assert_eq!((kept.number, kept.state()), (number, BindingState::Ready));
        // Not re-sent on the very next packet...
        send(&mut client);
        assert!(requests(&mut client).is_empty());
        // ...but once the retry delay has passed.
        client
            .binding_mgr
            .get_by_addr(&peer())
            .unwrap()
            .set_refreshed_at(Instant::now() - Duration::from_secs(301));
        send(&mut client);
        only(requests(&mut client), METHOD_CHANNEL_BIND);
    }

    #[test]
    fn allocation_refresh_is_retried_once_per_new_nonce() {
        let mut client = client_with_relay();

        client
            .relay(relayed())
            .unwrap()
            .refresh_allocation(Duration::from_secs(600))
            .unwrap();
        let first = only(requests(&mut client), METHOD_REFRESH);
        stale_nonce(&mut client, &first, "n1");

        let retry = only(requests(&mut client), METHOD_REFRESH);
        assert_eq!(nonce_of(&retry), "n1");
        let mut lifetime = proto::lifetime::Lifetime::default();
        lifetime.get_from(&retry).unwrap();
        assert_eq!(lifetime.0, Duration::from_secs(600));

        // The same nonce again is not a reason to retry: no loop against a confused server.
        stale_nonce(&mut client, &retry, "n1");
        assert!(requests(&mut client).is_empty());
    }

    #[test]
    fn permission_refresh_is_retried_on_stale_nonce() {
        let mut client = client_with_relay();

        client
            .relay(relayed())
            .unwrap()
            .refresh_permissions()
            .unwrap();
        let first = only(requests(&mut client), METHOD_CREATE_PERMISSION);
        stale_nonce(&mut client, &first, "n1");

        let retry = only(requests(&mut client), METHOD_CREATE_PERMISSION);
        assert_eq!(nonce_of(&retry), "n1");
        assert_eq!(
            client.relays[&relayed()].perm_map[&peer()].state(),
            PermState::Permitted,
            "a refresh does not drop the permission it renews"
        );
    }
}
