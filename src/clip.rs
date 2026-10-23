//! Clipboard text reader (Win32). eframe is built without its clipboard feature to keep the
//! dependency tree small, so Ctrl+V and the Paste button read the clipboard here.

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};

/// Standard clipboard format for UTF-16 text (CF_UNICODETEXT).
const CF_UNICODETEXT: u32 = 13;

/// Text on the clipboard, or `None` if it is empty, not text, or locked by another program.
pub fn read_text() -> Option<String> {
    unsafe {
        OpenClipboard(None).ok()?;
        let text = read_open();
        let _ = CloseClipboard();
        text
    }
}

unsafe fn read_open() -> Option<String> {
    let handle: HANDLE = GetClipboardData(CF_UNICODETEXT).ok()?;
    let global = HGLOBAL(handle.0);
    let ptr = GlobalLock(global) as *const u16;
    if ptr.is_null() {
        return None;
    }
    let mut len = 0;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
    let _ = GlobalUnlock(global);
    Some(text)
}

/// True once per Ctrl+V or Shift+Insert press. eframe is built without its clipboard feature, which
/// also swallows the key event, so the shortcut is read from the keyboard state. `was_down` keeps the
/// previous state between calls.
pub fn paste_pressed(was_down: &mut bool) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_INSERT, VK_SHIFT, VK_V};
    use windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY;
    let down = |vk: VIRTUAL_KEY| unsafe { GetAsyncKeyState(vk.0 as i32) } < 0;
    let now = (down(VK_CONTROL) && down(VK_V)) || (down(VK_SHIFT) && down(VK_INSERT));
    let pressed = now && !*was_down;
    *was_down = now;
    pressed
}

/// Pasted text for a link field: all whitespace and line breaks removed.
pub fn clean_link(s: &str) -> String {
    s.split_whitespace().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_link_removes_spaces_and_breaks() {
        assert_eq!(clean_link("  https://a.com/x\r\n "), "https://a.com/x");
        assert_eq!(clean_link("a b\tc"), "abc");
    }
}
