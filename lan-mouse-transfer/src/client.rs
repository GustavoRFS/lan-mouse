use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    sync::Mutex,
    time::Duration,
};

use rustls::{ClientConnection, StreamOwned};

use crate::{Manifest, Token, TransferError, tls, wire};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Building a manifest walks the copied folders on the peer, which can take a
/// while for large trees before the first byte of the reply arrives.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_IDLE_CONNECTIONS: usize = 4;

/// Where and how to fetch one offer.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub addr: SocketAddr,
    /// Fingerprint of the peer's certificate, taken from its DTLS session.
    pub fingerprint: String,
    pub token: Token,
}

/// An authenticated connection to the peer that serves an offer. Requests
/// are answered in order, so one connection serves one request at a time.
pub struct Connection {
    tls: StreamOwned<ClientConnection, TcpStream>,
}

impl Connection {
    pub fn open(endpoint: &Endpoint) -> Result<Self, TransferError> {
        let socket = TcpStream::connect_timeout(&endpoint.addr, CONNECT_TIMEOUT)
            .map_err(TransferError::Connect)?;
        socket.set_nodelay(true).map_err(TransferError::Connect)?;
        socket
            .set_read_timeout(Some(READ_TIMEOUT))
            .map_err(TransferError::Connect)?;
        socket
            .set_write_timeout(Some(READ_TIMEOUT))
            .map_err(TransferError::Connect)?;
        let config = tls::client_config(&endpoint.fingerprint).map_err(TransferError::Tls)?;
        let connection =
            ClientConnection::new(config, tls::server_name()).map_err(TransferError::Tls)?;
        let mut tls = StreamOwned::new(connection, socket);
        while tls.conn.is_handshaking() {
            tls.conn
                .complete_io(&mut tls.sock)
                .map_err(TransferError::from_io)?;
        }

        let mut connection = Self { tls };
        connection
            .send(|w| {
                w.write_all(wire::MAGIC)?;
                wire::write_u8(w, wire::VERSION)?;
                w.write_all(&endpoint.token)
            })
            .and_then(|()| connection.expect_ok())?;
        Ok(connection)
    }

    pub fn manifest(&mut self) -> Result<Manifest, TransferError> {
        self.send(|w| wire::write_u8(w, wire::OP_MANIFEST))?;
        self.tls
            .sock
            .set_read_timeout(Some(MANIFEST_TIMEOUT))
            .map_err(TransferError::from_io)?;
        let result = self
            .expect_ok()
            .and_then(|()| Manifest::read(&mut self.tls).map_err(TransferError::from_io));
        self.tls
            .sock
            .set_read_timeout(Some(READ_TIMEOUT))
            .map_err(TransferError::from_io)?;
        result
    }

    /// Requests the content of a file entry starting at `offset`. The
    /// connection is handed back by [`FileStream::into_connection`] once the
    /// file was read to its end.
    pub fn file(
        mut self,
        root: u32,
        components: &[String],
        offset: u64,
    ) -> Result<FileStream, TransferError> {
        self.send(|w| {
            wire::write_u8(w, wire::OP_FILE)?;
            wire::write_u32(w, root)?;
            wire::write_components(w, components)?;
            wire::write_u64(w, offset)
        })?;
        self.expect_ok()?;
        let size = wire::read_u64(&mut self.tls).map_err(TransferError::from_io)?;
        if offset > size {
            return Err(TransferError::Protocol(
                "offset past the end of the file".into(),
            ));
        }
        Ok(FileStream {
            connection: Some(self),
            size,
            position: offset,
            frame_left: 0,
            finished: false,
        })
    }

    fn send(
        &mut self,
        write: impl FnOnce(&mut StreamOwned<ClientConnection, TcpStream>) -> io::Result<()>,
    ) -> Result<(), TransferError> {
        write(&mut self.tls)
            .and_then(|()| self.tls.flush())
            .map_err(TransferError::from_io)
    }

    fn expect_ok(&mut self) -> Result<(), TransferError> {
        match wire::read_u8(&mut self.tls).map_err(TransferError::from_io)? {
            wire::STATUS_OK => Ok(()),
            wire::STATUS_ERROR => Err(self.read_remote_error()),
            status => Err(TransferError::Protocol(format!("unknown status {status}"))),
        }
    }

    fn read_remote_error(&mut self) -> TransferError {
        match wire::read_error(&mut self.tls) {
            Ok((kind, path, detail)) => TransferError::Remote { kind, path, detail },
            Err(error) => TransferError::from_io(error),
        }
    }
}

/// Content of one file as it arrives from the peer.
pub struct FileStream {
    connection: Option<Connection>,
    size: u64,
    position: u64,
    frame_left: usize,
    finished: bool,
}

