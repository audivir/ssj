//! The TCP side of the server: TLS over a socket on the loopback address.
//!
//! The port is reachable only from the host itself, through `ssh -W`, so that hosts behind a jump
//! host or a firewall that blocks UDP can still be reached. The authentication is the same as for
//! QUIC: mutual TLS 1.3 with pinned certificates.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ssj_net::TlsAcceptor;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc};
use tokio::time::{sleep, timeout};

use crate::session::Connection;
use crate::stream::bridge;

/// Handshakes that may be in flight at once, as for QUIC.
const MAX_HANDSHAKES: usize = 16;

/// Binds a kernel assigned port on the loopback address.
pub async fn bind_loopback() -> io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await
}

/// Accepts connections, runs the TLS handshake, and hands each stream to the session. Runs until
/// the task is aborted, which also closes the listening socket.
pub async fn serve_tcp(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    conns: mpsc::Sender<Connection>,
    handshake_timeout: Duration,
) {
    let handshakes = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    loop {
        let tcp = match listener.accept().await {
            Ok((tcp, _)) => tcp,
            Err(_) => {
                // out of descriptors or similar: wait instead of spinning
                sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = handshakes.clone().try_acquire_owned() else {
            continue;
        };
        let (acceptor, conns) = (acceptor.clone(), conns.clone());
        tokio::spawn(async move {
            let _ = tcp.set_nodelay(true);
            let Ok(Ok(tls)) = timeout(handshake_timeout, acceptor.accept(tcp)).await else {
                return;
            };
            drop(permit);
            let _ = conns.send(bridge(tls)).await;
        });
    }
}
