use std::io;

use thiserror::Error;

/// Failure reported by the serving peer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteErrorKind {
    /// The offer was replaced or withdrawn, e.g. something else was copied.
    InvalidToken,
    UnsupportedVersion,
    NotFound,
    PermissionDenied,
    /// A file changed size while it was being sent.
    Changed,
    InvalidRequest,
    /// A file name is not valid Unicode and cannot be represented.
    UnsupportedName,
    Io,
}

impl RemoteErrorKind {
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::InvalidToken => 1,
            Self::UnsupportedVersion => 2,
            Self::NotFound => 3,
            Self::PermissionDenied => 4,
            Self::Changed => 5,
            Self::InvalidRequest => 6,
            Self::UnsupportedName => 7,
            Self::Io => 8,
        }
    }

    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::InvalidToken,
            2 => Self::UnsupportedVersion,
            3 => Self::NotFound,
            4 => Self::PermissionDenied,
            5 => Self::Changed,
            6 => Self::InvalidRequest,
            7 => Self::UnsupportedName,
            _ => Self::Io,
        }
    }
}

#[derive(Debug, Error)]
pub enum TransferError {
    #[error("could not connect: {0}")]
    Connect(#[source] io::Error),
    #[error("the peer's certificate does not match its authenticated session")]
    IdentityMismatch,
    #[error("TLS error: {0}")]
    Tls(rustls::Error),
    #[error("timed out waiting for the peer")]
    Timeout,
    #[error("connection lost: {0}")]
    ConnectionLost(#[source] io::Error),
    #[error("peer reported {kind:?} for `{path}`: {detail}")]
    Remote {
        kind: RemoteErrorKind,
        /// Path of the affected item, relative to the copied items.
        path: String,
        detail: String,
    },
    #[error("malformed response: {0}")]
    Protocol(String),
}

impl TransferError {
    /// Classifies an I/O error raised while talking to an established peer.
    pub(crate) fn from_io(error: io::Error) -> Self {
        if let Some(tls) = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        {
            return Self::from_tls(tls.clone());
        }
        match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Self::Timeout,
            io::ErrorKind::InvalidData => Self::Protocol(error.to_string()),
            _ => Self::ConnectionLost(error),
        }
    }

    pub(crate) fn from_tls(error: rustls::Error) -> Self {
        match error {
            rustls::Error::InvalidCertificate(_) => Self::IdentityMismatch,
            error => Self::Tls(error),
        }
    }
}
