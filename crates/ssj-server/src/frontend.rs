//! The QUIC endpoint of the session, which opens and closes while the session keeps running.

use std::io;
use std::ops::RangeInclusive;
use std::time::Duration;

use ssj_net::{Identity, quic_server, server_tls};
use ssj_proto::bootstrap::Connect;
use tokio::sync::mpsc;

use crate::listen::{bind_in_range, endpoint_on};
use crate::session::Connection;
use crate::transport::serve;

/// Manages the current endpoint: at most one, bound to a fresh port with fresh certificates.
pub struct Frontend {
    range: RangeInclusive<u16>,
    conns: mpsc::Sender<Connection>,
    stream_timeout: Duration,
    current: Option<quinn::Endpoint>,
}

impl Frontend {
    pub fn new(
        range: RangeInclusive<u16>,
        conns: mpsc::Sender<Connection>,
        stream_timeout: Duration,
    ) -> Self {
        Self {
            range,
            conns,
            stream_timeout,
            current: None,
        }
    }

    /// Replaces the endpoint by a new one that accepts only the client with fingerprint `pin`.
    ///
    /// The old endpoint is closed first, which also ends its connections. Its port stays busy
    /// until QUIC has drained them, so the new endpoint waits for that, and retries a busy port
    /// for a short time. Must run inside the tokio runtime.
    pub async fn open(&mut self, pin: [u8; 32]) -> io::Result<Connect> {
        if let Some(old) = self.current.take() {
            old.close(0u8.into(), b"replaced");
            let _ = tokio::time::timeout(Duration::from_secs(3), old.wait_idle()).await;
        }
        let identity = Identity::generate();
        let mut tries = 0;
        let socket = loop {
            match bind_in_range(&self.range) {
                Ok(socket) => break socket,
                Err(err) if err.kind() == io::ErrorKind::AddrInUse && tries < 20 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err),
            }
        };
        let endpoint = endpoint_on(socket, quic_server(server_tls(&identity, pin)))?;
        let port = endpoint.local_addr()?.port();
        tokio::spawn(serve(
            endpoint.clone(),
            self.conns.clone(),
            self.stream_timeout,
        ));
        self.current = Some(endpoint);
        Ok(Connect {
            port,
            fingerprint: identity.fingerprint(),
        })
    }

    /// Stops listening. Nobody can connect until the next [`Frontend::open`].
    pub fn close(&mut self) {
        if let Some(endpoint) = self.current.take() {
            endpoint.close(0u8.into(), b"closed");
        }
    }
}
