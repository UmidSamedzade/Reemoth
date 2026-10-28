//! `reemoth view`: show a host's screen in a window.
//!
//! Threads: the winit event loop owns the main thread and the GPU; the
//! network session runs on the tokio runtime; H.264 decoding has its own
//! thread so a slow frame never stalls packet handling. Decoded frames go
//! through a one-slot mailbox, so the window always shows the newest one.
//!
//! Input: the pointer position goes on the unreliable `input` channel
//! (newest wins); clicks, keys and wheel on the reliable `ctrl` channel.
//! The pointer itself is this machine's own cursor, drawn by the local
//! compositor with zero latency, in the shape the host reports.
//! Ctrl+Alt+F toggles fullscreen; every other key goes to the host.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use anyhow::Context;
use reemoth_net::{ChannelId, NetCmd, NetEvent, PathKind};
use reemoth_proto::peer::{CtrlMsg, MouseButton, PointerMove};
use reemoth_video::{Decoder, I420Frame};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{CursorIcon, Fullscreen, Window, WindowId};

use crate::client::{Channels, Target, establish};
use crate::render::Renderer;

enum UserEvent {
    /// A new decoded frame is in the mailbox.
    Frame,
    Status(String),
    /// The host's pointer shape (CSS name) and visibility.
    Cursor(String, bool),
    Closed(String),
}

pub struct ViewOptions {
    pub clipboard: bool,
}

#[derive(Default)]
struct Shared {
    /// Newest decoded frame not yet drawn.
    mailbox: Mutex<Option<I420Frame>>,
    stats: Mutex<ViewStats>,
    net: Mutex<Option<mpsc::Sender<NetCmd>>>,
    channels: Mutex<Option<Channels>>,
    /// Control-channel input from the window. Unbounded and forwarded in
    /// order by a task that waits for room, so a key or button release is
    /// never dropped (a lost release is a stuck key on the host).
    ctrl_out: Mutex<Option<mpsc::UnboundedSender<CtrlMsg>>>,
}

#[derive(Default)]
struct ViewStats {
    frames_decoded: u32,
    decode_time: Duration,
    frames_drawn: u32,
    bytes: u64,
    /// Arrival of the first packet → picture ready to draw.
    pipeline_time: Duration,
    keyframe_requests: u32,
    gaps: u32,
    max_stall: Duration,
}

/// How often the viewer sends the host a small copy of what it shows.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(15);
/// Snapshot = every Nth pixel of the picture (1080p → 480×270).
const SNAPSHOT_STEP: usize = 4;
const SNAPSHOT_CHUNK: usize = 16 * 1024;

pub fn run(rt: Runtime, target: Target, opts: ViewOptions) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("opening a window (is a display server running?)")?;
    let proxy = event_loop.create_proxy();
    let shared = Arc::new(Shared::default());
    let title = format!("Reemoth — {}", target.id.grouped());
    {
        let (proxy, shared) = (proxy.clone(), shared.clone());
        rt.spawn(async move {
            let reason = match network(target, opts, proxy.clone(), shared).await {
                Ok(()) => "session ended".to_owned(),
                Err(e) => format!("{e:#}"),
            };
            let _ = proxy.send_event(UserEvent::Closed(reason));
        });
    }
    let mut app = App {
        shared,
        title,
        window: None,
        renderer: None,
        error: None,
        closed: None,
        pointer: None,
        last_pointer: (0.5, 0.5),
        pointer_seq: 0,
        held: HashSet::new(),
        modifiers: ModifiersState::empty(),
    };
    event_loop.run_app(&mut app)?;
    // Let the session send its goodbye before the runtime is torn down.
    rt.shutdown_timeout(Duration::from_millis(500));
    if let Some(e) = app.error {
        return Err(e);
    }
    if let Some(reason) = app.closed {
        eprintln!("{reason}");
    }
    Ok(())
}

struct App {
    shared: Arc<Shared>,
    title: String,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    error: Option<anyhow::Error>,
    closed: Option<String>,
    /// Pointer position over the picture, normalized (0..1); `None` over
    /// the black bars or outside the window.
    pointer: Option<(f32, f32)>,
    /// Last position sent, clamped to the picture: where releases land.
    last_pointer: (f32, f32),
    pointer_seq: u32,
    /// Buttons pressed on the host through this viewer.
    held: HashSet<MouseButton>,
    modifiers: ModifiersState,
}

