// each test crate uses a different subset of these helpers.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use quinn::Endpoint;
use ssj_net::{Fingerprint, Identity, client_tls, quic_client, quic_server, server_tls};
use tokio::net::UdpSocket;

pub fn localhost() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

pub fn server_endpoint(identity: &Identity, client_pin: Fingerprint) -> Endpoint {
    let config = quic_server(server_tls(identity, client_pin));
    Endpoint::server(config, localhost()).unwrap()
}

pub fn client_endpoint(identity: &Identity, server_pin: Fingerprint) -> Endpoint {
    let mut endpoint = Endpoint::client(localhost()).unwrap();
    endpoint.set_default_client_config(quic_client(client_tls(identity, server_pin)));
    endpoint
}

/// UDP forwarder between one client and one server that can drop all packets on demand.
pub struct Proxy {
    pub addr: SocketAddr,
    blackhole: Arc<AtomicBool>,
    drop_replies: Arc<AtomicBool>,
    delay_ms: Arc<AtomicU64>,
}

impl Proxy {
    pub async fn start(server: SocketAddr) -> Self {
        let front = Arc::new(UdpSocket::bind(localhost()).await.unwrap());
        let back = Arc::new(UdpSocket::bind(localhost()).await.unwrap());
        back.connect(server).await.unwrap();
        let addr = front.local_addr().unwrap();
        let blackhole = Arc::new(AtomicBool::new(false));
        let dropping = blackhole.clone();
        let drop_replies = Arc::new(AtomicBool::new(false));
        let dropping_replies = drop_replies.clone();
        let delay_ms = Arc::new(AtomicU64::new(0));
        let delay = delay_ms.clone();
        tokio::spawn(async move {
            let mut client = None;
            let mut from_client = vec![0u8; 65536];
            let mut from_server = vec![0u8; 65536];
            loop {
                tokio::select! {
                    Ok((n, peer)) = front.recv_from(&mut from_client) => {
                        client = Some(peer);
                        if !dropping.load(Ordering::Relaxed) {
                            let packet = from_client[..n].to_vec();
                            let back = back.clone();
                            let wait = Duration::from_millis(delay.load(Ordering::Relaxed));
                            tokio::spawn(async move {
                                tokio::time::sleep(wait).await;
                                let _ = back.send(&packet).await;
                            });
                        }
                    }
                    Ok(n) = back.recv(&mut from_server) => {
                        let dropped = dropping.load(Ordering::Relaxed) || dropping_replies.load(Ordering::Relaxed);
                        if let (Some(peer), false) = (client, dropped) {
                            let packet = from_server[..n].to_vec();
                            let front = front.clone();
                            let wait = Duration::from_millis(delay.load(Ordering::Relaxed));
                            tokio::spawn(async move {
                                tokio::time::sleep(wait).await;
                                let _ = front.send_to(&packet, peer).await;
                            });
                        }
                    }
                }
            }
        });
        Self {
            addr,
            blackhole,
            drop_replies,
            delay_ms,
        }
    }

    pub fn set_blackhole(&self, on: bool) {
        self.blackhole.store(on, Ordering::Relaxed);
    }

    /// Delays every packet by `delay` in each direction.
    pub fn set_delay(&self, delay: Duration) {
        self.delay_ms
            .store(delay.as_millis() as u64, Ordering::Relaxed);
    }

    /// Drops only the packets from the server to the client.
    pub fn set_drop_replies(&self, on: bool) {
        self.drop_replies.store(on, Ordering::Relaxed);
    }
}
