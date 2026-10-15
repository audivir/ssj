mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use quinn::Endpoint;
use rustls::client::Resumption;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ServerSessionMemoryCache;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use ssj_spike::{
    Identity, PinnedVerifier, SERVER_NAME, client_tls_with, quic_client, quic_server,
    server_tls_with,
};

use common::localhost;

/// Counts how often a full certificate verification happens, which a resumed handshake skips.
#[derive(Debug)]
struct Counting {
    inner: Arc<PinnedVerifier>,
    calls: AtomicUsize,
}

impl Counting {
    fn new(pin: ssj_spike::Fingerprint) -> Arc<Self> {
        Arc::new(Self {
            inner: PinnedVerifier::new(pin),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl ServerCertVerifier for Counting {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        ServerCertVerifier::verify_server_cert(
            &*self.inner,
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        ServerCertVerifier::verify_tls12_signature(&*self.inner, message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        ServerCertVerifier::verify_tls13_signature(&*self.inner, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        ServerCertVerifier::supported_verify_schemes(&*self.inner)
    }
}

impl ClientCertVerifier for Counting {
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
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        ClientCertVerifier::verify_client_cert(&*self.inner, end_entity, intermediates, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        ClientCertVerifier::verify_tls12_signature(&*self.inner, message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        ClientCertVerifier::verify_tls13_signature(&*self.inner, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        ClientCertVerifier::supported_verify_schemes(&*self.inner)
    }
}

struct Outcome {
    server_verifications: usize,
    client_verifications: usize,
    zero_rtt_possible: bool,
}

/// Connects twice with the same client endpoint and reports what the second handshake did.
async fn connect_twice(
    server_tls: impl FnOnce(&mut rustls::ServerConfig),
    client_tls: impl FnOnce(&mut rustls::ClientConfig),
) -> Outcome {
    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let on_server = Counting::new(client_id.fingerprint());
    let on_client = Counting::new(server_id.fingerprint());

    let mut tls = server_tls_with(&server_id, on_server.clone());
    server_tls(&mut tls);
    let server = Endpoint::server(quic_server(tls), localhost()).unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        while let Some(incoming) = server.accept().await {
            if let Ok(conn) = incoming.await {
                tokio::spawn(async move { conn.closed().await });
            }
        }
    });

    let mut tls = client_tls_with(&client_id, on_client.clone());
    client_tls(&mut tls);
    let mut client = Endpoint::client(localhost()).unwrap();
    client.set_default_client_config(quic_client(tls));

    let first = client.connect(addr, SERVER_NAME).unwrap().await.unwrap();
    // session tickets arrive after the handshake
    tokio::time::sleep(Duration::from_millis(300)).await;
    first.close(0u8.into(), b"");

    let connecting = client.connect(addr, SERVER_NAME).unwrap();
    let (second, zero_rtt_possible) = match connecting.into_0rtt() {
        Ok((conn, _accepted)) => (conn, true),
        Err(connecting) => (connecting.await.unwrap(), false),
    };
    // a 0-RTT connection is usable before the server has verified anything
    let (mut send, _recv) = second.open_bi().await.unwrap();
    send.write_all(b"x").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    Outcome {
        server_verifications: on_server.calls(),
        client_verifications: on_client.calls(),
        zero_rtt_possible,
    }
}

#[tokio::test]
async fn every_connection_verifies_both_certificates_even_if_client_asks_to_resume() {
    let outcome = connect_twice(
        |_| {},
        |tls| {
            tls.resumption = Resumption::in_memory_sessions(8);
            tls.enable_early_data = true;
        },
    )
    .await;

    assert_eq!(outcome.client_verifications, 2);
    assert_eq!(outcome.server_verifications, 2);
    assert!(!outcome.zero_rtt_possible);
}

// Without this, the test above would also pass if the counters never noticed a resumption.
#[tokio::test]
async fn counters_notice_resumption_when_server_allows_it() {
    let outcome = connect_twice(
        |tls| {
            tls.send_tls13_tickets = 2;
            tls.session_storage = ServerSessionMemoryCache::new(8);
            tls.max_early_data_size = u32::MAX;
        },
        |tls| {
            tls.resumption = Resumption::in_memory_sessions(8);
            tls.enable_early_data = true;
        },
    )
    .await;

    assert_eq!(outcome.client_verifications, 1);
    assert_eq!(outcome.server_verifications, 1);
    assert!(outcome.zero_rtt_possible);
}