impl App {
    fn send_ctrl(&self, msg: CtrlMsg) {
        if let Some(tx) = self.shared.ctrl_out.lock().expect("ctrl lock").as_ref() {
            let _ = tx.send(msg);
        }
    }

    fn pointer_moved(&mut self, wx: f64, wy: f64) {
        let Some((left, top, w, h)) = self.renderer.as_ref().and_then(Renderer::picture_rect)
        else {
            return;
        };
        let (mut x, mut y) = (((wx - left) / w) as f32, ((wy - top) / h) as f32);
        let inside = (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y);
        if !inside {
            if self.held.is_empty() {
                // Over the black bars: nothing to point at.
                self.pointer = None;
                return;
            }
            // Mid-drag: keep following, pinned to the screen edge.
            (x, y) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
        }
        self.pointer = Some((x, y));
        self.last_pointer = (x, y);
        self.pointer_seq = self.pointer_seq.wrapping_add(1);
        let (Some(tx), Some(ch)) = (
            self.shared.net.lock().expect("net lock").clone(),
            *self.shared.channels.lock().expect("channels lock"),
        ) else {
            return;
        };
        let seq = self.pointer_seq;
        let data = serde_json::to_vec(&PointerMove { x, y, seq }).expect("serializes");
        // Lossy on purpose: a newer position is always on its way.
        let _ = tx.try_send(NetCmd::Send {
            id: ch.input,
            binary: false,
            data,
        });
    }

    fn toggle_fullscreen(&self) {
        if let Some(w) = &self.window {
            let next = if w.fullscreen().is_some() {
                None
            } else {
                Some(Fullscreen::Borderless(None))
            };
            w.set_fullscreen(next);
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(format!("{} — connecting…", self.title))
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let result = event_loop
            .create_window(attrs)
            .context("creating window")
            .and_then(|w| {
                let w = Arc::new(w);
                Ok((w.clone(), Renderer::new(w)?))
            });
        match result {
            Ok((window, renderer)) => {
                println!("Drawing with {}", renderer.adapter_name);
                self.window = Some(window);
                self.renderer = Some(renderer);
            }
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Frame => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::Status(s) => {
                if let Some(w) = &self.window {
                    w.set_title(&format!("{} — {s}", self.title));
                }
            }
            UserEvent::Cursor(shape, visible) => {
                if let Some(w) = &self.window {
                    let icon = shape.parse::<CursorIcon>().unwrap_or_default();
                    w.set_cursor(icon);
                    w.set_cursor_visible(visible);
                }
            }
            UserEvent::Closed(reason) => {
                self.closed = Some(reason);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                if let Some(tx) = self.shared.net.lock().expect("net lock").as_ref() {
                    let _ = tx.try_send(NetCmd::Close);
                }
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state,
                        ..
                    },
                ..
            } => {
                let down = state == ElementState::Pressed;
                if code == KeyCode::KeyF
                    && down
                    && self.modifiers.control_key()
                    && self.modifiers.alt_key()
                {
                    self.toggle_fullscreen();
                    return;
                }
                if let Some(code) = w3c_code(code) {
                    // Repeats are forwarded too: injected keys don't auto-repeat.
                    self.send_ctrl(CtrlMsg::Key { code, down });
                }
            }
            WindowEvent::CursorMoved { position, .. } => self.pointer_moved(position.x, position.y),
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    winit::event::MouseButton::Back => MouseButton::Back,
                    winit::event::MouseButton::Forward => MouseButton::Forward,
                    winit::event::MouseButton::Other(_) => return,
                };
                let down = state == ElementState::Pressed;
                let (x, y) = if down {
                    // A press needs a spot on the picture.
                    let Some(p) = self.pointer else { return };
                    self.held.insert(button);
                    p
                } else {
                    // Release only what we pressed, where the pointer last was.
                    if !self.held.remove(&button) {
                        return;
                    }
                    self.last_pointer
                };
                self.send_ctrl(CtrlMsg::MouseButton { x, y, button, down });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    // One line = one notch.
                    // winit's positive x scrolls left; Windows' positive HWHEEL scrolls right.
                    MouseScrollDelta::LineDelta(x, y) => (-x * 120.0, y * 120.0),
                    // Touchpads: ~3 lines of ~16 px per notch.
                    MouseScrollDelta::PixelDelta(p) => ((-p.x * 2.5) as f32, (p.y * 2.5) as f32),
                };
                if self.pointer.is_some() {
                    self.send_ctrl(CtrlMsg::Wheel { dx, dy });
                }
            }
            WindowEvent::Focused(false) => {
                // Keys held while focus leaves would never see their release.
                self.send_ctrl(CtrlMsg::ReleaseInput);
                self.modifiers = ModifiersState::empty();
                self.held.clear();
            }
            WindowEvent::RedrawRequested => {
                let frame = self.shared.mailbox.lock().expect("mailbox lock").take();
                if let Some(r) = &mut self.renderer {
                    if let Err(e) = r.draw(frame.as_ref()) {
                        self.error = Some(e);
                        event_loop.exit();
                        return;
                    }
                    if frame.is_some() {
                        self.shared.stats.lock().expect("stats lock").frames_drawn += 1;
                    }
                }
            }
            _ => {}
        }
    }
}

