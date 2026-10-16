mod common;

use std::time::{Duration, Instant};

use quinn::Endpoint;
use ssj_spike::{ALPN, Identity, SERVER_NAME, accept_validated, client_tls, quic_client};
use tokio::time::{sleep, timeout};

use common::{Proxy, client_endpoint, localhost, server_endpoint};

/// Keeps a connection attempt alive until it ends, like a server that serves it.
fn hold(incoming: quinn::Incoming) {
    tokio::spawn(async move {
        if let Ok(conn) = incoming.await {
            conn.closed().await;
        }
    });
}

/// Client that offers `alpn` instead of the protocol of `ssj`.
fn client_with_alpn(
    identity: &Identity,
    server_pin: ssj_spike::Fingerprint,
    alpn: &[&[u8]],
) -> Endpoint {
    let mut tls = client_tls(identity, server_pin);
    tls.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    let mut endpoint = Endpoint::client(localhost()).unwrap();
    endpoint.set_default_client_config(quic_client(tls));
    endpoint
}

#[tokio::test]
async fn handshake_without_the_ssj_alpn_is_refused() {
    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let server = server_endpoint(&server_id, client_id.fingerprint());
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        while let Some(incoming) = server.accept().await {
            hold(incoming);
        }
    });

    // a correct client with the same setup connects, so the refusals below are about ALPN
    let ok = client_with_alpn(&client_id, server_id.fingerprint(), &[ALPN]);
    assert!(ok.connect(addr, SERVER_NAME).unwrap().await.is_ok());

    for alpn in [&[b"h3".as_slice()][..], &[b"ssj/2"], &[]] {
        let client = client_with_alpn(&client_id, server_id.fingerprint(), alpn);
        let result = client.connect(addr, SERVER_NAME).unwrap().await;
        assert!(result.is_err(), "ALPN {alpn:?} must be refused");
    }
}

/// Client whose connection attempt reaches the server, but whose replies never come back.
async fn unanswered_attempt(accept_with_retry: bool) -> usize {
    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let server = server_endpoint(&server_id, client_id.fingerprint());
    let proxy = Proxy::start(server.local_addr().unwrap()).await;
    proxy.set_drop_replies(true);

    let watched = server.clone();
    tokio::spawn(async move {
        loop {
            let incoming = if accept_with_retry {
                accept_validated(&server).await
            } else {
                server.accept().await
            };
            match incoming {
                Some(incoming) => {
                    hold(incoming);
                }
                None => break,
            }
        }
    });

    let client = client_endpoint(&client_id, server_id.fingerprint());
    let _attempt = client.connect(proxy.addr, SERVER_NAME).unwrap();
    sleep(Duration::from_millis(500)).await;
    watched.open_connections()
}

// A spoofed source address never sees the retry, so it must cost the server nothing.
#[tokio::test]
async fn unanswered_attempt_leaves_no_state_with_retry_but_does_without() {
    assert_eq!(unanswered_attempt(true).await, 0);
    assert_eq!(unanswered_attempt(false).await, 1);
}

#[tokio::test]
async fn retry_does_not_stop_honest_clients() {
    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let server = server_endpoint(&server_id, client_id.fingerprint());
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        while let Some(incoming) = accept_validated(&server).await {
            hold(incoming);
        }
    });

    let client = client_endpoint(&client_id, server_id.fingerprint());
    assert!(client.connect(addr, SERVER_NAME).unwrap().await.is_ok());
}

#[tokio::test]
async fn server_offers_no_datagrams_and_no_unidirectional_streams() {
    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let server = server_endpoint(&server_id, client_id.fingerprint());
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        while let Some(incoming) = server.accept().await {
            hold(incoming);
        }
    });

    let client = client_endpoint(&client_id, server_id.fingerprint());
    let conn = client.connect(addr, SERVER_NAME).unwrap().await.unwrap();

    assert!(conn.max_datagram_size().is_none());
    assert!(
        timeout(Duration::from_millis(500), conn.open_uni())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn server_limits_concurrent_bidirectional_streams() {
    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let server = server_endpoint(&server_id, client_id.fingerprint());
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        while let Some(incoming) = server.accept().await {
            hold(incoming);
        }
    });

    let client = client_endpoint(&client_id, server_id.fingerprint());
    let conn = client.connect(addr, SERVER_NAME).unwrap().await.unwrap();

    let mut open = Vec::new();
    for _ in 0..8 {
        open.push(conn.open_bi().await.unwrap());
    }
    assert!(
        timeout(Duration::from_millis(500), conn.open_bi())
            .await
            .is_err()
    );
}

// Pasting a large text must not crawl on a slow link.
#[tokio::test]
async fn large_paste_arrives_intact_and_fast_at_100ms_round_trip() {
    const PASTE: usize = 8 * 1024 * 1024;
    let paste: Vec<u8> = (0..PASTE).map(|i| (i % 251) as u8).collect();

    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let server = server_endpoint(&server_id, client_id.fingerprint());
    let proxy = Proxy::start(server.local_addr().unwrap()).await;
    proxy.set_delay(Duration::from_millis(50));
    let received = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        let (_send, mut recv) = conn.accept_bi().await.unwrap();
        recv.read_to_end(PASTE).await.unwrap()
    });

    let client = client_endpoint(&client_id, server_id.fingerprint());
    let conn = client
        .connect(proxy.addr, SERVER_NAME)
        .unwrap()
        .await
        .unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    let start = Instant::now();
    send.write_all(&paste).await.unwrap();
    send.finish().unwrap();
    let received = timeout(Duration::from_secs(30), received)
        .await
        .unwrap()
        .unwrap();
    let took = start.elapsed();

    assert!(received == paste, "paste was corrupted");
    println!("{PASTE} byte paste took {took:?}");
    assert!(took < Duration::from_secs(4), "paste took {took:?}");
}

// Input that the server does not read must not pile up in its memory.
#[tokio::test]
async fn unread_input_is_capped_by_the_receive_window() {
    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let server = server_endpoint(&server_id, client_id.fingerprint());
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        // accepts the stream but never reads from it
        let _stream = conn.accept_bi().await.unwrap();
        conn.closed().await;
    });

    let client = client_endpoint(&client_id, server_id.fingerprint());
    let conn = client.connect(addr, SERVER_NAME).unwrap().await.unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();

    let chunk = vec![0u8; 16 * 1024];
    let mut written = 0usize;
    let _ = timeout(Duration::from_secs(1), async {
        loop {
            written += send.write(&chunk).await.unwrap();
        }
    })
    .await;

    // fixed numbers on purpose: the cap must not move together with the configured window
    assert!(written <= 4 * 1024 * 1024, "wrote {written} bytes");
    assert!(written >= 64 * 1024, "wrote only {written} bytes");
}
