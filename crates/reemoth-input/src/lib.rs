//! Host side of remote control.
//!
//! - [`Injector`]: replays the viewer's mouse and keyboard on this machine.
//!   Keys arrive as W3C `code` names (physical positions), so the host's own
//!   keyboard layout decides which character they produce.
//! - [`CursorWatcher`]: reports the pointer shape so the viewer can show
//!   its own (zero-latency) pointer in the right shape.
//!
//! Only Windows is implemented; elsewhere both report themselves unsupported.

mod keymap;
#[cfg(windows)]
mod windows;

pub use keymap::windows_scancode;
#[cfg(windows)]
pub use windows::{CursorWatcher, Injector, enable_dpi_awareness, release_everything};

/// The screen area the stream shows, in desktop coordinates (pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl ScreenRect {
    /// Maps a normalized picture position (0..1) to a desktop pixel.
    pub fn to_pixel(self, x: f32, y: f32) -> (i32, i32) {
        let fx = x.clamp(0.0, 1.0) * (self.width - 1).max(0) as f32;
        let fy = y.clamp(0.0, 1.0) * (self.height - 1).max(0) as f32;
        (self.left + fx.round() as i32, self.top + fy.round() as i32)
    }
}

#[cfg(not(windows))]
mod unsupported {
    use reemoth_proto::peer::MouseButton;

    use crate::ScreenRect;

    pub struct Injector;

    impl Injector {
        pub fn new() -> anyhow::Result<Self> {
            anyhow::bail!("remote control of this OS is not implemented yet (M5)")
        }
        pub fn move_to(&mut self, _: ScreenRect, _: f32, _: f32) {}
        pub fn button(&mut self, _: ScreenRect, _: f32, _: f32, _: MouseButton, _: bool) {}
        pub fn wheel(&mut self, _: f32, _: f32) {}
        pub fn key(&mut self, _: &str, _: bool) {}
        pub fn release_all(&mut self) {}
    }

    pub struct CursorWatcher;

    impl CursorWatcher {
        pub fn new() -> Self {
            Self
        }
        pub fn poll(&mut self) -> Option<(&'static str, bool)> {
            None
        }
    }

    impl Default for CursorWatcher {
        fn default() -> Self {
            Self::new()
        }
    }

    pub fn enable_dpi_awareness() {}

    pub fn release_everything() {}
}

#[cfg(not(windows))]
pub use unsupported::{CursorWatcher, Injector, enable_dpi_awareness, release_everything};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_normalized_to_pixels() {
        let r = ScreenRect {
            left: 1920,
            top: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(r.to_pixel(0.0, 0.0), (1920, 0));
        assert_eq!(r.to_pixel(1.0, 1.0), (1920 + 1919, 1079));
        assert_eq!(r.to_pixel(0.5, 0.5), (1920 + 960, 540));
        assert_eq!(r.to_pixel(-1.0, 2.0), (1920, 1079));
    }
}
