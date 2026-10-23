# Reemoth — Plan

A remote desktop app built for the lowest possible latency. It connects peer-to-peer from anywhere, with no port forwarding.

## 1. Requirements (decided)

| Area | Decision |
|---|---|
| Platforms (v1) | Windows and Linux, as both host and client |
| Target | 1080p60 in good quality with minimum latency, over the **internet** (not tuned for LAN only) |
| Core stack | Rust core with a native UI (egui on wgpu) |
| Connectivity | P2P over UDP with NAT hole punching, and a managed TURN relay as fallback. No port forwarding. |
| Infra | Serverless and managed: Cloudflare Workers + Durable Objects for signaling, Cloudflare Realtime TURN for relay and STUN |
| Auth | Host password only. No accept prompt, so unattended access works. |
| v1 features | Video, mouse and keyboard, clipboard sync (text), audio streaming |
| Out of v1 | File transfer, multi-monitor switching (v1 streams the primary display), macOS, web and mobile clients |

## 2. Latency budget (1080p60, internet)

| Stage | Target |
|---|---|
| Capture (GPU, zero-copy) | ≤ 1–2 ms |
| Encode (HW, low-latency preset) | ≤ 3–5 ms |
| Packetize, pace, send | ≤ 1 ms |
| Network | one-way path delay (P2P direct whenever possible) |
| Jitter buffer | **0 frames**: show each frame as soon as it is complete |
| Decode (HW) | ≤ 2–4 ms |
| Render/present | ≤ 1 frame. Use a flip-model/mailbox swapchain, never vsync queueing. |
| **Glass-to-glass excluding network** | **≤ 15 ms goal, ≤ 25 ms hard ceiling** |

Bitrate is adaptive: about 6–25 Mbps for 1080p60, with a floor of 2 Mbps. When bandwidth drops, keep the frame rate and lower the bitrate first. Lower the resolution only as a last resort.

## 3. Architecture

```
            ┌─────────────────────────────────────────────┐
            │ Cloudflare                                   │
            │  Worker + Durable Object (per host ID)       │
            │   • host registration (WebSocket, hibernate) │
            │   • SDP/ICE relay, PAKE message relay        │
            │   • mints short-lived TURN credentials       │
            │  Realtime TURN/STUN (anycast)                │
            └──────▲──────────────────────────▲────────────┘
                   │ wss (signaling only)     │
         ┌─────────┴───────┐   UDP P2P   ┌────┴────────────┐
         │      HOST       │◄═══════════►│     CLIENT      │
         │ capture→encode  │ (or TURN)   │ decode→render   │
         │ input inject    │             │ input capture   │
         │ audio capture   │             │ audio playback  │
         └─────────────────┘             └─────────────────┘
```

### 3.1 Transport: WebRTC via `str0m` (sans-IO)
- **Why WebRTC:** it is a proven internet-scale low-latency stack (GeForce NOW runs at 60–240 fps on it). It provides ICE (hole punching plus TURN fallback), DTLS-SRTP encryption, NACK/RTX, transport-wide congestion control feedback, and SCTP data channels, all standards-based. A browser client stays possible later at no extra cost.
- **Why `str0m`:** it is sans-IO, so we own the socket loop, timers, and threads. There is no hidden runtime buffering and no hidden playout delay, and it is pure Rust.
- **Channels:**
  - Video: RTP (H.264 to start, then HEVC/AV1), with NACK and RTX for recovery.
  - Audio: RTP Opus with 10 ms frames.
  - `input`: unordered, unreliable data channel for mouse moves (only the newest position matters).
  - `ctrl`: reliable, ordered data channel for keys, buttons, clipboard, session control, and keyframe/LTR requests.
- **ICE:** gather host, srflx (Cloudflare STUN), and relay (Cloudflare TURN over UDP, then TCP/TLS 443 as a last resort) candidates. Use trickle ICE and aggressive nomination. Probe IPv6 too, because it often avoids NAT entirely.
- **Socket tuning:** large SO_SNDBUF/SO_RCVBUF, DSCP EF marking (best effort), a dedicated high-priority network thread, and packet pacing so keyframe bursts don't trigger router drops.

### 3.2 Congestion control and loss handling
- Use Google Congestion Control (delay-based plus loss-based) from TWCC feedback, through str0m's bandwidth estimator. Feed the estimate straight into the encoder's bitrate on every frame.
- **No periodic keyframes.** Use an infinite GOP with **intra-refresh** (a rolling I-slice band). This avoids big keyframe spikes.
- **Loss recovery ladder:**
  1. NACK/RTX if the RTT allows (one retransmit within about 1.5×RTT).
  2. Otherwise, the client reports the lost frame. The host encodes the next frame from the last acknowledged **long-term reference frame**, using NVENC/AMF/QSV reference invalidation.
  3. As a last resort, request an IDR.
- Optional later: FlexFEC/ULPFEC on lossy links, adaptive based on measured loss.

