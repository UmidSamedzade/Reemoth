//! `reemoth host`: register the ID, authenticate clients, serve sessions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use reemoth_auth::{HostPake, Lockout, PasswordSecret, b64, unb64};
use reemoth_net::gather::GatherOptions;
use reemoth_net::{NetEvent, Prepared, gather};
use reemoth_proto::peer::{CtrlMsg, PeerMsg};
use reemoth_proto::signal::{ErrorCode, HostId, Role, SessionId, SignalMsg};
use tokio::sync::mpsc;

use crate::config::ConfigFile;
use crate::monitor;
use crate::signal_client::{SignalConn, SignalTx};
use crate::stream::{self, StreamControl};

const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// The signaling server says another key owns our ID.
#[derive(Debug)]
struct IdTaken;

impl std::fmt::Display for IdTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("host ID is owned by another key")
    }
}

impl std::error::Error for IdTaken {}

pub struct HostOptions {
    pub signal_url: String,
}

struct Shared {
    id: HostId,
    salt: Vec<u8>,
    secret: PasswordSecret,
    /// Shared across reconnects so a signaling blip does not reset back-off.
    lockout: Arc<Mutex<Lockout>>,
    /// One password check at a time, so parallel sessions cannot each take
    /// a guess before any failure is recorded.
    attempt: Arc<tokio::sync::Semaphore>,
    /// The session currently streaming. Windows allows one capture of a
    /// display per process, so a new viewer takes it over.
    active_stream: Arc<Mutex<Option<Arc<StreamControl>>>>,
}

