//! Host side of a video session: capture → convert → encode on a dedicated
//! thread, frames handed to the network task.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reemoth_net::NetCmd;
use tokio::sync::mpsc;

/// Frame rate cap with a software encoder (it can't sustain more at 1080p).
const SOFTWARE_FPS: u32 = 60;
/// With a hardware encoder: frames are cheap, and a higher rate means the
/// viewer's next refresh always shows a fresher picture, even on a 60 Hz panel.
const HARDWARE_FPS: u32 = 120;
/// Encoder target never leaves this range, whatever bandwidth estimation says.
const MIN_BITRATE: u32 = 1_500_000;
const MAX_BITRATE: u32 = 20_000_000;

/// Shared between the network task and the capture thread.
#[derive(Default)]
pub struct StreamControl {
    stop: AtomicBool,
    keyframe: AtomicBool,
    bitrate: AtomicU32,
    pub stats: Mutex<Stats>,
}

#[derive(Default, Clone)]
pub struct Stats {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub encode_time: Duration,
    pub bitrate_bps: u32,
    pub bytes: u64,
    pub keyframes: u32,
    pub encoder: String,
}

impl StreamControl {
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    /// Loss-driven bitrate control from the viewer's once-a-second report:
    /// back off hard on loss, creep up while the link stays clean.
    pub fn on_viewer_report(&self, lost: bool) {
        let current = self.bitrate.load(Ordering::Relaxed).max(MIN_BITRATE);
        let next = if lost {
            (f64::from(current) * 0.7) as u32
        } else {
            (f64::from(current) * 1.1) as u32
        };
        self.bitrate
            .store(next.clamp(MIN_BITRATE, MAX_BITRATE), Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Returns and resets the per-second counters.
    pub fn take_stats(&self) -> Stats {
        let mut s = self.stats.lock().expect("stats lock");
        let out = s.clone();
        s.frames = 0;
        s.encode_time = Duration::ZERO;
        s.bytes = 0;
        s.keyframes = 0;
        out
    }
}

/// Starts streaming the primary display. The thread exits when `control` is
/// stopped or the network task goes away.
pub fn spawn(tx: mpsc::Sender<NetCmd>, control: Arc<StreamControl>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("reemoth-capture".into())
        .spawn(move || {
            if let Err(e) = platform::run(&tx, &control) {
                eprintln!("screen streaming stopped: {e:#}");
            }
        })
        .expect("spawning capture thread")
}

#[cfg(windows)]
mod platform {
    use super::*;
    use reemoth_video::VideoEncoder;
    use reemoth_video::capture::{CaptureResult, DesktopCapture};

    const START_BITRATE: u32 = 8_000_000;

    fn open_encoder(
        width: usize,
        height: usize,
        control: &StreamControl,
        allow_hw: bool,
    ) -> anyhow::Result<(VideoEncoder, Duration)> {
        let (encoder, reports) =
            reemoth_video::open_best(width, height, HARDWARE_FPS, START_BITRATE, allow_hw)?;
        for r in &reports {
            match (&r.error, r.encode_ms) {
                (None, Some(ms)) => println!("  encoder check: {} OK ({ms:.1} ms/frame)", r.name),
                (Some(e), _) => println!("  encoder check: {} unusable: {e}", r.name),
                _ => {}
            }
        }
        let fps = if encoder.is_hardware() {
            HARDWARE_FPS
        } else {
            SOFTWARE_FPS
        };
        println!("  encoding with {} at up to {fps} fps", encoder.name());
        control.stats.lock().expect("stats lock").encoder = encoder.name();
        Ok((encoder, Duration::from_secs_f64(1.0 / f64::from(fps))))
    }

    pub fn run(tx: &mpsc::Sender<NetCmd>, control: &StreamControl) -> anyhow::Result<()> {
        let mut capture = DesktopCapture::primary()?;
        let (w, h) = capture.size();
        let mut allow_hw = std::env::var_os("REEMOTH_SOFTWARE_ENCODER").is_none();
        let (first, mut interval) = open_encoder(w, h, control, allow_hw)?;
        // `None` only while being replaced, so the old one (and its GPU
        // session) is gone before the new one opens.
        let mut encoder = Some(first);
        let mut encoder_size = (w, h);
        control.bitrate.store(START_BITRATE, Ordering::Relaxed);
        let mut have_frame = false;
        // The latest captured image has not been sent (the encoder skipped it).
        let mut unsent = false;
        let start = Instant::now();
        let mut next_due = Instant::now();
        // The viewer cannot decode anything before the first keyframe.
        control.keyframe.store(true, Ordering::Relaxed);

        while !control.stop.load(Ordering::Relaxed) {
            // Pace to the frame-rate cap. When the screen was still, a change is
            // picked up as soon as it happens; only during continuous motion
            // does a frame wait for its slot.
            let now = Instant::now();
            if next_due > now {
                std::thread::sleep(next_due - now);
            }
            let wants_keyframe = control.keyframe.swap(false, Ordering::Relaxed);
            // Re-encode the last image when a keyframe is wanted or the rate
            // controller skipped it, even if the screen has gone still.
            let resend = have_frame && (wants_keyframe || unsent);
            let timeout = if resend {
                Duration::ZERO
            } else {
                Duration::from_millis(100)
            };
            let changed = matches!(capture.next_frame(timeout)?, CaptureResult::Frame(_));
            if !changed && !resend {
                if wants_keyframe {
                    control.keyframe.store(true, Ordering::Relaxed);
                }
                continue;
            }
            // Timed from here so the wait for a screen change is not counted.
            let captured_at = Instant::now();
            next_due = captured_at + interval;
            have_frame = true;
            unsent = true;
            let Some(f) = capture.last_frame()? else {
                // Capture was just reset (desktop switch); keep the request.
                if wants_keyframe {
                    control.keyframe.store(true, Ordering::Relaxed);
                }
                continue;
            };
            let (fw, fh) = (f.width, f.height);
            if (fw, fh) != encoder_size {
                // Resolution changed: a hardware encoder is fixed-size, so reopen.
                drop(encoder.take());
                let (e, i) = open_encoder(fw, fh, control, allow_hw)?;
                (encoder, interval, encoder_size) = (Some(e), i, (fw, fh));
                control.keyframe.store(true, Ordering::Relaxed);
                continue;
            }

            let enc = encoder
                .as_mut()
                .expect("encoder is only None while reopening");
            enc.set_bitrate(control.bitrate.load(Ordering::Relaxed));
            if wants_keyframe {
                enc.force_keyframe();
            }
            let encoded = match enc.encode_bgra(f.bgra, f.width, f.height, f.stride) {
                Ok(encoded) => encoded,
                Err(e) if enc.is_hardware() => {
                    // A GPU encoder failing mid-stream: carry on in software.
                    eprintln!("hardware encoder failed ({e:#}); switching to software encoding");
                    reemoth_video::forget_hardware_choice();
                    allow_hw = false;
                    drop(encoder.take());
                    let (e, i) = open_encoder(fw, fh, control, allow_hw)?;
                    (encoder, interval) = (Some(e), i);
                    control.keyframe.store(true, Ordering::Relaxed);
                    continue;
                }
                Err(e) => return Err(e),
            };
            let Some(encoded) = encoded else {
                // Over budget: `unsent` stays set, so the next slot retries.
                if wants_keyframe {
                    control.keyframe.store(true, Ordering::Relaxed);
                }
                continue;
            };
            {
                let mut s = control.stats.lock().expect("stats lock");
                s.width = (fw & !1) as u32;
                s.height = (fh & !1) as u32;
                s.frames += 1;
                s.encode_time += captured_at.elapsed();
                s.bitrate_bps = control.bitrate.load(Ordering::Relaxed);
                s.bytes += encoded.data.len() as u64;
                s.keyframes += u32::from(encoded.keyframe);
            }
            unsent = false;
            if wants_keyframe {
                tracing::debug!(
                    "forced keyframe -> encoder produced keyframe={}",
                    encoded.keyframe
                );
            }
            let rtp_time = (start.elapsed().as_secs_f64() * 90_000.0) as u64;
            let cmd = NetCmd::Video {
                data: encoded.data,
                rtp_time,
                captured: captured_at,
            };
            if tx.blocking_send(cmd).is_err() {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn run(_tx: &mpsc::Sender<NetCmd>, _control: &StreamControl) -> anyhow::Result<()> {
        anyhow::bail!(
            "screen capture on this OS arrives in M5 (Linux host); only Windows hosts stream for now"
        )
    }
}
