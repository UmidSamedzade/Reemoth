//! NAT behaviour check: ask two STUN servers for our public address from the
//! same socket. If both see the same port, the NAT maps endpoint-
//! independently and hole punching works; if the ports differ (symmetric
//! NAT), direct connections usually fail and a relay is needed.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

use crate::stun::{Class, Message, attr, method, new_transaction_id};

const SERVERS: [(&str, u16); 2] = [("stun.cloudflare.com", 3478), ("stun.l.google.com", 19302)];
const TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatKind {
    /// Public address unchanged by any NAT.
    Open,
    /// Same public port towards every server: hole punching works.
    EndpointIndependent,
    /// Different public port per server: direct connections usually fail.
    Symmetric,
    /// Only one server answered; can't tell.
    Unknown,
    /// No server answered: UDP to the internet looks blocked.
    UdpBlocked,
}

#[derive(Debug, Clone)]
pub struct NatReport {
    pub local: SocketAddr,
    /// (server, public address it saw)
    pub mapped: Vec<(String, SocketAddr)>,
    pub kind: NatKind,
}

pub async fn nat_check() -> anyhow::Result<NatReport> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    let local_port = sock.local_addr()?.port();
    let mut targets = Vec::new();
    for (host, port) in SERVERS {
        if let Some(addr) = tokio::net::lookup_host((host, port))
            .await
            .ok()
            .and_then(|mut a| a.find(SocketAddr::is_ipv4))
        {
            targets.push((host.to_owned(), addr, new_transaction_id()));
        }
    }
    let deadline = Instant::now() + TIMEOUT;
    let mut mapped = Vec::new();
    let mut buf = [0u8; 1024];
    let mut next_send = Instant::now();
    while Instant::now() < deadline && mapped.len() < targets.len() {
        if Instant::now() >= next_send {
            for (_, addr, txid) in &targets {
                let req = Message::new(Class::Request, method::BINDING, *txid).encode(None, true);
                let _ = sock.send_to(&req, addr).await;
            }
            next_send = Instant::now() + Duration::from_millis(300);
        }
        let wait = next_send
            .min(deadline)
            .saturating_duration_since(Instant::now());
        let Ok(Ok((n, from))) = tokio::time::timeout(wait, sock.recv_from(&mut buf)).await else {
            continue;
        };
        let Ok(msg) = Message::decode(&buf[..n]) else {
            continue;
        };
        if let Some((host, _, _)) = targets
            .iter()
            .find(|(_, addr, txid)| *addr == from && *txid == msg.transaction_id)
            && !mapped.iter().any(|(h, _)| h == host)
            && let Some(addr) = msg.xor_addr(attr::XOR_MAPPED_ADDRESS)
        {
            mapped.push((host.clone(), addr));
        }
    }
    let local_ip = local_ip().unwrap_or_else(|| "0.0.0.0".parse().expect("literal"));
    let local = SocketAddr::new(local_ip, local_port);
    let kind = match mapped.as_slice() {
        [] => NatKind::UdpBlocked,
        [(_, a)] if *a == local => NatKind::Open,
        [_] => NatKind::Unknown,
        [(_, a), rest @ ..] if rest.iter().all(|(_, b)| b == a) => {
            if *a == local {
                NatKind::Open
            } else {
                NatKind::EndpointIndependent
            }
        }
        _ => NatKind::Symmetric,
    };
    Ok(NatReport {
        local,
        mapped,
        kind,
    })
}

fn local_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:53").ok()?;
    Some(s.local_addr().ok()?.ip())
}
