//! A str0m WebRTC session driven on its own tokio task.
//!
//! [`Prepared`] wraps the `Rtc` and gathered sockets for the SDP exchange;
//! [`Prepared::spawn`] starts the I/O loop and returns a [`PeerConnection`]
//! for the app to send and receive data-channel messages through.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::ChannelId;
use str0m::net::{Protocol, Receive};
use str0m::{Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::gather::Gathered;
use crate::stun;
use crate::turn::TurnClient;

/// Give up if ICE has not connected by then (no route between the peers).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Give up after the path has been down this long.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Stop taking app sends while this much is queued inside the session.
const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;
/// str0m's SCTP buffer level above which we hold sends back.
const CHANNEL_HIGH_WATER: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Same network, no NAT in between.
    DirectLan,
    /// Directly over the internet (NAT hole punched, or public IPv6).
    Direct,
    /// Through our TURN relay.
    RelayLocal,
    /// Through the peer's TURN relay.
    RelayRemote,
}

impl std::fmt::Display for PathKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::DirectLan => "direct (local network)",
            Self::Direct => "direct (peer-to-peer over the internet)",
            Self::RelayLocal => "relayed (via our TURN server)",
            Self::RelayRemote => "relayed (via the peer's TURN server)",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathInfo {
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub kind: PathKind,
}

#[derive(Debug)]
pub enum NetEvent {
    Connected(PathInfo),
    PathChanged(PathInfo),
    ChannelOpen {
        id: ChannelId,
        label: String,
    },
    Data {
        id: ChannelId,
        binary: bool,
        data: Vec<u8>,
    },
    ChannelClosed(ChannelId),
    /// The session ended; no more events follow.
    Closed(String),
}

#[derive(Debug)]
pub enum NetCmd {
    Send {
        id: ChannelId,
        binary: bool,
        data: Vec<u8>,
    },
    Close,
}

/// An `Rtc` plus its sockets, before the SDP exchange completes.
pub struct Prepared {
    rtc: Rtc,
    gathered: Gathered,
    remote_relays: HashSet<SocketAddr>,
    remote_ips: Vec<IpAddr>,
}

impl Prepared {
    pub fn new(gathered: Gathered) -> Self {
        let mut rtc = Rtc::builder().build(Instant::now());
        for c in &gathered.candidates {
            rtc.add_local_candidate(c.clone());
        }
        Self {
            rtc,
            gathered,
            remote_relays: HashSet::new(),
            remote_ips: Vec::new(),
        }
    }

    /// Offerer: creates data channels named `labels` and returns the offer SDP.
    pub fn create_offer(&mut self, labels: &[&str]) -> anyhow::Result<(String, SdpPendingOffer)> {
        let mut api = self.rtc.sdp_api();
        for label in labels {
            api.add_channel((*label).to_owned());
        }
        let (offer, pending) = api.apply().context("nothing to offer")?;
        Ok((offer.to_sdp_string(), pending))
    }

    pub fn accept_answer(&mut self, pending: SdpPendingOffer, answer: &str) -> anyhow::Result<()> {
        let parsed = SdpAnswer::from_sdp_string(answer).context("invalid SDP answer")?;
        self.rtc
            .sdp_api()
            .accept_answer(pending, parsed)
            .context("rejected SDP answer")?;
        self.note_remote_candidates(answer);
        Ok(())
    }

    /// Answerer: returns the answer SDP.
    pub fn accept_offer(&mut self, offer: &str) -> anyhow::Result<String> {
        let parsed = SdpOffer::from_sdp_string(offer).context("invalid SDP offer")?;
        let answer = self
            .rtc
            .sdp_api()
            .accept_offer(parsed)
            .context("rejected SDP offer")?;
        self.note_remote_candidates(offer);
        Ok(answer.to_sdp_string())
    }

    fn note_remote_candidates(&mut self, sdp: &str) {
        for (addr, kind) in sdp_candidates(sdp) {
            if kind == "relay" {
                self.remote_relays.insert(addr);
            }
            self.remote_ips.push(addr.ip());
        }
    }

