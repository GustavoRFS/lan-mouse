use std::{
    cell::RefCell,
    ffi::OsStr,
    rc::Rc,
    time::{Duration, Instant},
};

use gtk::{
    gdk,
    gio,
    glib::{self, types::StaticType},
    prelude::*,
};
use lan_mouse_proto::{ClipboardUpdateId, MAX_CLIPBOARD_TEXT_BYTES};

use crate::window::Window;

const FORMAT_RETRY_DELAY: Duration = Duration::from_millis(50);
const MAX_FORMAT_RETRIES: u8 = 5;
const DUPLICATE_TEXT_WINDOW: Duration = Duration::from_secs(1);

#[derive(Default)]
struct ClipboardState {
    enabled: bool,
    generation: u64,
    pending_remote_text: Option<(ClipboardUpdateId, String)>,
    last_local_text: Option<(String, Instant)>,
}

/// Watches the desktop text clipboard and applies remote text without echoing it.
pub(crate) struct ClipboardSync {
    clipboard: gdk::Clipboard,
    state: Rc<RefCell<ClipboardState>>,
    wayland_watcher: RefCell<Option<gio::Subprocess>>,
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
            wayland_watcher: RefCell::new(None),
        });

        let sync_weak = Rc::downgrade(&sync);
        let window_weak = window.downgrade();
        clipboard.connect_changed(move |clipboard| {
            let Some(sync) = sync_weak.upgrade() else {
                return;
            };
            sync.handle_change(clipboard, &window_weak);
        });

        if glib::getenv("WAYLAND_DISPLAY").is_some() {
            sync.start_wayland_watcher(window.downgrade(), 0);
        }

        sync
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        let mut state = self.state.borrow_mut();
        state.enabled = enabled;
        state.generation = state.generation.wrapping_add(1);
        if !enabled {
            state.pending_remote_text = None;
            state.last_local_text = None;
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
            state.last_local_text = None;
        }
        self.clipboard.set_text(&text);
    }

    fn handle_change(self: &Rc<Self>, clipboard: &gdk::Clipboard, window: &glib::WeakRef<Window>) {
        let generation = {
            let mut state = self.state.borrow_mut();
            state.generation = state.generation.wrapping_add(1);
            state.generation
        };
        self.read_changed_text(clipboard, window, generation, 0);
    }

    fn handle_wayland_change(self: &Rc<Self>, window: &glib::WeakRef<Window>) {
        let generation = {
            let mut state = self.state.borrow_mut();
            state.generation = state.generation.wrapping_add(1);
            state.generation
        };
        if self.state.borrow().enabled {
            read_wayland_text(self.state.clone(), window.clone(), generation);
        }
    }

    fn start_wayland_watcher(self: &Rc<Self>, window: glib::WeakRef<Window>, failures: u8) {
        let argv = [
            OsStr::new("wl-paste"),
            OsStr::new("--watch"),
            OsStr::new("/bin/sh"),
            OsStr::new("-c"),
            OsStr::new("printf x"),
        ];
        let process = match gio::Subprocess::newv(
            &argv,
            gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
        ) {
            Ok(process) => process,
            Err(error) => {
                log::warn!("Wayland clipboard watcher unavailable: {error}");
                return;
            }
        };
        let Some(stdout) = process.stdout_pipe() else {
            process.force_exit();
            return;
        };
        self.wayland_watcher.replace(Some(process));
        let sync_weak = Rc::downgrade(self);
        let started = Instant::now();
        glib::spawn_future_local(async move {
            loop {
                let Ok(bytes) = stdout.read_bytes_future(64, glib::Priority::default()).await else {
                    break;
                };
                if bytes.is_empty() {
                    break;
                }
                let Some(sync) = sync_weak.upgrade() else {
                    break;
                };
                for _ in bytes.as_ref() {
                    sync.handle_wayland_change(&window);
                }
            }
            if let Some(sync) = sync_weak.upgrade() {
                sync.wayland_watcher.replace(None);
                let failures = if started.elapsed() < Duration::from_secs(5) {
                    failures.saturating_add(1)
                } else {
                    0
                };
                if failures >= 3 {
                    log::warn!("Wayland clipboard watcher stopped repeatedly");
                } else {
                    log::warn!("Wayland clipboard watcher stopped; retrying");
                    glib::timeout_add_local_once(Duration::from_secs(1), move || {
                        sync.start_wayland_watcher(window, failures);
                    });
                }
            }
        });
    }

    fn read_changed_text(
        self: &Rc<Self>,
        clipboard: &gdk::Clipboard,
        window: &glib::WeakRef<Window>,
        generation: u64,
        retries: u8,
    ) {
        {
            let state = self.state.borrow();
            if !state.enabled || state.generation != generation {
                return;
            }
        }

        // A hidden GTK window may not receive Wayland clipboard MIME offers.
        // The data-control watcher can still read the selection in that case.
        let formats = clipboard.formats();
        let formats_missing =
            formats.mime_types().is_empty() && !formats.contains_type(String::static_type());
        if formats_missing && glib::getenv("WAYLAND_DISPLAY").is_some() {
            read_wayland_text(self.state.clone(), window.clone(), generation);
            return;
        }
        if formats_missing {
            if retries < MAX_FORMAT_RETRIES {
                let sync = self.clone();
                let clipboard = clipboard.clone();
                let window = window.clone();
                glib::timeout_add_local_once(FORMAT_RETRY_DELAY, move || {
                    sync.read_changed_text(&clipboard, &window, generation, retries + 1);
                });
            } else {
                log::debug!("clipboard formats still unavailable after retry");
            }
            if retries < MAX_FORMAT_RETRIES {
                return;
            }
        }
        if !formats_missing && !is_plain_text_clipboard(clipboard) {
            self.state.borrow_mut().pending_remote_text = None;
            return;
        }

        let state = self.state.clone();
        let window = window.clone();
        clipboard.read_text_async(None::<&gtk::gio::Cancellable>, move |result| {
            let Ok(Some(text)) = result else {
                clear_pending_text(&state, generation);
                return;
            };
            deliver_local_text(&state, &window, generation, text.as_str());
        });
    }
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        if let Some(process) = self.wayland_watcher.get_mut().take() {
            process.force_exit();
        }
    }
}