struct EncodedIn {
    data: Arc<[u8]>,
    keyframe: bool,
    contiguous: bool,
    arrived: Instant,
}

async fn network(
    target: Target,
    opts: ViewOptions,
    proxy: EventLoopProxy<UserEvent>,
    shared: Arc<Shared>,
) -> anyhow::Result<()> {
    let (mut peer, channels, path) = establish(&target, true, &|line| println!("{line}")).await?;
    let ctrl = channels.ctrl;
    let net_tx = peer.sender();
    *shared.net.lock().expect("net lock") = Some(net_tx.clone());
    *shared.channels.lock().expect("channels lock") = Some(channels);
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<CtrlMsg>();
    *shared.ctrl_out.lock().expect("ctrl lock") = Some(ctrl_tx);
    {
        let net_tx = net_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = ctrl_rx.recv().await {
                send_ctrl(&net_tx, ctrl, &msg).await;
            }
        });
    }
    let mut clipboard = if opts.clipboard {
        crate::clipboard::spawn()
            .map_err(|e| eprintln!("clipboard sync off: {e:#}"))
            .ok()
    } else {
        None
    };

    let (frames_tx, frames_rx) = std_mpsc::channel::<EncodedIn>();
    {
        let (shared, proxy, net_tx) = (shared.clone(), proxy.clone(), net_tx.clone());
        std::thread::Builder::new()
            .name("reemoth-decode".into())
            .spawn(move || decode_loop(frames_rx, shared, proxy, net_tx, ctrl))
            .context("spawning decoder thread")?;
    }

    let mut path_kind = path.kind;
    let mut rtt: Option<Duration> = None;
    let mut ping_sent: Option<(u32, Instant)> = None;
    let mut ping_seq = 0u32;
    let mut host: Option<CtrlMsg> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut first_frame = true;
    let mut last_arrival: Option<Instant> = None;
    loop {
        tokio::select! {
            ev = peer.recv() => match ev {
                Some(NetEvent::VideoFrame { data, keyframe, contiguous, arrived, .. }) => {
                    if first_frame {
                        println!("Receiving video");
                        first_frame = false;
                    }
                    {
                        let mut s = shared.stats.lock().expect("stats lock");
                        s.bytes += data.len() as u64;
                        s.gaps += u32::from(!contiguous);
                        if let Some(prev) = last_arrival {
                            s.max_stall = s.max_stall.max(arrived.saturating_duration_since(prev));
                        }
                    }
                    last_arrival = Some(arrived);
                    if frames_tx.send(EncodedIn { data, keyframe, contiguous, arrived }).is_err() {
                        anyhow::bail!("decoder stopped");
                    }
                }
                Some(NetEvent::Data { id, binary: false, data }) if id == ctrl => {
                    match CtrlMsg::decode(&String::from_utf8_lossy(&data)) {
                        Ok(CtrlMsg::Pong { seq }) => {
                            if let Some((s, at)) = ping_sent && s == seq {
                                rtt = Some(at.elapsed());
                                ping_sent = None;
                            }
                        }
                        Ok(stats @ CtrlMsg::HostStats { .. }) => host = Some(stats),
                        Ok(CtrlMsg::Cursor { shape, visible }) => {
                            let _ = proxy.send_event(UserEvent::Cursor(shape, visible));
                        }
                        Ok(CtrlMsg::Clipboard { text }) => {
                            if let Some(c) = &clipboard {
                                let _ = c.set.send(text);
                            }
                        }
                        _ => {}
                    }
                }
                Some(NetEvent::PathChanged(p)) => path_kind = p.kind,
                Some(NetEvent::Closed(reason)) => anyhow::bail!("{reason}"),
                Some(_) => {}
                None => return Ok(()),
            },
            Some(text) = recv_clipboard(&mut clipboard) => {
                send_ctrl(&net_tx, ctrl, &CtrlMsg::Clipboard { text }).await;
            }
            _ = tick.tick() => {
                ping_seq += 1;
                ping_sent = Some((ping_seq, Instant::now()));
                send_ctrl(&net_tx, ctrl, &CtrlMsg::Ping { seq: ping_seq }).await;
                let (status, report) = status_line(&shared, rtt, host.as_ref(), path_kind);
                let _ = proxy.send_event(UserEvent::Status(status));
                send_ctrl(&net_tx, ctrl, &report).await;
            }
        }
    }
}

