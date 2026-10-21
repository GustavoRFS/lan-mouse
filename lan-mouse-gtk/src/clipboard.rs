use std::{
    cell::RefCell,
    collections::HashSet,
    ffi::OsStr,
    fs,
    rc::Rc,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use gtk::{
    gdk, gio,
    glib::{self, types::StaticType},
    prelude::*,
};
use lan_mouse_proto::{
    ClipboardFile, ClipboardUpdateId, MAX_CLIPBOARD_FILES, MAX_CLIPBOARD_FILES_BYTES,
    MAX_CLIPBOARD_IMAGE_BYTES, MAX_CLIPBOARD_TEXT_BYTES, decode_file_bundle, encode_file_bundle,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::window::Window;

const FORMAT_RETRY_DELAY: Duration = Duration::from_millis(50);
const MAX_FORMAT_RETRIES: u8 = 5;
const DUPLICATE_TEXT_WINDOW: Duration = Duration::from_secs(1);

#[derive(Default)]
struct ClipboardState {
    enabled: bool,
    generation: u64,
    pending_remote_text: Option<(ClipboardUpdateId, String)>,
    pending_remote_image: Option<[u8; 32]>,
    pending_remote_files: Option<[u8; 32]>,
    last_local_text: Option<(String, Instant)>,
    last_local_image: Option<([u8; 32], Instant)>,
    last_local_files: Option<([u8; 32], Instant)>,
}

/// Watches the desktop text clipboard and applies remote text without echoing it.
pub(crate) struct ClipboardSync {
    clipboard: gdk::Clipboard,
    state: Rc<RefCell<ClipboardState>>,
    wayland_watcher: RefCell<Option<gio::Subprocess>>,
    staged_files: RefCell<Option<TempDir>>,
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
            staged_files: RefCell::new(None),
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
            state.pending_remote_files = None;
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
            state.pending_remote_files = None;
            state.last_local_text = None;
            state.last_local_image = None;
            state.last_local_files = None;
        }
        self.clipboard.set_text(&text);
        self.staged_files.replace(None);
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
            state.pending_remote_files = None;
            state.last_local_text = None;
            state.last_local_image = None;
            state.last_local_files = None;
        }
        self.clipboard.set_texture(&texture);
        self.staged_files.replace(None);
    }

    pub(crate) fn apply_remote_files(&self, _update_id: ClipboardUpdateId, bundle_base64: String) {
        if bundle_base64.len() > MAX_CLIPBOARD_FILES_BYTES.div_ceil(3) * 4 {
            return;
        }
        let Ok(bundle) = BASE64.decode(bundle_base64) else {
            return;
        };
        let Ok(mut files) = decode_file_bundle(&bundle) else {
            return;
        };
        if !self.state.borrow().enabled {
            return;
        }
        let Ok(staging) = tempfile::Builder::new()
            .prefix("lan-mouse-clipboard-")
            .tempdir()
        else {
            return;
        };
        let mut used_names = HashSet::new();
        let mut local_files = Vec::with_capacity(files.len());
        for (index, file) in files.iter_mut().enumerate() {
            let name = unique_file_name(&file.name, index, &mut used_names);
            let path = staging.path().join(&name);
            if fs::write(&path, &file.contents).is_err() {
                return;
            }
            // mark files from the network as internet zone so the shell
            // applies the same checks as for downloads (Mark-of-the-Web)
            #[cfg(windows)]
            {
                let mut zone = path.clone().into_os_string();
                zone.push(":Zone.Identifier");
                let _ = fs::write(zone, "[ZoneTransfer]\r\nZoneId=3\r\n");
            }
            file.name = name;
            local_files.push(gio::File::for_path(path));
        }
        let Ok(canonical) = encode_file_bundle(&files) else {
            return;
        };
        let digest: [u8; 32] = Sha256::digest(&canonical).into();
        {
            let mut state = self.state.borrow_mut();
            if !state.enabled {
                return;
            }
            state.generation = state.generation.wrapping_add(1);
            state.pending_remote_text = None;
            state.pending_remote_image = None;
            state.pending_remote_files = Some(digest);
            state.last_local_text = None;
            state.last_local_image = None;
            state.last_local_files = None;
        }
        self.staged_files.replace(Some(staging));
        #[cfg(windows)]
        if crate::windows_clipboard::publish_file_paths(
            &local_files
                .iter()
                .filter_map(gio::File::path)
                .collect::<Vec<_>>(),
        ) {
            return;
        }
        self.clipboard.set(&gdk::FileList::from_array(&local_files));
    }

    fn handle_change(self: &Rc<Self>, clipboard: &gdk::Clipboard, window: &glib::WeakRef<Window>) {
        let generation = {
            let mut state = self.state.borrow_mut();
            state.generation = state.generation.wrapping_add(1);
            state.generation
        };
        #[cfg(windows)]
        if let Some(paths) = crate::windows_clipboard::copied_file_paths() {
            read_local_files(
                self.state.clone(),
                window.clone(),
                generation,
                paths.into_iter().map(gio::File::for_path).collect(),
            );
            return;
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
                            read_local_files(state, window, generation, files.files());
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
        current.pending_remote_files = None;
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
            state.pending_remote_image = None;
            state.pending_remote_files = None;
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
            .is_some_and(|(last, at)| *last == digest && at.elapsed() < DUPLICATE_TEXT_WINDOW);
        if remote_echo || duplicate_local {
            false
        } else {
            state.pending_remote_text = None;
            state.pending_remote_image = None;
            state.pending_remote_files = None;
            state.last_local_image = Some((digest, Instant::now()));
            true
        }
    };
    if should_send {
        if let Some(window) = window.upgrade() {
            window.send_clipboard_image(BASE64.encode(png));
        }
    }
}