impl FileStream {
    /// Size of the file on the peer when the request was served.
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn position(&self) -> u64 {
        self.position
    }

    /// Reads the next bytes of the file; `Ok(0)` marks its end.
    pub fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize, TransferError> {
        if self.finished || buf.is_empty() {
            return Ok(0);
        }
        let Some(connection) = self.connection.as_mut() else {
            return Err(TransferError::Protocol("stream already failed".into()));
        };
        let result = (|| {
            if self.frame_left == 0 {
                match wire::read_u8(&mut connection.tls).map_err(TransferError::from_io)? {
                    wire::FRAME_DATA => {
                        let len = wire::read_u32(&mut connection.tls)
                            .map_err(TransferError::from_io)?
                            as usize;
                        if len == 0 || len > wire::MAX_FRAME {
                            return Err(TransferError::Protocol("invalid frame length".into()));
                        }
                        self.frame_left = len;
                    }
                    wire::FRAME_END => {
                        if self.position != self.size {
                            return Err(TransferError::Protocol("file ended early".into()));
                        }
                        self.finished = true;
                        return Ok(0);
                    }
                    wire::FRAME_ERROR => {
                        return Err(match wire::read_error(&mut connection.tls) {
                            Ok((kind, path, detail)) => {
                                TransferError::Remote { kind, path, detail }
                            }
                            Err(error) => TransferError::from_io(error),
                        });
                    }
                    frame => return Err(TransferError::Protocol(format!("unknown frame {frame}"))),
                }
            }
            let want = buf.len().min(self.frame_left);
            let read = connection
                .tls
                .read(&mut buf[..want])
                .map_err(TransferError::from_io)?;
            if read == 0 {
                return Err(TransferError::ConnectionLost(
                    io::ErrorKind::UnexpectedEof.into(),
                ));
            }
            self.frame_left -= read;
            self.position += read as u64;
            if self.position > self.size {
                return Err(TransferError::Protocol(
                    "file is longer than announced".into(),
                ));
            }
            Ok(read)
        })();
        if result.is_err() {
            // the stream's framing is unknown after an error
            self.connection = None;
        }
        result
    }

    /// Reads the end marker if the content was consumed, so the connection
    /// can serve another request.
    pub fn finish(&mut self) -> Result<(), TransferError> {
        if !self.finished && self.position == self.size {
            let mut probe = [0u8; 1];
            if self.read_chunk(&mut probe)? != 0 {
                return Err(TransferError::Protocol(
                    "file is longer than announced".into(),
                ));
            }
        }
        Ok(())
    }

    /// The connection, if the whole file was read and it can be reused.
    pub fn into_connection(self) -> Option<Connection> {
        self.finished.then_some(self.connection).flatten()
    }
}

impl Read for FileStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_chunk(buf).map_err(io::Error::other)
    }
}

/// Idle connections to one endpoint, so pasting many small files does not
/// pay a TLS handshake per file.
#[derive(Default)]
pub struct Pool {
    idle: Mutex<Vec<Connection>>,
}

impl Pool {
    pub fn manifest(&self, endpoint: &Endpoint) -> Result<Manifest, TransferError> {
        self.with_retry(endpoint, |mut connection| {
            let manifest = connection.manifest()?;
            Ok((manifest, Some(connection)))
        })
    }

    pub fn file(
        &self,
        endpoint: &Endpoint,
        root: u32,
        components: &[String],
        offset: u64,
    ) -> Result<FileStream, TransferError> {
        self.with_retry(endpoint, |connection| {
            Ok((connection.file(root, components, offset)?, None))
        })
    }

    /// Returns a connection whose last request completed.
    pub fn put(&self, connection: Connection) {
        let mut idle = self.idle.lock().expect("lock");
        if idle.len() < MAX_IDLE_CONNECTIONS {
            idle.push(connection);
        }
    }

    /// Runs a request on an idle connection, falling back to a new one if the
    /// idle connection turns out to be closed by the peer in the meantime.
    fn with_retry<T>(
        &self,
        endpoint: &Endpoint,
        request: impl Fn(Connection) -> Result<(T, Option<Connection>), TransferError>,
    ) -> Result<T, TransferError> {
        let idle = self.idle.lock().expect("lock").pop();
        if let Some(connection) = idle {
            match request(connection) {
                Ok((value, connection)) => {
                    if let Some(connection) = connection {
                        self.put(connection);
                    }
                    return Ok(value);
                }
                Err(TransferError::ConnectionLost(_) | TransferError::Timeout) => {}
                Err(error) => return Err(error),
            }
        }
        let (value, connection) = request(Connection::open(endpoint)?)?;
        if let Some(connection) = connection {
            self.put(connection);
        }
        Ok(value)
    }
}