### 3.3 Video pipeline

| | Windows | Linux |
|---|---|---|
| Capture | DXGI Desktop Duplication (dirty rects, cursor separate), or Windows.Graphics.Capture as a fallback | PipeWire via xdg-desktop-portal ScreenCast (Wayland), X11 XShm + XDamage (X11) |
| Zero-copy | D3D11 texture → encoder input | DMA-BUF → VAAPI surface |
| Color convert | GPU shader BGRA→NV12 (or encoder-native RGB input) | VAAPI VPP / shader |
| HW encode | NVENC (NVIDIA Video Codec SDK), AMF (AMD), QSV/oneVPL (Intel) | NVENC, VAAPI (AMD/Intel) |
| SW fallback | openh264 or x264 `ultrafast`+`zerolatency` | same |
| HW decode | D3D11VA (via FFmpeg `hwaccel` or direct) | VAAPI / NVDEC |
| Render | wgpu (D3D12/Vulkan), NV12 sampled directly, mailbox present | wgpu (Vulkan) |

- **Encoder settings:** low-latency/ultra-low-latency tuning, CBR or capped VBR with a VBV of about 1 frame, no B-frames, no lookahead, 1 slice per frame (or several slices for parallel decode), and intra-refresh enabled.
- **Cursor:** send it as a separate overlay (shape plus position on `ctrl`/`input`) and draw it locally on the client. The pointer then feels instant even when video lags.
- **Frame pacing:** capture is driven by display updates. If nothing changed, send nothing, but keep the last frame alive.

### 3.4 Input
- **Client:** raw input (Windows Raw Input, Linux evdev/winit). Coalesce mouse moves per send tick, about 1 kHz at most, and translate keys by scancode, not by character.
- **Host, Windows:** `SendInput` with absolute coordinates on a virtual desktop. To work on UAC, the secure desktop, and the login screen, run as a **SYSTEM service** that launches a helper in the active session and follows desktop switches.
- **Host, Linux:** `uinput` virtual devices (works on X11 and Wayland, needs the `input` group or a udev rule), with the libei / portal RemoteDesktop API as the Wayland-sanctioned path.

### 3.5 Audio
- Capture: WASAPI loopback (Windows) and the PipeWire/PulseAudio monitor source (Linux).
- Encode with Opus at 48 kHz stereo, 10 ms frames, 128 kbps, with in-band FEC enabled.
- Play back through a tiny jitter buffer of about 20–30 ms. Drift-correct to the video clock, but never let audio delay video.

### 3.6 Clipboard
- Text only in v1. Watch the clipboard on both sides (Windows `AddClipboardFormatListener`, Linux `wl-clipboard`/X11 selection) and sync over `ctrl` with loop suppression. Cap the size at 1 MB.

## 4. Connection flow and security

1. The **host** generates a persistent Ed25519 identity, registers with signaling, and gets a short **9-digit ID**. The owner sets a **password**.
2. The **client** enters the ID and password. Signaling routes it to the host's Durable Object over WebSocket.
3. The two sides run a **PAKE (SPAKE2 or OPAQUE)** handshake relayed through signaling. Neither the server nor an eavesdropper ever sees the password or anything that allows an offline guess.
4. The PAKE session key **authenticates the SDP**, including both DTLS fingerprints, with an HMAC. A compromised signaling server therefore cannot MITM the media path.
5. ICE runs (P2P first, TURN fallback), then DTLS. The session starts **without any accept prompt**, as requested.
6. **Brute-force protection:** exponential back-off and lockout per host ID, enforced both in the Durable Object and on the host itself. The host's stored verifier uses Argon2id. A minimum password strength is enforced in the UI.
7. TURN credentials are minted by the Worker and issued only after the host reports a successful PAKE. They last 4 hours, so a relayed session longer than that loses its relay until mid-session re-minting is added.

As built in M1:
- **ID ownership:** the first Ed25519 key to register an ID owns it, proven by signing a server challenge. Unanswered host sockets expire after 10 s, at most 2 can be pending, and they don't count toward the room's 8-socket cap.
- **Rate limits:**
  - 60 connects/min per IP across all rooms.
  - 20 client sessions/min per room.
  - Clients of an offline host are refused before anything is written to storage.
- **TURN abuse:** anyone can claim an ID and authorize their own sessions, so minting is capped at 30/day per room and 10/min per host IP. Past either cap, clients get STUN only. Set a Cloudflare budget alert on TURN usage.
- **Password guessing:**
  - The host evaluates one attempt at a time and backs off after 5 failures (5 s doubling to 15 min).
  - The back-off lives in host memory: it survives signaling reconnects but not a host restart.
  - Failure-based back-off in the Durable Object is still to do.

Also: an optional allow-list of client device keys (trust on first use), a session indicator on the host (no prompt, but visible), and an audit log of connections.

