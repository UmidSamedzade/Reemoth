//! Video pipeline pieces for Reemoth.
//!
//! - [`capture`]: desktop capture (Windows: DXGI Desktop Duplication).
//! - [`I420Frame`]: planar YUV 4:2:0, BT.709 limited range, the format both
//!   the encoder and the viewer's shader agree on.
//! - [`Encoder`] / [`Decoder`]: H.264, Annex B byte streams (openh264).
//! - [`open_best`]: hardware H.264 (Media Foundation on Windows: NVENC, AMF,
//!   Quick Sync) when it passes a self-test, software otherwise.

#[cfg(windows)]
pub mod capture;
mod codec;
mod frame;
#[cfg(windows)]
mod mf;
mod select;

pub use codec::{Decoder, EncodedFrame, Encoder, EncoderSettings};
pub use frame::I420Frame;
pub use select::{
    EncoderReport, VideoEncoder, forget_hardware_choice, open_best, probe_all, probe_decoder,
};
