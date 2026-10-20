mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quinn::{Connection, ConnectionError, Endpoint};
use ssj_net::{Identity, SERVER_NAME};
use tokio::time::timeout;

use common::{Proxy, client_endpoint, server_endpoint};

async fn connect(endpoint: &Endpoint, addr: std::net::SocketAddr) -> Connection {
    endpoint.connect(addr, SERVER_NAME).unwrap().await.unwrap()
}

/// Sends `msg` on a fresh stream and returns the reply of an echo server.
async fn roundtrip(conn: &Connection, msg: &[u8]) -> Vec<u8> {
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(msg).await.unwrap();
    send.finish().unwrap();
    recv.read_to_end(1024).await.unwrap()
}

fn spawn_echo(server: Endpoint) {
    tokio::spawn(async move {
        while let Some(incoming) = server.accept().await {
            let conn = incoming.await.unwrap();
            tokio::spawn(async move {
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let msg = recv.read_to_end(1024).await.unwrap();
                    send.write_all(&msg).await.unwrap();
                    send.finish().unwrap();
                }
            });
        }
    });
}

#[tokio::test]
async fn same_connection_survives_network_blackout() {
    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let server = server_endpoint(&server_id, client_id.fingerprint());
    let proxy = Proxy::start(server.local_addr().unwrap()).await;
    spawn_echo(server);

    let client = client_endpoint(&client_id, server_id.fingerprint());
    let conn = connect(&client, proxy.addr).await;
    assert_eq!(roundtrip(&conn, b"before").await, b"before");

    proxy.set_blackhole(true);
    let reply = tokio::spawn({
        let conn = conn.clone();
        async move { roundtrip(&conn, b"during").await }
    });
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!reply.is_finished());
    proxy.set_blackhole(false);
    let restored = Instant::now();

    let reply = timeout(Duration::from_secs(30), reply)
        .await
        .unwrap()
        .unwrap();
    println!(
        "recovered {:?} after the network returned",
        restored.elapsed()
    );
    assert_eq!(reply, b"during");
    assert!(conn.close_reason().is_none());
}

#[tokio::test]
async fn same_connection_survives_client_address_change() {
    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let server = server_endpoint(&server_id, client_id.fingerprint());
    let addr = server.local_addr().unwrap();
    spawn_echo(server);

    let client = client_endpoint(&client_id, server_id.fingerprint());
    let conn = connect(&client, addr).await;
    assert_eq!(roundtrip(&conn, b"before").await, b"before");

    let old = client.local_addr().unwrap();
    client
        .rebind(std::net::UdpSocket::bind(common::localhost()).unwrap())
        .unwrap();
    assert_ne!(client.local_addr().unwrap(), old);

    let reply = timeout(Duration::from_secs(10), roundtrip(&conn, b"after"))
        .await
        .unwrap();
    assert_eq!(reply, b"after");
}

const PAYLOAD_LEN: usize = 4 << 20;

fn payload() -> Vec<u8> {
    (0..PAYLOAD_LEN).map(|i| (i % 251) as u8).collect()
}

/// Serves `payload()` from the offset requested by the client, newest connection wins.
fn spawn_resumable_server(server: Endpoint) {
    let current: Arc<Mutex<Option<Connection>>> = Arc::default();
    let payload = Arc::new(payload());
    tokio::spawn(async move {
        while let Some(incoming) = server.accept().await {
            let Ok(conn) = incoming.await else { continue };
            if let Some(old) = current.lock().unwrap().replace(conn.clone()) {
                old.close(1u8.into(), b"replaced");
            }
            let payload = payload.clone();
            tokio::spawn(async move {
                let Ok((mut send, mut recv)) = conn.accept_bi().await else {
                    return;
                };
                let mut offset = [0u8; 8];
                if recv.read_exact(&mut offset).await.is_err() {
                    return;
                }
                let offset = u64::from_be_bytes(offset) as usize;
                let _ = send.write_all(&payload[offset..]).await;
                let _ = send.finish();
                conn.closed().await;
            });
        }
    });
}

#[tokio::test]
async fn new_connection_takes_over_while_old_one_is_still_open() {
    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let server = server_endpoint(&server_id, client_id.fingerprint());
    let addr = server.local_addr().unwrap();
    spawn_resumable_server(server);

    // first connection reads a prefix, then goes silent without closing, like a roamed client
    let first_endpoint = client_endpoint(&client_id, server_id.fingerprint());
    let first = connect(&first_endpoint, addr).await;
    let (mut send, mut recv) = first.open_bi().await.unwrap();
    send.write_all(&0u64.to_be_bytes()).await.unwrap();
    let mut prefix = vec![0u8; 100_000];
    recv.read_exact(&mut prefix).await.unwrap();
    assert_eq!(prefix, payload()[..prefix.len()]);

    let second_endpoint = client_endpoint(&client_id, server_id.fingerprint());
    let second = connect(&second_endpoint, addr).await;
    let (mut send, mut recv) = second.open_bi().await.unwrap();
    send.write_all(&(prefix.len() as u64).to_be_bytes())
        .await
        .unwrap();
    let rest = timeout(Duration::from_secs(30), recv.read_to_end(PAYLOAD_LEN))
        .await
        .unwrap()
        .unwrap();

    assert_eq!([prefix, rest].concat(), payload());
    let error = timeout(Duration::from_secs(5), first.closed())
        .await
        .unwrap();
    // the server's close frame can be overtaken by packets of the old client, which the endpoint
    // then answers with a stateless reset. Both mean that the server ended the connection.
    match error {
        ConnectionError::ApplicationClosed(closed) => {
            assert_eq!(closed.reason.as_ref(), b"replaced");
        }
        ConnectionError::Reset => {}
        other => panic!("old connection must be closed by the server, got {other:?}"),
    }
}