## 5. Serverless infrastructure
- **Signaling:** a Cloudflare Worker written in Rust (`workers-rs`), with one **Durable Object per host ID**. It uses the WebSocket Hibernation API, so an idle host connection costs almost nothing.
- **STUN:** `stun.cloudflare.com:3478`.
- **TURN:** Cloudflare Realtime TURN (anycast, UDP/TCP/TLS 443), billed per GB. This only matters for the minority of sessions behind symmetric NATs on both ends.
- **Config:** `wrangler.toml` plus secrets for the TURN key. Deployed via GitHub Actions.
- The server URLs are configurable, so the infrastructure stays portable (it can be swapped for self-hosted coturn plus a signaling binary).

## 6. Repository layout (Cargo workspace)

```
crates/
  reemoth-proto     # message types (ctrl/input/clipboard), serde+postcard
  reemoth-net       # str0m wrapper, socket loop, ICE/TURN, BWE plumbing
  reemoth-auth      # PAKE, identity keys, SDP binding, Argon2 verifier
  reemoth-capture   # dxgi / wgc / pipewire / x11 backends (trait Capturer)
  reemoth-encode    # nvenc / amf / qsv / vaapi / sw (trait Encoder)
  reemoth-decode    # d3d11va / vaapi / nvdec / sw (trait Decoder)
  reemoth-render    # wgpu NV12 renderer + cursor overlay
  reemoth-input     # client capture + host injection (SendInput / uinput)
  reemoth-audio     # wasapi / pipewire capture+playback, opus
  reemoth-clipboard
apps/
  reemoth           # single binary: egui UI, runs as host and/or client
  reemoth-service   # Windows SYSTEM service / Linux systemd unit
signaling/          # Cloudflare Worker (workers-rs) + Durable Object
tools/
  latency-probe     # glass-to-glass measurement (flash-frame + photodiode/screen timestamp)
```

## 7. Milestones

| # | Milestone | Done when |
|---|---|---|
| M0 ✅ | Workspace, CI (Windows + Linux builds, clippy, tests), signaling Worker skeleton deployed | `cargo build` green on both OSes; Worker answers a WebSocket ping |
| M1 ✅ | **Connectivity:** ID registration, PAKE, ICE via STUN/TURN, data channel echo | Two machines on different networks (one on mobile hotspot/CGNAT) connect with no port forwarding; TURN fallback verified by blocking UDP. *Status: done and tested locally (direct path, wrong-password refusal, latency and throughput); cross-network and live TURN tests are pending your two-PC run.* |
| M2 | **Video, Windows:** DXGI capture → NVENC H.264 → RTP → D3D11VA decode → wgpu present | 1080p60 stable across the internet, with on-screen stats overlay (fps, bitrate, RTT, loss, per-stage latency) |
| M3 | **Input + cursor overlay + clipboard** | Full desktop control; pointer feels local |
| M4 | **Adaptive quality:** GCC bitrate → encoder, intra-refresh, LTR/reference invalidation | At 2% packet loss and 100 ms RTT (netem), there are no freezes longer than 1 frame and no IDR storms |
| M5 | **Linux host + client:** PipeWire/X11 capture, VAAPI encode/decode, uinput | Linux↔Windows sessions in both directions at the M2 quality |
| M6 | **Audio** (WASAPI/PipeWire + Opus) | A/V in sync within ±40 ms, with no added video latency |
| M7 | **Encoder coverage:** AMF, QSV, VAAPI, SW fallback, plus auto-selection | Runs on NVIDIA, AMD, Intel, and no-GPU VMs |
| M8 | **Unattended host:** Windows service (UAC, lock screen), systemd unit, lockout, audit log, installers (MSI, .deb/AppImage) | Reboot the host and connect at the login screen |
| M9 | **Hardening and performance:** fuzz the proto parsers, security review, latency regression suite in CI | Glass-to-glass ≤ 25 ms (excluding network) on reference hardware |

## 8. Key risks

| Risk | Mitigation |
|---|---|
| Symmetric NAT on both sides | TURN fallback (Cloudflare anycast keeps the added latency small); IPv6 first |
| HW encoder fragmentation | Trait-based backends, a SW fallback, and a startup capability probe |
| Wayland capture/input restrictions | Portal ScreenCast + RemoteDesktop with a persistent restore token; uinput as fallback |
| No accept prompt means the password is the only gate | PAKE (no offline attack), rate limits on both ends, strength rules, optional device allow-list |
| str0m API gaps (e.g., LTR feedback) | Carry custom feedback on the `ctrl` channel; contribute upstream if needed |
| Windows secure desktop | SYSTEM service plus a session helper (the same approach RustDesk and Parsec use) |

## 9. Open questions (decide later)
- Use HEVC/AV1 by default when both ends support it? Benchmark the quality per bit against the added encode latency.
- Allow 4:4:4 chroma for text-heavy work (NVENC supports it, but decode support varies)?
- Licensing for NVENC SDK headers and FFmpeg (LGPL build) if we distribute binaries.