fn clear_pending_text(state: &Rc<RefCell<ClipboardState>>, generation: u64) {
    let mut current = state.borrow_mut();
    if current.enabled && current.generation == generation {
        current.pending_remote_text = None;
    }
}

fn deliver_local_text(
    state: &Rc<RefCell<ClipboardState>>,
    window: &glib::WeakRef<Window>,
    generation: u64,
    text: &str,
) {
    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        clear_pending_text(state, generation);
        return;
    }
    let should_send = {
        let mut state = state.borrow_mut();
        if !state.enabled || state.generation != generation {
            return;
        }
        let remote_echo = state
            .pending_remote_text
            .as_ref()
            .is_some_and(|(_, remote_text)| remote_text == text);
        let duplicate_local = state
            .last_local_text
            .as_ref()
            .is_some_and(|(last, at)| last == text && at.elapsed() < DUPLICATE_TEXT_WINDOW);
        if remote_echo || duplicate_local {
            false
        } else {
            state.pending_remote_text = None;
            state.last_local_text = Some((text.to_string(), Instant::now()));
            true
        }
    };
    if should_send {
        if let Some(window) = window.upgrade() {
            window.send_clipboard_text(text.to_string());
        }
    }
}

fn read_wayland_text(
    state: Rc<RefCell<ClipboardState>>,
    window: glib::WeakRef<Window>,
    generation: u64,
) {
    if !state.borrow().enabled || state.borrow().generation != generation {
        return;
    }
    glib::spawn_future_local(async move {
        let types = run_wl_paste(&["--list-types"]).await;
        let Some(types) = types else {
            clear_pending_text(&state, generation);
            return;
        };
        let mime_types = types.lines().map(str::trim).collect::<Vec<_>>();
        if mime_types
            .iter()
            .any(|mime| *mime == "text/uri-list" || mime.starts_with("image/"))
        {
            clear_pending_text(&state, generation);
            return;
        }
        let Some(mime) = mime_types
            .iter()
            .copied()
            .find(|mime| *mime == "text/plain;charset=utf-8")
            .or_else(|| mime_types.iter().copied().find(|mime| *mime == "text/plain"))
            .or_else(|| mime_types.iter().copied().find(|mime| mime.starts_with("text/plain")))
        else {
            clear_pending_text(&state, generation);
            return;
        };
        if !state.borrow().enabled || state.borrow().generation != generation {
            return;
        }
        let text = run_wl_paste(&["--no-newline", "--type", mime]).await;
        match text {
            Some(text) => deliver_local_text(&state, &window, generation, &text),
            None => clear_pending_text(&state, generation),
        }
    });
}

async fn run_wl_paste(args: &[&str]) -> Option<String> {
    let argv = std::iter::once(OsStr::new("wl-paste"))
        .chain(args.iter().map(OsStr::new))
        .collect::<Vec<_>>();
    let process = gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    )
    .ok()?;
    let timeout_process = process.clone();
    glib::timeout_add_local_once(Duration::from_secs(5), move || {
        if !timeout_process.has_exited() {
            timeout_process.force_exit();
        }
    });
    let (stdout, _) = process.communicate_utf8_future(None).await.ok()?;
    process
        .is_successful()
        .then(|| stdout.map(|s| s.to_string()))
        .flatten()
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
