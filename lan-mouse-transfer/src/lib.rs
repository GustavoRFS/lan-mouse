//! On-demand transfer of copied files and folders between lan-mouse peers.
//!
//! The device where files were copied keeps an [`Offer`] (the copied paths
//! plus a random [`Token`]) and announces only the token over its
//! authenticated DTLS session. A peer fetches the [`Manifest`] and the file
//! contents over TLS on TCP when the user pastes. The TLS server presents the
//! same certificate as the DTLS session, which the client pins by fingerprint,
//! and the client proves it received the announcement by sending the token.

mod client;
mod error;
mod manifest;
mod server;
mod tls;
mod wire;

use sha2::{Digest, Sha256};

pub use client::{Connection, Endpoint, FileStream, Pool};
pub use error::{RemoteErrorKind, TransferError};
pub use manifest::{Entry, EntryKind, Manifest};
pub use server::{Offer, OfferStore, serve_connection};
pub use tls::server_config;

/// Secret that authorizes fetching one clipboard offer.
pub type Token = [u8; 32];

/// Generates a token from the system CSPRNG.
pub fn new_token() -> Token {
    let mut token = [0u8; 32];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut token)
        .expect("system random number generator failed");
    token
}

/// SHA-256 certificate fingerprint in lan-mouse's colon-separated hex format.
pub fn fingerprint(certificate_der: &[u8]) -> String {
    Sha256::digest(certificate_der)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}
