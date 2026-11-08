use lan_mouse_proto::KeyboardLayout;

pub(crate) fn effective_layout(configured: Option<&str>) -> Option<KeyboardLayout> {
    match configured {
        Some("es") => Some(KeyboardLayout::Spanish),
        Some("br") => Some(KeyboardLayout::PortugueseBrazil),
        Some("latam") => Some(KeyboardLayout::SpanishLatinAmerica),
        Some(unknown) => {
            log::warn!("unsupported configured keyboard layout `{unknown}`; using auto detection");
            active_layout()
        }
        None => active_layout(),
    }
}

#[cfg(windows)]
fn active_layout() -> Option<KeyboardLayout> {
    use windows::Win32::UI::{
        Input::KeyboardAndMouse::GetKeyboardLayout,
        WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
    };

    // The capture process runs in the background. Query the foreground
    // application's thread so this follows the input language the user is
    // currently typing with, rather than the daemon's own thread setting.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return None;
    }
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, None) };
    if thread_id == 0 {
        return None;
    }

    let keyboard_layout = unsafe { GetKeyboardLayout(thread_id) };
    let language_id = keyboard_layout.0 as usize as u16;
    windows_language_to_layout(language_id)
}

#[cfg(not(windows))]
fn active_layout() -> Option<KeyboardLayout> {
    None
}

#[cfg(any(windows, test))]
fn windows_language_to_layout(language_id: u16) -> Option<KeyboardLayout> {
    match language_id {
        0x040a | 0x0c0a => Some(KeyboardLayout::Spanish),
        id if id & 0x03ff == 0x000a => Some(KeyboardLayout::SpanishLatinAmerica),
        0x0416 => Some(KeyboardLayout::PortugueseBrazil),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_layout_names_map_to_xkb_layouts() {
        assert_eq!(effective_layout(Some("es")), Some(KeyboardLayout::Spanish));
        assert_eq!(
            effective_layout(Some("br")),
            Some(KeyboardLayout::PortugueseBrazil)
        );
        assert_eq!(
            effective_layout(Some("latam")),
            Some(KeyboardLayout::SpanishLatinAmerica)
        );
    }

    #[test]
    fn windows_active_language_ids_map_to_xkb_layouts() {
        assert_eq!(
            windows_language_to_layout(0x040a),
            Some(KeyboardLayout::Spanish)
        );
        assert_eq!(
            windows_language_to_layout(0x0c0a),
            Some(KeyboardLayout::Spanish)
        );
        assert_eq!(
            windows_language_to_layout(0x080a),
            Some(KeyboardLayout::SpanishLatinAmerica)
        );
        assert_eq!(
            windows_language_to_layout(0x2c0a),
            Some(KeyboardLayout::SpanishLatinAmerica)
        );
        assert_eq!(
            windows_language_to_layout(0x580a),
            Some(KeyboardLayout::SpanishLatinAmerica)
        );
        assert_eq!(
            windows_language_to_layout(0x0416),
            Some(KeyboardLayout::PortugueseBrazil)
        );
        assert_eq!(windows_language_to_layout(0x0409), None);
    }
}
