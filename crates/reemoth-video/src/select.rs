//! Picks the best working H.264 encoder on this machine.
//!
//! Every candidate is proven before use: it encodes a short moving test
//! pattern, and the result must decode with the viewer's decoder. A hardware
//! encoder that is present but broken, misconfigured, or emits a profile the
//! viewer can't read is skipped, and software encoding is always the floor.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use yuv::{BufferStoreMut, YuvBiPlanarImageMut, YuvConversionMode, YuvRange, YuvStandardMatrix};

use crate::{Decoder, EncodedFrame, Encoder, EncoderSettings, I420Frame};

const TEST_FRAMES: usize = 12;

#[derive(Debug, Clone)]
pub struct EncoderReport {
    pub name: String,
    pub hardware: bool,
    /// Mean encode time per test frame, if the encoder passed.
    pub encode_ms: Option<f64>,
    /// Why it was rejected.
    pub error: Option<String>,
}

impl EncoderReport {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }
}

/// Which hardware encoder (by name) passed for which size, so later sessions
/// skip the test. `None` = none passed; use software.
type Choice = ((u32, u32), Option<String>);
static CHOSEN: Mutex<Option<Choice>> = Mutex::new(None);

pub enum VideoEncoder {
    Software {
        enc: Box<Encoder>,
        frame: I420Frame,
    },
    #[cfg(windows)]
    Hardware {
        enc: crate::mf::MfEncoder,
        nv12: Vec<u8>,
    },
}

impl VideoEncoder {
    pub fn name(&self) -> String {
        match self {
            Self::Software { .. } => "openh264 (software)".into(),
            #[cfg(windows)]
            Self::Hardware { enc, .. } => format!("{} (hardware)", enc.name()),
        }
    }

    pub fn is_hardware(&self) -> bool {
        !matches!(self, Self::Software { .. })
    }

    pub fn set_bitrate(&mut self, bps: u32) {
        match self {
            Self::Software { enc, .. } => enc.set_bitrate(bps),
            #[cfg(windows)]
            Self::Hardware { enc, .. } => enc.set_bitrate(bps),
        }
    }

    pub fn force_keyframe(&mut self) {
        match self {
            Self::Software { enc, .. } => enc.force_keyframe(),
            #[cfg(windows)]
            Self::Hardware { enc, .. } => enc.force_keyframe(),
        }
    }

    /// Converts a BGRA image and encodes it. Odd edges are cropped. Returns
    /// `Ok(None)` when the encoder produced nothing for this frame.
    pub fn encode_bgra(
        &mut self,
        bgra: &[u8],
        width: usize,
        height: usize,
        stride: usize,
    ) -> anyhow::Result<Option<EncodedFrame>> {
        match self {
            Self::Software { enc, frame } => {
                frame.fill_from_bgra(bgra, width, height, stride)?;
                enc.encode(frame)
            }
            #[cfg(windows)]
            Self::Hardware { enc, nv12 } => {
                let (w, h) = enc.size();
                if (width & !1, height & !1) != (w as usize, h as usize) {
                    anyhow::bail!("screen size changed to {width}x{height}; encoder is {w}x{h}");
                }
                bgra_to_nv12(bgra, w as usize, h as usize, stride, nv12)?;
                enc.encode(nv12)
            }
        }
    }
}

