//! Text clipboard sync. One thread owns the OS clipboard (on X11/Wayland the
//! owner must stay alive to serve pastes) and polls it for changes.

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use reemoth_proto::peer::{CtrlMsg, MAX_CLIPBOARD_BYTES};
use tokio::sync::mpsc;

const POLL: Duration = Duration::from_millis(300);

/// Within the data-channel message limit once JSON-escaped.
fn fits(text: &str) -> bool {
    text.len() <= MAX_CLIPBOARD_BYTES
        && CtrlMsg::Clipboard {
            text: text.to_owned(),
        }
        .encode()
        .len()
            <= reemoth_net::session::MAX_MESSAGE_LEN - 256
}

#[cfg(windows)]
mod sensitive {
    use windows::Win32::System::DataExchange::{
        IsClipboardFormatAvailable, RegisterClipboardFormatW,
    };
    use windows::core::w;

    /// Password managers mark their copies with this format so clipboard
    /// monitors (history, cloud sync, remote tools) skip them.
    pub fn clipboard_is_private() -> bool {
        let format =
            unsafe { RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")) };
        format != 0 && unsafe { IsClipboardFormatAvailable(format) }.is_ok()
    }
}

#[cfg(not(windows))]
mod sensitive {
    /// X11/Wayland have no common marker for private clipboard content.
    pub fn clipboard_is_private() -> bool {
        false
    }
}

pub struct ClipboardSync {
    /// Text to put on this machine's clipboard (from the peer).
    pub set: std_mpsc::Sender<String>,
    /// Text copied on this machine (to send to the peer).
    pub changed: mpsc::UnboundedReceiver<String>,
}

/// Starts the clipboard thread. Fails if this session has no clipboard
/// (e.g. no display server).
pub fn spawn() -> anyhow::Result<ClipboardSync> {
    let (set_tx, set_rx) = std_mpsc::channel::<String>();
    let (changed_tx, changed_rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = std_mpsc::channel();
    std::thread::Builder::new()
        .name("reemoth-clipboard".into())
        .spawn(move || {
            let mut clipboard = match arboard::Clipboard::new() {
                Ok(c) => {
                    let _ = ready_tx.send(Ok(()));
                    c
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            // Whatever is on the clipboard at start stays local.
            let mut last = clipboard.get_text().ok();
            loop {
                match set_rx.recv_timeout(POLL) {
                    Ok(text) => {
                        if last.as_deref() != Some(&text) {
                            match clipboard.set_text(text.clone()) {
                                // Don't echo the peer's text back to it.
                                Ok(()) => last = Some(text),
                                Err(e) => {
                                    tracing::debug!("clipboard write failed: {e}");
                                    // Keep what is there marked as seen, so it
                                    // is not sent to the peer by mistake.
                                    last = clipboard.get_text().ok();
                                }
                            }
                        }
                    }
                    Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
                }
                if sensitive::clipboard_is_private() {
                    continue;
                }
                let Ok(now) = clipboard.get_text() else {
                    continue;
                };
                if last.as_deref() != Some(&now) {
                    last = Some(now.clone());
                    if fits(&now) && changed_tx.send(now).is_err() {
                        return;
                    }
                }
            }
        })?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(ClipboardSync {
            set: set_tx,
            changed: changed_rx,
        }),
        Ok(Err(e)) => anyhow::bail!("no clipboard access: {e}"),
        Err(_) => anyhow::bail!("clipboard thread failed to start"),
    }
}
