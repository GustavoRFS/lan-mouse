use std::sync::{Arc, OnceLock};

use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{
        CryptoProvider, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature,
    },
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};

fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::ring::default_provider()))
        .clone()
}

/// TLS server configuration presenting lan-mouse's own certificate, the same
/// one its DTLS sessions use. `private_key_pkcs8` is the PKCS#8 DER key.
pub fn server_config(
    certificates: Vec<CertificateDer<'static>>,
    private_key_pkcs8: Vec<u8>,
) -> Result<Arc<ServerConfig>, rustls::Error> {
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key_pkcs8));
    let config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(certificates, key)?;
    Ok(Arc::new(config))
}

/// TLS client configuration that accepts exactly the certificate with the
/// given fingerprint. Peers use self-signed certificates, so there is no CA
/// to check against; the fingerprint comes from the authenticated DTLS session.
pub(crate) fn client_config(fingerprint: &str) -> Result<Arc<ClientConfig>, rustls::Error> {
    let provider = provider();
    let verifier = PinnedCertificate {
        fingerprint: fingerprint.to_owned(),
        algorithms: provider.signature_verification_algorithms,
    };
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

pub(crate) fn server_name() -> ServerName<'static> {
    ServerName::try_from("lan-mouse").expect("static server name is valid")
}

#[derive(Debug)]
struct PinnedCertificate {
    fingerprint: String,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if crate::fingerprint(end_entity) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}