/// Opens the best encoder for `width`×`height`. The first call per size runs
/// the self-test (a fraction of a second); the report says what was tried.
pub fn open_best(
    width: usize,
    height: usize,
    fps: u32,
    bitrate: u32,
    allow_hardware: bool,
) -> anyhow::Result<(VideoEncoder, Vec<EncoderReport>)> {
    let (w, h) = ((width & !1) as u32, (height & !1) as u32);
    let mut reports = Vec::new();

    #[cfg(windows)]
    if allow_hardware {
        let cached = CHOSEN.lock().expect("encoder cache").clone();
        let decision = match cached {
            Some((size, choice)) if size == (w, h) => choice,
            _ => {
                reports = probe_hardware(w, h);
                let choice = reports.iter().find(|r| r.ok()).map(|r| r.name.clone());
                *CHOSEN.lock().expect("encoder cache") = Some(((w, h), choice.clone()));
                choice
            }
        };
        if let Some(name) = decision {
            let infos = crate::mf::list_hardware_encoders().unwrap_or_else(|e| {
                tracing::warn!("listing hardware encoders failed: {e:#}; using software");
                Vec::new()
            });
            if let Some(info) = infos.iter().find(|i| i.name == name) {
                match crate::mf::MfEncoder::new(info, w, h, fps, bitrate) {
                    Ok(enc) => {
                        let nv12 = vec![0; (w * h * 3 / 2) as usize];
                        return Ok((VideoEncoder::Hardware { enc, nv12 }, reports));
                    }
                    Err(e) => tracing::warn!("{name} failed to open: {e:#}; using software"),
                }
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (allow_hardware, &CHOSEN);

    let enc = Encoder::new(EncoderSettings {
        bitrate_bps: bitrate,
        // Software can't sustain more than 60 fps at 1080p; budget accordingly.
        max_fps: fps.min(60) as f32,
        threads: 4,
    })?;
    Ok((
        VideoEncoder::Software {
            enc: Box::new(enc),
            frame: I420Frame::new(2, 2),
        },
        reports,
    ))
}

/// Forgets which hardware encoder passed, so the next `open_best` re-tests
/// (call after a hardware encoder failed mid-stream).
pub fn forget_hardware_choice() {
    *CHOSEN.lock().expect("encoder cache") = None;
}

/// Tests every encoder this machine offers at `width`×`height`, software included.
/// Mean decode time (ms) of the viewer's decoder at `width`×`height`.
pub fn probe_decoder(width: u32, height: u32) -> anyhow::Result<f64> {
    let (w, h) = (width as usize, height as usize);
    let mut enc = VideoEncoder::Software {
        enc: Box::new(Encoder::new(EncoderSettings {
            bitrate_bps: 8_000_000,
            max_fps: 60.0,
            threads: 4,
        })?),
        frame: I420Frame::new(2, 2),
    };
    let mut dec = Decoder::new()?;
    let mut out = I420Frame::new(2, 2);
    let mut bgra = vec![0u8; w * h * 4];
    let (mut n, mut total) = (0u32, Duration::ZERO);
    for i in 0..TEST_FRAMES {
        test_pattern(&mut bgra, w, h, i);
        let Some(frame) = enc.encode_bgra(&bgra, w, h, w * 4)? else {
            continue;
        };
        let start = Instant::now();
        if dec.decode(&frame.data, frame.keyframe, &mut out)? {
            total += start.elapsed();
            n += 1;
        }
    }
    anyhow::ensure!(n > 0, "decoder produced no pictures");
    Ok(total.as_secs_f64() * 1e3 / f64::from(n))
}

pub fn probe_all(width: u32, height: u32) -> Vec<EncoderReport> {
    #[cfg(windows)]
    let mut reports = probe_hardware(width, height);
    #[cfg(not(windows))]
    let mut reports = Vec::new();
    reports.push(probe("openh264 (software)", false, width, height, || {
        let enc = Encoder::new(EncoderSettings {
            bitrate_bps: 8_000_000,
            max_fps: 60.0,
            threads: 4,
        })?;
        Ok(VideoEncoder::Software {
            enc: Box::new(enc),
            frame: I420Frame::new(2, 2),
        })
    }));
    reports
}

#[cfg(windows)]
fn probe_hardware(width: u32, height: u32) -> Vec<EncoderReport> {
    let infos = match crate::mf::list_hardware_encoders() {
        Ok(infos) => infos,
        Err(e) => {
            return vec![EncoderReport {
                name: "Media Foundation".into(),
                hardware: true,
                encode_ms: None,
                error: Some(format!("{e:#}")),
            }];
        }
    };
    infos
        .iter()
        .map(|info| {
            probe(&info.name, true, width, height, || {
                let enc = crate::mf::MfEncoder::new(info, width, height, 60, 8_000_000)?;
                let nv12 = vec![0; (width * height * 3 / 2) as usize];
                Ok(VideoEncoder::Hardware { enc, nv12 })
            })
        })
        .collect()
}

fn probe(
    name: &str,
    hardware: bool,
    width: u32,
    height: u32,
    open: impl FnOnce() -> anyhow::Result<VideoEncoder>,
) -> EncoderReport {
    let result = open().and_then(|mut enc| self_test(&mut enc, width as usize, height as usize));
    match result {
        Ok(ms) => EncoderReport {
            name: name.into(),
            hardware,
            encode_ms: Some(ms),
            error: None,
        },
        Err(e) => EncoderReport {
            name: name.into(),
            hardware,
            encode_ms: None,
            error: Some(format!("{e:#}")),
        },
    }
}

/// Encodes a moving pattern and decodes it with the viewer's decoder.
/// Returns the mean encode time in milliseconds.
fn self_test(enc: &mut VideoEncoder, w: usize, h: usize) -> anyhow::Result<f64> {
    let mut dec = Decoder::new()?;
    let mut out = I420Frame::new(2, 2);
    let mut bgra = vec![0u8; w * h * 4];
    let (mut decoded, mut total) = (0, Duration::ZERO);
    let mut first = true;
    for i in 0..TEST_FRAMES {
        test_pattern(&mut bgra, w, h, i);
        let start = Instant::now();
        let Some(frame) = enc.encode_bgra(&bgra, w, h, w * 4)? else {
            continue;
        };
        total += start.elapsed();
        if first {
            anyhow::ensure!(frame.keyframe, "first frame was not a keyframe");
            first = false;
        }
        if dec
            .decode(&frame.data, frame.keyframe, &mut out)
            .context("viewer's decoder rejected this encoder's output")?
        {
            anyhow::ensure!(
                (out.width, out.height) == (w & !1, h & !1),
                "decoded size {}x{} differs from {w}x{h}",
                out.width,
                out.height
            );
            decoded += 1;
        }
    }
    anyhow::ensure!(
        decoded >= TEST_FRAMES * 2 / 3,
        "only {decoded} of {TEST_FRAMES} test frames decoded"
    );
    Ok(total.as_secs_f64() * 1e3 / f64::from(decoded as u32))
}

/// Gradients plus a moving bar, so frames differ and have detail.
fn test_pattern(bgra: &mut [u8], w: usize, h: usize, i: usize) {
    let bar = (i * 37) % w.max(1);
    for y in 0..h {
        for x in 0..w {
            let p = (y * w + x) * 4;
            let on_bar = x.abs_diff(bar) < 24;
            bgra[p] = if on_bar {
                255
            } else {
                (x * 255 / w.max(1)) as u8
            };
            bgra[p + 1] = (y * 255 / h.max(1)) as u8;
            bgra[p + 2] = ((x + y + i * 8) % 256) as u8;
            bgra[p + 3] = 255;
        }
    }
}

#[cfg(windows)]
fn bgra_to_nv12(
    bgra: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    out: &mut Vec<u8>,
) -> anyhow::Result<()> {
    out.resize(w * h * 3 / 2, 0);
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    let mut image = YuvBiPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(y_plane),
        y_stride: w as u32,
        uv_plane: BufferStoreMut::Borrowed(uv_plane),
        uv_stride: w as u32,
        width: w as u32,
        height: h as u32,
    };
    yuv::bgra_to_yuv_nv12(
        &mut image,
        bgra,
        stride as u32,
        YuvRange::Limited,
        YuvStandardMatrix::Bt709,
        YuvConversionMode::Balanced,
    )
    .context("BGRA to NV12 conversion")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Consumer NVIDIA GPUs allow only a handful of encoder sessions; an
    /// encoder that is not shut down on drop runs out after a few reopens.
    #[cfg(windows)]
    #[test]
    fn hardware_encoder_reopens_without_leaking() {
        let Some(info) = crate::mf::list_hardware_encoders()
            .ok()
            .and_then(|l| l.into_iter().next())
        else {
            eprintln!("no hardware encoder; skipping");
            return;
        };
        for i in 0..12 {
            let enc = crate::mf::MfEncoder::new(&info, 640, 360, 60, 4_000_000);
            assert!(enc.is_ok(), "open #{i} failed: {:?}", enc.err());
        }
    }

    #[test]
    fn software_encoder_passes_self_test() {
        let reports = probe_all(320, 240);
        let sw = reports.last().unwrap();
        assert!(sw.ok(), "{:?}", sw.error);
        for r in &reports {
            eprintln!(
                "{} hw={} ms={:?} err={:?}",
                r.name, r.hardware, r.encode_ms, r.error
            );
        }
    }
}
