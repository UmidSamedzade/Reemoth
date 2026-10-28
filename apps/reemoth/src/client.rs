//! `reemoth connect`: authenticate to a host, open the P2P link, test it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use reemoth_auth::{ClientPake, PasswordSecret, SessionKeys, b64, unb64};
use reemoth_net::ChannelId;
use reemoth_net::gather::GatherOptions;
use reemoth_net::{NetEvent, PathInfo, PeerConnection, Prepared, gather};
use reemoth_proto::peer::{CtrlMsg, INPUT_CHANNEL, PeerMsg};
use reemoth_proto::signal::{HostId, IceServer, Role, SessionId, SignalMsg};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::signal_client::SignalConn;

const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// Which host to reach and how.
pub struct Target {
    pub signal_url: String,
    pub id: HostId,
    pub password: String,
    pub relay_only: bool,
}

pub struct ConnectOptions {
    pub target: Target,
    pub pings: u32,
    pub bulk_mib: u32,
    pub chat: bool,
}

pub async fn run(opts: ConnectOptions) -> anyhow::Result<()> {
    let (mut peer, Channels { ctrl, .. }, _) =
        establish(&opts.target, false, &|line| println!("{line}")).await?;
    if opts.pings > 0 {
        ping_test(&mut peer, ctrl, opts.pings).await?;
    }
    if opts.bulk_mib > 0 {
        bulk_test(&mut peer, ctrl, opts.bulk_mib).await?;
    }
    if opts.chat {
        chat(&mut peer, ctrl).await?;
    }
    peer.close().await;
    Ok(())
}

/// Authenticates, exchanges SDP and waits until the P2P link and the `ctrl`
/// channel are up. `log` receives progress lines.
pub async fn establish(
    target: &Target,
    receive_video: bool,
    log: &(dyn Fn(&str) + Sync),
) -> anyhow::Result<(PeerConnection, Channels, PathInfo)> {
    let started = Instant::now();
    let mut conn = SignalConn::connect(&target.signal_url, target.id, Role::Client).await?;
    log(&format!("Contacting host {} ...", target.id.grouped()));

    let (session, keys, servers) = authenticate(&mut conn, target).await?;
    log("Password accepted by host.");

    let gathered = gather(
        &servers,
        &GatherOptions {
            relay_only: target.relay_only,
        },
    )
    .await?;
    for c in gathered.describe() {
        log(&format!("  local candidate: {c}"));
    }
    let mut prepared = Prepared::new(gathered);
    let (offer, pending) = prepared.create_offer(&["ctrl"], &[INPUT_CHANNEL], receive_video)?;
    conn.send(SignalMsg::Relay {
        session,
        payload: PeerMsg::Offer {
            mac: b64(&keys.sdp_mac("offer", &offer)),
            sdp: offer,
        }
        .encode(),
    })?;
    let PeerMsg::Answer { sdp, mac } = next_peer(&mut conn).await? else {
        bail!("expected an SDP answer");
    };
    keys.verify_sdp("answer", &sdp, &unb64(&mac, "mac")?)
        .context("answer failed its integrity check (tampered in transit?)")?;
    prepared.accept_answer(pending, &sdp)?;
    // Signaling's job is done; the session runs peer-to-peer from here.
    drop(conn);

    let mut peer = prepared.spawn();
    let (channels, path) = wait_connected(&mut peer, started, log).await?;
    Ok((peer, channels, path))
}