pub async fn run(cfg: &mut ConfigFile, opts: HostOptions) -> anyhow::Result<()> {
    let Some((salt, secret)) = cfg.host_password()? else {
        bail!("no password set; run `reemoth set-password` first");
    };
    let lockout = Arc::new(Mutex::new(Lockout::default()));
    let attempt = Arc::new(tokio::sync::Semaphore::new(1));
    let active_stream = Arc::new(Mutex::new(None));
    let mut backoff = Duration::from_secs(1);
    loop {
        let (id, identity) = cfg.host_identity()?;
        let shared = Arc::new(Shared {
            id,
            salt: salt.clone(),
            secret: secret.clone(),
            lockout: lockout.clone(),
            attempt: attempt.clone(),
            active_stream: active_stream.clone(),
        });

        match register(&opts.signal_url, id, &identity).await {
            Ok(conn) => {
                println!();
                println!("  Reemoth host is online");
                println!("  ID: {}", id.grouped());
                println!("  Connect from another PC with:  reemoth connect {}", id);
                println!();
                backoff = Duration::from_secs(1);
                serve(conn, shared).await;
                eprintln!("lost connection to the signaling server; reconnecting");
            }
            Err(e) if e.is::<IdTaken>() => {
                // Another key owns this ID (or our config was copied). Pick a new one.
                eprintln!(
                    "ID {} is owned by another host; choosing a new ID",
                    id.grouped()
                );
                cfg.reset_host_id()?;
                continue;
            }
            Err(e) => eprintln!("signaling: {e:#}; retrying in {}s", backoff.as_secs()),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn register(
    url: &str,
    id: HostId,
    identity: &reemoth_auth::HostIdentity,
) -> anyhow::Result<SignalConn> {
    let mut conn = SignalConn::connect(url, id, Role::Host).await?;
    let SignalMsg::Challenge { nonce } = conn.expect().await? else {
        bail!("expected a challenge from the signaling server");
    };
    conn.send(SignalMsg::HostAuth {
        public_key: identity.public_base64(),
        signature: identity.sign_challenge(id, &nonce)?,
    })?;
    match conn.recv().await {
        Some(SignalMsg::Registered) => Ok(conn),
        Some(SignalMsg::Error {
            code: ErrorCode::IdTaken,
            ..
        }) => Err(IdTaken.into()),
        Some(SignalMsg::Error { code, message }) => {
            bail!("registration failed ({code:?}): {message}")
        }
        Some(other) => bail!("unexpected {other:?} during registration"),
        None => bail!("signaling connection closed during registration"),
    }
}

/// Routes signaling messages to per-session tasks until the socket closes.
async fn serve(mut conn: SignalConn, shared: Arc<Shared>) {
    let mut sessions: HashMap<SessionId, mpsc::UnboundedSender<SignalMsg>> = HashMap::new();
    while let Some(msg) = conn.recv().await {
        sessions.retain(|_, tx| !tx.is_closed());
        let session = match &msg {
            SignalMsg::Incoming { session } => {
                let (tx, rx) = mpsc::unbounded_channel();
                sessions.insert(*session, tx);
                let (shared, signal, session) = (shared.clone(), conn.sender(), *session);
                tokio::spawn(async move {
                    let tag = short(session);
                    println!("[{tag}] incoming connection");
                    if let Err(e) = handle_session(session, rx, signal.clone(), &shared).await {
                        println!("[{tag}] ended: {e:#}");
                    }
                    let _ = signal.send(SignalMsg::SessionClosed { session });
                });
                continue;
            }
            SignalMsg::Relay { session, .. }
            | SignalMsg::IceServers { session, .. }
            | SignalMsg::SessionClosed { session } => *session,
            SignalMsg::Error { code, message } => {
                eprintln!("signaling error ({code:?}): {message}");
                continue;
            }
            _ => continue,
        };
        if let Some(tx) = sessions.get(&session) {
            let _ = tx.send(msg);
        }
    }
}

async fn handle_session(
    session: SessionId,
    mut rx: mpsc::UnboundedReceiver<SignalMsg>,
    signal: SignalTx,
    shared: &Shared,
) -> anyhow::Result<()> {
    let tag = short(session);
    let relay = |msg: PeerMsg| {
        signal.send(SignalMsg::Relay {
            session,
            payload: msg.encode(),
        })
    };

    let Ok(permit) = shared.attempt.clone().try_acquire_owned() else {
        relay(PeerMsg::Denied {
            reason: "another login attempt is in progress; try again in a few seconds".into(),
            retry_after_secs: Some(5),
        })?;
        bail!("refused: another login attempt is in progress");
    };
    check_lockout(shared, &relay)?;

    let (pake, hello) = HostPake::start(&shared.secret, shared.id, session);
    relay(PeerMsg::Hello {
        salt: b64(&shared.salt),
        pake: b64(&hello),
    })?;

    let PeerMsg::Auth { pake: msg, confirm } = next_peer(&mut rx).await? else {
        bail!("client sent something other than its auth message");
    };
    // A failure may have been recorded while we waited for this client.
    check_lockout(shared, &relay)?;
    let keys = match pake.finish(&unb64(&msg, "pake")?, &unb64(&confirm, "confirm")?) {
        Ok(keys) => keys,
        Err(e) => {
            shared
                .lockout
                .lock()
                .expect("lockout lock")
                .record_failure(Instant::now());
            relay(PeerMsg::Denied {
                reason: "wrong password".into(),
                retry_after_secs: None,
            })?;
            bail!("client failed authentication: {e}");
        }
    };
    shared
        .lockout
        .lock()
        .expect("lockout lock")
        .record_success();
    // Only the password check is serialized; the session itself runs alongside others.
    drop(permit);
    println!("[{tag}] password accepted");
    signal.send(SignalMsg::Authorize { session })?;
    relay(PeerMsg::AuthOk {
        confirm: b64(&keys.host_confirmation()),
    })?;

    let SignalMsg::IceServers { servers, .. } = next(&mut rx).await? else {
        bail!("expected ICE servers from the signaling server");
    };
    let gathered = gather(&servers, &GatherOptions::default()).await?;
    tracing::info!("[{tag}] candidates: {:?}", gathered.describe());
    let mut prepared = Prepared::new(gathered);

    let PeerMsg::Offer { sdp, mac } = next_peer(&mut rx).await? else {
        bail!("expected an SDP offer");
    };
    keys.verify_sdp("offer", &sdp, &unb64(&mac, "mac")?)
        .context("offer failed its integrity check (tampered in transit?)")?;
    let answer = prepared.accept_offer(&sdp)?;
    relay(PeerMsg::Answer {
        mac: b64(&keys.sdp_mac("answer", &answer)),
        sdp: answer,
    })?;

    run_peer(prepared, &tag, shared).await
}

/// Serves one connected client: answers pings, prints chat, measures bulk.
/// Serves one connected client: streams the screen if it asked for video,
/// answers pings, prints chat, measures bulk transfers.
async fn run_peer(prepared: Prepared, tag: &str, shared: &Shared) -> anyhow::Result<()> {
    let mut peer = prepared.spawn();
    let mut ctrl = None;
    let mut bulk: Option<(u64, u64, Instant)> = None; // (expected, received, start)
    let mut streaming: Option<(Arc<StreamControl>, std::thread::JoinHandle<()>)> = None;
    let mut stats_tick = tokio::time::interval(Duration::from_secs(1));
    // Host-side numbers of the last second, printed next to the viewer's report.
    let mut last_host: Option<stream::Stats> = None;
    // Snapshot being received: (width, height, bytes so far).
    let mut pending_snapshot: Option<(u32, u32, Vec<u8>)> = None;
    let result = loop {
        let event = tokio::select! {
            ev = peer.recv() => match ev {
                Some(ev) => ev,
                None => break Ok(()),
            },
            _ = stats_tick.tick() => {
                if let (Some((control, _)), Some(id)) = (&streaming, ctrl) {
                    let s = control.take_stats();
                    last_host = Some(s.clone());
                    let msg = CtrlMsg::HostStats {
                        width: s.width,
                        height: s.height,
                        fps: s.frames as f32,
                        encode_ms: if s.frames == 0 { 0.0 } else {
                            s.encode_time.as_secs_f32() * 1e3 / s.frames as f32
                        },
                        bitrate_kbps: s.bitrate_bps / 1000,
                        encoder: s.encoder,
                    };
                    let _ = peer.send(id, false, msg.encode().into_bytes()).await;
                }
                continue;
            }
        };
        match event {
            NetEvent::VideoReady if streaming.is_none() => {
                println!(
                    "[{tag}] streaming the screen (viewer snapshots: {})",
                    monitor::snapshot_path(tag).display()
                );
                let control = Arc::new(StreamControl::default());
                let previous = shared
                    .active_stream
                    .lock()
                    .expect("active stream lock")
                    .replace(control.clone());
                if let Some(previous) = previous {
                    println!("[{tag}] taking over the screen from an earlier session");
                    previous.stop();
                }
                let handle = stream::spawn(peer.sender(), control.clone());
                streaming = Some((control, handle));
            }
            NetEvent::KeyframeRequested => {
                if let Some((control, _)) = &streaming {
                    control.request_keyframe();
                }
            }
            NetEvent::BitrateEstimate(_) => {}
            NetEvent::Connected(path) => println!(
                "[{tag}] connected: {}  ({} <-> {})",
                path.kind, path.local, path.remote
            ),
            NetEvent::PathChanged(path) => println!(
                "[{tag}] path changed: {}  ({} <-> {})",
                path.kind, path.local, path.remote
            ),
            NetEvent::ChannelOpen { id, label } if label == "ctrl" => ctrl = Some(id),
            NetEvent::Data {
                id,
                binary: false,
                data,
            } if Some(id) == ctrl => {
                let Ok(msg) = CtrlMsg::decode(&String::from_utf8_lossy(&data)) else {
                    continue;
                };
                match msg {
                    CtrlMsg::Ping { seq } => {
                        peer.send(id, false, CtrlMsg::Pong { seq }.encode().into_bytes())
                            .await?;
                    }
                    CtrlMsg::Chat { text } => println!("[{tag}] says: {text}"),
                    CtrlMsg::BulkStart { bytes } => bulk = Some((bytes, 0, Instant::now())),
                    report @ CtrlMsg::ViewerStats { .. } => {
                        if let (
                            Some((control, _)),
                            CtrlMsg::ViewerStats {
                                gaps,
                                keyframe_requests,
                                ..
                            },
                        ) = (&streaming, &report)
                        {
                            control.on_viewer_report(*gaps > 0 || *keyframe_requests > 0);
                        }
                        println!(
                            "[{tag}] {}",
                            monitor::stats_line(last_host.as_ref(), &report)
                        );
                    }
                    CtrlMsg::Snapshot { width, height } => {
                        pending_snapshot = Some((width, height, Vec::new()));
                    }
                    _ => {}
                }
            }
            NetEvent::Data {
                id,
                binary: true,
                data,
            } if Some(id) == ctrl => {
                if let Some((w, h, buf)) = &mut pending_snapshot {
                    buf.extend_from_slice(&data);
                    let expected = monitor::snapshot_len(*w, *h);
                    if buf.len() >= expected {
                        match monitor::save_snapshot(tag, *w, *h, buf) {
                            Ok(path) => {
                                tracing::info!("[{tag}] viewer snapshot: {}", path.display())
                            }
                            Err(e) => eprintln!("[{tag}] could not save viewer snapshot: {e:#}"),
                        }
                        pending_snapshot = None;
                    }
                    continue;
                }
                if let Some((expected, received, start)) = &mut bulk {
                    *received += data.len() as u64;
                    if *received >= *expected {
                        let report = CtrlMsg::BulkReport {
                            bytes: *received,
                            micros: start.elapsed().as_micros() as u64,
                        };
                        bulk = None;
                        peer.send(id, false, report.encode().into_bytes()).await?;
                    }
                }
            }
            NetEvent::Closed(reason) => break Err(anyhow::anyhow!("{reason}")),
            _ => {}
        }
    };
    if let Some((control, handle)) = streaming {
        control.stop();
        // The thread notices within one capture timeout (100 ms).
        let _ = tokio::task::spawn_blocking(move || handle.join()).await;
    }
    result
}

async fn next(rx: &mut mpsc::UnboundedReceiver<SignalMsg>) -> anyhow::Result<SignalMsg> {
    match tokio::time::timeout(STEP_TIMEOUT, rx.recv()).await {
        Ok(Some(SignalMsg::SessionClosed { .. })) | Ok(None) => bail!("client left"),
        Ok(Some(msg)) => Ok(msg),
        Err(_) => bail!("client timed out"),
    }
}

async fn next_peer(rx: &mut mpsc::UnboundedReceiver<SignalMsg>) -> anyhow::Result<PeerMsg> {
    match next(rx).await? {
        SignalMsg::Relay { payload, .. } => PeerMsg::decode(&payload).context("bad peer message"),
        other => bail!("expected a relayed message, got {other:?}"),
    }
}

fn check_lockout(
    shared: &Shared,
    relay: &impl Fn(PeerMsg) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let locked = shared
        .lockout
        .lock()
        .expect("lockout lock")
        .check(Instant::now());
    if let Some(wait) = locked {
        relay(PeerMsg::Denied {
            reason: "too many failed attempts; try again later".into(),
            retry_after_secs: Some(wait.as_secs().max(1)),
        })?;
        bail!(
            "refused: locked out for {}s after failed attempts",
            wait.as_secs()
        );
    }
    Ok(())
}

/// Short session label for logs.
fn short(session: SessionId) -> String {
    format!("{:04x}", session & 0xffff)
}

/// Asks for a new password twice (hidden input) or takes it from the environment.
pub fn prompt_new_password() -> anyhow::Result<String> {
    if let Ok(pw) = std::env::var("REEMOTH_PASSWORD") {
        PasswordSecret::check_strength(&pw).map_err(anyhow::Error::msg)?;
        return Ok(pw);
    }
    loop {
        let pw = rpassword::prompt_password("New host password: ")?;
        if let Err(why) = PasswordSecret::check_strength(&pw) {
            eprintln!("{why}");
            continue;
        }
        if rpassword::prompt_password("Repeat password: ")? != pw {
            eprintln!("passwords do not match");
            continue;
        }
        return Ok(pw);
    }
}
