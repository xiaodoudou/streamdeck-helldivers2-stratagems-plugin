//! Scancode-based SendInput; DirectInput-style games ignore virtual-key events.

use std::thread::sleep;
use std::time::Duration;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MAPVK_VK_TO_VSC, MOUSE_EVENT_FLAGS, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyW, SendInput, VIRTUAL_KEY,
};

/// name -> (virtual key, extended)
fn vk(name: &str) -> Option<(u16, bool)> {
    let fixed = match name {
        "up" => (0x26, true),
        "down" => (0x28, true),
        "left" => (0x25, true),
        "right" => (0x27, true),
        "home" => (0x24, true),
        "end" => (0x23, true),
        "pageup" => (0x21, true),
        "pagedown" => (0x22, true),
        "insert" => (0x2D, true),
        "delete" => (0x2E, true),
        "lctrl" | "ctrl" => (0xA2, false),
        "rctrl" => (0xA3, true),
        "lshift" | "shift" => (0xA0, false),
        "rshift" => (0xA1, false),
        "lalt" | "alt" => (0xA4, false),
        "ralt" => (0xA5, true),
        "space" => (0x20, false),
        "tab" => (0x09, false),
        "enter" => (0x0D, false),
        "capslock" => (0x14, false),
        "backspace" => (0x08, false),
        "escape" => (0x1B, false),
        _ => {
            if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()).filter(|n| (1..=12).contains(n)) {
                return Some((0x6F + n, false));
            }
            let mut chars = name.chars();
            return match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_lowercase() || c.is_ascii_digit() => Some((c.to_ascii_uppercase() as u16, false)),
                _ => None,
            };
        }
    };
    Some(fixed)
}

/// name -> (down flag, up flag, mouseData)
fn mouse(name: &str) -> Option<(MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS, u32)> {
    Some(match name {
        "mouse1" => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
        "mouse2" => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
        "mouse3" => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
        "mouse4" => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1),
        "mouse5" => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2),
        _ => return None,
    })
}

pub fn valid(name: &str) -> bool {
    vk(name).is_some() || mouse(name).is_some()
}

fn send(input: INPUT) -> Result<(), String> {
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent == 1 { Ok(()) } else { Err(format!("SendInput failed: {}", windows::core::Error::from_thread())) }
}

fn key(name: &str, up: bool) -> Result<(), String> {
    if let Some((down_f, up_f, data)) = mouse(name) {
        let mi = MOUSEINPUT { mouseData: data, dwFlags: if up { up_f } else { down_f }, ..Default::default() };
        return send(INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi } });
    }
    let (code, ext) = vk(name).ok_or_else(|| format!("unknown key '{name}'"))?;
    let scan = unsafe { MapVirtualKeyW(code as u32, MAPVK_VK_TO_VSC) } as u16;
    let mut flags = KEYEVENTF_SCANCODE;
    if ext {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    let ki = KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: KEYBD_EVENT_FLAGS(flags.0), time: 0, dwExtraInfo: 0 };
    send(INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } })
}

pub fn down(name: &str) -> Result<(), String> {
    key(name, false)
}

pub fn up(name: &str) -> Result<(), String> {
    key(name, true)
}

pub fn tap(name: &str, hold_ms: u64) -> Result<(), String> {
    down(name)?;
    sleep(Duration::from_millis(hold_ms));
    up(name)
}

#[cfg(test)]
mod tests {
    //! Key-name parsing only: nothing here sends input.

    use super::*;

    #[test]
    fn arrows_are_extended_keys() {
        assert_eq!(vk("up"), Some((0x26, true)));
        assert_eq!(vk("down"), Some((0x28, true)));
        assert_eq!(vk("left"), Some((0x25, true)));
        assert_eq!(vk("right"), Some((0x27, true)));
    }

    #[test]
    fn navigation_cluster_are_extended_keys() {
        assert_eq!(vk("home"), Some((0x24, true)));
        assert_eq!(vk("end"), Some((0x23, true)));
        assert_eq!(vk("pageup"), Some((0x21, true)));
        assert_eq!(vk("pagedown"), Some((0x22, true)));
        assert_eq!(vk("insert"), Some((0x2D, true)));
        assert_eq!(vk("delete"), Some((0x2E, true)));
    }

    #[test]
    fn modifiers_and_named_keys() {
        assert_eq!(vk("ctrl"), vk("lctrl"));
        assert_eq!(vk("lctrl"), Some((0xA2, false)));
        assert_eq!(vk("rctrl"), Some((0xA3, true)));
        assert_eq!(vk("shift"), vk("lshift"));
        assert_eq!(vk("lshift"), Some((0xA0, false)));
        assert_eq!(vk("rshift"), Some((0xA1, false)));
        assert_eq!(vk("alt"), vk("lalt"));
        assert_eq!(vk("lalt"), Some((0xA4, false)));
        assert_eq!(vk("ralt"), Some((0xA5, true)));
        for (name, code) in [("space", 0x20), ("tab", 0x09), ("enter", 0x0D), ("capslock", 0x14), ("backspace", 0x08), ("escape", 0x1B)] {
            assert_eq!(vk(name), Some((code, false)), "{name}");
        }
    }

    #[test]
    fn function_keys_f1_to_f12() {
        assert_eq!(vk("f1"), Some((0x70, false)));
        assert_eq!(vk("f12"), Some((0x7B, false)));
        for n in 1..=12u16 {
            assert_eq!(vk(&format!("f{n}")), Some((0x6F + n, false)));
        }
        for bad in ["f0", "f13", "f99", "f-1", "fx"] {
            assert_eq!(vk(bad), None, "{bad}");
        }
        assert_eq!(vk("f"), Some((b'F' as u16, false)), "a bare f is the letter key");
    }

    #[test]
    fn single_lowercase_letters_and_digits() {
        for c in ('a'..='z').chain('0'..='9') {
            assert_eq!(vk(&c.to_string()), Some((c.to_ascii_uppercase() as u16, false)), "{c}");
        }
    }

    #[test]
    fn mouse_buttons() {
        assert_eq!(mouse("mouse1"), Some((MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0)));
        assert_eq!(mouse("mouse2"), Some((MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0)));
        assert_eq!(mouse("mouse3"), Some((MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0)));
        assert_eq!(mouse("mouse4"), Some((MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1)));
        assert_eq!(mouse("mouse5"), Some((MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2)));
        for bad in ["mouse0", "mouse6", "mouse", "Mouse1", "w"] {
            assert_eq!(mouse(bad), None, "{bad}");
        }
        assert_eq!(vk("mouse1"), None, "mouse buttons are not keyboard keys");
    }

    #[test]
    fn valid_accepts_keys_and_buttons_only() {
        for good in ["w", "a", "s", "d", "up", "home", "delete", "lctrl", "f5", "mouse4", "9", "space"] {
            assert!(valid(good), "{good}");
        }
        for bad in [
            "", "W", "A", "Up", "Home", "LCTRL", "F1", "ctrl+w", "space ", " w", "ww", "é", "-", ";", "pgup", "del", "numpad1", "mouse6",
            "esc",
        ] {
            assert!(!valid(bad), "{bad:?}");
        }
    }
}
