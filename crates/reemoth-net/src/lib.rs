//! P2P transport for Reemoth.
//!
//! [`gather`] binds UDP sockets and collects ICE candidates (host, STUN
//! server-reflexive, TURN relayed). [`session`] runs a str0m WebRTC session
//! over them on its own task and exposes data channels to the app.

pub mod gather;
pub mod session;
pub mod stun;
pub mod turn;

pub use gather::{Gathered, gather};
pub use session::{NetCmd, NetEvent, PathInfo, PathKind, PeerConnection, Prepared};
pub use str0m::channel::ChannelId;
