//! W3C UI Events `code` → PC/AT set-1 scancode, as Windows' SendInput wants
//! with KEYEVENTF_SCANCODE. Scancodes name physical key positions, so the
//! host's keyboard layout maps them to characters, the same as a local
//! keyboard would.

/// `(scancode, extended)` for a `code`, or `None` if it has no scancode
/// (Pause is sent as a virtual key instead; see the injector).
pub fn windows_scancode(code: &str) -> Option<(u16, bool)> {
    let plain = |s| Some((s, false));
    let ext = |s| Some((s, true));
    match code {
        "Escape" => plain(0x01),
        "Digit1" => plain(0x02),
        "Digit2" => plain(0x03),
        "Digit3" => plain(0x04),
        "Digit4" => plain(0x05),
        "Digit5" => plain(0x06),
        "Digit6" => plain(0x07),
        "Digit7" => plain(0x08),
        "Digit8" => plain(0x09),
        "Digit9" => plain(0x0A),
        "Digit0" => plain(0x0B),
        "Minus" => plain(0x0C),
        "Equal" => plain(0x0D),
        "Backspace" => plain(0x0E),
        "Tab" => plain(0x0F),
        "KeyQ" => plain(0x10),
        "KeyW" => plain(0x11),
        "KeyE" => plain(0x12),
        "KeyR" => plain(0x13),
        "KeyT" => plain(0x14),
        "KeyY" => plain(0x15),
        "KeyU" => plain(0x16),
        "KeyI" => plain(0x17),
        "KeyO" => plain(0x18),
        "KeyP" => plain(0x19),
        "BracketLeft" => plain(0x1A),
        "BracketRight" => plain(0x1B),
        "Enter" => plain(0x1C),
        "ControlLeft" => plain(0x1D),
        "KeyA" => plain(0x1E),
        "KeyS" => plain(0x1F),
        "KeyD" => plain(0x20),
        "KeyF" => plain(0x21),
        "KeyG" => plain(0x22),
        "KeyH" => plain(0x23),
        "KeyJ" => plain(0x24),
        "KeyK" => plain(0x25),
        "KeyL" => plain(0x26),
        "Semicolon" => plain(0x27),
        "Quote" => plain(0x28),
        "Backquote" => plain(0x29),
        "ShiftLeft" => plain(0x2A),
        "Backslash" => plain(0x2B),
        "KeyZ" => plain(0x2C),
        "KeyX" => plain(0x2D),
        "KeyC" => plain(0x2E),
        "KeyV" => plain(0x2F),
        "KeyB" => plain(0x30),
        "KeyN" => plain(0x31),
        "KeyM" => plain(0x32),
        "Comma" => plain(0x33),
        "Period" => plain(0x34),
        "Slash" => plain(0x35),
        "ShiftRight" => plain(0x36),
        "NumpadMultiply" => plain(0x37),
        "AltLeft" => plain(0x38),
        "Space" => plain(0x39),
        "CapsLock" => plain(0x3A),
        "F1" => plain(0x3B),
        "F2" => plain(0x3C),
        "F3" => plain(0x3D),
        "F4" => plain(0x3E),
        "F5" => plain(0x3F),
        "F6" => plain(0x40),
        "F7" => plain(0x41),
        "F8" => plain(0x42),
        "F9" => plain(0x43),
        "F10" => plain(0x44),
        // Windows reports NumLock as E0 45; a plain 45 is Pause.
        "NumLock" => ext(0x45),
        "ScrollLock" => plain(0x46),
        "Numpad7" => plain(0x47),
        "Numpad8" => plain(0x48),
        "Numpad9" => plain(0x49),
        "NumpadSubtract" => plain(0x4A),
        "Numpad4" => plain(0x4B),
        "Numpad5" => plain(0x4C),
        "Numpad6" => plain(0x4D),
        "NumpadAdd" => plain(0x4E),
        "Numpad1" => plain(0x4F),
        "Numpad2" => plain(0x50),
        "Numpad3" => plain(0x51),
        "Numpad0" => plain(0x52),
        "NumpadDecimal" => plain(0x53),
        "IntlBackslash" => plain(0x56),
        "F11" => plain(0x57),
        "F12" => plain(0x58),
        "NumpadEqual" => plain(0x59),
        "F13" => plain(0x64),
        "F14" => plain(0x65),
        "F15" => plain(0x66),
        "F16" => plain(0x67),
        "F17" => plain(0x68),
        "F18" => plain(0x69),
        "F19" => plain(0x6A),
        "F20" => plain(0x6B),
        "F21" => plain(0x6C),
        "F22" => plain(0x6D),
        "F23" => plain(0x6E),
        "KanaMode" => plain(0x70),
        "Lang2" => plain(0x71),
        "Lang1" => plain(0x72),
        "IntlRo" => plain(0x73),
        "F24" => plain(0x76),
        "Convert" => plain(0x79),
        "NonConvert" => plain(0x7B),
        "IntlYen" => plain(0x7D),
        "NumpadComma" => plain(0x7E),
        "MediaTrackPrevious" => ext(0x10),
        "MediaTrackNext" => ext(0x19),
        "NumpadEnter" => ext(0x1C),
        "ControlRight" => ext(0x1D),
        "AudioVolumeMute" => ext(0x20),
        "LaunchApp2" => ext(0x21),
        "MediaPlayPause" => ext(0x22),
        "MediaStop" => ext(0x24),
        "AudioVolumeDown" => ext(0x2E),
        "AudioVolumeUp" => ext(0x30),
        "BrowserHome" => ext(0x32),
        "NumpadDivide" => ext(0x35),
        "PrintScreen" => ext(0x37),
        "AltRight" => ext(0x38),
        "Home" => ext(0x47),
        "ArrowUp" => ext(0x48),
        "PageUp" => ext(0x49),
        "ArrowLeft" => ext(0x4B),
        "ArrowRight" => ext(0x4D),
        "End" => ext(0x4F),
        "ArrowDown" => ext(0x50),
        "PageDown" => ext(0x51),
        "Insert" => ext(0x52),
        "Delete" => ext(0x53),
        "MetaLeft" => ext(0x5B),
        "MetaRight" => ext(0x5C),
        "ContextMenu" => ext(0x5D),
        "BrowserSearch" => ext(0x65),
        "BrowserFavorites" => ext(0x66),
        "BrowserRefresh" => ext(0x67),
        "BrowserStop" => ext(0x68),
        "BrowserForward" => ext(0x69),
        "BrowserBack" => ext(0x6A),
        "LaunchApp1" => ext(0x6B),
        "LaunchMail" => ext(0x6C),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_keys() {
        assert_eq!(windows_scancode("KeyA"), Some((0x1E, false)));
        assert_eq!(windows_scancode("ArrowUp"), Some((0x48, true)));
        assert_eq!(windows_scancode("ControlRight"), Some((0x1D, true)));
        assert_eq!(windows_scancode("Pause"), None);
        assert_eq!(windows_scancode("Nonsense"), None);
    }

    #[test]
    fn letters_and_digits_all_mapped_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in 'A'..='Z' {
            let s = windows_scancode(&format!("Key{c}")).unwrap();
            assert!(seen.insert(s), "duplicate scancode for Key{c}");
        }
        for d in 0..=9 {
            let s = windows_scancode(&format!("Digit{d}")).unwrap();
            assert!(seen.insert(s), "duplicate scancode for Digit{d}");
        }
    }
}
