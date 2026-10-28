//! H.264 via openh264 (software). Hardware encoders plug in behind the same
//! interface later.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use openh264::OpenH264API;
use openh264::decoder::{DecodedYUV, DecoderConfig};
use openh264::encoder::{
    BitRate, Complexity, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, QpRange,
    RateControlMode, UsageType,
};
use openh264::formats::{YUVSlices, YUVSource};
use openh264_sys2::{
    ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SBitrateInfo, SPATIAL_LAYER_ALL,
};

use crate::I420Frame;

#[derive(Debug, Clone, Copy)]
pub struct EncoderSettings {
    pub bitrate_bps: u32,
    pub max_fps: f32,
    pub threads: u16,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self {
            bitrate_bps: 4_000_000,
            max_fps: 60.0,
            threads: 4,
        }
    }
}

pub struct EncodedFrame {
    /// Annex B byte stream (SPS/PPS precede every keyframe).
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub encode_time: Duration,
}

pub struct Encoder {
    inner: openh264::encoder::Encoder,
    bitrate_bps: u32,
}

impl Encoder {
    pub fn new(settings: EncoderSettings) -> anyhow::Result<Self> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(settings.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(settings.max_fps))
            // Never skip: a skipped keyframe leaves the viewer with nothing to
            // decode. Over-budget frames get coarser quantization instead.
            .skip_frames(false)
            .complexity(Complexity::Low)
            .qp(QpRange::new(10, 40))
            // No periodic keyframes: they are bitrate spikes. The viewer asks
            // for one when it needs it.
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            .num_threads(settings.threads);
        let inner = openh264::encoder::Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("creating H.264 encoder")?;
        Ok(Self {
            inner,
            bitrate_bps: settings.bitrate_bps,
        })
    }

    /// Changes the target bitrate without a keyframe.
    pub fn set_bitrate(&mut self, bps: u32) {
        if bps == self.bitrate_bps {
            return;
        }
        self.bitrate_bps = bps;
        for option in [ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE] {
            let mut info = SBitrateInfo {
                iLayer: SPATIAL_LAYER_ALL,
                iBitrate: bps as i32,
            };
            // SAFETY: the encoder is initialized once a frame has been encoded
            // (and SetOption before that is harmless); `info` outlives the call.
            let rc = unsafe {
                self.inner
                    .raw_api()
                    .set_option(option, (&raw mut info).cast())
            };
            if rc != 0 {
                tracing::debug!("openh264 SetOption({option}) returned {rc}");
            }
        }
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate_bps
    }

    pub fn force_keyframe(&mut self) {
        self.inner.force_intra_frame();
    }

    /// Returns `None` when the rate controller skipped this frame.
    pub fn encode(&mut self, frame: &I420Frame) -> anyhow::Result<Option<EncodedFrame>> {
        let start = Instant::now();
        let w = frame.width;
        let src = YUVSlices::new(
            (&frame.y, &frame.u, &frame.v),
            (w, frame.height),
            (w, w / 2, w / 2),
        );
        let stream = self.inner.encode(&src).context("H.264 encode")?;
        let keyframe = match stream.frame_type() {
            FrameType::Skip | FrameType::Invalid => return Ok(None),
            FrameType::IDR | FrameType::I => true,
            FrameType::P | FrameType::IPMixed => false,
        };
        let data = stream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame {
            data,
            keyframe,
            encode_time: start.elapsed(),
        }))
    }
}

pub struct Decoder {
    inner: openh264::decoder::Decoder,
    /// After loss, drop frames until a keyframe arrives (decoding P-frames
    /// against missing references only produces garbage).
    waiting_for_keyframe: bool,
}

impl Decoder {
    pub fn new() -> anyhow::Result<Self> {
        let inner = openh264::decoder::Decoder::with_api_config(
            OpenH264API::from_source(),
            DecoderConfig::new(),
        )
        .context("creating H.264 decoder")?;
        Ok(Self {
            inner,
            waiting_for_keyframe: true,
        })
    }

    /// Marks the stream broken (packet loss); decoding resumes at the next keyframe.
    pub fn reset_until_keyframe(&mut self) {
        self.waiting_for_keyframe = true;
    }

