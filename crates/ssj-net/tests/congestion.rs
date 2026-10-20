//! The adaptive congestion controller on real connections: it stays with CUBIC on a clean path
//! and moves to BBR on a path that loses packets at random.

mod common;

use std::time::{Duration, Instant};

use quinn::Endpoint;
use ssj_net::adaptive::switches;
use ssj_net::{Identity, SERVER_NAME};
use tokio::time::timeout;

use common::{Proxy, client_endpoint, server_endpoint};

/// The server writes `bytes` of zeros to every stream that is opened and finishes it.
fn spawn_sender(server: Endpoint, bytes: usize) {
    tokio::spawn(async move {
        while let Some(incoming) = server.accept().await {
            let conn = incoming.await.unwrap();
            tokio::spawn(async move {
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let mut ask = [0u8; 1];
                    recv.read_exact(&mut ask).await.unwrap();
                    let chunk = vec![0u8; 64 * 1024];
                    let mut left = bytes;
                    while left > 0 {
                        let n = left.min(chunk.len());
                        send.write_all(&chunk[..n]).await.unwrap();
                        left -= n;
                    }
                    send.finish().unwrap();
                    let _ = send.stopped().await;
                }
            });
        }
    });
}

/// Receives the zeros and returns how long that took.
async fn download(client: &Endpoint, addr: std::net::SocketAddr, bytes: usize) -> Duration {
    let conn = client.connect(addr, SERVER_NAME).unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(b"x").await.unwrap();
    send.finish().unwrap();
    let began = Instant::now();
    let got = timeout(Duration::from_secs(120), recv.read_to_end(bytes + 1))
        .await
        .expect("the transfer ends")
        .unwrap();
    assert_eq!(got.len(), bytes);
    began.elapsed()
}

// one test, so that the counters are not shared with another test: the switches are counted for
// the whole process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn it_stays_with_cubic_on_a_clean_path_and_moves_to_bbr_on_a_lossy_one() {
    let (server_id, client_id) = (Identity::generate(), Identity::generate());
    let server = server_endpoint(&server_id, client_id.fingerprint());
    let addr = server.local_addr().unwrap();
    let bytes = 8 * 1024 * 1024;
    spawn_sender(server, bytes);
    let client = client_endpoint(&client_id, server_id.fingerprint());

    // a clean path: no loss, so nothing to learn from.
    let before = switches();
    let clean = download(&client, addr, bytes).await;
    assert_eq!(switches(), before, "a clean path stays with CUBIC");
    println!("clean: {bytes} bytes in {clean:?}");

    // a path with a round trip of 30 ms that loses 2 percent of the packets, none of it from a
    // queue.
    let proxy = Proxy::start(addr).await;
    proxy.set_delay(Duration::from_millis(15));
    proxy.set_loss_permille(20);
    let before = switches();
    let lossy = download(&client, proxy.addr, bytes).await;
    let after = switches();
    println!("lossy: {bytes} bytes in {lossy:?}");
    assert!(
        after.0 > before.0,
        "random loss moves the connection to BBR: {before:?} to {after:?}"
    );
}
