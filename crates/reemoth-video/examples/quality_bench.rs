//! Measures picture fidelity: encode a real desktop frame, decode it like
//! the viewer does, compare with the original (SSIM and PSNR on luma).
//!
//!   cargo run --release -p reemoth-video --example quality_bench
//!
//! "still": the same frame encoded repeatedly (an idle desktop).
//! "scroll": the frame shifted up 8 px per frame (scrolling a page), the
//! hard case for a fixed bitrate.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::Duration;

    use reemoth_video::capture::{CaptureResult, DesktopCapture};
    use reemoth_video::{Decoder, I420Frame};

    let mut cap = DesktopCapture::primary()?;
    let (w, h) = cap.size();
    let (w, h) = (w & !1, h & !1);
    let src = loop {
        if let CaptureResult::Frame(f) = cap.next_frame(Duration::from_millis(500))? {
            break f
                .bgra
                .chunks(f.stride)
                .take(h)
                .flat_map(|r| &r[..w * 4])
                .copied()
                .collect::<Vec<u8>>();
        }
        if let Some(f) = cap.last_frame()? {
            break f
                .bgra
                .chunks(f.stride)
                .take(h)
                .flat_map(|r| &r[..w * 4])
                .copied()
                .collect::<Vec<u8>>();
        }
    };
    println!(
        "frame {w}x{h}; SSIM 100% = identical, PSNR in dB (40+ very good, 45+ near-transparent)\n"
    );
    println!(
        "{:<34} {:>7} {:>16} {:>16}",
        "encoder", "Mbit/s", "still SSIM/PSNR", "scroll SSIM/PSNR"
    );

    for hardware in [true, false] {
        for mbps in [4u32, 8, 20] {
            let mut results = Vec::new();
            let mut name = String::new();
            for scroll in [false, true] {
                let (mut enc, _) = reemoth_video::open_best(w, h, 60, mbps * 1_000_000, hardware)?;
                if hardware && !enc.is_hardware() {
                    return Ok(());
                }
                name = enc.name();
                let mut dec = Decoder::new()?;
                let mut out = I420Frame::new(2, 2);
                let mut reference = I420Frame::new(2, 2);
                let (mut ssim_sum, mut psnr_sum, mut n) = (0.0, 0.0, 0);
                for i in 0..60 {
                    let shift = if scroll { (i * 8) % h } else { 0 };
                    let mut frame = Vec::with_capacity(src.len());
                    frame.extend_from_slice(&src[shift * w * 4..]);
                    frame.extend_from_slice(&src[..shift * w * 4]);
                    let Some(e) = enc.encode_bgra(&frame, w, h, w * 4)? else {
                        continue;
                    };
                    if !dec.decode(&e.data, e.keyframe, &mut out)? {
                        continue;
                    }
                    // Skip the first second: the stream settles after the keyframe.
                    if i < 30 {
                        continue;
                    }
                    reference.fill_from_bgra(&frame, w, h, w * 4)?;
                    ssim_sum += ssim(&reference.y, &out.y, w, h);
                    psnr_sum += psnr(&reference.y, &out.y);
                    n += 1;
                }
                results.push((ssim_sum / n.max(1) as f64, psnr_sum / n.max(1) as f64));
            }
            println!(
                "{:<34} {:>7} {:>8.2}% {:>5.1}dB {:>8.2}% {:>5.1}dB",
                name,
                mbps,
                results[0].0 * 100.0,
                results[0].1,
                results[1].0 * 100.0,
                results[1].1
            );
        }
    }
    Ok(())
}

/// Mean SSIM over 8×8 blocks (luma, 8-bit).
#[cfg(windows)]
fn ssim(a: &[u8], b: &[u8], w: usize, h: usize) -> f64 {
    const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
    let (mut total, mut blocks) = (0.0, 0);
    for by in (0..h - 7).step_by(8) {
        for bx in (0..w - 7).step_by(8) {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0.0, 0.0, 0.0, 0.0, 0.0);
            for y in by..by + 8 {
                for x in bx..bx + 8 {
                    let (p, q) = (f64::from(a[y * w + x]), f64::from(b[y * w + x]));
                    sa += p;
                    sb += q;
                    saa += p * p;
                    sbb += q * q;
                    sab += p * q;
                }
            }
            let n = 64.0;
            let (ma, mb) = (sa / n, sb / n);
            let (va, vb, cov) = (saa / n - ma * ma, sbb / n - mb * mb, sab / n - ma * mb);
            total += ((2.0 * ma * mb + C1) * (2.0 * cov + C2))
                / ((ma * ma + mb * mb + C1) * (va + vb + C2));
            blocks += 1;
        }
    }
    total / f64::from(blocks)
}

#[cfg(windows)]
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a
        .iter()
        .zip(b)
        .map(|(p, q)| (f64::from(*p) - f64::from(*q)).powi(2))
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("quality_bench is Windows-only");
}
