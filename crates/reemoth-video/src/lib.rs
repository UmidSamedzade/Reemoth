//! Video pipeline pieces for Reemoth.
//!
//! - [`capture`]: desktop capture (Windows: DXGI Desktop Duplication).
//! - [`I420Frame`]: planar YUV 4:2:0, BT.709 limited range, the format both
//!   the encoder and the viewer's shader agree on.
//! - [`Encoder`] / [`Decoder`]: H.264, Annex B byte streams.

#[cfg(windows)]
pub mod capture;
mod codec;
mod frame;

pub use codec::{Decoder, EncodedFrame, Encoder, EncoderSettings};
pub use frame::I420Frame;
