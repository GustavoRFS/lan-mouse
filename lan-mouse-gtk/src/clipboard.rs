use std::{cell::RefCell, rc::Rc};

use gtk::{
    gdk,
    glib::{self, types::StaticType},
    prelude::*,
};
use lan_mouse_proto::{ClipboardUpdateId, MAX_CLIPBOARD_TEXT_BYTES};

use crate::window::Window;

#[derive(Default)]
struct ClipboardState {
    enabled: bool,
    generation: u64,
    pending_remote_text: Option<(ClipboardUpdateId, String)>,
}

/// Watches the desktop text clipboard and applies remote text without echoing it.
pub(crate) struct ClipboardSync {
    clipboard: gdk::Clipboard,
    state: Rc<RefCell<ClipboardState>>,
}

impl ClipboardSync {
    pub(crate) fn new(window: &Window) -> Rc<Self> {
        let clipboard = gdk::Display::default()
            .expect("Could not connect to a display")
            .clipboard();
        let sync = Rc::new(Self {
            clipboard: clipboard.clone(),
            state: Rc::new(RefCell::new(ClipboardState {
                enabled: true,
                ..Default::default()
            })),
        });

        let sync_weak = Rc::downgrade(&sync);
        let window_weak = window.downgrade();
        clipboard.connect_changed(move |clipboard| {
            let Some(sync) = sync_weak.upgrade() else {
                return;
            };
            sync.read_changed_text(clipboard, &window_weak);
        });

        sync
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        let mut state = self.state.borrow_mut();
        state.enabled = enabled;
        state.generation = state.generation.wrapping_add(1);
        if !enabled {
            state.pending_remote_text = None;
        }
    }

    pub(crate) fn apply_remote_text(&self, update_id: ClipboardUpdateId, text: String) {
        if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
            log::debug!("ignoring oversized remote clipboard text");
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            if !state.enabled {
                return;
            }
            state.generation = state.generation.wrapping_add(1);
            state.pending_remote_text = Some((update_id, text.clone()));
        }
        self.clipboard.set_text(&text);
    }

    fn read_changed_text(&self, clipboard: &gdk::Clipboard, window: &glib::WeakRef<Window>) {
        let (enabled, generation) = {
            let mut state = self.state.borrow_mut();
            state.generation = state.generation.wrapping_add(1);
            (state.enabled, state.generation)
        };
        if !enabled || !is_plain_text_clipboard(clipboard) {
            self.state.borrow_mut().pending_remote_text = None;
            return;
        }

        let state = self.state.clone();
        let window = window.clone();
        clipboard.read_text_async(None::<&gtk::gio::Cancellable>, move |result| {
            let Ok(Some(text)) = result else {
                let mut current = state.borrow_mut();
                if current.enabled && current.generation == generation {
                    current.pending_remote_text = None;
                }
                return;
            };
            if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                let mut current = state.borrow_mut();
                if current.enabled && current.generation == generation {
                    current.pending_remote_text = None;
                }
                return;
            }

            let should_send = {
                let mut state = state.borrow_mut();
                if !state.enabled || state.generation != generation {
                    return;
                }
                if state
                    .pending_remote_text
                    .as_ref()
                    .is_some_and(|(_, remote_text)| remote_text == text.as_str())
                {
                    state.pending_remote_text = None;
                    false
                } else {
                    state.pending_remote_text = None;
                    true
                }
            };

            if should_send {
                if let Some(window) = window.upgrade() {
                    window.send_clipboard_text(text.to_string());
                }
            }
        });
    }
}

fn is_plain_text_clipboard(clipboard: &gdk::Clipboard) -> bool {
    let formats = clipboard.formats();
    if formats.contain_mime_type("text/uri-list") {
        return false;
    }
    let mime_types = formats.mime_types();
    if mime_types
        .iter()
        .any(|mime| mime.as_str().starts_with("image/"))
    {
        return false;
    }
    mime_types
        .iter()
        .any(|mime| mime.as_str().starts_with("text/plain"))
        || formats.contains_type(String::static_type())
}