    /// Starts the I/O loop.
    pub fn spawn(self) -> PeerConnection {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let Gathered { v4, v6, turn, .. } = self.gathered;
        let mut driver = Driver {
            rtc: self.rtc,
            v4,
            v6,
            turn,
            remote_relays: self.remote_relays,
            events: event_tx,
            queued: HashMap::new(),
            queued_bytes: 0,
            last_path: None,
            reported_path: None,
            connected: false,
            disconnected_since: None,
            started: Instant::now(),
        };
        let now = Instant::now();
        if let Some(turn) = &mut driver.turn {
            // The relay is IPv4 on the public internet; it cannot reach others.
            for ip in self.remote_ips {
                if ip.is_ipv4() && !is_private(ip) {
                    turn.ensure_permission(ip, now);
                }
            }
        }
        tokio::spawn(async move {
            let reason = match driver.run(cmd_rx).await {
                Ok(reason) => reason,
                Err(e) => format!("{e:#}"),
            };
            driver.shutdown().await;
            let _ = driver.events.send(NetEvent::Closed(reason));
        });
        PeerConnection { cmd_tx, event_rx }
    }
}

/// App-side handle to a running session.
pub struct PeerConnection {
    cmd_tx: mpsc::Sender<NetCmd>,
    event_rx: mpsc::UnboundedReceiver<NetEvent>,
}

impl PeerConnection {
    /// Waits for room in the session's queue, then queues the message.
    pub async fn send(&self, id: ChannelId, binary: bool, data: Vec<u8>) -> anyhow::Result<()> {
        self.cmd_tx
            .send(NetCmd::Send { id, binary, data })
            .await
            .map_err(|_| anyhow::anyhow!("session closed"))
    }

    pub fn sender(&self) -> mpsc::Sender<NetCmd> {
        self.cmd_tx.clone()
    }

    /// Next event; `None` after [`NetEvent::Closed`].
    pub async fn recv(&mut self) -> Option<NetEvent> {
        self.event_rx.recv().await
    }

    pub async fn close(&self) {
        let _ = self.cmd_tx.send(NetCmd::Close).await;
    }
}

struct Driver {
    rtc: Rtc,
    v4: Option<(Arc<UdpSocket>, SocketAddr)>,
    v6: Option<(Arc<UdpSocket>, SocketAddr)>,
    turn: Option<TurnClient>,
    remote_relays: HashSet<SocketAddr>,
    events: mpsc::UnboundedSender<NetEvent>,
    /// Sends waiting for SCTP buffer space, per channel.
    queued: HashMap<ChannelId, VecDeque<(bool, Vec<u8>)>>,
    queued_bytes: usize,
    /// (source, destination) of the latest non-STUN packet we sent.
    last_path: Option<(SocketAddr, SocketAddr)>,
    reported_path: Option<PathInfo>,
    connected: bool,
    disconnected_since: Option<Instant>,
    started: Instant,
}

impl Driver {
    /// Returns why the session ended.
    async fn run(&mut self, mut cmds: mpsc::Receiver<NetCmd>) -> anyhow::Result<String> {
        let mut buf4 = vec![0u8; 2048];
        let mut buf6 = vec![0u8; 2048];
        loop {
            // The last input (e.g. SCTP acks) may have freed room for queued
            // sends. Writing is a mutation, so it goes before the drain.
            self.flush_all()?;
            // Drain everything str0m wants to do before waiting again.
            let rtc_deadline = loop {
                match self.rtc.poll_output()? {
                    Output::Timeout(t) => break t,
                    Output::Transmit(t) => {
                        self.transmit(t.source, t.destination, &t.contents).await
                    }
                    Output::Event(e) => {
                        if let Some(reason) = self.on_event(e) {
                            return Ok(reason);
                        }
                    }
                }
            };
            self.flush_turn().await;
            self.report_path();

            let now = Instant::now();
            if !self.connected && now - self.started > CONNECT_TIMEOUT {
                return Ok(
                    "could not reach the peer: no direct path worked and no relay \
                           was available"
                        .into(),
                );
            }
            if let Some(since) = self.disconnected_since
                && now - since > DISCONNECT_TIMEOUT
            {
                return Ok("connection lost".into());
            }

            let mut deadline = rtc_deadline.min(now + Duration::from_secs(1));
            if let Some(t) = self.turn.as_ref().and_then(TurnClient::poll_timeout) {
                deadline = deadline.min(t);
            }
            let accept_cmds = self.queued_bytes < MAX_QUEUED_BYTES;

            tokio::select! {
                r = recv(&self.v4, &mut buf4) => {
                    if let Some((n, from)) = r { self.receive(from, true, &buf4[..n]); }
                }
                r = recv(&self.v6, &mut buf6) => {
                    if let Some((n, from)) = r { self.receive(from, false, &buf6[..n]); }
                }
                cmd = cmds.recv(), if accept_cmds => match cmd {
                    Some(NetCmd::Send { id, binary, data }) => {
                        self.queued_bytes += data.len();
                        self.queued.entry(id).or_default().push_back((binary, data));
                        self.flush_channel(id)?;
                    }
                    Some(NetCmd::Close) | None => return Ok("closed locally".into()),
                },
                _ = tokio::time::sleep_until(deadline.into()) => {
                    let now = Instant::now();
                    if let Some(turn) = &mut self.turn {
                        turn.handle_timeout(now);
                    }
                    self.rtc.handle_input(Input::Timeout(now))?;
                }
            }
        }
    }

