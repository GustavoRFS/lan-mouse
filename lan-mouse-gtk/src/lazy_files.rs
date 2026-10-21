//! Pasting a peer's files with GDK (Linux, macOS). The clipboard advertises
//! file URIs; the files are downloaded into a temporary folder the first time
//! an application asks for them, which holds the paste until they arrived.

use std::{
    cell::RefCell,
    fs,
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
    pin::Pin,
    rc::Rc,
    thread,
};

use gtk::{gdk, gio, glib, prelude::*, subclass::prelude::*};
use lan_mouse_ipc::RemoteFileOffer;
use lan_mouse_transfer::{Connection, EntryKind, TransferError};
use tempfile::TempDir;

use crate::{file_offer, notify};

const URI_LIST: &str = "text/uri-list";
const PLAIN_TEXT: &str = "text/plain;charset=utf-8";
const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";

glib::wrapper! {
    pub(crate) struct LazyFileProvider(ObjectSubclass<imp::LazyFileProvider>)
        @extends gdk::ContentProvider;
}

impl LazyFileProvider {
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) fn new(update_id: [u8; 16], offer: RemoteFileOffer) -> Self {
        let provider: Self = glib::Object::new();
        provider.imp().offer.replace(Some(Rc::new(PendingOffer {
            update_id,
            offer,
            state: RefCell::new(State::Idle),
        })));
        provider
    }
}

struct PendingOffer {
    update_id: [u8; 16],
    offer: RemoteFileOffer,
    state: RefCell<State>,
}

type Waiter = async_channel::Sender<Result<Vec<PathBuf>, String>>;

enum State {
    Idle,
    Downloading {
        staging: TempDir,
        waiters: Vec<Waiter>,
    },
    Done {
        _staging: TempDir,
        roots: Vec<PathBuf>,
    },
}

impl PendingOffer {
    /// Paths of the copied items, downloading them first if needed.
    async fn roots(self: Rc<Self>) -> Result<Vec<PathBuf>, String> {
        let (waiter, result) = async_channel::bounded(1);
        let start = {
            let mut state = self.state.borrow_mut();
            match &mut *state {
                State::Done { roots, .. } => return Ok(roots.clone()),
                State::Downloading { waiters, .. } => {
                    waiters.push(waiter);
                    None
                }
                State::Idle => {
                    let staging = tempfile::Builder::new()
                        .prefix("lan-mouse-clipboard-")
                        .tempdir()
                        .map_err(|e| file_offer::describe_local_error(&e, &std::env::temp_dir()))?;
                    let path = staging.path().to_owned();
                    *state = State::Downloading {
                        staging,
                        waiters: vec![waiter],
                    };
                    Some(path)
                }
            }
        };
        if let Some(staging) = start {
            self.clone().start_download(staging);
        }
        result
            .recv()
            .await
            .unwrap_or_else(|_| Err("A cópia foi interrompida.".to_owned()))
    }

    fn start_download(self: Rc<Self>, staging: PathBuf) {
        notify::transfer_started(&format!(
            "Recebendo {} de {}…",
            file_offer::describe_items(&self.offer),
            self.offer.host
        ));
        let (sender, receiver) = async_channel::bounded(1);
        let offer = self.offer.clone();
        thread::spawn(move || {
            let _ = sender.send_blocking(download(&offer, &staging));
        });
        glib::spawn_future_local(async move {
            let result = receiver
                .recv()
                .await
                .unwrap_or_else(|_| Err("A cópia foi interrompida.".to_owned()));
            self.finish(result);
        });
    }

    fn finish(&self, result: Result<Vec<PathBuf>, String>) {
        let State::Downloading { staging, waiters } = self.state.replace(State::Idle) else {
            return;
        };
        match &result {
            Ok(roots) => {
                log::info!(
                    "received {} copied items from {}",
                    roots.len(),
                    self.offer.host
                );
                self.state.replace(State::Done {
                    _staging: staging,
                    roots: roots.clone(),
                });
            }
            // dropping `staging` removes the partial download; a later paste retries
            Err(message) => notify::transfer_failed(message),
        }
        for waiter in waiters {
            let _ = waiter.try_send(result.clone());
        }
    }
}

