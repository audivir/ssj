mod common;

use std::sync::Arc;

use rustls::SignatureScheme;
use rustls::client::ResolvesClientCert;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use ssj_net::{Identity, SERVER_NAME, client_tls, provider, quic_client, quic_server, server_tls};

use common::{client_endpoint, localhost, server_endpoint};

/// Presents the certificate of someone else, signing with a different private key.
#[derive(Debug)]
struct StolenCert(Arc<CertifiedKey>);

impl StolenCert {
    fn new(victim: &Identity, attacker: &Identity) -> Arc<Self> {
        let signer = provider()
            .key_provider
            .load_private_key(attacker.private_key())
            .unwrap();
        Arc::new(Self(Arc::new(CertifiedKey::new(
            vec![victim.cert.clone()],
            signer,
        ))))
    }
}

impl ResolvesServerCert for StolenCert {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

impl ResolvesClientCert for StolenCert {
    fn resolve(&self, _hints: &[&[u8]], _schemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// Runs the server side of one handshake and returns whether the client was accepted.
async fn server_accepts(endpoint: quinn::Endpoint) -> bool {
    match endpoint.accept().await {
        Some(incoming) => incoming.await.is_ok(),
        None => false,
    }
}

#[tokio::test]
async fn pinned_peers_exchange_data() {
    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let server = server_endpoint(&server_id, client_id.fingerprint());
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let msg = recv.read_to_end(16).await.unwrap();
        send.write_all(&msg).await.unwrap();
        send.finish().unwrap();
        conn.closed().await;
    });

    let conn = client_endpoint(&client_id, server_id.fingerprint())
        .connect(addr, SERVER_NAME)
        .unwrap()
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(b"hello").await.unwrap();
    send.finish().unwrap();
    assert_eq!(recv.read_to_end(16).await.unwrap(), b"hello");
}

#[tokio::test]
async fn client_rejects_server_with_other_certificate() {
    let (pinned, impostor, client_id) = (
        Identity::generate(),
        Identity::generate(),
        Identity::generate(),
    );
    let server = server_endpoint(&impostor, client_id.fingerprint());
    let addr = server.local_addr().unwrap();
    tokio::spawn(server_accepts(server));

    let result = client_endpoint(&client_id, pinned.fingerprint())
        .connect(addr, SERVER_NAME)
        .unwrap()
        .await;
    assert!(result.is_err());
}

// passes the fingerprint check, so only the handshake signature can reject it.
#[tokio::test]
async fn client_rejects_pinned_server_certificate_without_its_private_key() {
    let (victim, attacker, client_id) = (
        Identity::generate(),
        Identity::generate(),
        Identity::generate(),
    );
    let mut tls = server_tls(&attacker, client_id.fingerprint());
    tls.cert_resolver = StolenCert::new(&victim, &attacker);
    let server = quinn::Endpoint::server(quic_server(tls), localhost()).unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(server_accepts(server));

    let result = client_endpoint(&client_id, victim.fingerprint())
        .connect(addr, SERVER_NAME)
        .unwrap()
        .await;
    let err = result.expect_err("handshake must fail").to_string();
    assert!(
        err.contains("BadSignature") || err.contains("signature"),
        "{err}"
    );
}

#[tokio::test]
async fn server_rejects_client_with_other_certificate() {
    let (server_id, pinned, impostor) = (
        Identity::generate(),
        Identity::generate(),
        Identity::generate(),
    );
    let server = server_endpoint(&server_id, pinned.fingerprint());
    let addr = server.local_addr().unwrap();
    let accepted = tokio::spawn(server_accepts(server));

    let _ = client_endpoint(&impostor, server_id.fingerprint())
        .connect(addr, SERVER_NAME)
        .unwrap()
        .await;
    assert!(!accepted.await.unwrap());
}

#[tokio::test]
async fn server_rejects_pinned_client_certificate_without_its_private_key() {
    let (server_id, victim, attacker) = (
        Identity::generate(),
        Identity::generate(),
        Identity::generate(),
    );
    let server = server_endpoint(&server_id, victim.fingerprint());
    let addr = server.local_addr().unwrap();
    let accepted = tokio::spawn(server_accepts(server));

    let mut tls = client_tls(&attacker, server_id.fingerprint());
    tls.client_auth_cert_resolver = StolenCert::new(&victim, &attacker);
    let mut endpoint = quinn::Endpoint::client(localhost()).unwrap();
    endpoint.set_default_client_config(quic_client(tls));
    let _ = endpoint.connect(addr, SERVER_NAME).unwrap().await;
    assert!(!accepted.await.unwrap());
}
