//! Desktop notifications about pasting files from peers.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

/// A paste can ask for the same data several times in a row; one failure
/// should produce one notification.
const REPEAT_WINDOW: Duration = Duration::from_secs(5);

/// Tells the user why pasting files from a peer failed. Callable from any thread.
pub(crate) fn transfer_failed(message: &str) {
    log::warn!("file transfer failed: {message}");
    if is_repeat(message) {
        return;
    }
    show("Falha ao colar arquivos", message, true);
}

/// Tells the user that a paste waits for files from a peer.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn transfer_started(message: &str) {
    show("Lan Mouse", message, false);
}

fn is_repeat(message: &str) -> bool {
    static LAST: Mutex<Option<(String, Instant)>> = Mutex::new(None);
    let mut last = LAST.lock().expect("lock");
    let repeat = last
        .as_ref()
        .is_some_and(|(text, at)| text == message && at.elapsed() < REPEAT_WINDOW);
    *last = Some((message.to_owned(), Instant::now()));
    repeat
}

#[cfg(windows)]
fn show(title: &str, message: &str, error: bool) {
    crate::windows_tray::notify(title, message, error);
}

#[cfg(not(windows))]
fn show(title: &str, message: &str, _error: bool) {
    use gtk::{gio, glib, prelude::*};

    let (title, message) = (title.to_owned(), message.to_owned());
    glib::MainContext::default().invoke(move || {
        let Some(app) = gio::Application::default() else {
            return;
        };
        let notification = gio::Notification::new(&title);
        notification.set_body(Some(&message));
        app.send_notification(Some("lan-mouse-file-transfer"), &notification);
    });
}
