//! Windows: SendInput for injection, GetCursorInfo for the pointer shape.
//!
//! Limits worth knowing: SendInput cannot reach windows of elevated (admin)
//! programs or the UAC/lock screen unless the host itself runs elevated or
//! as a service (M8).

use std::collections::HashSet;
use std::sync::Mutex;

use reemoth_proto::peer::MouseButton;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_PAUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetSystemMetrics, HCURSOR, IDC_APPSTARTING,
    IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW,
    IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT, LoadCursorW, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

use crate::{ScreenRect, windows_scancode};

/// Makes this process see physical pixels everywhere, so capture
/// coordinates (DXGI) and injection coordinates (SendInput) agree on
/// scaled displays. Must run before any window or DPI-dependent call.
pub fn enable_dpi_awareness() {
    if let Err(e) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        // Already set (e.g. by a manifest) is fine.
        tracing::debug!("SetProcessDpiAwarenessContext: {e}");
    }
}

/// Every key and button any injector currently holds down, so
/// [`release_everything`] can undo them when the process is stopped.
static HELD: Mutex<(Vec<String>, Vec<MouseButton>)> = Mutex::new((Vec::new(), Vec::new()));

fn record(key: Option<&str>, button: Option<MouseButton>, down: bool) {
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(k) = key {
        held.0.retain(|h| h != k);
        if down {
            held.0.push(k.to_owned());
        }
    }
    if let Some(b) = button {
        held.1.retain(|h| *h != b);
        if down {
            held.1.push(b);
        }
    }
}

/// Releases everything injected by any session (call before exiting).
pub fn release_everything() {
    let (keys, buttons) = std::mem::take(&mut *HELD.lock().unwrap_or_else(|e| e.into_inner()));
    for code in keys {
        if let Some(input) = key_input(&code, false) {
            send(&[input]);
        }
    }
    for button in buttons {
        let (flags, data) = button_flags(button, false);
        send(&[mouse((0, 0), data, flags)]);
    }
}

/// Replays viewer input. Remembers what it pressed so it can release
/// everything when the viewer loses focus or the session ends; a key left
/// down on the host would otherwise stay stuck.
#[derive(Default)]
pub struct Injector {
    keys: HashSet<String>,
    buttons: HashSet<MouseButton>,
    /// Sub-notch scroll carried to the next event (touchpads send small deltas).
    wheel: (f32, f32),
}

impl Injector {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self::default())
    }

    pub fn move_to(&mut self, rect: ScreenRect, x: f32, y: f32) {
        let (px, py) = rect.to_pixel(x, y);
        send(&[mouse(
            absolute(px, py),
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )]);
    }

    pub fn button(&mut self, rect: ScreenRect, x: f32, y: f32, button: MouseButton, down: bool) {
        let (px, py) = rect.to_pixel(x, y);
        let (flags, data) = button_flags(button, down);
        // Move and click in one call so nothing can slip in between.
        send(&[
            mouse(
                absolute(px, py),
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            ),
            mouse((0, 0), data, flags),
        ]);
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        record(None, Some(button), down);
    }

    /// `dx`/`dy` in wheel units (120 = one notch); positive `dy` scrolls up.
    pub fn wheel(&mut self, dx: f32, dy: f32) {
        self.wheel.0 += dx;
        self.wheel.1 += dy;
        let (wx, wy) = (self.wheel.0.trunc(), self.wheel.1.trunc());
        self.wheel.0 -= wx;
        self.wheel.1 -= wy;
        let mut events = Vec::new();
        if wy != 0.0 {
            events.push(mouse((0, 0), wy as i32 as u32, MOUSEEVENTF_WHEEL));
        }
        if wx != 0.0 {
            events.push(mouse((0, 0), wx as i32 as u32, MOUSEEVENTF_HWHEEL));
        }
        send(&events);
    }

    pub fn key(&mut self, code: &str, down: bool) {
        let Some(input) = key_input(code, down) else {
            tracing::debug!("no mapping for key {code}");
            return;
        };
        send(&[input]);
        if down {
            self.keys.insert(code.to_owned());
        } else {
            self.keys.remove(code);
        }
        record(Some(code), None, down);
    }

    pub fn release_all(&mut self) {
        for code in std::mem::take(&mut self.keys) {
            self.key(&code, false);
            self.keys.remove(&code);
        }
        for button in std::mem::take(&mut self.buttons) {
            let (flags, data) = button_flags(button, false);
            send(&[mouse((0, 0), data, flags)]);
            record(None, Some(button), false);
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

fn key_input(code: &str, down: bool) -> Option<INPUT> {
    let up = if down {
        KEYBD_EVENT_FLAGS(0)
    } else {
        KEYEVENTF_KEYUP
    };
    if let Some((scan, extended)) = windows_scancode(code) {
        let mut flags = KEYEVENTF_SCANCODE | up;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        Some(keyboard(VIRTUAL_KEY(0), scan, flags))
    } else if code == "Pause" {
        Some(keyboard(VK_PAUSE, 0, up))
    } else {
        None
    }
}

fn button_flags(button: MouseButton, down: bool) -> (MOUSE_EVENT_FLAGS, u32) {
    match (button, down) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON1)),
        (MouseButton::Back, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
        (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON2)),
        (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON2)),
    }
}

