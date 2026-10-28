//! Hardware H.264 through Windows Media Foundation.
//!
//! GPU vendors ship their encoders as Media Foundation transforms (NVIDIA
//! NVENC, AMD AMF, Intel Quick Sync), so one code path covers all of them.
//! These transforms are asynchronous: they announce through events when they
//! want input and when output is ready.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFShutdown, IMFTransform,
    METransformHaveOutput, METransformNeedInput, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_FULL,
    MFSampleExtension_CleanPoint, MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{Interface, PWSTR};

use crate::EncodedFrame;

/// How long to wait for the encoder to hand back a frame.
const OUTPUT_TIMEOUT: Duration = Duration::from_millis(200);
/// Frames in a row without output before the encoder is declared dead.
const MAX_MISSED_FRAMES: u32 = 20;

/// A hardware encoder registered on this machine.
pub struct HwEncoderInfo {
    pub name: String,
    activate: IMFActivate,
}

/// Lists hardware H.264 encoders, best first (the system's own ordering).
pub fn list_hardware_encoders() -> anyhow::Result<Vec<HwEncoderInfo>> {
    init_mf()?;
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&output),
            &mut list,
            &mut count,
        )
    }
    .context("enumerating hardware encoders")?;
    let mut out = Vec::new();
    if list.is_null() {
        return Ok(out);
    }
    // SAFETY: MFTEnumEx returned `count` activation objects in a CoTaskMem array.
    let items = unsafe { std::slice::from_raw_parts_mut(list, count as usize) };
    for item in items.iter_mut() {
        if let Some(activate) = item.take() {
            let name =
                friendly_name(&activate).unwrap_or_else(|| "unknown hardware encoder".into());
            out.push(HwEncoderInfo { name, activate });
        }
    }
    unsafe { CoTaskMemFree(Some(list as *const _)) };
    Ok(out)
}

fn friendly_name(activate: &IMFActivate) -> Option<String> {
    let mut ptr = PWSTR::null();
    let mut len = 0u32;
    unsafe { activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len) }
        .ok()?;
    let name = unsafe { ptr.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(ptr.0 as *const _)) };
    name
}

fn init_mf() -> anyhow::Result<()> {
    // Harmless if this thread already initialized COM (S_FALSE / mode mismatch).
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("starting Media Foundation")
}

pub struct MfEncoder {
    name: String,
    /// Kept to shut the encoder down properly (see `Drop`).
    activate: IMFActivate,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: ICodecAPI,
    width: u32,
    height: u32,
    bitrate: u32,
    need_input: u32,
    provides_samples: bool,
    frame_index: u64,
    fps: u32,
    /// Encoded output collected but not yet returned.
    pending: Vec<(Vec<u8>, bool)>,
    missed: u32,
}

