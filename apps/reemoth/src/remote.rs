//! Host side of remote control: applies the viewer's input to this machine.

use reemoth_input::{Injector, ScreenRect};
use reemoth_proto::peer::{CtrlMsg, PointerMove};

use crate::stream::StreamControl;

pub struct RemoteInput {
    injector: Option<Injector>,
    /// Newest pointer move applied (moves arrive unordered).
    last_seq: Option<u32>,
    /// When the viewer was last heard from (it pings every second).
    last_heard: std::time::Instant,
    enabled: bool,
    unsupported_reported: bool,
    /// Events applied since the last `take_counts`: (pointer moves, other).
    moves: u32,
    events: u32,
}

impl RemoteInput {
    pub fn new(enabled: bool) -> Self {
        Self {
            injector: None,
            last_seq: None,
            last_heard: std::time::Instant::now(),
            enabled,
            unsupported_reported: false,
            moves: 0,
            events: 0,
        }
    }

    /// Pointer position from the unreliable `input` channel.
    pub fn pointer(&mut self, control: Option<&StreamControl>, data: &[u8]) {
        let (Ok(m), Some(rect)) = (serde_json::from_slice::<PointerMove>(data), screen(control))
        else {
            return;
        };
        // Drop a move that arrived after a newer one (wrapping comparison).
        if let Some(last) = self.last_seq
            && (m.seq.wrapping_sub(last) as i32) <= 0
        {
            return;
        }
        self.last_seq = Some(m.seq);
        if let Some(inj) = self.injector() {
            inj.move_to(rect, m.x, m.y);
            self.moves += 1;
        }
    }

    /// Applies an input message from the control channel. Returns false if
    /// `msg` is not an input message.
    pub fn handle(&mut self, control: Option<&StreamControl>, msg: &CtrlMsg) -> bool {
        let rect = screen(control);
        match msg {
            CtrlMsg::MouseButton { x, y, button, down } => {
                if let (Some(rect), Some(inj)) = (rect, self.injector()) {
                    inj.button(rect, *x, *y, *button, *down);
                }
            }
            CtrlMsg::Wheel { dx, dy } => {
                if let Some(inj) = self.injector() {
                    inj.wheel(*dx, *dy);
                }
            }
            CtrlMsg::Key { code, down } => {
                if let Some(inj) = self.injector() {
                    inj.key(code, *down);
                }
            }
            CtrlMsg::ReleaseInput => {
                if let Some(inj) = &mut self.injector {
                    inj.release_all();
                }
            }
            _ => return false,
        }
        self.events += 1;
        true
    }

    /// Any message from the viewer proves it is alive.
    pub fn heard_from_viewer(&mut self) {
        self.last_heard = std::time::Instant::now();
    }

    /// Releases held keys and buttons if the viewer went silent: if it
    /// crashed mid-keypress, the session takes ~15 s more to time out.
    pub fn watchdog(&mut self) {
        if self.last_heard.elapsed() > std::time::Duration::from_secs(3)
            && let Some(inj) = &mut self.injector
        {
            inj.release_all();
        }
    }

    /// Releases everything and refuses further input (session taken over).
    pub fn disable(&mut self) {
        if let Some(inj) = &mut self.injector {
            inj.release_all();
        }
        self.enabled = false;
    }

    pub fn take_counts(&mut self) -> (u32, u32) {
        (
            std::mem::take(&mut self.moves),
            std::mem::take(&mut self.events),
        )
    }

    fn injector(&mut self) -> Option<&mut Injector> {
        if !self.enabled {
            return None;
        }
        if self.injector.is_none() && !self.unsupported_reported {
            match Injector::new() {
                Ok(i) => self.injector = Some(i),
                Err(e) => {
                    eprintln!("remote control unavailable: {e:#}");
                    self.unsupported_reported = true;
                }
            }
        }
        self.injector.as_mut()
    }
}

/// The streamed screen's position on the desktop; input is meaningless
/// until a stream has told us what the viewer is looking at.
fn screen(control: Option<&StreamControl>) -> Option<ScreenRect> {
    let control = control?;
    if control.is_stopped() {
        return None;
    }
    *control.desktop.lock().expect("desktop lock")
}
