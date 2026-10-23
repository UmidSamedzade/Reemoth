//! Host side of a video session: capture → convert → encode on a dedicated
//! thread, frames handed to the network task.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reemoth_net::NetCmd;
use tokio::sync::mpsc;

/// Frame rate cap, regardless of the display's refresh rate.
pub const MAX_FPS: f64 = 60.0;
/// Encoder target never leaves this range, whatever bandwidth estimation says.
const MIN_BITRATE: u32 = 1_000_000;
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
    pub encoder: String,
}

impl StreamControl {
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    /// Applies a bandwidth estimate, keeping headroom for retransmissions and
    /// the data channel.
    pub fn set_available_bandwidth(&self, bps: u64) {
        let target = ((bps as f64 * 0.85) as u64).clamp(MIN_BITRATE.into(), MAX_BITRATE.into());
        self.bitrate.store(target as u32, Ordering::Relaxed);
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
    use reemoth_video::capture::{CaptureResult, DesktopCapture};
    use reemoth_video::{Encoder, EncoderSettings, I420Frame};

    pub fn run(tx: &mpsc::Sender<NetCmd>, control: &StreamControl) -> anyhow::Result<()> {
        let mut capture = DesktopCapture::primary()?;
        let mut encoder = Encoder::new(EncoderSettings {
            bitrate_bps: 4_000_000,
            max_fps: MAX_FPS as f32,
            threads: 4,
        })?;
        control.bitrate.store(encoder.bitrate(), Ordering::Relaxed);
        {
            let mut s = control.stats.lock().expect("stats lock");
            s.encoder = "openh264 (software)".into();
        }
        let mut frame = I420Frame::new(2, 2);
        let mut have_frame = false;
        let start = Instant::now();
        let interval = Duration::from_secs_f64(1.0 / MAX_FPS);
        let mut next_due = Instant::now();
        // The viewer cannot decode anything before the first keyframe.
        control.keyframe.store(true, Ordering::Relaxed);

        while !control.stop.load(Ordering::Relaxed) {
            // Pace to MAX_FPS: wait until the next slot, then take whatever is newest.
            let now = Instant::now();
            if next_due > now {
                std::thread::sleep(next_due - now);
            }
            let wants_keyframe = control.keyframe.swap(false, Ordering::Relaxed);
            // When a keyframe is wanted but the screen is static, re-encode the last image.
            let timeout = if wants_keyframe && have_frame {
                Duration::ZERO
            } else {
                Duration::from_millis(100)
            };
            let captured_at = Instant::now();
            match capture.next_frame(timeout)? {
                CaptureResult::Frame(f) => {
                    frame.fill_from_bgra(f.bgra, f.width, f.height, f.stride)?;
                    have_frame = true;
                }
                CaptureResult::Unchanged if wants_keyframe && have_frame => {}
                CaptureResult::Unchanged => {
                    if wants_keyframe {
                        control.keyframe.store(true, Ordering::Relaxed);
                    }
                    continue;
                }
            }
            next_due = captured_at + interval;

            encoder.set_bitrate(control.bitrate.load(Ordering::Relaxed));
            if wants_keyframe {
                encoder.force_keyframe();
            }
            let Some(encoded) = encoder.encode(&frame)? else {
                if wants_keyframe {
                    control.keyframe.store(true, Ordering::Relaxed);
                }
                continue;
            };
            {
                let mut s = control.stats.lock().expect("stats lock");
                s.width = frame.width as u32;
                s.height = frame.height as u32;
                s.frames += 1;
                s.encode_time += captured_at.elapsed();
                s.bitrate_bps = encoder.bitrate();
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