/// Desktop pixel → SendInput's 0..65535 range over the whole virtual desktop.
fn absolute(x: i32, y: i32) -> (i32, i32) {
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        )
    };
    let nx = (i64::from(x - vx) * 65535 / i64::from(vw - 1)) as i32;
    let ny = (i64::from(y - vy) * 65535 / i64::from(vh - 1)) as i32;
    (nx, ny)
}

fn mouse((dx, dy): (i32, i32), data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn keyboard(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        // Typically UIPI: the foreground window belongs to an elevated program.
        tracing::debug!("SendInput injected {sent} of {} events", inputs.len());
    }
}

/// Reports the system pointer's shape when it changes, as a CSS cursor name.
pub struct CursorWatcher {
    known: Vec<(HCURSOR, &'static str)>,
    last: Option<(isize, bool)>,
}

// SAFETY: cursor handles are process-wide values, not thread-affine objects.
unsafe impl Send for CursorWatcher {}

impl Default for CursorWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorWatcher {
    pub fn new() -> Self {
        // System cursors are shared handles, so comparing handles identifies them.
        let table = [
            (IDC_ARROW, "default"),
            (IDC_IBEAM, "text"),
            (IDC_HAND, "pointer"),
            (IDC_WAIT, "wait"),
            (IDC_APPSTARTING, "progress"),
            (IDC_CROSS, "crosshair"),
            (IDC_SIZEALL, "move"),
            (IDC_SIZEWE, "ew-resize"),
            (IDC_SIZENS, "ns-resize"),
            (IDC_SIZENWSE, "nwse-resize"),
            (IDC_SIZENESW, "nesw-resize"),
            (IDC_NO, "not-allowed"),
            (IDC_HELP, "help"),
        ];
        let known = table
            .into_iter()
            .filter_map(|(id, name)| unsafe { LoadCursorW(None, id) }.ok().map(|h| (h, name)))
            .collect();
        Self { known, last: None }
    }

    /// `Some((shape, visible))` when the pointer changed since the last call.
    pub fn poll(&mut self) -> Option<(&'static str, bool)> {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetCursorInfo(&mut info) }.ok()?;
        let visible = info.flags.0 & CURSOR_SHOWING.0 != 0 && !info.hCursor.is_invalid();
        let key = (info.hCursor.0 as isize, visible);
        if self.last == Some(key) {
            return None;
        }
        self.last = Some(key);
        // App-specific cursors have no standard name; an arrow is the closest.
        let shape = self
            .known
            .iter()
            .find(|(h, _)| *h == info.hCursor)
            .map_or("default", |(_, name)| name);
        Some((shape, visible))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SM_CXSCREEN, SM_CYSCREEN};

    /// Moves the real pointer: run by hand (`cargo test -p reemoth-input -- --ignored`).
    #[test]
    #[ignore]
    fn pointer_lands_where_requested() {
        enable_dpi_awareness();
        let rect = ScreenRect {
            left: 0,
            top: 0,
            width: unsafe { GetSystemMetrics(SM_CXSCREEN) },
            height: unsafe { GetSystemMetrics(SM_CYSCREEN) },
        };
        let mut inj = Injector::new().unwrap();
        for (x, y) in [(0.25f32, 0.25f32), (0.5, 0.5), (0.9, 0.1)] {
            inj.move_to(rect, x, y);
            std::thread::sleep(std::time::Duration::from_millis(50));
            let mut p = POINT::default();
            unsafe { GetCursorPos(&mut p) }.unwrap();
            let (ex, ey) = rect.to_pixel(x, y);
            assert!(
                (p.x - ex).abs() <= 1 && (p.y - ey).abs() <= 1,
                "asked for ({ex},{ey}), pointer is at ({},{})",
                p.x,
                p.y
            );
        }
    }
}