async fn authenticate(
    conn: &mut SignalConn,
    opts: &Target,
) -> anyhow::Result<(SessionId, SessionKeys, Vec<IceServer>)> {
    let (session, hello) = match next(conn).await? {
        SignalMsg::Relay { session, payload } => (session, PeerMsg::decode(&payload)?),
        other => bail!("unexpected {other:?} from signaling"),
    };
    let PeerMsg::Hello { salt, pake } = hello else {
        return Err(denied(hello));
    };
    let salt = unb64(&salt, "salt")?;
    let password = opts.password.clone();
    // Argon2 takes ~50 ms of CPU; keep it off the async workers.
    let secret =
        tokio::task::spawn_blocking(move || PasswordSecret::derive(&password, &salt)).await??;
    let (client, msg) = ClientPake::start(&secret, opts.id);
    let keys = client.finish(&unb64(&pake, "pake")?, session)?;
    conn.send(SignalMsg::Relay {
        session,
        payload: PeerMsg::Auth {
            pake: b64(&msg),
            confirm: b64(&keys.client_confirmation()),
        }
        .encode(),
    })?;

    // AuthOk and IceServers come from different senders; accept either order.
    let mut host_confirmed = false;
    let mut servers = None;
    while !(host_confirmed && servers.is_some()) {
        match next(conn).await? {
            SignalMsg::IceServers { servers: s, .. } => servers = Some(s),
            SignalMsg::Relay { payload, .. } => {
                match PeerMsg::decode(&payload)? {
                    PeerMsg::AuthOk { confirm } => {
                        keys.verify_host_confirmation(&unb64(&confirm, "confirm")?)
                        .context("the host could not prove it knows the password; refusing to continue")?;
                        host_confirmed = true;
                    }
                    other => return Err(denied(other)),
                }
            }
            other => bail!("unexpected {other:?} during authentication"),
        }
    }
    Ok((session, keys, servers.expect("loop exits with servers")))
}

fn denied(msg: PeerMsg) -> anyhow::Error {
    match msg {
        PeerMsg::Denied {
            reason,
            retry_after_secs: Some(s),
        } => anyhow::anyhow!("host refused: {reason} (retry in {s}s)"),
        PeerMsg::Denied { reason, .. } => anyhow::anyhow!("host refused: {reason}"),
        other => anyhow::anyhow!("unexpected message from host: {other:?}"),
    }
}

/// The data channels every session opens.
#[derive(Debug, Clone, Copy)]
pub struct Channels {
    /// Reliable, ordered: control messages, keys, clicks, clipboard.
    pub ctrl: ChannelId,
    /// Unreliable: pointer position.
    pub input: ChannelId,
}

async fn wait_connected(
    peer: &mut PeerConnection,
    started: Instant,
    log: &(dyn Fn(&str) + Sync),
) -> anyhow::Result<(Channels, PathInfo)> {
    let mut ctrl = None;
    let mut input = None;
    let mut connected = None;
    while !(connected.is_some() && ctrl.is_some() && input.is_some()) {
        match peer.recv().await {
            Some(NetEvent::Connected(path)) => {
                log("");
                log(&format!(
                    "Connected in {} ms",
                    started.elapsed().as_millis()
                ));
                log(&format!("  path:   {}", path.kind));
                log(&format!("  local:  {}", path.local));
                log(&format!("  remote: {}", path.remote));
                connected = Some(path);
            }
            Some(NetEvent::ChannelOpen { id, label }) if label == "ctrl" => ctrl = Some(id),
            Some(NetEvent::ChannelOpen { id, label }) if label == INPUT_CHANNEL => input = Some(id),
            Some(NetEvent::Closed(reason)) => bail!("{reason}"),
            Some(_) => {}
            None => bail!("session ended"),
        }
    }
    let channels = Channels {
        ctrl: ctrl.expect("loop exits with ctrl"),
        input: input.expect("loop exits with input"),
    };
    Ok((channels, connected.expect("loop exits connected")))
}