// SAFETY: the transform is used from one thread at a time (the capture thread).
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    pub fn new(
        info: &HwEncoderInfo,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> anyhow::Result<Self> {
        init_mf()?;
        let transform: IMFTransform = unsafe { info.activate.ActivateObject() }
            .with_context(|| format!("activating {}", info.name))?;
        // Hardware encoders are async and refuse to work until unlocked.
        if let Ok(attrs) = unsafe { transform.GetAttributes() } {
            unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                .context("unlocking async MFT")?;
        }
        let codec: ICodecAPI = transform.cast().context("encoder has no ICodecAPI")?;
        let events: IMFMediaEventGenerator = transform.cast().context("encoder is not async")?;

        // Low latency: no B-frames, no lookahead, one frame in, one frame out.
        set_codec(&codec, &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
        set_codec(
            &codec,
            &CODECAPI_AVEncCommonRateControlMode,
            VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
        );
        set_codec(
            &codec,
            &CODECAPI_AVEncCommonMeanBitRate,
            VARIANT::from(bitrate),
        );
        set_codec(
            &codec,
            &CODECAPI_AVEncMPVDefaultBPictureCount,
            VARIANT::from(0u32),
        );
        // Keyframes only on request: periodic ones are bitrate spikes.
        set_codec(&codec, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(0u32));

        // Output type must be set before the input type.
        let out_type = unsafe { MFCreateMediaType() }?;
        unsafe {
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            // Baseline: what every software and browser H.264 decoder handles.
            out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
            transform
                .SetOutputType(0, &out_type, 0)
                .context("encoder rejected H.264 output type")?;
        }
        let in_type = unsafe { MFCreateMediaType() }?;
        unsafe {
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform
                .SetInputType(0, &in_type, 0)
                .context("encoder rejected NV12 input")?;
        }
        let info_out = unsafe { transform.GetOutputStreamInfo(0) }?;
        let provides_samples = info_out.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(Self {
            name: info.name.clone(),
            activate: info.activate.clone(),
            transform,
            events,
            codec,
            width,
            height,
            bitrate,
            need_input: 0,
            provides_samples,
            frame_index: 0,
            fps,
            pending: Vec::new(),
            missed: 0,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn set_bitrate(&mut self, bps: u32) {
        if bps != self.bitrate {
            self.bitrate = bps;
            set_codec(
                &self.codec,
                &CODECAPI_AVEncCommonMeanBitRate,
                VARIANT::from(bps),
            );
        }
    }

    pub fn force_keyframe(&mut self) {
        set_codec(
            &self.codec,
            &CODECAPI_AVEncVideoForceKeyFrame,
            VARIANT::from(1u32),
        );
    }

    /// Encodes one NV12 frame (`width`×`height`, Y plane then interleaved UV).
    pub fn encode(&mut self, nv12: &[u8]) -> anyhow::Result<Option<EncodedFrame>> {
        let start = Instant::now();
        let expected = (self.width * self.height * 3 / 2) as usize;
        if nv12.len() != expected {
            bail!("NV12 frame is {} bytes, expected {expected}", nv12.len());
        }
        let deadline = start + OUTPUT_TIMEOUT;

        // Wait until the encoder asks for input, collecting any output on the way.
        while self.need_input == 0 {
            if !self.pump_event(deadline)? {
                bail!("hardware encoder stopped asking for input");
            }
        }
        let sample = self.make_sample(nv12)?;
        unsafe { self.transform.ProcessInput(0, &sample, 0) }.context("ProcessInput")?;
        self.need_input -= 1;
        self.frame_index += 1;

        // Low-latency mode returns this frame's output right away. A slow
        // frame (GPU busy) is not fatal: its output is returned next time.
        while self.pending.is_empty() && self.pump_event(deadline)? {}
        if self.pending.is_empty() {
            self.missed += 1;
            if self.missed >= MAX_MISSED_FRAMES {
                bail!("hardware encoder produced nothing for {MAX_MISSED_FRAMES} frames");
            }
            return Ok(None);
        }
        self.missed = 0;
        // Several outputs (e.g. parameter sets sent separately, or a late
        // frame) concatenate into one valid Annex B access unit sequence.
        let mut data = Vec::new();
        let mut keyframe = false;
        for (d, k) in self.pending.drain(..) {
            data.extend_from_slice(&d);
            keyframe |= k;
        }
        Ok(Some(EncodedFrame {
            data,
            keyframe,
            encode_time: start.elapsed(),
        }))
    }

    /// Handles one encoder event. Returns false if none arrived by `deadline`.
    fn pump_event(&mut self, deadline: Instant) -> anyhow::Result<bool> {
        let event = loop {
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => break e,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_micros(200))
                }
                Err(_) => return Ok(false),
            }
        };
        let kind = unsafe { event.GetType() }?;
        if kind == METransformNeedInput.0 as u32 {
            self.need_input += 1;
        } else if kind == METransformHaveOutput.0 as u32 {
            self.take_output()?;
        }
        Ok(true)
    }

    /// Collects the output announced by one `METransformHaveOutput` event.
    /// Async encoders allow exactly one `ProcessOutput` per event (another
    /// call fails with E_UNEXPECTED), except that a format change asks for a
    /// retry to fetch the same sample.
    fn take_output(&mut self) -> anyhow::Result<()> {
        loop {
            match self.process_output()? {
                Output::Sample(frame) => {
                    self.pending.push(frame);
                    return Ok(());
                }
                Output::StreamChanged => continue,
                Output::NeedMoreInput => return Ok(()),
            }
        }
    }

    fn process_output(&mut self) -> anyhow::Result<Output> {
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            ..Default::default()
        };
        if !self.provides_samples {
            let info = unsafe { self.transform.GetOutputStreamInfo(0) }?;
            let sample = unsafe { MFCreateSample() }?;
            let mem = unsafe { MFCreateMemoryBuffer(info.cbSize.max(1 << 20)) }?;
            unsafe { sample.AddBuffer(&mem) }?;
            buffer.pSample = std::mem::ManuallyDrop::new(Some(sample));
        }
        let mut status = 0u32;
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status)
        };
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffer.pSample) };
        drop(unsafe { std::mem::ManuallyDrop::take(&mut buffer.pEvents) });
        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                return Ok(Output::NeedMoreInput);
            }
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // Renegotiate; the pending sample is collected on the next pass.
                let t: IMFMediaType = unsafe { self.transform.GetOutputAvailableType(0, 0) }?;
                unsafe { self.transform.SetOutputType(0, &t, 0) }?;
                let info = unsafe { self.transform.GetOutputStreamInfo(0) }?;
                self.provides_samples =
                    info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
                return Ok(Output::StreamChanged);
            }
            Err(e) => return Err(e).context("ProcessOutput"),
        }
        match sample {
            Some(sample) => Ok(Output::Sample(read_sample(&sample)?)),
            None => Ok(Output::NeedMoreInput),
        }
    }

    fn make_sample(&self, nv12: &[u8]) -> anyhow::Result<IMFSample> {
        let sample = unsafe { MFCreateSample() }?;
        let buffer = unsafe { MFCreateMemoryBuffer(nv12.len() as u32) }?;
        unsafe {
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;
            sample.AddBuffer(&buffer)?;
            // 100 ns units.
            let duration = 10_000_000 / i64::from(self.fps);
            sample.SetSampleTime(self.frame_index as i64 * duration)?;
            sample.SetSampleDuration(duration)?;
        }
        Ok(sample)
    }
}

enum Output {
    Sample((Vec<u8>, bool)),
    StreamChanged,
    NeedMoreInput,
}

impl Drop for MfEncoder {
    /// Async transforms keep themselves alive (and hold a GPU encoder
    /// session, of which consumer NVIDIA cards allow only a few) until shut
    /// down explicitly; dropping the COM pointers is not enough.
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}

fn read_sample(sample: &IMFSample) -> anyhow::Result<(Vec<u8>, bool)> {
    let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }?;
    let mut data = Vec::new();
    unsafe {
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        data.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
        buffer.Unlock()?;
    }
    Ok((data, keyframe))
}

/// Best effort: not every encoder supports every knob.
fn set_codec(codec: &ICodecAPI, api: &windows::core::GUID, value: VARIANT) {
    if let Err(e) = unsafe { codec.SetValue(api, &value) } {
        tracing::debug!("encoder ignored setting {api:?}: {e}");
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}
