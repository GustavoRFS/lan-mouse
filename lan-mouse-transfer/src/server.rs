use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    net::TcpStream,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use rustls::{ServerConfig, ServerConnection, StreamOwned};

use crate::{
    Manifest, RemoteErrorKind, Token,
    manifest::{self, RemoteError},
    wire,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a client may keep an idle connection for its next request.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// Files and folders copied on this device that peers may fetch.
pub struct Offer {
    token: Token,
    roots: Vec<PathBuf>,
    names: Vec<String>,
}

impl Offer {
    pub fn new(token: Token, roots: Vec<PathBuf>) -> Self {
        let names = manifest::root_names(&roots);
        Self {
            token,
            roots,
            names,
        }
    }

    /// Unique display names of the copied items.
    pub fn names(&self) -> &[String] {
        &self.names
    }
}

/// The offer currently being served. Only the latest copy can be fetched.
#[derive(Clone, Default)]
pub struct OfferStore {
    current: Arc<Mutex<Option<Arc<Offer>>>>,
}

impl OfferStore {
    pub fn replace(&self, offer: Option<Offer>) {
        *self.current.lock().expect("lock") = offer.map(Arc::new);
    }

    fn find(&self, token: &Token) -> Option<Arc<Offer>> {
        self.current
            .lock()
            .expect("lock")
            .as_ref()
            .filter(|offer| constant_time_eq(&offer.token, token))
            .cloned()
    }
}

/// Serves one client connection until it disconnects. Blocks; run it on a
/// dedicated thread.
pub fn serve_connection(
    stream: TcpStream,
    config: Arc<ServerConfig>,
    store: &OfferStore,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let connection = ServerConnection::new(config).map_err(io::Error::other)?;
    let mut tls = StreamOwned::new(connection, stream);

    let mut magic = [0u8; 4];
    tls.read_exact(&mut magic)?;
    if &magic != wire::MAGIC {
        return Err(wire::invalid("not a lan-mouse transfer client"));
    }
    let version = wire::read_u8(&mut tls)?;
    let mut token = [0u8; 32];
    tls.read_exact(&mut token)?;
    if version != wire::VERSION {
        reply_error(&mut tls, RemoteErrorKind::UnsupportedVersion, "", "")?;
        return close(tls);
    }
    let Some(offer) = store.find(&token) else {
        reply_error(
            &mut tls,
            RemoteErrorKind::InvalidToken,
            "",
            "offer withdrawn",
        )?;
        return close(tls);
    };
    wire::write_u8(&mut tls, wire::STATUS_OK)?;
    tls.flush()?;
    tls.sock.set_read_timeout(Some(IDLE_TIMEOUT))?;

    loop {
        let op = match wire::read_u8(&mut tls) {
            Ok(op) => op,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        let request = match op {
            wire::OP_MANIFEST => Request::Manifest,
            wire::OP_FILE => Request::File {
                root: wire::read_u32(&mut tls)?,
                components: wire::read_components(&mut tls)?,
                offset: wire::read_u64(&mut tls)?,
            },
            _ => return Err(wire::invalid("unknown request")),
        };
        // a newer copy on this device withdraws the offer for every client
        if store.find(&token).is_none() {
            reply_error(
                &mut tls,
                RemoteErrorKind::InvalidToken,
                "",
                "offer withdrawn",
            )?;
            return close(tls);
        }
        match request {
            Request::Manifest => match Manifest::build(&offer.roots, &offer.names) {
                Ok(manifest) => {
                    wire::write_u8(&mut tls, wire::STATUS_OK)?;
                    manifest.write(&mut tls)?;
                }
                Err(error) => reply_error(&mut tls, error.kind, &error.path, &error.detail)?,
            },
            Request::File {
                root,
                components,
                offset,
            } => send_file(&mut tls, &offer, root, &components, offset)?,
        }
        tls.flush()?;
    }
}

enum Request {
    Manifest,
    File {
        root: u32,
        components: Vec<String>,
        offset: u64,
    },
}

type TlsStream = StreamOwned<ServerConnection, TcpStream>;

fn send_file(
    tls: &mut TlsStream,
    offer: &Offer,
    root: u32,
    components: &[String],
    offset: u64,
) -> io::Result<()> {
    let display = manifest::display_path(
        offer
            .names
            .get(root as usize)
            .map(String::as_str)
            .unwrap_or_default(),
        components,
    );
    let opened = manifest::resolve(&offer.roots, &offer.names, root, components).and_then(|path| {
        let mut file =
            File::open(&path).map_err(|error| RemoteError::io(&error, display.clone()))?;
        let metadata = file
            .metadata()
            .map_err(|error| RemoteError::io(&error, display.clone()))?;
        if !metadata.is_file() {
            return Err(RemoteError::new(
                RemoteErrorKind::NotFound,
                display.clone(),
                "not a file",
            ));
        }
        if offset > metadata.len() {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidRequest,
                display.clone(),
                "offset past the end of the file",
            ));
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| RemoteError::io(&error, display.clone()))?;
        Ok((file, metadata.len()))
    });
    let (mut file, size) = match opened {
        Ok(opened) => opened,
        Err(error) => return reply_error(tls, error.kind, &error.path, &error.detail),
    };

    wire::write_u8(tls, wire::STATUS_OK)?;
    wire::write_u64(tls, size)?;
    let mut remaining = size - offset;
    let mut buf = vec![0u8; wire::MAX_FRAME];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let read = match file.read(&mut buf[..want]) {
            Ok(0) => {
                wire::write_u8(tls, wire::FRAME_ERROR)?;
                return wire::write_error(
                    tls,
                    RemoteErrorKind::Changed,
                    &display,
                    "file shrank while it was sent",
                );
            }
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let error = RemoteError::io(&error, display);
                wire::write_u8(tls, wire::FRAME_ERROR)?;
                return wire::write_error(tls, error.kind, &error.path, &error.detail);
            }
        };
        wire::write_u8(tls, wire::FRAME_DATA)?;
        wire::write_u32(tls, read as u32)?;
        tls.write_all(&buf[..read])?;
        remaining -= read as u64;
    }
    wire::write_u8(tls, wire::FRAME_END)
}

fn reply_error(
    tls: &mut TlsStream,
    kind: RemoteErrorKind,
    path: &str,
    detail: &str,
) -> io::Result<()> {
    wire::write_u8(tls, wire::STATUS_ERROR)?;
    wire::write_error(tls, kind, path, detail)?;
    tls.flush()
}

fn close(mut tls: TlsStream) -> io::Result<()> {
    tls.conn.send_close_notify();
    tls.flush()
}

fn constant_time_eq(a: &Token, b: &Token) -> bool {
    a.iter().zip(b).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}