async fn recv_clipboard(sync: &mut Option<crate::clipboard::ClipboardSync>) -> Option<String> {
    match sync {
        Some(s) => s.changed.recv().await,
        None => std::future::pending().await,
    }
}

/// winit names keys after the W3C UI Events `code` values, except that it
/// calls the OS key "Super" where W3C says "Meta".
fn w3c_code(code: KeyCode) -> Option<String> {
    let name = match code {
        KeyCode::SuperLeft | KeyCode::Meta => "MetaLeft".to_owned(),
        KeyCode::SuperRight => "MetaRight".to_owned(),
        other => format!("{other:?}"),
    };
    Some(name)
}

async fn send_ctrl(tx: &mpsc::Sender<NetCmd>, id: ChannelId, msg: &CtrlMsg) {
    let _ = tx
        .send(NetCmd::Send {
            id,
            binary: false,
            data: msg.encode().into_bytes(),
        })
        .await;
}

fn decode_loop(
    frames: std_mpsc::Receiver<EncodedIn>,
    shared: Arc<Shared>,
    proxy: EventLoopProxy<UserEvent>,
    net: mpsc::Sender<NetCmd>,
    ctrl: ChannelId,
) {
    // First snapshot a few seconds in, once the picture has settled.
    let mut next_snapshot = Instant::now() + Duration::from_secs(3);
    let mut decoder = match Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Closed(format!("{e:#}")));
            return;
        }
    };
    let mut out = I420Frame::new(2, 2);
    let ask_keyframe = |shared: &Shared| {
        // Rate-limited inside the session.
        let _ = net.try_send(NetCmd::RequestKeyframe);
        shared.stats.lock().expect("stats lock").keyframe_requests += 1;
    };
    while let Ok(f) = frames.recv() {
        if !f.contiguous {
            decoder.reset_until_keyframe();
        }
        if decoder.needs_keyframe() && !f.keyframe {
            ask_keyframe(&shared);
            continue;
        }
        let start = Instant::now();
        match decoder.decode(&f.data, f.keyframe, &mut out) {
            Ok(true) => {
                let decoded = start.elapsed();
                {
                    let mut s = shared.stats.lock().expect("stats lock");
                    s.frames_decoded += 1;
                    s.decode_time += decoded;
                    s.pipeline_time += f.arrived.elapsed();
                }
                if Instant::now() >= next_snapshot {
                    next_snapshot = Instant::now() + SNAPSHOT_INTERVAL;
                    send_snapshot(&net, ctrl, &out);
                }
                // Hand the picture to the window; reuse whatever it left behind.
                let previous = shared.mailbox.lock().expect("mailbox lock").replace(out);
                out = previous.unwrap_or_else(|| I420Frame::new(2, 2));
                if proxy.send_event(UserEvent::Frame).is_err() {
                    return;
                }
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!("{e:#}");
                ask_keyframe(&shared);
            }
        }
    }
}