async fn ping_test(peer: &mut PeerConnection, ctrl: ChannelId, count: u32) -> anyhow::Result<()> {
    println!();
    println!("Round-trip latency ({count} pings over the data channel):");
    let mut sent: HashMap<u32, Instant> = HashMap::new();
    let mut rtts = Vec::with_capacity(count as usize);
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut next_seq = 0;
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(20) * count + Duration::from_secs(5);
    while (rtts.len() as u32) < count {
        tokio::select! {
            _ = tick.tick(), if next_seq < count => {
                sent.insert(next_seq, Instant::now());
                peer.send(ctrl, false, CtrlMsg::Ping { seq: next_seq }.encode().into_bytes()).await?;
                next_seq += 1;
            }
            ev = peer.recv() => match ev {
                Some(NetEvent::Data { binary: false, data, .. }) => {
                    if let Ok(CtrlMsg::Pong { seq }) = CtrlMsg::decode(&String::from_utf8_lossy(&data))
                        && let Some(t) = sent.remove(&seq)
                    {
                        rtts.push(t.elapsed());
                    }
                }
                Some(NetEvent::PathChanged(p)) => println!("  path changed: {}", p.kind),
                Some(NetEvent::Closed(reason)) => bail!("{reason}"),
                Some(_) => {}
                None => bail!("session ended"),
            },
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
    if rtts.is_empty() {
        bail!("no pongs received");
    }
    rtts.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let pct = |p: f64| rtts[((rtts.len() - 1) as f64 * p).round() as usize];
    let avg = rtts.iter().sum::<Duration>() / rtts.len() as u32;
    println!(
        "  min {:.2} ms   avg {:.2} ms   p50 {:.2} ms   p95 {:.2} ms   max {:.2} ms   lost {}",
        ms(rtts[0]),
        ms(avg),
        ms(pct(0.5)),
        ms(pct(0.95)),
        ms(*rtts.last().expect("non-empty")),
        count as usize - rtts.len()
    );
    Ok(())
}

async fn bulk_test(peer: &mut PeerConnection, ctrl: ChannelId, mib: u32) -> anyhow::Result<()> {
    const CHUNK: usize = 16 * 1024;
    let total = u64::from(mib) * 1024 * 1024;
    println!();
    println!("Throughput (sending {mib} MiB to the host):");
    peer.send(
        ctrl,
        false,
        CtrlMsg::BulkStart { bytes: total }.encode().into_bytes(),
    )
    .await?;
    let tx = peer.sender();
    let sender = tokio::spawn(async move {
        let chunk = vec![0xA5u8; CHUNK];
        let mut left = total;
        while left > 0 {
            let n = left.min(CHUNK as u64) as usize;
            let cmd = reemoth_net::NetCmd::Send {
                id: ctrl,
                binary: true,
                data: chunk[..n].to_vec(),
            };
            if tx.send(cmd).await.is_err() {
                break;
            }
            left -= n as u64;
        }
    });
    let timeout = tokio::time::sleep(Duration::from_secs(120));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            ev = peer.recv() => match ev {
                Some(NetEvent::Data { binary: false, data, .. }) => {
                    if let Ok(CtrlMsg::BulkReport { bytes, micros }) = CtrlMsg::decode(&String::from_utf8_lossy(&data)) {
                        let secs = micros as f64 / 1e6;
                        println!(
                            "  {:.1} MiB in {:.2} s = {:.1} Mbit/s",
                            bytes as f64 / 1048576.0,
                            secs,
                            bytes as f64 * 8.0 / secs / 1e6
                        );
                        break;
                    }
                }
                Some(NetEvent::Closed(reason)) => bail!("{reason}"),
                Some(_) => {}
                None => bail!("session ended"),
            },
            _ = &mut timeout => bail!("throughput test timed out"),
        }
    }
    sender.await?;
    Ok(())
}

async fn chat(peer: &mut PeerConnection, ctrl: ChannelId) -> anyhow::Result<()> {
    println!();
    println!("Type a message and press Enter to show it on the host. Ctrl+C to quit.");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => match line? {
                Some(text) if !text.trim().is_empty() => {
                    peer.send(ctrl, false, CtrlMsg::Chat { text }.encode().into_bytes()).await?;
                }
                Some(_) => {}
                None => return Ok(()),
            },
            ev = peer.recv() => match ev {
                Some(NetEvent::PathChanged(p)) => println!("(path changed: {})", p.kind),
                Some(NetEvent::Closed(reason)) => bail!("{reason}"),
                Some(_) => {}
                None => return Ok(()),
            },
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
    }
}

async fn next(conn: &mut SignalConn) -> anyhow::Result<SignalMsg> {
    tokio::time::timeout(STEP_TIMEOUT, conn.expect())
        .await
        .context("host stopped responding")?
}

async fn next_peer(conn: &mut SignalConn) -> anyhow::Result<PeerMsg> {
    match next(conn).await? {
        SignalMsg::Relay { payload, .. } => Ok(PeerMsg::decode(&payload)?),
        SignalMsg::SessionClosed { .. } => bail!("host closed the session"),
        other => bail!("unexpected {other:?} from signaling"),
    }
}
