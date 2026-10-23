//! Times capture → BGRA-to-I420 → software H.264 on the primary display.
//!
//!   cargo run --release -p reemoth-video --example capture_bench

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    use reemoth_video::capture::{CaptureResult, DesktopCapture};
    use reemoth_video::{Encoder, EncoderSettings, I420Frame};

    let mut cap = DesktopCapture::primary()?;
    println!("display {}x{}", cap.size().0, cap.size().1);
    let mut enc = Encoder::new(EncoderSettings {
        bitrate_bps: 15_000_000,
        ..Default::default()
    })?;
    let mut frame = I420Frame::new(2, 2);
    let (mut conv, mut encode, mut bytes, mut frames) =
        (Duration::ZERO, Duration::ZERO, 0usize, 0u32);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let CaptureResult::Frame(f) = cap.next_frame(Duration::from_millis(100))? else {
            continue;
        };
        let t = Instant::now();
        frame.fill_from_bgra(f.bgra, f.width, f.height, f.stride)?;
        conv += t.elapsed();
        if frames.is_multiple_of(60) {
            enc.force_keyframe();
        }
        if let Some(e) = enc.encode(&frame)? {
            encode += e.encode_time;
            bytes += e.data.len();
        }
        frames += 1;
    }
    let n = frames.max(1);
    println!(
        "{frames} frames in 5 s ({:.0} fps of screen changes): convert {:.2} ms, encode {:.2} ms per frame, {:.1} Mbit/s",
        frames as f64 / 5.0,
        conv.as_secs_f64() * 1e3 / n as f64,
        encode.as_secs_f64() * 1e3 / n as f64,
        bytes as f64 * 8.0 / 5.0 / 1e6
    );
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("capture_bench is Windows-only");
}