fn read_local_files(
    state: Rc<RefCell<ClipboardState>>,
    window: glib::WeakRef<Window>,
    generation: u64,
    files: Vec<gio::File>,
) {
    if files.is_empty() || files.len() > MAX_CLIPBOARD_FILES {
        clear_pending_text(&state, generation);
        return;
    }
    glib::spawn_future_local(async move {
        let mut entries = Vec::with_capacity(files.len());
        let mut total_bytes = 6usize;
        for file in files {
            if !state.borrow().enabled || state.borrow().generation != generation {
                return;
            }
            let Some(path) = file.path() else {
                clear_pending_text(&state, generation);
                return;
            };
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                clear_pending_text(&state, generation);
                return;
            };
            if !metadata.is_file() || metadata.len() > MAX_CLIPBOARD_FILES_BYTES as u64 {
                clear_pending_text(&state, generation);
                return;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                clear_pending_text(&state, generation);
                return;
            };
            total_bytes = total_bytes.saturating_add(2 + name.len() + 4 + metadata.len() as usize);
            if total_bytes > MAX_CLIPBOARD_FILES_BYTES {
                clear_pending_text(&state, generation);
                return;
            }
            let Ok((contents, _)) = file.load_contents_future().await else {
                clear_pending_text(&state, generation);
                return;
            };
            if contents.len() != metadata.len() as usize {
                clear_pending_text(&state, generation);
                return;
            }
            entries.push(ClipboardFile {
                name: name.to_owned(),
                contents: contents.to_vec(),
            });
        }
        if let Ok(bundle) = encode_file_bundle(&entries) {
            deliver_local_files(&state, &window, generation, &bundle);
        }
    });
}

fn deliver_local_files(
    state: &Rc<RefCell<ClipboardState>>,
    window: &glib::WeakRef<Window>,
    generation: u64,
    bundle: &[u8],
) {
    let digest: [u8; 32] = Sha256::digest(bundle).into();
    let should_send = {
        let mut state = state.borrow_mut();
        if !state.enabled || state.generation != generation {
            return;
        }
        let remote_echo = state.pending_remote_files == Some(digest);
        let duplicate_local = state
            .last_local_files
            .as_ref()
            .is_some_and(|(last, at)| *last == digest && at.elapsed() < DUPLICATE_TEXT_WINDOW);
        if remote_echo || duplicate_local {
            false
        } else {
            state.pending_remote_text = None;
            state.pending_remote_image = None;
            state.pending_remote_files = None;
            state.last_local_files = Some((digest, Instant::now()));
            true
        }
    };
    if should_send {
        if let Some(window) = window.upgrade() {
            window.send_clipboard_files(BASE64.encode(bundle));
        }
    }
}

fn unique_file_name(name: &str, index: usize, used: &mut HashSet<String>) -> String {
    let mut clean = name
        .chars()
        .map(|character| {
            if character.is_control() || "<>:\"/\\|?*".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    clean = clean.trim_end_matches([' ', '.']).to_owned();
    while clean.len() > 200 {
        clean.pop();
    }
    let stem = clean
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if clean.is_empty()
        || matches!(
            stem.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
    {
        clean = format!("file-{}", index + 1);
    }
    let mut candidate = clean.clone();
    let mut suffix = 2;
    while !used.insert(candidate.to_lowercase()) {
        candidate = format!("{clean} ({suffix})");
        suffix += 1;
    }
    candidate
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
        if mime_types.contains(&"text/uri-list") {
            if let Some(uris) = run_wl_paste(&["--type", "text/uri-list"]).await {
                let files = uris
                    .lines()
                    .filter(|line| !line.is_empty() && !line.starts_with('#'))
                    .filter_map(|uri| {
                        let file = gio::File::for_uri(uri.trim());
                        file.path().map(|_| file)
                    })
                    .collect::<Vec<_>>();
                read_local_files(state, window, generation, files);
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
                if let Ok(texture) = gdk::Texture::from_bytes(&glib::Bytes::from(bytes.as_slice()))
                {
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
