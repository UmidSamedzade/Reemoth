//! Sans-IO TURN client (RFC 8656), UDP only.
//!
//! Gets one relayed address and keeps it alive, keeps a permission for each
//! peer IP we talk to, and wraps/unwraps Send and Data indications. The owner
//! moves bytes between this and a UDP socket connected to the TURN server.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use crate::stun::{
    self, Class, Message, TransactionId, attr, long_term_key, method, new_transaction_id,
};

const SOFTWARE: &[u8] = b"reemoth";
const UDP_TRANSPORT: [u8; 4] = [17, 0, 0, 0];
const INITIAL_RTO: Duration = Duration::from_millis(500);
const MAX_ATTEMPTS: u32 = 6;
/// Permissions expire after 300 s; refresh well before.
const PERMISSION_REFRESH: Duration = Duration::from_secs(240);
const REFRESH_MARGIN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvent {
    Allocated {
        relay: SocketAddr,
        mapped: Option<SocketAddr>,
    },
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Allocate,
    Refresh,
    Permission(IpAddr),
}

struct Pending {
    kind: Kind,
    /// Unsigned request; credentials are added on every (re)send.
    msg: Message,
    next_send: Instant,
    rto: Duration,
    attempts: u32,
    /// Sent with credentials, so a 401 means they are wrong.
    authed: bool,
    /// Already retried once after 438 Stale Nonce.
    stale_retried: bool,
}

pub struct TurnClient {
    server: SocketAddr,
    username: String,
    password: String,
    realm: Option<String>,
    nonce: Option<String>,
    key: Option<Vec<u8>>,
    relay: Option<SocketAddr>,
    failed: bool,
    allocation_expires: Option<Instant>,
    pending: HashMap<TransactionId, Pending>,
    /// Peer IP → when to refresh its permission.
    permissions: HashMap<IpAddr, Instant>,
    transmits: VecDeque<Vec<u8>>,
    events: VecDeque<TurnEvent>,
}

