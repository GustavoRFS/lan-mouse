use std::{
    cell::RefCell,
    ffi::OsStr,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use gtk::{
    gdk, gio,
    glib::{self, types::StaticType},
    prelude::*,
};
use lan_mouse_ipc::RemoteFileOffer;
use lan_mouse_proto::{ClipboardUpdateId, MAX_CLIPBOARD_IMAGE_BYTES, MAX_CLIPBOARD_TEXT_BYTES};
use sha2::{Digest, Sha256};

use crate::{file_offer::OFFER_MARKER_MIME, window::Window};

const FORMAT_RETRY_DELAY: Duration = Duration::from_millis(50);
const MAX_FORMAT_RETRIES: u8 = 5;

#[derive(Default)]
struct ClipboardState {
    enabled: bool,
    generation: u64,
    pending_remote_text: Option<(ClipboardUpdateId, String)>,
    pending_remote_image: Option<[u8; 32]>,
    // Last content shared from this device, cleared whenever a peer's update
    // replaces the clipboard. The desktop re-announces the current selection,
    // e.g. when focus returns, which must not resend it over a peer's copy.
    last_local_text: Option<String>,
    last_local_image: Option<[u8; 32]>,
    last_local_files: Option<[u8; 32]>,
}

/// Watches the desktop text clipboard and applies remote text without echoing it.
pub(crate) struct ClipboardSync {
    clipboard: gdk::Clipboard,
    state: Rc<RefCell<ClipboardState>>,
    wayland_watcher: RefCell<Option<gio::Subprocess>>,
    /// The last offer from a peer. It owns the downloaded files, so it is kept
    /// until the next offer: a file manager may still be copying from them.
    #[cfg(not(windows))]
    remote_files: RefCell<Option<crate::lazy_files::LazyFileProvider>>,
}

impl ClipboardSync {
    pub(crate) fn new(window: &Window) -> Rc<Self> {
        let clipboard = gdk::Display::default()
            .expect("Could not connect to a display")
            .clipboard();
        let sync = Rc::new(Self {
            clipboard: clipboard.clone(),
            state: Rc::new(RefCell::new(ClipboardState::default())),
            wayland_watcher: RefCell::new(None),
            #[cfg(not(windows))]
            remote_files: RefCell::new(None),
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
            state.pending_remote_image = None;
            state.last_local_text = None;
            state.last_local_image = None;
            state.last_local_files = None;
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
            state.pending_remote_image = None;
            state.last_local_text = None;
            state.last_local_image = None;
            state.last_local_files = None;
        }
        self.clipboard.set_text(&text);
    }

    pub(crate) fn apply_remote_image(&self, _update_id: ClipboardUpdateId, png_base64: String) {
        if png_base64.len() > MAX_CLIPBOARD_IMAGE_BYTES.div_ceil(3) * 4 {
            return;
        }
        let Ok(png) = BASE64.decode(png_base64) else {
            return;
        };
        if png.len() > MAX_CLIPBOARD_IMAGE_BYTES || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
            return;
        }
        let Ok(texture) = gdk::Texture::from_bytes(&glib::Bytes::from(&png)) else {
            return;
        };
        let canonical_digest: [u8; 32] =
            Sha256::digest(texture.save_to_png_bytes().as_ref()).into();
        {
            let mut state = self.state.borrow_mut();
            if !state.enabled {
                return;
            }
            state.generation = state.generation.wrapping_add(1);
            state.pending_remote_text = None;
            state.pending_remote_image = Some(canonical_digest);
            state.last_local_text = None;
            state.last_local_image = None;
            state.last_local_files = None;
        }
        #[cfg(windows)]
        if crate::windows_clipboard::publish_image(&png, &texture) {
            return;
        }
        self.clipboard.set_texture(&texture);
    }

    /// Publishes files a peer copied. Their content is fetched only when they
    /// are pasted, so nothing is transferred for a copy that is never pasted.
    pub(crate) fn apply_remote_file_offer(
        &self,
        update_id: ClipboardUpdateId,
        offer: RemoteFileOffer,
    ) {
        {
            let mut state = self.state.borrow_mut();
            if !state.enabled {
                return;
            }
            state.generation = state.generation.wrapping_add(1);
            state.pending_remote_text = None;
            state.pending_remote_image = None;
            state.last_local_text = None;
            state.last_local_image = None;
            state.last_local_files = None;
        }
        #[cfg(windows)]
        {
            let _ = update_id;
            crate::windows_file_offer::publish(&offer);
        }
        #[cfg(not(windows))]
        {
            let provider = crate::lazy_files::LazyFileProvider::new(update_id, offer);
            if let Err(error) = self.clipboard.set_content(Some(&provider)) {
                log::warn!("could not publish files offered by a peer: {error}");
            }
            self.remote_files.replace(Some(provider));
        }
    }

    fn handle_change(self: &Rc<Self>, clipboard: &gdk::Clipboard, window: &glib::WeakRef<Window>) {
        let generation = {
            let mut state = self.state.borrow_mut();
            state.generation = state.generation.wrapping_add(1);
            state.generation
        };
        #[cfg(windows)]
        match crate::windows_clipboard::copied_files() {
            crate::windows_clipboard::CopiedFiles::Paths(paths) => {
                log::debug!("local clipboard files: {} items", paths.len());
                share_local_files(&self.state, window, generation, paths);
                return;
            }
            // reading text here would send a stale text clipboard instead
            crate::windows_clipboard::CopiedFiles::Unreadable => {
                clear_pending_text(&self.state, generation);
                return;
            }
            crate::windows_clipboard::CopiedFiles::None => {}
        }
        self.read_changed_text(clipboard, window, generation, 0);
    }

    fn handle_wayland_change(self: &Rc<Self>, window: &glib::WeakRef<Window>) {
        let generation = {
            let mut state = self.state.borrow_mut();
            state.generation = state.generation.wrapping_add(1);
            state.generation
        };
        if self.state.borrow().enabled {
            read_wayland_selection(self.state.clone(), window.clone(), generation);
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
            let mut initial_selection = true;
            loop {
                let Ok(bytes) = stdout
                    .read_bytes_future(64, glib::Priority::default())
                    .await
                else {
                    break;
                };
                if bytes.is_empty() {
                    break;
                }
                let Some(sync) = sync_weak.upgrade() else {
                    break;
                };
                for _ in bytes.as_ref() {
                    if initial_selection {
                        initial_selection = false;
                        continue;
                    }
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
        if formats.contain_mime_type(OFFER_MARKER_MIME) {
            // files a peer copied, published here; not a copy on this device
            return;
        }
        let formats_missing =
            formats.mime_types().is_empty() && !formats.contains_type(String::static_type());
        if formats_missing && glib::getenv("WAYLAND_DISPLAY").is_some() {
            read_wayland_selection(self.state.clone(), window.clone(), generation);
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
        if is_file_clipboard(clipboard) {
            let state = self.state.clone();
            let window = window.clone();
            clipboard.read_value_async(
                gdk::FileList::static_type(),
                glib::Priority::default(),
                None::<&gio::Cancellable>,
                move |result| {
                    if let Ok(value) = result {
                        if let Ok(files) = value.get::<gdk::FileList>() {
                            let paths = files.files().iter().filter_map(gio::File::path).collect();
                            share_local_files(&state, &window, generation, paths);
                        }
                    }
                },
            );
            return;
        }
        if has_image_format(clipboard) {
            let state = self.state.clone();
            let window = window.clone();
            clipboard.read_texture_async(None::<&gio::Cancellable>, move |result| {
                if let Ok(Some(texture)) = result {
                    deliver_local_image(
                        &state,
                        &window,
                        generation,
                        texture.save_to_png_bytes().as_ref(),
                    );
                }
            });
            return;
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
        current.pending_remote_image = None;
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
            .is_some_and(|last| last == text);
        if remote_echo || duplicate_local {
            false
        } else {
            state.pending_remote_text = None;
            state.pending_remote_image = None;
            state.last_local_text = Some(text.to_string());
            true
        }
    };
    if should_send {
        if let Some(window) = window.upgrade() {
            window.send_clipboard_text(text.to_string());
        }
    }
}

fn deliver_local_image(
    state: &Rc<RefCell<ClipboardState>>,
    window: &glib::WeakRef<Window>,
    generation: u64,
    png: &[u8],
) {
    if png.len() > MAX_CLIPBOARD_IMAGE_BYTES || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        clear_pending_text(state, generation);
        return;
    }
    let digest: [u8; 32] = Sha256::digest(png).into();
    let should_send = {
        let mut state = state.borrow_mut();
        if !state.enabled || state.generation != generation {
            return;
        }
        let remote_echo = state.pending_remote_image == Some(digest);
        let duplicate_local = state
            .last_local_image
            .as_ref()
            .is_some_and(|last| *last == digest);
        if remote_echo || duplicate_local {
            false
        } else {
            state.pending_remote_text = None;
            state.pending_remote_image = None;
            state.last_local_image = Some(digest);
            true
        }
    };
    log::debug!(
        "local clipboard image: {} bytes, sending: {should_send}",
        png.len()
    );
    if should_send {
        if let Some(window) = window.upgrade() {
            window.send_clipboard_image(BASE64.encode(png));
        }
    }
}

/// Offers files or folders copied on this device to peers. Only the paths go
/// to the daemon; peers fetch the content from it when they paste.
fn share_local_files(
    state: &Rc<RefCell<ClipboardState>>,
    window: &glib::WeakRef<Window>,
    generation: u64,
    paths: Vec<PathBuf>,
) {
    if paths.is_empty() {
        clear_pending_text(state, generation);
        return;
    }
    let mut hasher = Sha256::new();
    for path in &paths {
        hasher.update(path.as_os_str().as_encoded_bytes());
        hasher.update([0]);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let should_send = {
        let mut state = state.borrow_mut();
        if !state.enabled || state.generation != generation {
            return;
        }
        let duplicate_local = state
            .last_local_files
            .as_ref()
            .is_some_and(|last| *last == digest);
        if duplicate_local {
            false
        } else {
            state.pending_remote_text = None;
            state.pending_remote_image = None;
            state.last_local_files = Some(digest);
            true
        }
    };
    if should_send {
        if let Some(window) = window.upgrade() {
            window.send_clipboard_files(paths);
        }
    }
}

fn read_wayland_selection(
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
        log::debug!("wayland selection changed: {} formats", mime_types.len());
        if mime_types.contains(&OFFER_MARKER_MIME) {
            // files a peer copied, published here; reading their URIs would
            // download them, and they are not a copy on this device
            return;
        }
        if mime_types.contains(&"text/uri-list") {
            if let Some(uris) = run_wl_paste(&["--type", "text/uri-list"]).await {
                let paths = uris
                    .lines()
                    .filter(|line| !line.is_empty() && !line.starts_with('#'))
                    .filter_map(|uri| gio::File::for_uri(uri.trim()).path())
                    .collect::<Vec<_>>();
                share_local_files(&state, &window, generation, paths);
            }
            return;
        }
        if let Some(mime) = ["image/png", "image/jpeg", "image/webp"]
            .into_iter()
            .find(|candidate| mime_types.contains(candidate))
        {
            if !state.borrow().enabled || state.borrow().generation != generation {
                return;
            }
            if let Some(bytes) =
                run_wl_paste_bytes(&["--type", mime], MAX_CLIPBOARD_IMAGE_BYTES).await
            {
                let texture = gdk::Texture::from_bytes(&glib::Bytes::from(bytes.as_slice()));
                if let Err(error) = &texture {
                    log::debug!("could not decode clipboard image ({mime}): {error}");
                }
                if let Ok(texture) = texture {
                    deliver_local_image(
                        &state,
                        &window,
                        generation,
                        texture.save_to_png_bytes().as_ref(),
                    );
                }
            }
            return;
        }
        if mime_types.iter().any(|mime| mime.starts_with("image/")) {
            clear_pending_text(&state, generation);
            return;
        }
        let Some(mime) = mime_types
            .iter()
            .copied()
            .find(|mime| *mime == "text/plain;charset=utf-8")
            .or_else(|| {
                mime_types
                    .iter()
                    .copied()
                    .find(|mime| *mime == "text/plain")
            })
            .or_else(|| {
                mime_types
                    .iter()
                    .copied()
                    .find(|mime| mime.starts_with("text/plain"))
            })
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

async fn run_wl_paste_bytes(args: &[&str], max_bytes: usize) -> Option<Vec<u8>> {
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
    let stdout = process.stdout_pipe()?;
    let mut data = Vec::new();
    loop {
        let bytes = stdout
            .read_bytes_future(64 * 1024, glib::Priority::default())
            .await
            .ok()?;
        if bytes.is_empty() {
            break;
        }
        if data.len().saturating_add(bytes.len()) > max_bytes {
            process.force_exit();
            return None;
        }
        data.extend_from_slice(bytes.as_ref());
    }
    process.wait_future().await.ok()?;
    process.is_successful().then_some(data)
}

fn is_file_clipboard(clipboard: &gdk::Clipboard) -> bool {
    let formats = clipboard.formats();
    formats.contain_mime_type("text/uri-list")
        || formats.contains_type(gdk::FileList::static_type())
}

fn has_image_format(clipboard: &gdk::Clipboard) -> bool {
    let formats = clipboard.formats();
    formats
        .mime_types()
        .iter()
        .any(|mime| mime.as_str().starts_with("image/"))
        || formats.contains_type(gdk::Texture::static_type())
}

async fn run_wl_paste(args: &[&str]) -> Option<String> {
    String::from_utf8(run_wl_paste_bytes(args, MAX_CLIPBOARD_TEXT_BYTES).await?).ok()
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