    /// Tells the peer we are leaving (DTLS close_notify, SCTP shutdown) and
    /// releases the TURN allocation.
    async fn shutdown(&mut self) {
        if self.rtc.is_alive() && self.rtc.close().is_ok() {
            let deadline = Instant::now() + Duration::from_millis(500);
            while self.rtc.is_alive() && Instant::now() < deadline {
                match self.rtc.poll_output() {
                    Ok(Output::Transmit(t)) => {
                        self.transmit(t.source, t.destination, &t.contents).await
                    }
                    Ok(Output::Event(_)) => {}
                    Ok(Output::Timeout(_)) | Err(_) => break,
                }
            }
        }
        if let (Some(turn), Some((sock, _))) = (&mut self.turn, &self.v4)
            && let Some(pkt) = turn.close()
        {
            let _ = sock.send_to(&pkt, turn.server()).await;
        }
    }

    /// Feeds one datagram to str0m. Anyone who learns our port can send us
    /// garbage, so a rejected packet is dropped rather than ending the session.
    fn receive(&mut self, from: SocketAddr, is_v4: bool, pkt: &[u8]) {
        let now = Instant::now();
        let (source, destination, contents): (SocketAddr, SocketAddr, Vec<u8>);
        if let Some(turn) = self.turn.as_mut().filter(|t| is_v4 && from == t.server()) {
            let Some((peer, data)) = turn.handle_packet(pkt, now) else {
                return;
            };
            let Some(relay) = turn.relay_addr() else {
                return;
            };
            (source, destination, contents) = (peer, relay, data);
        } else {
            let local = if is_v4 { &self.v4 } else { &self.v6 };
            let Some((_, local)) = local else {
                return;
            };
            (source, destination, contents) = (from, *local, pkt.to_vec());
        }
        let Ok(contents) = contents.as_slice().try_into() else {
            // Not STUN/DTLS/RTP: stray traffic.
            return;
        };
        let result = self.rtc.handle_input(Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source,
                destination,
                contents,
            },
        ));
        if let Err(e) = result {
            tracing::debug!("dropped packet from {source}: {e}");
        }
    }

    async fn transmit(&mut self, source: SocketAddr, destination: SocketAddr, contents: &[u8]) {
        if !stun::is_stun(contents) {
            self.last_path = Some((source, destination));
        }
        let now = Instant::now();
        if let Some(turn) = self
            .turn
            .as_mut()
            .filter(|t| t.relay_addr() == Some(source))
        {
            if let (Some(pkt), Some((sock, _))) =
                (turn.send_to(destination, contents, now), &self.v4)
            {
                send(sock, &pkt, turn.server()).await;
            }
            return;
        }
        let sock = if destination.is_ipv4() {
            &self.v4
        } else {
            &self.v6
        };
        if let Some((sock, _)) = sock {
            send(sock, contents, destination).await;
        }
    }

    async fn flush_turn(&mut self) {
        if let (Some(turn), Some((sock, _))) = (&mut self.turn, &self.v4) {
            while let Some(pkt) = turn.poll_transmit() {
                send(sock, &pkt, turn.server()).await;
            }
        }
    }

    /// Hands queued sends to str0m while its buffer has room.
    fn flush_channel(&mut self, id: ChannelId) -> anyhow::Result<()> {
        let Some(queue) = self.queued.get_mut(&id) else {
            return Ok(());
        };
        let Some(mut channel) = self.rtc.channel(id) else {
            // Not open yet; ChannelOpen flushes it.
            return Ok(());
        };
        while channel.buffered_amount() < CHANNEL_HIGH_WATER {
            let Some((binary, data)) = queue.pop_front() else {
                break;
            };
            // `false` means SCTP has no room right now; retry after acks arrive.
            if !channel.write(binary, &data)? {
                queue.push_front((binary, data));
                break;
            }
            self.queued_bytes -= data.len();
        }
        channel.set_buffered_amount_low_threshold(CHANNEL_HIGH_WATER / 2);
        Ok(())
    }

    fn flush_all(&mut self) -> anyhow::Result<()> {
        if self.queued_bytes == 0 {
            return Ok(());
        }
        let ids: Vec<ChannelId> = self
            .queued
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.flush_channel(id)?;
        }
        Ok(())
    }

    /// Returns `Some(reason)` when the session is over.
    fn on_event(&mut self, event: Event) -> Option<String> {
        match event {
            Event::Connected => {
                self.connected = true;
                self.disconnected_since = None;
            }
            Event::IceConnectionStateChange(state) => match state {
                IceConnectionState::Disconnected => {
                    self.disconnected_since.get_or_insert_with(Instant::now);
                }
                IceConnectionState::Connected | IceConnectionState::Completed => {
                    self.disconnected_since = None;
                }
                _ => {}
            },
            Event::ChannelOpen(id, label) => {
                let _ = self.events.send(NetEvent::ChannelOpen { id, label });
                if let Err(e) = self.flush_channel(id) {
                    return Some(format!("{e:#}"));
                }
            }
            Event::ChannelData(d) => {
                let _ = self.events.send(NetEvent::Data {
                    id: d.id,
                    binary: d.binary,
                    data: d.data,
                });
            }
            Event::ChannelBufferedAmountLow(id) => {
                if let Err(e) = self.flush_channel(id) {
                    return Some(format!("{e:#}"));
                }
            }
            Event::ChannelClose(id) => {
                let _ = self.events.send(NetEvent::ChannelClosed(id));
            }
            Event::Closed => return Some("peer closed the connection".into()),
            _ => {}
        }
        None
    }

    fn report_path(&mut self) {
        if !self.connected {
            return;
        }
        let Some((local, remote)) = self.last_path else {
            return;
        };
        let kind = if self.turn.as_ref().and_then(TurnClient::relay_addr) == Some(local) {
            PathKind::RelayLocal
        } else if self.remote_relays.contains(&remote) {
            PathKind::RelayRemote
        } else if is_private(remote.ip()) {
            PathKind::DirectLan
        } else {
            PathKind::Direct
        };
        let path = PathInfo {
            local,
            remote,
            kind,
        };
        if self.reported_path != Some(path) {
            let event = if self.reported_path.is_none() {
                NetEvent::Connected(path)
            } else {
                NetEvent::PathChanged(path)
            };
            self.reported_path = Some(path);
            let _ = self.events.send(event);
        }
    }
}

