//! ICE candidate gathering: bind sockets, ask STUN for our public address,
//! allocate a TURN relay. Runs before the SDP exchange (no trickle ICE): a
//! STUN round trip takes tens of milliseconds, so waiting costs little and
//! keeps signaling to one offer and one answer.

use std::net::{IpAddr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reemoth_proto::signal::IceServer;
use str0m::Candidate;
use tokio::net::UdpSocket;

use crate::stun::{self, Class, Message, attr, method, new_transaction_id};
use crate::turn::{TurnClient, TurnEvent};

const STUN_DEADLINE: Duration = Duration::from_millis(1500);
const TURN_DEADLINE: Duration = Duration::from_secs(3);
const STUN_RETRANSMIT: Duration = Duration::from_millis(300);

/// Sockets and candidates ready to hand to a session.
pub struct Gathered {
    pub(crate) v4: Option<(Arc<UdpSocket>, SocketAddr)>,
    pub(crate) v6: Option<(Arc<UdpSocket>, SocketAddr)>,
    pub(crate) turn: Option<TurnClient>,
    pub candidates: Vec<Candidate>,
}

impl Gathered {
    /// One line per candidate, for logs and the CLI.
    pub fn describe(&self) -> Vec<String> {
        self.candidates
            .iter()
            .map(|c| format!("{:?} {}", c.kind(), c.addr()))
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct GatherOptions {
    /// Offer only the TURN relay candidate (to test the relay path).
    pub relay_only: bool,
}

pub async fn gather(servers: &[IceServer], opts: &GatherOptions) -> anyhow::Result<Gathered> {
    let v4 = bind(default_route_ip(false)).await;
    let v6 = bind(default_route_ip(true)).await;
    if v4.is_none() && v6.is_none() {
        anyhow::bail!("no usable network interface (no default route)");
    }

    let mut stun_targets = Vec::new();
    let mut turn_target = None;
    for server in servers {
        for url in &server.urls {
            let Some(parsed) = parse_ice_url(url) else {
                continue;
            };
            match parsed.scheme {
                Scheme::Stun => stun_targets.extend(resolve(&parsed.host, parsed.port).await),
                Scheme::Turn if parsed.udp && turn_target.is_none() => {
                    if let (Some(user), Some(pass)) = (&server.username, &server.credential)
                        && let Some(addr) = resolve(&parsed.host, parsed.port)
                            .await
                            .into_iter()
                            .find(SocketAddr::is_ipv4)
                    {
                        turn_target = Some(TurnClient::new(addr, user.clone(), pass.clone()));
                    }
                }
                Scheme::Turn => {}
            }
        }
    }

    let (mut v4_srflx, mut v6_srflx, turn) = discover(&v4, &v6, stun_targets, turn_target).await;

    let mut candidates = Vec::new();
    let relay = turn.as_ref().and_then(TurnClient::relay_addr);
    if !opts.relay_only {
        for (sock, srflx) in [(&v4, &mut v4_srflx), (&v6, &mut v6_srflx)] {
            let Some((_, local)) = sock else { continue };
            candidates.extend(Candidate::host(*local, "udp").ok());
            if let Some(mapped) = srflx.take()
                && mapped != *local
            {
                candidates.extend(Candidate::server_reflexive(mapped, *local, "udp").ok());
            }
        }
    }
    if let (Some(relay), Some((_, local))) = (relay, &v4) {
        candidates.extend(Candidate::relayed(relay, *local, "udp").ok());
    }
    if candidates.is_empty() {
        anyhow::bail!("no ICE candidates (relay-only requested but no TURN relay available)");
    }
    Ok(Gathered {
        v4,
        v6,
        turn: turn.filter(|t| t.relay_addr().is_some()),
        candidates,
    })
}

/// Runs STUN binding requests and the TURN allocation concurrently on the
/// gathering sockets. Returns (v4 mapped, v6 mapped, turn client).
async fn discover(
    v4: &Option<(Arc<UdpSocket>, SocketAddr)>,
    v6: &Option<(Arc<UdpSocket>, SocketAddr)>,
    stun_targets: Vec<SocketAddr>,
    mut turn: Option<TurnClient>,
) -> (Option<SocketAddr>, Option<SocketAddr>, Option<TurnClient>) {
    let start = Instant::now();
    let mut v4_mapped = None;
    let mut v6_mapped = None;
    // One binding request per target, re-sent until answered.
    let mut requests: Vec<(SocketAddr, stun::TransactionId)> = stun_targets
        .iter()
        .filter(|t| {
            if t.is_ipv4() {
                v4.is_some()
            } else {
                v6.is_some()
            }
        })
        .map(|t| (*t, new_transaction_id()))
        .collect();
    let mut next_stun_send = start;
    let mut turn_done = turn.is_none();
    if let Some(t) = &mut turn {
        t.start(start);
    }

    let mut buf4 = vec![0u8; 2048];
    let mut buf6 = vec![0u8; 2048];
    loop {
        let now = Instant::now();
        let stun_done = requests.is_empty() || now >= start + STUN_DEADLINE;
        if stun_done && (turn_done || now >= start + TURN_DEADLINE) {
            break;
        }

        if !stun_done && now >= next_stun_send {
            for (target, txid) in &requests {
                let req = Message::new(Class::Request, method::BINDING, *txid).encode(None, true);
                let sock = if target.is_ipv4() { v4 } else { v6 };
                if let Some((s, _)) = sock {
                    let _ = s.send_to(&req, target).await;
                }
            }
            next_stun_send = now + STUN_RETRANSMIT;
        }
        if let (Some(t), Some((s, _))) = (&mut turn, v4) {
            t.handle_timeout(now);
            while let Some(pkt) = t.poll_transmit() {
                let _ = s.send_to(&pkt, t.server()).await;
            }
        }

        let mut wake = start + TURN_DEADLINE;
        if !stun_done {
            wake = wake.min(next_stun_send).min(start + STUN_DEADLINE);
        }
        if let Some(t) = turn
            .as_ref()
            .filter(|_| !turn_done)
            .and_then(TurnClient::poll_timeout)
        {
            wake = wake.min(t);
        }

        let (n, from, family_v4) = tokio::select! {
            r = recv(v4, &mut buf4) => match r { Some((n, from)) => (n, from, true), None => continue },
            r = recv(v6, &mut buf6) => match r { Some((n, from)) => (n, from, false), None => continue },
            _ = tokio::time::sleep_until(wake.into()) => continue,
        };
        let pkt = if family_v4 { &buf4[..n] } else { &buf6[..n] };

        if let Some(t) = turn.as_mut().filter(|t| family_v4 && from == t.server()) {
            t.handle_packet(pkt, Instant::now());
            while let Some(ev) = t.poll_event() {
                match ev {
                    TurnEvent::Allocated { relay, mapped } => {
                        tracing::debug!("TURN relay {relay} (mapped {mapped:?})");
                        v4_mapped = v4_mapped.or(mapped);
                        turn_done = true;
                    }
                    TurnEvent::Failed(why) => {
                        tracing::warn!("TURN allocation failed: {why}");
                        turn_done = true;
                    }
                }
            }
            continue;
        }

        let Ok(msg) = Message::decode(pkt) else {
            continue;
        };
        if msg.class != Class::Success || msg.method != method::BINDING {
            continue;
        }
        let Some(pos) = requests
            .iter()
            .position(|(_, id)| *id == msg.transaction_id)
        else {
            continue;
        };
        requests.swap_remove(pos);
        if let Some(mapped) = msg.xor_addr(attr::XOR_MAPPED_ADDRESS) {
            let slot = if mapped.is_ipv4() {
                &mut v4_mapped
            } else {
                &mut v6_mapped
            };
            *slot = slot.or(Some(mapped));
        }
        // One answer per family is enough.
        requests.retain(|(t, _)| {
            if t.is_ipv4() {
                v4_mapped.is_none()
            } else {
                v6_mapped.is_none()
            }
        });
    }
    (v4_mapped, v6_mapped, turn)
}

async fn recv(
    sock: &Option<(Arc<UdpSocket>, SocketAddr)>,
    buf: &mut [u8],
) -> Option<(usize, SocketAddr)> {
    match sock {
        // Windows reports ICMP port-unreachable as a recv error; ignore it.
        Some((s, _)) => s.recv_from(buf).await.ok(),
        None => std::future::pending().await,
    }
}

async fn bind(ip: Option<IpAddr>) -> Option<(Arc<UdpSocket>, SocketAddr)> {
    let ip = ip?;
    let any: SocketAddr = if ip.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }
        .parse()
        .expect("valid literal");
    let sock = bind_large(any)
        .map_err(|e| tracing::warn!("binding {any} failed: {e}"))
        .ok()?;
    let port = sock.local_addr().ok()?.port();
    Some((Arc::new(sock), SocketAddr::new(ip, port)))
}

/// Socket buffer size. A 1080p keyframe arrives as a burst of 100+ packets;
/// Windows' default receive buffer (~64 KB) overflows and drops them.
const SOCKET_BUFFER: usize = 4 * 1024 * 1024;

fn bind_large(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    // The OS may clamp these (Linux: net.core.rmem_max); smaller is still better than default.
    if let Err(e) = socket.set_recv_buffer_size(SOCKET_BUFFER) {
        tracing::debug!("SO_RCVBUF: {e}");
    }
    if let Err(e) = socket.set_send_buffer_size(SOCKET_BUFFER) {
        tracing::debug!("SO_SNDBUF: {e}");
    }
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    UdpSocket::from_std(socket.into())
}

/// The local IP the OS would use to reach the internet. `connect` on UDP
/// only picks a route; nothing is sent.
fn default_route_ip(v6: bool) -> Option<IpAddr> {
    let (bind, probe) = if v6 {
        ("[::]:0", "[2001:4860:4860::8888]:53")
    } else {
        ("0.0.0.0:0", "8.8.8.8:53")
    };
    let sock = StdUdpSocket::bind(bind).ok()?;
    sock.connect(probe).ok()?;
    let ip = sock.local_addr().ok()?.ip();
    let usable = match ip {
        IpAddr::V4(v4) => !v4.is_unspecified() && !v4.is_loopback(),
        // Link-local v6 needs a scope ID and is useless across the internet.
        IpAddr::V6(v6) => !v6.is_unspecified() && !v6.is_loopback() && !v6.is_unicast_link_local(),
    };
    usable.then_some(ip)
}

async fn resolve(host: &str, port: u16) -> Vec<SocketAddr> {
    match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => addrs.collect(),
        Err(e) => {
            tracing::warn!("resolving {host} failed: {e}");
            Vec::new()
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Scheme {
    Stun,
    Turn,
}

#[derive(Debug, PartialEq, Eq)]
struct IceUrl {
    scheme: Scheme,
    host: String,
    port: u16,
    /// TURN over UDP (we do not speak TURN over TCP/TLS yet).
    udp: bool,
}

/// Parses `stun:host[:port]` and `turn:host[:port][?transport=udp|tcp]`.
/// `turns:` (TLS) is skipped for now.
fn parse_ice_url(url: &str) -> Option<IceUrl> {
    let (scheme, rest) = url.split_once(':')?;
    let scheme = match scheme {
        "stun" => Scheme::Stun,
        "turn" => Scheme::Turn,
        _ => return None,
    };
    let (hostport, query) = rest.split_once('?').unwrap_or((rest, ""));
    let udp = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("transport="))
        .is_none_or(|t| t.eq_ignore_ascii_case("udp"));
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let (h, after) = v6.split_once(']')?;
        (h, after.strip_prefix(':'))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hostport, None),
        }
    };
    let port = port.map_or(Some(3478), |p| p.parse().ok())?;
    Some(IceUrl {
        scheme,
        host: host.to_owned(),
        port,
        udp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ice_urls() {
        let u = parse_ice_url("stun:stun.cloudflare.com:3478").unwrap();
        assert_eq!(
            (u.scheme, u.host.as_str(), u.port),
            (Scheme::Stun, "stun.cloudflare.com", 3478)
        );
        let u = parse_ice_url("turn:turn.cloudflare.com:53?transport=udp").unwrap();
        assert!(u.udp && u.port == 53 && u.scheme == Scheme::Turn);
        let u = parse_ice_url("turn:turn.cloudflare.com:3478?transport=tcp").unwrap();
        assert!(!u.udp);
        let u = parse_ice_url("turn:[2001:db8::1]").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("2001:db8::1", 3478));
        assert!(parse_ice_url("turns:turn.cloudflare.com:5349?transport=tcp").is_none());
        assert!(parse_ice_url("garbage").is_none());
    }
}
