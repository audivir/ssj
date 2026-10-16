//! QUIC transport of `ssj`: pinned mutual TLS, address validation, and transport limits.

use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::Resumption;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::NoServerSessionStorage;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use sha2::{Digest, Sha256};

pub type Fingerprint = [u8; 32];

/// Name sent as SNI. Not validated, since trust comes from the pinned fingerprint.
pub const SERVER_NAME: &str = "ssj";

/// Ephemeral self-signed certificate with its private key.
pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    pub fn generate() -> Self {
        let generated = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_owned()])
            .expect("certificate generation");
        Self {
            cert: generated.cert.der().clone(),
            key: PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der()),
        }
    }

    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint(&self.cert)
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

pub fn fingerprint(cert: &CertificateDer<'_>) -> Fingerprint {
    Sha256::digest(cert.as_ref()).into()
}

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts exactly one certificate (by fingerprint) and still verifies the handshake signature.
#[derive(Debug)]
pub struct PinnedVerifier {
    pin: Fingerprint,
    algorithms: WebPkiSupportedAlgorithms,
}

impl PinnedVerifier {
    pub fn new(pin: Fingerprint) -> Arc<Self> {
        Arc::new(Self {
            pin,
            algorithms: provider().signature_verification_algorithms,
        })
    }

    fn check_pin(&self, end_entity: &CertificateDer<'_>) -> Result<(), Error> {
        if fingerprint(end_entity) == self.pin {
            Ok(())
        } else {
            Err(Error::General(
                "certificate does not match pinned fingerprint".into(),
            ))
        }
    }

    fn verify_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.check_pin(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 is not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for PinnedVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.check_pin(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 is not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.verify_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Server TLS config that requires the client certificate with fingerprint `client_pin`.
pub fn server_tls(identity: &Identity, client_pin: Fingerprint) -> rustls::ServerConfig {
    server_tls_with(identity, PinnedVerifier::new(client_pin))
}

/// Like [`server_tls`] with a custom client verifier.
pub fn server_tls_with(
    identity: &Identity,
    verifier: Arc<dyn ClientCertVerifier>,
) -> rustls::ServerConfig {
    let mut config = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3 is supported")
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.cert.clone()], identity.private_key())
        .expect("matching certificate and key");
    config.alpn_protocols = vec![ssj_proto::alpn()];
    config.max_early_data_size = 0;
    config.send_tls13_tickets = 0;
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config
}

/// Client TLS config that accepts only the server certificate with fingerprint `server_pin`.
pub fn client_tls(identity: &Identity, server_pin: Fingerprint) -> rustls::ClientConfig {
    client_tls_with(identity, PinnedVerifier::new(server_pin))
}

/// Like [`client_tls`] with a custom server verifier.
pub fn client_tls_with(
    identity: &Identity,
    verifier: Arc<dyn ServerCertVerifier>,
) -> rustls::ClientConfig {
    let mut config = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3 is supported")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert.clone()], identity.private_key())
        .expect("matching certificate and key");
    config.alpn_protocols = vec![ssj_proto::alpn()];
    config.enable_early_data = false;
    config.resumption = Resumption::disabled();
    config
}

/// Limits shared by both roles: no datagrams, no unidirectional streams, few bidirectional ones.
fn base_transport() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    // the session must outlive long outages, and the keepalive notices a returning network
    transport.max_idle_timeout(None);
    transport.keep_alive_interval(Some(Duration::from_secs(1)));
    // a paste must not spend several round trips on slow start
    let mut cubic = quinn::congestion::CubicConfig::default();
    cubic.initial_window(INITIAL_CONGESTION_WINDOW);
    transport.congestion_controller_factory(Arc::new(cubic));
    transport.datagram_receive_buffer_size(None);
    transport.max_concurrent_uni_streams(0u8.into());
    transport.max_concurrent_bidi_streams(8u8.into());
    transport
}

/// Bytes the sender may have in flight before the first acknowledgement.
pub const INITIAL_CONGESTION_WINDOW: u64 = 256 * 1024;

/// Receive limits of the server, which only gets small input from the client.
pub const SERVER_RECEIVE_WINDOW: u32 = 2 * 1024 * 1024;
/// Receive limits of the client, which gets the (large) output of the shell.
pub const CLIENT_RECEIVE_WINDOW: u32 = 4 * 1024 * 1024;

pub fn quic_server(tls: rustls::ServerConfig) -> quinn::ServerConfig {
    let crypto = QuicServerConfig::try_from(tls).expect("QUIC-compatible TLS config");
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    let mut transport = base_transport();
    transport.receive_window(SERVER_RECEIVE_WINDOW.into());
    transport.stream_receive_window((SERVER_RECEIVE_WINDOW / 2).into());
    config.transport_config(Arc::new(transport));
    // one session, so only a handful of handshakes may be in flight at once
    config.max_incoming(16);
    config.incoming_buffer_size(64 * 1024);
    config.incoming_buffer_size_total(256 * 1024);
    config
}

pub fn quic_client(tls: rustls::ClientConfig) -> quinn::ClientConfig {
    let crypto = QuicClientConfig::try_from(tls).expect("QUIC-compatible TLS config");
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = base_transport();
    transport.receive_window(CLIENT_RECEIVE_WINDOW.into());
    transport.stream_receive_window(CLIENT_RECEIVE_WINDOW.into());
    config.transport_config(Arc::new(transport));
    config
}

/// Accepts the next connection attempt whose source address is validated.
///
/// Unvalidated attempts get a stateless retry, so a spoofed source address costs the server no
/// state and no amplified response.
pub async fn accept_validated(endpoint: &quinn::Endpoint) -> Option<quinn::Incoming> {
    loop {
        let incoming = endpoint.accept().await?;
        if incoming.remote_address_validated() {
            return Some(incoming);
        }
        // fails only if the attempt cannot be retried, which cannot happen when unvalidated
        let _ = incoming.retry();
    }
}