async fn recv(
    sock: &Option<(Arc<UdpSocket>, SocketAddr)>,
    buf: &mut [u8],
) -> Option<(usize, SocketAddr)> {
    let Some((s, _)) = sock else {
        return std::future::pending().await;
    };
    match s.recv_from(buf).await {
        Ok(r) => Some(r),
        // Windows surfaces ICMP port-unreachable as ConnectionReset on UDP.
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::ConnectionReset | ErrorKind::ConnectionRefused
            ) =>
        {
            None
        }
        Err(e) => {
            tracing::warn!("udp recv failed: {e}");
            None
        }
    }
}

async fn send(sock: &UdpSocket, data: &[u8], to: SocketAddr) {
    if let Err(e) = sock.send_to(data, to).await {
        // Unreachable candidates (e.g. no IPv6 route) fail here; ICE moves on.
        tracing::trace!("udp send to {to} failed: {e}");
    }
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unicast_link_local() || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// `(address, type)` of each `a=candidate` line.
fn sdp_candidates(sdp: &str) -> Vec<(SocketAddr, String)> {
    sdp.lines()
        .filter_map(|line| line.trim().strip_prefix("a=candidate:"))
        .filter_map(|c| {
            let f: Vec<&str> = c.split_whitespace().collect();
            // foundation component proto priority ip port typ <type> ...
            let ip: IpAddr = f.get(4)?.parse().ok()?;
            let port: u16 = f.get(5)?.parse().ok()?;
            (f.get(6) == Some(&"typ")).then(|| {
                (
                    SocketAddr::new(ip, port),
                    f.get(7).copied().unwrap_or("").to_owned(),
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sdp_candidates() {
        let sdp = "v=0\r\na=candidate:1 1 udp 2130706431 192.168.1.5 50000 typ host\r\n\
                   a=candidate:2 1 udp 16777215 198.51.100.7 60000 typ relay raddr 0.0.0.0 rport 0\r\n";
        let c = sdp_candidates(sdp);
        assert_eq!(c.len(), 2);
        assert_eq!(
            c[1],
            ("198.51.100.7:60000".parse().unwrap(), "relay".to_owned())
        );
    }

    #[test]
    fn private_ranges() {
        assert!(is_private("192.168.0.2".parse().unwrap()));
        assert!(is_private("fd00::1".parse().unwrap()));
        assert!(!is_private("8.8.8.8".parse().unwrap()));
        assert!(!is_private("2001:db8::1".parse().unwrap()));
    }
}
