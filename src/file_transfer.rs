use std::{
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use lan_mouse_proto::FileOfferAnnouncement;
use lan_mouse_transfer::{Offer, OfferStore, new_token, serve_connection};
use tokio::{
    net::TcpListener,
    task::{JoinHandle, spawn_local},
};
use webrtc_dtls::crypto::Certificate;

/// Peers fetching files at the same time; more connections are dropped.
const MAX_CONNECTIONS: usize = 32;

/// Serves files copied on this device to peers that paste them. Listens on
/// TCP on the same port number as the DTLS listener.
pub(crate) struct FileTransferServer {
    store: OfferStore,
    tls: Option<Arc<rustls::ServerConfig>>,
    listen_task: Option<JoinHandle<()>>,
    port: Option<u16>,
    connections: Arc<AtomicUsize>,
}

impl FileTransferServer {
    pub(crate) fn new(cert: &Certificate, port: u16) -> Self {
        let tls = lan_mouse_transfer::server_config(
            cert.certificate.clone(),
            cert.private_key.serialized_der.clone(),
        )
        .inspect_err(|e| log::warn!("file transfer disabled, unusable certificate: {e}"))
        .ok();
        let mut server = Self {
            store: OfferStore::default(),
            tls,
            listen_task: None,
            port: None,
            connections: Default::default(),
        };
        server.listen(port);
        server
    }

    /// (Re)binds the listener, e.g. after the lan-mouse port changed.
    pub(crate) fn listen(&mut self, port: u16) {
        self.terminate();
        let Some(tls) = self.tls.clone() else {
            return;
        };
        match bind(port) {
            Ok(listener) => {
                log::info!("serving copied files on TCP port {port}");
                self.port = Some(port);
                self.listen_task = Some(spawn_local(accept_loop(
                    listener,
                    tls,
                    self.store.clone(),
                    self.connections.clone(),
                )));
            }
            Err(e) => log::warn!("cannot share copied files, TCP port {port} unavailable: {e}"),
        }
    }

    /// Makes `paths` the only fetchable offer and returns its announcement,
    /// or `None` if files cannot be served.
    pub(crate) fn offer(&self, paths: Vec<PathBuf>) -> Option<FileOfferAnnouncement> {
        let port = self.port?;
        let token = new_token();
        let item_count = u32::try_from(paths.len()).ok()?;
        let offer = Offer::new(token, paths);
        let names = offer.names().to_vec();
        self.store.replace(Some(offer));
        Some(FileOfferAnnouncement {
            token,
            port,
            item_count,
            names,
        })
    }

    /// Stops serving the last offer, e.g. because something else was copied.
    pub(crate) fn withdraw(&self) {
        self.store.replace(None);
    }

    pub(crate) fn terminate(&mut self) {
        if let Some(task) = self.listen_task.take() {
            task.abort();
        }
        self.port = None;
    }
}

fn bind(port: u16) -> io::Result<TcpListener> {
    let listener = std::net::TcpListener::bind(("0.0.0.0", port))?;
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

async fn accept_loop(
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    store: OfferStore,
    connections: Arc<AtomicUsize>,
) {
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                log::warn!("file transfer accept failed: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        if connections.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            log::warn!("too many file transfer connections, dropping {addr}");
            continue;
        }
        let stream = match stream.into_std().and_then(|stream| {
            stream.set_nonblocking(false)?;
            Ok(stream)
        }) {
            Ok(stream) => stream,
            Err(e) => {
                log::warn!("file transfer connection from {addr} failed: {e}");
                continue;
            }
        };
        connections.fetch_add(1, Ordering::SeqCst);
        let (tls, store, active) = (tls.clone(), store.clone(), connections.clone());
        let spawned = thread::Builder::new()
            .name("file-transfer".into())
            .spawn(move || {
                log::debug!("file transfer connection from {addr}");
                if let Err(e) = serve_connection(stream, tls, &store) {
                    log::debug!("file transfer connection from {addr} ended: {e}");
                }
                active.fetch_sub(1, Ordering::SeqCst);
            });
        if let Err(e) = spawned {
            log::warn!("cannot serve file transfer connection: {e}");
            connections.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
