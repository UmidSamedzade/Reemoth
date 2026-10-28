//! Host-side view of a session: one log line per second combining what the
//! host sent with what the viewer reports, plus the viewer's periodic
//! snapshot saved as a PNG. Lets the host operator (or a tool reading the
//! host's terminal) judge a session without looking at the viewer's screen.

use std::path::PathBuf;

use anyhow::{Context, bail};
use reemoth_proto::peer::CtrlMsg;
use yuv::{YuvPlanarImage, YuvRange, YuvStandardMatrix};

use crate::stream::Stats;

/// `<temp>/reemoth/viewer-<session>.png`, overwritten by each snapshot.
pub fn snapshot_path(tag: &str) -> PathBuf {
    std::env::temp_dir()
        .join("reemoth")
        .join(format!("viewer-{tag}.png"))
}

/// Bytes of a `width`×`height` I420 snapshot.
pub fn snapshot_len(width: u32, height: u32) -> usize {
    let (w, h) = (width as usize, height as usize);
    w * h + 2 * (w / 2) * (h / 2)
}

pub fn save_snapshot(tag: &str, width: u32, height: u32, i420: &[u8]) -> anyhow::Result<PathBuf> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 || w > 4096 || h > 4096 {
        bail!("bad snapshot size {width}x{height}");
    }
    let (ys, cs) = (w * h, (w / 2) * (h / 2));
    if i420.len() != ys + 2 * cs {
        bail!("snapshot is {} bytes, expected {}", i420.len(), ys + 2 * cs);
    }
    let image = YuvPlanarImage {
        y_plane: &i420[..ys],
        y_stride: width,
        u_plane: &i420[ys..ys + cs],
        u_stride: width / 2,
        v_plane: &i420[ys + cs..],
        v_stride: width / 2,
        width,
        height,
    };
    let mut rgb = vec![0u8; w * h * 3];
    yuv::yuv420_to_rgb(
        &image,
        &mut rgb,
        width * 3,
        YuvRange::Limited,
        YuvStandardMatrix::Bt709,
    )
    .context("snapshot color conversion")?;

    let path = snapshot_path(tag);
    std::fs::create_dir_all(path.parent().expect("has parent"))?;
    let tmp = path.with_extension("png.tmp");
    {
        let file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        let mut enc = png::Encoder::new(file, width, height);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(&rgb)?;
    }
    // Replace atomically so a reader never sees half a file.
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn stats_line(host: Option<&Stats>, viewer: &CtrlMsg) -> String {
    let CtrlMsg::ViewerStats {
        fps,
        decoded,
        kbps,
        decode_ms,
        ready_ms,
        keyframe_requests,
        gaps,
        max_stall_ms,
        rtt_ms,
    } = viewer
    else {
        return String::new();
    };
    let host_part = match host {
        Some(h) => {
            let enc = if h.frames == 0 {
                0.0
            } else {
                h.encode_time.as_secs_f64() * 1e3 / f64::from(h.frames)
            };
            format!(
                "host {}fps enc {:.1}ms {:.1}Mb/s (target {:.1}) kf {}",
                h.frames,
                enc,
                h.bytes as f64 * 8.0 / 1e6,
                f64::from(h.bitrate_bps) / 1e6,
                h.keyframes
            )
        }
        None => "host -".into(),
    };
    let rtt = rtt_ms.map_or("-".into(), |r| format!("{r}ms"));
    format!(
        "{host_part} | viewer {fps}fps (dec {decoded}) {:.1}Mb/s dec {decode_ms:.1}ms ready {ready_ms:.1}ms gaps {gaps} stall {max_stall_ms}ms kfreq {keyframe_requests} rtt {rtt}",
        f64::from(*kbps) / 1e3
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_a_snapshot_png() {
        let (w, h) = (8u32, 4u32);
        let mut data = vec![128u8; 8 * 4 + 2 * 4 * 2];
        data[..32].fill(200);
        let path = save_snapshot("test", w, h, &data).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
        assert!(save_snapshot("test", w, h, &data[..10]).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn formats_both_sides() {
        let v = CtrlMsg::ViewerStats {
            fps: 59,
            decoded: 59,
            kbps: 4000,
            decode_ms: 2.0,
            ready_ms: 2.5,
            keyframe_requests: 0,
            gaps: 1,
            max_stall_ms: 40,
            rtt_ms: Some(17),
        };
        let line = stats_line(None, &v);
        assert!(
            line.contains("viewer 59fps") && line.contains("rtt 17ms") && line.contains("gaps 1")
        );
    }
}