/// Downscales `frame` by sampling and queues it for the host. Best effort:
/// dropped if the session queue is full.
fn send_snapshot(net: &mpsc::Sender<NetCmd>, ctrl: ChannelId, frame: &I420Frame) {
    let (w, h) = (
        (frame.width / SNAPSHOT_STEP) & !1,
        (frame.height / SNAPSHOT_STEP) & !1,
    );
    if w == 0 || h == 0 {
        return;
    }
    let mut data = Vec::with_capacity(w * h * 3 / 2);
    for y in 0..h {
        let row = y * SNAPSHOT_STEP * frame.width;
        data.extend((0..w).map(|x| frame.y[row + x * SNAPSHOT_STEP]));
    }
    let cw = frame.width / 2;
    for plane in [&frame.u, &frame.v] {
        for y in 0..h / 2 {
            let row = y * SNAPSHOT_STEP * cw;
            data.extend((0..w / 2).map(|x| plane[row + x * SNAPSHOT_STEP]));
        }
    }
    let header = CtrlMsg::Snapshot {
        width: w as u32,
        height: h as u32,
    };
    let header = NetCmd::Send {
        id: ctrl,
        binary: false,
        data: header.encode().into_bytes(),
    };
    // Chunked: one data-channel message must stay well under SCTP's limit.
    if net.try_send(header).is_ok() {
        for chunk in data.chunks(SNAPSHOT_CHUNK) {
            let cmd = NetCmd::Send {
                id: ctrl,
                binary: true,
                data: chunk.to_vec(),
            };
            if net.try_send(cmd).is_err() {
                return;
            }
        }
    }
}

/// Builds the title-bar line and the report for the host, then resets the counters.
fn status_line(
    shared: &Shared,
    rtt: Option<Duration>,
    host: Option<&CtrlMsg>,
    path: PathKind,
) -> (String, CtrlMsg) {
    let mut s = shared.stats.lock().expect("stats lock");
    let decoded = s.frames_decoded.max(1);
    let report = CtrlMsg::ViewerStats {
        fps: s.frames_drawn,
        decoded: s.frames_decoded,
        kbps: (s.bytes * 8 / 1000) as u32,
        decode_ms: s.decode_time.as_secs_f32() * 1e3 / decoded as f32,
        ready_ms: s.pipeline_time.as_secs_f32() * 1e3 / decoded as f32,
        keyframe_requests: s.keyframe_requests,
        gaps: s.gaps,
        max_stall_ms: s.max_stall.as_millis() as u32,
        rtt_ms: rtt.map(|r| r.as_millis() as u32),
    };
    let mut parts = vec![
        format!("{} fps", s.frames_drawn),
        format!("{:.1} Mbit/s", s.bytes as f64 * 8.0 / 1e6),
        format!(
            "decode {:.1} ms",
            s.decode_time.as_secs_f64() * 1e3 / decoded as f64
        ),
        format!(
            "recv→ready {:.1} ms",
            s.pipeline_time.as_secs_f64() * 1e3 / decoded as f64
        ),
    ];
    if let Some(CtrlMsg::HostStats {
        width,
        height,
        encode_ms,
        encoder,
        ..
    }) = host
    {
        parts.push(format!(
            "host {width}x{height} encode {encode_ms:.1} ms ({encoder})"
        ));
    }
    if let Some(rtt) = rtt {
        parts.push(format!("rtt {:.0} ms", rtt.as_secs_f64() * 1e3));
    }
    parts.push(
        match path {
            PathKind::DirectLan => "LAN",
            PathKind::Direct => "P2P",
            PathKind::RelayLocal | PathKind::RelayRemote => "relayed",
        }
        .to_owned(),
    );
    if s.keyframe_requests > 0 {
        parts.push(format!("{} keyframe requests", s.keyframe_requests));
    }
    if s.gaps > 0 {
        parts.push(format!("{} gaps", s.gaps));
    }
    *s = ViewStats::default();
    (parts.join(" · "), report)
}