    pub fn needs_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }

    /// Decodes one Annex B frame into `out`. Returns false if no picture came
    /// out (waiting for a keyframe, or the decoder buffered it).
    pub fn decode(
        &mut self,
        data: &[u8],
        keyframe: bool,
        out: &mut I420Frame,
    ) -> anyhow::Result<bool> {
        if self.waiting_for_keyframe {
            if !keyframe {
                return Ok(false);
            }
            self.waiting_for_keyframe = false;
        }
        match self.inner.decode(data) {
            Ok(Some(yuv)) => {
                copy_decoded(&yuv, out);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => {
                self.waiting_for_keyframe = true;
                bail!("H.264 decode: {e}")
            }
        }
    }
}

fn copy_decoded(yuv: &DecodedYUV<'_>, out: &mut I420Frame) {
    let (w, h) = yuv.dimensions();
    let (w, h) = (w & !1, h & !1);
    if (out.width, out.height) != (w, h) {
        *out = I420Frame::new(w, h);
    }
    let (ys, us, vs) = yuv.strides();
    copy_plane(yuv.y(), ys, &mut out.y, w, h);
    copy_plane(yuv.u(), us, &mut out.u, w / 2, h / 2);
    copy_plane(yuv.v(), vs, &mut out.v, w / 2, h / 2);
}

fn copy_plane(src: &[u8], stride: usize, dst: &mut [u8], w: usize, h: usize) {
    for row in 0..h {
        dst[row * w..(row + 1) * w].copy_from_slice(&src[row * stride..row * stride + w]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: usize, h: usize, shift: usize) -> I420Frame {
        let mut bgra = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                bgra[i] = ((x + shift) % 256) as u8;
                bgra[i + 1] = (y % 256) as u8;
                bgra[i + 2] = ((x + y) % 256) as u8;
                bgra[i + 3] = 255;
            }
        }
        let mut f = I420Frame::new(2, 2);
        f.fill_from_bgra(&bgra, w, h, w * 4).unwrap();
        f
    }

    #[test]
    fn encode_decode_round_trip() {
        let (w, h) = (320, 240);
        let mut enc = Encoder::new(EncoderSettings {
            bitrate_bps: 2_000_000,
            max_fps: 30.0,
            threads: 1,
        })
        .unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut out = I420Frame::new(2, 2);

        let mut decoded = 0;
        for i in 0..10 {
            let src = gradient(w, h, i * 4);
            let Some(e) = enc.encode(&src).unwrap() else {
                continue;
            };
            assert_eq!(e.keyframe, i == 0, "only the first frame is a keyframe");
            if dec.decode(&e.data, e.keyframe, &mut out).unwrap() {
                decoded += 1;
                assert_eq!((out.width, out.height), (w, h));
                // Lossy, but close: mean luma error stays small.
                let err: u64 = out
                    .y
                    .iter()
                    .zip(&src.y)
                    .map(|(a, b)| (i16::from(*a) - i16::from(*b)).unsigned_abs() as u64)
                    .sum();
                assert!(
                    err / (w * h) as u64 <= 6,
                    "mean error {}",
                    err / (w * h) as u64
                );
            }
        }
        assert!(decoded >= 8, "decoded {decoded} of 10");
        enc.set_bitrate(1_000_000);
        enc.force_keyframe();
        let e = enc.encode(&gradient(w, h, 99)).unwrap().unwrap();
        assert!(e.keyframe);
    }

    #[test]
    fn decoder_waits_for_keyframe_after_loss() {
        let (w, h) = (160, 120);
        let mut enc = Encoder::new(EncoderSettings {
            threads: 1,
            ..Default::default()
        })
        .unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut out = I420Frame::new(2, 2);
        let first = enc.encode(&gradient(w, h, 0)).unwrap().unwrap();
        let second = enc.encode(&gradient(w, h, 8)).unwrap().unwrap();
        // A P-frame first is refused.
        assert!(!dec.decode(&second.data, second.keyframe, &mut out).unwrap());
        assert!(dec.decode(&first.data, first.keyframe, &mut out).unwrap());
    }
}
