//! Times capture → convert → encode on the primary display with the encoder
//! the host would pick (pass `--software` to force openh264).
//!
//!   cargo run --release -p reemoth-video --example capture_bench

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    use reemoth_video::capture::{CaptureResult, DesktopCapture};

    let software = std::env::args().any(|a| a == "--software");
    let mut cap = DesktopCapture::primary()?;
    let (w, h) = cap.size();
    println!("display {w}x{h}");
    let (mut enc, reports) = reemoth_video::open_best(w, h, 60, 15_000_000, !software)?;
    for r in reports {
        println!("  tested {}: {:?} {:?}", r.name, r.encode_ms, r.error);
    }
    println!("encoder: {}", enc.name());
    let (mut total, mut bytes, mut frames, mut keyframes) = (Duration::ZERO, 0usize, 0u32, 0u32);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let CaptureResult::Frame(f) = cap.next_frame(Duration::from_millis(100))? else {
            continue;
        };
        let t = Instant::now();
        if frames.is_multiple_of(60) {
            enc.force_keyframe();
        }
        if let Some(e) = enc.encode_bgra(f.bgra, f.width, f.height, f.stride)? {
            total += t.elapsed();
            bytes += e.data.len();
            keyframes += u32::from(e.keyframe);
            frames += 1;
        }
    }
    println!(
        "{frames} frames ({keyframes} keyframes) in 5 s: convert+encode {:.2} ms per frame, {:.1} Mbit/s",
        total.as_secs_f64() * 1e3 / f64::from(frames.max(1)),
        bytes as f64 * 8.0 / 5.0 / 1e6
    );
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("capture_bench is Windows-only");
}