impl TurnClient {
    pub fn new(server: SocketAddr, username: String, password: String) -> Self {
        Self {
            server,
            username,
            password,
            realm: None,
            nonce: None,
            key: None,
            relay: None,
            failed: false,
            allocation_expires: None,
            pending: HashMap::new(),
            permissions: HashMap::new(),
            transmits: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    pub fn server(&self) -> SocketAddr {
        self.server
    }

    pub fn relay_addr(&self) -> Option<SocketAddr> {
        self.relay
    }

    /// Queues the Allocate request.
    pub fn start(&mut self, now: Instant) {
        let msg = Message::new(Class::Request, method::ALLOCATE, new_transaction_id())
            .with(attr::REQUESTED_TRANSPORT, UDP_TRANSPORT.to_vec());
        self.request(Kind::Allocate, msg, now, false);
    }

    /// Releases the allocation (best effort, no retransmits).
    pub fn close(&mut self) -> Option<Vec<u8>> {
        self.relay?;
        let msg = Message::new(Class::Request, method::REFRESH, new_transaction_id())
            .with(attr::LIFETIME, 0u32.to_be_bytes().to_vec());
        Some(self.sign(msg))
    }

    /// Wraps `data` for `peer` in a Send indication, requesting a permission
    /// first if needed (the first packets to a new IP may be dropped until it
    /// is installed; ICE retransmits cover that).
    pub fn send_to(&mut self, peer: SocketAddr, data: &[u8], now: Instant) -> Option<Vec<u8>> {
        self.relay?;
        self.ensure_permission(peer.ip(), now);
        let msg = Message::new(Class::Indication, method::SEND, new_transaction_id())
            .with_xor_addr(attr::XOR_PEER_ADDRESS, peer)
            .with(attr::DATA, data.to_vec());
        Some(msg.encode(None, false))
    }

    pub fn ensure_permission(&mut self, ip: IpAddr, now: Instant) {
        if self.relay.is_none() || self.permissions.contains_key(&ip) {
            return;
        }
        self.permissions.insert(ip, now + PERMISSION_REFRESH);
        self.request_permission(ip, now);
    }

    /// Handles a datagram from the TURN server. Returns relayed peer data.
    pub fn handle_packet(&mut self, buf: &[u8], now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        let msg = Message::decode(buf).ok()?;
        if msg.class == Class::Indication && msg.method == method::DATA {
            let peer = msg.xor_addr(attr::XOR_PEER_ADDRESS)?;
            return Some((peer, msg.get(attr::DATA)?.to_vec()));
        }
        let pending = self.pending.remove(&msg.transaction_id)?;
        // Responses are authenticated once we have a key.
        if let Some(key) = &self.key
            && msg.get(attr::MESSAGE_INTEGRITY).is_some()
            && !stun::verify_integrity(buf, key)
        {
            tracing::warn!("dropping TURN response with bad integrity");
            self.pending.insert(msg.transaction_id, pending);
            return None;
        }
        match msg.class {
            Class::Success => self.on_success(pending.kind, &msg, now),
            Class::Error => self.on_error(pending, &msg, now),
            _ => {}
        }
        None
    }

    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.transmits.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<TurnEvent> {
        self.events.pop_front()
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        let retransmit = self.pending.values().map(|p| p.next_send);
        let permissions = self.permissions.values().copied();
        let refresh = self.allocation_expires.map(|t| t - REFRESH_MARGIN);
        retransmit.chain(permissions).chain(refresh).min()
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        let mut resend = Vec::new();
        let mut give_up = Vec::new();
        for (txid, p) in &mut self.pending {
            if p.next_send > now {
                continue;
            }
            if p.attempts >= MAX_ATTEMPTS {
                give_up.push(*txid);
                continue;
            }
            p.attempts += 1;
            p.rto *= 2;
            p.next_send = now + p.rto;
            resend.push(p.msg.clone());
        }
        for msg in resend {
            let wire = self.sign(msg);
            self.transmits.push_back(wire);
        }
        for txid in give_up {
            if let Some(p) = self.pending.remove(&txid) {
                self.fail_request(p.kind, "TURN server did not respond");
            }
        }

        let due: Vec<IpAddr> = self
            .permissions
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(ip, _)| *ip)
            .collect();
        for ip in due {
            self.permissions.insert(ip, now + PERMISSION_REFRESH);
            self.request_permission(ip, now);
        }

        if let Some(expires) = self.allocation_expires
            && expires - REFRESH_MARGIN <= now
        {
            self.allocation_expires = None;
            let msg = Message::new(Class::Request, method::REFRESH, new_transaction_id());
            self.request(Kind::Refresh, msg, now, false);
        }
    }

    fn request_permission(&mut self, ip: IpAddr, now: Instant) {
        let msg = Message::new(
            Class::Request,
            method::CREATE_PERMISSION,
            new_transaction_id(),
        )
        .with_xor_addr(attr::XOR_PEER_ADDRESS, SocketAddr::new(ip, 0));
        self.request(Kind::Permission(ip), msg, now, false);
    }

    fn request(&mut self, kind: Kind, msg: Message, now: Instant, stale_retried: bool) {
        let wire = self.sign(msg.clone());
        let authed = self.key.is_some();
        self.transmits.push_back(wire);
        self.pending.insert(
            msg.transaction_id,
            Pending {
                kind,
                msg,
                next_send: now + INITIAL_RTO,
                rto: INITIAL_RTO,
                attempts: 1,
                authed,
                stale_retried,
            },
        );
    }

    /// Adds SOFTWARE and, once the server told us realm/nonce, long-term credentials.
    fn sign(&self, msg: Message) -> Vec<u8> {
        let mut msg = msg.with(attr::SOFTWARE, SOFTWARE.to_vec());
        match (&self.key, &self.realm, &self.nonce) {
            (Some(key), Some(realm), Some(nonce)) => {
                msg = msg
                    .with(attr::USERNAME, self.username.as_bytes().to_vec())
                    .with(attr::REALM, realm.as_bytes().to_vec())
                    .with(attr::NONCE, nonce.as_bytes().to_vec());
                msg.encode(Some(key), true)
            }
            _ => msg.encode(None, true),
        }
    }

    fn on_success(&mut self, kind: Kind, msg: &Message, now: Instant) {
        match kind {
            Kind::Allocate => {
                let Some(relay) = msg.xor_addr(attr::XOR_RELAYED_ADDRESS) else {
                    self.fail_request(kind, "allocation response had no relayed address");
                    return;
                };
                let lifetime = msg.lifetime().unwrap_or(600);
                self.relay = Some(relay);
                self.allocation_expires = Some(now + Duration::from_secs(lifetime.into()));
                self.events.push_back(TurnEvent::Allocated {
                    relay,
                    mapped: msg.xor_addr(attr::XOR_MAPPED_ADDRESS),
                });
            }
            Kind::Refresh => {
                let lifetime = msg.lifetime().unwrap_or(600);
                self.allocation_expires = Some(now + Duration::from_secs(lifetime.into()));
            }
            Kind::Permission(_) => {}
        }
    }

    fn on_error(&mut self, pending: Pending, msg: &Message, now: Instant) {
        let (code, reason) = msg.error_code().unwrap_or((0, String::new()));
        // 401: first contact, learn realm/nonce. 438: nonce went stale.
        let retry_with_creds = match code {
            401 => !pending.authed,
            438 => !pending.stale_retried,
            _ => false,
        };
        if retry_with_creds
            && let (Some(realm), Some(nonce)) = (msg.get_str(attr::REALM), msg.get_str(attr::NONCE))
        {
            self.key = Some(long_term_key(&self.username, realm, &self.password));
            self.realm = Some(realm.to_owned());
            self.nonce = Some(nonce.to_owned());
            // Fresh transaction ID for the retried request.
            let mut retry = pending.msg;
            retry.transaction_id = new_transaction_id();
            self.request(pending.kind, retry, now, code == 438);
            return;
        }
        self.fail_request(pending.kind, &format!("TURN error {code} {reason}"));
    }

    fn fail_request(&mut self, kind: Kind, why: &str) {
        match kind {
            Kind::Allocate | Kind::Refresh => {
                if !self.failed {
                    self.failed = true;
                    self.relay = None;
                    self.events.push_back(TurnEvent::Failed(why.to_owned()));
                }
            }
            Kind::Permission(ip) => {
                tracing::warn!("TURN permission for {ip} failed: {why}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays a TURN server against the client.
    struct FakeServer {
        key: Vec<u8>,
    }

    impl FakeServer {
        fn new() -> Self {
            Self {
                key: long_term_key("user", "example.org", "pass"),
            }
        }

        fn respond(&self, req: &[u8]) -> Vec<u8> {
            let msg = Message::decode(req).unwrap();
            assert!(stun::verify_fingerprint(req));
            if msg.get(attr::MESSAGE_INTEGRITY).is_none() {
                let mut e = Message::new(Class::Error, msg.method, msg.transaction_id)
                    .with(attr::ERROR_CODE, vec![0, 0, 4, 1])
                    .with(attr::REALM, b"example.org".to_vec())
                    .with(attr::NONCE, b"n0nce".to_vec());
                e.attrs.push((attr::SOFTWARE, b"fake".to_vec()));
                return e.encode(None, false);
            }
            assert!(stun::verify_integrity(req, &self.key));
            assert_eq!(msg.get_str(attr::USERNAME), Some("user"));
            let mut ok = Message::new(Class::Success, msg.method, msg.transaction_id);
            if msg.method == method::ALLOCATE {
                ok = ok
                    .with_xor_addr(
                        attr::XOR_RELAYED_ADDRESS,
                        "198.51.100.7:50000".parse().unwrap(),
                    )
                    .with_xor_addr(
                        attr::XOR_MAPPED_ADDRESS,
                        "203.0.113.1:6000".parse().unwrap(),
                    )
                    .with(attr::LIFETIME, 600u32.to_be_bytes().to_vec());
            }
            ok.encode(Some(&self.key), true)
        }
    }

    fn client() -> TurnClient {
        TurnClient::new(
            "192.0.2.10:3478".parse().unwrap(),
            "user".into(),
            "pass".into(),
        )
    }

    #[test]
    fn allocates_after_401_challenge() {
        let server = FakeServer::new();
        let mut c = client();
        let t = Instant::now();
        c.start(t);
        let first = c.poll_transmit().unwrap();
        assert!(c.handle_packet(&server.respond(&first), t).is_none());
        let second = c.poll_transmit().expect("retried with credentials");
        c.handle_packet(&server.respond(&second), t);
        assert_eq!(
            c.poll_event(),
            Some(TurnEvent::Allocated {
                relay: "198.51.100.7:50000".parse().unwrap(),
                mapped: Some("203.0.113.1:6000".parse().unwrap()),
            })
        );
        assert!(c.pending.is_empty());
    }

    #[test]
    fn send_requests_permission_and_wraps_data() {
        let server = FakeServer::new();
        let mut c = client();
        let t = Instant::now();
        c.start(t);
        let r = c.poll_transmit().unwrap();
        c.handle_packet(&server.respond(&r), t);
        let r = c.poll_transmit().unwrap();
        c.handle_packet(&server.respond(&r), t);

        let peer: SocketAddr = "203.0.113.50:7000".parse().unwrap();
        let wire = c.send_to(peer, b"hello", t).unwrap();
        let ind = Message::decode(&wire).unwrap();
        assert_eq!((ind.class, ind.method), (Class::Indication, method::SEND));
        assert_eq!(ind.xor_addr(attr::XOR_PEER_ADDRESS), Some(peer));
        assert_eq!(ind.get(attr::DATA), Some(&b"hello"[..]));

        let perm = c.poll_transmit().expect("permission requested");
        let perm_msg = Message::decode(&perm).unwrap();
        assert_eq!(perm_msg.method, method::CREATE_PERMISSION);
        c.handle_packet(&server.respond(&perm), t);
        // No second permission request for the same IP.
        c.send_to(peer, b"again", t).unwrap();
        assert!(c.poll_transmit().is_none());

        let data = Message::new(Class::Indication, method::DATA, new_transaction_id())
            .with_xor_addr(attr::XOR_PEER_ADDRESS, peer)
            .with(attr::DATA, b"reply".to_vec())
            .encode(None, false);
        assert_eq!(c.handle_packet(&data, t), Some((peer, b"reply".to_vec())));
    }

    #[test]
    fn gives_up_when_server_is_silent() {
        let mut c = client();
        let mut t = Instant::now();
        c.start(t);
        for _ in 0..20 {
            while c.poll_transmit().is_some() {}
            let Some(next) = c.poll_timeout() else { break };
            t = next;
            c.handle_timeout(t);
        }
        assert!(c.pending.is_empty());
        assert!(matches!(c.poll_event(), Some(TurnEvent::Failed(_))));
    }
}