/// Fetches all entries of the offer into `staging`. Runs on its own thread.
fn download(offer: &RemoteFileOffer, staging: &Path) -> Result<Vec<PathBuf>, String> {
    let transfer_error = |error: TransferError| file_offer::describe_error(&error, &offer.host);
    let mut connection = Connection::open(&file_offer::endpoint(offer)).map_err(transfer_error)?;
    let manifest = connection.manifest().map_err(transfer_error)?;
    let mut roots = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    for (entry, relative) in manifest
        .entries
        .iter()
        .zip(file_offer::local_paths(&manifest))
    {
        let path = staging.join(relative);
        let local_error = |error: io::Error| file_offer::describe_local_error(&error, &path);
        match entry.kind {
            EntryKind::Directory => fs::create_dir_all(&path).map_err(local_error)?,
            EntryKind::File => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(local_error)?;
                }
                let mut file = fs::File::create(&path).map_err(local_error)?;
                let mut stream = connection
                    .file(entry.root, &entry.components, 0)
                    .map_err(transfer_error)?;
                loop {
                    let read = stream.read_chunk(&mut buf).map_err(transfer_error)?;
                    if read == 0 {
                        break;
                    }
                    file.write_all(&buf[..read]).map_err(local_error)?;
                }
                if let Some(modified) = entry.modified {
                    let _ = file.set_modified(modified);
                }
                connection = stream.into_connection().ok_or_else(|| {
                    transfer_error(TransferError::Protocol("connection not reusable".into()))
                })?;
            }
        }
        if entry.components.is_empty() {
            roots.push(path);
        }
    }
    Ok(roots)
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub(crate) struct LazyFileProvider {
        pub(super) offer: RefCell<Option<Rc<PendingOffer>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LazyFileProvider {
        const NAME: &'static str = "LanMouseLazyFileProvider";
        type Type = super::LazyFileProvider;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for LazyFileProvider {}

    impl ContentProviderImpl for LazyFileProvider {
        fn formats(&self) -> gdk::ContentFormats {
            // Clipboard watchers such as `wl-paste --type text --watch` read
            // every new selection and take any text/* type, including the URI
            // list, which would start the download as soon as files are
            // copied. They prefer plain text, so it is offered first and only
            // names the items; file managers still ask for the URI list.
            gdk::ContentFormatsBuilder::new()
                .add_mime_type(PLAIN_TEXT)
                .add_mime_type(URI_LIST)
                .add_mime_type(GNOME_COPIED_FILES)
                .add_mime_type(file_offer::OFFER_MARKER_MIME)
                .build()
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'static>> {
            let offer = self.offer.borrow().clone();
            let mime_type = mime_type.to_owned();
            let stream = stream.clone();
            Box::pin(async move {
                let failed = |message: &str| glib::Error::new(gio::IOErrorEnum::Failed, message);
                let offer = offer.ok_or_else(|| failed("no file offer"))?;
                let content = match mime_type.as_str() {
                    file_offer::OFFER_MARKER_MIME => offer
                        .update_id
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>(),
                    PLAIN_TEXT => offer.offer.names.join("\n"),
                    URI_LIST | GNOME_COPIED_FILES => {
                        let roots = offer.clone().roots().await.map_err(|m| failed(&m))?;
                        let uris = roots
                            .iter()
                            .map(|path| glib::filename_to_uri(path, None).map(String::from))
                            .collect::<Result<Vec<_>, _>>()?;
                        if mime_type == URI_LIST {
                            uris.iter().map(|uri| format!("{uri}\r\n")).collect()
                        } else {
                            format!("copy\n{}", uris.join("\n"))
                        }
                    }
                    _ => {
                        return Err(glib::Error::new(
                            gio::IOErrorEnum::NotSupported,
                            "unsupported format",
                        ));
                    }
                };
                let (_, _, error) = stream
                    .write_all_future(content.into_bytes(), io_priority)
                    .await
                    .map_err(|(_, error)| error)?;
                error.map_or(Ok(()), Err)
            })
        }
    }
}
