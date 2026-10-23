//! Wire types shared by the Reemoth app and the signaling Worker.
//!
//! This crate must stay `wasm32-unknown-unknown` compatible: it is compiled
//! into the Cloudflare Worker as well as the native app.

pub mod peer;
pub mod signal;
